use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use hermes_core::tool::Tool;

/// A compile-time tool registration entry submitted via `inventory`.
pub struct ToolRegistration {
    pub factory: fn() -> Box<dyn Tool>,
}

inventory::collect!(ToolRegistration);

/// Runtime registry that holds named tool instances.
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    /// Build a registry from all `ToolRegistration` entries submitted to `inventory`.
    pub fn from_inventory() -> Self {
        let registry = Self::new();
        for reg in inventory::iter::<ToolRegistration> {
            let tool = (reg.factory)();
            registry.register(tool);
        }
        registry
    }

    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }

    /// Insert a tool, keyed by its `name()`.
    pub fn register(&self, tool: Box<dyn Tool>) {
        self.tools
            .write()
            .expect("tool registry write lock poisoned")
            .insert(tool.name().to_string(), Arc::from(tool));
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .read()
            .expect("tool registry read lock poisoned")
            .get(name)
            .cloned()
    }

    /// Return schemas for all currently-available tools (`is_available() == true`).
    pub fn available_schemas(&self) -> Vec<hermes_core::tool::ToolSchema> {
        self.tools
            .read()
            .expect("tool registry read lock poisoned")
            .values()
            .filter(|t| t.is_available())
            .map(|t| t.schema())
            .collect()
    }

    /// Return names of all registered tools.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .read()
            .expect("tool registry read lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools
            .read()
            .expect("tool registry read lock poisoned")
            .len()
    }

    /// True when the registry contains no tools.
    pub fn is_empty(&self) -> bool {
        self.tools
            .read()
            .expect("tool registry read lock poisoned")
            .is_empty()
    }

    /// Remove a tool by name and return whether it was present.
    pub fn remove(&self, name: &str) -> bool {
        self.tools
            .write()
            .expect("tool registry write lock poisoned")
            .remove(name)
            .is_some()
    }

    /// Return tool names grouped by toolset.
    pub fn tools_by_toolset(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        let guard = self.tools.read().expect("tool registry read lock poisoned");
        let mut map = std::collections::BTreeMap::<String, Vec<String>>::new();
        for (name, tool) in guard.iter() {
            map.entry(tool.toolset().to_string())
                .or_default()
                .push(name.clone());
        }
        for names in map.values_mut() {
            names.sort_unstable();
        }
        map
    }

    /// Replace every tool with the given toolset, keeping all other toolsets intact.
    pub fn replace_toolset(&self, toolset: &str, tools: Vec<Box<dyn Tool>>) {
        let mut guard = self
            .tools
            .write()
            .expect("tool registry write lock poisoned");
        guard.retain(|_, tool| tool.toolset() != toolset);
        for tool in tools {
            let tool_name = tool.name().to_string();
            if guard.contains_key(&tool_name) {
                tracing::warn!(tool = %tool_name, toolset = %toolset, "skipping toolset replacement due to name collision");
                continue;
            }
            guard.insert(tool_name, Arc::from(tool));
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use async_trait::async_trait;
    use hermes_core::{
        error::Result,
        message::ToolResult,
        tool::{Tool, ToolConfig, ToolContext, ToolSchema},
    };

    // ── Mock tools ────────────────────────────────────────────────────────────

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "echo".to_string(),
                description: "Echoes the input".to_string(),
                parameters: serde_json::json!({}),
            }
        }

        fn toolset(&self) -> &str {
            "test"
        }

        async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
            Ok(ToolResult::ok(args.to_string()))
        }
    }

    struct UnavailableTool;

    #[async_trait]
    impl Tool for UnavailableTool {
        fn name(&self) -> &str {
            "unavailable"
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "unavailable".to_string(),
                description: "Always unavailable".to_string(),
                parameters: serde_json::json!({}),
            }
        }

        fn toolset(&self) -> &str {
            "test"
        }

        fn is_available(&self) -> bool {
            false
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult> {
            Ok(ToolResult::error("unavailable"))
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_ctx() -> (
        ToolContext,
        tokio::sync::mpsc::Receiver<hermes_core::tool::ApprovalRequest>,
        tokio::sync::mpsc::Receiver<hermes_core::stream::StreamDelta>,
    ) {
        let (approval_tx, approval_rx) = tokio::sync::mpsc::channel(8);
        let (delta_tx, delta_rx) = tokio::sync::mpsc::channel(8);
        let ctx = ToolContext {
            session_id: "test-session".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            approval_tx,
            delta_tx,
            tool_config: Arc::new(ToolConfig::default()),
            memory: None,
            aux_provider: None,
            skills: None,
            delegation_depth: 0,
            clarify_tx: None,
        };
        (ctx, approval_rx, delta_rx)
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_empty_registry() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_manual_register_and_lookup() {
        let registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));

        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());

        let tool = registry.get("echo");
        assert!(tool.is_some());
        assert_eq!(tool.unwrap().name(), "echo");

        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_available_schemas() {
        let registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(UnavailableTool));

        let schemas = registry.available_schemas();
        // Only EchoTool is available
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "echo");
    }

    #[test]
    fn test_tool_names() {
        let registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(UnavailableTool));

        let mut names = registry.tool_names();
        names.sort_unstable();
        assert_eq!(names, vec!["echo".to_string(), "unavailable".to_string()]);
    }

    #[tokio::test]
    async fn test_tool_execute() {
        let registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));

        let (ctx, _approval_rx, _delta_rx) = make_ctx();
        let args = serde_json::json!({"msg": "hello"});

        let result = registry
            .get("echo")
            .unwrap()
            .execute(args.clone(), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.content, args.to_string());
    }

    #[test]
    fn test_from_inventory_runs_without_panic() {
        // No ToolRegistration entries are submitted in tests, so this just
        // verifies the function doesn't panic on an empty inventory.
        let registry = ToolRegistry::from_inventory();
        // The count may be 0 or more depending on linked registrations.
        let _ = registry.len();
    }
}
