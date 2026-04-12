use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use walkdir::WalkDir;

use hermes_core::error::{HermesError, Result};
use hermes_core::message::Message;
use hermes_core::tool::{SkillAccess, SkillDoc, SkillSummary};

use crate::skill::Skill;

const MAX_MATCHED_SKILLS: usize = 3;
const MAX_INJECTED_BODY_CHARS: usize = 12_000;

#[derive(Debug, Clone)]
pub struct SkillManager {
    skills: Vec<Skill>,
    dirs: Vec<PathBuf>,
}

#[derive(Clone)]
pub struct SharedSkillManager {
    inner: Arc<RwLock<SkillManager>>,
}

impl SharedSkillManager {
    pub fn new(inner: Arc<RwLock<SkillManager>>) -> Self {
        Self { inner }
    }

    pub async fn match_skills(
        &self,
        user_message: &str,
        history: &[Message],
        max_skills: usize,
    ) -> Vec<Skill> {
        self.inner
            .read()
            .await
            .match_for_turn(user_message, history, max_skills)
    }
}

#[async_trait]
impl SkillAccess for SharedSkillManager {
    async fn list(&self) -> Result<Vec<SkillSummary>> {
        let guard = self.inner.read().await;
        Ok(guard
            .list()
            .iter()
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect())
    }

    async fn get(&self, name: &str) -> Result<Option<SkillDoc>> {
        let guard = self.inner.read().await;
        Ok(guard.get(name).map(SkillDoc::from))
    }

    async fn match_for_turn(
        &self,
        user_message: &str,
        history: &[Message],
        max_skills: usize,
    ) -> Result<Vec<SkillDoc>> {
        let guard = self.inner.read().await;
        Ok(guard
            .match_for_turn(user_message, history, max_skills)
            .into_iter()
            .map(|skill| SkillDoc::from(&skill))
            .collect())
    }

    async fn create(&self, name: &str, content: &str) -> Result<()> {
        let mut guard = self.inner.write().await;
        guard.create(name, content)?;
        guard.reload()
    }

    async fn edit(&self, name: &str, content: &str) -> Result<()> {
        let mut guard = self.inner.write().await;
        guard.edit(name, content)?;
        guard.reload()
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let mut guard = self.inner.write().await;
        guard.delete(name)?;
        guard.reload()
    }

    async fn reload(&self) -> Result<()> {
        self.inner.write().await.reload()
    }
}

impl SkillManager {
    pub fn new(dirs: Vec<PathBuf>) -> Result<Self> {
        let mut this = Self {
            skills: Vec::new(),
            dirs,
        };
        this.discover()?;
        Ok(this)
    }

    pub fn discover(&mut self) -> Result<()> {
        let mut found = Vec::new();
        let mut seen_names = HashSet::new();

        for dir in &self.dirs {
            if !dir.exists() {
                continue;
            }

            for entry in WalkDir::new(dir)
                .into_iter()
                .filter_map(std::result::Result::ok)
                .filter(|e| e.file_type().is_file() && e.file_name() == "SKILL.md")
            {
                let skill = Skill::load(entry.path())?;
                if seen_names.insert(skill.name.clone()) {
                    found.push(skill);
                }
            }
        }

        found.sort_by(|a, b| a.name.cmp(&b.name));
        self.skills = found;
        Ok(())
    }

    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    pub fn reload(&mut self) -> Result<()> {
        self.discover()
    }

    pub fn match_for_turn(
        &self,
        user_message: &str,
        history: &[Message],
        max_skills: usize,
    ) -> Vec<Skill> {
        let _ = history;

        if self.skills.is_empty() {
            return Vec::new();
        }

        let platform = current_platform();
        let user_lower = user_message.to_lowercase();
        let user_tokens = tokenize(user_message);
        let limit = max_skills.clamp(1, MAX_MATCHED_SKILLS);

        let mut candidates = self
            .skills
            .iter()
            .filter(|skill| skill.matches_platform(platform))
            .filter_map(|skill| {
                let explicit = has_explicit_mention(&user_lower, &skill.name);
                let score = if explicit {
                    usize::MAX / 2
                } else {
                    lexical_score(&user_tokens, skill)
                };
                if explicit || score > 0 {
                    Some((skill.clone(), explicit, score))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        candidates.sort_by(
            |(a_skill, a_explicit, a_score), (b_skill, b_explicit, b_score)| {
                (
                    Reverse(*a_explicit),
                    Reverse(*a_score),
                    a_skill.name.as_str(),
                )
                    .cmp(&(
                        Reverse(*b_explicit),
                        Reverse(*b_score),
                        b_skill.name.as_str(),
                    ))
            },
        );

        let mut selected = Vec::new();
        let mut total_chars = 0usize;

        for (skill, _explicit, _) in candidates {
            if selected.len() >= limit {
                break;
            }

            let next_total = total_chars + skill.body.len();
            if next_total > MAX_INJECTED_BODY_CHARS {
                continue;
            }

            total_chars = next_total;
            selected.push(skill);
        }

        selected
    }

    pub fn inject_active_into_history(&self, active: &[Skill], history: &mut Vec<Message>) {
        if active.is_empty() {
            return;
        }

        let combined = active
            .iter()
            .map(|skill| format!("<skill name=\"{}\">\n{}\n</skill>", skill.name, skill.body))
            .collect::<Vec<_>>()
            .join("\n\n");

        history.insert(
            0,
            Message::user(format!("[Active skills for this turn]\n\n{combined}")),
        );
    }

    pub fn create(&mut self, name: &str, content: &str) -> Result<()> {
        validate_skill_name(name)?;
        let base_dir = self.primary_dir()?;
        let skill_dir = base_dir.join(name);
        let skill_path = skill_dir.join("SKILL.md");
        if skill_dir.exists() {
            return Err(HermesError::Config(format!(
                "skill already exists: {}",
                skill_dir.display()
            )));
        }
        validate_skill_content(content, &skill_path)?;
        fs::create_dir_all(&skill_dir).map_err(|e| {
            HermesError::Config(format!(
                "failed to create skill directory '{}': {e}",
                skill_dir.display()
            ))
        })?;
        fs::write(&skill_path, content).map_err(|e| {
            HermesError::Config(format!(
                "failed to write skill '{}': {e}",
                skill_path.display()
            ))
        })?;
        Ok(())
    }

    pub fn edit(&mut self, name: &str, content: &str) -> Result<()> {
        let skill = self
            .get(name)
            .ok_or_else(|| HermesError::Config(format!("skill not found: {name}")))?;
        let skill_path = skill.dir.join("SKILL.md");
        validate_skill_content(content, &skill_path)?;
        fs::write(&skill_path, content).map_err(|e| {
            HermesError::Config(format!(
                "failed to edit skill '{}': {e}",
                skill_path.display()
            ))
        })
    }

    pub fn delete(&mut self, name: &str) -> Result<()> {
        let skill = self
            .get(name)
            .ok_or_else(|| HermesError::Config(format!("skill not found: {name}")))?;
        fs::remove_dir_all(&skill.dir).map_err(|e| {
            HermesError::Config(format!(
                "failed to delete skill directory '{}': {e}",
                skill.dir.display()
            ))
        })
    }

    fn primary_dir(&self) -> Result<&Path> {
        self.dirs
            .first()
            .map(PathBuf::as_path)
            .ok_or_else(|| HermesError::Config("no skill directories configured".to_string()))
    }
}

impl From<&Skill> for SkillDoc {
    fn from(value: &Skill) -> Self {
        Self {
            name: value.name.clone(),
            description: value.description.clone(),
            body: value.body.clone(),
        }
    }
}

fn current_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        _ => "linux",
    }
}

fn validate_skill_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(HermesError::Config(
            "skill name cannot be empty".to_string(),
        ));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(HermesError::Config(
            "skill name must be a single path segment".to_string(),
        ));
    }
    Ok(())
}

