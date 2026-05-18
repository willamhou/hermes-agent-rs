//! Managed-agent control plane primitives.

pub mod filtered_registry;
pub mod filtered_skills;
pub mod publish;
pub mod run_registry;
pub mod run_summary;
pub mod runtime_factory;
pub mod signet;
pub mod store;
pub mod tool_policy;
pub mod types;
pub mod yaml;

#[doc(hidden)]
pub use filtered_registry::classify_managed_mcp_admission_rejection;
pub use filtered_registry::{
    ManagedMcpAdmissionRejection, ManagedMcpReadOnlyCapabilityAttribution,
    ManagedRegistryBuildError, build_filtered_registry, managed_mcp_admission_rejection_from_event,
    normalize_legacy_managed_mcp_admission_rejection, parse_legacy_managed_mcp_admission_rejection,
};
pub use filtered_skills::{FilteredSkillAccess, build_filtered_skill_manager};
pub use publish::{
    ManagedModelPreflight, ResolvedManagedVersionDefaults, preflight_managed_model,
    resolve_managed_version_defaults,
};
pub use run_registry::{RunHandle, RunRegistry, RunStatusSnapshot};
pub use run_summary::{
    ManagedRunArtifactContinuitySummary, ManagedRunBrowserHandoffState,
    ManagedRunBrowserHandoffSummary, ManagedRunBrowserReplayDisposition,
    ManagedRunBrowserSessionCheckpointSummary, ManagedRunCleanupFailureSummary,
    ManagedRunContinuationAction, ManagedRunContinuationBoundaryKind,
    ManagedRunContinuationCheckpointSummary, ManagedRunContinuationSummary,
    ManagedRunDerivedSummary, ManagedRunInterruptionCause, ManagedRunInterruptionSummary,
    ManagedRunMcpHandoffState, ManagedRunMcpHandoffSummary, ManagedRunMcpReplayDisposition,
    ManagedRunMcpRuntimeCheckpointSummary, ManagedRunOwnershipClaimSummary,
    ManagedRunOwnershipReleaseReason, ManagedRunOwnershipReleaseSummary,
    ManagedRunProcessHandoffState, ManagedRunProcessHandoffSummary,
    ManagedRunProcessReplayDisposition, ManagedRunProviderCallFenceSummary,
    ManagedRunRecoveryDecisionKind, ManagedRunRecoveryDecisionReason,
    ManagedRunRecoveryDecisionSummary, ManagedRunRecoveryHint, ManagedRunReplayChildSummary,
    ManagedRunReplayProvenanceSummary, ManagedRunReplayTrigger,
    ManagedRunTakeoverAssessmentSummary, ManagedRunTakeoverState, ManagedRunTakeoverSummary,
    load_managed_run_derived_summaries, load_managed_run_derived_summary,
    managed_run_browser_handoff_from_event, managed_run_browser_session_checkpoint_from_event,
    managed_run_cleanup_failure_from_event, managed_run_continuation_checkpoint_from_event,
    managed_run_continuation_from_event, managed_run_interruption_from_event,
    managed_run_mcp_handoff_from_event, managed_run_mcp_runtime_checkpoint_from_event,
    managed_run_ownership_claim_from_event, managed_run_process_handoff_from_event,
    managed_run_provider_call_fence_from_event, managed_run_recovery_decision_from_event,
    managed_run_recovery_hint_from_run, managed_run_replay_provenance_from_event,
    managed_run_takeover_assessment_from_event,
};
pub use runtime_factory::{
    ManagedRuntime, ManagedRuntimeBuildContext, ManagedRuntimeBuildError, build_managed_runtime,
};
pub use signet::build_signet_observer;
pub use store::ManagedStore;
pub use tool_policy::{
    MANAGED_BETA_ALLOWED_TOOLS, MANAGED_MCP_READ_ONLY_TOOLS, MANAGED_MCP_SIDE_EFFECT_TOOLS,
    is_managed_beta_allowed_tool, is_managed_mcp_read_only_tool, is_managed_mcp_side_effect_tool,
    is_managed_mcp_tool, validate_managed_beta_tools, validate_managed_mcp_policy,
    validate_managed_runtime_tool_policy,
};
pub use types::{
    ManagedAgent, ManagedAgentVersion, ManagedAgentVersionDraft, ManagedApprovalPolicy, ManagedRun,
    ManagedRunArtifact, ManagedRunArtifactDraft, ManagedRunArtifactKind, ManagedRunCleanupResource,
    ManagedRunCleanupResourceKind, ManagedRunEvent, ManagedRunEventDraft, ManagedRunEventKind,
    ManagedRunOwnerSnapshot, ManagedRunOwnerState, ManagedRunStatus, validate_managed_agent_name,
};
pub use yaml::{
    MANAGED_AGENT_SYNC_METADATA_PREFIX, ManagedAgentYaml, ManagedAgentYamlDiff,
    ManagedAgentYamlFieldDiff, extract_sync_metadata_sha256,
};
