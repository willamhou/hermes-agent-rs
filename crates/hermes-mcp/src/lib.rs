//! MCP client (JSON-RPC over stdio) and tool adapters.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context as _;
use async_trait::async_trait;
use hermes_config::config::McpServerConfig;
use hermes_core::{
    error::{HermesError, Result},
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{Mutex, oneshot},
};

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

const JSONRPC_VERSION: &str = "2.0";
const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone)]
struct StdioMcpClient {
    server_name: String,
    _child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
}

impl StdioMcpClient {
    async fn connect(config: &McpServerConfig) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command.args(&config.args);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        if !config.env.is_empty() {
            command.envs(config.env.iter());
        }

        let mut child = command.spawn().map_err(|err| {
            HermesError::Mcp(format!(
                "failed to spawn MCP server '{}' ({}): {err}",
                config.name, config.command
            ))
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            HermesError::Mcp(format!("MCP server '{}' missing stdin", config.name))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            HermesError::Mcp(format!("MCP server '{}' missing stdout", config.name))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            HermesError::Mcp(format!("MCP server '{}' missing stderr", config.name))
        })?;

        let client = Self {
            server_name: config.name.clone(),
            _child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        };

        spawn_stdout_reader(config.name.clone(), stdout, Arc::clone(&client.pending));
        spawn_stderr_logger(config.name.clone(), stderr);

        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<()> {
        let result = self
            .call_method(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "hermes-rs",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;

        let negotiated = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(PROTOCOL_VERSION);
        tracing::info!(
            server = %self.server_name,
            protocol_version = negotiated,
            "initialized MCP server"
        );

        self.notify("notifications/initialized", json!({})).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_message(json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn call_method(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(err) = self
            .write_message(json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": id,
                "method": method,
                "params": params,
            }))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }

        let response = rx.await.map_err(|_| {
            HermesError::Mcp(format!(
                "MCP server '{}' closed before responding to {method}",
                self.server_name
            ))
        })?;

        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown MCP error");
            let code = error
                .get("code")
                .and_then(|v| v.as_i64())
                .unwrap_or_default();
            return Err(HermesError::Mcp(format!(
                "MCP server '{}' returned error for {method}: [{code}] {message}",
                self.server_name
            )));
        }

        response.get("result").cloned().ok_or_else(|| {
            HermesError::Mcp(format!(
                "MCP server '{}' returned no result",
                self.server_name
            ))
        })
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>> {
        let mut cursor: Option<String> = None;
        let mut tools = Vec::new();

        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.call_method("tools/list", params).await?;
            let page: ToolsListResult = serde_json::from_value(result).map_err(|err| {
                HermesError::Mcp(format!(
                    "failed to parse tools/list response from '{}': {err}",
                    self.server_name
                ))
            })?;
            tools.extend(page.tools);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(tools)
    }

    async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<ToolResult> {
        let result = self
            .call_method(
                "tools/call",
                json!({
                    "name": tool_name,
                    "arguments": arguments,
                }),
            )
            .await?;
        let response: ToolCallResult = serde_json::from_value(result).map_err(|err| {
            HermesError::Mcp(format!(
                "failed to parse tools/call response from '{}' for '{}': {err}",
                self.server_name, tool_name
            ))
        })?;

        let content = flatten_tool_content(&response.content);
        Ok(if response.is_error {
            ToolResult::error(content)
        } else {
            ToolResult::ok(content)
        })
    }

    async fn write_message(&self, payload: Value) -> Result<()> {
        let bytes = serde_json::to_vec(&payload)
            .map_err(|err| HermesError::Mcp(format!("failed to serialize MCP payload: {err}")))?;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
            .await
            .map_err(|err| {
                HermesError::Mcp(format!(
                    "failed writing MCP header to '{}': {err}",
                    self.server_name
                ))
            })?;
        stdin.write_all(&bytes).await.map_err(|err| {
            HermesError::Mcp(format!(
                "failed writing MCP body to '{}': {err}",
                self.server_name
            ))
        })?;
        stdin.flush().await.map_err(|err| {
            HermesError::Mcp(format!(
                "failed flushing MCP request to '{}': {err}",
                self.server_name
            ))
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct McpToolAdapter {
    server_name: String,
    descriptor: McpToolDescriptor,
    client: StdioMcpClient,
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.descriptor.name
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.descriptor.name.clone(),
            description: format!(
                "{} [MCP: {}]",
                self.descriptor
                    .description
                    .as_deref()
                    .unwrap_or("External MCP tool"),
                self.server_name
            ),
            parameters: self
                .descriptor
                .input_schema
                .clone()
                .unwrap_or_else(|| json!({"type": "object"})),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        self.client.call_tool(&self.descriptor.name, args).await
    }
}

pub async fn discover_tools(configs: &[McpServerConfig]) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();

    for config in configs.iter().filter(|config| config.enabled) {
        match StdioMcpClient::connect(config).await {
            Ok(client) => match client.list_tools().await {
                Ok(descriptors) => {
                    for descriptor in descriptors {
                        tools.push(Box::new(McpToolAdapter {
                            server_name: config.name.clone(),
                            descriptor,
                            client: client.clone(),
                        }) as Box<dyn Tool>);
                    }
                }
                Err(err) => {
                    tracing::warn!(server = %config.name, "failed to list MCP tools: {err}");
                }
            },
            Err(err) => {
                tracing::warn!(server = %config.name, "failed to connect MCP server: {err}");
            }
        }
    }

    tools
}

fn spawn_stdout_reader(server_name: String, stdout: ChildStdout, pending: PendingMap) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_stdio_message(&mut reader).await {
                Ok(Some(message)) => {
                    let Some(id) = message.get("id").and_then(|v| v.as_u64()) else {
                        continue;
                    };
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let _ = tx.send(message);
                    }
                }
                Ok(None) => {
                    tracing::info!(server = %server_name, "MCP stdout closed");
                    pending.lock().await.clear();
                    break;
                }
                Err(err) => {
                    tracing::warn!(server = %server_name, "failed to read MCP message: {err}");
                    pending.lock().await.clear();
                    break;
                }
            }
        }
    });
}

