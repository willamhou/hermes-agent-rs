//! Application configuration — YAML loading with environment variable overrides.

use std::path::PathBuf;

use hermes_core::tool::{FileToolConfig, TerminalToolConfig, ToolConfig};
use serde::{Deserialize, Serialize};
use tracing::warn;

// ─── Home directory ───────────────────────────────────────────────────────────

/// Return the Hermes home directory.
///
/// Resolves `$HERMES_HOME` if set; otherwise falls back to `~/.hermes`.
pub fn hermes_home() -> PathBuf {
    if let Ok(val) = std::env::var("HERMES_HOME") {
        if !val.is_empty() {
            return PathBuf::from(val);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hermes")
}

// ─── Terminal config ──────────────────────────────────────────────────────────

/// YAML-compatible terminal tool configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfigYaml {
    #[serde(default = "default_terminal_timeout")]
    pub timeout: u64,
    #[serde(default = "default_terminal_max_timeout")]
    pub max_timeout: u64,
    #[serde(default = "default_terminal_output_max_chars")]
    pub output_max_chars: usize,
}

fn default_terminal_timeout() -> u64 {
    180
}

fn default_terminal_max_timeout() -> u64 {
    600
}

fn default_terminal_output_max_chars() -> usize {
    50_000
}

impl Default for TerminalConfigYaml {
    fn default() -> Self {
        Self {
            timeout: default_terminal_timeout(),
            max_timeout: default_terminal_max_timeout(),
            output_max_chars: default_terminal_output_max_chars(),
        }
    }
}

// ─── File config ──────────────────────────────────────────────────────────────

/// YAML-compatible file tool configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConfigYaml {
    #[serde(default = "default_file_read_max_chars")]
    pub read_max_chars: usize,
    #[serde(default = "default_file_read_max_lines")]
    pub read_max_lines: usize,
}

fn default_file_read_max_chars() -> usize {
    100_000
}

fn default_file_read_max_lines() -> usize {
    2000
}

impl Default for FileConfigYaml {
    fn default() -> Self {
        Self {
            read_max_chars: default_file_read_max_chars(),
            read_max_lines: default_file_read_max_lines(),
        }
    }
}

// ─── Approval config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    Ask,
    Yolo,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalConfigYaml {
    #[serde(default)]
    pub policy: ApprovalPolicy,
}

// ─── Config struct ────────────────────────────────────────────────────────────

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Model string in `"provider/model"` or bare `"model"` format.
    #[serde(default = "default_model")]
    pub model: String,

    /// Maximum agent loop iterations per invocation.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,

    /// Sampling temperature (0.0 – 1.0).
    #[serde(default = "default_temperature")]
    pub temperature: f32,

    /// Terminal tool configuration.
    #[serde(default)]
    pub terminal: TerminalConfigYaml,

    /// File tool configuration.
    #[serde(default)]
    pub file: FileConfigYaml,

    /// Dangerous tool approval behavior.
    #[serde(default)]
    pub approval: ApprovalConfigYaml,
}

fn default_model() -> String {
    "anthropic/claude-sonnet-4-20250514".to_string()
}

fn default_max_iterations() -> u32 {
    90
}

fn default_temperature() -> f32 {
    0.7
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            max_iterations: default_max_iterations(),
            temperature: default_temperature(),
            terminal: TerminalConfigYaml::default(),
            file: FileConfigYaml::default(),
            approval: ApprovalConfigYaml::default(),
        }
    }
}

impl AppConfig {
    /// Load configuration from `hermes_home()/config.yaml`.
    ///
    /// Loads `.env` from `hermes_home()/.env` first, then falls back to
    /// [`AppConfig::default`] if the YAML file is absent or unreadable.
    pub fn load() -> Self {
        let env_path = hermes_home().join(".env");
        if env_path.exists() {
            let _ = dotenvy::from_path(&env_path);
        }

        let path = hermes_home().join("config.yaml");

        let contents = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(e) => {
                warn!(path = %path.display(), "failed to read config file: {e}");
                return Self::default();
            }
        };

