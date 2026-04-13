//! MCP client (JSON-RPC over stdio) and tool adapters.

use std::{
    collections::HashMap,
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use hermes_config::config::{McpServerConfig, McpTransportKind};
use hermes_core::{
    error::{HermesError, Result},
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};
use hermes_tools::registry::ToolRegistry;
use reqwest::{
    Client,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{Mutex, broadcast, oneshot, watch},
};
use tokio_stream::Stream;
use tokio_util::io::StreamReader;

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

const JSONRPC_VERSION: &str = "2.0";
const PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_NOTIFICATION_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";
const MAX_RESOURCE_UPDATES: usize = 100;

#[derive(Debug, Clone)]
struct ResourceUpdateEvent {
    server_name: String,
    uri: Option<String>,
    payload: Value,
}

#[derive(Debug, Clone)]
enum McpClient {
    Stdio(StdioMcpClient),
    Http(HttpMcpClient),
}

#[derive(Debug, Clone, Default)]
struct McpCapabilities {
    tools: bool,
    prompts: bool,
    resources: bool,
    resource_subscribe: bool,
}

#[derive(Debug, Clone)]
struct StdioMcpClient {
    server_name: String,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
    capabilities: Arc<Mutex<McpCapabilities>>,
    refresh_tx: watch::Sender<u64>,
    resource_update_tx: broadcast::Sender<ResourceUpdateEvent>,
}

impl StdioMcpClient {
    async fn connect(config: &McpServerConfig) -> Result<Self> {
        if config.command.trim().is_empty() {
            return Err(HermesError::Mcp(format!(
                "MCP server '{}' is missing command for stdio transport",
                config.name
            )));
        }

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
            child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            capabilities: Arc::new(Mutex::new(McpCapabilities::default())),
            refresh_tx: watch::channel(0).0,
            resource_update_tx: broadcast::channel(64).0,
        };

        spawn_stdout_reader(
            config.name.clone(),
            stdout,
            Arc::clone(&client.stdin),
            Arc::clone(&client.pending),
            client.refresh_tx.clone(),
            client.resource_update_tx.clone(),
        );
        spawn_stderr_logger(config.name.clone(), stderr);

        if let Err(err) = client.initialize().await {
            client.shutdown().await;
            return Err(err);
        }
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
        *self.capabilities.lock().await = parse_capabilities(&result);
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

        let response = match tokio::time::timeout(MCP_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                return Err(HermesError::Mcp(format!(
                    "MCP server '{}' closed before responding to {method}",
                    self.server_name
                )));
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(HermesError::Mcp(format!(
                    "MCP server '{}' timed out waiting for {method} after {}s",
                    self.server_name,
                    MCP_REQUEST_TIMEOUT.as_secs()
                )));
            }
        };

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
        write_framed_message(&self.server_name, &self.stdin, &payload).await
    }

    async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(server = %self.server_name, "failed to inspect MCP child state: {err}");
            }
        }

        if let Err(err) = child.kill().await {
            tracing::warn!(server = %self.server_name, "failed to terminate MCP child: {err}");
        }
    }

    async fn capabilities(&self) -> McpCapabilities {
        self.capabilities.lock().await.clone()
    }

    fn subscribe_refresh(&self) -> watch::Receiver<u64> {
        self.refresh_tx.subscribe()
    }

    fn subscribe_resource_updates(&self) -> broadcast::Receiver<ResourceUpdateEvent> {
        self.resource_update_tx.subscribe()
    }
}

impl McpClient {
    async fn connect(config: &McpServerConfig) -> Result<Self> {
        match config.transport {
            McpTransportKind::Stdio => Ok(Self::Stdio(StdioMcpClient::connect(config).await?)),
            McpTransportKind::Http => Ok(Self::Http(HttpMcpClient::connect(config).await?)),
        }
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>> {
        match self {
            Self::Stdio(client) => client.list_tools().await,
            Self::Http(client) => client.list_tools().await,
        }
    }

    async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<ToolResult> {
        match self {
            Self::Stdio(client) => client.call_tool(tool_name, arguments).await,
            Self::Http(client) => client.call_tool(tool_name, arguments).await,
        }
    }

    async fn call_method(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Self::Stdio(client) => client.call_method(method, params).await,
            Self::Http(client) => client.call_method(method, params).await,
        }
    }

    async fn capabilities(&self) -> McpCapabilities {
        match self {
            Self::Stdio(client) => client.capabilities().await,
            Self::Http(client) => client.capabilities().await,
        }
    }

    async fn list_prompts(&self, cursor: Option<&str>) -> Result<Value> {
        let params = match cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        self.call_method("prompts/list", params).await
    }

    async fn get_prompt(&self, name: &str, arguments: Option<Value>) -> Result<Value> {
        let mut params = json!({ "name": name });
        if let Some(arguments) = arguments {
            params["arguments"] = arguments;
        }
        self.call_method("prompts/get", params).await
    }

    async fn list_resources(&self, cursor: Option<&str>) -> Result<Value> {
        let params = match cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        self.call_method("resources/list", params).await
    }

    async fn list_resource_templates(&self, cursor: Option<&str>) -> Result<Value> {
        let params = match cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        self.call_method("resources/templates/list", params).await
    }

    async fn read_resource(&self, uri: &str) -> Result<Value> {
        self.call_method("resources/read", json!({ "uri": uri }))
            .await
    }

    async fn subscribe_resource(&self, uri: &str) -> Result<Value> {
        self.call_method("resources/subscribe", json!({ "uri": uri }))
            .await
    }

    async fn unsubscribe_resource(&self, uri: &str) -> Result<Value> {
        self.call_method("resources/unsubscribe", json!({ "uri": uri }))
            .await
    }

    async fn shutdown(&self) {
        match self {
            Self::Stdio(client) => client.shutdown().await,
            Self::Http(client) => client.shutdown().await,
        }
    }

    fn subscribe_refresh(&self) -> watch::Receiver<u64> {
        match self {
            Self::Stdio(client) => client.subscribe_refresh(),
            Self::Http(client) => client.subscribe_refresh(),
        }
    }

    fn subscribe_resource_updates(&self) -> broadcast::Receiver<ResourceUpdateEvent> {
        match self {
            Self::Stdio(client) => client.subscribe_resource_updates(),
            Self::Http(client) => client.subscribe_resource_updates(),
        }
    }
}