fn spawn_stderr_logger(server_name: String, stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let text = line.trim_end();
                    if !text.is_empty() {
                        tracing::warn!(server = %server_name, "mcp stderr: {text}");
                    }
                }
                Err(err) => {
                    tracing::warn!(server = %server_name, "failed reading MCP stderr: {err}");
                    break;
                }
            }
        }
    });
}

async fn read_stdio_message<R>(reader: &mut BufReader<R>) -> anyhow::Result<Option<Value>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Ok(None);
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(
                rest.trim()
                    .parse::<usize>()
                    .context("invalid Content-Length")?,
            );
        }
    }

    let content_length = content_length.context("missing Content-Length header")?;
    let mut payload = vec![0u8; content_length];
    reader.read_exact(&mut payload).await?;
    let message = serde_json::from_slice::<Value>(&payload).context("invalid MCP JSON payload")?;
    Ok(Some(message))
}

fn flatten_tool_content(parts: &[Value]) -> String {
    let chunks = parts
        .iter()
        .map(|part| match part.get("type").and_then(|v| v.as_str()) {
            Some("text") => part
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            Some("image") => {
                let mime_type = part
                    .get("mimeType")
                    .or_else(|| part.get("mime_type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("application/octet-stream");
                format!("[image:{mime_type}]")
            }
            Some("resource") => {
                let resource = part.get("resource").cloned().unwrap_or_else(|| json!({}));
                let uri = resource
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .unwrap_or("resource://unknown");
                if let Some(text) = resource.get("text").and_then(|v| v.as_str()) {
                    format!("{uri}\n{text}")
                } else {
                    let mime_type = resource
                        .get("mimeType")
                        .or_else(|| resource.get("mime_type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("application/octet-stream");
                    format!("[resource:{uri} ({mime_type})]")
                }
            }
            _ => part.to_string(),
        })
        .collect::<Vec<_>>();

    chunks.join("\n")
}

#[derive(Debug, Clone, Deserialize)]
struct ToolsListResult {
    tools: Vec<McpToolDescriptor>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpToolDescriptor {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolCallResult {
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_stdio_message_parses_content_length_frame() {
        let frame =
            b"Content-Length: 17\r\nContent-Type: application/json\r\n\r\n{\"jsonrpc\":\"2.0\"}";
        let cursor = std::io::Cursor::new(frame.to_vec());
        let mut reader = BufReader::new(cursor);

        let parsed = read_stdio_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
    }

    #[test]
    fn flatten_tool_content_joins_text_and_resources() {
        let parts = vec![
            json!({"type": "text", "text": "hello"}),
            json!({
                "type": "resource",
                "resource": {
                    "uri": "file:///tmp/out.txt",
                    "text": "world",
                    "mimeType": "text/plain"
                }
            }),
        ];

        let flattened = flatten_tool_content(&parts);
        assert!(flattened.contains("hello"));
        assert!(flattened.contains("file:///tmp/out.txt"));
        assert!(flattened.contains("world"));
    }

    #[test]
    fn tools_list_result_parses_next_cursor() {
        let payload = json!({
            "tools": [{
                "name": "search_docs",
                "description": "Search docs",
                "inputSchema": {"type": "object"}
            }],
            "nextCursor": "page-2"
        });

        let parsed: ToolsListResult = serde_json::from_value(payload).unwrap();
        assert_eq!(parsed.tools.len(), 1);
        assert_eq!(parsed.next_cursor.as_deref(), Some("page-2"));
    }
}
