use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use hermes_core::{
    error::{HermesError, Result},
    message::Message,
    tool::{SkillAccess, SkillDoc, SkillSummary},
};
use hermes_skills::SkillManager;

pub fn build_filtered_skill_manager(
    source: &SkillManager,
    allowed_skills: &[String],
) -> Result<SkillManager> {
    let allowed = allowed_skills.iter().cloned().collect::<HashSet<_>>();
    let mut missing = allowed
        .iter()
        .filter(|name| source.get(name).is_none())
        .cloned()
        .collect::<Vec<_>>();
    missing.sort();

    if !missing.is_empty() {
        return Err(HermesError::Config(format!(
            "managed skill allowlist references unknown skills: {}",
            missing.join(", ")
        )));
    }

    Ok(source.filtered(&allowed))
}

#[derive(Clone)]
pub struct FilteredSkillAccess {
    inner: Arc<dyn SkillAccess>,
    allowed: Arc<HashSet<String>>,
}

impl FilteredSkillAccess {
    pub fn new(inner: Arc<dyn SkillAccess>, allowed_skills: &[String]) -> Self {
        Self {
            inner,
            allowed: Arc::new(allowed_skills.iter().cloned().collect()),
        }
    }

    fn allows(&self, name: &str) -> bool {
        self.allowed.contains(name)
    }
}

#[async_trait]
impl SkillAccess for FilteredSkillAccess {
    async fn list(&self) -> Result<Vec<SkillSummary>> {
        Ok(self
            .inner
            .list()
            .await?
            .into_iter()
            .filter(|skill| self.allows(&skill.name))
            .collect())
    }

    async fn get(&self, name: &str) -> Result<Option<SkillDoc>> {
        if !self.allows(name) {
            return Ok(None);
        }

        self.inner.get(name).await
    }

    async fn match_for_turn(
        &self,
        user_message: &str,
        history: &[Message],
        max_skills: usize,
    ) -> Result<Vec<SkillDoc>> {
        Ok(self
            .inner
            .match_for_turn(user_message, history, max_skills)
            .await?
            .into_iter()
            .filter(|skill| self.allows(&skill.name))
            .collect())
    }

    async fn create(&self, _name: &str, _content: &str) -> Result<()> {
        Err(HermesError::Config(
            "filtered skill access is read-only".to_string(),
        ))
    }

    async fn edit(&self, _name: &str, _content: &str) -> Result<()> {
        Err(HermesError::Config(
            "filtered skill access is read-only".to_string(),
        ))
    }

    async fn delete(&self, _name: &str) -> Result<()> {
        Err(HermesError::Config(
            "filtered skill access is read-only".to_string(),
        ))
    }

    async fn reload(&self) -> Result<()> {
        self.inner.reload().await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use hermes_skills::SharedSkillManager;
    use tokio::sync::RwLock;

    use super::*;

    fn write_skill(dir: &std::path::Path, name: &str, description: &str, body: &str) {
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
    fn filtered_skill_manager_only_keeps_allowed_skills() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "deploy", "Deployment helper", "Use deploy");
        write_skill(tmp.path(), "testing", "Testing helper", "Use testing");

        let manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let filtered = build_filtered_skill_manager(&manager, &["deploy".to_string()]).unwrap();

        assert_eq!(filtered.list().len(), 1);
        assert_eq!(filtered.list()[0].name, "deploy");

        let matched = filtered.match_for_turn("please use deploy and testing", &[], 3);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "deploy");
    }

    #[test]
    fn filtered_skill_manager_rejects_unknown_skills() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "deploy", "Deployment helper", "Use deploy");

        let manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let err = build_filtered_skill_manager(&manager, &["missing".to_string()])
            .err()
            .unwrap();

        assert!(err.to_string().contains("missing"));
    }

    #[tokio::test]
    async fn filtered_skill_access_hides_blocked_skills() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "deploy", "Deployment helper", "Use deploy");
        write_skill(tmp.path(), "testing", "Testing helper", "Use testing");

        let manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let shared: Arc<dyn SkillAccess> =
            Arc::new(SharedSkillManager::new(Arc::new(RwLock::new(manager))));
        let filtered = FilteredSkillAccess::new(shared, &["deploy".to_string()]);

        let listed = filtered.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "deploy");

        assert!(filtered.get("testing").await.unwrap().is_none());

        let matched = filtered
            .match_for_turn("please use deploy and testing", &[], 3)
            .await
            .unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "deploy");
    }

    #[tokio::test]
    async fn filtered_skill_access_rejects_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "deploy", "Deployment helper", "Use deploy");

        let manager = SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap();
        let shared: Arc<dyn SkillAccess> =
            Arc::new(SharedSkillManager::new(Arc::new(RwLock::new(manager))));
        let filtered = FilteredSkillAccess::new(shared, &["deploy".to_string()]);

        let err = filtered.create("new-skill", "ignored").await.unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }
}
