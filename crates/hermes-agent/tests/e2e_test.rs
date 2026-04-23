//! End-to-end integration tests for the agent loop with real tool implementations.
//!
//! These tests use a MockProvider that returns scripted ChatResponses and a
//! ToolRegistry loaded with real ReadFileTool, WriteFileTool, and TerminalTool.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use hermes_agent::{
    compressor::CompressionConfig,
    loop_runner::{Agent, AgentConfig},
};
use hermes_core::{
    error::Result as HermesResult,
    message::{Content, Role, ToolCall},
    provider::{ChatRequest, ChatResponse, FinishReason, ModelInfo, ModelPricing, TokenUsage},
    stream::StreamDelta,
    tool::{ApprovalDecision, ApprovalRequest, BrowserToolConfig, ToolConfig},
};
use hermes_memory::MemoryManager;
use hermes_tools::{
    browser::BrowserTool, file_read::ReadFileTool, file_write::WriteFileTool,
    registry::ToolRegistry, terminal::TerminalTool,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::mpsc,
};

// ── MockProvider ──────────────────────────────────────────────────────────────

struct MockProvider {
    responses: Mutex<Vec<ChatResponse>>,
}

impl MockProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

fn static_model_info() -> &'static ModelInfo {
    static INFO: std::sync::OnceLock<ModelInfo> = std::sync::OnceLock::new();
    INFO.get_or_init(|| ModelInfo {
        id: "mock".to_string(),
        provider: "mock".to_string(),
        max_context: 128_000,
        max_output: 4096,
        supports_tools: true,
        supports_vision: false,
        supports_reasoning: false,
        supports_caching: false,
        pricing: ModelPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
            cache_create_per_mtok: 0.0,
        },
    })
}

#[async_trait]
impl hermes_core::provider::Provider for MockProvider {
    async fn chat(
        &self,
        _request: &ChatRequest<'_>,
        _delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> HermesResult<ChatResponse> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Ok(ChatResponse {
                content: "done".to_string(),
                tool_calls: vec![],
                reasoning: None,
                finish_reason: FinishReason::Stop,
                usage: TokenUsage::default(),
                cache_meta: None,
            });
        }
        Ok(responses.remove(0))
    }

    fn model_info(&self) -> &ModelInfo {
        static_model_info()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn stop_response(content: &str) -> ChatResponse {
    ChatResponse {
        content: content.to_string(),
        tool_calls: vec![],
        reasoning: None,
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
        cache_meta: None,
    }
}

fn tool_response(content: &str, calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        content: content.to_string(),
        tool_calls: calls,
        reasoning: None,
        finish_reason: FinishReason::ToolUse,
        usage: TokenUsage::default(),
        cache_meta: None,
    }
}

fn make_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args,
    }
}

/// Build an agent with the given provider and registry, spawning an approval handler.
///
/// Returns `(agent, approval_rx)` — the receiver is owned by the spawned task,
/// so the caller does not need to drive it manually.
fn make_agent_with_approval<F>(
    provider: MockProvider,
    registry: ToolRegistry,
    workspace: &tempfile::TempDir,
    approval_handler: F,
) -> Agent
where
    F: Fn(ApprovalRequest) + Send + 'static,
{
    make_agent_with_tool_config(
        provider,
        registry,
        workspace,
        ToolConfig {
            workspace_root: workspace.path().to_path_buf(),
            ..ToolConfig::default()
        },
        approval_handler,
    )
}

fn make_agent_with_tool_config<F>(
    provider: MockProvider,
    registry: ToolRegistry,
    workspace: &tempfile::TempDir,
    tool_config: ToolConfig,
    approval_handler: F,
) -> Agent
where
    F: Fn(ApprovalRequest) + Send + 'static,
{
    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequest>(16);

    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            approval_handler(req);
        }
    });

    let workspace_path = workspace.path().to_path_buf();
    let memory = MemoryManager::new(workspace.path().join(".hermes-memory"), None).unwrap();

    Agent::new(AgentConfig {
        provider: Arc::new(provider),
        registry: Arc::new(registry),
        max_iterations: 20,
        system_prompt: "test agent".to_string(),
        session_id: "e2e-test".to_string(),
        working_dir: workspace_path.clone(),
        approval_tx,
        tool_config: Arc::new(tool_config),
        execution_observer: None,
        memory,
        skills: None,
        compression: CompressionConfig::default(),
        delegation_depth: 0,
        clarify_tx: None,
    })
}

