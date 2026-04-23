//! Application configuration — YAML loading with environment variable overrides.

use std::path::PathBuf;

use hermes_core::tool::{BrowserToolConfig, FileToolConfig, TerminalToolConfig, ToolConfig};
use secrecy::SecretString;
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

// ─── Browser config ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfigYaml {
    #[serde(default = "default_browser_headless")]
    pub headless: bool,
    #[serde(default = "default_browser_sandbox")]
    pub sandbox: bool,
    #[serde(default = "default_browser_launch_timeout_secs")]
    pub launch_timeout_secs: u64,
    #[serde(default = "default_browser_action_timeout_secs")]
    pub action_timeout_secs: u64,
    #[serde(default = "default_browser_output_max_chars")]
    pub output_max_chars: usize,
    #[serde(default = "default_browser_viewport_width")]
    pub viewport_width: u32,
    #[serde(default = "default_browser_viewport_height")]
    pub viewport_height: u32,
    #[serde(default)]
    pub executable: Option<PathBuf>,
}

fn default_browser_headless() -> bool {
    true
}

fn default_browser_sandbox() -> bool {
    true
}

fn default_browser_launch_timeout_secs() -> u64 {
    20
}

fn default_browser_action_timeout_secs() -> u64 {
    30
}

fn default_browser_output_max_chars() -> usize {
    50_000
}

fn default_browser_viewport_width() -> u32 {
    1280
}

fn default_browser_viewport_height() -> u32 {
    720
}

impl Default for BrowserConfigYaml {
    fn default() -> Self {
        Self {
            headless: default_browser_headless(),
            sandbox: default_browser_sandbox(),
            launch_timeout_secs: default_browser_launch_timeout_secs(),
            action_timeout_secs: default_browser_action_timeout_secs(),
            output_max_chars: default_browser_output_max_chars(),
            viewport_width: default_browser_viewport_width(),
            viewport_height: default_browser_viewport_height(),
            executable: None,
        }
    }
}

// ─── MCP config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    #[default]
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub transport: McpTransportKind,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_mcp_server_enabled")]
    pub enabled: bool,
}

fn default_mcp_server_enabled() -> bool {
    true
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

// ─── Gateway config ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub telegram: Option<TelegramGatewayConfig>,
    #[serde(default)]
    pub discord: Option<DiscordGatewayConfig>,
    #[serde(default)]
    pub api_server: Option<ApiServerGatewayConfig>,
    #[serde(default = "default_session_idle_timeout")]
    pub session_idle_timeout_secs: u64,
    #[serde(default = "default_max_sessions")]
    pub max_concurrent_sessions: usize,
}

fn default_session_idle_timeout() -> u64 {
    1800
}
fn default_max_sessions() -> usize {
    100
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            telegram: None,
            discord: None,
            api_server: None,
            session_idle_timeout_secs: default_session_idle_timeout(),
            max_concurrent_sessions: default_max_sessions(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordGatewayConfig {
    pub token: String,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allow_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramGatewayConfig {
    pub token: String,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allow_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiServerGatewayConfig {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Model name to report in `/v1/models` and completions responses.
    #[serde(default)]
    pub model_name: Option<String>,
}

fn default_bind_addr() -> String {
    "127.0.0.1:8080".into()
}

impl Default for ApiServerGatewayConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            api_key: None,
            model_name: None,
        }
    }
}

// ─── Signet config ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignetConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_signet_key_name")]
    pub key_name: String,
    #[serde(default = "default_signet_owner")]
    pub owner: String,
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

fn default_signet_key_name() -> String {
    "hermes-managed".to_string()
}

fn default_signet_owner() -> String {
    "hermes".to_string()
}

impl Default for SignetConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            key_name: default_signet_key_name(),
            owner: default_signet_owner(),
            dir: None,
        }
    }
}

