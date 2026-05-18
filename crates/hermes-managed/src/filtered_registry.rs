use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
};

use hermes_config::config::{AppConfig, McpTransportKind};
use hermes_core::error::HermesError;
use hermes_core::tool::Tool;
use hermes_mcp::{McpRegistryBuildOptions, populate_registry_with_options};
use hermes_tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tool_policy::{
    is_managed_beta_allowed_tool, is_managed_mcp_read_only_tool, is_managed_mcp_side_effect_tool,
    managed_mcp_allowed_http_server_configs, managed_mcp_blocked_http_server_configs,
    validate_managed_runtime_tool_policy,
};
use crate::types::{ManagedRunEvent, ManagedRunEventKind};

const MCP_PROMPT_READ_ONLY_TOOLS: &[&str] = &["mcp_prompt_list", "mcp_prompt_get"];
const MCP_RESOURCE_READ_ONLY_TOOLS: &[&str] = &[
    "mcp_resource_list",
    "mcp_resource_template_list",
    "mcp_resource_read",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedMcpAdmissionRejection {
    pub code: String,
    pub error: String,
    pub requested_tools: Vec<String>,
    pub requested_read_only_tools: Vec<String>,
    pub requested_side_effect_tools: Vec<String>,
    pub requested_dynamic_tools: Vec<String>,
    pub allowed_servers: Vec<String>,
    pub allowed_transports: Vec<String>,
    pub allow_side_effects: bool,
    pub allowed_stdio_servers: Vec<String>,
    pub allowed_stdio_env_keys: Vec<String>,
    pub stdio_server_summaries: Vec<ManagedMcpStdioServerSummary>,
    #[serde(
        default,
        skip_serializing_if = "ManagedMcpReadOnlyCapabilityAttribution::is_empty"
    )]
    pub read_only_capability_attribution: ManagedMcpReadOnlyCapabilityAttribution,
}

