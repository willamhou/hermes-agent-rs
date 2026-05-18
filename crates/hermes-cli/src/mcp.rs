use std::collections::HashMap;

use chrono::{DateTime, Utc};
use clap::Subcommand;
use hermes_config::config::{AppConfig, McpServerConfig, McpTransportKind};
use hermes_managed::{
    ManagedMcpAdmissionRejection, ManagedRun, ManagedRunEvent, ManagedRunEventKind, ManagedStore,
    managed_mcp_admission_rejection_from_event, normalize_legacy_managed_mcp_admission_rejection,
};
use hermes_mcp::{
    McpRuntimeAuditContext, McpRuntimeAuditEvent, McpRuntimeAuditEventKind, McpRuntimeAuditSeverity,
};
use serde::Serialize;

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// Show the current managed MCP policy plus recent operator-facing MCP events
    Inspect {
        /// Maximum number of recent events to print per section
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// List persisted shared MCP runtime audit events
    Audits {
        /// Maximum number of events to print
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Serialize)]
struct McpRuntimeAuditListJsonResponse {
    object: &'static str,
    data: Vec<McpRuntimeAuditEvent>,
}

#[derive(Debug, Clone, Serialize)]
struct McpInspectionJsonResponse {
    object: &'static str,
    policy: ManagedMcpPolicyInspection,
    recent_admission_rejections: Vec<ManagedMcpAdmissionRejectionEntry>,
    recent_runtime_audits: Vec<McpRuntimeAuditEvent>,
}

#[derive(Debug, Clone, Serialize)]
struct ManagedMcpPolicyInspection {
    enabled: bool,
    allow_side_effects: bool,
    allowed_transports: Vec<String>,
    allowed_http_servers: Vec<String>,
    unresolved_http_servers: Vec<String>,
    http_server_summaries: Vec<HttpServerSummary>,
    blocked_http_server_summaries: Vec<HttpServerSummary>,
    stdio: ManagedMcpStdioPolicyInspection,
}

