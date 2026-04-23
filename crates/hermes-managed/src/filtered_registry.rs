use std::collections::HashSet;

use hermes_core::error::{HermesError, Result};
use hermes_tools::ToolRegistry;

use crate::tool_policy::validate_managed_beta_tools;

pub fn build_filtered_registry(
    source: &ToolRegistry,
    allowed_tools: &[String],
) -> Result<ToolRegistry> {
    validate_managed_beta_tools(allowed_tools)?;

    let allowed = allowed_tools.iter().cloned().collect::<HashSet<_>>();
    let mut missing = allowed
        .iter()
        .filter(|name| !source.contains(name))
        .cloned()
        .collect::<Vec<_>>();
    missing.sort();

    if !missing.is_empty() {
        return Err(HermesError::Config(format!(
            "managed tool allowlist references unknown tools: {}",
            missing.join(", ")
        )));
    }

    Ok(source.filtered(allowed.iter().map(String::as_str)))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use hermes_core::{
        error::Result,
        message::ToolResult,
        tool::{Tool, ToolContext, ToolSchema},
    };
    use serde_json::json;

    use super::*;

    struct MockTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.name
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name().to_string(),
                description: format!("mock {}", self.name()),
                parameters: json!({}),
            }
        }

        fn toolset(&self) -> &str {
            "test"
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult> {
            Ok(ToolResult::ok(self.name().to_string()))
        }
    }

    #[test]
    fn filtered_registry_hides_blocked_tools_from_schema_and_lookup() {
        let source = ToolRegistry::new();
        source.register(Box::new(MockTool { name: "read_file" }));
        source.register(Box::new(MockTool { name: "terminal" }));

        let filtered = build_filtered_registry(&source, &["read_file".to_string()]).unwrap();
        let schemas = filtered.available_schemas();

        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "read_file");
        assert!(filtered.get("read_file").is_some());
        assert!(filtered.get("terminal").is_none());
    }

    #[test]
    fn filtered_registry_rejects_tools_outside_beta_policy() {
        let source = ToolRegistry::new();
        source.register(Box::new(MockTool { name: "read_file" }));
        source.register(Box::new(MockTool { name: "terminal" }));

        let err = build_filtered_registry(&source, &["terminal".to_string()])
            .err()
            .unwrap();
        assert!(err.to_string().contains("terminal"));
    }

    #[test]
    fn filtered_registry_rejects_unknown_tools() {
        let source = ToolRegistry::new();
        source.register(Box::new(MockTool { name: "read_file" }));
        source.register(Box::new(MockTool {
            name: "vision_analyze",
        }));

        let err = build_filtered_registry(
            &source,
            &["read_file".to_string(), "skill_view".to_string()],
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("skill_view"));
    }
}