fn browser_executable() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("HERMES_BROWSER_EXECUTABLE") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let candidates = [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

async fn spawn_browser_test_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", addr);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };

            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let Ok(read) = stream.read(&mut buffer).await else {
                    return;
                };
                if read == 0 {
                    return;
                }

                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                let body = match path {
                    "/" => {
                        r#"<!doctype html>
<html>
  <body>
    <h1>Browser Test</h1>
    <label for="name">Name</label>
    <input id="name" />
    <button id="go" onclick="window.location.href='/hello?name=' + encodeURIComponent(document.getElementById('name').value)">Go</button>
  </body>
</html>"#
                            .to_string()
                    }
                    p if p.starts_with("/hello") => {
                        let name = p
                            .split("?name=")
                            .nth(1)
                            .map(|value| value.replace("%20", " "))
                            .unwrap_or_else(|| "unknown".to_string());
                        if name == "Hermes" {
                            r#"<!doctype html><html><body><h1 id="message">Hello, Hermes!</h1></body></html>"#.to_string()
                        } else {
                            format!(
                                "<!doctype html><html><body><h1 id=\"message\">Hello, {}!</h1></body></html>",
                                name
                            )
                        }
                    }
                    _ => {
                        r#"<!doctype html><html><body><h1>Not found</h1></body></html>"#.to_string()
                    }
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (base_url, handle)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Verifies write_file → read_file → final-text agent loop with real tools.
#[tokio::test]
async fn test_agent_writes_and_reads_file() {
    let workspace = tempfile::TempDir::new().unwrap();
    let file_path = workspace.path().join("test.txt");

    let registry = ToolRegistry::new();
    registry.register(Box::new(WriteFileTool));
    registry.register(Box::new(ReadFileTool));

    let provider = MockProvider::new(vec![
        // Turn 1: write file
        tool_response(
            "I'll write the file.",
            vec![make_call(
                "call_1",
                "write_file",
                serde_json::json!({
                    "path": file_path.to_str().unwrap(),
                    "content": "hello from e2e test"
                }),
            )],
        ),
        // Turn 2: read file back
        tool_response(
            "Now I'll read it.",
            vec![make_call(
                "call_2",
                "read_file",
                serde_json::json!({ "path": file_path.to_str().unwrap() }),
            )],
        ),
        // Turn 3: final response
        stop_response("The file contains: hello from e2e test"),
    ]);

    let mut agent = make_agent_with_approval(provider, registry, &workspace, |req| {
        let _ = req.response_tx.send(ApprovalDecision::Allow);
    });

    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![];
    let result = agent
        .run_conversation("write and read a file", &mut history, tx)
        .await
        .unwrap();

    // File was actually written on disk
    assert!(file_path.exists(), "file was not written to disk");
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "hello from e2e test"
    );

    // 3 turns: user + [assistant+tool_result] * 2 + assistant_final = 6 messages
    // user, assistant(write), tool_result, assistant(read), tool_result, assistant(final)
    assert_eq!(
        history.len(),
        6,
        "expected 6 messages in history, got {}: {history:#?}",
        history.len()
    );
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[2].role, Role::Tool);
    assert_eq!(history[3].role, Role::Assistant);
    assert_eq!(history[4].role, Role::Tool);
    assert_eq!(history[5].role, Role::Assistant);

    assert_eq!(result, "The file contains: hello from e2e test");
}