#[derive(Debug, Clone)]
struct HttpMcpClient {
    server_name: String,
    endpoint: reqwest::Url,
    client: Client,
    session_id: Arc<Mutex<Option<String>>>,
    negotiated_protocol: Arc<Mutex<String>>,
    capabilities: Arc<Mutex<McpCapabilities>>,
    refresh_tx: watch::Sender<u64>,
    resource_update_tx: broadcast::Sender<ResourceUpdateEvent>,
    shutdown_tx: watch::Sender<bool>,
}

impl HttpMcpClient {
    async fn connect(config: &McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| {
                HermesError::Mcp(format!(
                    "MCP server '{}' is missing url for HTTP transport",
                    config.name
                ))
            })?;
        let endpoint = reqwest::Url::parse(url).map_err(|err| {
            HermesError::Mcp(format!(
                "failed to parse MCP server '{}' url '{}': {err}",
                config.name, url
            ))
        })?;
        let client = Client::builder()
            .timeout(MCP_REQUEST_TIMEOUT)
            .default_headers(build_http_headers(config)?)
            .build()
            .map_err(|err| {
                HermesError::Mcp(format!(
                    "failed to build HTTP client for MCP server '{}': {err}",
                    config.name
                ))
            })?;

        let http = Self {
            server_name: config.name.clone(),
            endpoint,
            client,
            session_id: Arc::new(Mutex::new(None)),
            negotiated_protocol: Arc::new(Mutex::new(PROTOCOL_VERSION.to_string())),
            capabilities: Arc::new(Mutex::new(McpCapabilities::default())),
            refresh_tx: watch::channel(0).0,
            resource_update_tx: broadcast::channel(64).0,
            shutdown_tx: watch::channel(false).0,
        };

        http.initialize().await?;
        http.start_notification_stream();
        Ok(http)
    }

    fn start_notification_stream(&self) {
        let client = self.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            client.run_notification_stream(&mut shutdown_rx).await;
        });
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
            .unwrap_or(PROTOCOL_VERSION)
            .to_string();
        *self.capabilities.lock().await = parse_capabilities(&result);
        *self.negotiated_protocol.lock().await = negotiated.clone();
        tracing::info!(
            server = %self.server_name,
            protocol_version = negotiated,
            "initialized MCP server over HTTP"
        );

        self.notify("notifications/initialized", json!({})).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_message(
            json!({
                "jsonrpc": JSONRPC_VERSION,
                "method": method,
                "params": params,
            }),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn call_method(&self, method: &str, params: Value) -> Result<Value> {
        let id = random_request_id();
        let response = self
            .send_message(
                json!({
                    "jsonrpc": JSONRPC_VERSION,
                    "id": id,
                    "method": method,
                    "params": params,
                }),
                Some(id),
            )
            .await?;

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

    async fn send_message(&self, payload: Value, expected_id: Option<u64>) -> Result<Value> {
        let response = self.post_payload(&payload).await?;

        self.maybe_store_session_id(response.headers()).await;

        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let trimmed = body.trim();
            let detail = if trimmed.is_empty() {
                format!("HTTP {}", status)
            } else {
                format!("HTTP {}: {}", status, trimmed)
            };
            return Err(HermesError::Mcp(format!(
                "MCP server '{}' request failed: {detail}",
                self.server_name
            )));
        }

        if expected_id.is_none() {
            return Ok(json!({ "jsonrpc": JSONRPC_VERSION, "result": {} }));
        }

        match content_type.as_deref() {
            Some(ct) if ct.starts_with("application/json") => {
                response.json::<Value>().await.map_err(|err| {
                    HermesError::Mcp(format!(
                        "failed to parse JSON response from MCP server '{}': {err}",
                        self.server_name
                    ))
                })
            }
            Some(ct) if ct.starts_with("text/event-stream") => {
                let body = response.text().await.map_err(|err| {
                    HermesError::Mcp(format!(
                        "failed to read SSE response from MCP server '{}': {err}",
                        self.server_name
                    ))
                })?;
                self.extract_response_from_sse(&body, expected_id).await
            }
            Some(other) => Err(HermesError::Mcp(format!(
                "MCP server '{}' returned unsupported content type '{other}'",
                self.server_name
            ))),
            None => Err(HermesError::Mcp(format!(
                "MCP server '{}' returned no content type",
                self.server_name
            ))),
        }
    }

    async fn consume_http_sse_stream<S>(
        &self,
        stream: S,
        expected_id: Option<u64>,
        shutdown_rx: &mut watch::Receiver<bool>,
    ) -> Result<Option<Value>>
    where
        S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
    {
        let mut sse = AsyncSseStream::new(stream);

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        return Ok(None);
                    }
                }
                event = sse.next_event() => {
                    match event {
                        Ok(Some(event)) => {
                            let message: Value = serde_json::from_str(&event.data).map_err(|err| {
                                HermesError::Mcp(format!(
                                    "failed to parse SSE payload from MCP server '{}': {err}",
                                    self.server_name
                                ))
                            })?;

                            if let Some(response) = self
                                .handle_http_inbound_message(message, expected_id)
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        Ok(None) => return Ok(None),
                        Err(err) => {
                            return Err(HermesError::Mcp(format!(
                                "failed to read SSE stream from MCP server '{}': {err}",
                                self.server_name
                            )));
                        }
                    }
                }
            }
        }
    }

    async fn handle_http_inbound_message(
        &self,
        message: Value,
        expected_id: Option<u64>,
    ) -> Result<Option<Value>> {
        let original = message.clone();
        match classify_inbound_message(message) {
            InboundMessage::Response { id, message } if expected_id == Some(id) => {
                Ok(Some(message))
            }
            InboundMessage::Response { .. } => Ok(None),
            InboundMessage::Request { id, method } => {
                if is_list_changed_notification(&method) {
                    emit_refresh_signal(&self.refresh_tx);
                    tracing::info!(server = %self.server_name, method = %method, "received MCP list_changed notification over HTTP");
                    return Ok(None);
                }
                if is_resource_updated_notification(&method) {
                    emit_resource_update_signal(
                        &self.resource_update_tx,
                        resource_update_event(&self.server_name, &original),
                    );
                    tracing::info!(server = %self.server_name, method = %method, "received MCP resource update notification over HTTP");
                    return Ok(None);
                }
                tracing::warn!(server = %self.server_name, method = %method, "ignoring unsupported inbound MCP method over HTTP");
                if let Some(id) = id {
                    self.reject_http_inbound_request(id, &method).await;
                }
                Ok(None)
            }
            InboundMessage::Other => Ok(None),
        }
    }

    async fn reject_http_inbound_request(&self, id: Value, method: &str) {
        let payload = json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("unsupported inbound MCP method '{method}'"),
            }
        });
        if let Err(err) = self.post_payload(&payload).await {
            tracing::warn!(server = %self.server_name, method = %method, "failed to reject unsupported inbound MCP method over HTTP: {err}");
        }
    }

    async fn run_notification_stream(&self, shutdown_rx: &mut watch::Receiver<bool>) {
        if self.session_id.lock().await.is_none() {
            tracing::debug!(server = %self.server_name, "skipping HTTP notification stream because the server did not issue an MCP session id");
            return;
        }

        loop {
            if *shutdown_rx.borrow() {
                return;
            }

            let response = match self.open_notification_stream().await {
                Ok(response) => response,
                Err(err) => {
                    tracing::warn!(server = %self.server_name, "failed to open HTTP notification stream: {err}");
                    if wait_for_notification_retry(shutdown_rx).await {
                        return;
                    }
                    continue;
                }
            };

            self.maybe_store_session_id(response.headers()).await;

            match classify_notification_stream_response(&self.server_name, &response) {
                NotificationStreamDisposition::Consume => {}
                NotificationStreamDisposition::Unsupported(reason) => {
                    tracing::info!(server = %self.server_name, "{reason}");
                    return;
                }
                NotificationStreamDisposition::Retry(reason) => {
                    tracing::warn!(server = %self.server_name, "{reason}");
                    if wait_for_notification_retry(shutdown_rx).await {
                        return;
                    }
                    continue;
                }
            }

            match self
                .consume_http_sse_stream(response.bytes_stream(), None, shutdown_rx)
                .await
            {
                Ok(None) if *shutdown_rx.borrow() => return,
                Ok(None) => {
                    tracing::info!(server = %self.server_name, "HTTP MCP notification stream closed; reconnecting");
                    if wait_for_notification_retry(shutdown_rx).await {
                        return;
                    }
                }
                Ok(Some(_)) => {}
                Err(err) => {
                    tracing::warn!(server = %self.server_name, "HTTP MCP notification stream failed: {err}");
                    if wait_for_notification_retry(shutdown_rx).await {
                        return;
                    }
                }
            }
        }
    }

    async fn open_notification_stream(&self) -> Result<reqwest::Response> {
        let mut request = self
            .client
            .get(self.endpoint.clone())
            .header(ACCEPT, "text/event-stream");

        let negotiated = self.negotiated_protocol.lock().await.clone();
        request = request.header(MCP_PROTOCOL_VERSION_HEADER, negotiated);

        if let Some(session_id) = self.session_id.lock().await.clone() {
            request = request.header(MCP_SESSION_ID_HEADER, session_id);
        }

        request.send().await.map_err(|err| {
            if err.is_timeout() {
                HermesError::Mcp(format!(
                    "MCP server '{}' timed out opening notification stream after {}s",
                    self.server_name,
                    MCP_REQUEST_TIMEOUT.as_secs()
                ))
            } else {
                HermesError::Mcp(format!(
                    "failed opening HTTP notification stream to MCP server '{}': {err}",
                    self.server_name
                ))
            }
        })
    }

    async fn extract_response_from_sse(
        &self,
        body: &str,
        expected_id: Option<u64>,
    ) -> Result<Value> {
        let stream = tokio_stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::copy_from_slice(
            body.as_bytes(),
        ))]);
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        self.consume_http_sse_stream(stream, expected_id, &mut shutdown_rx)
            .await?
            .ok_or_else(|| {
                HermesError::Mcp(format!(
                    "MCP server '{}' SSE response ended without reply to request {}",
                    self.server_name,
                    expected_id.unwrap_or_default()
                ))
            })
    }

    async fn maybe_store_session_id(&self, headers: &HeaderMap) {
        if let Some(value) = headers
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().await = Some(value.to_string());
        }
    }

    async fn post_payload(&self, payload: &Value) -> Result<reqwest::Response> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .json(payload);

        let negotiated = self.negotiated_protocol.lock().await.clone();
        request = request.header(MCP_PROTOCOL_VERSION_HEADER, negotiated);

        if let Some(session_id) = self.session_id.lock().await.clone() {
            request = request.header(MCP_SESSION_ID_HEADER, session_id);
        }

        request.send().await.map_err(|err| {
            if err.is_timeout() {
                HermesError::Mcp(format!(
                    "MCP server '{}' timed out after {}s",
                    self.server_name,
                    MCP_REQUEST_TIMEOUT.as_secs()
                ))
            } else {
                HermesError::Mcp(format!(
                    "failed sending HTTP request to MCP server '{}': {err}",
                    self.server_name
                ))
            }
        })
    }

    async fn capabilities(&self) -> McpCapabilities {
        self.capabilities.lock().await.clone()
    }

    async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    fn subscribe_refresh(&self) -> watch::Receiver<u64> {
        self.refresh_tx.subscribe()
    }

    fn subscribe_resource_updates(&self) -> broadcast::Receiver<ResourceUpdateEvent> {
        self.resource_update_tx.subscribe()
    }
}