fn validate_skill_content(content: &str, path: &Path) -> Result<()> {
    let _ = Skill::parse(content, path)?;
    Ok(())
}

fn has_explicit_mention(user_lower: &str, skill_name: &str) -> bool {
    let name_lower = skill_name.to_lowercase();
    user_lower.contains(&format!("${name_lower}"))
        || user_lower.contains(&format!("use {name_lower}"))
        || user_lower.contains(&name_lower)
}

fn lexical_score(user_tokens: &HashSet<String>, skill: &Skill) -> usize {
    if user_tokens.is_empty() {
        return 0;
    }
    let skill_tokens = tokenize(&format!("{} {}", skill.name, skill.description));
    user_tokens.intersection(&skill_tokens).count()
}

fn tokenize(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                r#"---
name: {name}
description: {description}
platforms: [linux]
---

{body}
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn discover_and_get_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "rust-test", "Rust testing helper", "Body");

        let manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        assert_eq!(manager.list().len(), 1);
        assert_eq!(
            manager.get("rust-test").unwrap().description,
            "Rust testing helper"
        );
    }

    #[test]
    fn explicit_match_wins() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "deploy", "Deployment helper", "Body");
        write_skill(tmp.path(), "testing", "Testing helper", "Body");

        let manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let matched = manager.match_for_turn("please use $deploy here", &[], 3);
        assert_eq!(matched.first().unwrap().name, "deploy");
    }

    #[test]
    fn inject_active_into_history_inserts_message() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "deploy", "Deployment helper", "Use deployment");

        let manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let mut history = vec![Message::user("hello")];
        let active = manager.match_for_turn("deploy release", &history, 3);
        manager.inject_active_into_history(&active, &mut history);

        assert!(
            history[0]
                .content
                .as_text_lossy()
                .contains("[Active skills")
        );
    }

    #[test]
    fn create_rejects_duplicate_skill_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "deploy", "Deployment helper", "Body");

        let mut manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let err = manager
            .create(
                "deploy",
                r#"---
name: deploy
description: Replacement
---

new body
"#,
            )
            .unwrap_err();

        assert!(err.to_string().contains("skill already exists"));
        let original = std::fs::read_to_string(tmp.path().join("deploy").join("SKILL.md")).unwrap();
        assert!(original.contains("Deployment helper"));
    }

    #[test]
    fn create_rejects_invalid_content_before_persisting() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();

        let err = manager
            .create("broken", "# Missing frontmatter")
            .unwrap_err();

        assert!(err.to_string().contains("invalid skill"));
        assert!(!tmp.path().join("broken").exists());
    }

    #[test]
    fn edit_rejects_invalid_content_without_overwriting_existing_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "deploy", "Deployment helper", "Original body");

        let mut manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let err = manager.edit("deploy", "# Missing frontmatter").unwrap_err();

        assert!(err.to_string().contains("invalid skill"));
        let on_disk = std::fs::read_to_string(tmp.path().join("deploy").join("SKILL.md")).unwrap();
        assert!(on_disk.contains("Original body"));
    }

    #[test]
    fn oversized_first_skill_is_skipped_when_smaller_match_fits() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "alpha",
            "Alpha helper",
            &"A".repeat(MAX_INJECTED_BODY_CHARS + 100),
        );
        write_skill(tmp.path(), "beta", "Beta helper", "small body");

        let manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let matched = manager.match_for_turn("alpha beta helper", &[], 3);

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "beta");
    }
}