/// Verifies terminal tool execution via the agent loop.
#[tokio::test]
async fn test_agent_terminal_execution() {
    let workspace = tempfile::TempDir::new().unwrap();

    let registry = ToolRegistry::new();
    registry.register(Box::new(TerminalTool));

    let provider = MockProvider::new(vec![
        tool_response(
            "Running command.",
            vec![make_call(
                "call_1",
                "terminal",
                serde_json::json!({ "command": "echo hello_e2e" }),
            )],
        ),
        stop_response("The output was hello_e2e."),
    ]);

    let mut agent = make_agent_with_approval(provider, registry, &workspace, |req| {
        let _ = req.response_tx.send(ApprovalDecision::Allow);
    });

    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![];
    let result = agent
        .run_conversation("run echo", &mut history, tx)
        .await
        .unwrap();

    // history: user, assistant(tool_call), tool_result, assistant(final) = 4
    assert_eq!(history.len(), 4);

    // The tool result (index 2) should contain "hello_e2e" and exit_code 0
    let tool_result_msg = &history[2];
    assert_eq!(tool_result_msg.role, Role::Tool);
    let content_str = match &tool_result_msg.content {
        Content::Text(s) => s.clone(),
        _ => panic!("expected text content"),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&content_str).expect("tool result should be valid JSON");
    assert!(
        parsed["output"]
            .as_str()
            .unwrap_or("")
            .contains("hello_e2e"),
        "expected 'hello_e2e' in output, got: {parsed}"
    );
    assert_eq!(parsed["exit_code"], 0);

    assert_eq!(result, "The output was hello_e2e.");
}

/// Verifies that an unknown tool name doesn't crash the agent — it returns an
/// error result and the loop continues normally.
#[tokio::test]
async fn test_agent_unknown_tool_graceful() {
    let workspace = tempfile::TempDir::new().unwrap();

    // Registry is empty — no tools registered
    let registry = ToolRegistry::new();

    let provider = MockProvider::new(vec![
        tool_response(
            "Calling unknown tool.",
            vec![make_call(
                "call_1",
                "nonexistent_tool",
                serde_json::json!({}),
            )],
        ),
        stop_response("I handled the error gracefully."),
    ]);

    let mut agent = make_agent_with_approval(provider, registry, &workspace, |req| {
        let _ = req.response_tx.send(ApprovalDecision::Allow);
    });

    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![];
    let result = agent
        .run_conversation("call a bad tool", &mut history, tx)
        .await
        .unwrap();

    // Must not panic — agent continues after unknown tool
    assert_eq!(result, "I handled the error gracefully.");
    // user, assistant(tool_call), tool_result(error), assistant(final) = 4
    assert_eq!(history.len(), 4);

    let tool_result_msg = &history[2];
    assert_eq!(tool_result_msg.role, Role::Tool);
    let content_str = match &tool_result_msg.content {
        Content::Text(s) => s.clone(),
        _ => panic!("expected text content"),
    };
    assert!(
        content_str.contains("unknown tool"),
        "expected 'unknown tool' in result, got: {content_str}"
    );
}

/// Two read_file calls in a single response → should_parallelize returns true
/// (both are read-only, no exclusive flag). Both results must appear in history.
#[tokio::test]
async fn test_agent_parallel_read_files() {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("a.txt"), "content_a").unwrap();
    std::fs::write(workspace.path().join("b.txt"), "content_b").unwrap();

    let a_path = workspace.path().join("a.txt");
    let b_path = workspace.path().join("b.txt");

    let registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool));

    let provider = MockProvider::new(vec![
        tool_response(
            "Reading both files.",
            vec![
                make_call(
                    "c1",
                    "read_file",
                    serde_json::json!({ "path": a_path.to_str().unwrap() }),
                ),
                make_call(
                    "c2",
                    "read_file",
                    serde_json::json!({ "path": b_path.to_str().unwrap() }),
                ),
            ],
        ),
        stop_response("Both files read."),
    ]);

    let mut agent = make_agent_with_approval(provider, registry, &workspace, |req| {
        let _ = req.response_tx.send(ApprovalDecision::Allow);
    });

    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![];
    let result = agent
        .run_conversation("read two files", &mut history, tx)
        .await
        .unwrap();

    // user, assistant(2 tool calls), tool_result_a, tool_result_b, assistant(final)
    assert_eq!(history.len(), 5, "expected 5 messages: {history:#?}");
    assert_eq!(history[2].role, Role::Tool);
    assert_eq!(history[3].role, Role::Tool);

    // Both tool results carry real file content
    let result_a = match &history[2].content {
        Content::Text(s) => s.clone(),
        _ => panic!("expected text"),
    };
    let result_b = match &history[3].content {
        Content::Text(s) => s.clone(),
        _ => panic!("expected text"),
    };
    let combined = format!("{result_a}{result_b}");
    assert!(
        combined.contains("content_a"),
        "expected 'content_a' in results: {combined}"
    );
    assert!(
        combined.contains("content_b"),
        "expected 'content_b' in results: {combined}"
    );

    assert_eq!(result, "Both files read.");
}