#[derive(Debug, Clone)]
struct McpToolAdapter {
    server_name: String,
    descriptor: McpToolDescriptor,
    client: McpClient,
}

#[derive(Debug, Clone)]
struct McpServerDirectory {
    entries: Arc<HashMap<String, McpServerEntry>>,
}

#[derive(Debug, Clone)]
struct McpServerEntry {
    client: McpClient,
    transport: McpTransportKind,
    capabilities: McpCapabilities,
}

#[derive(Debug)]
struct McpRuntime {
    entries: HashMap<String, McpServerEntry>,
    tool_cache: Mutex<HashMap<String, Vec<McpToolDescriptor>>>,
    resource_updates: Arc<Mutex<Vec<ResourceUpdateEvent>>>,
}

impl McpServerDirectory {
    fn new(entries: HashMap<String, McpServerEntry>) -> Self {
        Self {
            entries: Arc::new(entries),
        }
    }

    fn has_prompt_support(&self) -> bool {
        self.entries
            .values()
            .any(|entry| entry.capabilities.prompts)
    }

    fn has_resource_support(&self) -> bool {
        self.entries
            .values()
            .any(|entry| entry.capabilities.resources)
    }

    fn has_resource_subscription_support(&self) -> bool {
        self.entries.values().any(|entry| {
            entry.capabilities.resource_subscribe && entry.transport == McpTransportKind::Stdio
        })
    }

    fn resolve_prompt_server(&self, requested: Option<&str>) -> Result<(String, McpClient)> {
        self.resolve_capability(requested, |entry| entry.capabilities.prompts, "prompts")
    }

