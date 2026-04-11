//! Application configuration — YAML loading with environment variable overrides.

use std::path::PathBuf;

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
        }
    }
}

impl AppConfig {
    /// Load configuration from `hermes_home()/config.yaml`.
    ///
    /// Falls back to [`AppConfig::default`] if the file is absent or unreadable.
    pub fn load() -> Self {
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
            "openai" => Some("OPENAI_API_KEY"),
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
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.model, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(cfg.max_iterations, 90);
        assert!((cfg.temperature - 0.7f32).abs() < f32::EPSILON);
    }

    #[test]
    fn config_serde_roundtrip() {
        let original = AppConfig {
            model: "openai/gpt-4o".to_string(),
            max_iterations: 50,
            temperature: 0.5,
        };
        let yaml = serde_yaml_ng::to_string(&original).expect("serialize failed");
        let restored: AppConfig = serde_yaml_ng::from_str(&yaml).expect("deserialize failed");
        assert_eq!(restored.model, original.model);
        assert_eq!(restored.max_iterations, original.max_iterations);
        assert!((restored.temperature - original.temperature).abs() < f32::EPSILON);
    }

    #[test]
    fn hermes_home_default() {
        // When HERMES_HOME is not set, should resolve to something ending in ".hermes"
        // We cannot rely on HOME being absent, so just verify the path ends with ".hermes"
        // unless HERMES_HOME is overriding it.
        if std::env::var("HERMES_HOME").is_err() {
            let home = hermes_home();
            assert_eq!(home.file_name().and_then(|f| f.to_str()), Some(".hermes"));
        }
    }

    #[test]
    fn hermes_home_env_override() {
        // Temporarily set HERMES_HOME and verify it's used.
        // Use a temp-dir-style path to avoid side effects.
        let override_path = "/tmp/hermes_test_home";
        // SAFETY: single-threaded test, no concurrent env reads.
        unsafe {
            std::env::set_var("HERMES_HOME", override_path);
        }
        let home = hermes_home();
        // SAFETY: single-threaded test.
        unsafe {
            std::env::remove_var("HERMES_HOME");
        }
        assert_eq!(home, PathBuf::from(override_path));
    }
}
