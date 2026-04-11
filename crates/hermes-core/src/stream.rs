#[derive(Debug, Clone)]
pub enum StreamDelta {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallArgsDelta { id: String, delta: String },
    ToolProgress { tool: String, status: String },
    Done,
}
