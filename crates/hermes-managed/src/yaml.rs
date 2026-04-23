use std::{fs, path::Path};

use hermes_config::config::AppConfig;
use hermes_core::error::{HermesError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ManagedAgentVersion, ManagedAgentVersionDraft, ManagedApprovalPolicy,
    resolve_managed_version_defaults, validate_managed_agent_name, validate_managed_beta_tools,
};

pub const MANAGED_AGENT_SYNC_METADATA_PREFIX: &str = "# hermes-synced: sha256=";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedAgentYaml {
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    pub system_prompt: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub allowed_skills: Vec<String>,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default)]
    pub temperature: f64,
    #[serde(default)]
    pub approval_policy: ManagedApprovalPolicy,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedAgentYamlFieldDiff {
    pub field: &'static str,
    pub current: Option<String>,
    pub desired: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedAgentYamlDiff {
    pub current_exists: bool,
    pub changes: Vec<ManagedAgentYamlFieldDiff>,
}

impl ManagedAgentYamlDiff {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

impl ManagedAgentYaml {
    pub fn load_path(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path).map_err(|e| {
            HermesError::Config(format!(
                "failed to read managed agent YAML {}: {e}",
                path.display()
            ))
        })?;
        Self::parse_str(&contents)
    }

    pub fn parse_str(contents: &str) -> Result<Self> {
        let parsed = serde_yaml_ng::from_str::<Self>(contents)
            .map_err(|e| HermesError::Config(format!("failed to parse managed agent YAML: {e}")))?;
        parsed.normalized()
    }

    pub fn parse_str_with_defaults(contents: &str, app_config: &AppConfig) -> Result<Self> {
        let parsed = serde_yaml_ng::from_str::<Self>(contents)
            .map_err(|e| HermesError::Config(format!("failed to parse managed agent YAML: {e}")))?;
        parsed.normalized_with_defaults(app_config)
    }

    pub fn normalized(&self) -> Result<Self> {
        let mut normalized = self.clone();
        normalized.name = normalized.name.trim().to_string();
        normalized.model = normalized.model.trim().to_string();
        normalized.base_url = normalized
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        normalized.system_prompt = normalized
            .system_prompt
            .trim_end_matches(['\n', '\r'])
            .to_string();
        normalize_name_list(&mut normalized.allowed_tools);
        normalize_name_list(&mut normalized.allowed_skills);
        normalized.validate()?;
        Ok(normalized)
    }

    pub fn normalized_with_defaults(&self, app_config: &AppConfig) -> Result<Self> {
        let mut normalized = self.normalized_allow_missing_model()?;
        let resolved = resolve_managed_version_defaults(
            Some(normalized.model.as_str()),
            normalized.base_url.as_deref(),
            app_config,
        )?;
        normalized.model = resolved.model;
        normalized.base_url = resolved.base_url;
        normalized.validate()?;
        Ok(normalized)
    }

    pub fn validate(&self) -> Result<()> {
        validate_managed_agent_name(&self.name)?;

        if self.model.is_empty() {
            return Err(HermesError::Config(
                "managed agent YAML model is required".to_string(),
            ));
        }
        if self.system_prompt.trim().is_empty() {
            return Err(HermesError::Config(
                "managed agent YAML system_prompt is required".to_string(),
            ));
        }
        if self.max_iterations == 0 {
            return Err(HermesError::Config(
                "managed agent YAML max_iterations must be greater than 0".to_string(),
            ));
        }
        if self.timeout_secs == 0 {
            return Err(HermesError::Config(
                "managed agent YAML timeout_secs must be greater than 0".to_string(),
            ));
        }
        if !self.temperature.is_finite() {
            return Err(HermesError::Config(
                "managed agent YAML temperature must be finite".to_string(),
            ));
        }

        validate_managed_beta_tools(&self.allowed_tools)?;
        Ok(())
    }

    pub fn to_draft(&self) -> ManagedAgentVersionDraft {
        let mut draft = ManagedAgentVersionDraft::new(&self.model, &self.system_prompt);
        draft.base_url = self.base_url.clone();
        draft.allowed_tools = self.allowed_tools.clone();
        draft.allowed_skills = self.allowed_skills.clone();
        draft.max_iterations = self.max_iterations;
        draft.temperature = self.temperature;
        draft.approval_policy = self.approval_policy.clone();
        draft.timeout_secs = self.timeout_secs;
        draft
    }

    pub fn canonical_yaml(&self) -> Result<String> {
        let normalized = self.normalized()?;
        serde_yaml_ng::to_string(&normalized).map_err(|e| {
            HermesError::Config(format!("failed to serialize managed agent YAML: {e}"))
        })
    }

    pub fn canonical_sha256(&self) -> Result<String> {
        let canonical = self.canonical_yaml()?;
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        Ok(format!("{:x}", hasher.finalize()))
    }

    pub fn render_with_sync_metadata(&self) -> Result<String> {
        let canonical = self.canonical_yaml()?;
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let sha256 = format!("{:x}", hasher.finalize());
        Ok(format!(
            "{MANAGED_AGENT_SYNC_METADATA_PREFIX}{sha256}\n{canonical}"
        ))
    }

    pub fn from_agent_version(agent_name: &str, version: &ManagedAgentVersion) -> Result<Self> {
        Self {
            name: agent_name.to_string(),
            model: version.model.clone(),
            base_url: version.base_url.clone(),
            system_prompt: version.system_prompt.clone(),
            allowed_tools: version.allowed_tools.clone(),
            allowed_skills: version.allowed_skills.clone(),
            max_iterations: version.max_iterations,
            temperature: version.temperature,
            approval_policy: version.approval_policy.clone(),
            timeout_secs: version.timeout_secs,
        }
        .normalized()
    }

    pub fn diff_against_version(
        &self,
        current: Option<&ManagedAgentVersion>,
    ) -> Result<ManagedAgentYamlDiff> {
        let desired = self.normalized()?;
        let current_yaml = current
            .map(|version| Self::from_agent_version(&desired.name, version))
            .transpose()?;

        let mut changes = Vec::new();
        push_diff(
            &mut changes,
            "name",
            current_yaml.as_ref().map(|value| value.name.clone()),
            Some(desired.name.clone()),
        );
        push_diff(
            &mut changes,
            "model",
            current_yaml.as_ref().map(|value| value.model.clone()),
            Some(desired.model.clone()),
        );
        push_diff(
            &mut changes,
            "base_url",
            current_yaml
                .as_ref()
                .and_then(|value| value.base_url.clone()),
            desired.base_url.clone(),
        );
        push_diff(
            &mut changes,
            "system_prompt",
            current_yaml
                .as_ref()
                .map(|value| value.system_prompt.clone()),
            Some(desired.system_prompt.clone()),
        );
        push_diff(
            &mut changes,
            "allowed_tools",
            current_yaml
                .as_ref()
                .map(|value| render_string_list(&value.allowed_tools)),
            Some(render_string_list(&desired.allowed_tools)),
        );
        push_diff(
            &mut changes,
            "allowed_skills",
            current_yaml
                .as_ref()
                .map(|value| render_string_list(&value.allowed_skills)),
            Some(render_string_list(&desired.allowed_skills)),
        );
        push_diff(
            &mut changes,
            "max_iterations",
            current_yaml
                .as_ref()
                .map(|value| value.max_iterations.to_string()),
            Some(desired.max_iterations.to_string()),
        );
        push_diff(
            &mut changes,
            "temperature",
            current_yaml
                .as_ref()
                .map(|value| value.temperature.to_string()),
            Some(desired.temperature.to_string()),
        );
        push_diff(
            &mut changes,
            "approval_policy",
            current_yaml
                .as_ref()
                .map(|value| value.approval_policy.as_str().to_string()),
            Some(desired.approval_policy.as_str().to_string()),
        );
        push_diff(
            &mut changes,
            "timeout_secs",
            current_yaml
                .as_ref()
                .map(|value| value.timeout_secs.to_string()),
            Some(desired.timeout_secs.to_string()),
        );

        Ok(ManagedAgentYamlDiff {
            current_exists: current.is_some(),
            changes,
        })
    }
}

impl ManagedAgentYaml {
    fn normalized_allow_missing_model(&self) -> Result<Self> {
        let mut normalized = self.clone();
        normalized.name = normalized.name.trim().to_string();
        normalized.model = normalized.model.trim().to_string();
        normalized.base_url = normalized
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        normalized.system_prompt = normalized
            .system_prompt
            .trim_end_matches(['\n', '\r'])
            .to_string();
        normalize_name_list(&mut normalized.allowed_tools);
        normalize_name_list(&mut normalized.allowed_skills);

        validate_managed_agent_name(&normalized.name)?;
        if normalized.system_prompt.trim().is_empty() {
            return Err(HermesError::Config(
                "managed agent YAML system_prompt is required".to_string(),
            ));
        }
        if normalized.max_iterations == 0 {
            return Err(HermesError::Config(
                "managed agent YAML max_iterations must be greater than 0".to_string(),
            ));
        }
        if normalized.timeout_secs == 0 {
            return Err(HermesError::Config(
                "managed agent YAML timeout_secs must be greater than 0".to_string(),
            ));
        }
        if !normalized.temperature.is_finite() {
            return Err(HermesError::Config(
                "managed agent YAML temperature must be finite".to_string(),
            ));
        }
        validate_managed_beta_tools(&normalized.allowed_tools)?;
        Ok(normalized)
    }
}

pub fn extract_sync_metadata_sha256(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        line.trim()
            .strip_prefix(MANAGED_AGENT_SYNC_METADATA_PREFIX)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn default_max_iterations() -> u32 {
    90
}

fn default_timeout_secs() -> u32 {
    300
}

fn normalize_name_list(values: &mut Vec<String>) {
    values.retain_mut(|value| {
        *value = value.trim().to_string();
        !value.is_empty()
    });
    values.sort();
    values.dedup();
}

fn push_diff(
    changes: &mut Vec<ManagedAgentYamlFieldDiff>,
    field: &'static str,
    current: Option<String>,
    desired: Option<String>,
) {
    if current != desired {
        changes.push(ManagedAgentYamlFieldDiff {
            field,
            current,
            desired,
        });
    }
}

fn render_string_list(values: &[String]) -> String {
    if values.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", values.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn parse_str_applies_defaults_and_normalizes_lists() {
        let yaml = r#"
name: code-reviewer
model: openai/gpt-4o-mini
system_prompt: |
  Review carefully.

allowed_tools:
  - search_files
  - read_file
  - search_files
allowed_skills:
  - deploy
  - deploy
"#;

        let spec = ManagedAgentYaml::parse_str(yaml).unwrap();
        assert_eq!(spec.name, "code-reviewer");
        assert_eq!(spec.max_iterations, 90);
        assert_eq!(spec.timeout_secs, 300);
        assert_eq!(
            spec.allowed_tools,
            vec!["read_file".to_string(), "search_files".to_string()]
        );
        assert_eq!(spec.allowed_skills, vec!["deploy".to_string()]);
        assert_eq!(spec.system_prompt, "Review carefully.");
    }

    #[test]
    fn canonical_sha256_is_stable_after_normalization() {
        let yaml_a = r#"
name: reviewer
model: openai/gpt-4o-mini
system_prompt: review
allowed_tools: [search_files, read_file]
"#;
        let yaml_b = r#"
name: reviewer
model: openai/gpt-4o-mini
system_prompt: review
allowed_tools: [read_file, search_files, search_files]
"#;

        let spec_a = ManagedAgentYaml::parse_str(yaml_a).unwrap();
        let spec_b = ManagedAgentYaml::parse_str(yaml_b).unwrap();

        assert_eq!(
            spec_a.canonical_sha256().unwrap(),
            spec_b.canonical_sha256().unwrap()
        );
    }

    #[test]
    fn diff_against_version_reports_changed_fields() {
        let spec = ManagedAgentYaml::parse_str(
            r#"
name: reviewer
model: openai/gpt-4o
system_prompt: review all code
allowed_tools: [read_file]
timeout_secs: 180
"#,
        )
        .unwrap();

        let mut version = ManagedAgentVersion::new("agent_1", 1, "openai/gpt-4o-mini", "old");
        version.allowed_tools = vec!["search_files".to_string()];
        version.timeout_secs = 300;
        version.created_at = Utc::now();

        let diff = spec.diff_against_version(Some(&version)).unwrap();
        assert!(!diff.is_empty());
        assert!(diff.changes.iter().any(|change| change.field == "model"));
        assert!(
            diff.changes
                .iter()
                .any(|change| change.field == "system_prompt")
        );
        assert!(
            diff.changes
                .iter()
                .any(|change| change.field == "allowed_tools")
        );
        assert!(
            diff.changes
                .iter()
                .any(|change| change.field == "timeout_secs")
        );
    }

    #[test]
    fn render_with_sync_metadata_prefixes_canonical_sha() {
        let spec = ManagedAgentYaml::parse_str(
            r#"
name: reviewer
model: openai/gpt-4o-mini
system_prompt: review carefully
allowed_tools: [search_files, read_file]
"#,
        )
        .unwrap();

        let rendered = spec.render_with_sync_metadata().unwrap();
        let metadata_sha = extract_sync_metadata_sha256(&rendered).unwrap();

        assert_eq!(metadata_sha, spec.canonical_sha256().unwrap());
        assert_eq!(ManagedAgentYaml::parse_str(&rendered).unwrap(), spec);
    }

    #[test]
    fn parse_str_with_defaults_inherits_model_and_base_url() {
        let app_config = AppConfig {
            model: "openai/gpt-4o-mini".to_string(),
            base_url: Some("https://models.example/v1".to_string()),
            ..AppConfig::default()
        };

        let spec = ManagedAgentYaml::parse_str_with_defaults(
            r#"
name: reviewer
system_prompt: review carefully
"#,
            &app_config,
        )
        .unwrap();

        assert_eq!(spec.model, "openai/gpt-4o-mini");
        assert_eq!(spec.base_url.as_deref(), Some("https://models.example/v1"));
    }
}
