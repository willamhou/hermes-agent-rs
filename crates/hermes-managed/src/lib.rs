//! Managed-agent control plane primitives.

pub mod filtered_registry;
pub mod filtered_skills;
pub mod publish;
pub mod run_registry;
pub mod runtime_factory;
pub mod signet;
pub mod store;
pub mod tool_policy;
pub mod types;
pub mod yaml;

pub use filtered_registry::build_filtered_registry;
pub use filtered_skills::{FilteredSkillAccess, build_filtered_skill_manager};
pub use publish::{
    ManagedModelPreflight, ResolvedManagedVersionDefaults, preflight_managed_model,
    resolve_managed_version_defaults,
};
pub use run_registry::{RunHandle, RunRegistry, RunStatusSnapshot};
pub use runtime_factory::{ManagedRuntime, build_managed_runtime};
pub use signet::build_signet_observer;
pub use store::ManagedStore;
pub use tool_policy::{
    MANAGED_BETA_ALLOWED_TOOLS, is_managed_beta_allowed_tool, validate_managed_beta_tools,
};
pub use types::{
    ManagedAgent, ManagedAgentVersion, ManagedAgentVersionDraft, ManagedApprovalPolicy, ManagedRun,
    ManagedRunEvent, ManagedRunEventDraft, ManagedRunEventKind, ManagedRunStatus,
    validate_managed_agent_name,
};
pub use yaml::{
    MANAGED_AGENT_SYNC_METADATA_PREFIX, ManagedAgentYaml, ManagedAgentYamlDiff,
    ManagedAgentYamlFieldDiff, extract_sync_metadata_sha256,
};