    fn resolve_resource_server(&self, requested: Option<&str>) -> Result<(String, McpClient)> {
        self.resolve_capability(requested, |entry| entry.capabilities.resources, "resources")
    }

    fn resolve_resource_subscription_server(
        &self,
        requested: Option<&str>,
    ) -> Result<(String, McpClient)> {
        self.resolve_capability(
            requested,
            |entry| {
                entry.capabilities.resource_subscribe && entry.transport == McpTransportKind::Stdio
            },
            "resource subscriptions",
        )
    }

    fn resolve_capability<F>(
        &self,
        requested: Option<&str>,
        supports: F,
        capability_name: &str,
    ) -> Result<(String, McpClient)>
    where
        F: Fn(&McpServerEntry) -> bool,
    {
        if let Some(server) = requested {
            return self
                .entries
                .get(server)
                .filter(|entry| supports(entry))
                .cloned()
                .map(|entry| (server.to_string(), entry.client))
                .ok_or_else(|| {
                    HermesError::Mcp(format!(
                        "MCP server '{server}' does not support {capability_name}. Available servers: {}",
                        self.supporting_server_names(&supports).join(", ")
                    ))
                });
        }

        let supported = self.supporting_entries(&supports);
        match supported.len() {
            0 => Err(HermesError::Mcp(format!(
                "no MCP servers support {capability_name}"
            ))),
            1 => {
                let (name, entry) = supported.into_iter().next().expect("len checked");
                Ok((name, entry.client))
            }
            _ => Err(HermesError::Mcp(format!(
                "multiple MCP servers support {capability_name}; pass `server`. Available servers: {}",
                self.supporting_server_names(&supports).join(", ")
            ))),
        }
    }

    fn supporting_entries<F>(&self, supports: &F) -> Vec<(String, McpServerEntry)>
    where
        F: Fn(&McpServerEntry) -> bool,
    {
        let mut entries = self
            .entries
            .iter()
            .filter(|(_, entry)| supports(entry))
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect::<Vec<_>>();
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        entries
    }

    fn supporting_server_names<F>(&self, supports: &F) -> Vec<String>
    where
        F: Fn(&McpServerEntry) -> bool,
    {
        self.supporting_entries(supports)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }
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

#[derive(Debug, Clone)]
struct McpPromptListTool {
    servers: McpServerDirectory,
}

#[derive(Debug, Clone)]
struct McpPromptGetTool {
    servers: McpServerDirectory,
}

#[derive(Debug, Clone)]
struct McpResourceListTool {
    servers: McpServerDirectory,
}

#[derive(Debug, Clone)]
struct McpResourceTemplateListTool {
    servers: McpServerDirectory,
}

#[derive(Debug, Clone)]
struct McpResourceReadTool {
    servers: McpServerDirectory,
}

#[derive(Debug, Clone)]
struct McpResourceSubscribeTool {
    servers: McpServerDirectory,
}

#[derive(Debug, Clone)]
struct McpResourceUnsubscribeTool {
    servers: McpServerDirectory,
}

#[derive(Debug, Clone)]
struct McpResourceUpdatesTool {
    updates: Arc<Mutex<Vec<ResourceUpdateEvent>>>,
}

#[async_trait]
impl Tool for McpPromptListTool {
    fn name(&self) -> &str {
        "mcp_prompt_list"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_prompt_list".to_string(),
            description: "List prompts exposed by a configured MCP server. Use when the user wants to inspect or choose available MCP prompts.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name. Required when multiple MCP servers are configured."},
                    "cursor": {"type": "string", "description": "Pagination cursor from a previous result."}
                }
            }),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_prompt_server(optional_string_arg(&args, "server"))?;
        let result = client
            .list_prompts(optional_string_arg(&args, "cursor"))
            .await?;
        pretty_json_result(server_name, result)
    }
}

#[async_trait]
impl Tool for McpPromptGetTool {
    fn name(&self) -> &str {
        "mcp_prompt_get"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_prompt_get".to_string(),
            description: "Fetch one MCP prompt definition and its rendered messages. Use when the user explicitly wants to inspect or apply a specific MCP prompt.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name. Required when multiple MCP servers are configured."},
                    "name": {"type": "string", "description": "Prompt name."},
                    "arguments": {"type": "object", "description": "Optional prompt arguments."}
                },
                "required": ["name"]
            }),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_prompt_server(optional_string_arg(&args, "server"))?;
        let name = required_string_arg(&args, "name")?;
        let arguments = args.get("arguments").cloned();
        let result = client.get_prompt(name, arguments).await?;
        pretty_json_result(server_name, result)
    }
}

#[async_trait]
impl Tool for McpResourceListTool {
    fn name(&self) -> &str {
        "mcp_resource_list"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_resource_list".to_string(),
            description: "List resources exposed by a configured MCP server.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name. Required when multiple MCP servers are configured."},
                    "cursor": {"type": "string", "description": "Pagination cursor from a previous result."}
                }
            }),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_server(optional_string_arg(&args, "server"))?;
        let result = client
            .list_resources(optional_string_arg(&args, "cursor"))
            .await?;
        pretty_json_result(server_name, result)
    }
}

#[async_trait]
impl Tool for McpResourceTemplateListTool {
    fn name(&self) -> &str {
        "mcp_resource_template_list"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_resource_template_list".to_string(),
            description: "List resource templates exposed by a configured MCP server.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name. Required when multiple MCP servers are configured."},
                    "cursor": {"type": "string", "description": "Pagination cursor from a previous result."}
                }
            }),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_server(optional_string_arg(&args, "server"))?;
        let result = client
            .list_resource_templates(optional_string_arg(&args, "cursor"))
            .await?;
        pretty_json_result(server_name, result)
    }
}

#[async_trait]
impl Tool for McpResourceReadTool {
    fn name(&self) -> &str {
        "mcp_resource_read"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_resource_read".to_string(),
            description: "Read one resource from a configured MCP server.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name. Required when multiple MCP servers are configured."},
                    "uri": {"type": "string", "description": "Resource URI to read."}
                },
                "required": ["uri"]
            }),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_server(optional_string_arg(&args, "server"))?;
        let uri = required_string_arg(&args, "uri")?;
        let result = client.read_resource(uri).await?;
        pretty_json_result(server_name, result)
    }
}

