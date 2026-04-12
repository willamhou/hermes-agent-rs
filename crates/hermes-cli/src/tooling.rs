use std::sync::Arc;

use hermes_config::config::AppConfig;
use hermes_mcp::discover_tools;
use hermes_tools::registry::ToolRegistry;

pub async fn build_registry(config: &AppConfig) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::from_inventory();

    for tool in discover_tools(&config.mcp_servers).await {
        let tool_name = tool.name().to_string();
        if registry.get(&tool_name).is_some() {
            tracing::warn!(tool = %tool_name, "skipping MCP tool name collision");
            continue;
        }
        registry.register(tool);
    }

    Arc::new(registry)
}
