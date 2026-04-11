use hermes_core::message::{Content, ContentPart, Message, Role, ToolCall, ToolResult};

#[test]
fn test_user_message_constructor() {
    let msg = Message::user("hello");
    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.content.as_text(), Some("hello"));
    assert!(msg.tool_calls.is_empty());
    assert!(msg.reasoning.is_none());
    assert!(msg.name.is_none());
    assert!(msg.tool_call_id.is_none());
}

#[test]
fn test_assistant_message_constructor() {
    let msg = Message::assistant("hi there");
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.content.as_text(), Some("hi there"));
}

#[test]
fn test_system_message_constructor() {
    let msg = Message::system("you are a helpful assistant");
    assert_eq!(msg.role, Role::System);
    assert_eq!(msg.content.as_text(), Some("you are a helpful assistant"));
}

#[test]
fn test_content_text_serde_roundtrip() {
    let content = Content::Text("hello world".to_string());
    let json = serde_json::to_string(&content).unwrap();
    let back: Content = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_text(), Some("hello world"));
}

#[test]
fn test_content_parts_serde_roundtrip() {
    let content = Content::Parts(vec![
        ContentPart::Text {
            text: "some text".to_string(),
        },
        ContentPart::Image {
            data: "base64data".to_string(),
            media_type: "image/png".to_string(),
        },
    ]);
    let json = serde_json::to_string(&content).unwrap();
    let back: Content = serde_json::from_str(&json).unwrap();
    if let Content::Parts(parts) = back {
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], ContentPart::Text { text } if text == "some text"));
        assert!(
            matches!(&parts[1], ContentPart::Image { data, media_type } if data == "base64data" && media_type == "image/png")
        );
    } else {
        panic!("expected Content::Parts");
    }
}

#[test]
fn test_content_as_text_lossy_concatenates_text_parts() {
    let content = Content::Parts(vec![
        ContentPart::Text {
            text: "hello ".to_string(),
        },
        ContentPart::Image {
            data: "img".to_string(),
            media_type: "image/jpeg".to_string(),
        },
        ContentPart::Text {
            text: "world".to_string(),
        },
    ]);
    assert_eq!(content.as_text_lossy(), "hello world");
}

#[test]
fn test_message_serde_roundtrip() {
    let mut msg = Message::assistant("thinking...");
    msg.reasoning = Some("internal thoughts".to_string());
    msg.tool_calls = vec![ToolCall {
        id: "call_1".to_string(),
        name: "get_weather".to_string(),
        arguments: serde_json::json!({"city": "London"}),
    }];

    let json = serde_json::to_string(&msg).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back.role, Role::Assistant);
    assert_eq!(back.content.as_text(), Some("thinking..."));
    assert_eq!(back.reasoning.as_deref(), Some("internal thoughts"));
    assert_eq!(back.tool_calls.len(), 1);
    assert_eq!(back.tool_calls[0].name, "get_weather");
}

#[test]
fn test_message_serde_skips_none_fields() {
    let msg = Message::user("test");
    let json = serde_json::to_string(&msg).unwrap();
    assert!(!json.contains("reasoning"));
    assert!(!json.contains("name"));
    assert!(!json.contains("tool_call_id"));
}

#[test]
fn test_role_serde_lowercase() {
    assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
    assert_eq!(
        serde_json::to_string(&Role::Assistant).unwrap(),
        "\"assistant\""
    );
    assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
    assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
}

#[test]
fn test_tool_result_constructors() {
    let ok = ToolResult::ok("success response");
    assert_eq!(ok.content, "success response");
    assert!(!ok.is_error);

    let err = ToolResult::error("something went wrong");
    assert_eq!(err.content, "something went wrong");
    assert!(err.is_error);
}
