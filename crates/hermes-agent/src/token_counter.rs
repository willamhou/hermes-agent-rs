use hermes_core::message::Message;

pub struct TokenCounter;

impl TokenCounter {
    pub fn count_text(text: &str) -> usize {
        text.len().div_ceil(4)
    }

    pub fn count_message(msg: &Message) -> usize {
        let mut count = 4; // per-message overhead
        count += Self::count_text(&msg.content.as_text_lossy());
        for tc in &msg.tool_calls {
            count += Self::count_text(&tc.name);
            count += Self::count_text(&tc.arguments.to_string());
        }
        if let Some(ref r) = msg.reasoning {
            count += Self::count_text(r);
        }
        count
    }

    pub fn count_messages(msgs: &[Message]) -> usize {
        msgs.iter().map(Self::count_message).sum()
    }

    pub fn estimate_request(system: &str, msgs: &[Message], tool_count: usize) -> usize {
        let mut total = Self::count_text(system);
        total += Self::count_messages(msgs);
        total += tool_count * 50; // ~50 tokens per tool schema
        total
    }
}

#[cfg(test)]
mod tests {
    use hermes_core::message::Message;

    use super::TokenCounter;

    #[test]
    fn test_count_text_empty() {
        assert_eq!(TokenCounter::count_text(""), 0);
    }

    #[test]
    fn test_count_text_short() {
        assert_eq!(TokenCounter::count_text("hello"), 2);
    }

    #[test]
    fn test_count_text_long() {
        let s = "a".repeat(400);
        assert_eq!(TokenCounter::count_text(&s), 100);
    }

    #[test]
    fn test_count_message_with_tool_calls() {
        use hermes_core::message::ToolCall;
        use serde_json::json;

        let mut msg = Message::user("hello world");
        msg.tool_calls.push(ToolCall {
            id: "call_1".to_string(),
            name: "my_tool".to_string(),
            arguments: json!({"key": "value"}),
        });

        let content_only_count = 4 + TokenCounter::count_text("hello world");
        let with_tool_count = TokenCounter::count_message(&msg);
        assert!(with_tool_count > content_only_count);
    }

    #[test]
    fn test_estimate_request_includes_system() {
        let system = "You are a helpful assistant.";
        let msgs = vec![Message::user("hi")];

        let base = TokenCounter::estimate_request("", &msgs, 0);
        let with_system = TokenCounter::estimate_request(system, &msgs, 0);
        let with_tools = TokenCounter::estimate_request("", &msgs, 3);

        assert!(with_system > base);
        assert_eq!(with_tools, base + 150); // 3 tools * 50
    }
}
