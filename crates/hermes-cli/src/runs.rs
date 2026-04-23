use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::Context;
use clap::Subcommand;
use hermes_config::config::AppConfig;
use hermes_managed::{ManagedRun, ManagedRunEvent, ManagedStore};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use signet_core::audit::{self, AuditFilter};

#[derive(Subcommand, Debug)]
pub enum RunsAction {
    /// List recent managed runs
    List {
        /// Maximum number of runs to print
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Show one managed run by id
    Get {
        /// Run id
        run: String,
        /// Emit JSON instead of a text summary
        #[arg(long)]
        json: bool,
    },
    /// Show persisted events for one managed run
    Events {
        /// Run id
        run: String,
        /// Maximum number of recent events to print
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Poll for new events until the run reaches a terminal state
        #[arg(long)]
        follow: bool,
        /// Poll interval in milliseconds when following
        #[arg(long, default_value_t = 500)]
        poll_ms: u64,
        /// Emit JSON instead of text output. Not supported with --follow.
        #[arg(long)]
        json: bool,
    },
    /// Verify Signet receipts referenced by one managed run
    Verify {
        /// Run id
        run: String,
        /// Emit JSON instead of text output
        #[arg(long)]
        json: bool,
        /// Suppress successful output and rely on the exit code
        #[arg(long, conflicts_with = "json")]
        quiet: bool,
        /// Exit non-zero when verification is incomplete or invalid
        #[arg(long)]
        strict: bool,
    },
    /// Replay one managed run through the gateway API
    Replay {
        /// Run id
        run: String,
        /// Emit JSON instead of text output
        #[arg(long)]
        json: bool,
    },
}

pub async fn run_runs(action: RunsAction) -> anyhow::Result<()> {
    let store = ManagedStore::open().await?;
    let app_config = AppConfig::load();

    match action {
        RunsAction::List { limit, json } => list_runs(&store, limit, json).await,
        RunsAction::Get { run, json } => get_run(&store, &run, json).await,
        RunsAction::Events {
            run,
            limit,
            follow,
            poll_ms,
            json,
        } => show_run_events(&store, &run, limit, follow, poll_ms, json).await,
        RunsAction::Verify {
            run,
            json,
            quiet,
            strict,
        } => verify_run_signet(&store, &app_config, &run, json, quiet, strict).await,
        RunsAction::Replay { run, json } => replay_run(&store, &app_config, &run, json).await,
    }
}

#[derive(Serialize)]
struct RunJsonEntry {
    run: ManagedRun,
    agent_name: String,
}

#[derive(Serialize)]
struct RunListJsonResponse {
    object: &'static str,
    data: Vec<RunJsonEntry>,
}

#[derive(Deserialize, Serialize)]
struct RunGetJsonResponse {
    run: ManagedRun,
    agent_name: String,
}

#[derive(Serialize)]
struct RunEventsJsonResponse {
    object: &'static str,
    run: ManagedRun,
    agent_name: String,
    data: Vec<ManagedRunEvent>,
}

#[derive(Debug, Serialize)]
struct RunVerifyChainBreakJson {
    file: String,
    line: usize,
    expected_hash: String,
    actual_hash: String,
}

#[derive(Debug, Serialize)]
struct RunVerifyChainJson {
    total_records: usize,
    valid: bool,
    break_point: Option<RunVerifyChainBreakJson>,
}

#[derive(Debug, Serialize)]
struct RunVerifyFailureJson {
    file: String,
    line: usize,
    receipt_id: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct RunVerifySignaturesJson {
    total: usize,
    valid: usize,
    failures: Vec<RunVerifyFailureJson>,
}

#[derive(Debug, Serialize)]
struct RunVerifyJsonResponse {
    run: ManagedRun,
    agent_name: String,
    signet_dir: PathBuf,
    has_receipts: bool,
    verified: bool,
    referenced_receipt_ids: Vec<String>,
    found_receipt_ids: Vec<String>,
    missing_receipt_ids: Vec<String>,
    referenced_signature_failure_ids: Vec<String>,
    chain: RunVerifyChainJson,
    signatures: RunVerifySignaturesJson,
}

async fn list_runs(store: &ManagedStore, limit: usize, json: bool) -> anyhow::Result<()> {
    let runs = store.list_runs(limit.clamp(1, 1000)).await?;
    if runs.is_empty() {
        if json {
            print_json(&RunListJsonResponse {
                object: "list",
                data: Vec::new(),
            })?;
        } else {
            println!("No managed runs found.");
        }
        return Ok(());
    }

    let agent_labels = load_agent_labels(store, &runs).await?;
    if json {
        let data = runs
            .into_iter()
            .map(|run| RunJsonEntry {
                agent_name: agent_labels
                    .get(&run.agent_id)
                    .cloned()
                    .unwrap_or_else(|| run.agent_id.clone()),
                run,
            })
            .collect();
        print_json(&RunListJsonResponse {
            object: "list",
            data,
        })?;
        return Ok(());
    }

    println!(
        "{:<24} {:<20} {:<8} {:<12} {:<28} Started",
        "ID", "Agent", "Version", "Status", "Model"
    );
    println!("{}", "-".repeat(112));
    for run in &runs {
        let agent_label = agent_labels
            .get(&run.agent_id)
            .map(String::as_str)
            .unwrap_or(run.agent_id.as_str());
        println!(
            "{:<24} {:<20} {:<8} {:<12} {:<28} {}",
            truncate(&run.id, 24),
            truncate(agent_label, 20),
            run.agent_version,
            run.status.as_str(),
            truncate(&run.model, 28),
            format_ts(run.started_at),
        );
    }

    Ok(())
}

async fn get_run(store: &ManagedStore, run_ref: &str, json: bool) -> anyhow::Result<()> {
    let run = load_run(store, run_ref).await?;
    let agent_label = load_agent_label(store, &run.agent_id).await?;

    if json {
        print_json(&RunGetJsonResponse {
            run,
            agent_name: agent_label,
        })?;
        return Ok(());
    }

    println!("ID:               {}", run.id);
    println!("Agent:            {}", agent_label);
    println!("Agent ID:         {}", run.agent_id);
    println!("Agent version:    {}", run.agent_version);
    println!("Model:            {}", run.model);
    println!(
        "Replay of:        {}",
        run.replay_of_run_id.as_deref().unwrap_or("-")
    );
    println!("Prompt chars:     {}", run.prompt.chars().count());
    println!("Status:           {}", run.status.as_str());
    println!("Started:          {}", format_ts(run.started_at));
    println!("Updated:          {}", format_ts(run.updated_at));
    println!(
        "Ended:            {}",
        run.ended_at
            .map(format_ts)
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "Cancel requested: {}",
        run.cancel_requested_at
            .map(format_ts)
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "Last error:       {}",
        run.last_error.as_deref().unwrap_or("-")
    );

    Ok(())
}

async fn replay_run(
    store: &ManagedStore,
    app_config: &AppConfig,
    run_ref: &str,
    json: bool,
) -> anyhow::Result<()> {
    let run = load_run(store, run_ref).await?;
    let base_url = managed_gateway_base_url(app_config)?;
    let api_key = managed_gateway_api_key(app_config)?;
    let url = format!(
        "{}/v1/runs/{}/replay",
        base_url.trim_end_matches('/'),
        run.id
    );

    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(api_key)
        .send()
        .await
        .context("failed to call managed run replay API")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read managed run replay response")?;

    if status != StatusCode::CREATED {
        anyhow::bail!(
            "managed run replay failed ({}): {}",
            status.as_u16(),
            body.trim()
        );
    }

    let value: serde_json::Value =
        serde_json::from_str(&body).context("failed to parse managed run replay response")?;
    if json {
        print_json(&value)?;
        return Ok(());
    }

    let replayed_run: RunGetJsonResponse = serde_json::from_value(value)
        .context("failed to decode managed run replay response body")?;
    println!("Replayed run created: {}", replayed_run.run.id);
    println!("Agent:              {}", replayed_run.agent_name);
    println!("Agent version:      {}", replayed_run.run.agent_version);
    println!(
        "Replay of:          {}",
        replayed_run.run.replay_of_run_id.as_deref().unwrap_or("-")
    );
    println!("Status:             {}", replayed_run.run.status.as_str());
    Ok(())
}

async fn show_run_events(
    store: &ManagedStore,
    run_ref: &str,
    limit: usize,
    follow: bool,
    poll_ms: u64,
    json: bool,
) -> anyhow::Result<()> {
    if json && follow {
        anyhow::bail!("--json is not supported with --follow yet");
    }

    let run = load_run(store, run_ref).await?;
    let agent_label = load_agent_label(store, &run.agent_id).await?;
    let events = store
        .list_run_events_tail(&run.id, limit.clamp(1, 1000))
        .await?;

    if json {
        print_json(&RunEventsJsonResponse {
            object: "list",
            run,
            agent_name: agent_label,
            data: events,
        })?;
        return Ok(());
    }

    println!("Run:    {}", run.id);
    println!("Status: {}", run.status.as_str());
    println!();

    if events.is_empty() {
        println!("No managed run events recorded yet.");
    } else {
        for event in &events {
            print_run_event(event);
        }
    }

    if !follow {
        return Ok(());
    }

    if run.status.is_terminal() {
        println!();
        println!("Run is already terminal: {}", run.status.as_str());
        return Ok(());
    }

    let mut last_seen_id = events.last().map(|event| event.id).unwrap_or(0);
    let poll_ms = poll_ms.max(100);

    println!();
    println!("Following new events every {poll_ms}ms. Press Ctrl+C to stop.");
    loop {
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;

        let new_events = store
            .list_run_events_after(&run.id, last_seen_id, 1000)
            .await?;
        for event in &new_events {
            print_run_event(event);
            last_seen_id = event.id;
        }

        let latest = load_run(store, &run.id).await?;
        if latest.status.is_terminal() {
            let tail = store
                .list_run_events_after(&run.id, last_seen_id, 1000)
                .await?;
            for event in &tail {
                print_run_event(event);
            }
            println!();
            println!("Run reached terminal state: {}", latest.status.as_str());
            break;
        }
    }

    Ok(())
}

async fn verify_run_signet(
    store: &ManagedStore,
    config: &AppConfig,
    run_ref: &str,
    json: bool,
    quiet: bool,
    strict: bool,
) -> anyhow::Result<()> {
    let summary = build_run_signet_verification(store, config, run_ref).await?;

    if json {
        print_json(&summary)?;
        return apply_verify_strictness(&summary, strict);
    }

    if quiet {
        return apply_verify_strictness(&summary, strict);
    }

    println!("Run:                {}", summary.run.id);
    println!("Agent:              {}", summary.agent_name);
    println!("Status:             {}", summary.run.status.as_str());
    println!("Signet dir:         {}", summary.signet_dir.display());

    if !summary.has_receipts {
        println!("Signet receipts:    none recorded for this run");
        return apply_verify_strictness(&summary, strict);
    }

    println!(
        "Referenced receipts: {}",
        summary.referenced_receipt_ids.len()
    );
    println!("Found in audit:      {}", summary.found_receipt_ids.len());
    println!("Missing receipts:    {}", summary.missing_receipt_ids.len());
    println!(
        "Audit chain:         {} ({} records)",
        if summary.chain.valid {
            "valid"
        } else {
            "invalid"
        },
        summary.chain.total_records
    );
    println!(
        "Signatures:          {}/{} valid",
        summary.signatures.valid, summary.signatures.total
    );
    println!(
        "Verification:        {}",
        if summary.verified { "OK" } else { "FAILED" }
    );

    if let Some(break_point) = &summary.chain.break_point {
        println!(
            "Chain break:         {}:{} expected={} actual={}",
            break_point.file, break_point.line, break_point.expected_hash, break_point.actual_hash
        );
    }

    if !summary.missing_receipt_ids.is_empty() {
        println!(
            "Missing receipt ids: {}",
            summary.missing_receipt_ids.join(", ")
        );
    }

    if !summary.referenced_signature_failure_ids.is_empty() {
        println!(
            "Receipt signature failures: {}",
            summary.referenced_signature_failure_ids.join(", ")
        );
    }

    apply_verify_strictness(&summary, strict)
}

async fn build_run_signet_verification(
    store: &ManagedStore,
    config: &AppConfig,
    run_ref: &str,
) -> anyhow::Result<RunVerifyJsonResponse> {
    let run = load_run(store, run_ref).await?;
    let agent_name = load_agent_label(store, &run.agent_id).await?;
    let events = store.list_run_events(&run.id, 10_000).await?;
    let referenced_receipt_ids = collect_signet_receipt_ids(&events);
    let signet_dir = config.signet_dir();

    if referenced_receipt_ids.is_empty() {
        return Ok(RunVerifyJsonResponse {
            run,
            agent_name,
            signet_dir,
            has_receipts: false,
            verified: false,
            referenced_receipt_ids,
            found_receipt_ids: Vec::new(),
            missing_receipt_ids: Vec::new(),
            referenced_signature_failure_ids: Vec::new(),
            chain: RunVerifyChainJson {
                total_records: 0,
                valid: true,
                break_point: None,
            },
            signatures: RunVerifySignaturesJson {
                total: 0,
                valid: 0,
                failures: Vec::new(),
            },
        });
    }

    let chain_status = audit::verify_chain(&signet_dir).with_context(|| {
        format!(
            "failed to verify Signet audit chain at {}",
            signet_dir.display()
        )
    })?;
    let verify_result = audit::verify_signatures(&signet_dir, &AuditFilter::default())
        .with_context(|| {
            format!(
                "failed to verify Signet signatures at {}",
                signet_dir.display()
            )
        })?;
    let records = audit::query(
        &signet_dir,
        &AuditFilter {
            limit: None,
            since: None,
            tool: None,
            signer: None,
        },
    )
    .with_context(|| {
        format!(
            "failed to query Signet audit records at {}",
            signet_dir.display()
        )
    })?;

    let referenced_set: HashSet<&str> = referenced_receipt_ids.iter().map(String::as_str).collect();
    let found_receipt_ids = collect_found_receipt_ids(&records, &referenced_set);
    let found_set: HashSet<&str> = found_receipt_ids.iter().map(String::as_str).collect();
    let missing_receipt_ids = referenced_receipt_ids
        .iter()
        .filter(|receipt_id| !found_set.contains(receipt_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    let referenced_signature_failure_ids = verify_result
        .failures
        .iter()
        .filter(|failure| referenced_set.contains(failure.receipt_id.as_str()))
        .map(|failure| failure.receipt_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let chain = RunVerifyChainJson {
        total_records: chain_status.total_records,
        valid: chain_status.valid,
        break_point: chain_status
            .break_point
            .map(|break_point| RunVerifyChainBreakJson {
                file: break_point.file,
                line: break_point.line,
                expected_hash: break_point.expected_hash,
                actual_hash: break_point.actual_hash,
            }),
    };
    let signatures = RunVerifySignaturesJson {
        total: verify_result.total,
        valid: verify_result.valid,
        failures: verify_result
            .failures
            .into_iter()
            .map(|failure| RunVerifyFailureJson {
                file: failure.file,
                line: failure.line,
                receipt_id: failure.receipt_id,
                reason: failure.reason,
            })
            .collect(),
    };

    let verified = chain.valid
        && missing_receipt_ids.is_empty()
        && referenced_signature_failure_ids.is_empty();

    Ok(RunVerifyJsonResponse {
        run,
        agent_name,
        signet_dir,
        has_receipts: true,
        verified,
        referenced_receipt_ids,
        found_receipt_ids,
        missing_receipt_ids,
        referenced_signature_failure_ids,
        chain,
        signatures,
    })
}

async fn load_run(store: &ManagedStore, run_id: &str) -> anyhow::Result<ManagedRun> {
    store
        .get_run(run_id)
        .await?
        .with_context(|| format!("Managed run not found: {run_id}"))
}

async fn load_agent_labels(
    store: &ManagedStore,
    runs: &[ManagedRun],
) -> anyhow::Result<HashMap<String, String>> {
    let mut labels = HashMap::new();
    for run in runs {
        if labels.contains_key(&run.agent_id) {
            continue;
        }
        labels.insert(
            run.agent_id.clone(),
            load_agent_label(store, &run.agent_id).await?,
        );
    }
    Ok(labels)
}

async fn load_agent_label(store: &ManagedStore, agent_id: &str) -> anyhow::Result<String> {
    Ok(match store.get_agent(agent_id).await? {
        Some(agent) => agent.name,
        None => format!("(missing:{})", truncate(agent_id, 16)),
    })
}

fn print_run_event(event: &ManagedRunEvent) {
    println!(
        "{}  {:<18} {}",
        format_ts(event.created_at),
        event.kind.as_str(),
        format_event_detail(event),
    );
}

fn format_event_detail(event: &ManagedRunEvent) -> String {
    let mut parts = Vec::new();
    if let Some(tool_name) = &event.tool_name {
        parts.push(format!("tool={tool_name}"));
    }
    if let Some(tool_call_id) = &event.tool_call_id {
        parts.push(format!("call={tool_call_id}"));
    }
    if let Some(message) = &event.message {
        parts.push(message.clone());
    }
    if let Some(metadata) = &event.metadata {
        if let Some(receipt_id) = metadata.get("receipt_id").and_then(|value| value.as_str()) {
            parts.push(format!("receipt={receipt_id}"));
        }
        if let Some(record_hash) = metadata.get("record_hash").and_then(|value| value.as_str()) {
            parts.push(format!("record_hash={record_hash}"));
        }
        if let Some(response_hash) = metadata
            .get("response_hash")
            .and_then(|value| value.as_str())
        {
            parts.push(format!("response_hash={response_hash}"));
        }
    }

    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

fn collect_signet_receipt_ids(events: &[ManagedRunEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                hermes_managed::ManagedRunEventKind::ToolRequestSigned
                    | hermes_managed::ManagedRunEventKind::ToolResponseSigned
            )
        })
        .filter_map(|event| {
            event.metadata.as_ref().and_then(|metadata| {
                metadata
                    .get("receipt_id")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn collect_found_receipt_ids(
    records: &[audit::AuditRecord],
    referenced_receipts: &HashSet<&str>,
) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| {
            record
                .receipt
                .get("id")
                .and_then(|value| value.as_str())
                .filter(|receipt_id| referenced_receipts.contains(receipt_id))
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn apply_verify_strictness(summary: &RunVerifyJsonResponse, strict: bool) -> anyhow::Result<()> {
    if strict && !summary.verified {
        anyhow::bail!(
            "Signet verification failed for run {}: {}",
            summary.run.id,
            verification_failure_summary(summary)
        );
    }

    Ok(())
}

fn verification_failure_summary(summary: &RunVerifyJsonResponse) -> String {
    let mut reasons = Vec::new();

    if !summary.has_receipts {
        reasons.push("no Signet receipts recorded".to_string());
    }
    if !summary.chain.valid {
        reasons.push("audit chain invalid".to_string());
    }
    if !summary.missing_receipt_ids.is_empty() {
        reasons.push(format!(
            "{} referenced receipt(s) missing from audit",
            summary.missing_receipt_ids.len()
        ));
    }
    if !summary.referenced_signature_failure_ids.is_empty() {
        reasons.push(format!(
            "{} referenced receipt signature(s) failed verification",
            summary.referenced_signature_failure_ids.len()
        ));
    }

    if reasons.is_empty() {
        "verification did not succeed".to_string()
    } else {
        reasons.join("; ")
    }
}

fn managed_gateway_base_url(app_config: &AppConfig) -> anyhow::Result<String> {
    if let Ok(value) = std::env::var("HERMES_GATEWAY_BASE_URL") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.trim_end_matches('/').to_string());
        }
    }

    let bind_addr = app_config
        .gateway
        .as_ref()
        .and_then(|gateway| gateway.api_server.as_ref())
        .map(|api| api.bind_addr.as_str())
        .unwrap_or("127.0.0.1:8080");
    Ok(format!("http://{}", bind_addr.trim_end_matches('/')))
}

fn managed_gateway_api_key(app_config: &AppConfig) -> anyhow::Result<String> {
    if let Some(api_key) = app_config
        .gateway
        .as_ref()
        .and_then(|gateway| gateway.api_server.as_ref())
        .and_then(|api| api.api_key.as_ref())
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(api_key.clone());
    }

    std::env::var("HERMES_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .context(
            "No managed gateway API key found. Set gateway.api_server.api_key or HERMES_API_KEY",
        )
}

fn format_ts(value: chrono::DateTime<chrono::Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn truncate(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }

    let visible = max_chars.saturating_sub(1);
    let mut truncated = value.chars().take(visible).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use hermes_config::config::SignetConfig;
    use signet_core::{Action, audit, generate_and_save, load_signing_key, sign, sign_compound};
    use tempfile::TempDir;

    use super::*;
    use hermes_managed::{
        ManagedAgent, ManagedAgentVersion, ManagedRunEventDraft, ManagedRunEventKind,
        ManagedRunStatus,
    };

    async fn temp_store() -> (TempDir, ManagedStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ManagedStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();
        (dir, store)
    }

    #[test]
    fn format_event_detail_includes_tool_metadata_and_message() {
        let event = ManagedRunEvent {
            id: 1,
            run_id: "run_123".to_string(),
            kind: ManagedRunEventKind::ToolProgress,
            message: Some("reading README.md".to_string()),
            tool_name: Some("read_file".to_string()),
            tool_call_id: Some("call_123".to_string()),
            metadata: Some(serde_json::json!({
                "receipt_id": "rec_123",
                "record_hash": "sha256:abc",
            })),
            created_at: Utc::now(),
        };

        assert_eq!(
            format_event_detail(&event),
            "tool=read_file call=call_123 reading README.md receipt=rec_123 record_hash=sha256:abc"
        );
    }

    #[test]
    fn print_json_renders_pretty_json() {
        let payload = RunGetJsonResponse {
            run: ManagedRun {
                id: "run_123".to_string(),
                agent_id: "agent_123".to_string(),
                agent_version: 1,
                status: hermes_managed::ManagedRunStatus::Completed,
                model: "openai/gpt-4o-mini".to_string(),
                started_at: Utc::now(),
                updated_at: Utc::now(),
                ended_at: None,
                cancel_requested_at: None,
                last_error: None,
            },
            agent_name: "observer".to_string(),
        };

        let rendered = serde_json::to_string_pretty(&payload).unwrap();
        assert!(rendered.contains("\"agent_name\": \"observer\""));
        assert!(rendered.contains("\"status\": \"completed\""));
    }

    #[test]
    fn apply_verify_strictness_allows_non_strict_failures() {
        let summary = RunVerifyJsonResponse {
            run: ManagedRun {
                id: "run_123".to_string(),
                agent_id: "agent_123".to_string(),
                agent_version: 1,
                status: ManagedRunStatus::Completed,
                model: "openai/gpt-4o-mini".to_string(),
                started_at: Utc::now(),
                updated_at: Utc::now(),
                ended_at: None,
                cancel_requested_at: None,
                last_error: None,
            },
            agent_name: "observer".to_string(),
            signet_dir: PathBuf::from("/tmp/signet"),
            has_receipts: true,
            verified: false,
            referenced_receipt_ids: vec!["rec_123".to_string()],
            found_receipt_ids: vec![],
            missing_receipt_ids: vec!["rec_123".to_string()],
            referenced_signature_failure_ids: vec![],
            chain: RunVerifyChainJson {
                total_records: 0,
                valid: true,
                break_point: None,
            },
            signatures: RunVerifySignaturesJson {
                total: 0,
                valid: 0,
                failures: Vec::new(),
            },
        };

        assert!(apply_verify_strictness(&summary, false).is_ok());
        assert!(apply_verify_strictness(&summary, true).is_err());
    }

    #[test]
    fn verification_failure_summary_lists_relevant_reasons() {
        let summary = RunVerifyJsonResponse {
            run: ManagedRun {
                id: "run_123".to_string(),
                agent_id: "agent_123".to_string(),
                agent_version: 1,
                status: ManagedRunStatus::Completed,
                model: "openai/gpt-4o-mini".to_string(),
                started_at: Utc::now(),
                updated_at: Utc::now(),
                ended_at: None,
                cancel_requested_at: None,
                last_error: None,
            },
            agent_name: "observer".to_string(),
            signet_dir: PathBuf::from("/tmp/signet"),
            has_receipts: true,
            verified: false,
            referenced_receipt_ids: vec!["rec_123".to_string(), "rec_456".to_string()],
            found_receipt_ids: vec!["rec_123".to_string()],
            missing_receipt_ids: vec!["rec_456".to_string()],
            referenced_signature_failure_ids: vec!["rec_123".to_string()],
            chain: RunVerifyChainJson {
                total_records: 7,
                valid: false,
                break_point: None,
            },
            signatures: RunVerifySignaturesJson {
                total: 2,
                valid: 1,
                failures: Vec::new(),
            },
        };

        assert_eq!(
            verification_failure_summary(&summary),
            "audit chain invalid; 1 referenced receipt(s) missing from audit; 1 referenced receipt signature(s) failed verification"
        );
    }

    #[test]
    fn verification_failure_summary_handles_missing_receipts_case() {
        let summary = RunVerifyJsonResponse {
            run: ManagedRun {
                id: "run_123".to_string(),
                agent_id: "agent_123".to_string(),
                agent_version: 1,
                status: ManagedRunStatus::Completed,
                model: "openai/gpt-4o-mini".to_string(),
                started_at: Utc::now(),
                updated_at: Utc::now(),
                ended_at: None,
                cancel_requested_at: None,
                last_error: None,
            },
            agent_name: "observer".to_string(),
            signet_dir: PathBuf::from("/tmp/signet"),
            has_receipts: false,
            verified: false,
            referenced_receipt_ids: Vec::new(),
            found_receipt_ids: Vec::new(),
            missing_receipt_ids: Vec::new(),
            referenced_signature_failure_ids: Vec::new(),
            chain: RunVerifyChainJson {
                total_records: 0,
                valid: true,
                break_point: None,
            },
            signatures: RunVerifySignaturesJson {
                total: 0,
                valid: 0,
                failures: Vec::new(),
            },
        };

        assert_eq!(
            verification_failure_summary(&summary),
            "no Signet receipts recorded"
        );
    }

    #[tokio::test]
    async fn load_agent_label_falls_back_when_agent_is_missing() {
        let (_dir, store) = temp_store().await;
        let label = load_agent_label(&store, "agent_missing_identifier")
            .await
            .unwrap();
        assert!(label.starts_with("(missing:"));
    }

    #[test]
    fn collect_signet_receipt_ids_uses_structured_signet_events() {
        let events = vec![
            ManagedRunEvent {
                id: 1,
                run_id: "run_123".to_string(),
                kind: ManagedRunEventKind::ToolRequestSigned,
                message: None,
                tool_name: Some("read_file".to_string()),
                tool_call_id: Some("call_123".to_string()),
                metadata: Some(serde_json::json!({ "receipt_id": "rec_req" })),
                created_at: Utc::now(),
            },
            ManagedRunEvent {
                id: 2,
                run_id: "run_123".to_string(),
                kind: ManagedRunEventKind::ToolResponseSigned,
                message: None,
                tool_name: Some("read_file".to_string()),
                tool_call_id: Some("call_123".to_string()),
                metadata: Some(serde_json::json!({ "receipt_id": "rec_res" })),
                created_at: Utc::now(),
            },
            ManagedRunEvent {
                id: 3,
                run_id: "run_123".to_string(),
                kind: ManagedRunEventKind::ToolProgress,
                message: Some("plain progress".to_string()),
                tool_name: Some("read_file".to_string()),
                tool_call_id: None,
                metadata: Some(serde_json::json!({ "receipt_id": "ignored" })),
                created_at: Utc::now(),
            },
        ];

        assert_eq!(
            collect_signet_receipt_ids(&events),
            vec!["rec_req".to_string(), "rec_res".to_string()]
        );
    }

    #[tokio::test]
    async fn build_run_signet_verification_reports_verified_signed_run() {
        let (_dir, store) = temp_store().await;
        let signet_dir = tempfile::tempdir().unwrap();

        generate_and_save(signet_dir.path(), "managed-test", Some("qa"), None, None).unwrap();
        let signing_key = load_signing_key(signet_dir.path(), "managed-test", None).unwrap();

        let agent = ManagedAgent::new("observer");
        store.create_agent(&agent).await.unwrap();
        let version =
            ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "observe everything");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Completed;
        store.create_run(&run).await.unwrap();

        let action = Action {
            tool: "read_file".to_string(),
            params: serde_json::json!({ "path": "/tmp/demo.txt" }),
            params_hash: String::new(),
            target: "hermes://toolset/file/read_file".to_string(),
            transport: "in_process".to_string(),
            session: Some(run.id.clone()),
            call_id: Some("call_123".to_string()),
            response_hash: None,
        };
        let request_receipt = sign(&signing_key, &action, "managed-test", "qa").unwrap();
        let request_record = audit::append(
            signet_dir.path(),
            &serde_json::to_value(&request_receipt).unwrap(),
        )
        .unwrap();
        let response_receipt = sign_compound(
            &signing_key,
            &action,
            &serde_json::json!({ "content": "ok" }),
            "managed-test",
            "qa",
            &request_receipt.ts,
            &Utc::now().to_rfc3339(),
        )
        .unwrap();
        let response_record = audit::append(
            signet_dir.path(),
            &serde_json::to_value(&response_receipt).unwrap(),
        )
        .unwrap();

        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolRequestSigned,
                    message: Some("Signet request receipt appended".to_string()),
                    tool_name: Some("read_file".to_string()),
                    tool_call_id: Some("call_123".to_string()),
                    metadata: Some(serde_json::json!({
                        "receipt_id": request_receipt.id,
                        "receipt_version": 1,
                        "record_hash": request_record.record_hash,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolResponseSigned,
                    message: Some("Signet response receipt appended".to_string()),
                    tool_name: Some("read_file".to_string()),
                    tool_call_id: Some("call_123".to_string()),
                    metadata: Some(serde_json::json!({
                        "receipt_id": response_receipt.id,
                        "receipt_version": 2,
                        "record_hash": response_record.record_hash,
                        "response_hash": response_receipt.response.content_hash,
                    })),
                },
            )
            .await
            .unwrap();

        let config = AppConfig {
            signet: SignetConfig {
                enabled: true,
                key_name: "managed-test".to_string(),
                owner: "qa".to_string(),
                dir: Some(signet_dir.path().to_path_buf()),
            },
            ..AppConfig::default()
        };

        let summary = build_run_signet_verification(&store, &config, &run.id)
            .await
            .unwrap();

        assert!(summary.has_receipts);
        assert!(summary.verified);
        assert_eq!(summary.referenced_receipt_ids.len(), 2);
        assert!(summary.missing_receipt_ids.is_empty());
        assert!(summary.referenced_signature_failure_ids.is_empty());
        assert!(summary.chain.valid);
        assert_eq!(summary.signatures.total, 2);
        assert_eq!(summary.signatures.valid, 2);
    }

    #[tokio::test]
    async fn load_run_returns_existing_run() {
        let (_dir, store) = temp_store().await;
        let agent = ManagedAgent::new("observer");
        store.create_agent(&agent).await.unwrap();
        let version =
            ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "observe everything");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunStarted,
                    message: None,
                    tool_name: None,
                    tool_call_id: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();

        let loaded = load_run(&store, &run.id).await.unwrap();
        assert_eq!(loaded.id, run.id);
    }
}