#[async_trait]
impl Tool for McpResourceSubscribeTool {
    fn name(&self) -> &str {
        "mcp_resource_subscribe"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_resource_subscribe".to_string(),
            description: "Subscribe to update notifications for one MCP resource.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name. Required when multiple MCP servers are configured."},
                    "uri": {"type": "string", "description": "Resource URI to subscribe to."}
                },
                "required": ["uri"]
            }),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_subscription_server(optional_string_arg(&args, "server"))?;
        let uri = required_string_arg(&args, "uri")?;
        let result = client.subscribe_resource(uri).await?;
        pretty_json_result(server_name, result)
    }
}

#[async_trait]
impl Tool for McpResourceUnsubscribeTool {
    fn name(&self) -> &str {
        "mcp_resource_unsubscribe"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_resource_unsubscribe".to_string(),
            description: "Stop update notifications for one MCP resource.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name. Required when multiple MCP servers are configured."},
                    "uri": {"type": "string", "description": "Resource URI to unsubscribe from."}
                },
                "required": ["uri"]
            }),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_subscription_server(optional_string_arg(&args, "server"))?;
        let uri = required_string_arg(&args, "uri")?;
        let result = client.unsubscribe_resource(uri).await?;
        pretty_json_result(server_name, result)
    }
}

#[async_trait]
impl Tool for McpResourceUpdatesTool {
    fn name(&self) -> &str {
        "mcp_resource_updates"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_resource_updates".to_string(),
            description:
                "List recent MCP resource update notifications received from configured servers."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "Optional MCP server name filter."},
                    "limit": {"type": "integer", "description": "Maximum number of recent updates to return. Defaults to 20."},
                    "clear": {"type": "boolean", "description": "When true, remove the returned updates from the in-memory buffer."}
                }
            }),
        }
    }

    fn toolset(&self) -> &str {
        "mcp"
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let server = optional_string_arg(&args, "server");
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(20)
            .min(MAX_RESOURCE_UPDATES as u64) as usize;
        let clear = args
            .get("clear")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        let mut updates = self.updates.lock().await;
        let matching_indices = updates
            .iter()
            .enumerate()
            .filter(|(_, update)| server.is_none_or(|requested| update.server_name == requested))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();

        let selected_indices = matching_indices
            .into_iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>();

        let selected = selected_indices
            .iter()
            .rev()
            .map(|index| updates[*index].clone())
            .collect::<Vec<_>>();

        if clear {
            for index in selected_indices.into_iter().rev() {
                updates.remove(index);
            }
        }

        let count = selected.len();
        let result = json!({
            "updates": selected.into_iter().map(resource_update_to_json).collect::<Vec<_>>(),
            "count": count,
        });
        Ok(ToolResult::ok(
            serde_json::to_string_pretty(&result).map_err(|err| {
                HermesError::Mcp(format!("failed to render resource updates JSON: {err}"))
            })?,
        ))
    }
}

impl McpRuntime {
    async fn connect(configs: &[McpServerConfig]) -> Self {
        let mut entries: HashMap<String, McpServerEntry> = HashMap::new();
        let mut tool_cache: HashMap<String, Vec<McpToolDescriptor>> = HashMap::new();

        for config in configs.iter().filter(|config| config.enabled) {
            match McpClient::connect(config).await {
                Ok(client) => {
                    let capabilities = client.capabilities().await;
                    let keep_server =
                        capabilities.prompts || capabilities.resources || capabilities.tools;
                    let mut descriptors = Vec::new();

                    if capabilities.tools {
                        match client.list_tools().await {
                            Ok(found) => {
                                if found.is_empty() {
                                    tracing::info!(server = %config.name, "MCP server reported no model-callable tools");
                                }
                                descriptors = found;
                            }
                            Err(err) => {
                                tracing::warn!(server = %config.name, "failed to list MCP tools: {err}");
                            }
                        }
                    } else {
                        tracing::info!(server = %config.name, "MCP server does not advertise model-callable tools");
                    }

                    if keep_server {
                        entries.insert(
                            config.name.clone(),
                            McpServerEntry {
                                client: client.clone(),
                                transport: config.transport.clone(),
                                capabilities,
                            },
                        );
                        tool_cache.insert(config.name.clone(), descriptors);
                    } else {
                        client.shutdown().await;
                    }
                }
                Err(err) => {
                    tracing::warn!(server = %config.name, "failed to connect MCP server: {err}");
                }
            }
        }

        Self {
            entries,
            tool_cache: Mutex::new(tool_cache),
            resource_updates: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn build_mcp_tools(&self) -> Vec<Box<dyn Tool>> {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        let cache = self.tool_cache.lock().await.clone();

        for (server_name, entry) in &self.entries {
            for descriptor in cache.get(server_name).cloned().unwrap_or_default() {
                tools.push(Box::new(McpToolAdapter {
                    server_name: server_name.clone(),
                    descriptor,
                    client: entry.client.clone(),
                }) as Box<dyn Tool>);
            }
        }

        let server_directory = McpServerDirectory::new(self.entries.clone());
        if server_directory.has_prompt_support() {
            tools.push(Box::new(McpPromptListTool {
                servers: server_directory.clone(),
            }));
            tools.push(Box::new(McpPromptGetTool {
                servers: server_directory.clone(),
            }));
        }
        if server_directory.has_resource_support() {
            tools.push(Box::new(McpResourceListTool {
                servers: server_directory.clone(),
            }));
            tools.push(Box::new(McpResourceTemplateListTool {
                servers: server_directory.clone(),
            }));
            tools.push(Box::new(McpResourceReadTool {
                servers: server_directory.clone(),
            }));
        }
        if server_directory.has_resource_subscription_support() {
            tools.push(Box::new(McpResourceSubscribeTool {
                servers: server_directory.clone(),
            }));
            tools.push(Box::new(McpResourceUnsubscribeTool {
                servers: server_directory,
            }));
        }
        if self
            .entries
            .values()
            .any(|entry| entry.capabilities.resources)
        {
            tools.push(Box::new(McpResourceUpdatesTool {
                updates: Arc::clone(&self.resource_updates),
            }));
        }

        tools
    }

    async fn refresh_server(&self, server_name: &str) -> Result<()> {
        let Some(entry) = self.entries.get(server_name) else {
            return Ok(());
        };

        if !entry.capabilities.tools {
            return Ok(());
        }

        let descriptors = entry.client.list_tools().await?;
        self.tool_cache
            .lock()
            .await
            .insert(server_name.to_string(), descriptors);
        Ok(())
    }

    fn spawn_refresh_tasks(self: Arc<Self>, registry: Arc<ToolRegistry>) {
        for (server_name, entry) in &self.entries {
            if !entry.capabilities.tools {
                continue;
            }

            let server_name = server_name.clone();
            let mut refresh_rx = entry.client.subscribe_refresh();
            let runtime = Arc::clone(&self);
            let registry = Arc::clone(&registry);

            tokio::spawn(async move {
                while refresh_rx.changed().await.is_ok() {
                    tracing::info!(server = %server_name, "refreshing MCP tool registry after notification");
                    match runtime.refresh_server(&server_name).await {
                        Ok(()) => {
                            let mcp_tools = runtime.build_mcp_tools().await;
                            registry.replace_toolset("mcp", mcp_tools);
                        }
                        Err(err) => {
                            tracing::warn!(server = %server_name, "failed to refresh MCP tools: {err}");
                        }
                    }
                }
            });
        }
    }

    fn spawn_resource_update_tasks(self: Arc<Self>) {
        for entry in self.entries.values() {
            if !entry.capabilities.resources {
                continue;
            }

            let runtime = Arc::clone(&self);
            let mut updates_rx = entry.client.subscribe_resource_updates();
            tokio::spawn(async move {
                loop {
                    match updates_rx.recv().await {
                        Ok(update) => runtime.record_resource_update(update).await,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "lagged while receiving MCP resource updates");
                        }
                    }
                }
            });
        }
    }

