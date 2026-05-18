//! MCP client (JSON-RPC over stdio) and tool adapters.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use hermes_config::{
    config::{McpServerConfig, McpTransportKind},
    hermes_home,
};
use hermes_core::{
    error::{HermesError, Result},
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};
use hermes_tools::registry::ToolRegistry;
use hermes_tools::session_cleanup::{
    self, DurableCleanupExecutor, DurableCleanupResource, DurableCleanupResourceKind,
    SessionCleanupRegistration,
};
use reqwest::{
    Client,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{Mutex, broadcast, oneshot, watch},
    task::JoinHandle,
};
use tokio_rusqlite::Connection;
use tokio_stream::Stream;
use tokio_util::io::StreamReader;
use uuid::Uuid;

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

const JSONRPC_VERSION: &str = "2.0";
const PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_NOTIFICATION_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";
const MAX_RESOURCE_UPDATES: usize = 100;
const MCP_RUNTIME_WORKER_LEASE_INTERVAL_SECS: u64 = 10;
const MCP_RUNTIME_WORKER_LEASE_SECS: i64 = 45;
const MCP_RUNTIME_RECOVERY_BATCH_SIZE: usize = 128;
const MCP_RUNTIME_STORE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mcp_runtime_workers (
    worker_id TEXT PRIMARY KEY,
    started_at TEXT NOT NULL,
    last_heartbeat_at TEXT NOT NULL,
    lease_expires_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcp_runtime_workers_lease_expires_at
    ON mcp_runtime_workers(lease_expires_at);

CREATE TABLE IF NOT EXISTS mcp_runtime_audit_events (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    severity TEXT NOT NULL,
    context TEXT NOT NULL,
    transport TEXT NOT NULL,
    worker_id TEXT,
    message TEXT NOT NULL,
    metadata TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcp_runtime_audit_events_created_at
    ON mcp_runtime_audit_events(created_at DESC);

CREATE TABLE IF NOT EXISTS mcp_stdio_runtime_manifests (
    id TEXT PRIMARY KEY,
    owner_worker_id TEXT NOT NULL,
    server_name TEXT NOT NULL,
    process_group INTEGER NOT NULL,
    command TEXT,
    cwd TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcp_stdio_runtime_manifests_owner
    ON mcp_stdio_runtime_manifests(owner_worker_id);
CREATE INDEX IF NOT EXISTS idx_mcp_stdio_runtime_manifests_updated_at
    ON mcp_stdio_runtime_manifests(updated_at);

CREATE TABLE IF NOT EXISTS mcp_http_runtime_manifests (
    id TEXT PRIMARY KEY,
    owner_worker_id TEXT NOT NULL,
    server_name TEXT NOT NULL,
    session_id TEXT NOT NULL,
    protocol_version TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcp_http_runtime_manifests_owner
    ON mcp_http_runtime_manifests(owner_worker_id);
CREATE INDEX IF NOT EXISTS idx_mcp_http_runtime_manifests_updated_at
    ON mcp_http_runtime_manifests(updated_at);
";
static MCP_RESOURCE_SUBSCRIPTIONS: LazyLock<
    StdMutex<HashMap<String, HashMap<String, SessionCleanupRegistration>>>,
> = LazyLock::new(|| StdMutex::new(HashMap::new()));
static MCP_HTTP_SESSIONS: LazyLock<
    StdMutex<HashMap<String, HashMap<String, SessionCleanupRegistration>>>,
> = LazyLock::new(|| StdMutex::new(HashMap::new()));

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ResourceUpdateEvent {
    server_name: String,
    uri: Option<String>,
    payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DurableMcpHttpResourceSubscriptionTarget {
    server: String,
    session_id: String,
    #[serde(default = "default_durable_protocol_version")]
    protocol_version: String,
    uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DurableMcpHttpSessionTarget {
    server: String,
    session_id: String,
    #[serde(default = "default_durable_protocol_version")]
    protocol_version: String,
}

#[derive(Debug, Clone)]
struct StdioRuntimeManifest {
    id: String,
    owner_worker_id: String,
    server_name: String,
    process_group: u32,
    command: Option<String>,
    cwd: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct HttpRuntimeManifest {
    id: String,
    owner_worker_id: String,
    server_name: String,
    session_id: String,
    protocol_version: String,
    created_at: String,
    updated_at: String,
}

#[derive(Clone)]
struct StdioRuntimeManifestHandle {
    store: Arc<McpRuntimeStore>,
    manifest_id: String,
}

impl std::fmt::Debug for StdioRuntimeManifestHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioRuntimeManifestHandle")
            .field("manifest_id", &self.manifest_id)
            .finish()
    }
}

#[derive(Clone)]
struct HttpRuntimeManifestHandle {
    store: Arc<McpRuntimeStore>,
    manifest_id: String,
}

impl std::fmt::Debug for HttpRuntimeManifestHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRuntimeManifestHandle")
            .field("manifest_id", &self.manifest_id)
            .finish()
    }
}

#[derive(Clone)]
struct McpRuntimePersistence {
    store: Arc<McpRuntimeStore>,
    worker_id: String,
}

impl std::fmt::Debug for McpRuntimePersistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRuntimePersistence")
            .field("worker_id", &self.worker_id)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum McpRuntimeAuditEventKind {
    #[serde(rename = "runtime.reclaim_succeeded")]
    RuntimeReclaimSucceeded,
    #[serde(rename = "runtime.reclaim_failed")]
    RuntimeReclaimFailed,
}

impl McpRuntimeAuditEventKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::RuntimeReclaimSucceeded => "runtime.reclaim_succeeded",
            Self::RuntimeReclaimFailed => "runtime.reclaim_failed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "runtime.reclaim_succeeded" => Some(Self::RuntimeReclaimSucceeded),
            "runtime.reclaim_failed" => Some(Self::RuntimeReclaimFailed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum McpRuntimeAuditSeverity {
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "error")]
    Error,
}

impl McpRuntimeAuditSeverity {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Error => "error",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "info" => Some(Self::Info),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum McpRuntimeAuditContext {
    #[serde(rename = "startup")]
    Startup,
    #[serde(rename = "periodic")]
    Periodic,
}

impl McpRuntimeAuditContext {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Periodic => "periodic",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "startup" => Some(Self::Startup),
            "periodic" => Some(Self::Periodic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpRuntimeAuditEvent {
    pub id: String,
    pub kind: McpRuntimeAuditEventKind,
    pub severity: McpRuntimeAuditSeverity,
    pub context: McpRuntimeAuditContext,
    pub transport: McpTransportKind,
    pub worker_id: Option<String>,
    pub message: String,
    pub metadata: Option<Value>,
    pub created_at: DateTime<Utc>,
}

type RuntimeAuditEventRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
);

struct McpRuntimeStore {
    conn: Connection,
}

impl McpRuntimeStore {
    async fn open() -> anyhow::Result<Self> {
        let db_path = hermes_home().join("state.db");
        Self::open_at(&db_path).await
    }

    async fn open_at(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path).await?;
        conn.call(|c| -> rusqlite::Result<()> {
            c.execute_batch(MCP_RUNTIME_STORE_SCHEMA)?;
            Ok(())
        })
        .await?;

        Ok(Self { conn })
    }

    async fn upsert_worker_lease(
        &self,
        worker_id: &str,
        heartbeat_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let worker_id = worker_id.to_owned();
        let heartbeat_at = format_ts(heartbeat_at);
        let lease_expires_at = format_ts(lease_expires_at);
        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO mcp_runtime_workers (
                         worker_id, started_at, last_heartbeat_at, lease_expires_at
                     ) VALUES (?1, ?2, ?2, ?3)
                     ON CONFLICT(worker_id) DO UPDATE SET
                         last_heartbeat_at = excluded.last_heartbeat_at,
                         lease_expires_at = excluded.lease_expires_at",
                    rusqlite::params![worker_id, heartbeat_at, lease_expires_at],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn delete_worker_lease(&self, worker_id: &str) -> anyhow::Result<bool> {
        let worker_id = worker_id.to_owned();
        let deleted = self
            .conn
            .call(move |c| -> rusqlite::Result<bool> {
                let affected = c.execute(
                    "DELETE FROM mcp_runtime_workers WHERE worker_id = ?1",
                    rusqlite::params![worker_id],
                )?;
                Ok(affected > 0)
            })
            .await?;
        Ok(deleted)
    }

    async fn insert_runtime_audit_event(&self, event: &McpRuntimeAuditEvent) -> anyhow::Result<()> {
        let event = event.clone();
        let metadata = event
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let created_at = format_ts(event.created_at);
        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO mcp_runtime_audit_events (
                         id, kind, severity, context, transport, worker_id, message, metadata, created_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        event.id,
                        event.kind.as_str(),
                        event.severity.as_str(),
                        event.context.as_str(),
                        match event.transport {
                            McpTransportKind::Stdio => "stdio",
                            McpTransportKind::Http => "http",
                        },
                        event.worker_id,
                        event.message,
                        metadata,
                        created_at,
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn list_runtime_audit_events(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<McpRuntimeAuditEvent>> {
        let rows = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RuntimeAuditEventRow>> {
                let mut stmt = c.prepare(
                    "SELECT id, kind, severity, context, transport, worker_id, message, metadata, created_at
                     FROM mcp_runtime_audit_events
                     ORDER BY created_at DESC
                     LIMIT ?1",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![limit as i64], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut events = Vec::with_capacity(rows.len());
        for (id, kind, severity, context, transport, worker_id, message, metadata, created_at) in
            rows
        {
            let metadata = metadata.as_deref().map(serde_json::from_str).transpose()?;
            events.push(McpRuntimeAuditEvent {
                id,
                kind: McpRuntimeAuditEventKind::parse(&kind).ok_or_else(|| {
                    anyhow::anyhow!("unknown MCP runtime audit event kind: {kind}")
                })?,
                severity: McpRuntimeAuditSeverity::parse(&severity).ok_or_else(|| {
                    anyhow::anyhow!("unknown MCP runtime audit severity: {severity}")
                })?,
                context: McpRuntimeAuditContext::parse(&context).ok_or_else(|| {
                    anyhow::anyhow!("unknown MCP runtime audit context: {context}")
                })?,
                transport: match transport.as_str() {
                    "stdio" => McpTransportKind::Stdio,
                    "http" => McpTransportKind::Http,
                    _ => {
                        return Err(anyhow::anyhow!(
                            "unknown MCP runtime audit transport: {transport}"
                        ));
                    }
                },
                worker_id,
                message,
                metadata,
                created_at: parse_ts(&created_at)?,
            });
        }

        Ok(events)
    }

    async fn insert_stdio_runtime_manifest(
        &self,
        manifest: &StdioRuntimeManifest,
    ) -> anyhow::Result<()> {
        let manifest = manifest.clone();
        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO mcp_stdio_runtime_manifests (
                         id, owner_worker_id, server_name, process_group, command, cwd, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        manifest.id,
                        manifest.owner_worker_id,
                        manifest.server_name,
                        i64::from(manifest.process_group),
                        manifest.command,
                        manifest.cwd,
                        manifest.created_at,
                        manifest.updated_at
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn delete_stdio_runtime_manifest(&self, manifest_id: &str) -> anyhow::Result<bool> {
        let manifest_id = manifest_id.to_owned();
        let deleted = self
            .conn
            .call(move |c| -> rusqlite::Result<bool> {
                let affected = c.execute(
                    "DELETE FROM mcp_stdio_runtime_manifests WHERE id = ?1",
                    rusqlite::params![manifest_id],
                )?;
                Ok(affected > 0)
            })
            .await?;
        Ok(deleted)
    }

    async fn insert_http_runtime_manifest(
        &self,
        manifest: &HttpRuntimeManifest,
    ) -> anyhow::Result<()> {
        let manifest = manifest.clone();
        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO mcp_http_runtime_manifests (
                         id, owner_worker_id, server_name, session_id, protocol_version, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        manifest.id,
                        manifest.owner_worker_id,
                        manifest.server_name,
                        manifest.session_id,
                        manifest.protocol_version,
                        manifest.created_at,
                        manifest.updated_at
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn delete_http_runtime_manifest(&self, manifest_id: &str) -> anyhow::Result<bool> {
        let manifest_id = manifest_id.to_owned();
        let deleted = self
            .conn
            .call(move |c| -> rusqlite::Result<bool> {
                let affected = c.execute(
                    "DELETE FROM mcp_http_runtime_manifests WHERE id = ?1",
                    rusqlite::params![manifest_id],
                )?;
                Ok(affected > 0)
            })
            .await?;
        Ok(deleted)
    }

    async fn list_stale_stdio_runtime_manifests(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> anyhow::Result<Vec<StdioRuntimeManifest>> {
        let now = format_ts(now);
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<StdioRuntimeManifest>> {
                let mut stmt = c.prepare(
                    "SELECT m.id, m.owner_worker_id, m.server_name, m.process_group,
                            m.command, m.cwd, m.created_at, m.updated_at
                     FROM mcp_stdio_runtime_manifests m
                     LEFT JOIN mcp_runtime_workers w
                       ON w.worker_id = m.owner_worker_id
                     WHERE w.worker_id IS NULL
                        OR w.lease_expires_at <= ?1
                     ORDER BY m.created_at ASC
                     LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![now, limit as i64], |row| {
                        Ok(StdioRuntimeManifest {
                            id: row.get(0)?,
                            owner_worker_id: row.get(1)?,
                            server_name: row.get(2)?,
                            process_group: row.get::<_, i64>(3)? as u32,
                            command: row.get(4)?,
                            cwd: row.get(5)?,
                            created_at: row.get(6)?,
                            updated_at: row.get(7)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        Ok(raws)
    }

    async fn list_stale_http_runtime_manifests(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> anyhow::Result<Vec<HttpRuntimeManifest>> {
        let now = format_ts(now);
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<HttpRuntimeManifest>> {
                let mut stmt = c.prepare(
                    "SELECT m.id, m.owner_worker_id, m.server_name, m.session_id,
                            m.protocol_version, m.created_at, m.updated_at
                     FROM mcp_http_runtime_manifests m
                     LEFT JOIN mcp_runtime_workers w
                       ON w.worker_id = m.owner_worker_id
                     WHERE w.worker_id IS NULL
                        OR w.lease_expires_at <= ?1
                     ORDER BY m.created_at ASC
                     LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![now, limit as i64], |row| {
                        Ok(HttpRuntimeManifest {
                            id: row.get(0)?,
                            owner_worker_id: row.get(1)?,
                            server_name: row.get(2)?,
                            session_id: row.get(3)?,
                            protocol_version: row.get(4)?,
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        Ok(raws)
    }

    #[cfg(test)]
    async fn get_stdio_runtime_manifest(
        &self,
        manifest_id: &str,
    ) -> anyhow::Result<Option<StdioRuntimeManifest>> {
        use rusqlite::OptionalExtension as _;

        let manifest_id = manifest_id.to_owned();
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<StdioRuntimeManifest>> {
                c.query_row(
                    "SELECT id, owner_worker_id, server_name, process_group,
                            command, cwd, created_at, updated_at
                     FROM mcp_stdio_runtime_manifests
                     WHERE id = ?1",
                    rusqlite::params![manifest_id],
                    |row| {
                        Ok(StdioRuntimeManifest {
                            id: row.get(0)?,
                            owner_worker_id: row.get(1)?,
                            server_name: row.get(2)?,
                            process_group: row.get::<_, i64>(3)? as u32,
                            command: row.get(4)?,
                            cwd: row.get(5)?,
                            created_at: row.get(6)?,
                            updated_at: row.get(7)?,
                        })
                    },
                )
                .optional()
            })
            .await?;
        Ok(raw)
    }

    #[cfg(test)]
    async fn get_http_runtime_manifest(
        &self,
        manifest_id: &str,
    ) -> anyhow::Result<Option<HttpRuntimeManifest>> {
        use rusqlite::OptionalExtension as _;

        let manifest_id = manifest_id.to_owned();
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<HttpRuntimeManifest>> {
                c.query_row(
                    "SELECT id, owner_worker_id, server_name, session_id,
                            protocol_version, created_at, updated_at
                     FROM mcp_http_runtime_manifests
                     WHERE id = ?1",
                    rusqlite::params![manifest_id],
                    |row| {
                        Ok(HttpRuntimeManifest {
                            id: row.get(0)?,
                            owner_worker_id: row.get(1)?,
                            server_name: row.get(2)?,
                            session_id: row.get(3)?,
                            protocol_version: row.get(4)?,
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
                        })
                    },
                )
                .optional()
            })
            .await?;
        Ok(raw)
    }
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
    process_group: Option<u32>,
    runtime_manifest: Option<StdioRuntimeManifestHandle>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: Arc<AtomicU64>,
    capabilities: Arc<Mutex<McpCapabilities>>,
    refresh_tx: watch::Sender<u64>,
    resource_update_tx: broadcast::Sender<ResourceUpdateEvent>,
    background_tasks: Arc<StdMutex<Vec<JoinHandle<()>>>>,
}

impl StdioMcpClient {
    async fn connect(
        config: &McpServerConfig,
        persistence: Option<&McpRuntimePersistence>,
    ) -> Result<Self> {
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
        command.kill_on_drop(true);
        configure_stdio_process_group(&mut command);

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
        let process_group = child.id();
        let runtime_manifest = match (
            persistence,
            process_group.filter(|process_group| *process_group != 0),
        ) {
            (Some(persistence), Some(process_group)) => {
                match persist_stdio_runtime_manifest(persistence, config, process_group).await {
                    Ok(handle) => Some(handle),
                    Err(err) => {
                        tracing::warn!(
                            server = %config.name,
                            process_group,
                            "failed to persist MCP stdio runtime manifest: {err}"
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        let stdin = Arc::new(Mutex::new(stdin));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let refresh_tx = watch::channel(0).0;
        let resource_update_tx = broadcast::channel(64).0;
        let background_tasks = Arc::new(StdMutex::new(vec![
            spawn_stdout_reader(
                config.name.clone(),
                stdout,
                Arc::clone(&stdin),
                Arc::clone(&pending),
                refresh_tx.clone(),
                resource_update_tx.clone(),
            ),
            spawn_stderr_logger(config.name.clone(), stderr),
        ]));

        let client = Self {
            server_name: config.name.clone(),
            child: Arc::new(Mutex::new(child)),
            process_group,
            runtime_manifest,
            stdin,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            capabilities: Arc::new(Mutex::new(McpCapabilities::default())),
            refresh_tx,
            resource_update_tx,
            background_tasks,
        };

        if let Err(err) = client.initialize().await {
            client.shutdown();
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

    fn shutdown(&self) {
        let mut should_fallback_to_child_kill = true;
        if let Some(process_group) = self.process_group {
            should_fallback_to_child_kill = false;
            if let Err(err) = kill_stdio_process_group(process_group) {
                tracing::warn!(
                    server = %self.server_name,
                    process_group,
                    "failed to terminate MCP process group: {err}"
                );
                should_fallback_to_child_kill = true;
            }
        }

        let child_lock = self.child.try_lock();
        match child_lock {
            Ok(mut child) => match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) if should_fallback_to_child_kill => {
                    if let Err(err) = child.start_kill() {
                        tracing::warn!(
                            server = %self.server_name,
                            "failed to terminate MCP child: {err}"
                        );
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        server = %self.server_name,
                        "failed to inspect MCP child state: {err}"
                    );
                    if let Err(err) = child.start_kill() {
                        tracing::warn!(
                            server = %self.server_name,
                            "failed to terminate MCP child after state inspection error: {err}"
                        );
                    }
                }
            },
            Err(_) if should_fallback_to_child_kill => {
                tracing::warn!(
                    server = %self.server_name,
                    "skipping MCP child shutdown because the child lock is contended"
                );
            }
            Err(_) => {}
        }

        let mut tasks = self
            .background_tasks
            .lock()
            .expect("stdio background task lock poisoned");
        for task in tasks.drain(..) {
            task.abort();
        }

        if let Some(runtime_manifest) = self.runtime_manifest.clone() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let server_name = self.server_name.clone();
                handle.spawn(async move {
                    if let Err(err) = runtime_manifest
                        .store
                        .delete_stdio_runtime_manifest(&runtime_manifest.manifest_id)
                        .await
                    {
                        tracing::warn!(
                            server = %server_name,
                            manifest_id = %runtime_manifest.manifest_id,
                            "failed to delete MCP stdio runtime manifest during shutdown: {err}"
                        );
                    }
                });
            }
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

#[cfg(unix)]
fn configure_stdio_process_group(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_stdio_process_group(_command: &mut Command) {}

fn kill_stdio_process_group(process_group: u32) -> std::result::Result<(), String> {
    #[cfg(unix)]
    {
        if process_group == 0 {
            return Err("unknown process group".to_string());
        }

        let ret = unsafe { libc::kill(-(process_group as libc::pid_t), libc::SIGKILL) };
        if ret == 0 {
            return Ok(());
        }

        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(format!("killpg failed: {err}"))
        }
    }

    #[cfg(not(unix))]
    {
        let _ = process_group;
        Err("process-group cleanup is unsupported on this platform".to_string())
    }
}

fn mcp_resource_subscription_key(server_name: &str, uri: &str) -> String {
    format!("{server_name}\n{uri}")
}

fn default_durable_protocol_version() -> String {
    PROTOCOL_VERSION.to_string()
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339()
}

fn parse_ts(ts: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(ts)?.with_timezone(&Utc))
}

fn new_mcp_runtime_worker_id() -> String {
    format!("mcpw_{}", Uuid::new_v4().simple())
}

fn new_stdio_runtime_manifest_id() -> String {
    format!("mcpstd_{}", Uuid::new_v4().simple())
}

fn new_http_runtime_manifest_id() -> String {
    format!("mcphttp_{}", Uuid::new_v4().simple())
}

fn new_mcp_runtime_audit_event_id() -> String {
    format!("mcpaud_{}", Uuid::new_v4().simple())
}

fn mcp_runtime_worker_lease_expires_at(now: DateTime<Utc>) -> DateTime<Utc> {
    now + chrono::Duration::seconds(MCP_RUNTIME_WORKER_LEASE_SECS)
}

async fn persist_stdio_runtime_manifest(
    persistence: &McpRuntimePersistence,
    config: &McpServerConfig,
    process_group: u32,
) -> anyhow::Result<StdioRuntimeManifestHandle> {
    let now = Utc::now();
    let manifest = StdioRuntimeManifest {
        id: new_stdio_runtime_manifest_id(),
        owner_worker_id: persistence.worker_id.clone(),
        server_name: config.name.clone(),
        process_group,
        command: Some(config.command.clone()),
        cwd: config.cwd.as_ref().map(|cwd| cwd.display().to_string()),
        created_at: format_ts(now),
        updated_at: format_ts(now),
    };
    persistence
        .store
        .insert_stdio_runtime_manifest(&manifest)
        .await?;
    Ok(StdioRuntimeManifestHandle {
        store: Arc::clone(&persistence.store),
        manifest_id: manifest.id,
    })
}

async fn persist_http_runtime_manifest(
    persistence: &McpRuntimePersistence,
    server_name: &str,
    session_id: &str,
    protocol_version: &str,
) -> anyhow::Result<HttpRuntimeManifestHandle> {
    let now = Utc::now();
    let manifest = HttpRuntimeManifest {
        id: new_http_runtime_manifest_id(),
        owner_worker_id: persistence.worker_id.clone(),
        server_name: server_name.to_string(),
        session_id: session_id.to_string(),
        protocol_version: protocol_version.to_string(),
        created_at: format_ts(now),
        updated_at: format_ts(now),
    };
    persistence
        .store
        .insert_http_runtime_manifest(&manifest)
        .await?;
    Ok(HttpRuntimeManifestHandle {
        store: Arc::clone(&persistence.store),
        manifest_id: manifest.id,
    })
}

fn build_http_client_and_endpoint(config: &McpServerConfig) -> Result<(reqwest::Url, Client)> {
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
    Ok((endpoint, client))
}

fn replace_mcp_resource_subscription_registration(
    session_id: &str,
    server_name: &str,
    uri: &str,
    registration: SessionCleanupRegistration,
) -> Option<SessionCleanupRegistration> {
    MCP_RESOURCE_SUBSCRIPTIONS
        .lock()
        .expect("mcp resource subscriptions lock poisoned")
        .entry(session_id.to_string())
        .or_default()
        .insert(
            mcp_resource_subscription_key(server_name, uri),
            registration,
        )
}

fn take_mcp_resource_subscription_registration(
    session_id: &str,
    server_name: &str,
    uri: &str,
) -> Option<SessionCleanupRegistration> {
    let mut guard = MCP_RESOURCE_SUBSCRIPTIONS
        .lock()
        .expect("mcp resource subscriptions lock poisoned");
    let registrations = guard.get_mut(session_id)?;
    let registration = registrations.remove(&mcp_resource_subscription_key(server_name, uri));
    if registrations.is_empty() {
        guard.remove(session_id);
    }
    registration
}

fn replace_mcp_http_session_registration(
    session_id: &str,
    server_name: &str,
    registration: SessionCleanupRegistration,
) -> Option<SessionCleanupRegistration> {
    MCP_HTTP_SESSIONS
        .lock()
        .expect("mcp http sessions lock poisoned")
        .entry(session_id.to_string())
        .or_default()
        .insert(server_name.to_string(), registration)
}

fn take_mcp_http_session_registration(
    session_id: &str,
    server_name: &str,
) -> Option<SessionCleanupRegistration> {
    let mut guard = MCP_HTTP_SESSIONS
        .lock()
        .expect("mcp http sessions lock poisoned");
    let registrations = guard.get_mut(session_id)?;
    let registration = registrations.remove(server_name);
    if registrations.is_empty() {
        guard.remove(session_id);
    }
    registration
}

async fn register_mcp_resource_subscription_cleanup(
    session_id: &str,
    server_name: &str,
    uri: &str,
    client: McpClient,
) {
    let label = format!("mcp resource subscription {server_name} {uri}");
    let cleanup_session_id = session_id.to_string();
    let cleanup_server_name = server_name.to_string();
    let cleanup_uri = uri.to_string();
    let cleanup_client = client.clone();
    let registration = match client.durable_resource_subscription(server_name, uri).await {
        Some(durable_resource) => session_cleanup::register_async_cleanup_with_durable_resource(
            session_id,
            label,
            durable_resource,
            move || {
                let session_id = cleanup_session_id.clone();
                let server_name = cleanup_server_name.clone();
                let uri = cleanup_uri.clone();
                let client = cleanup_client.clone();
                async move {
                    let _ = take_mcp_resource_subscription_registration(
                        &session_id,
                        &server_name,
                        &uri,
                    );
                    client
                        .cleanup_resource_subscription(&uri)
                        .await
                        .map_err(|err| {
                            format!(
                                "failed to unsubscribe MCP resource '{server_name}:{uri}': {err}"
                            )
                        })
                }
            },
        ),
        None => session_cleanup::register_async_cleanup(session_id, label, move || {
            let session_id = cleanup_session_id.clone();
            let server_name = cleanup_server_name.clone();
            let uri = cleanup_uri.clone();
            let client = cleanup_client.clone();
            async move {
                let _ =
                    take_mcp_resource_subscription_registration(&session_id, &server_name, &uri);
                client
                    .cleanup_resource_subscription(&uri)
                    .await
                    .map_err(|err| {
                        format!("failed to unsubscribe MCP resource '{server_name}:{uri}': {err}")
                    })
            }
        }),
    };

    if let Some(previous) =
        replace_mcp_resource_subscription_registration(session_id, server_name, uri, registration)
    {
        let _ = session_cleanup::unregister(&previous);
    }
}

fn unregister_mcp_resource_subscription_cleanup(session_id: &str, server_name: &str, uri: &str) {
    if let Some(previous) =
        take_mcp_resource_subscription_registration(session_id, server_name, uri)
    {
        let _ = session_cleanup::unregister(&previous);
    }
}

async fn register_mcp_http_session_cleanup(session_id: &str, server_name: &str, client: McpClient) {
    let Some(durable_resource) = client.durable_http_session(server_name).await else {
        return;
    };
    let cleanup_session_id = session_id.to_string();
    let cleanup_server_name = server_name.to_string();
    let cleanup_client = client.clone();
    let registration = session_cleanup::register_async_cleanup_with_durable_resource(
        session_id,
        format!("mcp http session {server_name}"),
        durable_resource,
        move || {
            let session_id = cleanup_session_id.clone();
            let server_name = cleanup_server_name.clone();
            let client = cleanup_client.clone();
            async move {
                let _ = take_mcp_http_session_registration(&session_id, &server_name);
                client
                    .delete_session()
                    .await
                    .map_err(|err| format!("failed to delete MCP session '{server_name}': {err}"))
            }
        },
    );

    if let Some(previous) =
        replace_mcp_http_session_registration(session_id, server_name, registration)
    {
        let _ = session_cleanup::unregister(&previous);
    }
}

impl McpClient {
    async fn connect(
        config: &McpServerConfig,
        persistence: Option<&McpRuntimePersistence>,
    ) -> Result<Self> {
        match config.transport {
            McpTransportKind::Stdio => Ok(Self::Stdio(
                StdioMcpClient::connect(config, persistence).await?,
            )),
            McpTransportKind::Http => Ok(Self::Http(
                HttpMcpClient::connect(config, persistence).await?,
            )),
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

    async fn cleanup_resource_subscription(&self, uri: &str) -> Result<()> {
        match self
            .call_method("resources/unsubscribe", json!({ "uri": uri }))
            .await
        {
            Ok(_) => Ok(()),
            Err(HermesError::Mcp(message)) if message.contains("HTTP 404") => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn durable_http_session(&self, server_name: &str) -> Option<DurableCleanupResource> {
        match self {
            Self::Stdio(_) => None,
            Self::Http(client) => client.durable_http_session(server_name).await,
        }
    }

    async fn delete_session(&self) -> Result<()> {
        match self {
            Self::Stdio(_) => Ok(()),
            Self::Http(client) => client.delete_session().await,
        }
    }

    fn shutdown(&self) {
        match self {
            Self::Stdio(client) => client.shutdown(),
            Self::Http(client) => client.shutdown(),
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

    async fn durable_resource_subscription(
        &self,
        server_name: &str,
        uri: &str,
    ) -> Option<DurableCleanupResource> {
        match self {
            Self::Stdio(_) => None,
            Self::Http(client) => client.durable_resource_subscription(server_name, uri).await,
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
    notification_task: Arc<StdMutex<Option<JoinHandle<()>>>>,
    initialized: Arc<AtomicBool>,
    runtime_persistence: Option<McpRuntimePersistence>,
    runtime_manifest: Arc<Mutex<Option<HttpRuntimeManifestHandle>>>,
}

impl HttpMcpClient {
    async fn connect(
        config: &McpServerConfig,
        persistence: Option<&McpRuntimePersistence>,
    ) -> Result<Self> {
        let (endpoint, client) = build_http_client_and_endpoint(config)?;

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
            notification_task: Arc::new(StdMutex::new(None)),
            initialized: Arc::new(AtomicBool::new(false)),
            runtime_persistence: persistence.cloned(),
            runtime_manifest: Arc::new(Mutex::new(None)),
        };

        http.initialize().await?;
        http.start_notification_stream();
        Ok(http)
    }

    fn start_notification_stream(&self) {
        let client = self.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let task = tokio::spawn(async move {
            client.run_notification_stream(&mut shutdown_rx).await;
        });
        let previous = self
            .notification_task
            .lock()
            .expect("http notification task lock poisoned")
            .replace(task);
        if let Some(previous) = previous {
            previous.abort();
        }
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
        self.initialized.store(true, Ordering::Release);
        if let Err(err) = self.ensure_runtime_manifest_persisted().await {
            tracing::warn!(
                server = %self.server_name,
                "failed to persist MCP HTTP runtime manifest after initialize: {err}"
            );
        }

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
        if self.initialized.load(Ordering::Acquire) {
            if let Err(err) = self.ensure_runtime_manifest_persisted().await {
                tracing::warn!(
                    server = %self.server_name,
                    "failed to persist MCP HTTP runtime manifest after response: {err}"
                );
            }
        }

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
            if let Err(err) = self.ensure_runtime_manifest_persisted().await {
                tracing::warn!(
                    server = %self.server_name,
                    "failed to persist MCP HTTP runtime manifest after notification stream connect: {err}"
                );
            }

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

    async fn ensure_runtime_manifest_persisted(&self) -> anyhow::Result<()> {
        let Some(persistence) = &self.runtime_persistence else {
            return Ok(());
        };
        let session_id = match self.session_id.lock().await.clone() {
            Some(session_id) => session_id,
            None => return Ok(()),
        };
        let protocol_version = self.negotiated_protocol.lock().await.clone();
        let mut runtime_manifest = self.runtime_manifest.lock().await;
        if runtime_manifest.is_some() {
            return Ok(());
        }

        let manifest = persist_http_runtime_manifest(
            persistence,
            &self.server_name,
            &session_id,
            &protocol_version,
        )
        .await?;
        *runtime_manifest = Some(manifest);
        Ok(())
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

    async fn durable_resource_subscription(
        &self,
        server_name: &str,
        uri: &str,
    ) -> Option<DurableCleanupResource> {
        let session_id = self.session_id.lock().await.clone()?;
        let protocol_version = self.negotiated_protocol.lock().await.clone();
        let target_value = serde_json::to_string(&DurableMcpHttpResourceSubscriptionTarget {
            server: server_name.to_string(),
            session_id,
            protocol_version,
            uri: uri.to_string(),
        })
        .ok()?;
        Some(DurableCleanupResource {
            kind: DurableCleanupResourceKind::McpHttpResourceSubscription,
            label: format!("mcp resource subscription {server_name} {uri}"),
            target_value,
        })
    }

    async fn durable_http_session(&self, server_name: &str) -> Option<DurableCleanupResource> {
        let session_id = self.session_id.lock().await.clone()?;
        let protocol_version = self.negotiated_protocol.lock().await.clone();
        let target_value = serde_json::to_string(&DurableMcpHttpSessionTarget {
            server: server_name.to_string(),
            session_id,
            protocol_version,
        })
        .ok()?;
        Some(DurableCleanupResource {
            kind: DurableCleanupResourceKind::McpHttpSession,
            label: format!("mcp http session {server_name}"),
            target_value,
        })
    }

    async fn delete_session(&self) -> Result<()> {
        let Some(session_id) = self.session_id.lock().await.clone() else {
            return Ok(());
        };

        let negotiated = self.negotiated_protocol.lock().await.clone();
        delete_http_session_request(
            &self.client,
            self.endpoint.clone(),
            self.server_name.clone(),
            session_id,
            negotiated,
        )
        .await
    }

    fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(task) = self
            .notification_task
            .lock()
            .expect("http notification task lock poisoned")
            .take()
        {
            task.abort();
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let session_id = Arc::clone(&self.session_id);
        let negotiated_protocol = Arc::clone(&self.negotiated_protocol);
        let runtime_manifest = Arc::clone(&self.runtime_manifest);
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let server_name = self.server_name.clone();
        handle.spawn(async move {
            let session_id = session_id.lock().await.take();
            let protocol_version = negotiated_protocol.lock().await.clone();
            if let Some(session_id) = session_id {
                if let Err(err) = delete_http_session_request(
                    &client,
                    endpoint,
                    server_name.clone(),
                    session_id,
                    protocol_version,
                )
                .await
                {
                    tracing::warn!(server = %server_name, "failed deleting MCP HTTP session during shutdown: {err}");
                }
            }
            let runtime_manifest = runtime_manifest.lock().await.take();
            if let Some(runtime_manifest) = runtime_manifest {
                if let Err(err) = runtime_manifest
                    .store
                    .delete_http_runtime_manifest(&runtime_manifest.manifest_id)
                    .await
                {
                    tracing::warn!(
                        server = %server_name,
                        manifest_id = %runtime_manifest.manifest_id,
                        "failed to delete MCP HTTP runtime manifest during shutdown: {err}"
                    );
                }
            }
        });
    }

    fn subscribe_refresh(&self) -> watch::Receiver<u64> {
        self.refresh_tx.subscribe()
    }

    fn subscribe_resource_updates(&self) -> broadcast::Receiver<ResourceUpdateEvent> {
        self.resource_update_tx.subscribe()
    }

    fn connect_with_existing_session(
        config: &McpServerConfig,
        session_id: String,
        protocol_version: String,
    ) -> Result<Self> {
        let (endpoint, client) = build_http_client_and_endpoint(config)?;

        Ok(Self {
            server_name: config.name.clone(),
            endpoint,
            client,
            session_id: Arc::new(Mutex::new(Some(session_id))),
            negotiated_protocol: Arc::new(Mutex::new(protocol_version)),
            capabilities: Arc::new(Mutex::new(McpCapabilities::default())),
            refresh_tx: watch::channel(0).0,
            resource_update_tx: broadcast::channel(64).0,
            shutdown_tx: watch::channel(false).0,
            notification_task: Arc::new(StdMutex::new(None)),
            initialized: Arc::new(AtomicBool::new(true)),
            runtime_persistence: None,
            runtime_manifest: Arc::new(Mutex::new(None)),
        })
    }
}

async fn delete_http_session_request(
    client: &Client,
    endpoint: reqwest::Url,
    server_name: String,
    session_id: String,
    protocol_version: String,
) -> Result<()> {
    let response = client
        .delete(endpoint)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(MCP_PROTOCOL_VERSION_HEADER, protocol_version)
        .header(MCP_SESSION_ID_HEADER, session_id)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                HermesError::Mcp(format!(
                    "MCP server '{}' timed out deleting session after {}s",
                    server_name,
                    MCP_REQUEST_TIMEOUT.as_secs()
                ))
            } else {
                HermesError::Mcp(format!(
                    "failed deleting HTTP session for MCP server '{}': {err}",
                    server_name
                ))
            }
        })?;

    match response.status() {
        status if status.is_success() => Ok(()),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED => Ok(()),
        status => {
            let body = response.text().await.unwrap_or_default();
            let trimmed = body.trim();
            let detail = if trimmed.is_empty() {
                format!("HTTP {}", status)
            } else {
                format!("HTTP {}: {}", status, trimmed)
            };
            Err(HermesError::Mcp(format!(
                "MCP server '{}' session delete failed: {detail}",
                server_name
            )))
        }
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
    resource_updates_path: PathBuf,
}

#[derive(Debug, Default)]
struct McpRuntimeRecoverySummary {
    attempted: usize,
    cleaned: usize,
    failures: Vec<String>,
}

struct McpRuntimeLeaseOwner {
    store: Arc<McpRuntimeStore>,
    worker_id: String,
    heartbeat_task: JoinHandle<()>,
}

impl std::fmt::Debug for McpRuntimeLeaseOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRuntimeLeaseOwner")
            .field("worker_id", &self.worker_id)
            .finish()
    }
}

impl McpRuntimeLeaseOwner {
    fn new(persistence: McpRuntimePersistence, configs: &[McpServerConfig]) -> Self {
        let store = Arc::clone(&persistence.store);
        let worker_id = persistence.worker_id.clone();
        let heartbeat_store = Arc::clone(&store);
        let heartbeat_worker_id = worker_id.clone();
        let http_configs = runtime_http_recovery_configs(configs);
        let heartbeat_task = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(MCP_RUNTIME_WORKER_LEASE_INTERVAL_SECS));
            interval.tick().await;
            loop {
                interval.tick().await;
                let heartbeat_at = Utc::now();
                if let Err(err) = heartbeat_store
                    .upsert_worker_lease(
                        &heartbeat_worker_id,
                        heartbeat_at,
                        mcp_runtime_worker_lease_expires_at(heartbeat_at),
                    )
                    .await
                {
                    tracing::warn!(
                        worker_id = %heartbeat_worker_id,
                        "failed to refresh MCP runtime worker lease: {err}"
                    );
                }
                match reclaim_stale_stdio_runtime_manifests(heartbeat_store.as_ref()).await {
                    Ok(summary) => {
                        log_and_record_stale_stdio_runtime_recovery(
                            heartbeat_store.as_ref(),
                            &summary,
                            McpRuntimeAuditContext::Periodic,
                            Some(heartbeat_worker_id.as_str()),
                        )
                        .await;
                    }
                    Err(err) => {
                        tracing::warn!(
                            worker_id = %heartbeat_worker_id,
                            "failed reclaiming stale MCP stdio runtime manifests: {err}"
                        );
                        record_runtime_reclaim_error_audit_event(
                            heartbeat_store.as_ref(),
                            McpTransportKind::Stdio,
                            McpRuntimeAuditContext::Periodic,
                            Some(heartbeat_worker_id.as_str()),
                            "failed reclaiming stale MCP stdio runtime manifests",
                            &err,
                        )
                        .await;
                    }
                }
                match reclaim_stale_http_runtime_manifests(
                    heartbeat_store.as_ref(),
                    http_configs.as_ref(),
                )
                .await
                {
                    Ok(summary) => {
                        log_and_record_stale_http_runtime_recovery(
                            heartbeat_store.as_ref(),
                            &summary,
                            McpRuntimeAuditContext::Periodic,
                            Some(heartbeat_worker_id.as_str()),
                        )
                        .await;
                    }
                    Err(err) => {
                        tracing::warn!(
                            worker_id = %heartbeat_worker_id,
                            "failed reclaiming stale MCP HTTP runtime manifests: {err}"
                        );
                        record_runtime_reclaim_error_audit_event(
                            heartbeat_store.as_ref(),
                            McpTransportKind::Http,
                            McpRuntimeAuditContext::Periodic,
                            Some(heartbeat_worker_id.as_str()),
                            "failed reclaiming stale MCP HTTP runtime manifests",
                            &err,
                        )
                        .await;
                    }
                }
            }
        });
        Self {
            store,
            worker_id,
            heartbeat_task,
        }
    }
}

impl Drop for McpRuntimeLeaseOwner {
    fn drop(&mut self) {
        self.heartbeat_task.abort();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = Arc::clone(&self.store);
        let worker_id = self.worker_id.clone();
        handle.spawn(async move {
            if let Err(err) = store.delete_worker_lease(&worker_id).await {
                tracing::warn!(
                    worker_id = %worker_id,
                    "failed to delete MCP runtime worker lease during shutdown: {err}"
                );
            }
        });
    }
}

#[derive(Debug)]
struct McpRuntimeOwner {
    runtime: Arc<McpRuntime>,
    background_tasks: Vec<JoinHandle<()>>,
    _lease_owner: Option<McpRuntimeLeaseOwner>,
}

impl McpRuntimeOwner {
    fn new(
        runtime: Arc<McpRuntime>,
        registry: Arc<ToolRegistry>,
        persistence: Option<McpRuntimePersistence>,
        configs: &[McpServerConfig],
        options: McpRegistryBuildOptions,
    ) -> Self {
        let mut background_tasks = Arc::clone(&runtime).spawn_refresh_tasks(registry, options);
        if options.include_resource_updates_tool {
            background_tasks.extend(Arc::clone(&runtime).spawn_resource_update_tasks());
        }
        let lease_owner =
            persistence.map(|persistence| McpRuntimeLeaseOwner::new(persistence, configs));
        Self {
            runtime,
            background_tasks,
            _lease_owner: lease_owner,
        }
    }
}

impl Drop for McpRuntimeOwner {
    fn drop(&mut self) {
        self.runtime.shutdown();
        for task in &self.background_tasks {
            task.abort();
        }
    }
}

async fn build_mcp_runtime_persistence(
    configs: &[McpServerConfig],
) -> Option<McpRuntimePersistence> {
    if !configs.iter().any(|config| config.enabled) {
        return None;
    }

    let store = match McpRuntimeStore::open().await {
        Ok(store) => Arc::new(store),
        Err(err) => {
            tracing::warn!("MCP runtime persistence disabled: failed to open store: {err}");
            return None;
        }
    };

    match reclaim_stale_stdio_runtime_manifests(store.as_ref()).await {
        Ok(summary) => {
            log_and_record_stale_stdio_runtime_recovery(
                store.as_ref(),
                &summary,
                McpRuntimeAuditContext::Startup,
                None,
            )
            .await;
        }
        Err(err) => {
            tracing::warn!("failed reclaiming stale MCP stdio runtime manifests: {err}");
            record_runtime_reclaim_error_audit_event(
                store.as_ref(),
                McpTransportKind::Stdio,
                McpRuntimeAuditContext::Startup,
                None,
                "failed reclaiming stale MCP stdio runtime manifests",
                &err,
            )
            .await;
        }
    }
    let http_configs = runtime_http_recovery_configs(configs);
    match reclaim_stale_http_runtime_manifests(store.as_ref(), http_configs.as_ref()).await {
        Ok(summary) => {
            log_and_record_stale_http_runtime_recovery(
                store.as_ref(),
                &summary,
                McpRuntimeAuditContext::Startup,
                None,
            )
            .await;
        }
        Err(err) => {
            tracing::warn!("failed reclaiming stale MCP HTTP runtime manifests: {err}");
            record_runtime_reclaim_error_audit_event(
                store.as_ref(),
                McpTransportKind::Http,
                McpRuntimeAuditContext::Startup,
                None,
                "failed reclaiming stale MCP HTTP runtime manifests",
                &err,
            )
            .await;
        }
    }

    let worker_id = new_mcp_runtime_worker_id();
    let heartbeat_at = Utc::now();
    if let Err(err) = store
        .upsert_worker_lease(
            &worker_id,
            heartbeat_at,
            mcp_runtime_worker_lease_expires_at(heartbeat_at),
        )
        .await
    {
        tracing::warn!(
            worker_id = %worker_id,
            "MCP runtime persistence disabled: failed to register worker lease: {err}"
        );
        return None;
    }

    Some(McpRuntimePersistence { store, worker_id })
}

async fn reclaim_stale_stdio_runtime_manifests(
    store: &McpRuntimeStore,
) -> anyhow::Result<McpRuntimeRecoverySummary> {
    let manifests = store
        .list_stale_stdio_runtime_manifests(Utc::now(), MCP_RUNTIME_RECOVERY_BATCH_SIZE)
        .await?;
    let mut summary = McpRuntimeRecoverySummary {
        attempted: manifests.len(),
        ..McpRuntimeRecoverySummary::default()
    };

    for manifest in manifests {
        match kill_stdio_process_group(manifest.process_group) {
            Ok(()) => match store.delete_stdio_runtime_manifest(&manifest.id).await {
                Ok(true) | Ok(false) => summary.cleaned += 1,
                Err(err) => summary.failures.push(format!(
                    "failed to delete stale MCP stdio runtime manifest '{}' for server '{}': {err}",
                    manifest.id, manifest.server_name
                )),
            },
            Err(err) => summary.failures.push(format!(
                "failed to reclaim stale MCP stdio runtime '{}' process group {}: {err}",
                manifest.server_name, manifest.process_group
            )),
        }
    }

    Ok(summary)
}

async fn reclaim_stale_http_runtime_manifests(
    store: &McpRuntimeStore,
    configs: &HashMap<String, McpServerConfig>,
) -> anyhow::Result<McpRuntimeRecoverySummary> {
    let manifests = store
        .list_stale_http_runtime_manifests(Utc::now(), MCP_RUNTIME_RECOVERY_BATCH_SIZE)
        .await?;
    let mut summary = McpRuntimeRecoverySummary {
        attempted: manifests.len(),
        ..McpRuntimeRecoverySummary::default()
    };

    for manifest in manifests {
        let Some(config) = configs.get(&manifest.server_name) else {
            summary.failures.push(format!(
                "missing MCP HTTP server config '{}' required to reclaim stale runtime session '{}'",
                manifest.server_name, manifest.id
            ));
            continue;
        };

        let target = DurableMcpHttpSessionTarget {
            server: manifest.server_name.clone(),
            session_id: manifest.session_id.clone(),
            protocol_version: manifest.protocol_version.clone(),
        };
        match cleanup_http_session(config, &target).await {
            Ok(()) => match store.delete_http_runtime_manifest(&manifest.id).await {
                Ok(true) | Ok(false) => summary.cleaned += 1,
                Err(err) => summary.failures.push(format!(
                    "failed to delete stale MCP HTTP runtime manifest '{}' for server '{}': {err}",
                    manifest.id, manifest.server_name
                )),
            },
            Err(err) => summary.failures.push(format!(
                "failed to reclaim stale MCP HTTP runtime '{}' session '{}': {err}",
                manifest.server_name, manifest.session_id
            )),
        }
    }

    Ok(summary)
}

async fn log_and_record_stale_stdio_runtime_recovery(
    store: &McpRuntimeStore,
    summary: &McpRuntimeRecoverySummary,
    context: McpRuntimeAuditContext,
    worker_id: Option<&str>,
) {
    if summary.attempted == 0 {
        return;
    }

    if summary.failures.is_empty() {
        tracing::warn!(
            attempted = summary.attempted,
            cleaned = summary.cleaned,
            context = context.as_str(),
            "reclaimed stale MCP stdio runtime manifests"
        );
    } else {
        tracing::warn!(
            attempted = summary.attempted,
            cleaned = summary.cleaned,
            failures = ?summary.failures,
            context = context.as_str(),
            "MCP stdio runtime reclaim completed with failures"
        );
    }
    record_runtime_reclaim_audit_event(store, McpTransportKind::Stdio, summary, context, worker_id)
        .await;
}

async fn log_and_record_stale_http_runtime_recovery(
    store: &McpRuntimeStore,
    summary: &McpRuntimeRecoverySummary,
    context: McpRuntimeAuditContext,
    worker_id: Option<&str>,
) {
    if summary.attempted == 0 {
        return;
    }

    if summary.failures.is_empty() {
        tracing::warn!(
            attempted = summary.attempted,
            cleaned = summary.cleaned,
            context = context.as_str(),
            "reclaimed stale MCP HTTP runtime manifests"
        );
    } else {
        tracing::warn!(
            attempted = summary.attempted,
            cleaned = summary.cleaned,
            failures = ?summary.failures,
            context = context.as_str(),
            "MCP HTTP runtime reclaim completed with failures"
        );
    }
    record_runtime_reclaim_audit_event(store, McpTransportKind::Http, summary, context, worker_id)
        .await;
}

async fn record_runtime_reclaim_audit_event(
    store: &McpRuntimeStore,
    transport: McpTransportKind,
    summary: &McpRuntimeRecoverySummary,
    context: McpRuntimeAuditContext,
    worker_id: Option<&str>,
) {
    let transport_label = match &transport {
        McpTransportKind::Stdio => "stdio",
        McpTransportKind::Http => "http",
    };
    let (kind, severity, message) = if summary.failures.is_empty() {
        (
            McpRuntimeAuditEventKind::RuntimeReclaimSucceeded,
            McpRuntimeAuditSeverity::Info,
            match transport {
                McpTransportKind::Stdio => "reclaimed stale MCP stdio runtime manifests",
                McpTransportKind::Http => "reclaimed stale MCP HTTP runtime manifests",
            }
            .to_string(),
        )
    } else {
        (
            McpRuntimeAuditEventKind::RuntimeReclaimFailed,
            McpRuntimeAuditSeverity::Error,
            match transport {
                McpTransportKind::Stdio => "MCP stdio runtime reclaim completed with failures",
                McpTransportKind::Http => "MCP HTTP runtime reclaim completed with failures",
            }
            .to_string(),
        )
    };

    let event = McpRuntimeAuditEvent {
        id: new_mcp_runtime_audit_event_id(),
        kind,
        severity,
        context,
        transport,
        worker_id: worker_id.map(ToOwned::to_owned),
        message,
        metadata: Some(json!({
            "attempted": summary.attempted,
            "cleaned": summary.cleaned,
            "failures": summary.failures,
        })),
        created_at: Utc::now(),
    };

    if let Err(err) = store.insert_runtime_audit_event(&event).await {
        tracing::warn!(
            transport = transport_label,
            context = event.context.as_str(),
            "failed to persist MCP runtime audit event: {err}"
        );
    }
}

async fn record_runtime_reclaim_error_audit_event(
    store: &McpRuntimeStore,
    transport: McpTransportKind,
    context: McpRuntimeAuditContext,
    worker_id: Option<&str>,
    message: &str,
    err: &anyhow::Error,
) {
    let transport_label = match &transport {
        McpTransportKind::Stdio => "stdio",
        McpTransportKind::Http => "http",
    };
    let event = McpRuntimeAuditEvent {
        id: new_mcp_runtime_audit_event_id(),
        kind: McpRuntimeAuditEventKind::RuntimeReclaimFailed,
        severity: McpRuntimeAuditSeverity::Error,
        context,
        transport,
        worker_id: worker_id.map(ToOwned::to_owned),
        message: message.to_string(),
        metadata: Some(json!({
            "error": err.to_string(),
        })),
        created_at: Utc::now(),
    };

    if let Err(persist_err) = store.insert_runtime_audit_event(&event).await {
        tracing::warn!(
            transport = transport_label,
            context = event.context.as_str(),
            "failed to persist MCP runtime audit event: {persist_err}"
        );
    }
}

fn runtime_http_recovery_configs(
    configs: &[McpServerConfig],
) -> Arc<HashMap<String, McpServerConfig>> {
    Arc::new(
        configs
            .iter()
            .filter(|config| config.enabled && config.transport == McpTransportKind::Http)
            .cloned()
            .map(|config| (config.name.clone(), config))
            .collect(),
    )
}

pub struct McpDurableCleanupExecutor {
    configs: HashMap<String, McpServerConfig>,
}

impl McpDurableCleanupExecutor {
    pub fn new(configs: Vec<McpServerConfig>) -> Self {
        Self {
            configs: configs
                .into_iter()
                .map(|config| (config.name.clone(), config))
                .collect(),
        }
    }
}

#[async_trait]
impl DurableCleanupExecutor for McpDurableCleanupExecutor {
    async fn cleanup(
        &self,
        resource: &DurableCleanupResource,
    ) -> std::result::Result<bool, String> {
        match resource.kind {
            DurableCleanupResourceKind::McpHttpResourceSubscription => {
                let target: DurableMcpHttpResourceSubscriptionTarget =
                    serde_json::from_str(&resource.target_value).map_err(|err| {
                        format!(
                            "invalid MCP durable cleanup target '{}': {err}",
                            resource.target_value
                        )
                    })?;
                let config = self.configs.get(&target.server).ok_or_else(|| {
                    format!(
                        "missing MCP server config '{}' for durable cleanup resource '{}'",
                        target.server, resource.label
                    )
                })?;
                if !config.enabled {
                    return Err(format!(
                        "MCP server '{}' is disabled; cannot clean durable resource '{}'",
                        target.server, resource.label
                    ));
                }
                if config.transport != McpTransportKind::Http {
                    return Err(format!(
                        "MCP server '{}' does not use HTTP transport required for durable resource '{}'",
                        target.server, resource.label
                    ));
                }

                cleanup_http_resource_subscription(config, &target)
                    .await
                    .map_err(|err| {
                        format!(
                            "failed to clean durable MCP resource '{}': {err}",
                            resource.label
                        )
                    })?;
                Ok(true)
            }
            DurableCleanupResourceKind::McpHttpSession => {
                let target: DurableMcpHttpSessionTarget =
                    serde_json::from_str(&resource.target_value).map_err(|err| {
                        format!(
                            "invalid MCP durable cleanup target '{}': {err}",
                            resource.target_value
                        )
                    })?;
                let config = self.configs.get(&target.server).ok_or_else(|| {
                    format!(
                        "missing MCP server config '{}' for durable cleanup resource '{}'",
                        target.server, resource.label
                    )
                })?;
                if !config.enabled {
                    return Err(format!(
                        "MCP server '{}' is disabled; cannot clean durable resource '{}'",
                        target.server, resource.label
                    ));
                }
                if config.transport != McpTransportKind::Http {
                    return Err(format!(
                        "MCP server '{}' does not use HTTP transport required for durable resource '{}'",
                        target.server, resource.label
                    ));
                }

                cleanup_http_session(config, &target).await.map_err(|err| {
                    format!(
                        "failed to clean durable MCP resource '{}': {err}",
                        resource.label
                    )
                })?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        register_mcp_http_session_cleanup(&ctx.session_id, &self.server_name, self.client.clone())
            .await;
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
    updates_path: PathBuf,
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_prompt_server(optional_string_arg(&args, "server"))?;
        register_mcp_http_session_cleanup(&ctx.session_id, &server_name, client.clone()).await;
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_prompt_server(optional_string_arg(&args, "server"))?;
        let name = required_string_arg(&args, "name")?;
        let arguments = args.get("arguments").cloned();
        register_mcp_http_session_cleanup(&ctx.session_id, &server_name, client.clone()).await;
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_server(optional_string_arg(&args, "server"))?;
        register_mcp_http_session_cleanup(&ctx.session_id, &server_name, client.clone()).await;
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_server(optional_string_arg(&args, "server"))?;
        register_mcp_http_session_cleanup(&ctx.session_id, &server_name, client.clone()).await;
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_server(optional_string_arg(&args, "server"))?;
        let uri = required_string_arg(&args, "uri")?;
        register_mcp_http_session_cleanup(&ctx.session_id, &server_name, client.clone()).await;
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_subscription_server(optional_string_arg(&args, "server"))?;
        let uri = required_string_arg(&args, "uri")?;
        let result = client.subscribe_resource(uri).await?;
        register_mcp_http_session_cleanup(&ctx.session_id, &server_name, client.clone()).await;
        register_mcp_resource_subscription_cleanup(&ctx.session_id, &server_name, uri, client)
            .await;
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

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let (server_name, client) = self
            .servers
            .resolve_resource_subscription_server(optional_string_arg(&args, "server"))?;
        let uri = required_string_arg(&args, "uri")?;
        register_mcp_http_session_cleanup(&ctx.session_id, &server_name, client.clone()).await;
        let result = client.unsubscribe_resource(uri).await?;
        unregister_mcp_resource_subscription_cleanup(&ctx.session_id, &server_name, uri);
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
        let total_before = updates.len();
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
            persist_resource_updates(&self.updates_path, &updates).await?;
        }

        let count = selected.len();
        let result = json!({
            "updates": selected.into_iter().map(resource_update_to_json).collect::<Vec<_>>(),
            "count": count,
            "total_buffered": total_before,
        });
        Ok(ToolResult::ok(
            serde_json::to_string_pretty(&result).map_err(|err| {
                HermesError::Mcp(format!("failed to render resource updates JSON: {err}"))
            })?,
        ))
    }
}

impl McpRuntime {
    async fn connect(
        configs: &[McpServerConfig],
        persistence: Option<&McpRuntimePersistence>,
    ) -> Self {
        let mut entries: HashMap<String, McpServerEntry> = HashMap::new();
        let mut tool_cache: HashMap<String, Vec<McpToolDescriptor>> = HashMap::new();
        let resource_updates_path = hermes_home().join("mcp-resource-updates.json");

        for config in configs.iter().filter(|config| config.enabled) {
            match McpClient::connect(config, persistence).await {
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
                        client.shutdown();
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
            resource_updates: Arc::new(Mutex::new(
                load_resource_updates(&resource_updates_path)
                    .await
                    .unwrap_or_default(),
            )),
            resource_updates_path,
        }
    }

    async fn build_mcp_tools_with_options(
        &self,
        options: McpRegistryBuildOptions,
    ) -> Vec<Box<dyn Tool>> {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        let cache = self.tool_cache.lock().await.clone();

        if options.include_dynamic_tools {
            for (server_name, entry) in &self.entries {
                for descriptor in cache.get(server_name).cloned().unwrap_or_default() {
                    tools.push(Box::new(McpToolAdapter {
                        server_name: server_name.clone(),
                        descriptor,
                        client: entry.client.clone(),
                    }) as Box<dyn Tool>);
                }
            }
        }

        let server_directory = McpServerDirectory::new(self.entries.clone());
        if options.include_prompt_tools && server_directory.has_prompt_support() {
            tools.push(Box::new(McpPromptListTool {
                servers: server_directory.clone(),
            }));
            tools.push(Box::new(McpPromptGetTool {
                servers: server_directory.clone(),
            }));
        }
        if options.include_resource_read_tools && server_directory.has_resource_support() {
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
        if options.include_resource_subscription_tools
            && server_directory.has_resource_subscription_support()
        {
            tools.push(Box::new(McpResourceSubscribeTool {
                servers: server_directory.clone(),
            }));
            tools.push(Box::new(McpResourceUnsubscribeTool {
                servers: server_directory,
            }));
        }
        if options.include_resource_updates_tool
            && self
                .entries
                .values()
                .any(|entry| entry.capabilities.resources)
        {
            tools.push(Box::new(McpResourceUpdatesTool {
                updates: Arc::clone(&self.resource_updates),
                updates_path: self.resource_updates_path.clone(),
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

    fn spawn_refresh_tasks(
        self: Arc<Self>,
        registry: Arc<ToolRegistry>,
        options: McpRegistryBuildOptions,
    ) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        for (server_name, entry) in &self.entries {
            if !entry.capabilities.tools {
                continue;
            }

            let server_name = server_name.clone();
            let mut refresh_rx = entry.client.subscribe_refresh();
            let runtime = Arc::clone(&self);
            let registry = Arc::clone(&registry);

            tasks.push(tokio::spawn(async move {
                while refresh_rx.changed().await.is_ok() {
                    tracing::info!(server = %server_name, "refreshing MCP tool registry after notification");
                    match runtime.refresh_server(&server_name).await {
                        Ok(()) => {
                            let mcp_tools = runtime.build_mcp_tools_with_options(options).await;
                            registry.replace_toolset("mcp", mcp_tools);
                        }
                        Err(err) => {
                            tracing::warn!(server = %server_name, "failed to refresh MCP tools: {err}");
                        }
                    }
                }
            }));
        }
        tasks
    }

    fn spawn_resource_update_tasks(self: Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        for entry in self.entries.values() {
            if !entry.capabilities.resources {
                continue;
            }

            let runtime = Arc::clone(&self);
            let mut updates_rx = entry.client.subscribe_resource_updates();
            tasks.push(tokio::spawn(async move {
                loop {
                    match updates_rx.recv().await {
                        Ok(update) => runtime.record_resource_update(update).await,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "lagged while receiving MCP resource updates");
                        }
                    }
                }
            }));
        }
        tasks
    }

    async fn record_resource_update(&self, update: ResourceUpdateEvent) {
        let mut updates = self.resource_updates.lock().await;
        updates.push(update);
        if updates.len() > MAX_RESOURCE_UPDATES {
            let overflow = updates.len() - MAX_RESOURCE_UPDATES;
            updates.drain(0..overflow);
        }
        if let Err(err) = persist_resource_updates(&self.resource_updates_path, &updates).await {
            tracing::warn!(
                path = %self.resource_updates_path.display(),
                "failed to persist MCP resource updates: {err}"
            );
        }
    }

    fn shutdown(&self) {
        for entry in self.entries.values() {
            entry.client.shutdown();
        }
    }
}

async fn cleanup_http_resource_subscription(
    config: &McpServerConfig,
    target: &DurableMcpHttpResourceSubscriptionTarget,
) -> Result<()> {
    let client = McpClient::Http(HttpMcpClient::connect_with_existing_session(
        config,
        target.session_id.clone(),
        target.protocol_version.clone(),
    )?);
    let result = client.cleanup_resource_subscription(&target.uri).await;
    client.shutdown();
    result
}

async fn cleanup_http_session(
    config: &McpServerConfig,
    target: &DurableMcpHttpSessionTarget,
) -> Result<()> {
    let (endpoint, client) = build_http_client_and_endpoint(config)?;
    delete_http_session_request(
        &client,
        endpoint,
        config.name.clone(),
        target.session_id.clone(),
        target.protocol_version.clone(),
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpRegistryBuildOptions {
    pub include_dynamic_tools: bool,
    pub include_prompt_tools: bool,
    pub include_resource_read_tools: bool,
    pub include_resource_subscription_tools: bool,
    pub include_resource_updates_tool: bool,
}

impl McpRegistryBuildOptions {
    pub fn managed_http_read_only() -> Self {
        Self {
            include_dynamic_tools: false,
            include_prompt_tools: true,
            include_resource_read_tools: true,
            include_resource_subscription_tools: false,
            include_resource_updates_tool: false,
        }
    }
}

impl Default for McpRegistryBuildOptions {
    fn default() -> Self {
        Self {
            include_dynamic_tools: true,
            include_prompt_tools: true,
            include_resource_read_tools: true,
            include_resource_subscription_tools: true,
            include_resource_updates_tool: true,
        }
    }
}

pub async fn populate_registry(registry: Arc<ToolRegistry>, configs: &[McpServerConfig]) {
    populate_registry_with_options(registry, configs, McpRegistryBuildOptions::default()).await;
}

pub async fn populate_registry_with_options(
    registry: Arc<ToolRegistry>,
    configs: &[McpServerConfig],
    options: McpRegistryBuildOptions,
) {
    let persistence = build_mcp_runtime_persistence(configs).await;
    let runtime = Arc::new(McpRuntime::connect(configs, persistence.as_ref()).await);
    let mcp_tools = runtime.build_mcp_tools_with_options(options).await;
    registry.replace_toolset("mcp", mcp_tools);
    registry.set_toolset_owner(
        "mcp",
        Some(Arc::new(McpRuntimeOwner::new(
            runtime,
            Arc::clone(&registry),
            persistence,
            configs,
            options,
        ))),
    );
}

pub async fn discover_tools(configs: &[McpServerConfig]) -> Vec<Box<dyn Tool>> {
    discover_tools_with_options(configs, McpRegistryBuildOptions::default()).await
}

pub async fn discover_tools_with_options(
    configs: &[McpServerConfig],
    options: McpRegistryBuildOptions,
) -> Vec<Box<dyn Tool>> {
    let runtime = McpRuntime::connect(configs, None).await;
    runtime.build_mcp_tools_with_options(options).await
}

pub async fn list_runtime_audit_events(limit: usize) -> anyhow::Result<Vec<McpRuntimeAuditEvent>> {
    let store = McpRuntimeStore::open().await?;
    store.list_runtime_audit_events(limit.clamp(1, 1000)).await
}

fn spawn_stdout_reader(
    server_name: String,
    stdout: ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    refresh_tx: watch::Sender<u64>,
    resource_update_tx: broadcast::Sender<ResourceUpdateEvent>,
) -> JoinHandle<()> {
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
    })
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

async fn load_resource_updates(path: &PathBuf) -> Result<Vec<ResourceUpdateEvent>> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(HermesError::Mcp(format!(
                "failed to read MCP resource updates from '{}': {err}",
                path.display()
            )));
        }
    };

    serde_json::from_str(&contents).map_err(|err| {
        HermesError::Mcp(format!(
            "failed to parse MCP resource updates from '{}': {err}",
            path.display()
        ))
    })
}

async fn persist_resource_updates(path: &PathBuf, updates: &[ResourceUpdateEvent]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| {
            HermesError::Mcp(format!(
                "failed to create MCP resource update directory '{}': {err}",
                parent.display()
            ))
        })?;
    }

    let encoded = serde_json::to_vec_pretty(updates).map_err(|err| {
        HermesError::Mcp(format!(
            "failed to serialize MCP resource updates for '{}': {err}",
            path.display()
        ))
    })?;

    tokio::fs::write(path, encoded).await.map_err(|err| {
        HermesError::Mcp(format!(
            "failed to write MCP resource updates to '{}': {err}",
            path.display()
        ))
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

fn spawn_stderr_logger(server_name: String, stderr: ChildStderr) -> JoinHandle<()> {
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
    })
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
    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::delete,
    };
    use tempfile::{NamedTempFile, TempDir};
    use tokio::time::Duration;

    type HttpDeleteAudits = Arc<StdMutex<Vec<(String, String)>>>;

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
            notification_task: Arc::new(StdMutex::new(None)),
            initialized: Arc::new(AtomicBool::new(false)),
            runtime_persistence: None,
            runtime_manifest: Arc::new(Mutex::new(None)),
        }
    }

    fn pid_is_alive(pid: u32) -> bool {
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() != Some(libc::ESRCH)
    }

    async fn wait_for_pid_file(path: &std::path::Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(path) {
                    if let Ok(pid) = contents.trim().parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn temp_runtime_store() -> (TempDir, McpRuntimeStore) {
        let dir = TempDir::new().unwrap();
        let store = McpRuntimeStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();
        (dir, store)
    }

    async fn bind_test_listener() -> Option<tokio::net::TcpListener> {
        match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => Some(listener),
            Err(err) => {
                tracing::warn!("skipping MCP HTTP runtime test: failed to bind listener: {err}");
                None
            }
        }
    }

    async fn spawn_http_session_delete_server() -> Option<(String, HttpDeleteAudits, JoinHandle<()>)>
    {
        let listener = bind_test_listener().await?;
        let addr = listener.local_addr().ok()?;
        let deletes: HttpDeleteAudits = Arc::new(StdMutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/mcp",
                delete(
                    |State(deletes): State<HttpDeleteAudits>, headers: HeaderMap| async move {
                        let session_id = headers
                            .get(MCP_SESSION_ID_HEADER)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let protocol_version = headers
                            .get(MCP_PROTOCOL_VERSION_HEADER)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        deletes
                            .lock()
                            .expect("mock delete audits lock poisoned")
                            .push((session_id, protocol_version));
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(Arc::clone(&deletes));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Some((format!("http://{addr}/mcp"), deletes, server))
    }

    fn test_stdio_client(server_name: &str, command: &str) -> StdioMcpClient {
        let mut cmd = Command::new("bash");
        cmd.args(["-lc", command]);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        configure_stdio_process_group(&mut cmd);

        let mut child = cmd.spawn().expect("spawn stdio test child");
        let process_group = child.id();
        let stdin = child.stdin.take().expect("stdio test child missing stdin");
        let stdout = child
            .stdout
            .take()
            .expect("stdio test child missing stdout");
        let stderr = child
            .stderr
            .take()
            .expect("stdio test child missing stderr");
        let stdin = Arc::new(Mutex::new(stdin));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let refresh_tx = watch::channel(0).0;
        let resource_update_tx = broadcast::channel(64).0;
        let background_tasks = Arc::new(StdMutex::new(vec![
            spawn_stdout_reader(
                server_name.to_string(),
                stdout,
                Arc::clone(&stdin),
                Arc::clone(&pending),
                refresh_tx.clone(),
                resource_update_tx.clone(),
            ),
            spawn_stderr_logger(server_name.to_string(), stderr),
        ]));

        StdioMcpClient {
            server_name: server_name.to_string(),
            child: Arc::new(Mutex::new(child)),
            process_group,
            runtime_manifest: None,
            stdin,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            capabilities: Arc::new(Mutex::new(McpCapabilities::default())),
            refresh_tx,
            resource_update_tx,
            background_tasks,
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

    #[tokio::test]
    async fn persist_and_load_resource_updates_roundtrip() {
        let tempdir = TempDir::new().unwrap();
        let path = tempdir.path().join("mcp-resource-updates.json");
        let updates = vec![ResourceUpdateEvent {
            server_name: "docs".to_string(),
            uri: Some("file:///tmp/doc.txt".to_string()),
            payload: json!({"uri": "file:///tmp/doc.txt"}),
        }];

        persist_resource_updates(&path, &updates).await.unwrap();
        let loaded = load_resource_updates(&path).await.unwrap();

        assert_eq!(loaded, updates);
    }

    #[tokio::test]
    async fn http_client_reports_durable_resource_subscription_target() {
        let client = test_http_client("docs");
        *client.session_id.lock().await = Some("sid_123".to_string());
        *client.negotiated_protocol.lock().await = "2025-06-18".to_string();

        let durable = client
            .durable_resource_subscription("docs", "file:///tmp/doc.txt")
            .await
            .unwrap();

        assert_eq!(
            durable.kind,
            DurableCleanupResourceKind::McpHttpResourceSubscription
        );
        let target: DurableMcpHttpResourceSubscriptionTarget =
            serde_json::from_str(&durable.target_value).unwrap();
        assert_eq!(
            target,
            DurableMcpHttpResourceSubscriptionTarget {
                server: "docs".to_string(),
                session_id: "sid_123".to_string(),
                protocol_version: "2025-06-18".to_string(),
                uri: "file:///tmp/doc.txt".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn http_client_reports_durable_http_session_target() {
        let client = test_http_client("docs");
        *client.session_id.lock().await = Some("sid_789".to_string());
        *client.negotiated_protocol.lock().await = "2025-06-18".to_string();

        let durable = client.durable_http_session("docs").await.unwrap();

        assert_eq!(durable.kind, DurableCleanupResourceKind::McpHttpSession);
        let target: DurableMcpHttpSessionTarget =
            serde_json::from_str(&durable.target_value).unwrap();
        assert_eq!(
            target,
            DurableMcpHttpSessionTarget {
                server: "docs".to_string(),
                session_id: "sid_789".to_string(),
                protocol_version: "2025-06-18".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn http_client_persists_runtime_manifest_after_session_established() {
        let (_dir, store) = temp_runtime_store().await;
        let store = Arc::new(store);
        let client = test_http_client("docs");
        *client.session_id.lock().await = Some("sid_runtime".to_string());
        *client.negotiated_protocol.lock().await = "2025-06-18".to_string();
        client.initialized.store(true, Ordering::Release);
        let persistence = McpRuntimePersistence {
            store: Arc::clone(&store),
            worker_id: "mcpw_http_persist".to_string(),
        };
        let mut client = client;
        client.runtime_persistence = Some(persistence);

        client.ensure_runtime_manifest_persisted().await.unwrap();

        let runtime_manifest = client.runtime_manifest.lock().await.clone().unwrap();
        let stored = store
            .get_http_runtime_manifest(&runtime_manifest.manifest_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.server_name, "docs");
        assert_eq!(stored.session_id, "sid_runtime");
        assert_eq!(stored.protocol_version, "2025-06-18");
    }

    #[tokio::test]
    async fn http_shutdown_clears_session_id() {
        let client = test_http_client("docs");
        *client.session_id.lock().await = Some("sid_shutdown".to_string());

        client.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(client.session_id.lock().await.is_none());
    }

    #[tokio::test]
    async fn register_and_unregister_mcp_resource_subscription_cleanup() {
        let session_id = format!("mcp-subscription-{}", random_request_id());
        let client = test_http_client("docs");
        *client.session_id.lock().await = Some("sid_456".to_string());

        register_mcp_resource_subscription_cleanup(
            &session_id,
            "docs",
            "file:///tmp/doc.txt",
            McpClient::Http(client),
        )
        .await;
        let registrations = MCP_RESOURCE_SUBSCRIPTIONS
            .lock()
            .expect("mcp resource subscriptions lock poisoned");
        assert_eq!(registrations.get(&session_id).map(HashMap::len), Some(1));
        drop(registrations);

        unregister_mcp_resource_subscription_cleanup(&session_id, "docs", "file:///tmp/doc.txt");
        let registrations = MCP_RESOURCE_SUBSCRIPTIONS
            .lock()
            .expect("mcp resource subscriptions lock poisoned");
        assert!(!registrations.contains_key(&session_id));
    }

    #[tokio::test]
    async fn register_mcp_http_session_cleanup_tracks_session_registration() {
        let session_id = format!("mcp-http-session-{}", random_request_id());
        let client = test_http_client("docs");
        *client.session_id.lock().await = Some("sid_999".to_string());

        register_mcp_http_session_cleanup(&session_id, "docs", McpClient::Http(client)).await;

        let registrations = MCP_HTTP_SESSIONS
            .lock()
            .expect("mcp http sessions lock poisoned");
        assert_eq!(registrations.get(&session_id).map(HashMap::len), Some(1));
        drop(registrations);

        if let Some(registration) = take_mcp_http_session_registration(&session_id, "docs") {
            let _ = session_cleanup::unregister(&registration);
        }
    }

    #[tokio::test]
    async fn runtime_owner_shutdowns_http_clients_on_drop() {
        let client = test_http_client("docs");
        let mut shutdown_rx = client.shutdown_tx.subscribe();
        let runtime = Arc::new(McpRuntime {
            entries: HashMap::from([(
                "docs".to_string(),
                McpServerEntry {
                    client: McpClient::Http(client),
                    transport: McpTransportKind::Http,
                    capabilities: McpCapabilities::default(),
                },
            )]),
            tool_cache: Mutex::new(HashMap::new()),
            resource_updates: Arc::new(Mutex::new(Vec::new())),
            resource_updates_path: PathBuf::from("/tmp/mcp-runtime-owner-test.json"),
        });

        let owner = Arc::new(McpRuntimeOwner::new(
            Arc::clone(&runtime),
            Arc::new(ToolRegistry::new()),
            None,
            &[],
            McpRegistryBuildOptions::default(),
        ));
        drop(runtime);
        drop(owner);

        shutdown_rx.changed().await.unwrap();
        assert!(*shutdown_rx.borrow());
    }

    #[tokio::test]
    async fn runtime_audit_events_round_trip() {
        let (_dir, store) = temp_runtime_store().await;
        let event = McpRuntimeAuditEvent {
            id: "mcpaudit_1".to_string(),
            kind: McpRuntimeAuditEventKind::RuntimeReclaimSucceeded,
            severity: McpRuntimeAuditSeverity::Info,
            context: McpRuntimeAuditContext::Startup,
            transport: McpTransportKind::Stdio,
            worker_id: Some("mcpw_audit".to_string()),
            message: "reclaimed stale MCP stdio runtime manifests".to_string(),
            metadata: Some(json!({
                "attempted": 1,
                "cleaned": 1,
                "failures": [],
            })),
            created_at: Utc::now(),
        };

        store.insert_runtime_audit_event(&event).await.unwrap();

        let listed = store.list_runtime_audit_events(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, event.kind);
        assert_eq!(listed[0].severity, event.severity);
        assert_eq!(listed[0].context, event.context);
        assert_eq!(listed[0].transport, event.transport);
        assert_eq!(listed[0].worker_id.as_deref(), Some("mcpw_audit"));
    }

    #[tokio::test]
    async fn log_and_record_reclaim_summary_persists_runtime_audit_event() {
        let (_dir, store) = temp_runtime_store().await;
        let summary = McpRuntimeRecoverySummary {
            attempted: 2,
            cleaned: 1,
            failures: vec!["boom".to_string()],
        };

        log_and_record_stale_stdio_runtime_recovery(
            &store,
            &summary,
            McpRuntimeAuditContext::Periodic,
            Some("mcpw_periodic"),
        )
        .await;

        let listed = store.list_runtime_audit_events(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].kind,
            McpRuntimeAuditEventKind::RuntimeReclaimFailed
        );
        assert_eq!(listed[0].severity, McpRuntimeAuditSeverity::Error);
        assert_eq!(listed[0].context, McpRuntimeAuditContext::Periodic);
        assert_eq!(listed[0].transport, McpTransportKind::Stdio);
        assert_eq!(listed[0].worker_id.as_deref(), Some("mcpw_periodic"));
        assert_eq!(
            listed[0]
                .metadata
                .as_ref()
                .and_then(|value| value.get("attempted")),
            Some(&json!(2))
        );
        assert_eq!(
            listed[0]
                .metadata
                .as_ref()
                .and_then(|value| value.get("failures"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[tokio::test]
    async fn record_reclaim_error_persists_runtime_audit_event() {
        let (_dir, store) = temp_runtime_store().await;

        record_runtime_reclaim_error_audit_event(
            &store,
            McpTransportKind::Http,
            McpRuntimeAuditContext::Startup,
            None,
            "failed reclaiming stale MCP HTTP runtime manifests",
            &anyhow::anyhow!("network down"),
        )
        .await;

        let listed = store.list_runtime_audit_events(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].kind,
            McpRuntimeAuditEventKind::RuntimeReclaimFailed
        );
        assert_eq!(listed[0].severity, McpRuntimeAuditSeverity::Error);
        assert_eq!(listed[0].context, McpRuntimeAuditContext::Startup);
        assert_eq!(listed[0].transport, McpTransportKind::Http);
        assert_eq!(listed[0].worker_id, None);
        assert_eq!(
            listed[0]
                .metadata
                .as_ref()
                .and_then(|value| value.get("error")),
            Some(&json!("network down"))
        );
    }

    #[tokio::test]
    async fn live_worker_lease_keeps_stdio_runtime_manifest_out_of_recovery() {
        let (_dir, store) = temp_runtime_store().await;
        let worker_id = "mcpw_live";
        let now = Utc::now();
        store
            .upsert_worker_lease(worker_id, now, mcp_runtime_worker_lease_expires_at(now))
            .await
            .unwrap();
        let manifest = StdioRuntimeManifest {
            id: "manifest_live".to_string(),
            owner_worker_id: worker_id.to_string(),
            server_name: "docs".to_string(),
            process_group: 12345,
            command: Some("fake".to_string()),
            cwd: None,
            created_at: format_ts(now),
            updated_at: format_ts(now),
        };
        store
            .insert_stdio_runtime_manifest(&manifest)
            .await
            .unwrap();

        let summary = reclaim_stale_stdio_runtime_manifests(&store).await.unwrap();

        assert_eq!(summary.attempted, 0);
        assert!(summary.failures.is_empty());
        assert!(
            store
                .get_stdio_runtime_manifest(&manifest.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn live_worker_lease_keeps_http_runtime_manifest_out_of_recovery() {
        let (_dir, store) = temp_runtime_store().await;
        let worker_id = "mcpw_http_live";
        let now = Utc::now();
        store
            .upsert_worker_lease(worker_id, now, mcp_runtime_worker_lease_expires_at(now))
            .await
            .unwrap();
        let manifest = HttpRuntimeManifest {
            id: "manifest_http_live".to_string(),
            owner_worker_id: worker_id.to_string(),
            server_name: "docs".to_string(),
            session_id: "sid_live".to_string(),
            protocol_version: "2025-06-18".to_string(),
            created_at: format_ts(now),
            updated_at: format_ts(now),
        };
        store.insert_http_runtime_manifest(&manifest).await.unwrap();

        let summary = reclaim_stale_http_runtime_manifests(&store, &HashMap::new())
            .await
            .unwrap();

        assert_eq!(summary.attempted, 0);
        assert!(summary.failures.is_empty());
        assert!(
            store
                .get_http_runtime_manifest(&manifest.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn stale_stdio_runtime_manifest_reclaim_kills_process_group_and_clears_manifest() {
        let (_dir, store) = temp_runtime_store().await;
        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && cat", pid_file.path().display());
        let client = test_stdio_client("docs-stdio", &command);
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        assert!(pid_is_alive(descendant_pid));
        let process_group = client.process_group.expect("stdio process group");

        let now = Utc::now();
        let manifest = StdioRuntimeManifest {
            id: "manifest_stale".to_string(),
            owner_worker_id: "mcpw_missing".to_string(),
            server_name: "docs-stdio".to_string(),
            process_group,
            command: Some("bash".to_string()),
            cwd: None,
            created_at: format_ts(now),
            updated_at: format_ts(now),
        };
        store
            .insert_stdio_runtime_manifest(&manifest)
            .await
            .unwrap();

        let summary = reclaim_stale_stdio_runtime_manifests(&store).await.unwrap();

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pid_is_alive(descendant_pid));
        assert!(
            store
                .get_stdio_runtime_manifest(&manifest.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn stale_http_runtime_manifest_reclaim_deletes_session_and_clears_manifest() {
        let Some((base_url, deletes, server_handle)) = spawn_http_session_delete_server().await
        else {
            return;
        };
        let (_dir, store) = temp_runtime_store().await;
        let now = Utc::now();
        let manifest = HttpRuntimeManifest {
            id: "manifest_http_stale".to_string(),
            owner_worker_id: "mcpw_missing_http".to_string(),
            server_name: "docs".to_string(),
            session_id: "sid_stale".to_string(),
            protocol_version: "2025-06-18".to_string(),
            created_at: format_ts(now),
            updated_at: format_ts(now),
        };
        store.insert_http_runtime_manifest(&manifest).await.unwrap();
        let configs = HashMap::from([(
            "docs".to_string(),
            McpServerConfig {
                name: "docs".to_string(),
                transport: McpTransportKind::Http,
                url: Some(base_url),
                enabled: true,
                ..McpServerConfig::default()
            },
        )]);

        let summary = reclaim_stale_http_runtime_manifests(&store, &configs)
            .await
            .unwrap();

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());
        assert_eq!(
            deletes
                .lock()
                .expect("mock delete audits lock poisoned")
                .as_slice(),
            [("sid_stale".to_string(), "2025-06-18".to_string())]
        );
        assert!(
            store
                .get_http_runtime_manifest(&manifest.id)
                .await
                .unwrap()
                .is_none()
        );
        server_handle.abort();
    }

    #[tokio::test]
    async fn stdio_shutdown_deletes_runtime_manifest() {
        let (_dir, store) = temp_runtime_store().await;
        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && cat", pid_file.path().display());
        let mut client = test_stdio_client("docs-stdio", &command);
        let manifest = StdioRuntimeManifest {
            id: "manifest_shutdown".to_string(),
            owner_worker_id: "mcpw_shutdown".to_string(),
            server_name: "docs-stdio".to_string(),
            process_group: client.process_group.expect("stdio process group"),
            command: Some("bash".to_string()),
            cwd: None,
            created_at: format_ts(Utc::now()),
            updated_at: format_ts(Utc::now()),
        };
        store
            .insert_stdio_runtime_manifest(&manifest)
            .await
            .unwrap();
        let store = Arc::new(store);
        client.runtime_manifest = Some(StdioRuntimeManifestHandle {
            store: Arc::clone(&store),
            manifest_id: manifest.id.clone(),
        });

        client.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            store
                .get_stdio_runtime_manifest(&manifest.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn http_shutdown_deletes_runtime_manifest() {
        let (_dir, store) = temp_runtime_store().await;
        let manifest = HttpRuntimeManifest {
            id: "manifest_http_shutdown".to_string(),
            owner_worker_id: "mcpw_http_shutdown".to_string(),
            server_name: "docs".to_string(),
            session_id: "sid_shutdown".to_string(),
            protocol_version: "2025-06-18".to_string(),
            created_at: format_ts(Utc::now()),
            updated_at: format_ts(Utc::now()),
        };
        store.insert_http_runtime_manifest(&manifest).await.unwrap();
        let store = Arc::new(store);
        let client = test_http_client("docs");
        {
            let mut runtime_manifest = client.runtime_manifest.lock().await;
            *runtime_manifest = Some(HttpRuntimeManifestHandle {
                store: Arc::clone(&store),
                manifest_id: manifest.id.clone(),
            });
        }

        client.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            store
                .get_http_runtime_manifest(&manifest.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn http_shutdown_drains_notification_task_owner() {
        let client = test_http_client("docs");
        *client.session_id.lock().await = Some("sid_notification".to_string());
        client.start_notification_stream();
        assert!(
            client
                .notification_task
                .lock()
                .expect("http notification task lock poisoned")
                .is_some()
        );

        client.shutdown();

        assert!(
            client
                .notification_task
                .lock()
                .expect("http notification task lock poisoned")
                .is_none()
        );
    }

    #[tokio::test]
    async fn runtime_owner_shutdowns_stdio_clients_on_drop() {
        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && cat", pid_file.path().display());
        let client = test_stdio_client("docs-stdio", &command);
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        assert!(pid_is_alive(descendant_pid));

        let runtime = Arc::new(McpRuntime {
            entries: HashMap::from([(
                "docs-stdio".to_string(),
                McpServerEntry {
                    client: McpClient::Stdio(client.clone()),
                    transport: McpTransportKind::Stdio,
                    capabilities: McpCapabilities::default(),
                },
            )]),
            tool_cache: Mutex::new(HashMap::new()),
            resource_updates: Arc::new(Mutex::new(Vec::new())),
            resource_updates_path: PathBuf::from("/tmp/mcp-runtime-owner-stdio-test.json"),
        });

        let owner = Arc::new(McpRuntimeOwner::new(
            Arc::clone(&runtime),
            Arc::new(ToolRegistry::new()),
            None,
            &[],
            McpRegistryBuildOptions::default(),
        ));
        drop(runtime);
        drop(owner);

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pid_is_alive(descendant_pid));
        assert!(
            client
                .background_tasks
                .lock()
                .expect("stdio background task lock poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn stdio_shutdown_kills_process_group_descendants_and_drains_tasks() {
        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && cat", pid_file.path().display());
        let client = test_stdio_client("docs-stdio", &command);
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        assert!(pid_is_alive(descendant_pid));
        assert!(
            !client
                .background_tasks
                .lock()
                .expect("stdio background task lock poisoned")
                .is_empty()
        );

        client.shutdown();

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pid_is_alive(descendant_pid));
        assert!(
            client
                .background_tasks
                .lock()
                .expect("stdio background task lock poisoned")
                .is_empty()
        );
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