        match serde_yaml_ng::from_str::<AppConfig>(&contents) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(path = %path.display(), "failed to parse config YAML: {e}");
                Self::default()
            }
        }
    }

    /// Resolve the API key for the configured provider.
    ///
    /// Checks a provider-specific environment variable first:
    /// - `anthropic` → `ANTHROPIC_API_KEY`
    /// - `openai` → `OPENAI_API_KEY`
    /// - `openrouter` → `OPENROUTER_API_KEY`
    ///
    /// Falls back to `HERMES_API_KEY` if the provider-specific var is absent.
    pub fn api_key(&self) -> Option<String> {
        let provider = self
            .model
            .split('/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();

        let provider_var = match provider.as_str() {
            "anthropic" => Some("ANTHROPIC_API_KEY"),
            "openai" | "openai-codex" | "openai-responses" => Some("OPENAI_API_KEY"),
            "openrouter" => Some("OPENROUTER_API_KEY"),
            _ => None,
        };

        if let Some(var) = provider_var {
            if let Ok(key) = std::env::var(var) {
                if !key.is_empty() {
                    return Some(key);
                }
            }
        }

        std::env::var("HERMES_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
    }

    /// Convert this config into a [`ToolConfig`] for the given workspace root.
    pub fn tool_config(&self, workspace_root: PathBuf) -> ToolConfig {
        ToolConfig {
            terminal: TerminalToolConfig {
                timeout: self.terminal.timeout,
                max_timeout: self.terminal.max_timeout,
                output_max_chars: self.terminal.output_max_chars,
            },
            file: FileToolConfig {
                read_max_chars: self.file.read_max_chars,
                read_max_lines: self.file.read_max_lines,
                blocked_prefixes: vec![
                    PathBuf::from("/etc/"),
                    PathBuf::from("/boot/"),
                    PathBuf::from("/usr/lib/systemd/"),
                ],
            },
            workspace_root,
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn default_config() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.model, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(cfg.max_iterations, 90);
        assert!((cfg.temperature - 0.7f32).abs() < f32::EPSILON);
        assert_eq!(cfg.approval.policy, ApprovalPolicy::Ask);
    }

    #[test]
    fn config_serde_roundtrip() {
        let original = AppConfig {
            model: "openai/gpt-4o".to_string(),
            max_iterations: 50,
            temperature: 0.5,
            terminal: TerminalConfigYaml::default(),
            file: FileConfigYaml::default(),
            approval: ApprovalConfigYaml::default(),
        };
        let yaml = serde_yaml_ng::to_string(&original).expect("serialize failed");
        let restored: AppConfig = serde_yaml_ng::from_str(&yaml).expect("deserialize failed");
        assert_eq!(restored.model, original.model);
        assert_eq!(restored.max_iterations, original.max_iterations);
        assert!((restored.temperature - original.temperature).abs() < f32::EPSILON);
        assert_eq!(restored.approval.policy, ApprovalPolicy::Ask);
    }

    #[test]
    fn hermes_home_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var("HERMES_HOME").ok();
        // SAFETY: test holds ENV_LOCK, so no concurrent env mutation in this module.
        unsafe {
            std::env::remove_var("HERMES_HOME");
        }

        let home = hermes_home();

        match previous {
            Some(value) => {
                // SAFETY: test holds ENV_LOCK, so no concurrent env mutation in this module.
                unsafe {
                    std::env::set_var("HERMES_HOME", value);
                }
            }
            None => {
                // SAFETY: test holds ENV_LOCK, so no concurrent env mutation in this module.
                unsafe {
                    std::env::remove_var("HERMES_HOME");
                }
            }
        }

        assert_eq!(home.file_name().and_then(|f| f.to_str()), Some(".hermes"));
    }

    #[test]
    fn hermes_home_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var("HERMES_HOME").ok();
        let override_path = "/tmp/hermes_test_home";
        // SAFETY: test holds ENV_LOCK, so no concurrent env mutation in this module.
        unsafe {
            std::env::set_var("HERMES_HOME", override_path);
        }
        let home = hermes_home();
        match previous {
            Some(value) => {
                // SAFETY: test holds ENV_LOCK, so no concurrent env mutation in this module.
                unsafe {
                    std::env::set_var("HERMES_HOME", value);
                }
            }
            None => {
                // SAFETY: test holds ENV_LOCK, so no concurrent env mutation in this module.
                unsafe {
                    std::env::remove_var("HERMES_HOME");
                }
            }
        }
        assert_eq!(home, PathBuf::from(override_path));
    }

    #[test]
    fn config_with_terminal_section() {
        let yaml = r#"
model: anthropic/claude-sonnet-4-20250514
terminal:
  timeout: 300
  max_timeout: 900
  output_max_chars: 80000
"#;
        let cfg: AppConfig = serde_yaml_ng::from_str(yaml).expect("deserialize failed");
        assert_eq!(cfg.terminal.timeout, 300);
        assert_eq!(cfg.terminal.max_timeout, 900);
        assert_eq!(cfg.terminal.output_max_chars, 80_000);
    }

    #[test]
    fn config_defaults_when_sections_missing() {
        let yaml = r#"
model: openai/gpt-4o
"#;
        let cfg: AppConfig = serde_yaml_ng::from_str(yaml).expect("deserialize failed");
        assert_eq!(cfg.terminal.timeout, 180);
        assert_eq!(cfg.terminal.max_timeout, 600);
        assert_eq!(cfg.terminal.output_max_chars, 50_000);
        assert_eq!(cfg.file.read_max_chars, 100_000);
        assert_eq!(cfg.file.read_max_lines, 2000);
        assert_eq!(cfg.approval.policy, ApprovalPolicy::Ask);
    }

    #[test]
    fn approval_policy_serde_roundtrip() {
        let yaml = r#"
model: openai-codex/gpt-5
approval:
  policy: yolo
"#;
        let cfg: AppConfig = serde_yaml_ng::from_str(yaml).expect("deserialize failed");
        assert_eq!(cfg.approval.policy, ApprovalPolicy::Yolo);
    }

    #[test]
    fn tool_config_conversion() {
        let cfg = AppConfig::default();
        let root = PathBuf::from("/tmp/workspace");
        let tc = cfg.tool_config(root.clone());
        assert_eq!(tc.terminal.timeout, 180);
        assert_eq!(tc.terminal.max_timeout, 600);
        assert_eq!(tc.terminal.output_max_chars, 50_000);
        assert_eq!(tc.file.read_max_chars, 100_000);
        assert_eq!(tc.file.read_max_lines, 2000);
        assert_eq!(tc.workspace_root, root);
        assert!(tc.file.blocked_prefixes.contains(&PathBuf::from("/etc/")));
    }
}
