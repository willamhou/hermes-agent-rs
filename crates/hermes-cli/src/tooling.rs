use std::sync::Arc;

use hermes_agent::delegate::DelegationTool;
use hermes_config::config::{AppConfig, hermes_home};
use hermes_mcp::populate_registry;
use hermes_tools::registry::ToolRegistry;

pub async fn build_registry(config: &AppConfig) -> Arc<ToolRegistry> {
    let registry = ToolRegistry::from_inventory();

    // Register DelegationTool manually (lives in hermes-agent, not hermes-tools,
    // so it cannot use inventory::submit!).
    let memory_dir = hermes_home().join("memories").join("delegation");
    registry.register(Box::new(DelegationTool::new(memory_dir)));

    let registry = Arc::new(registry);
    populate_registry(Arc::clone(&registry), &config.mcp_servers).await;
    registry
}