// ─── Config struct ────────────────────────────────────────────────────────────

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Model string in `"provider/model"` or bare `"model"` format.
    #[serde(default = "default_model")]
    pub model: String,

    /// Override the provider base URL (e.g. for OpenAI-compatible proxies).
    #[serde(default)]
    pub base_url: Option<String>,

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

    /// Browser automation tool configuration.
    #[serde(default)]
    pub browser: BrowserConfigYaml,

    /// Dangerous tool approval behavior.
    #[serde(default)]
    pub approval: ApprovalConfigYaml,

    /// External MCP servers discovered at startup.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,

    /// Gateway configuration (Telegram, API server, etc.).
    #[serde(default)]
    pub gateway: Option<GatewayConfig>,

    /// Optional Signet tool-call receipts for managed runtimes.
    #[serde(default)]
    pub signet: SignetConfig,
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
            base_url: None,
            max_iterations: default_max_iterations(),
            temperature: default_temperature(),
            terminal: TerminalConfigYaml::default(),
            file: FileConfigYaml::default(),
            browser: BrowserConfigYaml::default(),
            approval: ApprovalConfigYaml::default(),
            mcp_servers: vec![],
            gateway: None,
            signet: SignetConfig::default(),
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
        let cfg = match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_yaml_ng::from_str::<AppConfig>(&contents) {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!(path = %path.display(), "failed to parse config YAML: {e}");
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!(path = %path.display(), "failed to read config file: {e}");
                Self::default()
            }
        };

        cfg.apply_env_overrides()
    }

    fn apply_env_overrides(mut self) -> Self {
        if let Ok(model) = std::env::var("HERMES_MODEL") {
            let trimmed = model.trim();
            if !trimmed.is_empty() {
                self.model = trimmed.to_string();
            }
        }

        if let Ok(base_url) = std::env::var("HERMES_BASE_URL") {
            self.base_url = normalize_env_override(&base_url);
        }

        self
    }

    /// Resolve the API key for the configured provider.
    ///
    /// Checks a provider-specific environment variable first:
    /// - `anthropic` → `ANTHROPIC_API_KEY`
    /// - `openai` → `OPENAI_API_KEY`
    /// - `openrouter` → `OPENROUTER_API_KEY`
    ///
    /// Falls back to `HERMES_API_KEY` if the provider-specific var is absent.
    pub fn api_key(&self) -> Option<SecretString> {
        self.api_key_for_model(&self.model)
    }

    /// Resolve the API key for an arbitrary `"provider/model"` string.
    ///
    /// Falls back to `HERMES_API_KEY` when the provider-specific environment
    /// variable is absent.
    pub fn api_key_for_model(&self, model: &str) -> Option<SecretString> {
        let provider = model.split('/').next().unwrap_or("").to_ascii_lowercase();

        let provider_var = match provider.as_str() {
            "anthropic" => Some("ANTHROPIC_API_KEY"),
            "openai" | "openai-codex" | "openai-responses" => Some("OPENAI_API_KEY"),
            "openrouter" => Some("OPENROUTER_API_KEY"),
            _ => None,
        };

        if let Some(var) = provider_var {
            if let Ok(key) = std::env::var(var) {
                if !key.is_empty() {
                    return Some(SecretString::new(key.into()));
                }
            }
        }

        std::env::var("HERMES_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .map(|k| SecretString::new(k.into()))
    }

    /// Resolve the base directory for Signet keys and audit logs.
    pub fn signet_dir(&self) -> PathBuf {
        self.signet
            .dir
            .clone()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| hermes_home().join("signet"))
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
            browser: BrowserToolConfig {
                headless: self.browser.headless,
                sandbox: self.browser.sandbox,
                launch_timeout_secs: self.browser.launch_timeout_secs,
                action_timeout_secs: self.browser.action_timeout_secs,
                output_max_chars: self.browser.output_max_chars,
                viewport_width: self.browser.viewport_width,
                viewport_height: self.browser.viewport_height,
                executable: self.browser.executable.clone(),
            },
            workspace_root,
        }
    }
}

