use std::collections::HashSet;

use hermes_config::config::{AppConfig, McpServerConfig, McpTransportKind};
use hermes_core::error::{HermesError, Result};

pub const MANAGED_BETA_ALLOWED_TOOLS: &[&str] = &[
    "read_file",
    "search_files",
    "write_file",
    "patch",
    "memory_read",
    "memory_write",
    "web_search",
    "web_extract",
    "vision_analyze",
    "skill_list",
    "skill_view",
];

pub const MANAGED_MCP_READ_ONLY_TOOLS: &[&str] = &[
    "mcp_prompt_list",
    "mcp_prompt_get",
    "mcp_resource_list",
    "mcp_resource_template_list",
    "mcp_resource_read",
];

pub const MANAGED_MCP_SIDE_EFFECT_TOOLS: &[&str] = &[
    "mcp_resource_subscribe",
    "mcp_resource_unsubscribe",
    "mcp_resource_updates",
];

pub fn is_managed_beta_allowed_tool(name: &str) -> bool {
    MANAGED_BETA_ALLOWED_TOOLS.contains(&name)
}

pub fn is_managed_mcp_read_only_tool(name: &str) -> bool {
    MANAGED_MCP_READ_ONLY_TOOLS.contains(&name)
}

pub fn is_managed_mcp_side_effect_tool(name: &str) -> bool {
    MANAGED_MCP_SIDE_EFFECT_TOOLS.contains(&name)
}

pub fn is_managed_mcp_tool(name: &str) -> bool {
    is_managed_mcp_read_only_tool(name) || is_managed_mcp_side_effect_tool(name)
}

pub fn validate_managed_mcp_policy(app_config: &AppConfig) -> Result<()> {
    let policy = &app_config.managed.mcp;
    if !policy.enabled
        && policy.allowed_servers.is_empty()
        && policy.allowed_transports.is_empty()
        && policy.stdio.allowed_servers.is_empty()
        && policy.stdio.allowed_env_keys.is_empty()
    {
        return Ok(());
    }

    validate_managed_mcp_stdio_policy(app_config)?;

    let mut unknown = policy
        .allowed_servers
        .iter()
        .filter(|server_name| {
            !app_config.mcp_servers.iter().any(|server| {
                server.enabled
                    && server.name == **server_name
                    && server.transport == McpTransportKind::Http
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    unknown.sort();
    unknown.dedup();

    if !unknown.is_empty() {
        return Err(HermesError::Config(format!(
            "managed.mcp.allowed_servers references unknown, disabled, or non-HTTP MCP servers: {}",
            unknown.join(", ")
        )));
    }

    if policy.enabled
        && !policy.allowed_servers.is_empty()
        && !policy.allowed_transports.contains(&McpTransportKind::Http)
    {
        return Err(HermesError::Config(
            "managed MCP policy requires `http` in managed.mcp.allowed_transports when allowlisting MCP servers"
                .to_string(),
        ));
    }

    Ok(())
}

fn validate_managed_mcp_stdio_policy(app_config: &AppConfig) -> Result<()> {
    let policy = &app_config.managed.mcp;
    let stdio_policy = &policy.stdio;
    let stdio_transport_enabled = policy.allowed_transports.contains(&McpTransportKind::Stdio);

    if !stdio_transport_enabled
        && stdio_policy.allowed_servers.is_empty()
        && stdio_policy.allowed_env_keys.is_empty()
    {
        return Ok(());
    }

    if stdio_transport_enabled && stdio_policy.allowed_servers.is_empty() {
        return Err(HermesError::Config(
            "managed MCP stdio policy requires at least one allowlisted stdio server in managed.mcp.stdio.allowed_servers"
                .to_string(),
        ));
    }

    if !stdio_transport_enabled && !stdio_policy.allowed_servers.is_empty() {
        return Err(HermesError::Config(
            "managed MCP stdio policy requires `stdio` in managed.mcp.allowed_transports when allowlisting stdio MCP servers"
                .to_string(),
        ));
    }

    if !stdio_transport_enabled
        && stdio_policy.allowed_servers.is_empty()
        && !stdio_policy.allowed_env_keys.is_empty()
    {
        return Err(HermesError::Config(
            "managed MCP stdio policy requires at least one allowlisted stdio server before env-key policy can be configured"
                .to_string(),
        ));
    }

    let allowed_stdio_servers = stdio_policy
        .allowed_servers
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let allowed_env_keys = stdio_policy
        .allowed_env_keys
        .iter()
        .cloned()
        .collect::<HashSet<_>>();

    let mut unknown = stdio_policy
        .allowed_servers
        .iter()
        .filter(|server_name| {
            !app_config.mcp_servers.iter().any(|server| {
                server.enabled
                    && server.name == **server_name
                    && server.transport == McpTransportKind::Stdio
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    unknown.sort();
    unknown.dedup();
    if !unknown.is_empty() {
        return Err(HermesError::Config(format!(
            "managed.mcp.stdio.allowed_servers references unknown, disabled, or non-stdio MCP servers: {}",
            unknown.join(", ")
        )));
    }

    let mut missing_commands = app_config
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.transport == McpTransportKind::Stdio
                && allowed_stdio_servers.contains(&server.name)
                && server.command.trim().is_empty()
        })
        .map(|server| server.name.clone())
        .collect::<Vec<_>>();
    missing_commands.sort();
    missing_commands.dedup();
    if !missing_commands.is_empty() {
        return Err(HermesError::Config(format!(
            "managed.mcp.stdio.allowed_servers references stdio MCP servers without an explicit command: {}",
            missing_commands.join(", ")
        )));
    }

    let mut missing_env = app_config
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.transport == McpTransportKind::Stdio
                && allowed_stdio_servers.contains(&server.name)
        })
        .filter_map(|server| {
            let mut missing_keys = server
                .env
                .keys()
                .filter(|key| !allowed_env_keys.contains(*key))
                .cloned()
                .collect::<Vec<_>>();
            missing_keys.sort();
            missing_keys.dedup();
            if missing_keys.is_empty() {
                None
            } else {
                Some(format!("{}: {}", server.name, missing_keys.join(", ")))
            }
        })
        .collect::<Vec<_>>();
    missing_env.sort();
    missing_env.dedup();
    if !missing_env.is_empty() {
        return Err(HermesError::Config(format!(
            "managed.mcp.stdio.allowed_env_keys must explicitly allow env keys required by allowlisted stdio MCP servers: {}",
            missing_env.join("; ")
        )));
    }

    Ok(())
}

pub fn managed_mcp_allowed_http_server_configs(app_config: &AppConfig) -> Vec<McpServerConfig> {
    if !app_config.managed.mcp.enabled {
        return Vec::new();
    }

    let allowed = app_config
        .managed
        .mcp
        .allowed_servers
        .iter()
        .cloned()
        .collect::<HashSet<_>>();

    app_config
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.transport == McpTransportKind::Http
                && allowed.contains(&server.name)
        })
        .cloned()
        .collect()
}

pub fn managed_mcp_blocked_http_server_configs(app_config: &AppConfig) -> Vec<McpServerConfig> {
    let allowed = app_config
        .managed
        .mcp
        .allowed_servers
        .iter()
        .cloned()
        .collect::<HashSet<_>>();

    app_config
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.transport == McpTransportKind::Http
                && !allowed.contains(&server.name)
        })
        .cloned()
        .collect()
}