#[derive(Debug, Clone, Serialize)]
struct ManagedMcpStdioPolicyInspection {
    allowed_servers: Vec<String>,
    unresolved_servers: Vec<String>,
    allowed_env_keys: Vec<String>,
    server_summaries: Vec<StdioServerSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct HttpServerSummary {
    name: String,
    url: Option<String>,
    header_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StdioServerSummary {
    name: String,
    command: String,
    arg_count: usize,
    cwd_configured: bool,
    env_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ManagedMcpAdmissionRejectionEntry {
    event: ManagedRunEvent,
    run_status: Option<String>,
    agent_name: Option<String>,
    model: Option<String>,
    rejection: Option<ManagedMcpAdmissionRejection>,
}

pub async fn run_mcp(action: McpAction) -> anyhow::Result<()> {
    match action {
        McpAction::Inspect { limit, json } => inspect_mcp(limit, json).await,
        McpAction::Audits { limit, json } => list_runtime_audits(limit, json).await,
    }
}

async fn inspect_mcp(limit: usize, json: bool) -> anyhow::Result<()> {
    let app_config = AppConfig::load();
    let policy = build_managed_mcp_policy_inspection(&app_config);
    let store = ManagedStore::open().await?;
    let recent_admission_rejections = load_recent_mcp_admission_rejections(&store, limit).await?;
    let recent_runtime_audits = hermes_mcp::list_runtime_audit_events(limit.clamp(1, 1000)).await?;

    if json {
        print_json(&McpInspectionJsonResponse {
            object: "mcp_inspection",
            policy,
            recent_admission_rejections,
            recent_runtime_audits,
        })?;
        return Ok(());
    }

    print_policy_summary(&policy);
    println!();
    print_recent_admission_rejections(&recent_admission_rejections);
    println!();
    print_runtime_audit_table(&recent_runtime_audits);
    Ok(())
}

async fn list_runtime_audits(limit: usize, json: bool) -> anyhow::Result<()> {
    let events = hermes_mcp::list_runtime_audit_events(limit.clamp(1, 1000)).await?;
    if json {
        print_json(&McpRuntimeAuditListJsonResponse {
            object: "list",
            data: events,
        })?;
        return Ok(());
    }

    print_runtime_audit_table(&events);
    Ok(())
}

fn build_managed_mcp_policy_inspection(app_config: &AppConfig) -> ManagedMcpPolicyInspection {
    let policy = &app_config.managed.mcp;
    let allowed_http_servers = sorted_strings(policy.allowed_servers.clone());
    let allowed_stdio_servers = sorted_strings(policy.stdio.allowed_servers.clone());
    let allowed_stdio_env_keys = sorted_strings(policy.stdio.allowed_env_keys.clone());

    let mut http_server_summaries = app_config
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.transport == McpTransportKind::Http
                && allowed_http_servers.contains(&server.name)
        })
        .map(summarize_http_server)
        .collect::<Vec<_>>();
    http_server_summaries.sort_by(|left, right| left.name.cmp(&right.name));

    let mut blocked_http_server_summaries = app_config
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.transport == McpTransportKind::Http
                && !allowed_http_servers.contains(&server.name)
        })
        .map(summarize_http_server)
        .collect::<Vec<_>>();
    blocked_http_server_summaries.sort_by(|left, right| left.name.cmp(&right.name));

    let mut stdio_server_summaries = app_config
        .mcp_servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.transport == McpTransportKind::Stdio
                && allowed_stdio_servers.contains(&server.name)
        })
        .map(summarize_stdio_server)
        .collect::<Vec<_>>();
    stdio_server_summaries.sort_by(|left, right| left.name.cmp(&right.name));

    let unresolved_http_servers = unresolved_server_names(
        &allowed_http_servers,
        &app_config.mcp_servers,
        McpTransportKind::Http,
    );
    let unresolved_stdio_servers = unresolved_server_names(
        &allowed_stdio_servers,
        &app_config.mcp_servers,
        McpTransportKind::Stdio,
    );

    ManagedMcpPolicyInspection {
        enabled: policy.enabled,
        allow_side_effects: policy.allow_side_effects,
        allowed_transports: sorted_strings(
            policy
                .allowed_transports
                .iter()
                .map(transport_label_owned)
                .collect(),
        ),
        allowed_http_servers,
        unresolved_http_servers,
        http_server_summaries,
        blocked_http_server_summaries,
        stdio: ManagedMcpStdioPolicyInspection {
            allowed_servers: allowed_stdio_servers,
            unresolved_servers: unresolved_stdio_servers,
            allowed_env_keys: allowed_stdio_env_keys,
            server_summaries: stdio_server_summaries,
        },
    }
}

async fn load_recent_mcp_admission_rejections(
    store: &ManagedStore,
    limit: usize,
) -> anyhow::Result<Vec<ManagedMcpAdmissionRejectionEntry>> {
    let events = store
        .list_recent_run_events_by_kind(
            ManagedRunEventKind::RunMcpAdmissionRejected,
            limit.clamp(1, 1000),
        )
        .await?;

    let mut runs_by_id: HashMap<String, ManagedRun> = HashMap::new();
    let mut agent_names_by_id: HashMap<String, String> = HashMap::new();
    let mut entries = Vec::with_capacity(events.len());

    for event in events {
        let run = if let Some(existing) = runs_by_id.get(&event.run_id) {
            Some(existing.clone())
        } else {
            let fetched = store.get_run(&event.run_id).await?;
            if let Some(run) = fetched.as_ref() {
                runs_by_id.insert(run.id.clone(), run.clone());
            }
            fetched
        };

        let agent_name = match run.as_ref() {
            Some(run) => {
                if let Some(existing) = agent_names_by_id.get(&run.agent_id) {
                    Some(existing.clone())
                } else {
                    let fetched = store
                        .get_agent(&run.agent_id)
                        .await?
                        .map(|agent| agent.name)
                        .unwrap_or_else(|| run.agent_id.clone());
                    agent_names_by_id.insert(run.agent_id.clone(), fetched.clone());
                    Some(fetched)
                }
            }
            None => None,
        };

        let rejection = managed_mcp_admission_rejection_from_event(&event);

        entries.push(ManagedMcpAdmissionRejectionEntry {
            run_status: run.as_ref().map(|run| run.status.as_str().to_string()),
            agent_name,
            model: run.as_ref().map(|run| run.model.clone()),
            event,
            rejection,
        });
    }

    Ok(entries)
}

