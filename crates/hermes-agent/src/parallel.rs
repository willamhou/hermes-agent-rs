//! Parallel tool execution with conflict detection.

use std::sync::Arc;

use hermes_core::{
    message::{ToolCall, ToolResult},
    stream::StreamDelta,
    tool::{ToolContext, ToolExecutionObservation, ToolExecutionResultObservation},
};
use hermes_tools::registry::ToolRegistry;
use tokio::task::JoinSet;

/// Result of executing a single tool call.
pub struct ToolCallResult {
    pub call_id: String,
    pub tool_name: String,
    pub result: ToolResult,
}

/// Tools that must never be run in parallel.
pub const NEVER_PARALLEL: &[&str] = &["clarify"];

async fn notify_tool_observer(
    ctx: &ToolContext,
    call: &ToolCall,
    toolset: Option<String>,
) -> ToolExecutionObservation {
    let observation = ToolExecutionObservation {
        session_id: ctx.session_id.clone(),
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        toolset,
        arguments: call.arguments.clone(),
    };

    let Some(observer) = ctx.execution_observer.as_ref() else {
        return observation;
    };

    if let Err(err) = observer.on_tool_call(observation.clone(), ctx).await {
        tracing::warn!(
            tool = %call.name,
            call_id = %call.id,
            "tool execution observer failed: {err}"
        );
    }

    observation
}

async fn notify_tool_result_observer(
    ctx: &ToolContext,
    observation: ToolExecutionObservation,
    result: &ToolResult,
) {
    let Some(observer) = ctx.execution_observer.as_ref() else {
        return;
    };
    let tool_name = observation.tool_name.clone();
    let call_id = observation.call_id.clone();

    if let Err(err) = observer
        .on_tool_result(
            ToolExecutionResultObservation {
                request: observation,
                result: result.clone(),
            },
            ctx,
        )
        .await
    {
        tracing::warn!(
            tool = %tool_name,
            call_id = %call_id,
            "tool result observer failed: {err}"
        );
    }
}

/// Returns `true` when the given calls can be executed concurrently.
pub fn should_parallelize(calls: &[ToolCall], registry: &ToolRegistry) -> bool {
    if calls.len() <= 1 {
        return false;
    }

    // Any call in the blocklist prevents parallelism.
    if calls
        .iter()
        .any(|c| NEVER_PARALLEL.contains(&c.name.as_str()))
    {
        return false;
    }

    // Any exclusive tool prevents parallelism.
    if calls
        .iter()
        .any(|c| registry.get(&c.name).is_some_and(|t| t.is_exclusive()))
    {
        return false;
    }

    // Collect write paths from non-read-only tools that carry a "path" arg.
    let write_paths: Vec<String> = calls
        .iter()
        .filter(|c| {
            registry
                .get(&c.name)
                .map(|t| !t.is_read_only())
                .unwrap_or(true) // unknown tool → treat as writer
        })
        .filter_map(|c| {
            c.arguments
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    if has_path_conflicts(&write_paths) {
        return false;
    }

    true
}

/// Returns `true` when `paths` contains duplicates or one path is a prefix of another.
pub fn has_path_conflicts(paths: &[String]) -> bool {
    for (i, a) in paths.iter().enumerate() {
        for b in paths.iter().skip(i + 1) {
            if a == b {
                return true;
            }
            // Check prefix relationship (ensure prefix ends at a path separator).
            let (shorter, longer) = if a.len() < b.len() { (a, b) } else { (b, a) };
            if longer.starts_with(shorter.as_str()) {
                let next = longer.as_bytes().get(shorter.len());
                if next == Some(&b'/') || next.is_none() {
                    return true;
                }
            }
        }
    }
    false
}

/// Execute `calls` concurrently using a `JoinSet`.
///
/// Results are re-sorted to match the original call order.
pub async fn execute_parallel(
    calls: &[ToolCall],
    registry: Arc<ToolRegistry>,
    ctx: &ToolContext,
) -> Vec<ToolCallResult> {
    let mut set: JoinSet<(usize, ToolCallResult)> = JoinSet::new();

    for (idx, call) in calls.iter().enumerate() {
        let registry = Arc::clone(&registry);
        let ctx = ctx.clone();
        let call = call.clone();

        // Notify progress.
        let _ = ctx
            .delta_tx
            .send(StreamDelta::ToolProgress {
                tool: call.name.clone(),
                status: "starting".to_string(),
            })
            .await;

        set.spawn(async move {
            let result = match registry.get(&call.name) {
                Some(tool) => {
                    let observation =
                        notify_tool_observer(&ctx, &call, Some(tool.toolset().to_string())).await;
                    let result = tool
                        .execute(call.arguments.clone(), &ctx)
                        .await
                        .unwrap_or_else(|e| ToolResult::error(e.to_string()));
                    notify_tool_result_observer(&ctx, observation, &result).await;
                    result
                }
                None => ToolResult::error(format!("unknown tool: {}", call.name)),
            };

            (
                idx,
                ToolCallResult {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    result,
                },
            )
        });
    }

    let mut indexed: Vec<(usize, ToolCallResult)> = Vec::with_capacity(calls.len());
    while let Some(res) = set.join_next().await {
        match res {
            Ok(pair) => indexed.push(pair),
            Err(e) => {
                // Panic in a task — surface as an error result at a dummy index.
                indexed.push((
                    usize::MAX,
                    ToolCallResult {
                        call_id: String::new(),
                        tool_name: String::new(),
                        result: ToolResult::error(format!("task panicked: {e}")),
                    },
                ));
            }
        }
    }

    // Restore original ordering.
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, r)| r).collect()
}