/// Two terminal calls in a single response: terminal is_exclusive=true, so
/// should_parallelize returns false. They run sequentially — both succeed.
#[tokio::test]
async fn test_agent_terminal_not_parallel() {
    let workspace = tempfile::TempDir::new().unwrap();

    let registry = ToolRegistry::new();
    registry.register(Box::new(TerminalTool));

    let provider = MockProvider::new(vec![
        tool_response(
            "Running two commands.",
            vec![
                make_call(
                    "cmd1",
                    "terminal",
                    serde_json::json!({ "command": "echo first" }),
                ),
                make_call(
                    "cmd2",
                    "terminal",
                    serde_json::json!({ "command": "echo second" }),
                ),
            ],
        ),
        stop_response("Both commands ran."),
    ]);

    let mut agent = make_agent_with_approval(provider, registry, &workspace, |req| {
        let _ = req.response_tx.send(ApprovalDecision::Allow);
    });

    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![];
    let result = agent
        .run_conversation("run two commands", &mut history, tx)
        .await
        .unwrap();

    // user, assistant(2 tool_calls), tool_result_1, tool_result_2, assistant(final) = 5
    assert_eq!(history.len(), 5, "expected 5 messages: {history:#?}");

    // Both tool results should have exit_code 0 and expected output
    for (i, expected_output) in [
        ("first", history[2].clone()),
        ("second", history[3].clone()),
    ] {
        let content_str = match &expected_output.content {
            Content::Text(s) => s.clone(),
            _ => panic!("expected text"),
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&content_str).expect("valid JSON from terminal tool");
        assert_eq!(
            parsed["exit_code"], 0,
            "expected exit_code 0 for '{i}': {parsed}"
        );
        assert!(
            parsed["output"].as_str().unwrap_or("").contains(i),
            "expected '{i}' in output: {parsed}"
        );
    }

    assert_eq!(result, "Both commands ran.");
}