fn summarize_http_server(server: &McpServerConfig) -> HttpServerSummary {
    let mut header_keys = server.headers.keys().cloned().collect::<Vec<_>>();
    header_keys.sort();
    header_keys.dedup();
    HttpServerSummary {
        name: server.name.clone(),
        url: server.url.clone(),
        header_keys,
    }
}

fn summarize_stdio_server(server: &McpServerConfig) -> StdioServerSummary {
    let mut env_keys = server.env.keys().cloned().collect::<Vec<_>>();
    env_keys.sort();
    env_keys.dedup();
    StdioServerSummary {
        name: server.name.clone(),
        command: server.command.clone(),
        arg_count: server.args.len(),
        cwd_configured: server.cwd.is_some(),
        env_keys,
    }
}

fn unresolved_server_names(
    allowed_servers: &[String],
    configured_servers: &[McpServerConfig],
    transport: McpTransportKind,
) -> Vec<String> {
    allowed_servers
        .iter()
        .filter(|name| {
            !configured_servers.iter().any(|server| {
                server.enabled && server.transport == transport && server.name == **name
            })
        })
        .cloned()
        .collect()
}

fn print_policy_summary(policy: &ManagedMcpPolicyInspection) {
    println!("Managed MCP policy");
    println!("Enabled:           {}", yes_no(policy.enabled));
    println!(
        "Allowed transports: {}",
        join_or_dash(&policy.allowed_transports)
    );
    println!(
        "Allowed HTTP:      {}",
        join_or_dash(&policy.allowed_http_servers)
    );
    if !policy.unresolved_http_servers.is_empty() {
        println!(
            "Unresolved HTTP:   {}",
            policy.unresolved_http_servers.join(", ")
        );
    }
    println!("Allow side effects: {}", yes_no(policy.allow_side_effects));
    println!(
        "Candidate stdio:   {}",
        join_or_dash(&policy.stdio.allowed_servers)
    );
    println!(
        "Allowed env keys:  {}",
        join_or_dash(&policy.stdio.allowed_env_keys)
    );
    if !policy.stdio.unresolved_servers.is_empty() {
        println!(
            "Unresolved stdio:  {}",
            policy.stdio.unresolved_servers.join(", ")
        );
    }

    println!();
    println!("Allowlisted HTTP server configs");
    if policy.http_server_summaries.is_empty() {
        println!("- none");
    } else {
        for server in &policy.http_server_summaries {
            println!(
                "- {} url={} header_keys={}",
                server.name,
                server.url.as_deref().unwrap_or("-"),
                join_or_dash(&server.header_keys)
            );
        }
    }

    println!();
    println!("Configured non-allowlisted HTTP server configs");
    if policy.blocked_http_server_summaries.is_empty() {
        println!("- none");
    } else {
        for server in &policy.blocked_http_server_summaries {
            println!(
                "- {} url={} header_keys={}",
                server.name,
                server.url.as_deref().unwrap_or("-"),
                join_or_dash(&server.header_keys)
            );
        }
    }

    println!();
    println!("Candidate stdio server configs");
    if policy.stdio.server_summaries.is_empty() {
        println!("- none");
    } else {
        for server in &policy.stdio.server_summaries {
            println!(
                "- {} command={} arg_count={} cwd_configured={} env_keys={}",
                server.name,
                if server.command.trim().is_empty() {
                    "-"
                } else {
                    server.command.as_str()
                },
                server.arg_count,
                yes_no(server.cwd_configured),
                join_or_dash(&server.env_keys)
            );
        }
    }
}