fn normalize_env_override(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
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
            base_url: None,
            max_iterations: 50,
            temperature: 0.5,
            terminal: TerminalConfigYaml::default(),
            file: FileConfigYaml::default(),
            browser: BrowserConfigYaml::default(),
            approval: ApprovalConfigYaml::default(),
            mcp_servers: vec![],
            gateway: None,
            signet: SignetConfig::default(),
        };
        let yaml = serde_yaml_ng::to_string(&original).expect("serialize failed");
        let restored: AppConfig = serde_yaml_ng::from_str(&yaml).expect("deserialize failed");
        assert_eq!(restored.model, original.model);
        assert_eq!(restored.max_iterations, original.max_iterations);
        assert!((restored.temperature - original.temperature).abs() < f32::EPSILON);
        assert_eq!(restored.approval.policy, ApprovalPolicy::Ask);
        assert!(restored.mcp_servers.is_empty());
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
    fn api_key_for_model_respects_provider_specific_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        let prev_hermes = std::env::var("HERMES_API_KEY").ok();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "openai-key");
            std::env::set_var("HERMES_API_KEY", "fallback-key");
        }

        let cfg = AppConfig::default();
        let key = cfg
            .api_key_for_model("openai/gpt-4o")
            .expect("expected openai key");
        assert_eq!(key.expose_secret(), "openai-key");

        match prev_openai {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        match prev_hermes {
            Some(value) => unsafe { std::env::set_var("HERMES_API_KEY", value) },
            None => unsafe { std::env::remove_var("HERMES_API_KEY") },
        }
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
    fn load_applies_env_overrides_over_yaml() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.yaml"),
            r#"
model: anthropic/claude-sonnet-4-20250514
base_url: https://yaml.example/v1
"#,
        )
        .unwrap();

        let prev_home = std::env::var("HERMES_HOME").ok();
        let prev_model = std::env::var("HERMES_MODEL").ok();
        let prev_base_url = std::env::var("HERMES_BASE_URL").ok();

        unsafe {
            std::env::set_var("HERMES_HOME", tmp.path());
            std::env::set_var("HERMES_MODEL", "openai/gpt-4o-mini");
            std::env::set_var("HERMES_BASE_URL", "https://env.example/v1");
        }

        let cfg = AppConfig::load();
        assert_eq!(cfg.model, "openai/gpt-4o-mini");
        assert_eq!(cfg.base_url.as_deref(), Some("https://env.example/v1"));

        match prev_home {
            Some(value) => unsafe { std::env::set_var("HERMES_HOME", value) },
            None => unsafe { std::env::remove_var("HERMES_HOME") },
        }
        match prev_model {
            Some(value) => unsafe { std::env::set_var("HERMES_MODEL", value) },
            None => unsafe { std::env::remove_var("HERMES_MODEL") },
        }
        match prev_base_url {
            Some(value) => unsafe { std::env::set_var("HERMES_BASE_URL", value) },
            None => unsafe { std::env::remove_var("HERMES_BASE_URL") },
        }
    }

    #[test]
    fn load_treats_empty_env_base_url_as_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();

        let prev_home = std::env::var("HERMES_HOME").ok();
        let prev_model = std::env::var("HERMES_MODEL").ok();
        let prev_base_url = std::env::var("HERMES_BASE_URL").ok();

        unsafe {
            std::env::set_var("HERMES_HOME", tmp.path());
            std::env::set_var(
                "HERMES_MODEL",
                "openrouter/meta-llama/llama-3.1-8b-instruct",
            );
            std::env::set_var("HERMES_BASE_URL", "   ");
        }

        let cfg = AppConfig::load();
        assert_eq!(cfg.model, "openrouter/meta-llama/llama-3.1-8b-instruct");
        assert_eq!(cfg.base_url, None);

        match prev_home {
            Some(value) => unsafe { std::env::set_var("HERMES_HOME", value) },
            None => unsafe { std::env::remove_var("HERMES_HOME") },
        }
        match prev_model {
            Some(value) => unsafe { std::env::set_var("HERMES_MODEL", value) },
            None => unsafe { std::env::remove_var("HERMES_MODEL") },
        }
        match prev_base_url {
            Some(value) => unsafe { std::env::set_var("HERMES_BASE_URL", value) },
            None => unsafe { std::env::remove_var("HERMES_BASE_URL") },
        }
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
        assert!(cfg.browser.headless);
        assert!(cfg.browser.sandbox);
        assert_eq!(cfg.browser.launch_timeout_secs, 20);
        assert_eq!(cfg.browser.action_timeout_secs, 30);
        assert_eq!(cfg.browser.output_max_chars, 50_000);
        assert_eq!(cfg.approval.policy, ApprovalPolicy::Ask);
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn signet_dir_defaults_to_hermes_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var("HERMES_HOME").ok();

        unsafe {
            std::env::set_var("HERMES_HOME", tmp.path());
        }

        let cfg = AppConfig::default();
        assert_eq!(cfg.signet_dir(), tmp.path().join("signet"));

        match previous {
            Some(value) => unsafe { std::env::set_var("HERMES_HOME", value) },
            None => unsafe { std::env::remove_var("HERMES_HOME") },
        }
    }

    #[test]
    fn signet_dir_prefers_explicit_config_dir() {
        let cfg = AppConfig {
            signet: SignetConfig {
                enabled: true,
                key_name: "managed".to_string(),
                owner: "team".to_string(),
                dir: Some(PathBuf::from("/tmp/hermes-signet")),
            },
            ..AppConfig::default()
        };

        assert_eq!(cfg.signet_dir(), PathBuf::from("/tmp/hermes-signet"));
    }

    #[test]
    fn config_with_browser_section() {
        let yaml = r#"
model: openai/gpt-4o
browser:
  headless: false
  sandbox: false
  launch_timeout_secs: 45
  action_timeout_secs: 15
  output_max_chars: 12000
  viewport_width: 1440
  viewport_height: 900
  executable: /usr/bin/chromium
"#;
        let cfg: AppConfig = serde_yaml_ng::from_str(yaml).expect("deserialize failed");
        assert!(!cfg.browser.headless);
        assert!(!cfg.browser.sandbox);
        assert_eq!(cfg.browser.launch_timeout_secs, 45);
        assert_eq!(cfg.browser.action_timeout_secs, 15);
        assert_eq!(cfg.browser.output_max_chars, 12_000);
        assert_eq!(cfg.browser.viewport_width, 1440);
        assert_eq!(cfg.browser.viewport_height, 900);
        assert_eq!(
            cfg.browser.executable,
            Some(PathBuf::from("/usr/bin/chromium"))
        );
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
    fn mcp_server_defaults_enabled() {
        let yaml = r#"
model: openai/gpt-4o
mcp_servers:
  - name: demo
    command: /usr/bin/demo-mcp
"#;
        let cfg: AppConfig = serde_yaml_ng::from_str(yaml).expect("deserialize failed");
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].name, "demo");
        assert_eq!(cfg.mcp_servers[0].transport, McpTransportKind::Stdio);
        assert!(cfg.mcp_servers[0].enabled);
        assert!(cfg.mcp_servers[0].args.is_empty());
    }

    #[test]
    fn mcp_http_server_config_deserializes() {
        let yaml = r#"
model: openai/gpt-4o
mcp_servers:
  - name: remote-docs
    transport: http
    url: https://mcp.example.com
    headers:
      Authorization: Bearer test-token
"#;
        let cfg: AppConfig = serde_yaml_ng::from_str(yaml).expect("deserialize failed");
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].transport, McpTransportKind::Http);
        assert_eq!(
            cfg.mcp_servers[0].url.as_deref(),
            Some("https://mcp.example.com")
        );
        assert_eq!(
            cfg.mcp_servers[0]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer test-token")
        );
        assert!(cfg.mcp_servers[0].command.is_empty());
    }

    #[test]
    fn gateway_config_parsing() {
        let yaml = r#"
model: "openai/gpt-4o"
gateway:
  session_idle_timeout_secs: 3600
  telegram:
    token: "test-token"
    allow_all: true
  api_server:
    bind_addr: "127.0.0.1:9090"
    api_key: "secret"
"#;
        let cfg: AppConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let gw = cfg.gateway.unwrap();
        assert_eq!(gw.session_idle_timeout_secs, 3600);
        let tg = gw.telegram.unwrap();
        assert_eq!(tg.token, "test-token");
        assert!(tg.allow_all);
        let api = gw.api_server.unwrap();
        assert_eq!(api.bind_addr, "127.0.0.1:9090");
        assert_eq!(api.api_key, Some("secret".into()));
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
        assert!(tc.browser.headless);
        assert!(tc.browser.sandbox);
        assert_eq!(tc.browser.launch_timeout_secs, 20);
        assert_eq!(tc.browser.action_timeout_secs, 30);
        assert_eq!(tc.browser.output_max_chars, 50_000);
        assert_eq!(tc.workspace_root, root);
        assert!(tc.file.blocked_prefixes.contains(&PathBuf::from("/etc/")));
    }
}