#[tokio::test]
async fn test_agent_browser_tool_real_page_flow() {
    if std::env::var_os("GITHUB_ACTIONS").is_some()
        && std::env::var_os("HERMES_RUN_BROWSER_E2E").is_none()
    {
        eprintln!("skipping browser e2e on GitHub Actions; set HERMES_RUN_BROWSER_E2E=1 to enable");
        return;
    }

    let Some(executable) = browser_executable() else {
        eprintln!("skipping browser e2e: no Chromium executable found");
        return;
    };

    let workspace = tempfile::TempDir::new().unwrap();
    let (base_url, server_handle) = spawn_browser_test_server().await;

    let registry = ToolRegistry::new();
    registry.register(Box::new(BrowserTool::new()));

    let provider = MockProvider::new(vec![
        tool_response(
            "Open the page.",
            vec![make_call(
                "browser_1",
                "browser",
                serde_json::json!({ "action": "navigate", "url": base_url }),
            )],
        ),
        tool_response(
            "Type the name.",
            vec![make_call(
                "browser_2",
                "browser",
                serde_json::json!({
                    "action": "type",
                    "selector": "#name",
                    "text": "Hermes",
                    "clear": true
                }),
            )],
        ),
        tool_response(
            "Submit the form.",
            vec![make_call(
                "browser_3",
                "browser",
                serde_json::json!({
                    "action": "click",
                    "selector": "#go",
                    "wait_for_navigation": true
                }),
            )],
        ),
        tool_response(
            "Wait for the greeting.",
            vec![make_call(
                "browser_3b",
                "browser",
                serde_json::json!({
                    "action": "wait",
                    "selector": "#message",
                    "timeout_ms": 5000
                }),
            )],
        ),
        tool_response(
            "Read the page.",
            vec![make_call(
                "browser_4",
                "browser",
                serde_json::json!({
                    "action": "extract_text",
                    "selector": "#message"
                }),
            )],
        ),
        tool_response(
            "Close the session.",
            vec![make_call(
                "browser_5",
                "browser",
                serde_json::json!({ "action": "close" }),
            )],
        ),
        stop_response("The page says: Hello, Hermes!"),
    ]);

    let tool_config = ToolConfig {
        browser: BrowserToolConfig {
            executable: Some(executable),
            sandbox: false,
            action_timeout_secs: 10,
            launch_timeout_secs: 20,
            ..BrowserToolConfig::default()
        },
        workspace_root: workspace.path().to_path_buf(),
        ..ToolConfig::default()
    };

    let mut agent =
        make_agent_with_tool_config(provider, registry, &workspace, tool_config, |req| {
            let _ = req.response_tx.send(ApprovalDecision::Allow);
        });

    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![];
    let result = agent
        .run_conversation("fill the browser page", &mut history, tx)
        .await
        .unwrap();

    server_handle.abort();

    assert_eq!(result, "The page says: Hello, Hermes!");
    assert_eq!(history.len(), 14, "expected 14 messages: {history:#?}");

    let extract_result = match &history[10].content {
        Content::Text(s) => s.clone(),
        _ => panic!("expected text content"),
    };
    let parsed: serde_json::Value = serde_json::from_str(&extract_result).unwrap_or_else(|err| {
        panic!("browser extract result should be valid JSON ({err}): {extract_result}")
    });
    assert_eq!(parsed["content"].as_str(), Some("Hello, Hermes!"));

    let close_result = match &history[12].content {
        Content::Text(s) => s.clone(),
        _ => panic!("expected text content"),
    };
    let close_parsed: serde_json::Value =
        serde_json::from_str(&close_result).unwrap_or_else(|err| {
            panic!("browser close result should be valid JSON ({err}): {close_result}")
        });
    assert_eq!(close_parsed["closed"].as_bool(), Some(true));
}

/// When the approval handler denies a dangerous command (rm -rf /), the tool
/// result must be an error containing "denied" and the agent must continue.
#[tokio::test]
async fn test_agent_dangerous_command_denied() {
    let workspace = tempfile::TempDir::new().unwrap();

    let registry = ToolRegistry::new();
    registry.register(Box::new(TerminalTool));

    let provider = MockProvider::new(vec![
        tool_response(
            "Attempting dangerous command.",
            vec![make_call(
                "call_dangerous",
                "terminal",
                serde_json::json!({ "command": "rm -rf /" }),
            )],
        ),
        stop_response("The command was denied as expected."),
    ]);

    let mut agent = make_agent_with_approval(provider, registry, &workspace, |req| {
        // Always deny
        let _ = req.response_tx.send(ApprovalDecision::Deny);
    });

    let (tx, _rx) = mpsc::channel(64);
    let mut history = vec![];
    let result = agent
        .run_conversation("do something dangerous", &mut history, tx)
        .await
        .unwrap();

    // Agent must complete, not panic
    assert_eq!(result, "The command was denied as expected.");
    // user, assistant(tool_call), tool_result(denied), assistant(final) = 4
    assert_eq!(history.len(), 4);

    let tool_result_msg = &history[2];
    assert_eq!(tool_result_msg.role, Role::Tool);
    let content_str = match &tool_result_msg.content {
        Content::Text(s) => s.clone(),
        _ => panic!("expected text content"),
    };
    assert!(
        content_str.to_lowercase().contains("denied"),
        "expected 'denied' in tool result, got: {content_str}"
    );
}
