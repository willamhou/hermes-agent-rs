use std::path::{Path, PathBuf};

use hermes_core::error::{HermesError, Result};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub platforms: Vec<String>,
    pub category: Option<String>,
    pub dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    category: Option<String>,
}

impl Skill {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            HermesError::Config(format!("failed to read skill '{}': {e}", path.display()))
        })?;

        Self::parse(&raw, path)
    }

    pub(crate) fn parse(raw: &str, path: &Path) -> Result<Self> {
        let (frontmatter, body) = split_frontmatter(raw)
            .map_err(|e| HermesError::Config(format!("invalid skill '{}': {e}", path.display())))?;

        let meta: SkillFrontmatter = serde_yaml_ng::from_str(frontmatter).map_err(|e| {
            HermesError::Config(format!(
                "failed to parse skill frontmatter '{}': {e}",
                path.display()
            ))
        })?;

        Ok(Self {
            name: meta.name,
            description: meta.description,
            body: body.trim().to_string(),
            platforms: meta.platforms,
            category: meta.category,
            dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        })
    }

    pub fn matches_platform(&self, platform: &str) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|p| p == platform)
    }
}

fn split_frontmatter(raw: &str) -> std::result::Result<(&str, &str), String> {
    let mut cursor = 0usize;
    let mut lines = raw.split_inclusive('\n');

    let first = lines
        .next()
        .ok_or_else(|| "missing opening frontmatter fence".to_string())?;
    if first.trim_end_matches(&['\r', '\n'][..]) != "---" {
        return Err("missing opening frontmatter fence".to_string());
    }
    cursor += first.len();

    let frontmatter_start = cursor;

    for line in lines {
        if line.trim_end_matches(&['\r', '\n'][..]) == "---" {
            let frontmatter = &raw[frontmatter_start..cursor];
            cursor += line.len();
            let body = raw.get(cursor..).unwrap_or_default();
            return Ok((frontmatter, body));
        }
        cursor += line.len();
    }

    let tail = &raw[cursor..];
    if tail.trim_end_matches(&['\r', '\n'][..]) == "---" {
        let frontmatter = &raw[frontmatter_start..cursor];
        return Ok((frontmatter, ""));
    }

    Err("missing closing frontmatter fence".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_file() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_path = tmp.path().join("SKILL.md");
        std::fs::write(
            &skill_path,
            r#"---
name: demo-skill
description: Demo skill
platforms: [linux]
category: demo
---

# Demo

Use this skill."#,
        )
        .unwrap();

        let skill = Skill::load(&skill_path).unwrap();
        assert_eq!(skill.name, "demo-skill");
        assert_eq!(skill.description, "Demo skill");
        assert_eq!(skill.platforms, vec!["linux"]);
        assert_eq!(skill.category.as_deref(), Some("demo"));
        assert!(skill.body.contains("# Demo"));
    }

    #[test]
    fn parse_skill_requires_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_path = tmp.path().join("SKILL.md");
        std::fs::write(&skill_path, "# Missing frontmatter").unwrap();

        let err = Skill::load(&skill_path).unwrap_err();
        assert!(
            err.to_string()
                .contains("missing opening frontmatter fence")
        );
    }

    #[test]
    fn parse_skill_from_string_uses_supplied_path_context() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_path = tmp.path().join("custom").join("SKILL.md");
        let skill = Skill::parse(
            r#"---
name: inline-skill
description: Inline skill
---

Body
"#,
            &skill_path,
        )
        .unwrap();

        assert_eq!(skill.name, "inline-skill");
        assert_eq!(skill.dir, skill_path.parent().unwrap());
    }
}
