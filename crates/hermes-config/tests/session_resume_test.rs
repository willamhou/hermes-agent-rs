use hermes_config::sqlite_store::SqliteSessionStore;
use hermes_core::{
    message::{Content, Message, Role, ToolCall},
    provider::TokenUsage,
    session::{SessionMeta, SessionStore},
};
use std::sync::Arc;

fn make_meta(id: &str, started_at: &str) -> SessionMeta {
    SessionMeta {
        id: id.to_owned(),
        source: "cli".to_owned(),
        model: "test-model".to_owned(),
        system_prompt: "You are a test assistant.".to_owned(),
        cwd: "/tmp".to_owned(),
        started_at: started_at.to_owned(),
        ended_at: None,
        message_count: 0,
        tool_call_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        title: Some("Test session".to_owned()),
    }
}

#[tokio::test]
async fn test_full_session_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = SqliteSessionStore::open_at(&db_path).await.unwrap();

    store
        .create_session(&make_meta("sess-lifecycle", "2024-01-01T00:00:00"))
        .await
        .unwrap();

    // user message
    let user_msg = Message::user("Hello from user");

    // assistant message
    let asst_msg = Message::assistant("Hello from assistant");

    // assistant message with tool call
    let tool_call_msg = Message {
        role: Role::Assistant,
        content: Content::Text("Calling tool".to_owned()),
        tool_calls: vec![ToolCall {
            id: "tc-1".to_owned(),
            name: "terminal".to_owned(),
            arguments: serde_json::json!({"command": "ls"}),
        }],
        reasoning: None,
        name: None,
        tool_call_id: None,
    };

    // tool result message
    let tool_result_msg = Message {
        role: Role::Tool,
        content: Content::Text("file1.txt\nfile2.txt".to_owned()),
        tool_calls: vec![],
        reasoning: None,
        name: Some("terminal".to_owned()),
        tool_call_id: Some("tc-1".to_owned()),
    };

    store
        .append_message("sess-lifecycle", &user_msg)
        .await
        .unwrap();
    store
        .append_message("sess-lifecycle", &asst_msg)
        .await
        .unwrap();
    store
        .append_message("sess-lifecycle", &tool_call_msg)
        .await
        .unwrap();
    store
        .append_message("sess-lifecycle", &tool_result_msg)
        .await
        .unwrap();

    store.end_session("sess-lifecycle").await.unwrap();

    let history = store.load_history("sess-lifecycle").await.unwrap();
    assert_eq!(history.len(), 4);

    // order and roles
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[2].role, Role::Assistant);
    assert_eq!(history[3].role, Role::Tool);

    // content preserved
    assert_eq!(history[0].content.as_text_lossy(), "Hello from user");
    assert_eq!(history[1].content.as_text_lossy(), "Hello from assistant");
    assert_eq!(history[2].content.as_text_lossy(), "Calling tool");
    assert_eq!(history[3].content.as_text_lossy(), "file1.txt\nfile2.txt");

    // tool call JSON roundtrip
    assert_eq!(history[2].tool_calls.len(), 1);
    assert_eq!(history[2].tool_calls[0].id, "tc-1");
    assert_eq!(history[2].tool_calls[0].name, "terminal");
    assert_eq!(
        history[2].tool_calls[0].arguments,
        serde_json::json!({"command": "ls"})
    );

    // tool_call_id and tool_name preserved on tool result
    assert_eq!(history[3].tool_call_id, Some("tc-1".to_owned()));
    assert_eq!(history[3].name, Some("terminal".to_owned()));

    // ended_at is set
    let meta = store.get_session("sess-lifecycle").await.unwrap().unwrap();
    assert!(meta.ended_at.is_some());
}

#[tokio::test]
async fn test_resume_preserves_tool_calls_json() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");

    {
        let store = SqliteSessionStore::open_at(&db_path).await.unwrap();
        store
            .create_session(&make_meta("sess-tc-json", "2024-01-02T00:00:00"))
            .await
            .unwrap();

        let msg = Message {
            role: Role::Assistant,
            content: Content::Text("".to_owned()),
            tool_calls: vec![ToolCall {
                id: "c1".to_owned(),
                name: "terminal".to_owned(),
                arguments: serde_json::json!({"command": "ls"}),
            }],
            reasoning: None,
            name: None,
            tool_call_id: None,
        };
        store.append_message("sess-tc-json", &msg).await.unwrap();
        // store dropped here, closing the connection
    }

    // reopen same DB
    let store2 = SqliteSessionStore::open_at(&db_path).await.unwrap();
    let history = store2.load_history("sess-tc-json").await.unwrap();

    assert_eq!(history.len(), 1);
    let tc = &history[0].tool_calls[0];
    assert_eq!(tc.id, "c1");
    assert_eq!(tc.name, "terminal");
    assert_eq!(tc.arguments, serde_json::json!({"command": "ls"}));
}

#[tokio::test]
async fn test_resume_preserves_reasoning() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = SqliteSessionStore::open_at(&db_path).await.unwrap();

    store
        .create_session(&make_meta("sess-reasoning", "2024-01-03T00:00:00"))
        .await
        .unwrap();

    let msg = Message {
        role: Role::Assistant,
        content: Content::Text("My answer".to_owned()),
        tool_calls: vec![],
        reasoning: Some("thinking...".to_owned()),
        name: None,
        tool_call_id: None,
    };
    store.append_message("sess-reasoning", &msg).await.unwrap();

    let history = store.load_history("sess-reasoning").await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].reasoning, Some("thinking...".to_owned()));
}