/// Execute `calls` one-by-one in order.
pub async fn execute_sequential(
    calls: &[ToolCall],
    registry: Arc<ToolRegistry>,
    ctx: &ToolContext,
) -> Vec<ToolCallResult> {
    let mut results = Vec::with_capacity(calls.len());

    for call in calls {
        let _ = ctx
            .delta_tx
            .send(StreamDelta::ToolProgress {
                tool: call.name.clone(),
                status: "starting".to_string(),
            })
            .await;

        let result = match registry.get(&call.name) {
            Some(tool) => {
                let observation =
                    notify_tool_observer(ctx, call, Some(tool.toolset().to_string())).await;
                let result = tool
                    .execute(call.arguments.clone(), ctx)
                    .await
                    .unwrap_or_else(|e| ToolResult::error(e.to_string()));
                notify_tool_result_observer(ctx, observation, &result).await;
                result
            }
            None => ToolResult::error(format!("unknown tool: {}", call.name)),
        };

        results.push(ToolCallResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            result,
        });
    }

    results
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use hermes_core::{
        error::Result,
        message::ToolResult,
        tool::{
            ApprovalRequest, Tool, ToolConfig, ToolContext, ToolExecutionObservation,
            ToolExecutionObserver, ToolSchema,
        },
    };
    use hermes_tools::registry::ToolRegistry;
    use tokio::sync::mpsc;

    fn make_call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("id-{name}"),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        }
    }

    fn make_call_with_path(name: &str, path: &str) -> ToolCall {
        ToolCall {
            id: format!("id-{name}"),
            name: name.to_string(),
            arguments: serde_json::json!({ "path": path }),
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "echo".to_string(),
                description: "echo".to_string(),
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

    #[derive(Default)]
    struct RecordingObserver {
        observations: Mutex<Vec<ToolExecutionObservation>>,
    }

    #[async_trait]
    impl ToolExecutionObserver for RecordingObserver {
        async fn on_tool_call(
            &self,
            observation: ToolExecutionObservation,
            _ctx: &ToolContext,
        ) -> Result<()> {
            self.observations.lock().unwrap().push(observation);
            Ok(())
        }
    }

    fn make_ctx(observer: Arc<dyn ToolExecutionObserver>) -> ToolContext {
        let (approval_tx, _) = mpsc::channel::<ApprovalRequest>(1);
        let (delta_tx, _) = mpsc::channel(4);
        ToolContext {
            session_id: "run_test".to_string(),
            working_dir: std::env::temp_dir(),
            approval_tx,
            delta_tx,
            execution_observer: Some(observer),
            tool_config: Arc::new(ToolConfig::default()),
            memory: None,
            aux_provider: None,
            skills: None,
            delegation_depth: 0,
            clarify_tx: None,
        }
    }

    #[test]
    fn test_single_call_not_parallel() {
        let registry = ToolRegistry::new();
        let calls = vec![make_call("read_file")];
        assert!(!should_parallelize(&calls, &registry));
    }

    #[test]
    fn test_clarify_blocks_parallelization() {
        let registry = ToolRegistry::new();
        let calls = vec![make_call("clarify"), make_call("read_file")];
        assert!(!should_parallelize(&calls, &registry));
    }

    #[test]
    fn test_no_conflicts_allows_parallel() {
        let registry = ToolRegistry::new();
        let calls = vec![
            make_call_with_path("write_file", "/a/foo.txt"),
            make_call_with_path("write_file", "/b/bar.txt"),
        ];
        assert!(should_parallelize(&calls, &registry));
    }

    #[test]
    fn test_has_path_conflicts_same_path() {
        let paths = vec!["/a/b".to_string(), "/a/b".to_string()];
        assert!(has_path_conflicts(&paths));
    }

    #[test]
    fn test_has_path_conflicts_prefix() {
        let paths = vec!["/a".to_string(), "/a/b".to_string()];
        assert!(has_path_conflicts(&paths));
    }

    #[test]
    fn test_no_path_conflicts() {
        let paths = vec!["/a/foo".to_string(), "/b/bar".to_string()];
        assert!(!has_path_conflicts(&paths));
    }

    #[test]
    fn test_empty_paths_no_conflict() {
        assert!(!has_path_conflicts(&[]));
    }

    #[tokio::test]
    async fn test_execute_sequential_notifies_tool_observer() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Box::new(EchoTool));

        let observer = Arc::new(RecordingObserver::default());
        let ctx = make_ctx(observer.clone());
        let calls = vec![ToolCall {
            id: "call_123".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({ "value": "hello" }),
        }];

        let results = execute_sequential(&calls, registry, &ctx).await;

        assert_eq!(results.len(), 1);
        let observations = observer.observations.lock().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].call_id, "call_123");
        assert_eq!(observations[0].tool_name, "echo");
        assert_eq!(observations[0].toolset.as_deref(), Some("test"));
    }
}
