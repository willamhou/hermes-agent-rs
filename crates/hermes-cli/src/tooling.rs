use std::sync::Arc;

use hermes_config::config::AppConfig;
use hermes_mcp::populate_registry;
use hermes_tools::registry::ToolRegistry;

pub async fn build_registry(config: &AppConfig) -> Arc<ToolRegistry> {
    let registry = Arc::new(ToolRegistry::from_inventory());
    populate_registry(Arc::clone(&registry), &config.mcp_servers).await;
    registry
}