pub fn validate_managed_beta_tools(allowed_tools: &[String]) -> Result<()> {
    let allowed = MANAGED_BETA_ALLOWED_TOOLS
        .iter()
        .chain(MANAGED_MCP_READ_ONLY_TOOLS.iter())
        .copied()
        .collect::<HashSet<_>>();

    let mut disallowed = allowed_tools
        .iter()
        .filter(|name| !allowed.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    disallowed.sort();
    disallowed.dedup();

    if disallowed.is_empty() {
        return Ok(());
    }

    Err(HermesError::Config(format!(
        "managed beta tool allowlist contains unsupported tools: {}",
        disallowed.join(", ")
    )))
}

pub fn validate_managed_runtime_tool_policy(
    allowed_tools: &[String],
    app_config: &AppConfig,
) -> Result<()> {
    let _ = allowed_tools;
    validate_managed_mcp_policy(app_config)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_config::config::{
        ManagedConfigYaml, ManagedMcpPolicyYaml, ManagedMcpStdioPolicyYaml, McpServerConfig,
    };

    #[test]
    fn beta_policy_accepts_supported_tools() {
        let tools = vec![
            "read_file".to_string(),
            "skill_view".to_string(),
            "mcp_resource_read".to_string(),
        ];
        validate_managed_beta_tools(&tools).unwrap();
    }

    #[test]
    fn beta_policy_rejects_unsupported_tools() {
        let err = validate_managed_beta_tools(&[
            "read_file".to_string(),
            "terminal".to_string(),
            "mcp_resource_subscribe".to_string(),
            "browser".to_string(),
        ])
        .unwrap_err();

        assert!(err.to_string().contains("terminal"));
        assert!(err.to_string().contains("mcp_resource_subscribe"));
        assert!(err.to_string().contains("browser"));
    }

    #[test]
    fn managed_mcp_policy_requires_stdio_allowlist_when_transport_enabled() {
        let app_config = AppConfig {
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Stdio],
                    allowed_servers: vec![],
                    allow_side_effects: false,
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let err = validate_managed_mcp_policy(&app_config).unwrap_err();
        assert!(
            err.to_string()
                .contains("at least one allowlisted stdio server")
        );
    }

    #[test]
    fn managed_mcp_policy_rejects_unknown_or_non_http_servers() {
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "local-docs".to_string(),
                transport: McpTransportKind::Stdio,
                command: "demo".to_string(),
                ..McpServerConfig::default()
            }],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Http],
                    allowed_servers: vec!["local-docs".to_string(), "missing".to_string()],
                    allow_side_effects: false,
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let err = validate_managed_mcp_policy(&app_config).unwrap_err();
        assert!(err.to_string().contains("local-docs"));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn managed_mcp_allowed_http_server_configs_returns_allowlisted_http_servers() {
        let app_config = AppConfig {
            mcp_servers: vec![
                McpServerConfig {
                    name: "docs".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some("https://docs.example.com".to_string()),
                    enabled: true,
                    ..McpServerConfig::default()
                },
                McpServerConfig {
                    name: "blocked".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some("https://blocked.example.com".to_string()),
                    enabled: true,
                    ..McpServerConfig::default()
                },
            ],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Http],
                    allowed_servers: vec!["docs".to_string()],
                    allow_side_effects: false,
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let configs = managed_mcp_allowed_http_server_configs(&app_config);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "docs");
    }

    #[test]
    fn managed_mcp_blocked_http_server_configs_returns_non_allowlisted_http_servers() {
        let app_config = AppConfig {
            mcp_servers: vec![
                McpServerConfig {
                    name: "docs".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some("https://docs.example.com".to_string()),
                    enabled: true,
                    ..McpServerConfig::default()
                },
                McpServerConfig {
                    name: "archive".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some("https://archive.example.com".to_string()),
                    enabled: true,
                    ..McpServerConfig::default()
                },
                McpServerConfig {
                    name: "filesystem".to_string(),
                    transport: McpTransportKind::Stdio,
                    command: "filesystem-mcp".to_string(),
                    enabled: true,
                    ..McpServerConfig::default()
                },
            ],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Http],
                    allowed_servers: vec!["docs".to_string()],
                    allow_side_effects: false,
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let configs = managed_mcp_blocked_http_server_configs(&app_config);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "archive");
    }

    #[test]
    fn managed_mcp_stdio_policy_rejects_unknown_or_non_stdio_servers() {
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "remote-docs".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://mcp.example.com".to_string()),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Stdio],
                    stdio: ManagedMcpStdioPolicyYaml {
                        allowed_servers: vec!["remote-docs".to_string(), "missing".to_string()],
                        allowed_env_keys: vec![],
                    },
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let err = validate_managed_mcp_policy(&app_config).unwrap_err();
        assert!(err.to_string().contains("remote-docs"));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn managed_mcp_stdio_policy_requires_explicit_env_key_allowlist() {
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "local-docs".to_string(),
                transport: McpTransportKind::Stdio,
                command: "/usr/bin/docs-mcp".to_string(),
                env: std::collections::BTreeMap::from([
                    ("DOCS_TOKEN".to_string(), "secret".to_string()),
                    ("DOCS_REGION".to_string(), "us".to_string()),
                ]),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Stdio],
                    stdio: ManagedMcpStdioPolicyYaml {
                        allowed_servers: vec!["local-docs".to_string()],
                        allowed_env_keys: vec!["DOCS_TOKEN".to_string()],
                    },
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let err = validate_managed_mcp_policy(&app_config).unwrap_err();
        assert!(err.to_string().contains("DOCS_REGION"));
    }

    #[test]
    fn managed_mcp_stdio_policy_accepts_explicit_server_and_env_policy() {
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "local-docs".to_string(),
                transport: McpTransportKind::Stdio,
                command: "/usr/bin/docs-mcp".to_string(),
                env: std::collections::BTreeMap::from([
                    ("DOCS_TOKEN".to_string(), "secret".to_string()),
                    ("DOCS_REGION".to_string(), "us".to_string()),
                ]),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Stdio],
                    stdio: ManagedMcpStdioPolicyYaml {
                        allowed_servers: vec!["local-docs".to_string()],
                        allowed_env_keys: vec!["DOCS_REGION".to_string(), "DOCS_TOKEN".to_string()],
                    },
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        validate_managed_mcp_policy(&app_config).unwrap();
    }
}
