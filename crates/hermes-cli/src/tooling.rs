use std::sync::Arc;

use hermes_agent::delegate::DelegationTool;
use hermes_config::config::{AppConfig, hermes_home};
use hermes_cron::CronTool;
use hermes_mcp::populate_registry;
use hermes_tools::registry::ToolRegistry;

pub async fn build_registry(config: &AppConfig) -> Arc<ToolRegistry> {
    let registry = ToolRegistry::from_inventory();

    // DelegationTool registered manually (not via inventory) so child agents
    // built with from_inventory() won't have it. See delegate.rs.
    // Lives in hermes-agent (not hermes-tools), so inventory::submit! is unavailable.
    let memory_dir = hermes_home().join("memories").join("delegation");
    registry.register(Box::new(DelegationTool::new(memory_dir)));

    let cron_store_path = hermes_home().join("cron").join("jobs.json");
    registry.register(Box::new(CronTool::new(cron_store_path)));

    let registry = Arc::new(registry);
    populate_registry(Arc::clone(&registry), &config.mcp_servers).await;
    registry
}