#[derive(Debug, Error)]
pub enum ManagedRegistryBuildError {
    #[error(transparent)]
    Config(#[from] HermesError),
    #[error("{message}")]
    McpAdmissionRejected {
        rejection: Box<ManagedMcpAdmissionRejection>,
        message: String,
    },
}

impl ManagedRegistryBuildError {
    pub fn mcp_admission_rejection(&self) -> Option<&ManagedMcpAdmissionRejection> {
        match self {
            Self::McpAdmissionRejected { rejection, .. } => Some(rejection),
            Self::Config(_) => None,
        }
    }
}

fn managed_registry_rejection(
    rejection: ManagedMcpAdmissionRejection,
) -> ManagedRegistryBuildError {
    let message = rejection.error.clone();
    ManagedRegistryBuildError::McpAdmissionRejected {
        rejection: Box::new(rejection),
        message,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedMcpStdioServerSummary {
    pub name: String,
    pub command: String,
    pub arg_count: usize,
    pub cwd_configured: bool,
    pub env_keys: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedMcpReadOnlyCapabilityAttribution {
    #[serde(default)]
    pub prompt_tools: Vec<String>,
    #[serde(default)]
    pub resource_tools: Vec<String>,
    #[serde(default)]
    pub blocked_http_prompt_servers: Vec<String>,
    #[serde(default)]
    pub blocked_http_resource_servers: Vec<String>,
}

impl ManagedMcpReadOnlyCapabilityAttribution {
    pub fn is_empty(&self) -> bool {
        self.prompt_tools.is_empty()
            && self.resource_tools.is_empty()
            && self.blocked_http_prompt_servers.is_empty()
            && self.blocked_http_resource_servers.is_empty()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ManagedMcpHttpCapabilityProbe {
    prompt_servers: Vec<String>,
    resource_servers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedMcpPolicyGateFailure {
    DisabledByOperatorPolicy,
    NoAllowlistedHttpServers,
    HttpTransportNotAllowed,
}

impl ManagedMcpPolicyGateFailure {
    fn code(self) -> &'static str {
        match self {
            Self::DisabledByOperatorPolicy => "disabled_by_operator_policy",
            Self::NoAllowlistedHttpServers => "no_allowlisted_http_servers",
            Self::HttpTransportNotAllowed => "http_transport_not_allowed",
        }
    }

    fn message(self, requested: &[String]) -> String {
        match self {
            Self::DisabledByOperatorPolicy => format!(
                "managed MCP tools are disabled by operator policy: {}",
                requested.join(", ")
            ),
            Self::NoAllowlistedHttpServers => format!(
                "managed MCP policy requires at least one allowlisted HTTP server before MCP tools can be admitted: {}",
                requested.join(", ")
            ),
            Self::HttpTransportNotAllowed => format!(
                "managed MCP policy does not currently admit HTTP MCP servers: {}",
                requested.join(", ")
            ),
        }
    }
}

pub async fn build_filtered_registry(
    source: &ToolRegistry,
    allowed_tools: &[String],
    app_config: &AppConfig,
) -> std::result::Result<ToolRegistry, ManagedRegistryBuildError> {
    validate_managed_runtime_tool_policy(allowed_tools, app_config)?;

    let allowed = allowed_tools.iter().cloned().collect::<HashSet<_>>();
    let mut allowed_non_mcp = Vec::new();
    let mut requested_read_only_mcp = Vec::new();
    let mut requested_side_effect_mcp = Vec::new();
    let mut requested_dynamic_mcp = Vec::new();
    let mut missing = Vec::new();
    let mut disallowed = Vec::new();

    for name in &allowed {
        if is_managed_mcp_read_only_tool(name) {
            requested_read_only_mcp.push(name.clone());
            continue;
        }
        if is_managed_mcp_side_effect_tool(name) {
            requested_side_effect_mcp.push(name.clone());
            continue;
        }

        match source.get(name) {
            Some(tool) if tool.toolset() == "mcp" => requested_dynamic_mcp.push(name.clone()),
            Some(_) if !is_managed_beta_allowed_tool(name) => disallowed.push(name.clone()),
            Some(_) => allowed_non_mcp.push(name.clone()),
            None => missing.push(name.clone()),
        }
    }

    disallowed.sort();
    disallowed.dedup();
    if !disallowed.is_empty() {
        return Err(ManagedRegistryBuildError::Config(HermesError::Config(
            format!(
                "managed beta tool allowlist contains unsupported tools: {}",
                disallowed.join(", ")
            ),
        )));
    }

    requested_dynamic_mcp.sort();
    requested_dynamic_mcp.dedup();
    if !requested_dynamic_mcp.is_empty() {
        if let Some(message) =
            managed_mcp_stdio_only_dynamic_message(source, &requested_dynamic_mcp, app_config)
        {
            return Err(managed_registry_rejection(
                build_managed_mcp_admission_rejection(
                    "stdio_dynamic_tools_not_admitted",
                    message,
                    Vec::new(),
                    Vec::new(),
                    requested_dynamic_mcp.clone(),
                    app_config,
                    ManagedMcpReadOnlyCapabilityAttribution::default(),
                ),
            ));
        }
        if let Some(gate) = managed_mcp_policy_gate_failure(app_config) {
            return Err(managed_registry_rejection(
                build_managed_mcp_admission_rejection(
                    gate.code(),
                    gate.message(&requested_dynamic_mcp),
                    Vec::new(),
                    Vec::new(),
                    requested_dynamic_mcp.clone(),
                    app_config,
                    ManagedMcpReadOnlyCapabilityAttribution::default(),
                ),
            ));
        }
        return Err(managed_registry_rejection(
            build_managed_mcp_admission_rejection(
                "dynamic_tools_not_admitted",
                format!(
                    "managed runtime does not admit model-callable MCP tools: {}",
                    requested_dynamic_mcp.join(", ")
                ),
                Vec::new(),
                Vec::new(),
                requested_dynamic_mcp.clone(),
                app_config,
                ManagedMcpReadOnlyCapabilityAttribution::default(),
            ),
        ));
    }

    requested_side_effect_mcp.sort();
    requested_side_effect_mcp.dedup();
    if !requested_side_effect_mcp.is_empty() {
        if let Some(message) =
            managed_mcp_stdio_only_side_effect_message(&requested_side_effect_mcp, app_config)
        {
            return Err(managed_registry_rejection(
                build_managed_mcp_admission_rejection(
                    "stdio_side_effect_tools_not_admitted",
                    message,
                    Vec::new(),
                    requested_side_effect_mcp.clone(),
                    Vec::new(),
                    app_config,
                    ManagedMcpReadOnlyCapabilityAttribution::default(),
                ),
            ));
        }
        if let Some(gate) = managed_mcp_policy_gate_failure(app_config) {
            return Err(managed_registry_rejection(
                build_managed_mcp_admission_rejection(
                    gate.code(),
                    gate.message(&requested_side_effect_mcp),
                    Vec::new(),
                    requested_side_effect_mcp.clone(),
                    Vec::new(),
                    app_config,
                    ManagedMcpReadOnlyCapabilityAttribution::default(),
                ),
            ));
        }
        if !app_config.managed.mcp.allow_side_effects {
            return Err(managed_registry_rejection(
                build_managed_mcp_admission_rejection(
                    "side_effects_disabled_by_operator_policy",
                    format!(
                        "managed MCP side-effect tools are disabled by operator policy: {}",
                        requested_side_effect_mcp.join(", ")
                    ),
                    Vec::new(),
                    requested_side_effect_mcp.clone(),
                    Vec::new(),
                    app_config,
                    ManagedMcpReadOnlyCapabilityAttribution::default(),
                ),
            ));
        }
        return Err(managed_registry_rejection(
            build_managed_mcp_admission_rejection(
                "side_effect_tools_not_admitted",
                format!(
                    "managed runtime does not yet admit MCP side-effect tools: {}",
                    requested_side_effect_mcp.join(", ")
                ),
                Vec::new(),
                requested_side_effect_mcp.clone(),
                Vec::new(),
                app_config,
                ManagedMcpReadOnlyCapabilityAttribution::default(),
            ),
        ));
    }

    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        return Err(ManagedRegistryBuildError::Config(HermesError::Config(
            format!(
                "managed tool allowlist references unknown tools: {}",
                missing.join(", ")
            ),
        )));
    }

    let filtered = ToolRegistry::new();
    filtered.extend_from(source, allowed_non_mcp.iter().map(String::as_str));

    requested_read_only_mcp.sort();
    requested_read_only_mcp.dedup();
    if requested_read_only_mcp.is_empty() {
        return Ok(filtered);
    }

    if let Some(message) =
        managed_mcp_stdio_only_read_only_message(&requested_read_only_mcp, app_config)
    {
        return Err(managed_registry_rejection(
            build_managed_mcp_admission_rejection(
                "stdio_read_only_tools_not_admitted",
                message,
                requested_read_only_mcp.clone(),
                Vec::new(),
                Vec::new(),
                app_config,
                ManagedMcpReadOnlyCapabilityAttribution::default(),
            ),
        ));
    }

    if let Some(gate) = managed_mcp_policy_gate_failure(app_config) {
        return Err(managed_registry_rejection(
            build_managed_mcp_admission_rejection(
                gate.code(),
                gate.message(&requested_read_only_mcp),
                requested_read_only_mcp.clone(),
                Vec::new(),
                Vec::new(),
                app_config,
                ManagedMcpReadOnlyCapabilityAttribution::default(),
            ),
        ));
    }

    let configs = managed_mcp_allowed_http_server_configs(app_config);
    let managed_mcp_registry = Arc::new(ToolRegistry::new());
    populate_registry_with_options(
        Arc::clone(&managed_mcp_registry),
        &configs,
        McpRegistryBuildOptions::managed_http_read_only(),
    )
    .await;

    let mut unavailable = Vec::new();
    for name in &requested_read_only_mcp {
        if managed_mcp_registry.get(name).is_none() {
            unavailable.push(name.clone());
        }
    }
    if !unavailable.is_empty() {
        let mut server_names = configs
            .iter()
            .map(|config| config.name.clone())
            .collect::<Vec<_>>();
        server_names.sort();
        let blocked_probe = probe_blocked_http_read_only_capabilities(
            &managed_mcp_blocked_http_server_configs(app_config),
        )
        .await;
        let message = managed_mcp_read_only_capability_unavailable_message(
            &unavailable,
            &server_names,
            &blocked_probe,
        );
        let (code, attribution) =
            read_only_capability_rejection_details(&unavailable, &blocked_probe);
        return Err(managed_registry_rejection(
            build_managed_mcp_admission_rejection(
                code,
                message,
                requested_read_only_mcp.clone(),
                Vec::new(),
                Vec::new(),
                app_config,
                attribution,
            ),
        ));
    }

    filtered.extend_from(
        managed_mcp_registry.as_ref(),
        requested_read_only_mcp.iter().map(String::as_str),
    );

    Ok(filtered)
}

fn managed_mcp_policy_gate_failure(app_config: &AppConfig) -> Option<ManagedMcpPolicyGateFailure> {
    let policy = &app_config.managed.mcp;
    if !policy.enabled {
        return Some(ManagedMcpPolicyGateFailure::DisabledByOperatorPolicy);
    }
    if policy.allowed_servers.is_empty() {
        return Some(ManagedMcpPolicyGateFailure::NoAllowlistedHttpServers);
    }
    if !policy.allowed_transports.contains(&McpTransportKind::Http) {
        return Some(ManagedMcpPolicyGateFailure::HttpTransportNotAllowed);
    }
    None
}

fn managed_mcp_stdio_only_dynamic_message(
    source: &ToolRegistry,
    requested_dynamic_mcp: &[String],
    app_config: &AppConfig,
) -> Option<String> {
    let tool_servers = requested_dynamic_mcp
        .iter()
        .filter_map(|tool_name| {
            source
                .get(tool_name)
                .and_then(|tool| inferred_mcp_server_name(tool.as_ref()))
                .map(|server_name| (tool_name.clone(), server_name))
        })
        .collect::<Vec<_>>();
    if tool_servers.is_empty() {
        return None;
    }

    let transport_by_server = app_config
        .mcp_servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| (server.name.as_str(), server.transport.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut stdio_only_tools = Vec::new();
    let mut stdio_servers = BTreeSet::new();
    for (tool_name, server_name) in tool_servers {
        if matches!(
            transport_by_server.get(server_name.as_str()),
            Some(McpTransportKind::Stdio)
        ) {
            stdio_servers.insert(server_name);
            stdio_only_tools.push(tool_name);
        }
    }

    if stdio_only_tools.is_empty() {
        return None;
    }

    stdio_only_tools.sort();
    stdio_only_tools.dedup();
    Some(format!(
        "managed runtime does not yet admit model-callable MCP tools that currently resolve only to stdio servers: {} (servers: {})",
        stdio_only_tools.join(", "),
        stdio_servers.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

fn managed_mcp_stdio_only_side_effect_message(
    requested_side_effect_mcp: &[String],
    app_config: &AppConfig,
) -> Option<String> {
    let stdio_servers = app_config
        .mcp_servers
        .iter()
        .filter(|server| server.enabled && server.transport == McpTransportKind::Stdio)
        .map(|server| server.name.clone())
        .collect::<BTreeSet<_>>();

    let mut requested_tools = requested_side_effect_mcp.to_vec();
    requested_tools.sort();
    requested_tools.dedup();
    Some(format!(
        "managed runtime does not yet admit stdio-only MCP bridge tools: {} (candidate stdio servers: {})",
        requested_tools.join(", "),
        join_or_dash(stdio_servers.into_iter().collect())
    ))
}

fn managed_mcp_stdio_only_read_only_message(
    requested_read_only_mcp: &[String],
    app_config: &AppConfig,
) -> Option<String> {
    let policy = &app_config.managed.mcp;
    if !policy.enabled {
        return None;
    }
    if !policy.allowed_servers.is_empty()
        && policy.allowed_transports.contains(&McpTransportKind::Http)
    {
        return None;
    }

    let mut stdio_servers = policy.stdio.allowed_servers.clone();
    stdio_servers.sort();
    stdio_servers.dedup();
    if stdio_servers.is_empty() {
        return None;
    }

    let mut requested_tools = requested_read_only_mcp.to_vec();
    requested_tools.sort();
    requested_tools.dedup();
    Some(format!(
        "managed runtime currently admits read-only MCP bridge tools only through allowlisted HTTP MCP servers; current operator policy leaves only stdio MCP candidates for these tools: {} (candidate stdio servers: {})",
        requested_tools.join(", "),
        join_or_dash(stdio_servers)
    ))
}

fn is_prompt_read_only_tool(name: &str) -> bool {
    MCP_PROMPT_READ_ONLY_TOOLS.contains(&name)
}

fn is_resource_read_only_tool(name: &str) -> bool {
    MCP_RESOURCE_READ_ONLY_TOOLS.contains(&name)
}

fn managed_mcp_read_only_capability_unavailable_message(
    unavailable: &[String],
    server_names: &[String],
    blocked_probe: &ManagedMcpHttpCapabilityProbe,
) -> String {
    let mut prompt_tools = unavailable
        .iter()
        .filter(|name| is_prompt_read_only_tool(name))
        .cloned()
        .collect::<Vec<_>>();
    prompt_tools.sort();
    prompt_tools.dedup();

    let mut resource_tools = unavailable
        .iter()
        .filter(|name| is_resource_read_only_tool(name))
        .cloned()
        .collect::<Vec<_>>();
    resource_tools.sort();
    resource_tools.dedup();

    let servers = join_or_dash(server_names.to_vec());
    let blocked_prompt_servers = join_or_dash(blocked_probe.prompt_servers.clone());
    let blocked_resource_servers = join_or_dash(blocked_probe.resource_servers.clone());
    match (prompt_tools.is_empty(), resource_tools.is_empty()) {
        (false, true) => format!(
            "allowlisted managed MCP HTTP servers did not expose prompt capability required by requested read-only MCP tools: {} (allowlisted servers: {}; configured non-allowlisted HTTP servers exposing prompt capability: {})",
            prompt_tools.join(", "),
            servers,
            blocked_prompt_servers
        ),
        (true, false) => format!(
            "allowlisted managed MCP HTTP servers did not expose resource capability required by requested read-only MCP tools: {} (allowlisted servers: {}; configured non-allowlisted HTTP servers exposing resource capability: {})",
            resource_tools.join(", "),
            servers,
            blocked_resource_servers
        ),
        (false, false) => format!(
            "allowlisted managed MCP HTTP servers did not expose required read-only MCP capabilities: prompts => {} (configured non-allowlisted HTTP servers exposing prompt capability: {}); resources => {} (configured non-allowlisted HTTP servers exposing resource capability: {}) (allowlisted servers: {})",
            prompt_tools.join(", "),
            blocked_prompt_servers,
            resource_tools.join(", "),
            blocked_resource_servers,
            servers
        ),
        (true, true) => format!(
            "allowlisted managed MCP HTTP servers could not expose requested read-only tools; they may be unreachable or lack the required capabilities: {} (servers: {})",
            unavailable.join(", "),
            servers
        ),
    }
}

async fn probe_blocked_http_read_only_capabilities(
    configs: &[hermes_config::config::McpServerConfig],
) -> ManagedMcpHttpCapabilityProbe {
    let mut probe = ManagedMcpHttpCapabilityProbe::default();
    for config in configs {
        let registry = Arc::new(ToolRegistry::new());
        populate_registry_with_options(
            Arc::clone(&registry),
            std::slice::from_ref(config),
            McpRegistryBuildOptions::managed_http_read_only(),
        )
        .await;
        if registry.contains("mcp_prompt_list") || registry.contains("mcp_prompt_get") {
            probe.prompt_servers.push(config.name.clone());
        }
        if registry.contains("mcp_resource_list")
            || registry.contains("mcp_resource_template_list")
            || registry.contains("mcp_resource_read")
        {
            probe.resource_servers.push(config.name.clone());
        }
    }
    probe.prompt_servers.sort();
    probe.prompt_servers.dedup();
    probe.resource_servers.sort();
    probe.resource_servers.dedup();
    probe
}

fn inferred_mcp_server_name(tool: &dyn Tool) -> Option<String> {
    let description = tool.schema().description;
    let (_, tail) = description.rsplit_once("[MCP: ")?;
    Some(tail.trim_end_matches(']').trim().to_string())
}

fn join_or_dash(values: Vec<String>) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

fn extract_named_list_segment(message: &str, prefix: &str) -> Vec<String> {
    let Some((_, tail)) = message.split_once(prefix) else {
        return Vec::new();
    };
    let value = tail
        .split(['(', ')', ';'])
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if value.is_empty() || value == "-" {
        Vec::new()
    } else {
        value
            .split(',')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn build_read_only_capability_attribution(
    code: &str,
    error: &str,
) -> ManagedMcpReadOnlyCapabilityAttribution {
    match code {
        "read_only_prompt_capability_unavailable"
        | "read_only_prompt_capability_blocked_by_allowlist" => {
            ManagedMcpReadOnlyCapabilityAttribution {
                prompt_tools: extract_named_list_segment(
                    error,
                    "allowlisted managed MCP HTTP servers did not expose prompt capability required by requested read-only MCP tools: ",
                ),
                blocked_http_prompt_servers: extract_named_list_segment(
                    error,
                    "configured non-allowlisted HTTP servers exposing prompt capability: ",
                ),
                ..ManagedMcpReadOnlyCapabilityAttribution::default()
            }
        }
        "read_only_resource_capability_unavailable"
        | "read_only_resource_capability_blocked_by_allowlist" => {
            ManagedMcpReadOnlyCapabilityAttribution {
                resource_tools: extract_named_list_segment(
                    error,
                    "allowlisted managed MCP HTTP servers did not expose resource capability required by requested read-only MCP tools: ",
                ),
                blocked_http_resource_servers: extract_named_list_segment(
                    error,
                    "configured non-allowlisted HTTP servers exposing resource capability: ",
                ),
                ..ManagedMcpReadOnlyCapabilityAttribution::default()
            }
        }
        "read_only_capabilities_unavailable" | "read_only_capabilities_blocked_by_allowlist" => {
            ManagedMcpReadOnlyCapabilityAttribution {
                prompt_tools: extract_named_list_segment(error, "prompts => "),
                resource_tools: extract_named_list_segment(error, "resources => "),
                blocked_http_prompt_servers: extract_named_list_segment(
                    error,
                    "configured non-allowlisted HTTP servers exposing prompt capability: ",
                ),
                blocked_http_resource_servers: extract_named_list_segment(
                    error,
                    "configured non-allowlisted HTTP servers exposing resource capability: ",
                ),
            }
        }
        _ => ManagedMcpReadOnlyCapabilityAttribution::default(),
    }
}

/// Legacy compatibility helper for rejection metadata that predates structured
/// `read_only_capability_attribution`.
pub fn normalize_legacy_managed_mcp_admission_rejection(
    rejection: &mut ManagedMcpAdmissionRejection,
) {
    if rejection.read_only_capability_attribution.is_empty() {
        rejection.read_only_capability_attribution =
            build_read_only_capability_attribution(&rejection.code, &rejection.error);
    }
}

pub fn managed_mcp_admission_rejection_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedMcpAdmissionRejection> {
    if event.kind != ManagedRunEventKind::RunMcpAdmissionRejected {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    let mut rejection: ManagedMcpAdmissionRejection =
        serde_json::from_value(metadata.clone()).ok()?;
    normalize_legacy_managed_mcp_admission_rejection(&mut rejection);
    Some(rejection)
}

fn build_managed_mcp_admission_rejection(
    code: impl Into<String>,
    error: impl Into<String>,
    requested_read_only_tools: Vec<String>,
    requested_side_effect_tools: Vec<String>,
    requested_dynamic_tools: Vec<String>,
    app_config: &AppConfig,
    read_only_capability_attribution: ManagedMcpReadOnlyCapabilityAttribution,
) -> ManagedMcpAdmissionRejection {
    let mut requested_tools = requested_read_only_tools
        .iter()
        .chain(requested_side_effect_tools.iter())
        .chain(requested_dynamic_tools.iter())
        .cloned()
        .collect::<Vec<_>>();
    requested_tools.sort();
    requested_tools.dedup();

    let mut allowed_servers = app_config.managed.mcp.allowed_servers.clone();
    allowed_servers.sort();
    allowed_servers.dedup();

    let mut allowed_transports = app_config
        .managed
        .mcp
        .allowed_transports
        .iter()
        .map(|transport| match transport {
            McpTransportKind::Stdio => "stdio".to_string(),
            McpTransportKind::Http => "http".to_string(),
        })
        .collect::<Vec<_>>();
    allowed_transports.sort();
    allowed_transports.dedup();

    let mut allowed_stdio_servers = app_config.managed.mcp.stdio.allowed_servers.clone();
    allowed_stdio_servers.sort();
    allowed_stdio_servers.dedup();

    let mut allowed_stdio_env_keys = app_config.managed.mcp.stdio.allowed_env_keys.clone();
    allowed_stdio_env_keys.sort();
    allowed_stdio_env_keys.dedup();

    let mut stdio_server_summaries = app_config
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.transport == McpTransportKind::Stdio
                && allowed_stdio_servers.contains(&server.name)
        })
        .map(|server| {
            let mut env_keys = server.env.keys().cloned().collect::<Vec<_>>();
            env_keys.sort();
            env_keys.dedup();
            ManagedMcpStdioServerSummary {
                name: server.name.clone(),
                command: server.command.clone(),
                arg_count: server.args.len(),
                cwd_configured: server.cwd.is_some(),
                env_keys,
            }
        })
        .collect::<Vec<_>>();
    stdio_server_summaries.sort_by(|left, right| left.name.cmp(&right.name));

    ManagedMcpAdmissionRejection {
        code: code.into(),
        error: error.into(),
        requested_tools,
        requested_read_only_tools,
        requested_side_effect_tools,
        requested_dynamic_tools,
        allowed_servers,
        allowed_transports,
        allow_side_effects: app_config.managed.mcp.allow_side_effects,
        allowed_stdio_servers,
        allowed_stdio_env_keys,
        stdio_server_summaries,
        read_only_capability_attribution,
    }
}

fn read_only_capability_rejection_details(
    unavailable: &[String],
    blocked_probe: &ManagedMcpHttpCapabilityProbe,
) -> (&'static str, ManagedMcpReadOnlyCapabilityAttribution) {
    let mut prompt_tools = unavailable
        .iter()
        .filter(|name| is_prompt_read_only_tool(name))
        .cloned()
        .collect::<Vec<_>>();
    prompt_tools.sort();
    prompt_tools.dedup();

    let mut resource_tools = unavailable
        .iter()
        .filter(|name| is_resource_read_only_tool(name))
        .cloned()
        .collect::<Vec<_>>();
    resource_tools.sort();
    resource_tools.dedup();

    let attribution = ManagedMcpReadOnlyCapabilityAttribution {
        prompt_tools,
        resource_tools,
        blocked_http_prompt_servers: blocked_probe.prompt_servers.clone(),
        blocked_http_resource_servers: blocked_probe.resource_servers.clone(),
    };

    let code = match (
        attribution.prompt_tools.is_empty(),
        attribution.resource_tools.is_empty(),
    ) {
        (false, true) if attribution.blocked_http_prompt_servers.is_empty() => {
            "read_only_prompt_capability_unavailable"
        }
        (false, true) => "read_only_prompt_capability_blocked_by_allowlist",
        (true, false) if attribution.blocked_http_resource_servers.is_empty() => {
            "read_only_resource_capability_unavailable"
        }
        (true, false) => "read_only_resource_capability_blocked_by_allowlist",
        (false, false)
            if attribution.blocked_http_prompt_servers.is_empty()
                && attribution.blocked_http_resource_servers.is_empty() =>
        {
            "read_only_capabilities_unavailable"
        }
        (false, false) => "read_only_capabilities_blocked_by_allowlist",
        (true, true) => "read_only_tools_unavailable",
    };

    (code, attribution)
}

/// Legacy compatibility parser for older call sites that only had an error
/// string. New managed runtime build/preflight paths construct structured
/// rejection payloads directly and should not use this helper.
pub fn parse_legacy_managed_mcp_admission_rejection(
    allowed_tools: &[String],
    app_config: &AppConfig,
    error: &str,
) -> Option<ManagedMcpAdmissionRejection> {
    let mut requested_read_only_tools = allowed_tools
        .iter()
        .filter(|name| is_managed_mcp_read_only_tool(name))
        .cloned()
        .collect::<Vec<_>>();
    requested_read_only_tools.sort();
    requested_read_only_tools.dedup();

    let mut requested_side_effect_tools = allowed_tools
        .iter()
        .filter(|name| is_managed_mcp_side_effect_tool(name))
        .cloned()
        .collect::<Vec<_>>();
    requested_side_effect_tools.sort();
    requested_side_effect_tools.dedup();

    let mut requested_dynamic_tools = allowed_tools
        .iter()
        .filter(|name| {
            !is_managed_beta_allowed_tool(name)
                && !is_managed_mcp_read_only_tool(name)
                && !is_managed_mcp_side_effect_tool(name)
        })
        .cloned()
        .collect::<Vec<_>>();
    requested_dynamic_tools.sort();
    requested_dynamic_tools.dedup();

    let mut requested_tools = requested_read_only_tools
        .iter()
        .chain(requested_side_effect_tools.iter())
        .chain(requested_dynamic_tools.iter())
        .cloned()
        .collect::<Vec<_>>();
    requested_tools.sort();
    requested_tools.dedup();

    let code = if error.contains("managed MCP tools are disabled by operator policy") {
        "disabled_by_operator_policy"
    } else if error.contains("managed MCP policy requires at least one allowlisted HTTP server") {
        "no_allowlisted_http_servers"
    } else if error.contains("managed MCP policy does not currently admit HTTP MCP servers") {
        "http_transport_not_allowed"
    } else if error.contains("managed runtime does not admit model-callable MCP tools") {
        "dynamic_tools_not_admitted"
    } else if error.contains("managed MCP side-effect tools are disabled by operator policy") {
        "side_effects_disabled_by_operator_policy"
    } else if error.contains("managed runtime does not yet admit MCP side-effect tools") {
        "side_effect_tools_not_admitted"
    } else if error.contains("managed runtime does not yet admit stdio-only MCP bridge tools") {
        "stdio_side_effect_tools_not_admitted"
    } else if error
        .contains("managed runtime does not yet admit model-callable MCP tools that currently resolve only to stdio servers")
    {
        "stdio_dynamic_tools_not_admitted"
    } else if error.contains(
        "managed runtime currently admits read-only MCP bridge tools only through allowlisted HTTP MCP servers; current operator policy leaves only stdio MCP candidates for these tools",
    ) {
        "stdio_read_only_tools_not_admitted"
    } else if error.contains(
        "allowlisted managed MCP HTTP servers did not expose required read-only MCP capabilities",
    ) && (!(error.contains("prompt capability: -") && error.contains("resource capability: -")))
    {
        "read_only_capabilities_blocked_by_allowlist"
    } else if error.contains(
        "allowlisted managed MCP HTTP servers did not expose prompt capability required by requested read-only MCP tools",
    ) && !error.contains("configured non-allowlisted HTTP servers exposing prompt capability: -")
    {
        "read_only_prompt_capability_blocked_by_allowlist"
    } else if error.contains(
        "allowlisted managed MCP HTTP servers did not expose resource capability required by requested read-only MCP tools",
    ) && !error.contains("configured non-allowlisted HTTP servers exposing resource capability: -")
    {
        "read_only_resource_capability_blocked_by_allowlist"
    } else if error.contains(
        "allowlisted managed MCP HTTP servers did not expose prompt capability required by requested read-only MCP tools",
    ) {
        "read_only_prompt_capability_unavailable"
    } else if error.contains(
        "allowlisted managed MCP HTTP servers did not expose resource capability required by requested read-only MCP tools",
    ) {
        "read_only_resource_capability_unavailable"
    } else if error.contains(
        "allowlisted managed MCP HTTP servers did not expose required read-only MCP capabilities",
    ) {
        "read_only_capabilities_unavailable"
    } else if error
        .contains("allowlisted managed MCP HTTP servers could not expose requested read-only tools")
    {
        "read_only_tools_unavailable"
    } else if error.contains("managed.mcp.allowed_servers references")
        || error.contains("managed MCP policy requires `http` in managed.mcp.allowed_transports")
        || error.contains("managed MCP stdio policy requires")
        || error.contains("managed.mcp.stdio.allowed_servers references")
        || error.contains("managed.mcp.stdio.allowed_env_keys")
    {
        "invalid_operator_policy"
    } else {
        return None;
    };

    if requested_tools.is_empty() {
        return None;
    }

    Some(build_managed_mcp_admission_rejection(
        code,
        error.to_string(),
        requested_read_only_tools,
        requested_side_effect_tools,
        requested_dynamic_tools,
        app_config,
        build_read_only_capability_attribution(code, error),
    ))
}

/// Compatibility alias retained for older callers. Prefer
/// `parse_legacy_managed_mcp_admission_rejection(...)` for any remaining
/// legacy string-only paths.
#[doc(hidden)]
pub fn classify_managed_mcp_admission_rejection(
    allowed_tools: &[String],
    app_config: &AppConfig,
    error: &str,
) -> Option<ManagedMcpAdmissionRejection> {
    parse_legacy_managed_mcp_admission_rejection(allowed_tools, app_config, error)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, HeaderValue},
        routing::post,
    };
    use hermes_config::config::{
        ManagedConfigYaml, ManagedMcpPolicyYaml, McpServerConfig, McpTransportKind,
    };
    use hermes_core::{
        error::Result,
        message::ToolResult,
        tool::{Tool, ToolConfig, ToolContext, ToolSchema},
    };
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    use super::*;

    struct MockTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.name
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name().to_string(),
                description: format!("mock {}", self.name()),
                parameters: json!({}),
            }
        }

        fn toolset(&self) -> &str {
            "test"
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult> {
            Ok(ToolResult::ok(self.name().to_string()))
        }
    }

    struct MockMcpTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for MockMcpTool {
        fn name(&self) -> &str {
            self.name
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name().to_string(),
                description: self.description.to_string(),
                parameters: json!({}),
            }
        }

        fn toolset(&self) -> &str {
            "mcp"
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult> {
            Ok(ToolResult::ok(self.name().to_string()))
        }
    }

    #[derive(Clone)]
    struct MockMcpHttpState {
        body: String,
        supports_prompts: bool,
        supports_resources: bool,
    }

    fn make_tool_context() -> ToolContext {
        let (approval_tx, _approval_rx) = mpsc::channel(8);
        let (delta_tx, _delta_rx) = mpsc::channel(8);
        ToolContext {
            session_id: "managed-mcp-test".to_string(),
            working_dir: std::env::temp_dir(),
            approval_tx,
            delta_tx,
            execution_observer: None,
            tool_config: Arc::new(ToolConfig::default()),
            memory: None,
            aux_provider: None,
            skills: None,
            delegation_depth: 0,
            clarify_tx: None,
        }
    }

    async fn bind_test_listener() -> Option<tokio::net::TcpListener> {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()
    }

    async fn spawn_mock_mcp_http_server_with_capabilities(
        body: &str,
        supports_prompts: bool,
        supports_resources: bool,
    ) -> Option<(String, tokio::task::JoinHandle<()>)> {
        let listener = bind_test_listener().await?;
        let addr = listener.local_addr().ok()?;
        let app = Router::new()
            .route(
                "/mcp",
                post(
                    |State(state): State<MockMcpHttpState>, Json(payload): Json<Value>| async move {
                        let id = payload.get("id").cloned().unwrap_or(Value::Null);
                        let method = payload
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let result = match method {
                            "initialize" => json!({
                                "protocolVersion": "2024-11-05",
                                "capabilities": {
                                    "prompts": state.supports_prompts.then_some(json!({})),
                                    "resources": state.supports_resources.then_some(json!({}))
                                }
                            }),
                            "prompts/list" => json!({
                                "prompts": [
                                    {
                                        "name": "demo_prompt",
                                        "description": "Demo prompt"
                                    }
                                ]
                            }),
                            "prompts/get" => json!({
                                "description": "Demo prompt",
                                "messages": [
                                    {
                                        "role": "user",
                                        "content": {
                                            "type": "text",
                                            "text": state.body
                                        }
                                    }
                                ]
                            }),
                            "resources/list" => json!({
                                "resources": [
                                    {
                                        "uri": "file:///docs/readme.txt",
                                        "name": "Readme"
                                    }
                                ]
                            }),
                            "resources/read" => json!({
                                "contents": [
                                    {
                                        "uri": "file:///docs/readme.txt",
                                        "mimeType": "text/plain",
                                        "text": state.body
                                    }
                                ]
                            }),
                            _ => json!({}),
                        };

                        let mut headers = HeaderMap::new();
                        headers.insert("Mcp-Session-Id", HeaderValue::from_static("sid_managed"));
                        headers.insert(
                            "MCP-Protocol-Version",
                            HeaderValue::from_static("2024-11-05"),
                        );
                        (
                            headers,
                            Json(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": result,
                            })),
                        )
                    },
                ),
            )
            .with_state(MockMcpHttpState {
                body: body.to_string(),
                supports_prompts,
                supports_resources,
            });
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Some((format!("http://{addr}/mcp"), server))
    }

    async fn spawn_mock_mcp_http_server(
        body: &str,
    ) -> Option<(String, tokio::task::JoinHandle<()>)> {
        spawn_mock_mcp_http_server_with_capabilities(body, false, true).await
    }

    fn managed_http_policy(
        allowed_servers: Vec<String>,
        allow_side_effects: bool,
    ) -> ManagedConfigYaml {
        ManagedConfigYaml {
            mcp: ManagedMcpPolicyYaml {
                enabled: true,
                allowed_transports: vec![McpTransportKind::Http],
                allowed_servers,
                allow_side_effects,
                ..ManagedMcpPolicyYaml::default()
            },
            ..ManagedConfigYaml::default()
        }
    }

    #[tokio::test]
    async fn filtered_registry_hides_blocked_tools_from_schema_and_lookup() {
        let source = ToolRegistry::new();
        source.register(Box::new(MockTool { name: "read_file" }));
        source.register(Box::new(MockTool { name: "terminal" }));

        let filtered =
            build_filtered_registry(&source, &["read_file".to_string()], &AppConfig::default())
                .await
                .unwrap();
        let schemas = filtered.available_schemas();

        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "read_file");
        assert!(filtered.get("read_file").is_some());
        assert!(filtered.get("terminal").is_none());
    }

    #[tokio::test]
    async fn filtered_registry_rejects_tools_outside_beta_policy() {
        let source = ToolRegistry::new();
        source.register(Box::new(MockTool { name: "read_file" }));
        source.register(Box::new(MockTool { name: "terminal" }));

        let err =
            build_filtered_registry(&source, &["terminal".to_string()], &AppConfig::default())
                .await
                .err()
                .unwrap();
        assert!(err.to_string().contains("terminal"));
    }

    #[tokio::test]
    async fn filtered_registry_rejects_unknown_tools() {
        let source = ToolRegistry::new();
        source.register(Box::new(MockTool { name: "read_file" }));
        source.register(Box::new(MockTool {
            name: "vision_analyze",
        }));

        let err = build_filtered_registry(
            &source,
            &["read_file".to_string(), "skill_view".to_string()],
            &AppConfig::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("skill_view"));
    }

    #[tokio::test]
    async fn filtered_registry_rejects_mcp_tools_when_policy_disabled() {
        let source = ToolRegistry::new();

        let err = build_filtered_registry(
            &source,
            &["mcp_resource_read".to_string()],
            &AppConfig::default(),
        )
        .await
        .err()
        .unwrap();

        assert!(err.to_string().contains("managed MCP tools are disabled"));
    }

    #[tokio::test]
    async fn filtered_registry_rejects_dynamic_mcp_tools_when_policy_is_configured() {
        let source = ToolRegistry::new();
        source.register(Box::new(MockMcpTool {
            name: "demo_remote_tool",
            description: "mock remote tool [MCP: docs]",
        }));
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "docs".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://mcp.example.com".to_string()),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: managed_http_policy(vec!["docs".to_string()], false),
            ..AppConfig::default()
        };

        let err = build_filtered_registry(&source, &["demo_remote_tool".to_string()], &app_config)
            .await
            .err()
            .unwrap();

        assert!(
            err.to_string().contains("model-callable MCP tools"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn filtered_registry_rejects_dynamic_mcp_tools_that_resolve_only_to_stdio_servers() {
        let source = ToolRegistry::new();
        source.register(Box::new(MockMcpTool {
            name: "demo_local_tool",
            description: "mock local tool [MCP: filesystem]",
        }));
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "filesystem".to_string(),
                transport: McpTransportKind::Stdio,
                command: "/usr/bin/filesystem-mcp".to_string(),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Stdio],
                    stdio: hermes_config::config::ManagedMcpStdioPolicyYaml {
                        allowed_servers: vec!["filesystem".to_string()],
                        allowed_env_keys: vec![],
                    },
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let err = build_filtered_registry(&source, &["demo_local_tool".to_string()], &app_config)
            .await
            .err()
            .unwrap();

        assert!(
            err.to_string()
                .contains("currently resolve only to stdio servers"),
            "{err}"
        );
        assert!(err.to_string().contains("filesystem"), "{err}");
    }

    #[test]
    fn parse_legacy_managed_mcp_rejection_includes_stdio_policy_snapshot() {
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "local-docs".to_string(),
                transport: McpTransportKind::Stdio,
                command: "/usr/bin/docs-mcp".to_string(),
                args: vec!["--workspace".to_string(), "/tmp".to_string()],
                env: std::collections::BTreeMap::from([
                    ("DOCS_REGION".to_string(), "us".to_string()),
                    ("DOCS_TOKEN".to_string(), "secret".to_string()),
                ]),
                cwd: Some(std::path::PathBuf::from("/srv/docs")),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Stdio],
                    stdio: hermes_config::config::ManagedMcpStdioPolicyYaml {
                        allowed_servers: vec!["local-docs".to_string()],
                        allowed_env_keys: vec!["DOCS_TOKEN".to_string()],
                    },
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let rejection = parse_legacy_managed_mcp_admission_rejection(
            &["demo_remote_tool".to_string()],
            &app_config,
            "managed.mcp.stdio.allowed_env_keys must explicitly allow env keys required by allowlisted stdio MCP servers: local-docs: DOCS_REGION",
        )
        .expect("expected managed MCP rejection");

        assert_eq!(rejection.code, "invalid_operator_policy");
        assert_eq!(
            rejection.requested_dynamic_tools,
            vec!["demo_remote_tool".to_string()]
        );
        assert_eq!(
            rejection.allowed_stdio_servers,
            vec!["local-docs".to_string()]
        );
        assert_eq!(
            rejection.allowed_stdio_env_keys,
            vec!["DOCS_TOKEN".to_string()]
        );
        assert_eq!(rejection.stdio_server_summaries.len(), 1);
        let summary = &rejection.stdio_server_summaries[0];
        assert_eq!(summary.name, "local-docs");
        assert_eq!(summary.command, "/usr/bin/docs-mcp");
        assert_eq!(summary.arg_count, 2);
        assert!(summary.cwd_configured);
        assert_eq!(
            summary.env_keys,
            vec!["DOCS_REGION".to_string(), "DOCS_TOKEN".to_string()]
        );
    }

    #[test]
    fn parse_legacy_managed_mcp_rejection_maps_stdio_dynamic_code() {
        let rejection = parse_legacy_managed_mcp_admission_rejection(
            &["demo_local_tool".to_string()],
            &AppConfig::default(),
            "managed runtime does not yet admit model-callable MCP tools that currently resolve only to stdio servers: demo_local_tool (servers: filesystem)",
        )
        .expect("expected stdio dynamic rejection");

        assert_eq!(rejection.code, "stdio_dynamic_tools_not_admitted");
        assert_eq!(
            rejection.requested_dynamic_tools,
            vec!["demo_local_tool".to_string()]
        );
    }

    #[test]
    fn parse_legacy_managed_mcp_rejection_maps_stdio_read_only_code() {
        let rejection = parse_legacy_managed_mcp_admission_rejection(
            &["mcp_resource_read".to_string(), "mcp_prompt_list".to_string()],
            &AppConfig::default(),
            "managed runtime currently admits read-only MCP bridge tools only through allowlisted HTTP MCP servers; current operator policy leaves only stdio MCP candidates for these tools: mcp_prompt_list, mcp_resource_read (candidate stdio servers: filesystem)",
        )
        .expect("expected stdio read-only rejection");

        assert_eq!(rejection.code, "stdio_read_only_tools_not_admitted");
        assert_eq!(
            rejection.requested_read_only_tools,
            vec![
                "mcp_prompt_list".to_string(),
                "mcp_resource_read".to_string()
            ]
        );
    }

    #[test]
    fn parse_legacy_managed_mcp_rejection_maps_read_only_prompt_capability_code() {
        let rejection = parse_legacy_managed_mcp_admission_rejection(
            &["mcp_prompt_list".to_string()],
            &AppConfig::default(),
            "allowlisted managed MCP HTTP servers did not expose prompt capability required by requested read-only MCP tools: mcp_prompt_list (allowlisted servers: docs; configured non-allowlisted HTTP servers exposing prompt capability: -)",
        )
        .expect("expected prompt capability rejection");

        assert_eq!(rejection.code, "read_only_prompt_capability_unavailable");
        assert_eq!(
            rejection.requested_read_only_tools,
            vec!["mcp_prompt_list".to_string()]
        );
        assert_eq!(
            rejection.read_only_capability_attribution.prompt_tools,
            vec!["mcp_prompt_list".to_string()]
        );
        assert!(
            rejection
                .read_only_capability_attribution
                .blocked_http_prompt_servers
                .is_empty()
        );
    }

    #[test]
    fn parse_legacy_managed_mcp_rejection_maps_read_only_resource_capability_code() {
        let rejection = parse_legacy_managed_mcp_admission_rejection(
            &["mcp_resource_read".to_string()],
            &AppConfig::default(),
            "allowlisted managed MCP HTTP servers did not expose resource capability required by requested read-only MCP tools: mcp_resource_read (allowlisted servers: docs; configured non-allowlisted HTTP servers exposing resource capability: -)",
        )
        .expect("expected resource capability rejection");

        assert_eq!(rejection.code, "read_only_resource_capability_unavailable");
        assert_eq!(
            rejection.requested_read_only_tools,
            vec!["mcp_resource_read".to_string()]
        );
        assert_eq!(
            rejection.read_only_capability_attribution.resource_tools,
            vec!["mcp_resource_read".to_string()]
        );
        assert!(
            rejection
                .read_only_capability_attribution
                .blocked_http_resource_servers
                .is_empty()
        );
    }

    #[test]
    fn parse_legacy_managed_mcp_rejection_maps_read_only_prompt_blocked_by_allowlist_code() {
        let rejection = parse_legacy_managed_mcp_admission_rejection(
            &["mcp_prompt_list".to_string()],
            &AppConfig::default(),
            "allowlisted managed MCP HTTP servers did not expose prompt capability required by requested read-only MCP tools: mcp_prompt_list (allowlisted servers: docs; configured non-allowlisted HTTP servers exposing prompt capability: archive)",
        )
        .expect("expected prompt allowlist rejection");

        assert_eq!(
            rejection.code,
            "read_only_prompt_capability_blocked_by_allowlist"
        );
        assert_eq!(
            rejection.read_only_capability_attribution.prompt_tools,
            vec!["mcp_prompt_list".to_string()]
        );
        assert_eq!(
            rejection
                .read_only_capability_attribution
                .blocked_http_prompt_servers,
            vec!["archive".to_string()]
        );
    }

    #[test]
    fn parse_legacy_managed_mcp_rejection_maps_read_only_resource_blocked_by_allowlist_code() {
        let rejection = parse_legacy_managed_mcp_admission_rejection(
            &["mcp_resource_read".to_string()],
            &AppConfig::default(),
            "allowlisted managed MCP HTTP servers did not expose resource capability required by requested read-only MCP tools: mcp_resource_read (allowlisted servers: docs; configured non-allowlisted HTTP servers exposing resource capability: archive)",
        )
        .expect("expected resource allowlist rejection");

        assert_eq!(
            rejection.code,
            "read_only_resource_capability_blocked_by_allowlist"
        );
        assert_eq!(
            rejection.read_only_capability_attribution.resource_tools,
            vec!["mcp_resource_read".to_string()]
        );
        assert_eq!(
            rejection
                .read_only_capability_attribution
                .blocked_http_resource_servers,
            vec!["archive".to_string()]
        );
    }

    #[test]
    fn managed_mcp_rejection_deserializes_without_structured_read_only_attribution() {
        let metadata = json!({
            "code": "read_only_prompt_capability_blocked_by_allowlist",
            "error": "legacy",
            "requested_tools": ["mcp_prompt_list"],
            "requested_read_only_tools": ["mcp_prompt_list"],
            "requested_side_effect_tools": [],
            "requested_dynamic_tools": [],
            "allowed_servers": ["docs"],
            "allowed_transports": ["http"],
            "allow_side_effects": false,
            "allowed_stdio_servers": [],
            "allowed_stdio_env_keys": [],
            "stdio_server_summaries": []
        });

        let rejection: ManagedMcpAdmissionRejection =
            serde_json::from_value(metadata).expect("legacy metadata should deserialize");
        assert!(rejection.read_only_capability_attribution.is_empty());
    }

    #[test]
    fn normalize_legacy_rejection_backfills_read_only_attribution() {
        let mut rejection = ManagedMcpAdmissionRejection {
            code: "read_only_prompt_capability_blocked_by_allowlist".to_string(),
            error: "allowlisted managed MCP HTTP servers did not expose prompt capability required by requested read-only MCP tools: mcp_prompt_list (allowlisted servers: docs; configured non-allowlisted HTTP servers exposing prompt capability: archive)".to_string(),
            requested_tools: vec!["mcp_prompt_list".to_string()],
            requested_read_only_tools: vec!["mcp_prompt_list".to_string()],
            requested_side_effect_tools: vec![],
            requested_dynamic_tools: vec![],
            allowed_servers: vec!["docs".to_string()],
            allowed_transports: vec!["http".to_string()],
            allow_side_effects: false,
            allowed_stdio_servers: vec![],
            allowed_stdio_env_keys: vec![],
            stdio_server_summaries: vec![],
            read_only_capability_attribution: ManagedMcpReadOnlyCapabilityAttribution::default(),
        };

        normalize_legacy_managed_mcp_admission_rejection(&mut rejection);
        assert_eq!(
            rejection.read_only_capability_attribution.prompt_tools,
            vec!["mcp_prompt_list".to_string()]
        );
        assert_eq!(
            rejection
                .read_only_capability_attribution
                .blocked_http_prompt_servers,
            vec!["archive".to_string()]
        );
    }

    #[tokio::test]
    async fn filtered_registry_rejects_mcp_side_effect_tools_even_when_opted_in() {
        let source = ToolRegistry::new();
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "docs".to_string(),
                transport: McpTransportKind::Http,
                url: Some("https://mcp.example.com".to_string()),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: managed_http_policy(vec!["docs".to_string()], true),
            ..AppConfig::default()
        };

        let err = build_filtered_registry(
            &source,
            &["mcp_resource_subscribe".to_string()],
            &app_config,
        )
        .await
        .err()
        .unwrap();

        assert!(
            err.to_string()
                .contains("does not yet admit stdio-only MCP bridge tools"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn filtered_registry_rejects_read_only_mcp_tools_when_only_stdio_candidates_remain() {
        let source = ToolRegistry::new();
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "filesystem".to_string(),
                transport: McpTransportKind::Stdio,
                command: "/usr/bin/filesystem-mcp".to_string(),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Stdio],
                    stdio: hermes_config::config::ManagedMcpStdioPolicyYaml {
                        allowed_servers: vec!["filesystem".to_string()],
                        allowed_env_keys: vec![],
                    },
                    ..ManagedMcpPolicyYaml::default()
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let err = build_filtered_registry(
            &source,
            &[
                "mcp_resource_read".to_string(),
                "mcp_prompt_list".to_string(),
            ],
            &app_config,
        )
        .await
        .err()
        .unwrap();

        assert!(
            err.to_string().contains("leaves only stdio MCP candidates"),
            "{err}"
        );
        assert!(err.to_string().contains("filesystem"), "{err}");
    }

    #[tokio::test]
    async fn filtered_registry_rejects_read_only_prompt_tools_when_http_servers_lack_prompt_capability()
     {
        let Some((docs_url, docs_server)) =
            spawn_mock_mcp_http_server_with_capabilities("resources only", false, true).await
        else {
            return;
        };
        let source = ToolRegistry::new();
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "docs".to_string(),
                transport: McpTransportKind::Http,
                url: Some(docs_url),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: managed_http_policy(vec!["docs".to_string()], false),
            ..AppConfig::default()
        };

        let err = build_filtered_registry(&source, &["mcp_prompt_list".to_string()], &app_config)
            .await
            .err()
            .unwrap();

        assert!(
            err.to_string().contains("did not expose prompt capability"),
            "{err}"
        );
        assert!(err.to_string().contains("mcp_prompt_list"), "{err}");
        docs_server.abort();
    }

    #[tokio::test]
    async fn filtered_registry_rejects_read_only_prompt_tools_with_allowlist_attribution_when_blocked_http_server_supports_them()
     {
        let Some((docs_url, docs_server)) =
            spawn_mock_mcp_http_server_with_capabilities("resources only", false, true).await
        else {
            return;
        };
        let Some((archive_url, archive_server)) =
            spawn_mock_mcp_http_server_with_capabilities("prompts only", true, false).await
        else {
            docs_server.abort();
            return;
        };
        let source = ToolRegistry::new();
        let app_config = AppConfig {
            mcp_servers: vec![
                McpServerConfig {
                    name: "docs".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some(docs_url),
                    enabled: true,
                    ..McpServerConfig::default()
                },
                McpServerConfig {
                    name: "archive".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some(archive_url),
                    enabled: true,
                    ..McpServerConfig::default()
                },
            ],
            managed: managed_http_policy(vec!["docs".to_string()], false),
            ..AppConfig::default()
        };

        let err = build_filtered_registry(&source, &["mcp_prompt_list".to_string()], &app_config)
            .await
            .err()
            .unwrap();

        assert!(
            err.to_string().contains(
                "configured non-allowlisted HTTP servers exposing prompt capability: archive"
            ),
            "{err}"
        );
        docs_server.abort();
        archive_server.abort();
    }

    #[tokio::test]
    async fn filtered_registry_rejects_read_only_resource_tools_when_http_servers_lack_resource_capability()
     {
        let Some((docs_url, docs_server)) =
            spawn_mock_mcp_http_server_with_capabilities("prompts only", true, false).await
        else {
            return;
        };
        let source = ToolRegistry::new();
        let app_config = AppConfig {
            mcp_servers: vec![McpServerConfig {
                name: "docs".to_string(),
                transport: McpTransportKind::Http,
                url: Some(docs_url),
                enabled: true,
                ..McpServerConfig::default()
            }],
            managed: managed_http_policy(vec!["docs".to_string()], false),
            ..AppConfig::default()
        };

        let err = build_filtered_registry(&source, &["mcp_resource_read".to_string()], &app_config)
            .await
            .err()
            .unwrap();

        assert!(
            err.to_string()
                .contains("did not expose resource capability"),
            "{err}"
        );
        assert!(err.to_string().contains("mcp_resource_read"), "{err}");
        docs_server.abort();
    }

    #[tokio::test]
    async fn filtered_registry_rejects_read_only_resource_tools_with_allowlist_attribution_when_blocked_http_server_supports_them()
     {
        let Some((docs_url, docs_server)) =
            spawn_mock_mcp_http_server_with_capabilities("prompts only", true, false).await
        else {
            return;
        };
        let Some((archive_url, archive_server)) =
            spawn_mock_mcp_http_server_with_capabilities("resources only", false, true).await
        else {
            docs_server.abort();
            return;
        };
        let source = ToolRegistry::new();
        let app_config = AppConfig {
            mcp_servers: vec![
                McpServerConfig {
                    name: "docs".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some(docs_url),
                    enabled: true,
                    ..McpServerConfig::default()
                },
                McpServerConfig {
                    name: "archive".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some(archive_url),
                    enabled: true,
                    ..McpServerConfig::default()
                },
            ],
            managed: managed_http_policy(vec!["docs".to_string()], false),
            ..AppConfig::default()
        };

        let err = build_filtered_registry(&source, &["mcp_resource_read".to_string()], &app_config)
            .await
            .err()
            .unwrap();

        assert!(
            err.to_string().contains(
                "configured non-allowlisted HTTP servers exposing resource capability: archive"
            ),
            "{err}"
        );
        docs_server.abort();
        archive_server.abort();
    }

    #[tokio::test]
    async fn filtered_registry_admits_allowlisted_http_read_only_mcp_tools() {
        let Some((docs_url, docs_server)) = spawn_mock_mcp_http_server("managed mcp ok").await
        else {
            return;
        };
        let Some((blocked_url, blocked_server)) =
            spawn_mock_mcp_http_server("should stay blocked").await
        else {
            docs_server.abort();
            return;
        };

        let source = ToolRegistry::new();
        source.register(Box::new(MockTool { name: "read_file" }));
        let app_config = AppConfig {
            mcp_servers: vec![
                McpServerConfig {
                    name: "docs".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some(docs_url),
                    enabled: true,
                    ..McpServerConfig::default()
                },
                McpServerConfig {
                    name: "blocked".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some(blocked_url),
                    enabled: true,
                    ..McpServerConfig::default()
                },
            ],
            managed: managed_http_policy(vec!["docs".to_string()], false),
            ..AppConfig::default()
        };

        let filtered = build_filtered_registry(
            &source,
            &["read_file".to_string(), "mcp_resource_read".to_string()],
            &app_config,
        )
        .await
        .unwrap();

        assert!(filtered.get("read_file").is_some());
        assert!(filtered.get("mcp_resource_read").is_some());
        assert!(filtered.get("mcp_resource_subscribe").is_none());

        let ctx = make_tool_context();
        let result = filtered
            .get("mcp_resource_read")
            .unwrap()
            .execute(json!({"uri": "file:///docs/readme.txt"}), &ctx)
            .await
            .unwrap();
        assert!(result.content.contains("managed mcp ok"));
        assert!(result.content.contains("\"server\": \"docs\""));

        let err = filtered
            .get("mcp_resource_read")
            .unwrap()
            .execute(
                json!({"server": "blocked", "uri": "file:///docs/readme.txt"}),
                &ctx,
            )
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("Available servers: docs"), "{err}");

        docs_server.abort();
        blocked_server.abort();
    }
}