fn print_recent_admission_rejections(entries: &[ManagedMcpAdmissionRejectionEntry]) {
    println!("Recent managed MCP admission rejections");
    if entries.is_empty() {
        println!("No managed MCP admission rejection events found.");
        return;
    }

    println!(
        "{:<20} {:<24} {:<18} {:<24} Code",
        "Time", "Run", "Agent", "Model"
    );
    println!("{}", "-".repeat(112));
    for entry in entries {
        let code = entry
            .rejection
            .as_ref()
            .map(|rejection| rejection.code.as_str())
            .unwrap_or("unknown");
        println!(
            "{:<20} {:<24} {:<18} {:<24} {}",
            format_ts(entry.event.created_at),
            truncate(&entry.event.run_id, 24),
            truncate(entry.agent_name.as_deref().unwrap_or("-"), 18),
            truncate(entry.model.as_deref().unwrap_or("-"), 24),
            code,
        );
        if let Some(rejection) = &entry.rejection {
            println!(
                "  requested={} transports={} stdio_servers={} stdio_env_keys={}",
                join_or_dash(&rejection.requested_tools),
                join_or_dash(&rejection.allowed_transports),
                join_or_dash(&rejection.allowed_stdio_servers),
                join_or_dash(&rejection.allowed_stdio_env_keys),
            );
            if let Some(summary) = rejection_operator_summary(rejection) {
                println!("  {}", summary);
            }
        } else if let Some(message) = &entry.event.message {
            println!("  {}", truncate(message, 160));
        }
    }
}

pub(crate) fn rejection_operator_summary(
    rejection: &ManagedMcpAdmissionRejection,
) -> Option<String> {
    let mut normalized = rejection.clone();
    normalize_legacy_managed_mcp_admission_rejection(&mut normalized);
    let attribution = normalized.read_only_capability_attribution;

    match normalized.code.as_str() {
        "read_only_prompt_capability_blocked_by_allowlist" => Some(format!(
            "prompt capability exists on non-allowlisted HTTP servers={}",
            join_or_dash(&attribution.blocked_http_prompt_servers)
        )),
        "read_only_resource_capability_blocked_by_allowlist" => Some(format!(
            "resource capability exists on non-allowlisted HTTP servers={}",
            join_or_dash(&attribution.blocked_http_resource_servers)
        )),
        "read_only_capabilities_blocked_by_allowlist" => Some(format!(
            "prompt_shadowed_by={} resource_shadowed_by={}",
            join_or_dash(&attribution.blocked_http_prompt_servers),
            join_or_dash(&attribution.blocked_http_resource_servers)
        )),
        "read_only_prompt_capability_unavailable" => {
            Some("allowlisted HTTP servers do not expose required prompt capability".to_string())
        }
        "read_only_resource_capability_unavailable" => {
            Some("allowlisted HTTP servers do not expose required resource capability".to_string())
        }
        "read_only_capabilities_unavailable" => Some(
            "allowlisted HTTP servers do not expose the required prompt/resource capability mix"
                .to_string(),
        ),
        _ => None,
    }
}

fn print_runtime_audit_table(events: &[McpRuntimeAuditEvent]) {
    println!("Recent shared MCP runtime audits");
    if events.is_empty() {
        println!("No MCP runtime audit events found.");
        return;
    }

    println!(
        "{:<20} {:<7} {:<9} {:<8} {:<24} Message",
        "Time", "Severity", "Transport", "Context", "Worker"
    );
    println!("{}", "-".repeat(112));
    for event in events {
        println!(
            "{:<20} {:<7} {:<9} {:<8} {:<24} {}",
            format_ts(event.created_at),
            severity_label(&event.severity),
            transport_label(&event.transport),
            context_label(&event.context),
            truncate(event.worker_id.as_deref().unwrap_or("-"), 24),
            format_runtime_audit_message(event),
        );
    }
}

