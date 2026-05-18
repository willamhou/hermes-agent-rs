use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::Context;
use clap::Subcommand;
use hermes_config::config::AppConfig;
use hermes_managed::{
    ManagedMcpAdmissionRejection, ManagedRun, ManagedRunArtifact,
    ManagedRunArtifactContinuitySummary, ManagedRunArtifactKind, ManagedRunBrowserHandoffSummary,
    ManagedRunBrowserSessionCheckpointSummary, ManagedRunCleanupFailureSummary,
    ManagedRunContinuationBoundaryKind, ManagedRunContinuationCheckpointSummary,
    ManagedRunContinuationSummary, ManagedRunDerivedSummary, ManagedRunEvent,
    ManagedRunInterruptionCause, ManagedRunInterruptionSummary, ManagedRunMcpHandoffSummary,
    ManagedRunMcpRuntimeCheckpointSummary, ManagedRunOwnerSnapshot, ManagedRunOwnerState,
    ManagedRunProcessHandoffSummary, ManagedRunProviderCallFenceSummary,
    ManagedRunRecoveryDecisionKind, ManagedRunRecoveryDecisionReason,
    ManagedRunRecoveryDecisionSummary, ManagedRunRecoveryHint, ManagedRunReplayChildSummary,
    ManagedRunReplayProvenanceSummary, ManagedRunTakeoverAssessmentSummary,
    ManagedRunTakeoverSummary, ManagedStore, load_managed_run_derived_summary,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use signet_core::audit::{self, AuditFilter};

use crate::mcp::rejection_operator_summary;

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
    /// Show persisted artifacts for one managed run
    Artifacts {
        /// Run id
        run: String,
        /// Maximum number of artifacts to print
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Include artifacts from replay ancestors, oldest first
        #[arg(long)]
        lineage: bool,
        /// Emit JSON instead of text output
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
        RunsAction::Artifacts {
            run,
            limit,
            lineage,
            json,
        } => show_run_artifacts(&store, &run, limit, lineage, json).await,
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
    #[serde(default, skip_serializing_if = "ManagedRunDerivedSummary::is_empty")]
    summary: ManagedRunDerivedSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_admission_rejection: Option<ManagedMcpAdmissionRejection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_failure: Option<ManagedRunCleanupFailureSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ownership: Option<ManagedRunOwnerSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover_assessment: Option<ManagedRunTakeoverAssessmentSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover: Option<ManagedRunTakeoverSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_decision: Option<ManagedRunRecoveryDecisionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_hint: Option<ManagedRunRecoveryHint>,
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
    #[serde(default, skip_serializing_if = "ManagedRunDerivedSummary::is_empty")]
    summary: ManagedRunDerivedSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_admission_rejection: Option<ManagedMcpAdmissionRejection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_failure: Option<ManagedRunCleanupFailureSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ownership: Option<ManagedRunOwnerSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover_assessment: Option<ManagedRunTakeoverAssessmentSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover: Option<ManagedRunTakeoverSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_decision: Option<ManagedRunRecoveryDecisionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_hint: Option<ManagedRunRecoveryHint>,
}

#[derive(Serialize)]
struct RunEventsJsonResponse {
    object: &'static str,
    run: ManagedRun,
    agent_name: String,
    data: Vec<ManagedRunEvent>,
    #[serde(default, skip_serializing_if = "ManagedRunDerivedSummary::is_empty")]
    summary: ManagedRunDerivedSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_admission_rejection: Option<ManagedMcpAdmissionRejection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_failure: Option<ManagedRunCleanupFailureSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ownership: Option<ManagedRunOwnerSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover_assessment: Option<ManagedRunTakeoverAssessmentSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover: Option<ManagedRunTakeoverSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_decision: Option<ManagedRunRecoveryDecisionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_hint: Option<ManagedRunRecoveryHint>,
}

#[derive(Serialize)]
struct RunArtifactsJsonResponse {
    object: &'static str,
    run: ManagedRun,
    agent_name: String,
    data: Vec<ManagedRunArtifact>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    lineage_run_ids: Vec<String>,
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

async fn load_run_summary(
    store: &ManagedStore,
    run_id: &str,
) -> anyhow::Result<ManagedRunDerivedSummary> {
    Ok(load_managed_run_derived_summary(store, run_id).await?)
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
    let mut summaries_by_run_id = HashMap::with_capacity(runs.len());
    for run in &runs {
        summaries_by_run_id.insert(run.id.clone(), load_run_summary(store, &run.id).await?);
    }
    if json {
        let data = runs
            .into_iter()
            .map(|run| {
                let summary = summaries_by_run_id.remove(&run.id).unwrap_or_default();
                RunJsonEntry {
                    agent_name: agent_labels
                        .get(&run.agent_id)
                        .cloned()
                        .unwrap_or_else(|| run.agent_id.clone()),
                    summary: summary.clone(),
                    mcp_admission_rejection: summary.mcp_admission_rejection,
                    cleanup_failure: summary.cleanup_failure,
                    ownership: summary.ownership,
                    takeover_assessment: summary.takeover_assessment,
                    takeover: summary.takeover,
                    recovery_decision: summary.recovery_decision,
                    recovery_hint: summary.recovery_hint,
                    run,
                }
            })
            .collect();
        print_json(&RunListJsonResponse {
            object: "list",
            data,
        })?;
        return Ok(());
    }

    println!(
        "{:<24} {:<20} {:<8} {:<12} {:<24} {:<20} Started",
        "ID", "Agent", "Version", "Status", "Model", "MCP admission"
    );
    println!("{}", "-".repeat(140));
    for run in &runs {
        let agent_label = agent_labels
            .get(&run.agent_id)
            .map(String::as_str)
            .unwrap_or(run.agent_id.as_str());
        let rejection = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.mcp_admission_rejection.as_ref());
        let cleanup_failure = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.cleanup_failure.as_ref());
        let ownership = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.ownership.as_ref());
        let takeover = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.takeover.as_ref());
        let recovery_decision = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.recovery_decision.as_ref());
        let recovery_hint = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.recovery_hint.as_ref());
        let continuation_checkpoint = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.continuation_checkpoint.as_ref());
        let provider_call_fence = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.provider_call_fence.as_ref());
        let process_handoff = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.process_handoff.as_ref());
        let browser_handoff = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.browser_handoff.as_ref());
        let browser_session_checkpoint = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.browser_session_checkpoint.as_ref());
        let mcp_handoff = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.mcp_handoff.as_ref());
        let mcp_runtime_checkpoint = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.mcp_runtime_checkpoint.as_ref());
        let artifact_continuity = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.artifact_continuity.as_ref());
        let replay_provenance = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.replay_provenance.as_ref());
        let replay_child = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.replay_child.as_ref());
        println!(
            "{:<24} {:<20} {:<8} {:<12} {:<24} {:<20} {}",
            truncate(&run.id, 24),
            truncate(agent_label, 20),
            run.agent_version,
            run.status.as_str(),
            truncate(&run.model, 24),
            truncate(
                rejection.map(|value| value.code.as_str()).unwrap_or("-"),
                20
            ),
            format_ts(run.started_at),
        );
        if let Some(rejection) = rejection.and_then(rejection_operator_summary) {
            println!("  mcp: {}", truncate(&rejection, 160));
        }
        if let Some(cleanup) = cleanup_failure {
            println!(
                "  cleanup: phase={} cleaned={}/{} failures={}",
                cleanup.phase,
                cleanup.cleaned,
                cleanup.attempted,
                cleanup.failures.len()
            );
        }
        if let Some(interruption) = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.interruption.as_ref())
        {
            println!(
                "  interruption: {}",
                interruption_operator_summary(interruption)
            );
            if let Some(detail) = interruption_detail(interruption) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(ownership) = ownership {
            println!("  owner: {}", ownership_operator_summary(ownership));
            if let Some(detail) = ownership_detail(ownership) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(takeover_assessment) = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.takeover_assessment.as_ref())
        {
            println!(
                "  takeover assessment: {}",
                takeover_assessment_operator_summary(takeover_assessment)
            );
            if let Some(detail) = takeover_assessment_detail(takeover_assessment) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(takeover) = takeover {
            println!("  takeover: {}", takeover_operator_summary(takeover));
            if let Some(detail) = takeover_detail(takeover) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(continuation) = summaries_by_run_id
            .get(&run.id)
            .and_then(|summary| summary.continuation.as_ref())
        {
            println!(
                "  continuation_lineage: {}",
                continuation_lineage_operator_summary(continuation)
            );
            if let Some(detail) = continuation_lineage_detail(continuation) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(decision) = recovery_decision {
            println!(
                "  recovery decision: {}",
                recovery_decision_operator_summary(decision)
            );
            if let Some(detail) = recovery_decision_detail(decision) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(recovery) = recovery_hint {
            println!(
                "  recovery: replayable={} action={} reuses_session={}",
                yes_no(recovery.replayable),
                recovery.suggested_action.as_deref().unwrap_or("-"),
                yes_no(recovery.reuses_session_id)
            );
        }
        if let Some(replay) = replay_provenance {
            println!("  replay: {}", replay_provenance_operator_summary(replay));
            if let Some(detail) = replay_provenance_detail(replay) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(replay_child) = replay_child {
            println!(
                "  replacement: {}",
                replay_child_operator_summary(replay_child)
            );
            if let Some(detail) = replay_child_detail(replay_child) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(artifact) = artifact_continuity {
            println!(
                "  artifacts: {}",
                artifact_continuity_operator_summary(artifact)
            );
            if let Some(detail) = artifact_continuity_detail(artifact) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(checkpoint) = continuation_checkpoint {
            println!(
                "  continuation: {}",
                continuation_operator_summary(checkpoint)
            );
        }
        if let Some(fence) = provider_call_fence {
            println!(
                "  provider fence: {}",
                provider_fence_operator_summary(fence)
            );
        }
        if let Some(handoff) = process_handoff {
            println!(
                "  process handoff: {}",
                process_handoff_operator_summary(handoff)
            );
            if let Some(detail) = process_handoff_detail(handoff) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(handoff) = browser_handoff {
            println!(
                "  browser handoff: {}",
                browser_handoff_operator_summary(handoff)
            );
            if let Some(detail) = browser_handoff_detail(handoff) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(checkpoint) = browser_session_checkpoint {
            println!(
                "  browser checkpoint: {}",
                browser_session_checkpoint_operator_summary(checkpoint)
            );
            if let Some(detail) = browser_session_checkpoint_detail(checkpoint) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(handoff) = mcp_handoff {
            println!("  mcp handoff: {}", mcp_handoff_operator_summary(handoff));
            if let Some(detail) = mcp_handoff_detail(handoff) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
        if let Some(checkpoint) = mcp_runtime_checkpoint {
            println!(
                "  mcp runtime: {}",
                mcp_runtime_checkpoint_operator_summary(checkpoint)
            );
            if let Some(detail) = mcp_runtime_checkpoint_detail(checkpoint) {
                println!("    detail: {}", truncate(&detail, 160));
            }
        }
    }

    Ok(())
}

async fn get_run(store: &ManagedStore, run_ref: &str, json: bool) -> anyhow::Result<()> {
    let run = load_run(store, run_ref).await?;
    let agent_label = load_agent_label(store, &run.agent_id).await?;
    let summary = load_run_summary(store, &run.id).await?;

    if json {
        print_json(&RunGetJsonResponse {
            run,
            agent_name: agent_label,
            summary: summary.clone(),
            mcp_admission_rejection: summary.mcp_admission_rejection.clone(),
            cleanup_failure: summary.cleanup_failure.clone(),
            ownership: summary.ownership.clone(),
            takeover_assessment: summary.takeover_assessment.clone(),
            takeover: summary.takeover.clone(),
            recovery_decision: summary.recovery_decision.clone(),
            recovery_hint: summary.recovery_hint.clone(),
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
    if let Some(rejection) = summary.mcp_admission_rejection {
        println!("MCP admission:    {}", rejection.code);
        println!("MCP detail:       {}", rejection.error);
        if let Some(summary) = rejection_operator_summary(&rejection) {
            println!("MCP summary:      {}", summary);
        }
    }
    if let Some(cleanup) = summary.cleanup_failure {
        println!(
            "Cleanup failure:  phase={} cleaned={}/{} failures={}",
            cleanup.phase,
            cleanup.cleaned,
            cleanup.attempted,
            cleanup.failures.len()
        );
        if let Some(first_failure) = cleanup.failures.first() {
            println!("Cleanup detail:   {}", first_failure);
        }
    }
    if let Some(interruption) = summary.interruption {
        println!(
            "Interruption:     {}",
            interruption_operator_summary(&interruption)
        );
        if let Some(detail) = interruption_detail(&interruption) {
            println!("Interrupt detail: {}", detail);
        }
    }
    if let Some(ownership) = summary.ownership {
        println!(
            "Ownership:        {}",
            ownership_operator_summary(&ownership)
        );
        if let Some(detail) = ownership_detail(&ownership) {
            println!("Ownership detail: {}", detail);
        }
    }
    if let Some(takeover_assessment) = summary.takeover_assessment {
        println!(
            "Takeover assess:  {}",
            takeover_assessment_operator_summary(&takeover_assessment)
        );
        if let Some(detail) = takeover_assessment_detail(&takeover_assessment) {
            println!("Assess detail:    {}", detail);
        }
    }
    if let Some(takeover) = summary.takeover {
        println!("Takeover:         {}", takeover_operator_summary(&takeover));
        if let Some(detail) = takeover_detail(&takeover) {
            println!("Takeover detail:  {}", detail);
        }
    }
    if let Some(continuation) = summary.continuation {
        println!(
            "Continuation run: {}",
            continuation_lineage_operator_summary(&continuation)
        );
        if let Some(detail) = continuation_lineage_detail(&continuation) {
            println!("Continuation det: {}", detail);
        }
    }
    if let Some(decision) = summary.recovery_decision {
        println!(
            "Recovery decis.:  {}",
            recovery_decision_operator_summary(&decision)
        );
        if let Some(detail) = recovery_decision_detail(&decision) {
            println!("Recovery dec det: {}", detail);
        }
    }
    if let Some(recovery) = summary.recovery_hint {
        println!(
            "Recovery hint:    replayable={} action={} reuses_session={}",
            yes_no(recovery.replayable),
            recovery.suggested_action.as_deref().unwrap_or("-"),
            yes_no(recovery.reuses_session_id)
        );
        if let Some(note) = recovery.note {
            println!("Recovery detail:  {}", note);
        }
    }
    if let Some(replay) = summary.replay_provenance {
        println!(
            "Replay lineage:   {}",
            replay_provenance_operator_summary(&replay)
        );
        if let Some(detail) = replay_provenance_detail(&replay) {
            println!("Replay detail:    {}", detail);
        }
    }
    if let Some(replay_child) = summary.replay_child {
        println!(
            "Replacement run:  {}",
            replay_child_operator_summary(&replay_child)
        );
        if let Some(detail) = replay_child_detail(&replay_child) {
            println!("Replacement det.: {}", detail);
        }
    }
    if let Some(artifact) = summary.artifact_continuity {
        println!(
            "Artifacts:        {}",
            artifact_continuity_operator_summary(&artifact)
        );
        if let Some(detail) = artifact_continuity_detail(&artifact) {
            println!("Artifact detail:  {}", detail);
        }
    }
    if let Some(checkpoint) = summary.continuation_checkpoint {
        println!(
            "Continuation:     {}",
            continuation_operator_summary(&checkpoint)
        );
    }
    if let Some(fence) = summary.provider_call_fence {
        println!(
            "Provider fence:   {}",
            provider_fence_operator_summary(&fence)
        );
    }
    if let Some(handoff) = summary.process_handoff {
        println!(
            "Process handoff:  {}",
            process_handoff_operator_summary(&handoff)
        );
        if let Some(detail) = process_handoff_detail(&handoff) {
            println!("Process detail:   {}", detail);
        }
    }
    if let Some(handoff) = summary.browser_handoff {
        println!(
            "Browser handoff:  {}",
            browser_handoff_operator_summary(&handoff)
        );
        if let Some(detail) = browser_handoff_detail(&handoff) {
            println!("Browser detail:   {}", detail);
        }
    }
    if let Some(checkpoint) = summary.browser_session_checkpoint {
        println!(
            "Browser checkpoint:{}",
            if checkpoint.session_open { " live" } else { "" }
        );
        println!(
            "Browser summary:  {}",
            browser_session_checkpoint_operator_summary(&checkpoint)
        );
        if let Some(detail) = browser_session_checkpoint_detail(&checkpoint) {
            println!("Browser state:    {}", detail);
        }
    }
    if let Some(handoff) = summary.mcp_handoff {
        println!(
            "MCP handoff:      {}",
            mcp_handoff_operator_summary(&handoff)
        );
        if let Some(detail) = mcp_handoff_detail(&handoff) {
            println!("MCP handoff det.: {}", detail);
        }
    }
    if let Some(checkpoint) = summary.mcp_runtime_checkpoint {
        println!(
            "MCP runtime:      {}",
            mcp_runtime_checkpoint_operator_summary(&checkpoint)
        );
        if let Some(detail) = mcp_runtime_checkpoint_detail(&checkpoint) {
            println!("MCP runtime det.: {}", detail);
        }
    }

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
    let summary = load_run_summary(store, &run.id).await?;
    let events = store
        .list_run_events_tail(&run.id, limit.clamp(1, 1000))
        .await?;

    if json {
        print_json(&RunEventsJsonResponse {
            object: "list",
            run,
            agent_name: agent_label,
            data: events,
            summary: summary.clone(),
            mcp_admission_rejection: summary.mcp_admission_rejection.clone(),
            cleanup_failure: summary.cleanup_failure.clone(),
            ownership: summary.ownership.clone(),
            takeover_assessment: summary.takeover_assessment.clone(),
            takeover: summary.takeover.clone(),
            recovery_decision: summary.recovery_decision.clone(),
            recovery_hint: summary.recovery_hint.clone(),
        })?;
        return Ok(());
    }

    println!("Run:    {}", run.id);
    println!("Status: {}", run.status.as_str());
    if let Some(rejection) = &summary.mcp_admission_rejection {
        println!("MCP:    {}", rejection.code);
        if let Some(summary) = rejection_operator_summary(rejection) {
            println!("        {}", summary);
        }
    }
    if let Some(cleanup) = &summary.cleanup_failure {
        println!(
            "Cleanup: phase={} cleaned={}/{} failures={}",
            cleanup.phase,
            cleanup.cleaned,
            cleanup.attempted,
            cleanup.failures.len()
        );
    }
    if let Some(interruption) = &summary.interruption {
        println!("Interrupt: {}", interruption_operator_summary(interruption));
        if let Some(detail) = interruption_detail(interruption) {
            println!("           {}", truncate(&detail, 160));
        }
    }
    if let Some(ownership) = &summary.ownership {
        println!("Owner: {}", ownership_operator_summary(ownership));
        if let Some(detail) = ownership_detail(ownership) {
            println!("       {}", truncate(&detail, 160));
        }
    }
    if let Some(takeover_assessment) = &summary.takeover_assessment {
        println!(
            "Assessment: {}",
            takeover_assessment_operator_summary(takeover_assessment)
        );
        if let Some(detail) = takeover_assessment_detail(takeover_assessment) {
            println!("            {}", truncate(&detail, 160));
        }
    }
    if let Some(takeover) = &summary.takeover {
        println!("Takeover: {}", takeover_operator_summary(takeover));
        if let Some(detail) = takeover_detail(takeover) {
            println!("          {}", truncate(&detail, 160));
        }
    }
    if let Some(continuation) = &summary.continuation {
        println!(
            "Continuation: {}",
            continuation_lineage_operator_summary(continuation)
        );
        if let Some(detail) = continuation_lineage_detail(continuation) {
            println!("             {}", truncate(&detail, 160));
        }
    }
    if let Some(decision) = &summary.recovery_decision {
        println!("Decision: {}", recovery_decision_operator_summary(decision));
        if let Some(detail) = recovery_decision_detail(decision) {
            println!("          {}", truncate(&detail, 160));
        }
    }
    if let Some(recovery) = &summary.recovery_hint {
        println!(
            "Recovery: replayable={} action={} reuses_session={}",
            yes_no(recovery.replayable),
            recovery.suggested_action.as_deref().unwrap_or("-"),
            yes_no(recovery.reuses_session_id)
        );
    }
    if let Some(replay) = &summary.replay_provenance {
        println!("Replay: {}", replay_provenance_operator_summary(replay));
        if let Some(detail) = replay_provenance_detail(replay) {
            println!("        {}", truncate(&detail, 160));
        }
    }
    if let Some(replay_child) = &summary.replay_child {
        println!(
            "Replacement: {}",
            replay_child_operator_summary(replay_child)
        );
        if let Some(detail) = replay_child_detail(replay_child) {
            println!("             {}", truncate(&detail, 160));
        }
    }
    if let Some(artifact) = &summary.artifact_continuity {
        println!(
            "Artifacts: {}",
            artifact_continuity_operator_summary(artifact)
        );
        if let Some(detail) = artifact_continuity_detail(artifact) {
            println!("          {}", truncate(&detail, 160));
        }
    }
    if let Some(checkpoint) = &summary.continuation_checkpoint {
        println!(
            "Continuation: {}",
            continuation_operator_summary(checkpoint)
        );
    }
    if let Some(fence) = &summary.provider_call_fence {
        println!("Provider fence: {}", provider_fence_operator_summary(fence));
    }
    if let Some(handoff) = &summary.process_handoff {
        println!(
            "Process handoff: {}",
            process_handoff_operator_summary(handoff)
        );
        if let Some(detail) = process_handoff_detail(handoff) {
            println!("               {}", truncate(&detail, 160));
        }
    }
    if let Some(handoff) = &summary.browser_handoff {
        println!(
            "Browser handoff: {}",
            browser_handoff_operator_summary(handoff)
        );
        if let Some(detail) = browser_handoff_detail(handoff) {
            println!("               {}", truncate(&detail, 160));
        }
    }
    if let Some(checkpoint) = &summary.browser_session_checkpoint {
        println!(
            "Browser checkpoint: {}",
            browser_session_checkpoint_operator_summary(checkpoint)
        );
        if let Some(detail) = browser_session_checkpoint_detail(checkpoint) {
            println!("                  {}", truncate(&detail, 160));
        }
    }
    if let Some(handoff) = &summary.mcp_handoff {
        println!("MCP handoff: {}", mcp_handoff_operator_summary(handoff));
        if let Some(detail) = mcp_handoff_detail(handoff) {
            println!("            {}", truncate(&detail, 160));
        }
    }
    if let Some(checkpoint) = &summary.mcp_runtime_checkpoint {
        println!(
            "MCP runtime: {}",
            mcp_runtime_checkpoint_operator_summary(checkpoint)
        );
        if let Some(detail) = mcp_runtime_checkpoint_detail(checkpoint) {
            println!("            {}", truncate(&detail, 160));
        }
    }
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

async fn show_run_artifacts(
    store: &ManagedStore,
    run_ref: &str,
    limit: usize,
    include_lineage: bool,
    json: bool,
) -> anyhow::Result<()> {
    let run = load_run(store, run_ref).await?;
    let agent_label = load_agent_label(store, &run.agent_id).await?;
    let limit = limit.clamp(1, 1000);
    let (lineage_run_ids, artifacts) = if include_lineage {
        let (lineage, artifacts) = store
            .list_run_artifacts_with_replay_lineage(&run.id, limit, 64)
            .await?;
        (
            lineage.into_iter().map(|entry| entry.id).collect(),
            artifacts,
        )
    } else {
        (Vec::new(), store.list_run_artifacts(&run.id, limit).await?)
    };

    if json {
        print_json(&RunArtifactsJsonResponse {
            object: "list",
            run,
            agent_name: agent_label,
            data: artifacts,
            lineage_run_ids,
        })?;
        return Ok(());
    }

    println!("Run:    {}", run.id);
    println!("Status: {}", run.status.as_str());
    println!("Agent:  {}", agent_label);
    println!("Items:  {}", artifacts.len());
    if !lineage_run_ids.is_empty() {
        println!("Lineage: {}", lineage_run_ids.join(" -> "));
    }
    if artifacts.is_empty() {
        println!();
        println!("No persisted artifacts.");
        return Ok(());
    }

    println!();
    println!(
        "{:<6} {:<18} {:<20} {:<18} Preview",
        "ID", "Kind", "Label", "Tool"
    );
    println!("{}", "-".repeat(108));
    for artifact in artifacts {
        if include_lineage {
            println!("  run={}", artifact.run_id);
        }
        println!(
            "{:<6} {:<18} {:<20} {:<18} {}",
            artifact.id,
            artifact_kind_label(&artifact.kind),
            truncate(&artifact.label, 20),
            truncate(artifact.tool_name.as_deref().unwrap_or("-"), 18),
            truncate(&artifact_preview(&artifact.content), 80),
        );
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

fn artifact_kind_label(kind: &ManagedRunArtifactKind) -> &'static str {
    match kind {
        ManagedRunArtifactKind::AssistantOutput => "assistant_output",
        ManagedRunArtifactKind::ToolOutput => "tool_output",
    }
}

fn artifact_preview(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    trimmed.replace('\n', " ")
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn replay_provenance_operator_summary(summary: &ManagedRunReplayProvenanceSummary) -> String {
    let trigger = match summary.trigger {
        hermes_managed::ManagedRunReplayTrigger::ManualReplay => "manual",
        hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay => "auto",
    };
    let mut rendered = format!(
        "trigger={} depth={} source_status={}",
        trigger,
        summary.replay_depth,
        summary
            .source_status
            .as_ref()
            .map(|status| status.as_str())
            .unwrap_or("unknown")
    );
    if summary.resumed_existing_turn {
        rendered.push_str(" resumed_existing_turn=yes");
    }
    if let Some(boundary) = summary.source_boundary {
        rendered.push_str(&format!(
            " source_boundary={}",
            continuation_boundary_label(boundary)
        ));
    }
    if summary.reused_session_id {
        rendered.push_str(" reused_session_id=yes");
    }
    if let Some(lineage_id) = summary.takeover_lineage_id.as_deref() {
        rendered.push_str(&format!(" lineage={lineage_id}"));
    }
    if let Some(note) = &summary.note {
        rendered.push(' ');
        rendered.push_str(note);
    }
    rendered
}

fn replay_provenance_detail(summary: &ManagedRunReplayProvenanceSummary) -> Option<String> {
    let mut details = vec![
        format!("source={}", summary.source_run_id),
        format!("root={}", summary.root_run_id),
    ];
    if !summary.trigger_worker_id.is_empty() {
        details.push(format!("worker={}", summary.trigger_worker_id));
    }
    if let Some(cause) = summary.source_interruption_cause {
        let label = match cause {
            ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
            ManagedRunInterruptionCause::OwnershipNotEstablished => "ownership_not_established",
        };
        details.push(format!("source_interruption={label}"));
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn continuation_lineage_operator_summary(summary: &ManagedRunContinuationSummary) -> String {
    let trigger = match summary.trigger {
        hermes_managed::ManagedRunReplayTrigger::ManualReplay => "manual",
        hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay => "auto",
    };
    let mut rendered = format!(
        "source={} depth={} trigger={} source_status={}",
        summary.source_run_id,
        summary.replay_depth,
        trigger,
        summary
            .source_status
            .as_ref()
            .map(|status| status.as_str())
            .unwrap_or("unknown")
    );
    if summary.resumed_existing_turn {
        rendered.push_str(" resumed_existing_turn=yes");
    }
    if let Some(boundary) = summary.source_boundary {
        rendered.push_str(&format!(
            " source_boundary={}",
            continuation_boundary_label(boundary)
        ));
    }
    if summary.reused_session_id {
        rendered.push_str(" reused_session_id=yes");
    }
    if let Some(worker_id) = summary.evaluated_by_worker_id.as_deref() {
        rendered.push_str(&format!(" evaluated_by={worker_id}"));
    }
    if let Some(worker_id) = summary.takeover_worker_id.as_deref() {
        rendered.push_str(&format!(" takeover_worker={worker_id}"));
    }
    if let Some(lineage_id) = summary.takeover_lineage_id.as_deref() {
        rendered.push_str(&format!(" lineage={lineage_id}"));
    }
    rendered
}

fn continuation_lineage_detail(summary: &ManagedRunContinuationSummary) -> Option<String> {
    let mut details = vec![format!("root={}", summary.root_run_id)];
    if let Some(cause) = summary.source_interruption_cause {
        let label = match cause {
            ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
            ManagedRunInterruptionCause::OwnershipNotEstablished => "ownership_not_established",
        };
        details.push(format!("source_interruption={label}"));
    }
    if let Some(note) = &summary.note {
        details.push(note.clone());
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn replay_child_operator_summary(summary: &ManagedRunReplayChildSummary) -> String {
    let trigger = summary
        .trigger
        .map(|trigger| match trigger {
            hermes_managed::ManagedRunReplayTrigger::ManualReplay => "manual",
            hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay => "auto",
        })
        .unwrap_or("unknown");
    let mut rendered = format!(
        "latest={} status={} count={} trigger={}",
        summary.latest_run_id,
        summary.latest_status.as_str(),
        summary.replay_child_count,
        trigger
    );
    if summary.resumed_existing_turn {
        rendered.push_str(" resumed_existing_turn=yes");
    }
    if let Some(boundary) = summary.source_boundary {
        rendered.push_str(&format!(
            " source_boundary={}",
            continuation_boundary_label(boundary)
        ));
    }
    if summary.reused_session_id {
        rendered.push_str(" reused_session_id=yes");
    }
    if let Some(note) = &summary.note {
        rendered.push(' ');
        rendered.push_str(note);
    }
    rendered
}

fn replay_child_detail(summary: &ManagedRunReplayChildSummary) -> Option<String> {
    let mut details = Vec::new();
    if summary.replay_child_count > 1 {
        details.push("multiple replay children exist".to_string());
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn interruption_operator_summary(summary: &ManagedRunInterruptionSummary) -> String {
    match (summary.cause, summary.owner_worker_id.as_deref()) {
        (ManagedRunInterruptionCause::LeaseExpired, Some(worker_id)) => {
            format!("cause=lease_expired worker={worker_id}")
        }
        (ManagedRunInterruptionCause::LeaseExpired, None) => "cause=lease_expired".to_string(),
        (ManagedRunInterruptionCause::OwnershipNotEstablished, _) => {
            "cause=ownership_not_established".to_string()
        }
    }
}

fn interruption_detail(summary: &ManagedRunInterruptionSummary) -> Option<String> {
    let mut details = Vec::new();
    details.push(summary.message());
    if let Some(claimed_at) = summary.owner_claimed_at {
        details.push(format!("claimed_at={}", format_ts(claimed_at)));
    }
    if let Some(last_heartbeat_at) = summary.owner_last_heartbeat_at {
        details.push(format!(
            "last_heartbeat_at={}",
            format_ts(last_heartbeat_at)
        ));
    }
    if let Some(lease_expires_at) = summary.owner_lease_expires_at {
        details.push(format!("lease_expires_at={}", format_ts(lease_expires_at)));
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn ownership_state_label(state: ManagedRunOwnerState) -> &'static str {
    match state {
        ManagedRunOwnerState::Active => "active",
        ManagedRunOwnerState::Expired => "expired",
        ManagedRunOwnerState::Incomplete => "incomplete",
    }
}

fn ownership_operator_summary(summary: &ManagedRunOwnerSnapshot) -> String {
    let mut rendered = format!(
        "worker={} state={}",
        summary.worker_id,
        ownership_state_label(summary.state)
    );
    if let Some(lease_expires_at) = summary.lease_expires_at {
        rendered.push_str(&format!(
            " lease_expires_at={}",
            format_ts(lease_expires_at)
        ));
    }
    rendered
}

fn ownership_detail(summary: &ManagedRunOwnerSnapshot) -> Option<String> {
    let mut details = Vec::new();
    if let Some(claimed_at) = summary.claimed_at {
        details.push(format!("claimed_at={}", format_ts(claimed_at)));
    }
    if let Some(last_heartbeat_at) = summary.last_heartbeat_at {
        details.push(format!(
            "last_heartbeat_at={}",
            format_ts(last_heartbeat_at)
        ));
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn ownership_claim_operator_summary(
    summary: &hermes_managed::ManagedRunOwnershipClaimSummary,
) -> String {
    let mut rendered = format!("worker={}", summary.worker_id);
    if let Some(lease_expires_at) = summary.lease_expires_at {
        rendered.push_str(&format!(
            " lease_expires_at={}",
            format_ts(lease_expires_at)
        ));
    }
    rendered
}

fn ownership_claim_detail(
    summary: &hermes_managed::ManagedRunOwnershipClaimSummary,
) -> Option<String> {
    let mut details = Vec::new();
    if let Some(claimed_at) = summary.claimed_at {
        details.push(format!("claimed_at={}", format_ts(claimed_at)));
    }
    if let Some(lineage_id) = summary.takeover_lineage_id.as_deref() {
        details.push(format!("lineage={lineage_id}"));
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn takeover_assessment_operator_summary(summary: &ManagedRunTakeoverAssessmentSummary) -> String {
    let mut risks = Vec::new();
    if summary.provider_call_in_flight {
        risks.push("provider_call");
    }
    if summary.process_handoff_risk {
        risks.push("process_handoff");
    }
    if summary.browser_handoff_risk {
        risks.push("browser_handoff");
    }
    if summary.browser_session_state {
        risks.push("browser_session");
    }
    if summary.mcp_handoff_risk {
        risks.push("mcp_handoff");
    }
    if summary.mcp_runtime_state {
        risks.push("mcp_runtime");
    }

    let mut rendered = format!(
        "depth={}/{} blocking_risks={}",
        summary.replay_depth,
        summary.max_auto_replays,
        if risks.is_empty() {
            "none".to_string()
        } else {
            risks.join(",")
        }
    );
    if summary.provider_call_in_flight {
        rendered.push_str(" provider_call_in_flight=yes");
    }
    if let Some(boundary) = summary.source_boundary {
        rendered.push_str(&format!(
            " source_boundary={}",
            continuation_boundary_label(boundary)
        ));
    }
    if let Some(worker_id) = summary.evaluated_by_worker_id.as_deref() {
        rendered.push_str(&format!(" evaluated_by={worker_id}"));
    }
    if let Some(lineage_id) = summary.takeover_lineage_id.as_deref() {
        rendered.push_str(&format!(" lineage={lineage_id}"));
    }
    rendered
}

fn takeover_assessment_detail(summary: &ManagedRunTakeoverAssessmentSummary) -> Option<String> {
    let mut details = Vec::new();
    if let Some(cause) = summary.interruption_cause {
        let label = match cause {
            ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
            ManagedRunInterruptionCause::OwnershipNotEstablished => "ownership_not_established",
        };
        details.push(format!("interruption_cause={label}"));
    }
    if let Some(note) = &summary.note {
        details.push(note.clone());
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn takeover_operator_summary(summary: &ManagedRunTakeoverSummary) -> String {
    let state = match summary.takeover_state {
        hermes_managed::ManagedRunTakeoverState::Active => "active",
        hermes_managed::ManagedRunTakeoverState::Completed => "completed",
        hermes_managed::ManagedRunTakeoverState::Failed => "failed",
        hermes_managed::ManagedRunTakeoverState::Cancelled => "cancelled",
        hermes_managed::ManagedRunTakeoverState::TimedOut => "timed_out",
        hermes_managed::ManagedRunTakeoverState::Interrupted => "interrupted",
    };
    let trigger = summary
        .trigger
        .map(|trigger| match trigger {
            hermes_managed::ManagedRunReplayTrigger::ManualReplay => "manual",
            hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay => "auto",
        })
        .unwrap_or("unknown");
    let mut rendered = format!(
        "run={} status={} state={} trigger={}",
        summary.replay_run_id,
        summary.replay_run_status.as_str(),
        state,
        trigger
    );
    if summary.lineage_depth > 1 {
        rendered.push_str(&format!(" lineage_depth={}", summary.lineage_depth));
    }
    if let Some(boundary) = summary.source_boundary {
        rendered.push_str(&format!(
            " source_boundary={}",
            continuation_boundary_label(boundary)
        ));
    }
    if let Some(worker_id) = summary.evaluated_by_worker_id.as_deref() {
        rendered.push_str(&format!(" evaluated_by={worker_id}"));
    }
    if let Some(worker_id) = summary.takeover_worker_id.as_deref() {
        rendered.push_str(&format!(" takeover_worker={worker_id}"));
    }
    if let Some(owner) = summary.current_owner.as_ref() {
        rendered.push_str(&format!(
            " current_owner={}({})",
            owner.worker_id,
            ownership_state_label(owner.state)
        ));
    }
    if let Some(lineage_id) = summary.takeover_lineage_id.as_deref() {
        rendered.push_str(&format!(" lineage={lineage_id}"));
    }
    rendered
}

fn takeover_detail(summary: &ManagedRunTakeoverSummary) -> Option<String> {
    let mut details = Vec::new();
    if summary.lineage_depth > 1 {
        details.push(format!(
            "latest continuation leaf is replay descendant depth {}",
            summary.lineage_depth
        ));
    }
    if summary.replay_child_count > 1 {
        details.push(format!(
            "{} replay children exist in this lineage",
            summary.replay_child_count
        ));
    }
    if summary.resumed_existing_turn {
        details.push("replay child resumed the existing interrupted turn".to_string());
    }
    if summary.reused_session_id {
        details.push("replay child reused the persisted session id".to_string());
    }
    if let Some(owner) = summary.current_owner.as_ref() {
        if let Some(owner_detail) = ownership_detail(owner) {
            details.push(format!("current_owner {owner_detail}"));
        }
    }
    if let Some(claim) = summary.follow_target_ownership_claim.as_ref() {
        details.push(format!(
            "follow_target_ownership_claim {}",
            ownership_claim_operator_summary(claim)
        ));
        if let Some(detail) = ownership_claim_detail(claim) {
            details.push(format!("follow_target_ownership_claim_detail {detail}"));
        }
    }
    if let Some(decision) = summary.follow_target_recovery_decision.as_ref() {
        details.push(format!(
            "follow_target {}",
            recovery_decision_operator_summary(decision)
        ));
        if let Some(detail) = recovery_decision_detail(decision) {
            details.push(format!("follow_target_detail {detail}"));
        }
    }
    if let Some(checkpoint) = summary.follow_target_continuation_checkpoint.as_ref() {
        details.push(format!(
            "follow_target_checkpoint {}",
            continuation_operator_summary(checkpoint)
        ));
    }
    if let Some(fence) = summary.follow_target_provider_call_fence.as_ref() {
        details.push(format!(
            "follow_target_provider_fence {}",
            provider_fence_operator_summary(fence)
        ));
    }
    if let Some(handoff) = summary.follow_target_process_handoff.as_ref() {
        details.push(format!(
            "follow_target_process_handoff {}",
            process_handoff_operator_summary(handoff)
        ));
        if let Some(detail) = process_handoff_detail(handoff) {
            details.push(format!("follow_target_process_handoff_detail {detail}"));
        }
    }
    if let Some(handoff) = summary.follow_target_browser_handoff.as_ref() {
        details.push(format!(
            "follow_target_browser_handoff {}",
            browser_handoff_operator_summary(handoff)
        ));
        if let Some(detail) = browser_handoff_detail(handoff) {
            details.push(format!("follow_target_browser_handoff_detail {detail}"));
        }
    }
    if let Some(checkpoint) = summary.follow_target_browser_session_checkpoint.as_ref() {
        details.push(format!(
            "follow_target_browser_session {}",
            browser_session_checkpoint_operator_summary(checkpoint)
        ));
        if let Some(detail) = browser_session_checkpoint_detail(checkpoint) {
            details.push(format!("follow_target_browser_session_detail {detail}"));
        }
    }
    if let Some(checkpoint) = summary.follow_target_mcp_runtime_checkpoint.as_ref() {
        details.push(format!(
            "follow_target_mcp_runtime {}",
            mcp_runtime_checkpoint_operator_summary(checkpoint)
        ));
        if let Some(detail) = mcp_runtime_checkpoint_detail(checkpoint) {
            details.push(format!("follow_target_mcp_runtime_detail {detail}"));
        }
    }
    if let Some(handoff) = summary.follow_target_mcp_handoff.as_ref() {
        details.push(format!(
            "follow_target_mcp_handoff {}",
            mcp_handoff_operator_summary(handoff)
        ));
        if let Some(detail) = mcp_handoff_detail(handoff) {
            details.push(format!("follow_target_mcp_handoff_detail {detail}"));
        }
    }
    if let Some(artifact) = summary.follow_target_artifact_continuity.as_ref() {
        details.push(format!(
            "follow_target_artifact {}",
            artifact_continuity_operator_summary(artifact)
        ));
        if let Some(detail) = artifact_continuity_detail(artifact) {
            details.push(format!("follow_target_artifact_detail {detail}"));
        }
    }
    if let Some(assessment) = summary.follow_target_takeover_assessment.as_ref() {
        details.push(format!(
            "follow_target_assessment {}",
            takeover_assessment_operator_summary(assessment)
        ));
        if let Some(detail) = takeover_assessment_detail(assessment) {
            details.push(format!("follow_target_assessment_detail {detail}"));
        }
    }
    if let Some(release) = summary.follow_target_ownership_release.as_ref() {
        details.push(format!(
            "follow_target_owner_release worker={} reason={}",
            release.worker_id,
            ownership_release_reason_label(release.reason)
        ));
    }
    if let Some(note) = &summary.note {
        details.push(note.clone());
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn recovery_decision_reason_label(reason: ManagedRunRecoveryDecisionReason) -> &'static str {
    match reason {
        ManagedRunRecoveryDecisionReason::RunStillActive => "run_still_active",
        ManagedRunRecoveryDecisionReason::ReplayChildActive => "replay_child_active",
        ManagedRunRecoveryDecisionReason::DepthLimitReached => "depth_limit",
        ManagedRunRecoveryDecisionReason::ProcessHandoffRisk => "process_handoff_risk",
        ManagedRunRecoveryDecisionReason::BrowserHandoffRisk => "browser_handoff_risk",
        ManagedRunRecoveryDecisionReason::BrowserSessionState => "browser_session_state",
        ManagedRunRecoveryDecisionReason::McpHandoffRisk => "mcp_handoff_risk",
        ManagedRunRecoveryDecisionReason::McpRuntimeState => "mcp_runtime_state",
        ManagedRunRecoveryDecisionReason::ReplaySpawnFailed => "replay_spawn_failed",
    }
}

fn ownership_release_reason_label(
    reason: hermes_managed::ManagedRunOwnershipReleaseReason,
) -> &'static str {
    match reason {
        hermes_managed::ManagedRunOwnershipReleaseReason::Completed => "completed",
        hermes_managed::ManagedRunOwnershipReleaseReason::Failed => "failed",
        hermes_managed::ManagedRunOwnershipReleaseReason::Cancelled => "cancelled",
        hermes_managed::ManagedRunOwnershipReleaseReason::TimedOut => "timed_out",
        hermes_managed::ManagedRunOwnershipReleaseReason::Interrupted => "interrupted",
    }
}

fn recovery_decision_operator_summary(summary: &ManagedRunRecoveryDecisionSummary) -> String {
    let decision = match summary.decision {
        ManagedRunRecoveryDecisionKind::ReplayStarted => "replay_started",
        ManagedRunRecoveryDecisionKind::FollowReplay => "follow_replay",
        ManagedRunRecoveryDecisionKind::ManualReview => "manual_review",
        ManagedRunRecoveryDecisionKind::Blocked => "blocked",
        ManagedRunRecoveryDecisionKind::Failed => "failed",
    };
    let mut rendered = format!("decision={decision}");
    if let Some(reason) = summary.reason {
        rendered.push_str(&format!(
            " reason={}",
            recovery_decision_reason_label(reason)
        ));
    }
    if let Some(boundary) = summary.source_boundary {
        rendered.push_str(&format!(
            " source_boundary={}",
            continuation_boundary_label(boundary)
        ));
    }
    if let Some(run_id) = summary.replay_run_id.as_deref() {
        rendered.push_str(&format!(" replay_run_id={run_id}"));
    }
    if let (Some(run_id), Some(depth)) = (
        summary.active_follow_target_run_id.as_deref(),
        summary.active_follow_target_lineage_depth,
    ) {
        if depth > 1 || summary.replay_run_id.as_deref() != Some(run_id) {
            rendered.push_str(&format!(" follow_target={run_id}"));
        }
        if depth > 1 {
            rendered.push_str(&format!(" follow_depth={depth}"));
        }
    }
    if let Some(worker_id) = summary.evaluated_by_worker_id.as_deref() {
        rendered.push_str(&format!(" evaluated_by={worker_id}"));
    }
    if let Some(worker_id) = summary.takeover_worker_id.as_deref() {
        rendered.push_str(&format!(" takeover_worker={worker_id}"));
    } else if let Some(worker_id) = summary.worker_id.as_deref() {
        rendered.push_str(&format!(" worker={worker_id}"));
    }
    if let Some(lineage_id) = summary.takeover_lineage_id.as_deref() {
        rendered.push_str(&format!(" lineage={lineage_id}"));
    }
    rendered
}

fn recovery_decision_detail(summary: &ManagedRunRecoveryDecisionSummary) -> Option<String> {
    let mut details = Vec::new();
    if let (Some(run_id), Some(depth)) = (
        summary.active_follow_target_run_id.as_deref(),
        summary.active_follow_target_lineage_depth,
    ) {
        if depth > 1 {
            details.push(format!(
                "active follow target is replay descendant {} at depth {}",
                run_id, depth
            ));
        }
    }
    if let Some(note) = &summary.note {
        details.push(note.clone());
    }
    (!details.is_empty()).then_some(details.join("; "))
}

fn artifact_continuity_operator_summary(summary: &ManagedRunArtifactContinuitySummary) -> String {
    let mut rendered = format!(
        "kind={} label={} source={} depth={}",
        artifact_kind_label(&summary.latest_kind),
        summary.latest_label,
        if summary.latest_run_is_current {
            "current_run"
        } else {
            "replay_lineage"
        },
        summary.lineage_depth
    );
    if let Some(tool_name) = &summary.latest_tool_name {
        rendered.push_str(&format!(" tool={tool_name}"));
    }
    if let Some(note) = &summary.note {
        rendered.push(' ');
        rendered.push_str(note);
    }
    rendered
}

fn artifact_continuity_detail(summary: &ManagedRunArtifactContinuitySummary) -> Option<String> {
    let mut details = vec![format!("run={}", summary.latest_run_id)];
    if let Some(tool_call_id) = &summary.latest_tool_call_id {
        details.push(format!("call={tool_call_id}"));
    }
    if let Some(preview) = &summary.latest_content_preview {
        details.push(format!("preview={preview}"));
    }
    (!details.is_empty()).then(|| details.join(" "))
}

fn continuation_boundary_label(boundary: ManagedRunContinuationBoundaryKind) -> &'static str {
    match boundary {
        ManagedRunContinuationBoundaryKind::UserCheckpointed => "user_checkpointed",
        ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => {
            "assistant_response_checkpointed"
        }
        ManagedRunContinuationBoundaryKind::PendingToolCalls => "pending_tool_calls",
        ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => "tool_results_checkpointed",
    }
}

fn continuation_operator_summary(checkpoint: &ManagedRunContinuationCheckpointSummary) -> String {
    let boundary = continuation_boundary_label(checkpoint.kind);
    let action = match checkpoint.safe_action {
        hermes_managed::ManagedRunContinuationAction::CallProvider => "call_provider",
        hermes_managed::ManagedRunContinuationAction::ExecutePendingTools => {
            "execute_pending_tools"
        }
        hermes_managed::ManagedRunContinuationAction::CompleteTurn => "complete_turn",
    };
    if checkpoint.pending_tool_calls > 0 {
        format!(
            "boundary={boundary} action={action} history_len={} pending_tool_calls={}",
            checkpoint.history_len, checkpoint.pending_tool_calls
        )
    } else {
        format!(
            "boundary={boundary} action={action} history_len={}",
            checkpoint.history_len
        )
    }
}

fn provider_fence_operator_summary(fence: &ManagedRunProviderCallFenceSummary) -> String {
    let mut summary = format!(
        "safe_resume_from=({}) request_history_len={} tool_count={}",
        continuation_operator_summary(&fence.safe_resume_from),
        fence.request_history_len,
        fence.tool_count
    );
    if let Some(note) = &fence.note {
        summary.push(' ');
        summary.push_str(note);
    }
    summary
}

fn process_handoff_operator_summary(handoff: &ManagedRunProcessHandoffSummary) -> String {
    let state = match handoff.state {
        hermes_managed::ManagedRunProcessHandoffState::Running => "running",
        hermes_managed::ManagedRunProcessHandoffState::Completed => "completed",
        hermes_managed::ManagedRunProcessHandoffState::Failed => "failed",
        hermes_managed::ManagedRunProcessHandoffState::TimedOut => "timed_out",
    };
    let disposition = match handoff.replay_disposition {
        hermes_managed::ManagedRunProcessReplayDisposition::SafeToReplay => "safe_to_replay",
        hermes_managed::ManagedRunProcessReplayDisposition::UnsafeSideEffectWindow => {
            "unsafe_side_effect_window"
        }
        hermes_managed::ManagedRunProcessReplayDisposition::CompletedButNotRecorded => {
            "completed_but_not_recorded"
        }
    };
    let mut summary = format!(
        "tool={} state={} disposition={}",
        handoff.tool_name, state, disposition
    );
    if let Some(exit_code) = handoff.exit_code {
        summary.push_str(&format!(" exit_code={exit_code}"));
    }
    if let Some(timeout_secs) = handoff.timeout_secs {
        summary.push_str(&format!(" timeout_secs={timeout_secs}"));
    }
    if let Some(note) = &handoff.note {
        summary.push(' ');
        summary.push_str(note);
    }
    summary
}

fn process_handoff_detail(handoff: &ManagedRunProcessHandoffSummary) -> Option<String> {
    match (
        handoff
            .stdout_preview
            .as_deref()
            .filter(|value| !value.is_empty()),
        handoff
            .stderr_preview
            .as_deref()
            .filter(|value| !value.is_empty()),
    ) {
        (Some(stdout), Some(stderr)) => Some(format!("stdout={stdout} stderr={stderr}")),
        (Some(stdout), None) => Some(format!("stdout={stdout}")),
        (None, Some(stderr)) => Some(format!("stderr={stderr}")),
        (None, None) => None,
    }
}

fn browser_handoff_operator_summary(handoff: &ManagedRunBrowserHandoffSummary) -> String {
    let state = match handoff.state {
        hermes_managed::ManagedRunBrowserHandoffState::Started => "started",
        hermes_managed::ManagedRunBrowserHandoffState::Completed => "completed",
        hermes_managed::ManagedRunBrowserHandoffState::Failed => "failed",
    };
    let disposition = match handoff.replay_disposition {
        hermes_managed::ManagedRunBrowserReplayDisposition::SafeToReplay => "safe_to_replay",
        hermes_managed::ManagedRunBrowserReplayDisposition::UnsafeSideEffectWindow => {
            "unsafe_side_effect_window"
        }
        hermes_managed::ManagedRunBrowserReplayDisposition::CompletedButNotRecorded => {
            "completed_but_not_recorded"
        }
    };
    let mut summary = format!(
        "action={} state={} disposition={}",
        handoff.action, state, disposition
    );
    if let Some(target) = &handoff.target {
        summary.push_str(&format!(" target={target}"));
    }
    if handoff.wait_for_navigation {
        summary.push_str(" wait_for_navigation=true");
    }
    if let Some(note) = &handoff.note {
        summary.push(' ');
        summary.push_str(note);
    }
    summary
}

fn browser_handoff_detail(handoff: &ManagedRunBrowserHandoffSummary) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(page_url) = &handoff.page_url {
        parts.push(format!("url={page_url}"));
    }
    if let Some(page_title) = &handoff.page_title {
        parts.push(format!("title={page_title}"));
    }
    if let Some(output_preview) = handoff
        .output_preview
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("preview={output_preview}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn browser_session_checkpoint_operator_summary(
    checkpoint: &ManagedRunBrowserSessionCheckpointSummary,
) -> String {
    let mut summary = format!(
        "action={} session_open={}",
        checkpoint.action,
        yes_no(checkpoint.session_open)
    );
    if let Some(target) = &checkpoint.target {
        summary.push_str(&format!(" target={target}"));
    }
    if let Some(note) = &checkpoint.note {
        summary.push(' ');
        summary.push_str(note);
    }
    summary
}

fn browser_session_checkpoint_detail(
    checkpoint: &ManagedRunBrowserSessionCheckpointSummary,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(page_url) = &checkpoint.page_url {
        parts.push(format!("url={page_url}"));
    }
    if let Some(page_title) = &checkpoint.page_title {
        parts.push(format!("title={page_title}"));
    }
    if let Some(output_preview) = checkpoint
        .output_preview
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("preview={output_preview}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn mcp_handoff_operator_summary(handoff: &ManagedRunMcpHandoffSummary) -> String {
    let state = match handoff.state {
        hermes_managed::ManagedRunMcpHandoffState::Started => "started",
        hermes_managed::ManagedRunMcpHandoffState::Completed => "completed",
        hermes_managed::ManagedRunMcpHandoffState::Failed => "failed",
    };
    let disposition = match handoff.replay_disposition {
        hermes_managed::ManagedRunMcpReplayDisposition::SafeToReplay => "safe_to_replay",
        hermes_managed::ManagedRunMcpReplayDisposition::UnsafeSideEffectWindow => {
            "unsafe_side_effect_window"
        }
        hermes_managed::ManagedRunMcpReplayDisposition::CompletedButNotRecorded => {
            "completed_but_not_recorded"
        }
    };
    let mut summary = format!(
        "tool={} state={} disposition={} read_only={} live_runtime={}",
        handoff.tool_name,
        state,
        disposition,
        yes_no(handoff.read_only),
        yes_no(handoff.requires_live_runtime)
    );
    if let Some(server) = &handoff.server {
        summary.push_str(&format!(" server={server}"));
    }
    if let Some(transport) = &handoff.transport {
        summary.push_str(&format!(" transport={transport}"));
    }
    if let Some(target) = &handoff.target {
        summary.push_str(&format!(" target={target}"));
    }
    if let Some(note) = &handoff.note {
        summary.push(' ');
        summary.push_str(note);
    }
    summary
}

fn mcp_handoff_detail(handoff: &ManagedRunMcpHandoffSummary) -> Option<String> {
    handoff
        .output_preview
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("preview={value}"))
}

fn mcp_runtime_checkpoint_operator_summary(
    checkpoint: &ManagedRunMcpRuntimeCheckpointSummary,
) -> String {
    let mut summary = format!(
        "tool={} live_runtime={} subscriptions={}",
        checkpoint.tool_name,
        yes_no(checkpoint.live_runtime_required),
        checkpoint.active_subscription_count
    );
    if !checkpoint.active_servers.is_empty() {
        summary.push_str(&format!(" servers={}", checkpoint.active_servers.join(",")));
    }
    if let Some(server) = &checkpoint.server {
        summary.push_str(&format!(" last_server={server}"));
    }
    if let Some(transport) = &checkpoint.transport {
        summary.push_str(&format!(" transport={transport}"));
    }
    if let Some(target) = &checkpoint.target {
        summary.push_str(&format!(" target={target}"));
    }
    if let Some(note) = &checkpoint.note {
        summary.push(' ');
        summary.push_str(note);
    }
    summary
}

fn mcp_runtime_checkpoint_detail(
    checkpoint: &ManagedRunMcpRuntimeCheckpointSummary,
) -> Option<String> {
    (!checkpoint.active_servers.is_empty())
        .then(|| format!("active_servers={}", checkpoint.active_servers.join(", ")))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use hermes_config::config::SignetConfig;
    use signet_core::{Action, audit, generate_and_save, load_signing_key, sign, sign_compound};
    use tempfile::TempDir;

    use super::*;
    use hermes_managed::{
        ManagedAgent, ManagedAgentVersion, ManagedRunArtifactDraft, ManagedRunArtifactKind,
        ManagedRunEventDraft, ManagedRunEventKind, ManagedRunStatus,
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
                session_id: None,
                prompt: String::new(),
                replay_of_run_id: None,
                started_at: Utc::now(),
                updated_at: Utc::now(),
                ended_at: None,
                cancel_requested_at: None,
                last_error: None,
            },
            agent_name: "observer".to_string(),
            summary: ManagedRunDerivedSummary::default(),
            mcp_admission_rejection: None,
            cleanup_failure: None,
            ownership: None,
            takeover_assessment: None,
            takeover: None,
            recovery_decision: None,
            recovery_hint: None,
        };

        let rendered = serde_json::to_string_pretty(&payload).unwrap();
        assert!(rendered.contains("\"agent_name\": \"observer\""));
        assert!(rendered.contains("\"status\": \"completed\""));
    }

    #[tokio::test]
    async fn load_run_summary_reads_structured_metadata() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("observer");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Failed;
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunMcpAdmissionRejected,
                    message: Some("Managed MCP admission rejected: disabled_by_operator_policy".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "code": "disabled_by_operator_policy",
                        "error": "managed MCP tools are disabled by operator policy: mcp_resource_read",
                        "requested_tools": ["mcp_resource_read"],
                        "requested_read_only_tools": ["mcp_resource_read"],
                        "requested_side_effect_tools": [],
                        "requested_dynamic_tools": [],
                        "allowed_servers": [],
                        "allowed_transports": [],
                        "allow_side_effects": false,
                        "allowed_stdio_servers": [],
                        "allowed_stdio_env_keys": [],
                        "stdio_server_summaries": []
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunInterrupted,
                    message: Some(
                        "managed run interrupted after worker lease expired during execution"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "cause": "lease_expired",
                        "owner_worker_id": "gw_expired",
                        "owner_claimed_at": Utc::now() - chrono::Duration::seconds(30),
                        "owner_last_heartbeat_at": Utc::now() - chrono::Duration::seconds(10),
                        "owner_lease_expires_at": Utc::now() - chrono::Duration::seconds(5),
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunContinuationCheckpoint,
                    message: Some("managed run checkpointed after user input".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "kind": "user_checkpointed",
                        "safe_action": "call_provider",
                        "history_len": 1,
                        "pending_tool_calls": 0,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunProviderCallStarted,
                    message: Some(
                        "managed run provider call started from user checkpointed boundary"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "request_history_len": 2,
                        "tool_count": 0,
                        "safe_resume_from": {
                            "kind": "user_checkpointed",
                            "safe_action": "call_provider",
                            "history_len": 1,
                            "pending_tool_calls": 0,
                        },
                        "note": "provider call dispatched before a newer durable response checkpoint",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCleanupFailed,
                    message: Some("Managed run cleanup failed for 1 resource(s)".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "phase": "terminal_cleanup",
                        "attempted": 2,
                        "cleaned": 1,
                        "failures": ["failed to clean durable resource"],
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "Checkpointed answer".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        let summary = load_run_summary(&store, &run.id).await.unwrap();
        let rejection = summary
            .mcp_admission_rejection
            .expect("managed MCP rejection should be present");

        assert_eq!(rejection.code, "disabled_by_operator_policy");
        assert_eq!(
            rejection.requested_tools,
            vec!["mcp_resource_read".to_string()]
        );
        assert_eq!(
            summary
                .cleanup_failure
                .as_ref()
                .map(|cleanup| cleanup.phase.as_str()),
            Some("terminal_cleanup")
        );
        assert_eq!(
            summary
                .interruption
                .as_ref()
                .map(|interruption| interruption.cause),
            Some(ManagedRunInterruptionCause::LeaseExpired)
        );
        assert_eq!(
            summary
                .interruption
                .as_ref()
                .and_then(|interruption| interruption.owner_worker_id.as_deref()),
            Some("gw_expired")
        );
        assert_eq!(
            summary
                .continuation_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.kind),
            Some(ManagedRunContinuationBoundaryKind::UserCheckpointed)
        );
        assert_eq!(
            summary
                .provider_call_fence
                .as_ref()
                .map(|fence| fence.safe_resume_from.kind),
            Some(ManagedRunContinuationBoundaryKind::UserCheckpointed)
        );
        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .map(|artifact| artifact.latest_kind.clone()),
            Some(ManagedRunArtifactKind::AssistantOutput)
        );
        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .and_then(|artifact| artifact.latest_content_preview.as_deref()),
            Some("Checkpointed answer")
        );
    }

    #[tokio::test]
    async fn load_run_summary_reads_replay_provenance() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("replay-observer");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Completed;
        run.replay_of_run_id = Some("run_source".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some(
                        "managed run replayed from run_source for replay-observer@1".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": "run_source",
                        "replay_root_run_id": "run_root",
                        "replay_depth": 2,
                        "replay_trigger": "manual_replay",
                        "replay_trigger_worker_id": "worker_xyz",
                        "replay_source_status": "interrupted",
                        "replay_source_interruption_cause": "lease_expired",
                        "reused_session_id": false,
                        "resumed_existing_turn": false,
                        "replay_source_boundary": "assistant_response_checkpointed",
                        "replay_note": "manual replay created a new managed run from persisted source context"
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_run_summary(&store, &run.id).await.unwrap();
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .map(|value| value.replay_depth),
            Some(2)
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .map(|value| value.trigger),
            Some(hermes_managed::ManagedRunReplayTrigger::ManualReplay)
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .and_then(|value| value.source_boundary),
            Some(ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed)
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .and_then(|value| value.source_interruption_cause),
            Some(ManagedRunInterruptionCause::LeaseExpired)
        );
        assert_eq!(
            summary
                .continuation
                .as_ref()
                .and_then(|value| value.takeover_worker_id.as_deref()),
            Some("worker_xyz")
        );
    }

    #[tokio::test]
    async fn load_run_summary_reads_replay_child() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("replacement-observer");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let mut source = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        source.status = ManagedRunStatus::Interrupted;
        source.prompt = "retry me".to_string();
        store.create_run(&source).await.unwrap();
        let source_run_id = source.id.clone();

        let mut child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        child.status = ManagedRunStatus::Running;
        child.prompt = source.prompt.clone();
        child.replay_of_run_id = Some(source_run_id.clone());
        store.create_run(&child).await.unwrap();
        let claimed_at = Utc::now();
        store
            .claim_run_ownership(
                &child.id,
                "worker_cli_takeover",
                "claim_cli_takeover",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipClaimed,
                    message: Some("managed run ownership claimed by worker_cli_takeover".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "worker_cli_takeover",
                        "claimed_at": claimed_at.to_rfc3339(),
                        "lease_expires_at": (claimed_at + chrono::Duration::seconds(30)).to_rfc3339(),
                        "takeover_lineage_id": child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source_run_id.as_str(),
                        "replay_root_run_id": source_run_id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "interrupted_auto_replay",
                        "reused_session_id": true,
                        "replay_source_boundary": "tool_results_checkpointed",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_run_summary(&store, &source.id).await.unwrap();
        assert_eq!(
            summary
                .replay_child
                .as_ref()
                .map(|value| value.latest_run_id.as_str()),
            Some(child.id.as_str())
        );
        assert_eq!(
            summary
                .replay_child
                .as_ref()
                .and_then(|value| value.takeover_lineage_id.as_deref()),
            Some(child.id.as_str())
        );
        assert_eq!(
            summary
                .replay_child
                .as_ref()
                .and_then(|value| value.source_boundary),
            Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .map(|value| value.replay_run_id.as_str()),
            Some(child.id.as_str())
        );
        assert!(
            takeover_operator_summary(summary.takeover.as_ref().expect("takeover missing"))
                .contains("state=active")
        );
        assert!(
            takeover_operator_summary(summary.takeover.as_ref().expect("takeover missing"))
                .contains("current_owner=worker_cli_takeover(active)")
        );
        assert!(
            takeover_operator_summary(summary.takeover.as_ref().expect("takeover missing"))
                .contains(&format!("lineage={}", child.id))
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("follow_replay")
        );
    }

    #[tokio::test]
    async fn load_run_summary_reads_recovery_decision() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("recovery-decision-observer");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "review me".to_string();
        store.create_run(&run).await.unwrap();

        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunRecoveryDecision,
                    message: Some(
                        "automatic replay is blocked because the configured replay depth limit was reached".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "decision": "blocked",
                        "reason": "depth_limit_reached",
                        "evaluated_by_worker_id": "worker_eval_789",
                        "worker_id": "worker_789",
                        "source_boundary": "tool_results_checkpointed",
                        "note": "automatic replay skipped because replay depth 1 reached configured limit 1; manual replay remains available",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_run_summary(&store, &run.id).await.unwrap();
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .map(|decision| decision.decision),
            Some(ManagedRunRecoveryDecisionKind::Blocked)
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.reason),
            Some(ManagedRunRecoveryDecisionReason::DepthLimitReached)
        );
        assert!(
            recovery_decision_operator_summary(
                summary
                    .recovery_decision
                    .as_ref()
                    .expect("decision missing")
            )
            .contains("reason=depth_limit")
        );
        assert!(
            recovery_decision_operator_summary(
                summary
                    .recovery_decision
                    .as_ref()
                    .expect("decision missing")
            )
            .contains("evaluated_by=worker_eval_789")
        );
    }

    #[test]
    fn recovery_decision_operator_summary_surfaces_leaf_follow_target() {
        let summary = ManagedRunRecoveryDecisionSummary {
            decision: ManagedRunRecoveryDecisionKind::FollowReplay,
            reason: Some(ManagedRunRecoveryDecisionReason::ReplayChildActive),
            replay_run_id: Some("run_child".to_string()),
            takeover_lineage_id: Some("lineage_takeover_1".to_string()),
            evaluated_by_worker_id: Some("worker_eval".to_string()),
            takeover_worker_id: Some("worker_leaf".to_string()),
            worker_id: Some("worker_leaf".to_string()),
            active_follow_target_run_id: Some("run_leaf".to_string()),
            active_follow_target_status: Some(ManagedRunStatus::Running),
            active_follow_target_lineage_depth: Some(2),
            source_boundary: Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed),
            note: Some("follow the active leaf continuation".to_string()),
        };

        let operator = recovery_decision_operator_summary(&summary);
        assert!(operator.contains("decision=follow_replay"));
        assert!(operator.contains("replay_run_id=run_child"));
        assert!(operator.contains("follow_target=run_leaf"));
        assert!(operator.contains("follow_depth=2"));
        assert!(operator.contains("lineage=lineage_takeover_1"));

        let detail = recovery_decision_detail(&summary).expect("detail missing");
        assert!(detail.contains("replay descendant run_leaf at depth 2"));
        assert!(detail.contains("follow the active leaf continuation"));
    }

    #[test]
    fn takeover_detail_surfaces_follow_target_recovery_decision() {
        let summary = ManagedRunTakeoverSummary {
            replay_run_id: "run_leaf".to_string(),
            replay_run_status: ManagedRunStatus::Interrupted,
            takeover_state: hermes_managed::ManagedRunTakeoverState::Interrupted,
            takeover_lineage_id: Some("lineage_takeover_leaf".to_string()),
            replay_child_count: 1,
            lineage_depth: 1,
            trigger: Some(hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay),
            reused_session_id: true,
            resumed_existing_turn: true,
            source_boundary: Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed),
            evaluated_by_worker_id: Some("worker_source_eval".to_string()),
            takeover_worker_id: Some("worker_leaf_owner".to_string()),
            current_owner: Some(ManagedRunOwnerSnapshot {
                worker_id: "worker_leaf_owner".to_string(),
                state: ManagedRunOwnerState::Active,
                claimed_at: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-04-23T12:08:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                last_heartbeat_at: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-04-23T12:08:15Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                lease_expires_at: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-04-23T12:09:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
            }),
            follow_target_ownership_claim: Some(
                hermes_managed::ManagedRunOwnershipClaimSummary {
                    worker_id: "worker_leaf_owner".to_string(),
                    claimed_at: Some(
                        chrono::DateTime::parse_from_rfc3339("2026-04-23T12:08:00Z")
                            .unwrap()
                            .with_timezone(&Utc),
                    ),
                    lease_expires_at: Some(
                        chrono::DateTime::parse_from_rfc3339("2026-04-23T12:09:00Z")
                            .unwrap()
                            .with_timezone(&Utc),
                    ),
                    takeover_lineage_id: Some("lineage_takeover_leaf".to_string()),
                },
            ),
            follow_target_recovery_decision: Some(ManagedRunRecoveryDecisionSummary {
                decision: ManagedRunRecoveryDecisionKind::ManualReview,
                reason: Some(ManagedRunRecoveryDecisionReason::ProcessHandoffRisk),
                replay_run_id: None,
                takeover_lineage_id: Some("lineage_takeover_leaf".to_string()),
                evaluated_by_worker_id: Some("worker_leaf_eval".to_string()),
                takeover_worker_id: None,
                worker_id: Some("worker_leaf_eval".to_string()),
                active_follow_target_run_id: None,
                active_follow_target_status: None,
                active_follow_target_lineage_depth: None,
                source_boundary: Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed),
                note: Some("leaf continuation now requires manual review".to_string()),
            }),
            follow_target_continuation_checkpoint: Some(ManagedRunContinuationCheckpointSummary {
                kind: ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed,
                safe_action: hermes_managed::ManagedRunContinuationAction::ExecutePendingTools,
                history_len: 7,
                pending_tool_calls: 1,
            }),
            follow_target_provider_call_fence: Some(ManagedRunProviderCallFenceSummary {
                request_history_len: 8,
                tool_count: 0,
                safe_resume_from: ManagedRunContinuationCheckpointSummary {
                    kind: ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed,
                    safe_action: hermes_managed::ManagedRunContinuationAction::ExecutePendingTools,
                    history_len: 7,
                    pending_tool_calls: 1,
                },
                note: Some(
                    "leaf provider call was dispatched after the last durable checkpoint"
                        .to_string(),
                ),
            }),
            follow_target_process_handoff: Some(ManagedRunProcessHandoffSummary {
                tool_name: "terminal".to_string(),
                tool_call_id: Some("call_terminal_1".to_string()),
                state: hermes_managed::ManagedRunProcessHandoffState::Running,
                replay_disposition:
                    hermes_managed::ManagedRunProcessReplayDisposition::UnsafeSideEffectWindow,
                process_group: Some(4242),
                timeout_secs: Some(30),
                exit_code: None,
                stdout_preview: Some("building...".to_string()),
                stderr_preview: None,
                note: Some(
                    "terminal process started and may still have side effects in flight"
                        .to_string(),
                ),
            }),
            follow_target_browser_handoff: Some(ManagedRunBrowserHandoffSummary {
                action: "click".to_string(),
                state: hermes_managed::ManagedRunBrowserHandoffState::Started,
                replay_disposition:
                    hermes_managed::ManagedRunBrowserReplayDisposition::UnsafeSideEffectWindow,
                target: Some("#submit".to_string()),
                wait_for_navigation: true,
                page_url: Some("https://example.com/form".to_string()),
                page_title: Some("Form".to_string()),
                output_preview: None,
                note: Some(
                    "browser action 'click' started and may still have page or external side effects in flight"
                        .to_string(),
                ),
            }),
            follow_target_browser_session_checkpoint: Some(
                ManagedRunBrowserSessionCheckpointSummary {
                    action: "navigate".to_string(),
                    session_open: true,
                    target: Some("https://example.com/dashboard".to_string()),
                    page_url: Some("https://example.com/dashboard".to_string()),
                    page_title: Some("Dashboard".to_string()),
                    output_preview: None,
                    note: Some("leaf browser session remained open after navigation".to_string()),
                },
            ),
            follow_target_mcp_runtime_checkpoint: Some(ManagedRunMcpRuntimeCheckpointSummary {
                tool_name: "mcp_resource_subscribe".to_string(),
                live_runtime_required: true,
                active_subscription_count: 1,
                active_servers: vec!["docs".to_string()],
                server: Some("docs".to_string()),
                transport: Some("http".to_string()),
                target: Some("uri:docs://guide".to_string()),
                note: Some(
                    "1 active MCP subscription(s) still depend on a live runtime/session after 'mcp_resource_subscribe'"
                        .to_string(),
                ),
            }),
            follow_target_mcp_handoff: Some(ManagedRunMcpHandoffSummary {
                tool_name: "mcp_resource_subscribe".to_string(),
                state: hermes_managed::ManagedRunMcpHandoffState::Started,
                replay_disposition:
                    hermes_managed::ManagedRunMcpReplayDisposition::UnsafeSideEffectWindow,
                read_only: false,
                requires_live_runtime: true,
                server: Some("docs".to_string()),
                transport: Some("http".to_string()),
                target: Some("uri:docs://guide".to_string()),
                output_preview: None,
                note: Some(
                    "MCP tool 'mcp_resource_subscribe' started and may still have runtime or external side effects in flight"
                        .to_string(),
                ),
            }),
            follow_target_artifact_continuity: Some(ManagedRunArtifactContinuitySummary {
                latest_kind: ManagedRunArtifactKind::AssistantOutput,
                latest_label: "assistant_output".to_string(),
                latest_run_id: "run_leaf".to_string(),
                latest_run_is_current: true,
                lineage_depth: 0,
                latest_tool_name: None,
                latest_tool_call_id: None,
                latest_content_preview: Some("Recovered answer preview".to_string()),
                note: Some(
                    "latest checkpointed assistant output is available from current run"
                        .to_string(),
                ),
            }),
            follow_target_takeover_assessment: Some(
                hermes_managed::ManagedRunTakeoverAssessmentSummary {
                    takeover_lineage_id: Some("lineage_takeover_leaf".to_string()),
                    evaluated_by_worker_id: Some("worker_leaf_eval".to_string()),
                    source_boundary: Some(
                        ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed,
                    ),
                    interruption_cause: Some(ManagedRunInterruptionCause::LeaseExpired),
                    provider_call_in_flight: false,
                    process_handoff_risk: true,
                    browser_handoff_risk: false,
                    browser_session_state: false,
                    mcp_handoff_risk: false,
                    mcp_runtime_state: false,
                    replay_depth: 1,
                    max_auto_replays: 3,
                    note: Some("leaf takeover assessment flagged process handoff risk".to_string()),
                },
            ),
            follow_target_ownership_release: Some(
                hermes_managed::ManagedRunOwnershipReleaseSummary {
                    worker_id: "worker_leaf_owner".to_string(),
                    reason: hermes_managed::ManagedRunOwnershipReleaseReason::Interrupted,
                    owner_claimed_at: None,
                    owner_last_heartbeat_at: None,
                    owner_lease_expires_at: None,
                    note: Some("leaf owner released ownership after interruption".to_string()),
                },
            ),
            note: Some("replay child run_leaf was interrupted after taking over".to_string()),
        };

        let detail = takeover_detail(&summary).expect("detail missing");
        assert!(detail.contains("follow_target decision=manual_review"));
        assert!(detail.contains("reason=process_handoff_risk"));
        assert!(detail.contains(
            "follow_target_checkpoint boundary=tool_results_checkpointed action=execute_pending_tools history_len=7 pending_tool_calls=1"
        ));
        assert!(detail.contains(
            "follow_target_provider_fence safe_resume_from=(boundary=tool_results_checkpointed action=execute_pending_tools history_len=7 pending_tool_calls=1) request_history_len=8 tool_count=0"
        ));
        assert!(detail.contains(
            "follow_target_process_handoff tool=terminal state=running disposition=unsafe_side_effect_window timeout_secs=30"
        ));
        assert!(detail.contains("follow_target_process_handoff_detail stdout=building..."));
        assert!(detail.contains(
            "follow_target_browser_handoff action=click state=started disposition=unsafe_side_effect_window target=#submit wait_for_navigation=true"
        ));
        assert!(detail.contains(
            "follow_target_browser_handoff_detail url=https://example.com/form title=Form"
        ));
        assert!(detail.contains("follow_target_browser_session action=navigate session_open=yes"));
        assert!(detail.contains(
            "follow_target_browser_session_detail url=https://example.com/dashboard title=Dashboard"
        ));
        assert!(detail.contains(
            "follow_target_mcp_handoff tool=mcp_resource_subscribe state=started disposition=unsafe_side_effect_window read_only=no live_runtime=yes server=docs transport=http target=uri:docs://guide"
        ));
        assert!(detail.contains(
            "follow_target_mcp_runtime tool=mcp_resource_subscribe live_runtime=yes subscriptions=1"
        ));
        assert!(detail.contains("follow_target_mcp_runtime_detail active_servers=docs"));
        assert!(detail.contains(
            "follow_target_artifact kind=assistant_output label=assistant_output source=current_run depth=0"
        ));
        assert!(detail.contains(
            "follow_target_artifact_detail run=run_leaf preview=Recovered answer preview"
        ));
        assert!(detail.contains("follow_target_assessment depth=1/3"));
        assert!(detail.contains("blocking_risks=process_handoff"));
        assert!(detail.contains("follow_target_detail"));
        assert!(detail.contains(
            "follow_target_ownership_claim worker=worker_leaf_owner lease_expires_at=2026-04-23 12:09:00 UTC"
        ));
        assert!(detail.contains(
            "follow_target_ownership_claim_detail claimed_at=2026-04-23 12:08:00 UTC lineage=lineage_takeover_leaf"
        ));
        assert!(detail.contains("follow_target_owner_release worker=worker_leaf_owner"));
        assert!(detail.contains("leaf continuation now requires manual review"));
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
                session_id: None,
                prompt: String::new(),
                replay_of_run_id: None,
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
                session_id: None,
                prompt: String::new(),
                replay_of_run_id: None,
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
                session_id: None,
                prompt: String::new(),
                replay_of_run_id: None,
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
