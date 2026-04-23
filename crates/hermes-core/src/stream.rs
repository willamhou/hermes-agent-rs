#[derive(Debug, Clone)]
pub enum StreamDelta {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        id: String,
        delta: String,
    },
    ToolProgress {
        tool: String,
        status: String,
    },
    ToolEvent {
        kind: String,
        tool: String,
        call_id: Option<String>,
        message: Option<String>,
        metadata: Option<serde_json::Value>,
    },
    Done,
}