fn format_runtime_audit_message(event: &McpRuntimeAuditEvent) -> String {
    let mut message = kind_label(&event.kind).to_string();
    message.push_str(": ");
    message.push_str(&event.message);
    if let Some(summary) = runtime_audit_metadata_summary(event.metadata.as_ref()) {
        message.push_str(" (");
        message.push_str(&summary);
        message.push(')');
    }
    truncate_owned(message, 160)
}

fn runtime_audit_metadata_summary(metadata: Option<&serde_json::Value>) -> Option<String> {
    let metadata = metadata?.as_object()?;
    if let Some(error) = metadata.get("error").and_then(serde_json::Value::as_str) {
        return Some(format!("error={error}"));
    }

    let attempted = metadata
        .get("attempted")
        .and_then(serde_json::Value::as_u64);
    let cleaned = metadata.get("cleaned").and_then(serde_json::Value::as_u64);
    let failures = metadata
        .get("failures")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len);

    match (attempted, cleaned, failures) {
        (Some(attempted), Some(cleaned), Some(failures)) => Some(format!(
            "attempted={attempted} cleaned={cleaned} failures={failures}"
        )),
        _ => None,
    }
}

fn transport_label(kind: &McpTransportKind) -> &'static str {
    match kind {
        McpTransportKind::Stdio => "stdio",
        McpTransportKind::Http => "http",
    }
}

fn transport_label_owned(kind: &McpTransportKind) -> String {
    transport_label(kind).to_string()
}

fn context_label(context: &McpRuntimeAuditContext) -> &'static str {
    match context {
        McpRuntimeAuditContext::Startup => "startup",
        McpRuntimeAuditContext::Periodic => "periodic",
    }
}

fn severity_label(severity: &McpRuntimeAuditSeverity) -> &'static str {
    match severity {
        McpRuntimeAuditSeverity::Info => "info",
        McpRuntimeAuditSeverity::Error => "error",
    }
}

fn kind_label(kind: &McpRuntimeAuditEventKind) -> &'static str {
    match kind {
        McpRuntimeAuditEventKind::RuntimeReclaimSucceeded => "runtime.reclaim_succeeded",
        McpRuntimeAuditEventKind::RuntimeReclaimFailed => "runtime.reclaim_failed",
    }
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn truncate(value: &str, max: usize) -> &str {
    if value.len() <= max {
        value
    } else {
        &value[..max]
    }
}

fn truncate_owned(mut value: String, max: usize) -> String {
    if value.len() > max {
        value.truncate(max);
    }
    value
}