#[tokio::test]
async fn test_multiple_sessions_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = SqliteSessionStore::open_at(&db_path).await.unwrap();

    store
        .create_session(&make_meta("sess-a", "2024-01-04T00:00:00"))
        .await
        .unwrap();
    store
        .create_session(&make_meta("sess-b", "2024-01-04T00:01:00"))
        .await
        .unwrap();

    for i in 0..3u8 {
        let msg = Message::user(format!("session-a message {i}"));
        store.append_message("sess-a", &msg).await.unwrap();
    }
    for i in 0..2u8 {
        let msg = Message::user(format!("session-b message {i}"));
        store.append_message("sess-b", &msg).await.unwrap();
    }

    let history_a = store.load_history("sess-a").await.unwrap();
    let history_b = store.load_history("sess-b").await.unwrap();

    assert_eq!(history_a.len(), 3);
    assert_eq!(history_b.len(), 2);

    // no cross-contamination: all msgs in A start with "session-a"
    for msg in &history_a {
        assert!(
            msg.content.as_text_lossy().starts_with("session-a"),
            "unexpected content in session A: {}",
            msg.content.as_text_lossy()
        );
    }
    for msg in &history_b {
        assert!(
            msg.content.as_text_lossy().starts_with("session-b"),
            "unexpected content in session B: {}",
            msg.content.as_text_lossy()
        );
    }
}

#[tokio::test]
async fn test_session_message_count_tracking() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = SqliteSessionStore::open_at(&db_path).await.unwrap();

    store
        .create_session(&make_meta("sess-count", "2024-01-05T00:00:00"))
        .await
        .unwrap();

    for i in 0..5u8 {
        let msg = Message::user(format!("msg {i}"));
        store.append_message("sess-count", &msg).await.unwrap();
    }

    let meta = store.get_session("sess-count").await.unwrap().unwrap();
    assert_eq!(meta.message_count, 5);
}

#[tokio::test]
async fn test_session_usage_accumulation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = SqliteSessionStore::open_at(&db_path).await.unwrap();

    store
        .create_session(&make_meta("sess-usage", "2024-01-06T00:00:00"))
        .await
        .unwrap();

    store
        .update_usage(
            "sess-usage",
            &TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    store
        .update_usage(
            "sess-usage",
            &TokenUsage {
                input_tokens: 200,
                output_tokens: 100,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let meta = store.get_session("sess-usage").await.unwrap().unwrap();
    assert_eq!(meta.input_tokens, 300);
    assert_eq!(meta.output_tokens, 150);
}

#[tokio::test]
async fn test_list_sessions_ordering() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = SqliteSessionStore::open_at(&db_path).await.unwrap();

    for i in 1..=5u8 {
        // Use distinct ISO timestamps so ordering is deterministic
        let ts = format!("2024-01-{:02}T00:00:00", i);
        store
            .create_session(&make_meta(&format!("sess-{i}"), &ts))
            .await
            .unwrap();
    }

    let list = store.list_sessions(3).await.unwrap();

    assert_eq!(list.len(), 3, "should return exactly 3 sessions");
    // most recent first
    assert_eq!(list[0].id, "sess-5");
    assert_eq!(list[1].id, "sess-4");
    assert_eq!(list[2].id, "sess-3");
}

#[tokio::test]
async fn test_reopen_database_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");

    {
        let store = SqliteSessionStore::open_at(&db_path).await.unwrap();
        store
            .create_session(&make_meta("sess-persist", "2024-01-10T00:00:00"))
            .await
            .unwrap();
        store
            .append_message("sess-persist", &Message::user("persisted user msg"))
            .await
            .unwrap();
        store
            .append_message("sess-persist", &Message::assistant("persisted asst msg"))
            .await
            .unwrap();
        // store and connection dropped here
    }

    // reopen
    let store2 = SqliteSessionStore::open_at(&db_path).await.unwrap();
    let history = store2.load_history("sess-persist").await.unwrap();

    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content.as_text_lossy(), "persisted user msg");
    assert_eq!(history[1].content.as_text_lossy(), "persisted asst msg");
}

#[tokio::test]
async fn test_empty_content_message() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = SqliteSessionStore::open_at(&db_path).await.unwrap();

    store
        .create_session(&make_meta("sess-empty", "2024-01-11T00:00:00"))
        .await
        .unwrap();

    let msg = Message::user("");
    store.append_message("sess-empty", &msg).await.unwrap();

    let history = store.load_history("sess-empty").await.unwrap();
    assert_eq!(history.len(), 1);
    // empty string, not None
    assert_eq!(history[0].content.as_text_lossy(), "");
}

#[tokio::test]
async fn test_concurrent_append_safety() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = Arc::new(SqliteSessionStore::open_at(&db_path).await.unwrap());

    store
        .create_session(&make_meta("sess-concurrent", "2024-01-12T00:00:00"))
        .await
        .unwrap();

    let mut handles = Vec::new();
    for i in 0..10u8 {
        let store_clone = Arc::clone(&store);
        let content = format!("concurrent-msg-{i}");
        let handle = tokio::spawn(async move {
            let msg = Message::user(content);
            store_clone
                .append_message("sess-concurrent", &msg)
                .await
                .unwrap();
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.unwrap();
    }

    let history = store.load_history("sess-concurrent").await.unwrap();
    assert_eq!(history.len(), 10, "all 10 messages should be present");

    // verify each expected message is present (order may vary)
    let texts: Vec<String> = history.iter().map(|m| m.content.as_text_lossy()).collect();
    for i in 0..10u8 {
        let expected = format!("concurrent-msg-{i}");
        assert!(texts.contains(&expected), "missing message: {expected}");
    }
}
