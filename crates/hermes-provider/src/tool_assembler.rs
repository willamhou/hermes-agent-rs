//! Assembles streaming tool call fragments into complete [`ToolCall`] values.

use std::collections::HashMap;

use hermes_core::message::ToolCall;

/// Partially-assembled tool call accumulated during streaming.
#[derive(Debug, Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments_buf: String,
}

/// Collects incremental tool-call deltas from a streaming response and
/// produces fully-assembled [`ToolCall`] values when the stream ends.
#[derive(Debug, Default)]
pub struct ToolCallAssembler {
    pending: HashMap<usize, PendingToolCall>,
}

impl ToolCallAssembler {
    /// Create an empty assembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new tool call at `index` with the given `id` and `name`.
    ///
    /// Calling `start` again for an existing index overwrites the entry.
    pub fn start(&mut self, index: usize, id: impl Into<String>, name: impl Into<String>) {
        self.pending.insert(
            index,
            PendingToolCall {
                id: id.into(),
                name: name.into(),
                arguments_buf: String::new(),
            },
        );
    }

    /// Append an arguments delta for the tool call at `index`.
    ///
    /// If `index` does not exist this is a no-op.
    pub fn append_arguments(&mut self, index: usize, delta: &str) {
        if let Some(entry) = self.pending.get_mut(&index) {
            entry.arguments_buf.push_str(delta);
        }
    }

    /// Consume all pending tool calls and return them sorted by index.
    ///
    /// Each accumulated arguments buffer is parsed as JSON.  If parsing fails
    /// the arguments value becomes `{"_raw": "<buf>", "_error": "<msg>"}`.
    pub fn finish(&mut self) -> Vec<ToolCall> {
        let mut entries: Vec<(usize, PendingToolCall)> = self.pending.drain().collect();
        entries.sort_by_key(|(idx, _)| *idx);

        entries
            .into_iter()
            .map(|(_, p)| {
                let arguments = match serde_json::from_str::<serde_json::Value>(&p.arguments_buf) {
                    Ok(v) => v,
                    Err(e) => serde_json::json!({
                        "_raw": p.arguments_buf,
                        "_error": e.to_string(),
                    }),
                };
                ToolCall {
                    id: p.id,
                    name: p.name,
                    arguments,
                }
            })
            .collect()
    }

    /// Returns `true` if there is at least one pending tool call.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_tool_call_assembled() {
        let mut a = ToolCallAssembler::new();
        a.start(0, "call_abc", "get_weather");
        a.append_arguments(0, r#"{"location":"#);
        a.append_arguments(0, r#""Paris"}"#);
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, serde_json::json!({"location": "Paris"}));
    }

    #[test]
    fn test_multiple_tool_calls_sorted_by_index() {
        let mut a = ToolCallAssembler::new();
        a.start(2, "call_c", "tool_c");
        a.append_arguments(2, r#"{"n":3}"#);
        a.start(0, "call_a", "tool_a");
        a.append_arguments(0, r#"{"n":1}"#);
        a.start(1, "call_b", "tool_b");
        a.append_arguments(1, r#"{"n":2}"#);
        let calls = a.finish();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].name, "tool_a");
        assert_eq!(calls[1].name, "tool_b");
        assert_eq!(calls[2].name, "tool_c");
    }

    #[test]
    fn test_invalid_json_produces_error_wrapper() {
        let mut a = ToolCallAssembler::new();
        a.start(0, "call_x", "bad_tool");
        a.append_arguments(0, "not json at all");
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["_raw"], "not json at all");
        assert!(calls[0].arguments["_error"].is_string());
    }

    #[test]
    fn test_empty_assembler() {
        let mut a = ToolCallAssembler::new();
        let calls = a.finish();
        assert!(calls.is_empty());
    }

    #[test]
    fn test_has_pending() {
        let mut a = ToolCallAssembler::new();
        assert!(!a.has_pending());
        a.start(0, "id", "name");
        assert!(a.has_pending());
        a.finish();
        assert!(!a.has_pending());
    }

    #[test]
    fn test_append_to_nonexistent_index_is_noop() {
        let mut a = ToolCallAssembler::new();
        // Should not panic and assembler stays empty
        a.append_arguments(99, r#"{"x":1}"#);
        assert!(!a.has_pending());
        let calls = a.finish();
        assert!(calls.is_empty());
    }

    #[test]
    fn test_empty_arguments() {
        // Empty string is invalid JSON — must produce error wrapper
        let mut a = ToolCallAssembler::new();
        a.start(0, "call_e", "empty_tool");
        // No append_arguments call — buffer stays empty
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["_raw"], "");
        assert!(calls[0].arguments["_error"].is_string());
    }
}