fn sorted_strings(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_config::config::{
        ManagedConfigYaml, ManagedMcpPolicyYaml, ManagedMcpStdioPolicyYaml,
    };

    #[test]
    fn runtime_audit_metadata_summary_formats_reclaim_counts() {
        let metadata = serde_json::json!({
            "attempted": 3,
            "cleaned": 2,
            "failures": ["failed a"],
        });
        assert_eq!(
            runtime_audit_metadata_summary(Some(&metadata)).as_deref(),
            Some("attempted=3 cleaned=2 failures=1")
        );
    }

    #[test]
    fn runtime_audit_metadata_summary_prefers_explicit_error() {
        let metadata = serde_json::json!({
            "error": "boom",
            "attempted": 3,
            "cleaned": 0,
            "failures": [],
        });
        assert_eq!(
            runtime_audit_metadata_summary(Some(&metadata)).as_deref(),
            Some("error=boom")
        );
    }

    #[test]
    fn build_policy_inspection_redacts_http_headers_and_stdio_env_values() {
        let app_config = AppConfig {
            mcp_servers: vec![
                McpServerConfig {
                    name: "remote-docs".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some("https://mcp.example.com".to_string()),
                    headers: std::collections::BTreeMap::from([
                        ("Authorization".to_string(), "secret".to_string()),
                        ("X-Tenant".to_string(), "team-a".to_string()),
                    ]),
                    enabled: true,
                    ..McpServerConfig::default()
                },
                McpServerConfig {
                    name: "archive-docs".to_string(),
                    transport: McpTransportKind::Http,
                    url: Some("https://archive.example.com".to_string()),
                    headers: std::collections::BTreeMap::from([(
                        "Authorization".to_string(),
                        "secret-2".to_string(),
                    )]),
                    enabled: true,
                    ..McpServerConfig::default()
                },
                McpServerConfig {
                    name: "filesystem".to_string(),
                    transport: McpTransportKind::Stdio,
                    command: "npx".to_string(),
                    args: vec!["-y".to_string(), "server-filesystem".to_string()],
                    env: std::collections::BTreeMap::from([
                        ("WORKSPACE_ROOT".to_string(), "/srv/workspace".to_string()),
                        ("DOCS_TOKEN".to_string(), "secret".to_string()),
                    ]),
                    enabled: true,
                    ..McpServerConfig::default()
                },
            ],
            managed: ManagedConfigYaml {
                mcp: ManagedMcpPolicyYaml {
                    enabled: true,
                    allowed_transports: vec![McpTransportKind::Http, McpTransportKind::Stdio],
                    allowed_servers: vec!["remote-docs".to_string()],
                    allow_side_effects: false,
                    stdio: ManagedMcpStdioPolicyYaml {
                        allowed_servers: vec!["filesystem".to_string()],
                        allowed_env_keys: vec!["WORKSPACE_ROOT".to_string()],
                    },
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };

        let inspection = build_managed_mcp_policy_inspection(&app_config);
        assert_eq!(inspection.http_server_summaries.len(), 1);
        assert_eq!(
            inspection.http_server_summaries[0].header_keys,
            vec!["Authorization".to_string(), "X-Tenant".to_string()]
        );
        assert_eq!(inspection.blocked_http_server_summaries.len(), 1);
        assert_eq!(
            inspection.blocked_http_server_summaries[0].name,
            "archive-docs"
        );
        assert_eq!(
            inspection.blocked_http_server_summaries[0].header_keys,
            vec!["Authorization".to_string()]
        );
        assert_eq!(inspection.stdio.server_summaries.len(), 1);
        assert_eq!(
            inspection.stdio.server_summaries[0].env_keys,
            vec!["DOCS_TOKEN".to_string(), "WORKSPACE_ROOT".to_string()]
        );
    }

    #[test]
    fn rejection_operator_summary_describes_allowlist_shadowing() {
        let rejection = ManagedMcpAdmissionRejection {
            code: "read_only_prompt_capability_blocked_by_allowlist".to_string(),
            error: "ignored".to_string(),
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
            read_only_capability_attribution:
                hermes_managed::ManagedMcpReadOnlyCapabilityAttribution {
                    prompt_tools: vec!["mcp_prompt_list".to_string()],
                    resource_tools: vec![],
                    blocked_http_prompt_servers: vec!["archive".to_string()],
                    blocked_http_resource_servers: vec![],
                },
        };

        assert_eq!(
            rejection_operator_summary(&rejection).as_deref(),
            Some("prompt capability exists on non-allowlisted HTTP servers=archive")
        );
    }

    #[test]
    fn rejection_operator_summary_backfills_legacy_allowlist_shadowing() {
        let rejection = ManagedMcpAdmissionRejection {
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
            read_only_capability_attribution:
                hermes_managed::ManagedMcpReadOnlyCapabilityAttribution::default(),
        };

        assert_eq!(
            rejection_operator_summary(&rejection).as_deref(),
            Some("prompt capability exists on non-allowlisted HTTP servers=archive")
        );
    }
}