    async fn record_resource_update(&self, update: ResourceUpdateEvent) {
        let mut updates = self.resource_updates.lock().await;
        updates.push(update);
        if updates.len() > MAX_RESOURCE_UPDATES {
            let overflow = updates.len() - MAX_RESOURCE_UPDATES;
            updates.drain(0..overflow);
        }
    }
}

pub async fn populate_registry(registry: Arc<ToolRegistry>, configs: &[McpServerConfig]) {
    let runtime = Arc::new(McpRuntime::connect(configs).await);
    let mcp_tools = runtime.build_mcp_tools().await;
    registry.replace_toolset("mcp", mcp_tools);
    Arc::clone(&runtime).spawn_refresh_tasks(registry);
    runtime.spawn_resource_update_tasks();
}

pub async fn discover_tools(configs: &[McpServerConfig]) -> Vec<Box<dyn Tool>> {
    let runtime = McpRuntime::connect(configs).await;
    runtime.build_mcp_tools().await
}

fn spawn_stdout_reader(
    server_name: String,
    stdout: ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    refresh_tx: watch::Sender<u64>,
    resource_update_tx: broadcast::Sender<ResourceUpdateEvent>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_stdio_message(&mut reader).await {
                Ok(Some(message)) => {
                    let original = message.clone();
                    match classify_inbound_message(message) {
                        InboundMessage::Response { id, message } => {
                            if let Some(tx) = pending.lock().await.remove(&id) {
                                let _ = tx.send(message);
                            }
                        }
                        InboundMessage::Request { id, method } => {
                            if is_list_changed_notification(&method) {
                                emit_refresh_signal(&refresh_tx);
                                tracing::info!(server = %server_name, method = %method, "received MCP list_changed notification");
                                continue;
                            }
                            if is_resource_updated_notification(&method) {
                                emit_resource_update_signal(
                                    &resource_update_tx,
                                    resource_update_event(&server_name, &original),
                                );
                                tracing::info!(server = %server_name, method = %method, "received MCP resource update notification");
                                continue;
                            }
                            tracing::warn!(server = %server_name, method = %method, "ignoring unsupported inbound MCP method");
                            if let Some(id) = id {
                                let payload = json!({
                                    "jsonrpc": JSONRPC_VERSION,
                                    "id": id,
                                    "error": {
                                        "code": -32601,
                                        "message": format!("unsupported inbound MCP method '{method}'"),
                                    }
                                });
                                if let Err(err) =
                                    write_framed_message(&server_name, &stdin, &payload).await
                                {
                                    tracing::warn!(server = %server_name, method = %method, "failed to reject unsupported inbound MCP method: {err}");
                                }
                            }
                        }
                        InboundMessage::Other => {
                            tracing::debug!(server = %server_name, "ignoring non-response MCP message");
                        }
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

async fn write_framed_message(
    server_name: &str,
    stdin: &Arc<Mutex<ChildStdin>>,
    payload: &Value,
) -> Result<()> {
    let bytes = serde_json::to_vec(payload)
        .map_err(|err| HermesError::Mcp(format!("failed to serialize MCP payload: {err}")))?;
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await
        .map_err(|err| {
            HermesError::Mcp(format!(
                "failed writing MCP header to '{}': {err}",
                server_name
            ))
        })?;
    stdin.write_all(&bytes).await.map_err(|err| {
        HermesError::Mcp(format!(
            "failed writing MCP body to '{}': {err}",
            server_name
        ))
    })?;
    stdin.flush().await.map_err(|err| {
        HermesError::Mcp(format!(
            "failed flushing MCP request to '{}': {err}",
            server_name
        ))
    })?;
    Ok(())
}

fn build_http_headers(config: &McpServerConfig) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in &config.headers {
        let header_name = HeaderName::try_from(name.as_str()).map_err(|err| {
            HermesError::Mcp(format!(
                "invalid HTTP header name '{}' for MCP server '{}': {err}",
                name, config.name
            ))
        })?;
        let header_value = HeaderValue::from_str(value).map_err(|err| {
            HermesError::Mcp(format!(
                "invalid HTTP header value for '{}' on MCP server '{}': {err}",
                name, config.name
            ))
        })?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

fn random_request_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1_000_000);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn emit_refresh_signal(refresh_tx: &watch::Sender<u64>) {
    let next = *refresh_tx.borrow() + 1;
    let _ = refresh_tx.send(next);
}

fn emit_resource_update_signal(
    resource_update_tx: &broadcast::Sender<ResourceUpdateEvent>,
    update: Option<ResourceUpdateEvent>,
) {
    if let Some(update) = update {
        let _ = resource_update_tx.send(update);
    }
}

fn parse_capabilities(result: &Value) -> McpCapabilities {
    let capabilities = result
        .get("capabilities")
        .and_then(|value| value.as_object());

    McpCapabilities {
        tools: capabilities.is_some_and(|caps| caps.contains_key("tools")),
        prompts: capabilities.is_some_and(|caps| caps.contains_key("prompts")),
        resources: capabilities.is_some_and(|caps| caps.contains_key("resources")),
        resource_subscribe: capabilities
            .and_then(|caps| caps.get("resources"))
            .and_then(|value| value.get("subscribe"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

fn is_list_changed_notification(method: &str) -> bool {
    matches!(
        method,
        "notifications/tools/list_changed"
            | "notifications/prompts/list_changed"
            | "notifications/resources/list_changed"
    )
}

fn is_resource_updated_notification(method: &str) -> bool {
    matches!(method, "notifications/resources/updated")
}

fn resource_update_event(server_name: &str, message: &Value) -> Option<ResourceUpdateEvent> {
    if !matches!(
        message.get("method").and_then(|value| value.as_str()),
        Some("notifications/resources/updated")
    ) {
        return None;
    }

    let payload = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let uri = payload
        .get("uri")
        .or_else(|| payload.get("resource").and_then(|value| value.get("uri")))
        .and_then(|value| value.as_str())
        .map(str::to_string);

    Some(ResourceUpdateEvent {
        server_name: server_name.to_string(),
        uri,
        payload,
    })
}

fn resource_update_to_json(update: ResourceUpdateEvent) -> Value {
    json!({
        "server": update.server_name,
        "uri": update.uri,
        "params": update.payload,
    })
}

enum NotificationStreamDisposition {
    Consume,
    Retry(String),
    Unsupported(String),
}

fn classify_notification_stream_response(
    server_name: &str,
    response: &reqwest::Response,
) -> NotificationStreamDisposition {
    use reqwest::StatusCode;

    match response.status() {
        StatusCode::OK => {}
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND | StatusCode::NOT_IMPLEMENTED => {
            return NotificationStreamDisposition::Unsupported(format!(
                "HTTP MCP notification stream is not supported by '{server_name}' (status {})",
                response.status()
            ));
        }
        status => {
            return NotificationStreamDisposition::Retry(format!(
                "HTTP MCP notification stream request failed for '{server_name}' with status {status}"
            ));
        }
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type.starts_with("text/event-stream") {
        return NotificationStreamDisposition::Unsupported(format!(
            "HTTP MCP notification stream for '{server_name}' returned unsupported content type '{content_type}'"
        ));
    }

    NotificationStreamDisposition::Consume
}

async fn wait_for_notification_retry(shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        changed = shutdown_rx.changed() => changed.is_ok() && *shutdown_rx.borrow(),
        _ = tokio::time::sleep(HTTP_NOTIFICATION_RECONNECT_DELAY) => false,
    }
}

fn optional_string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|value| value.as_str())
}

fn required_string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    optional_string_arg(args, key)
        .ok_or_else(|| HermesError::Mcp(format!("missing required MCP parameter: {key}")))
}

fn pretty_json_result(server_name: String, result: Value) -> Result<ToolResult> {
    let wrapped = json!({
        "server": server_name,
        "result": result,
    });
    let rendered = serde_json::to_string_pretty(&wrapped)
        .map_err(|err| HermesError::Mcp(format!("failed to render MCP response JSON: {err}")))?;
    Ok(ToolResult::ok(rendered))
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SseEvent {
    event: Option<String>,
    data: String,
}

struct MapSseStream<S>(S);

impl<S> Stream for MapSseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = io::Result<Bytes>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<io::Result<Bytes>>> {
        use std::pin::Pin;

        Pin::new(&mut self.0)
            .poll_next(cx)
            .map(|opt| opt.map(|res| res.map_err(io::Error::other)))
    }
}

struct AsyncSseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    reader: BufReader<StreamReader<MapSseStream<S>, Bytes>>,
    current_event: Option<String>,
    data_buf: Vec<String>,
}

impl<S> AsyncSseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    fn new(stream: S) -> Self {
        Self {
            reader: BufReader::new(StreamReader::new(MapSseStream(stream))),
            current_event: None,
            data_buf: Vec::new(),
        }
    }

    async fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(self.flush_event());
            }

            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if let Some(event) = self.flush_event() {
                    return Ok(Some(event));
                }
            } else if let Some(value) = line.strip_prefix("event:") {
                self.current_event = Some(value.trim_start().to_owned());
            } else if let Some(value) = line.strip_prefix("data:") {
                let value = value.trim_start();
                if value == "[DONE]" {
                    return Ok(None);
                }
                self.data_buf.push(value.to_owned());
            }
        }
    }

    fn flush_event(&mut self) -> Option<SseEvent> {
        if self.data_buf.is_empty() {
            self.current_event = None;
            return None;
        }
        let event = self.current_event.take();
        let data = self.data_buf.join("\n");
        self.data_buf.clear();
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
fn parse_sse_events(raw: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut current_event: Option<String> = None;
    let mut data_buf: Vec<String> = Vec::new();

    for line in raw.lines() {
        if line.is_empty() {
            if !data_buf.is_empty() {
                events.push(SseEvent {
                    event: current_event.take(),
                    data: data_buf.join("\n"),
                });
                data_buf.clear();
            } else {
                current_event = None;
            }
            continue;
        }

        if let Some(value) = line.strip_prefix("event:") {
            current_event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.trim_start();
            if value == "[DONE]" {
                break;
            }
            data_buf.push(value.to_owned());
        }
    }

    if !data_buf.is_empty() {
        events.push(SseEvent {
            event: current_event,
            data: data_buf.join("\n"),
        });
    }

    events
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

enum InboundMessage {
    Response { id: u64, message: Value },
    Request { id: Option<Value>, method: String },
    Other,
}

fn classify_inbound_message(message: Value) -> InboundMessage {
    if let Some(method) = message.get("method").and_then(|value| value.as_str()) {
        return InboundMessage::Request {
            id: message.get("id").cloned(),
            method: method.to_string(),
        };
    }

    if let Some(id) = message.get("id").and_then(|value| value.as_u64()) {
        if message.get("result").is_some() || message.get("error").is_some() {
            return InboundMessage::Response { id, message };
        }
    }

    InboundMessage::Other
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

    fn test_http_client(server_name: &str) -> HttpMcpClient {
        HttpMcpClient {
            server_name: server_name.to_string(),
            endpoint: reqwest::Url::parse("https://example.com/mcp").unwrap(),
            client: Client::new(),
            session_id: Arc::new(Mutex::new(None)),
            negotiated_protocol: Arc::new(Mutex::new(PROTOCOL_VERSION.to_string())),
            capabilities: Arc::new(Mutex::new(McpCapabilities::default())),
            refresh_tx: watch::channel(0).0,
            resource_update_tx: broadcast::channel(64).0,
            shutdown_tx: watch::channel(false).0,
        }
    }

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

    #[test]
    fn classify_inbound_message_treats_server_request_as_request() {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "sampling/createMessage",
            "params": {}
        });

        match classify_inbound_message(payload) {
            InboundMessage::Request { id, method } => {
                assert_eq!(id, Some(json!(7)));
                assert_eq!(method, "sampling/createMessage");
            }
            _ => panic!("expected request classification"),
        }
    }

    #[test]
    fn classify_inbound_message_treats_result_as_response() {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {"ok": true}
        });

        match classify_inbound_message(payload.clone()) {
            InboundMessage::Response { id, message } => {
                assert_eq!(id, 3);
                assert_eq!(message, payload);
            }
            _ => panic!("expected response classification"),
        }
    }

    #[test]
    fn parse_sse_events_joins_multiline_data() {
        let raw = "event: message\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":1}\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "{\"jsonrpc\":\"2.0\",\n\"id\":1}");
    }

    #[tokio::test]
    async fn http_sse_list_changed_notification_triggers_refresh() {
        let client = test_http_client("docs");
        let mut refresh_rx = client.subscribe_refresh();
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\",\"params\":{}}\n\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
        );

        let response = client
            .extract_response_from_sse(body, Some(1))
            .await
            .unwrap();

        assert_eq!(response["result"]["ok"], true);
        refresh_rx.changed().await.unwrap();
        assert_eq!(*refresh_rx.borrow(), 1);
    }

    #[tokio::test]
    async fn http_sse_resource_updated_notification_is_accepted() {
        let client = test_http_client("docs");
        let mut resource_update_rx = client.resource_update_tx.subscribe();
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/resources/updated\",\"params\":{\"uri\":\"file:///tmp/doc.txt\"}}\n\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
        );

        let response = client
            .extract_response_from_sse(body, Some(1))
            .await
            .unwrap();

        assert_eq!(response["result"]["ok"], true);
        let update = resource_update_rx.recv().await.unwrap();
        assert_eq!(update.server_name, "docs");
        assert_eq!(update.uri.as_deref(), Some("file:///tmp/doc.txt"));
    }

    #[tokio::test]
    async fn async_http_sse_stream_handles_chunked_notifications() {
        let client = test_http_client("docs");
        let mut refresh_rx = client.subscribe_refresh();
        let mut resource_update_rx = client.resource_update_tx.subscribe();
        let mut shutdown_rx = client.shutdown_tx.subscribe();
        let stream = tokio_stream::iter(vec![
            Ok::<Bytes, reqwest::Error>(Bytes::from_static(
                b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/",
            )),
            Ok::<Bytes, reqwest::Error>(Bytes::from_static(
                b"list_changed\",\"params\":{}}\n\n",
            )),
            Ok::<Bytes, reqwest::Error>(Bytes::from_static(
                b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/resources/updated\",\"params\":{\"uri\":\"file:///tmp/doc.txt\"}}\n\n",
            )),
            Ok::<Bytes, reqwest::Error>(Bytes::from_static(
                b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
            )),
        ]);

        let response = client
            .consume_http_sse_stream(stream, Some(1), &mut shutdown_rx)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(response["result"]["ok"], true);
        refresh_rx.changed().await.unwrap();
        let update = resource_update_rx.recv().await.unwrap();
        assert_eq!(*refresh_rx.borrow(), 1);
        assert_eq!(update.uri.as_deref(), Some("file:///tmp/doc.txt"));
    }

    #[test]
    fn resource_update_event_extracts_uri_from_params() {
        let message = json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/updated",
            "params": {"uri": "file:///tmp/doc.txt"}
        });

        let update = resource_update_event("docs", &message).unwrap();

        assert_eq!(update.server_name, "docs");
        assert_eq!(update.uri.as_deref(), Some("file:///tmp/doc.txt"));
    }

    #[test]
    fn server_directory_requires_server_when_multiple() {
        let directory = McpServerDirectory::new(HashMap::from([
            (
                "docs".to_string(),
                McpServerEntry {
                    client: McpClient::Http(test_http_client("docs")),
                    transport: McpTransportKind::Http,
                    capabilities: McpCapabilities {
                        prompts: true,
                        ..McpCapabilities::default()
                    },
                },
            ),
            (
                "files".to_string(),
                McpServerEntry {
                    client: McpClient::Http(test_http_client("files")),
                    transport: McpTransportKind::Http,
                    capabilities: McpCapabilities {
                        prompts: true,
                        ..McpCapabilities::default()
                    },
                },
            ),
        ]));

        let err = directory
            .resolve_prompt_server(None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple MCP servers support prompts"));
        assert!(err.contains("docs"));
        assert!(err.contains("files"));
    }

    #[test]
    fn server_directory_uses_only_server_without_explicit_name() {
        let directory = McpServerDirectory::new(HashMap::from([(
            "docs".to_string(),
            McpServerEntry {
                client: McpClient::Http(test_http_client("docs")),
                transport: McpTransportKind::Http,
                capabilities: McpCapabilities {
                    prompts: true,
                    ..McpCapabilities::default()
                },
            },
        )]));

        let (name, _) = directory.resolve_prompt_server(None).unwrap();
        assert_eq!(name, "docs");
    }

    #[test]
    fn server_directory_resolves_resource_subscription_support() {
        let directory = McpServerDirectory::new(HashMap::from([(
            "docs".to_string(),
            McpServerEntry {
                client: McpClient::Http(test_http_client("docs")),
                transport: McpTransportKind::Stdio,
                capabilities: McpCapabilities {
                    resources: true,
                    resource_subscribe: true,
                    ..McpCapabilities::default()
                },
            },
        )]));

        let (name, _) = directory
            .resolve_resource_subscription_server(None)
            .unwrap();
        assert_eq!(name, "docs");
    }

    #[test]
    fn resource_subscription_support_ignores_http_servers() {
        let directory = McpServerDirectory::new(HashMap::from([(
            "docs".to_string(),
            McpServerEntry {
                client: McpClient::Http(test_http_client("docs")),
                transport: McpTransportKind::Http,
                capabilities: McpCapabilities {
                    resources: true,
                    resource_subscribe: true,
                    ..McpCapabilities::default()
                },
            },
        )]));

        assert!(!directory.has_resource_subscription_support());
        assert!(
            directory
                .resolve_resource_subscription_server(None)
                .is_err()
        );
    }

    #[test]
    fn parse_capabilities_reads_prompt_and_resource_flags() {
        let payload = json!({
            "capabilities": {
                "prompts": {"listChanged": true},
                "resources": {"subscribe": true}
            }
        });

        let parsed = parse_capabilities(&payload);
        assert!(!parsed.tools);
        assert!(parsed.prompts);
        assert!(parsed.resources);
        assert!(parsed.resource_subscribe);
    }
}
