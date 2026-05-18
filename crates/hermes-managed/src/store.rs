use std::{collections::HashSet, path::Path};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hermes_config::hermes_home;
use hermes_core::error::{HermesError, Result};
use hermes_tools::session_cleanup::{
    DurableCleanupRecorder, DurableCleanupResource, DurableCleanupResourceKind,
};
use rusqlite::OptionalExtension;
use tokio_rusqlite::Connection;

use crate::types::{
    ManagedAgent, ManagedAgentVersion, ManagedAgentVersionDraft, ManagedApprovalPolicy, ManagedRun,
    ManagedRunArtifact, ManagedRunArtifactDraft, ManagedRunArtifactKind, ManagedRunCleanupResource,
    ManagedRunCleanupResourceKind, ManagedRunEvent, ManagedRunEventDraft, ManagedRunEventKind,
    ManagedRunOwnerSnapshot, ManagedRunOwnerState, ManagedRunStatus,
};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    latest_version INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agents_name ON agents(name);

CREATE TABLE IF NOT EXISTS agent_versions (
    agent_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    model TEXT NOT NULL,
    base_url TEXT,
    system_prompt TEXT NOT NULL,
    allowed_tools TEXT NOT NULL,
    allowed_skills TEXT NOT NULL,
    max_iterations INTEGER NOT NULL,
    temperature REAL NOT NULL,
    approval_policy TEXT NOT NULL,
    timeout_secs INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, version),
    FOREIGN KEY (agent_id) REFERENCES agents(id)
);
CREATE INDEX IF NOT EXISTS idx_agent_versions_agent ON agent_versions(agent_id, version DESC);

CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    agent_version INTEGER NOT NULL,
    status TEXT NOT NULL,
    model TEXT NOT NULL,
    session_id TEXT,
    prompt TEXT NOT NULL DEFAULT '',
    replay_of_run_id TEXT,
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    ended_at TEXT,
    cancel_requested_at TEXT,
    terminal_status_hint TEXT,
    terminal_reason_hint TEXT,
    owner_worker_id TEXT,
    owner_claim_token TEXT,
    owner_claimed_at TEXT,
    owner_last_heartbeat_at TEXT,
    owner_lease_expires_at TEXT,
    last_error TEXT,
    FOREIGN KEY (agent_id, agent_version) REFERENCES agent_versions(agent_id, version)
);
CREATE INDEX IF NOT EXISTS idx_runs_agent_time ON runs(agent_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_runs_status_updated ON runs(status, updated_at DESC);

CREATE TABLE IF NOT EXISTS run_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    message TEXT,
    tool_name TEXT,
    tool_call_id TEXT,
    metadata TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_run_events_run_id ON run_events(run_id, id ASC);

CREATE TABLE IF NOT EXISTS run_artifacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    label TEXT NOT NULL,
    tool_name TEXT,
    tool_call_id TEXT,
    content TEXT NOT NULL,
    metadata TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_run_artifacts_run_id ON run_artifacts(run_id, id ASC);

CREATE TABLE IF NOT EXISTS run_cleanup_resources (
    run_id TEXT NOT NULL,
    entry_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    label TEXT NOT NULL,
    target_value TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (run_id, entry_id),
    FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_run_cleanup_resources_run_id ON run_cleanup_resources(run_id);
CREATE INDEX IF NOT EXISTS idx_run_cleanup_resources_updated_at ON run_cleanup_resources(updated_at);
";

pub struct ManagedStore {
    conn: Connection,
}

impl ManagedStore {
    /// Open the default managed database inside the shared Hermes state DB.
    pub async fn open() -> anyhow::Result<Self> {
        let db_path = hermes_home().join("state.db");
        Self::open_at(&db_path).await
    }

    /// Open the managed store at a specific SQLite path.
    pub async fn open_at(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path).await?;
        conn.call(|c| -> rusqlite::Result<()> {
            c.execute_batch(SCHEMA)?;
            if !has_column(c, "runs", "prompt")? {
                c.execute(
                    "ALTER TABLE runs ADD COLUMN prompt TEXT NOT NULL DEFAULT ''",
                    [],
                )?;
            }
            if !has_column(c, "runs", "session_id")? {
                c.execute("ALTER TABLE runs ADD COLUMN session_id TEXT", [])?;
            }
            if !has_column(c, "runs", "replay_of_run_id")? {
                c.execute("ALTER TABLE runs ADD COLUMN replay_of_run_id TEXT", [])?;
            }
            if !has_column(c, "runs", "terminal_status_hint")? {
                c.execute("ALTER TABLE runs ADD COLUMN terminal_status_hint TEXT", [])?;
            }
            if !has_column(c, "runs", "terminal_reason_hint")? {
                c.execute("ALTER TABLE runs ADD COLUMN terminal_reason_hint TEXT", [])?;
            }
            if !has_column(c, "runs", "owner_worker_id")? {
                c.execute("ALTER TABLE runs ADD COLUMN owner_worker_id TEXT", [])?;
            }
            if !has_column(c, "runs", "owner_claim_token")? {
                c.execute("ALTER TABLE runs ADD COLUMN owner_claim_token TEXT", [])?;
            }
            if !has_column(c, "runs", "owner_claimed_at")? {
                c.execute("ALTER TABLE runs ADD COLUMN owner_claimed_at TEXT", [])?;
            }
            if !has_column(c, "runs", "owner_last_heartbeat_at")? {
                c.execute(
                    "ALTER TABLE runs ADD COLUMN owner_last_heartbeat_at TEXT",
                    [],
                )?;
            }
            if !has_column(c, "runs", "owner_lease_expires_at")? {
                c.execute(
                    "ALTER TABLE runs ADD COLUMN owner_lease_expires_at TEXT",
                    [],
                )?;
            }
            if !has_column(c, "run_events", "metadata")? {
                c.execute("ALTER TABLE run_events ADD COLUMN metadata TEXT", [])?;
            }
            Ok(())
        })
        .await?;

        Ok(Self { conn })
    }

    pub async fn create_agent(&self, agent: &ManagedAgent) -> Result<()> {
        let id = agent.id.clone();
        let name = agent.name.clone();
        let latest_version = i64::from(agent.latest_version);
        let archived = if agent.archived { 1i64 } else { 0i64 };
        let created_at = format_ts(&agent.created_at);
        let updated_at = format_ts(&agent.updated_at);

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO agents (id, name, latest_version, archived, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![id, name, latest_version, archived, created_at, updated_at],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }

    pub async fn get_agent(&self, agent_id: &str) -> Result<Option<ManagedAgent>> {
        let agent_id = agent_id.to_owned();
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<RawAgent>> {
                c.query_row(
                    "SELECT id, name, latest_version, archived, created_at, updated_at
                     FROM agents
                     WHERE id = ?1",
                    rusqlite::params![agent_id],
                    |row| {
                        Ok(RawAgent {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            latest_version: row.get(2)?,
                            archived: row.get(3)?,
                            created_at: row.get(4)?,
                            updated_at: row.get(5)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(db_err)?;

        raw.map(map_agent).transpose()
    }

    pub async fn get_agent_by_name(&self, name: &str) -> Result<Option<ManagedAgent>> {
        let name = name.to_owned();
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<RawAgent>> {
                c.query_row(
                    "SELECT id, name, latest_version, archived, created_at, updated_at
                     FROM agents
                     WHERE name = ?1",
                    rusqlite::params![name],
                    |row| {
                        Ok(RawAgent {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            latest_version: row.get(2)?,
                            archived: row.get(3)?,
                            created_at: row.get(4)?,
                            updated_at: row.get(5)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(db_err)?;

        raw.map(map_agent).transpose()
    }

    pub async fn list_agents(&self, limit: usize) -> Result<Vec<ManagedAgent>> {
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawAgent>> {
                let mut stmt = c.prepare(
                    "SELECT id, name, latest_version, archived, created_at, updated_at
                     FROM agents
                     ORDER BY updated_at DESC
                     LIMIT ?1",
                )?;
                stmt.query_map(rusqlite::params![limit as i64], |row| {
                    Ok(RawAgent {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        latest_version: row.get(2)?,
                        archived: row.get(3)?,
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_agent).collect()
    }

    pub async fn create_agent_version(&self, version: &ManagedAgentVersion) -> Result<()> {
        let agent_id = version.agent_id.clone();
        let version_num = i64::from(version.version);
        let model = version.model.clone();
        let base_url = version.base_url.clone();
        let system_prompt = version.system_prompt.clone();
        let allowed_tools = serde_json::to_string(&version.allowed_tools)
            .map_err(|e| HermesError::Config(format!("failed to serialize allowed_tools: {e}")))?;
        let allowed_skills = serde_json::to_string(&version.allowed_skills)
            .map_err(|e| HermesError::Config(format!("failed to serialize allowed_skills: {e}")))?;
        let max_iterations = i64::from(version.max_iterations);
        let temperature = version.temperature;
        let approval_policy = version.approval_policy.as_str().to_string();
        let timeout_secs = i64::from(version.timeout_secs);
        let created_at = format_ts(&version.created_at);
        let updated_at = format_ts(&Utc::now());

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO agent_versions
                        (agent_id, version, model, base_url, system_prompt, allowed_tools,
                         allowed_skills, max_iterations, temperature, approval_policy,
                         timeout_secs, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    rusqlite::params![
                        agent_id,
                        version_num,
                        model,
                        base_url,
                        system_prompt,
                        allowed_tools,
                        allowed_skills,
                        max_iterations,
                        temperature,
                        approval_policy,
                        timeout_secs,
                        created_at
                    ],
                )?;

                c.execute(
                    "UPDATE agents
                     SET latest_version = CASE
                         WHEN latest_version < ?1 THEN ?1
                         ELSE latest_version
                      END,
                      updated_at = ?2
                     WHERE id = ?3",
                    rusqlite::params![version_num, updated_at, agent_id],
                )?;

                Ok(())
            })
            .await
            .map_err(db_err)
    }

    pub async fn create_next_agent_version(
        &self,
        agent_id: &str,
        draft: &ManagedAgentVersionDraft,
    ) -> Result<ManagedAgentVersion> {
        let agent_id = agent_id.to_owned();
        let model = draft.model.clone();
        let base_url = draft.base_url.clone();
        let system_prompt = draft.system_prompt.clone();
        let allowed_tools = serde_json::to_string(&draft.allowed_tools)
            .map_err(|e| HermesError::Config(format!("failed to serialize allowed_tools: {e}")))?;
        let allowed_skills = serde_json::to_string(&draft.allowed_skills)
            .map_err(|e| HermesError::Config(format!("failed to serialize allowed_skills: {e}")))?;
        let max_iterations = i64::from(draft.max_iterations);
        let temperature = draft.temperature;
        let approval_policy = draft.approval_policy.as_str().to_string();
        let timeout_secs = i64::from(draft.timeout_secs);
        let created_at = format_ts(&Utc::now());

        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<RawAgentVersion> {
                let tx = c.transaction()?;

                let current_latest: i64 = tx.query_row(
                    "SELECT latest_version
                     FROM agents
                     WHERE id = ?1 AND archived = 0",
                    rusqlite::params![agent_id.clone()],
                    |row| row.get(0),
                )?;

                let next_version = current_latest + 1;

                tx.execute(
                    "INSERT INTO agent_versions
                        (agent_id, version, model, base_url, system_prompt, allowed_tools,
                         allowed_skills, max_iterations, temperature, approval_policy,
                         timeout_secs, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    rusqlite::params![
                        agent_id.clone(),
                        next_version,
                        model.clone(),
                        base_url.clone(),
                        system_prompt.clone(),
                        allowed_tools.clone(),
                        allowed_skills.clone(),
                        max_iterations,
                        temperature,
                        approval_policy.clone(),
                        timeout_secs,
                        created_at.clone(),
                    ],
                )?;

                tx.execute(
                    "UPDATE agents
                     SET latest_version = ?1,
                         updated_at = ?2
                     WHERE id = ?3",
                    rusqlite::params![next_version, created_at.clone(), agent_id.clone()],
                )?;

                tx.commit()?;

                Ok(RawAgentVersion {
                    agent_id,
                    version: next_version,
                    model,
                    base_url,
                    system_prompt,
                    allowed_tools,
                    allowed_skills,
                    max_iterations,
                    temperature,
                    approval_policy,
                    timeout_secs,
                    created_at,
                })
            })
            .await
            .map_err(db_err)?;

        map_agent_version(raw)
    }

    pub async fn get_agent_version(
        &self,
        agent_id: &str,
        version: u32,
    ) -> Result<Option<ManagedAgentVersion>> {
        let agent_id = agent_id.to_owned();
        let version = i64::from(version);
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<RawAgentVersion>> {
                c.query_row(
                    "SELECT agent_id, version, model, base_url, system_prompt, allowed_tools,
                            allowed_skills, max_iterations, temperature, approval_policy,
                            timeout_secs, created_at
                     FROM agent_versions
                     WHERE agent_id = ?1 AND version = ?2",
                    rusqlite::params![agent_id, version],
                    |row| {
                        Ok(RawAgentVersion {
                            agent_id: row.get(0)?,
                            version: row.get(1)?,
                            model: row.get(2)?,
                            base_url: row.get(3)?,
                            system_prompt: row.get(4)?,
                            allowed_tools: row.get(5)?,
                            allowed_skills: row.get(6)?,
                            max_iterations: row.get(7)?,
                            temperature: row.get(8)?,
                            approval_policy: row.get(9)?,
                            timeout_secs: row.get(10)?,
                            created_at: row.get(11)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(db_err)?;

        raw.map(map_agent_version).transpose()
    }

    pub async fn list_agent_versions(&self, agent_id: &str) -> Result<Vec<ManagedAgentVersion>> {
        let agent_id = agent_id.to_owned();
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawAgentVersion>> {
                let mut stmt = c.prepare(
                    "SELECT agent_id, version, model, base_url, system_prompt, allowed_tools,
                            allowed_skills, max_iterations, temperature, approval_policy,
                            timeout_secs, created_at
                     FROM agent_versions
                     WHERE agent_id = ?1
                     ORDER BY version DESC",
                )?;
                stmt.query_map(rusqlite::params![agent_id], |row| {
                    Ok(RawAgentVersion {
                        agent_id: row.get(0)?,
                        version: row.get(1)?,
                        model: row.get(2)?,
                        base_url: row.get(3)?,
                        system_prompt: row.get(4)?,
                        allowed_tools: row.get(5)?,
                        allowed_skills: row.get(6)?,
                        max_iterations: row.get(7)?,
                        temperature: row.get(8)?,
                        approval_policy: row.get(9)?,
                        timeout_secs: row.get(10)?,
                        created_at: row.get(11)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_agent_version).collect()
    }

    pub async fn archive_agent(&self, agent_id: &str) -> Result<()> {
        let agent_id = agent_id.to_owned();
        let updated_at = format_ts(&Utc::now());

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE agents
                     SET archived = 1,
                         updated_at = ?1
                     WHERE id = ?2",
                    rusqlite::params![updated_at, agent_id],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }

    pub async fn create_run(&self, run: &ManagedRun) -> Result<()> {
        let id = run.id.clone();
        let agent_id = run.agent_id.clone();
        let agent_version = i64::from(run.agent_version);
        let status = run.status.as_str().to_string();
        let model = run.model.clone();
        let session_id = run.session_id.clone();
        let prompt = run.prompt.clone();
        let replay_of_run_id = run.replay_of_run_id.clone();
        let started_at = format_ts(&run.started_at);
        let updated_at = format_ts(&run.updated_at);
        let ended_at = run.ended_at.as_ref().map(format_ts);
        let cancel_requested_at = run.cancel_requested_at.as_ref().map(format_ts);
        let last_error = run.last_error.clone();

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO runs
                        (id, agent_id, agent_version, status, model, session_id, prompt,
                         replay_of_run_id, started_at, updated_at, ended_at, cancel_requested_at,
                         terminal_status_hint, terminal_reason_hint, owner_worker_id,
                         owner_claim_token, owner_claimed_at, owner_last_heartbeat_at,
                         owner_lease_expires_at, last_error)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, NULL, NULL,
                             NULL, NULL, NULL, NULL, ?13)",
                    rusqlite::params![
                        id,
                        agent_id,
                        agent_version,
                        status,
                        model,
                        session_id,
                        prompt,
                        replay_of_run_id,
                        started_at,
                        updated_at,
                        ended_at,
                        cancel_requested_at,
                        last_error
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }

    pub async fn claim_run_ownership(
        &self,
        run_id: &str,
        worker_id: &str,
        claim_token: &str,
        claimed_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        let run_id = run_id.to_owned();
        let worker_id = worker_id.to_owned();
        let claim_token = claim_token.to_owned();
        let claimed_at = format_ts(&claimed_at);
        let lease_expires_at = format_ts(&lease_expires_at);
        let running = ManagedRunStatus::Running.as_str().to_string();
        let pending = ManagedRunStatus::Pending.as_str().to_string();

        let changed = self
            .conn
            .call(move |c| -> rusqlite::Result<usize> {
                c.execute(
                    "UPDATE runs
                     SET status = ?1,
                         updated_at = ?2,
                         owner_worker_id = ?3,
                         owner_claim_token = ?4,
                         owner_claimed_at = ?2,
                         owner_last_heartbeat_at = ?2,
                         owner_lease_expires_at = ?5
                     WHERE id = ?6
                       AND status IN (?7, ?1)
                       AND (
                           owner_worker_id IS NULL
                           OR owner_claim_token IS NULL
                           OR owner_lease_expires_at IS NULL
                           OR owner_lease_expires_at <= ?2
                       )",
                    rusqlite::params![
                        running,
                        claimed_at,
                        worker_id,
                        claim_token,
                        lease_expires_at,
                        run_id,
                        pending,
                    ],
                )
            })
            .await
            .map_err(db_err)?;

        Ok(changed > 0)
    }

    pub async fn heartbeat_run_ownership(
        &self,
        run_id: &str,
        worker_id: &str,
        claim_token: &str,
        heartbeat_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        let run_id = run_id.to_owned();
        let worker_id = worker_id.to_owned();
        let claim_token = claim_token.to_owned();
        let heartbeat_at = format_ts(&heartbeat_at);
        let lease_expires_at = format_ts(&lease_expires_at);
        let running = ManagedRunStatus::Running.as_str().to_string();

        let changed = self
            .conn
            .call(move |c| -> rusqlite::Result<usize> {
                c.execute(
                    "UPDATE runs
                     SET owner_last_heartbeat_at = ?1,
                         owner_lease_expires_at = ?2
                     WHERE id = ?3
                       AND status = ?4
                       AND owner_worker_id = ?5
                       AND owner_claim_token = ?6",
                    rusqlite::params![
                        heartbeat_at,
                        lease_expires_at,
                        run_id,
                        running,
                        worker_id,
                        claim_token,
                    ],
                )
            })
            .await
            .map_err(db_err)?;

        Ok(changed > 0)
    }

    pub async fn record_run_terminal_intent_if_owned(
        &self,
        run_id: &str,
        worker_id: &str,
        claim_token: &str,
        status: ManagedRunStatus,
        reason: Option<&str>,
    ) -> Result<bool> {
        if !matches!(
            status,
            ManagedRunStatus::Completed
                | ManagedRunStatus::Cancelled
                | ManagedRunStatus::Failed
                | ManagedRunStatus::TimedOut
        ) {
            return Err(HermesError::Config(format!(
                "terminal intent only supports completed/cancelled/failed/timed_out, got {}",
                status.as_str()
            )));
        }

        let run_id = run_id.to_owned();
        let worker_id = worker_id.to_owned();
        let claim_token = claim_token.to_owned();
        let status_str = status.as_str().to_string();
        let reason = reason.map(ToOwned::to_owned);
        let now = format_ts(&Utc::now());
        let cancel_requested_at = if status == ManagedRunStatus::Cancelled {
            Some(now.clone())
        } else {
            None
        };

        let changed = self
            .conn
            .call(move |c| -> rusqlite::Result<usize> {
                c.execute(
                    "UPDATE runs
                     SET updated_at = ?1,
                         cancel_requested_at = CASE
                             WHEN ?2 IS NOT NULL THEN COALESCE(cancel_requested_at, ?2)
                             ELSE cancel_requested_at
                         END,
                         terminal_status_hint = ?3,
                         terminal_reason_hint = ?4
                     WHERE id = ?5
                       AND owner_worker_id = ?6
                       AND owner_claim_token = ?7",
                    rusqlite::params![
                        now,
                        cancel_requested_at,
                        status_str,
                        reason,
                        run_id,
                        worker_id,
                        claim_token,
                    ],
                )
            })
            .await
            .map_err(db_err)?;

        Ok(changed > 0)
    }

    pub async fn update_run_status_if_owned(
        &self,
        run_id: &str,
        worker_id: &str,
        claim_token: &str,
        status: ManagedRunStatus,
        last_error: Option<&str>,
    ) -> Result<bool> {
        let run_id = run_id.to_owned();
        let worker_id = worker_id.to_owned();
        let claim_token = claim_token.to_owned();
        let status_str = status.as_str().to_string();
        let updated_at = format_ts(&Utc::now());
        let ended_at = if status.is_terminal() {
            Some(format_ts(&Utc::now()))
        } else {
            None
        };
        let last_error = last_error.map(ToOwned::to_owned);

        let changed = self
            .conn
            .call(move |c| -> rusqlite::Result<usize> {
                c.execute(
                    "UPDATE runs
                     SET status = ?1,
                         updated_at = ?2,
                         ended_at = COALESCE(?3, ended_at),
                         terminal_status_hint = NULL,
                         terminal_reason_hint = NULL,
                         owner_worker_id = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_worker_id END,
                         owner_claim_token = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_claim_token END,
                         owner_claimed_at = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_claimed_at END,
                         owner_last_heartbeat_at = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_last_heartbeat_at END,
                         owner_lease_expires_at = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_lease_expires_at END,
                         last_error = ?4
                     WHERE id = ?5
                       AND owner_worker_id = ?6
                       AND owner_claim_token = ?7",
                    rusqlite::params![
                        status_str,
                        updated_at,
                        ended_at,
                        last_error,
                        run_id,
                        worker_id,
                        claim_token,
                    ],
                )
            })
            .await
            .map_err(db_err)?;

        Ok(changed > 0)
    }

    pub async fn get_run(&self, run_id: &str) -> Result<Option<ManagedRun>> {
        let run_id = run_id.to_owned();
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<RawRun>> {
                c.query_row(
                    "SELECT id, agent_id, agent_version, status, model, session_id, prompt,
                            replay_of_run_id, started_at, updated_at, ended_at,
                            cancel_requested_at, terminal_status_hint, terminal_reason_hint,
                            owner_worker_id, owner_claim_token, owner_claimed_at,
                            owner_last_heartbeat_at, owner_lease_expires_at, last_error
                     FROM runs
                     WHERE id = ?1",
                    rusqlite::params![run_id],
                    |row| {
                        Ok(RawRun {
                            id: row.get(0)?,
                            agent_id: row.get(1)?,
                            agent_version: row.get(2)?,
                            status: row.get(3)?,
                            model: row.get(4)?,
                            session_id: row.get(5)?,
                            prompt: row.get(6)?,
                            replay_of_run_id: row.get(7)?,
                            started_at: row.get(8)?,
                            updated_at: row.get(9)?,
                            ended_at: row.get(10)?,
                            cancel_requested_at: row.get(11)?,
                            terminal_status_hint: row.get(12)?,
                            terminal_reason_hint: row.get(13)?,
                            owner_worker_id: row.get(14)?,
                            owner_claim_token: row.get(15)?,
                            owner_claimed_at: row.get(16)?,
                            owner_last_heartbeat_at: row.get(17)?,
                            owner_lease_expires_at: row.get(18)?,
                            last_error: row.get(19)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(db_err)?;

        raw.map(map_run).transpose()
    }

    pub async fn get_run_owner_snapshot(
        &self,
        run_id: &str,
    ) -> Result<Option<ManagedRunOwnerSnapshot>> {
        let run_id = run_id.to_owned();
        let raw = self
            .conn
            .call(move |c| {
                c.query_row(
                    "SELECT id, agent_id, agent_version, status, model, session_id, prompt,
                            replay_of_run_id, started_at, updated_at, ended_at,
                            cancel_requested_at, terminal_status_hint, terminal_reason_hint,
                            owner_worker_id, owner_claim_token, owner_claimed_at,
                            owner_last_heartbeat_at, owner_lease_expires_at, last_error
                     FROM runs
                     WHERE id = ?1",
                    rusqlite::params![run_id],
                    |row| {
                        Ok(RawRun {
                            id: row.get(0)?,
                            agent_id: row.get(1)?,
                            agent_version: row.get(2)?,
                            status: row.get(3)?,
                            model: row.get(4)?,
                            session_id: row.get(5)?,
                            prompt: row.get(6)?,
                            replay_of_run_id: row.get(7)?,
                            started_at: row.get(8)?,
                            updated_at: row.get(9)?,
                            ended_at: row.get(10)?,
                            cancel_requested_at: row.get(11)?,
                            terminal_status_hint: row.get(12)?,
                            terminal_reason_hint: row.get(13)?,
                            owner_worker_id: row.get(14)?,
                            owner_claim_token: row.get(15)?,
                            owner_claimed_at: row.get(16)?,
                            owner_last_heartbeat_at: row.get(17)?,
                            owner_lease_expires_at: row.get(18)?,
                            last_error: row.get(19)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(db_err)?;

        raw.map(map_run_owner_snapshot)
            .transpose()
            .map(|snapshot| snapshot.flatten())
    }

    pub async fn list_runs(&self, limit: usize) -> Result<Vec<ManagedRun>> {
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRun>> {
                let mut stmt = c.prepare(
                    "SELECT id, agent_id, agent_version, status, model, session_id, prompt,
                            replay_of_run_id, started_at, updated_at, ended_at,
                            cancel_requested_at, terminal_status_hint, terminal_reason_hint,
                            owner_worker_id, owner_claim_token, owner_claimed_at,
                            owner_last_heartbeat_at, owner_lease_expires_at, last_error
                     FROM runs
                     ORDER BY started_at DESC
                     LIMIT ?1",
                )?;
                stmt.query_map(rusqlite::params![limit as i64], |row| {
                    Ok(RawRun {
                        id: row.get(0)?,
                        agent_id: row.get(1)?,
                        agent_version: row.get(2)?,
                        status: row.get(3)?,
                        model: row.get(4)?,
                        session_id: row.get(5)?,
                        prompt: row.get(6)?,
                        replay_of_run_id: row.get(7)?,
                        started_at: row.get(8)?,
                        updated_at: row.get(9)?,
                        ended_at: row.get(10)?,
                        cancel_requested_at: row.get(11)?,
                        terminal_status_hint: row.get(12)?,
                        terminal_reason_hint: row.get(13)?,
                        owner_worker_id: row.get(14)?,
                        owner_claim_token: row.get(15)?,
                        owner_claimed_at: row.get(16)?,
                        owner_last_heartbeat_at: row.get(17)?,
                        owner_lease_expires_at: row.get(18)?,
                        last_error: row.get(19)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run).collect()
    }

    pub async fn list_interrupted_runs_pending_replay(
        &self,
        limit: usize,
    ) -> Result<Vec<ManagedRun>> {
        let interrupted = ManagedRunStatus::Interrupted.as_str().to_string();
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRun>> {
                let mut stmt = c.prepare(
                    "SELECT id, agent_id, agent_version, status, model, session_id, prompt,
                            replay_of_run_id, started_at, updated_at, ended_at,
                            cancel_requested_at, terminal_status_hint, terminal_reason_hint,
                            owner_worker_id, owner_claim_token, owner_claimed_at,
                            owner_last_heartbeat_at, owner_lease_expires_at, last_error
                     FROM runs
                     WHERE status = ?1
                       AND trim(prompt) <> ''
                       AND NOT EXISTS (
                           SELECT 1
                           FROM runs replay
                           WHERE replay.replay_of_run_id = runs.id
                       )
                     ORDER BY updated_at ASC
                     LIMIT ?2",
                )?;
                stmt.query_map(rusqlite::params![interrupted, limit as i64], |row| {
                    Ok(RawRun {
                        id: row.get(0)?,
                        agent_id: row.get(1)?,
                        agent_version: row.get(2)?,
                        status: row.get(3)?,
                        model: row.get(4)?,
                        session_id: row.get(5)?,
                        prompt: row.get(6)?,
                        replay_of_run_id: row.get(7)?,
                        started_at: row.get(8)?,
                        updated_at: row.get(9)?,
                        ended_at: row.get(10)?,
                        cancel_requested_at: row.get(11)?,
                        terminal_status_hint: row.get(12)?,
                        terminal_reason_hint: row.get(13)?,
                        owner_worker_id: row.get(14)?,
                        owner_claim_token: row.get(15)?,
                        owner_claimed_at: row.get(16)?,
                        owner_last_heartbeat_at: row.get(17)?,
                        owner_lease_expires_at: row.get(18)?,
                        last_error: row.get(19)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run).collect()
    }

    pub async fn request_run_cancel(
        &self,
        run_id: &str,
        requested_at: DateTime<Utc>,
    ) -> Result<()> {
        let run_id = run_id.to_owned();
        let requested_at = format_ts(&requested_at);
        let updated_at = format_ts(&Utc::now());

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE runs
                     SET cancel_requested_at = ?1, updated_at = ?2
                     WHERE id = ?3",
                    rusqlite::params![requested_at, updated_at, run_id],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }

    pub async fn record_run_terminal_intent(
        &self,
        run_id: &str,
        status: ManagedRunStatus,
        reason: Option<&str>,
    ) -> Result<()> {
        if !matches!(
            status,
            ManagedRunStatus::Completed
                | ManagedRunStatus::Cancelled
                | ManagedRunStatus::Failed
                | ManagedRunStatus::TimedOut
        ) {
            return Err(HermesError::Config(format!(
                "terminal intent only supports completed/cancelled/failed/timed_out, got {}",
                status.as_str()
            )));
        }

        let run_id = run_id.to_owned();
        let status_str = status.as_str().to_string();
        let reason = reason.map(ToOwned::to_owned);
        let now = format_ts(&Utc::now());
        let cancel_requested_at = if status == ManagedRunStatus::Cancelled {
            Some(now.clone())
        } else {
            None
        };

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE runs
                     SET updated_at = ?1,
                         cancel_requested_at = CASE
                             WHEN ?2 IS NOT NULL THEN COALESCE(cancel_requested_at, ?2)
                             ELSE cancel_requested_at
                         END,
                         terminal_status_hint = ?3,
                         terminal_reason_hint = ?4
                     WHERE id = ?5",
                    rusqlite::params![now, cancel_requested_at, status_str, reason, run_id],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }

    pub async fn update_run_status(
        &self,
        run_id: &str,
        status: ManagedRunStatus,
        last_error: Option<&str>,
    ) -> Result<()> {
        let run_id = run_id.to_owned();
        let status_str = status.as_str().to_string();
        let updated_at = format_ts(&Utc::now());
        let ended_at = if status.is_terminal() {
            Some(format_ts(&Utc::now()))
        } else {
            None
        };
        let last_error = last_error.map(ToOwned::to_owned);

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE runs
                     SET status = ?1,
                         updated_at = ?2,
                         ended_at = COALESCE(?3, ended_at),
                         terminal_status_hint = NULL,
                         terminal_reason_hint = NULL,
                         owner_worker_id = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_worker_id END,
                         owner_claim_token = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_claim_token END,
                         owner_claimed_at = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_claimed_at END,
                         owner_last_heartbeat_at = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_last_heartbeat_at END,
                         owner_lease_expires_at = CASE WHEN ?3 IS NOT NULL THEN NULL ELSE owner_lease_expires_at END,
                         last_error = ?4
                     WHERE id = ?5",
                    rusqlite::params![status_str, updated_at, ended_at, last_error, run_id],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }

    pub async fn reconcile_incomplete_runs(&self) -> Result<Vec<ManagedRun>> {
        let reconciled_at = format_ts(&Utc::now());
        let pending = ManagedRunStatus::Pending.as_str().to_string();
        let running = ManagedRunStatus::Running.as_str().to_string();

        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRun>> {
                let mut stmt = c.prepare(
                    "SELECT id, agent_id, agent_version, status, model, session_id, prompt,
                            replay_of_run_id, started_at, updated_at, ended_at,
                            cancel_requested_at, terminal_status_hint, terminal_reason_hint,
                            owner_worker_id, owner_claim_token, owner_claimed_at,
                            owner_last_heartbeat_at, owner_lease_expires_at, last_error
                     FROM runs
                     WHERE status IN (?1, ?2)
                       AND (
                           owner_worker_id IS NULL
                           OR owner_claim_token IS NULL
                           OR owner_lease_expires_at IS NULL
                           OR owner_lease_expires_at <= ?3
                       )
                     ORDER BY started_at ASC",
                )?;
                let mut raws = stmt
                    .query_map(rusqlite::params![pending, running, reconciled_at], |row| {
                        Ok(RawRun {
                            id: row.get(0)?,
                            agent_id: row.get(1)?,
                            agent_version: row.get(2)?,
                            status: row.get(3)?,
                            model: row.get(4)?,
                            session_id: row.get(5)?,
                            prompt: row.get(6)?,
                            replay_of_run_id: row.get(7)?,
                            started_at: row.get(8)?,
                            updated_at: row.get(9)?,
                            ended_at: row.get(10)?,
                            cancel_requested_at: row.get(11)?,
                            terminal_status_hint: row.get(12)?,
                            terminal_reason_hint: row.get(13)?,
                            owner_worker_id: row.get(14)?,
                            owner_claim_token: row.get(15)?,
                            owner_claimed_at: row.get(16)?,
                            owner_last_heartbeat_at: row.get(17)?,
                            owner_lease_expires_at: row.get(18)?,
                            last_error: row.get(19)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(stmt);

                if raws.is_empty() {
                    return Ok(raws);
                }

                let tx = c.transaction()?;
                for raw in &mut raws {
                    let reconciled = reconcile_incomplete_run(raw);
                    let status_str = reconciled.status.as_str().to_string();
                    let kind_str = reconciled.event_kind.as_str().to_string();
                    let run_id = raw.id.clone();
                    let ownership_release_metadata = raw.owner_worker_id.as_ref().map(|worker_id| {
                        serde_json::json!({
                            "worker_id": worker_id,
                            "reason": ownership_release_reason_str(&reconciled.status),
                            "owner_claimed_at": raw.owner_claimed_at.as_deref(),
                            "owner_last_heartbeat_at": raw.owner_last_heartbeat_at.as_deref(),
                            "owner_lease_expires_at": raw.owner_lease_expires_at.as_deref(),
                            "note": reconciled.event_message.as_deref(),
                        })
                    });
                    let ownership_release_message = raw.owner_worker_id.as_ref().map(|worker_id| {
                        ownership_release_message(worker_id, &reconciled.status)
                    });

                    tx.execute(
                        "UPDATE runs
                         SET status = ?1,
                             updated_at = ?2,
                             ended_at = COALESCE(ended_at, ?3),
                             terminal_status_hint = NULL,
                             terminal_reason_hint = NULL,
                             owner_worker_id = NULL,
                             owner_claim_token = NULL,
                             owner_claimed_at = NULL,
                             owner_last_heartbeat_at = NULL,
                             owner_lease_expires_at = NULL,
                             last_error = ?4
                         WHERE id = ?5",
                        rusqlite::params![
                            status_str,
                            reconciled_at,
                            reconciled_at,
                            reconciled.last_error,
                            run_id,
                        ],
                    )?;
                    let metadata_json = reconciled
                        .event_metadata
                        .as_ref()
                        .map(|metadata| {
                            serde_json::to_string(metadata)
                                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
                        })
                        .transpose()?;
                    tx.execute(
                        "INSERT INTO run_events
                            (run_id, kind, message, tool_name, tool_call_id, metadata, created_at)
                         VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5)",
                        rusqlite::params![
                            raw.id,
                            kind_str,
                            reconciled.event_message,
                            metadata_json,
                            reconciled_at
                        ],
                    )?;
                    if let (Some(message), Some(metadata)) = (
                        ownership_release_message.as_deref(),
                        ownership_release_metadata.as_ref(),
                    ) {
                        let metadata_json = serde_json::to_string(metadata).map_err(|e| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(e))
                        })?;
                        tx.execute(
                            "INSERT INTO run_events
                                (run_id, kind, message, tool_name, tool_call_id, metadata, created_at)
                             VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5)",
                            rusqlite::params![
                                raw.id,
                                ManagedRunEventKind::RunOwnershipReleased.as_str(),
                                message,
                                metadata_json,
                                reconciled_at
                            ],
                        )?;
                    }

                    raw.status = status_str;
                    raw.updated_at = reconciled_at.clone();
                    raw.ended_at = Some(reconciled_at.clone());
                    raw.owner_worker_id = None;
                    raw.owner_claim_token = None;
                    raw.owner_claimed_at = None;
                    raw.owner_last_heartbeat_at = None;
                    raw.owner_lease_expires_at = None;
                    raw.last_error = reconciled.last_error;
                }
                tx.commit()?;

                Ok(raws)
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run).collect()
    }

    pub async fn upsert_run_cleanup_resource(
        &self,
        run_id: &str,
        entry_id: u64,
        kind: ManagedRunCleanupResourceKind,
        label: &str,
        target_value: &str,
    ) -> Result<bool> {
        let run_id = run_id.to_owned();
        let entry_id = i64::try_from(entry_id).map_err(|_| {
            HermesError::Config("cleanup resource entry id out of range for i64".to_string())
        })?;
        let kind = kind.as_str().to_string();
        let label = label.to_owned();
        let target_value = target_value.to_owned();
        let now = format_ts(&Utc::now());

        self.conn
            .call(move |c| -> rusqlite::Result<bool> {
                let exists = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runs WHERE id = ?1)",
                    rusqlite::params![&run_id],
                    |row| row.get::<_, i64>(0),
                )? != 0;
                if !exists {
                    return Ok(false);
                }

                let mut created_at = now.clone();
                let existing_created_at: Option<String> = c
                    .query_row(
                        "SELECT created_at
                         FROM run_cleanup_resources
                         WHERE run_id = ?1 AND entry_id = ?2",
                        rusqlite::params![&run_id, entry_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing_created_at) = existing_created_at {
                    created_at = existing_created_at;
                }

                c.execute(
                    "INSERT INTO run_cleanup_resources
                        (run_id, entry_id, kind, label, target_value, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(run_id, entry_id) DO UPDATE SET
                        kind = excluded.kind,
                        label = excluded.label,
                        target_value = excluded.target_value,
                        created_at = excluded.created_at,
                        updated_at = excluded.updated_at",
                    rusqlite::params![
                        &run_id,
                        entry_id,
                        &kind,
                        &label,
                        &target_value,
                        created_at,
                        now
                    ],
                )?;
                Ok(true)
            })
            .await
            .map_err(db_err)
    }

    pub async fn delete_run_cleanup_resource(&self, run_id: &str, entry_id: u64) -> Result<bool> {
        let run_id = run_id.to_owned();
        let entry_id = i64::try_from(entry_id).map_err(|_| {
            HermesError::Config("cleanup resource entry id out of range for i64".to_string())
        })?;

        let deleted = self
            .conn
            .call(move |c| -> rusqlite::Result<usize> {
                c.execute(
                    "DELETE FROM run_cleanup_resources
                     WHERE run_id = ?1 AND entry_id = ?2",
                    rusqlite::params![run_id, entry_id],
                )
            })
            .await
            .map_err(db_err)?;
        Ok(deleted != 0)
    }

    pub async fn list_run_cleanup_resources(
        &self,
        run_id: &str,
    ) -> Result<Vec<ManagedRunCleanupResource>> {
        let run_id = run_id.to_owned();
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRunCleanupResource>> {
                let mut stmt = c.prepare(
                    "SELECT run_id, entry_id, kind, label, target_value, created_at, updated_at
                     FROM run_cleanup_resources
                     WHERE run_id = ?1
                     ORDER BY entry_id ASC",
                )?;
                stmt.query_map(rusqlite::params![run_id], |row| {
                    Ok(RawRunCleanupResource {
                        run_id: row.get(0)?,
                        entry_id: row.get(1)?,
                        kind: row.get(2)?,
                        label: row.get(3)?,
                        target_value: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run_cleanup_resource).collect()
    }

    pub async fn list_terminal_run_cleanup_resources(
        &self,
        limit: usize,
    ) -> Result<Vec<ManagedRunCleanupResource>> {
        let completed = ManagedRunStatus::Completed.as_str().to_string();
        let failed = ManagedRunStatus::Failed.as_str().to_string();
        let interrupted = ManagedRunStatus::Interrupted.as_str().to_string();
        let cancelled = ManagedRunStatus::Cancelled.as_str().to_string();
        let timed_out = ManagedRunStatus::TimedOut.as_str().to_string();
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRunCleanupResource>> {
                let mut stmt = c.prepare(
                    "SELECT rcr.run_id, rcr.entry_id, rcr.kind, rcr.label, rcr.target_value,
                            rcr.created_at, rcr.updated_at
                     FROM run_cleanup_resources rcr
                     JOIN runs r ON r.id = rcr.run_id
                     WHERE r.status IN (?1, ?2, ?3, ?4, ?5)
                     ORDER BY rcr.updated_at ASC, rcr.entry_id ASC
                     LIMIT ?6",
                )?;
                stmt.query_map(
                    rusqlite::params![
                        completed,
                        failed,
                        interrupted,
                        cancelled,
                        timed_out,
                        limit as i64
                    ],
                    |row| {
                        Ok(RawRunCleanupResource {
                            run_id: row.get(0)?,
                            entry_id: row.get(1)?,
                            kind: row.get(2)?,
                            label: row.get(3)?,
                            target_value: row.get(4)?,
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run_cleanup_resource).collect()
    }

    pub async fn append_run_event(
        &self,
        run_id: &str,
        event: &ManagedRunEventDraft,
    ) -> Result<ManagedRunEvent> {
        let run_id = run_id.to_owned();
        let kind = event.kind.as_str().to_string();
        let message = event.message.clone();
        let tool_name = event.tool_name.clone();
        let tool_call_id = event.tool_call_id.clone();
        let metadata = event
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| {
                HermesError::Config(format!("failed to serialize run event metadata: {e}"))
            })?;
        let created_at = format_ts(&Utc::now());

        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<RawRunEvent> {
                c.execute(
                    "INSERT INTO run_events
                        (run_id, kind, message, tool_name, tool_call_id, metadata, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        run_id,
                        kind,
                        message,
                        tool_name,
                        tool_call_id,
                        metadata,
                        created_at.clone()
                    ],
                )?;

                Ok(RawRunEvent {
                    id: c.last_insert_rowid(),
                    run_id,
                    kind,
                    message,
                    tool_name,
                    tool_call_id,
                    metadata,
                    created_at,
                })
            })
            .await
            .map_err(db_err)?;

        map_run_event(raw)
    }

    pub async fn append_run_artifact(
        &self,
        run_id: &str,
        artifact: &ManagedRunArtifactDraft,
    ) -> Result<ManagedRunArtifact> {
        let run_id = run_id.to_owned();
        let kind = artifact.kind.as_str().to_string();
        let label = artifact.label.clone();
        let tool_name = artifact.tool_name.clone();
        let tool_call_id = artifact.tool_call_id.clone();
        let content = artifact.content.clone();
        let metadata = artifact
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| {
                HermesError::Config(format!("failed to serialize run artifact metadata: {e}"))
            })?;
        let created_at = format_ts(&Utc::now());

        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<RawRunArtifact> {
                c.execute(
                    "INSERT INTO run_artifacts
                        (run_id, kind, label, tool_name, tool_call_id, content, metadata, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        run_id,
                        kind,
                        label,
                        tool_name,
                        tool_call_id,
                        content,
                        metadata,
                        created_at.clone()
                    ],
                )?;

                Ok(RawRunArtifact {
                    id: c.last_insert_rowid(),
                    run_id,
                    kind,
                    label,
                    tool_name,
                    tool_call_id,
                    content,
                    metadata,
                    created_at,
                })
            })
            .await
            .map_err(db_err)?;

        map_run_artifact(raw)
    }

    pub async fn list_run_events(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<ManagedRunEvent>> {
        let run_id = run_id.to_owned();
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRunEvent>> {
                let mut stmt = c.prepare(
                    "SELECT id, run_id, kind, message, tool_name, tool_call_id, metadata, created_at
                     FROM run_events
                     WHERE run_id = ?1
                     ORDER BY id ASC
                     LIMIT ?2",
                )?;
                stmt.query_map(rusqlite::params![run_id, limit as i64], |row| {
                    Ok(RawRunEvent {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        kind: row.get(2)?,
                        message: row.get(3)?,
                        tool_name: row.get(4)?,
                        tool_call_id: row.get(5)?,
                        metadata: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run_event).collect()
    }

    pub async fn list_run_events_tail(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<ManagedRunEvent>> {
        let run_id = run_id.to_owned();
        let mut raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRunEvent>> {
                let mut stmt = c.prepare(
                    "SELECT id, run_id, kind, message, tool_name, tool_call_id, metadata, created_at
                     FROM run_events
                     WHERE run_id = ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                stmt.query_map(rusqlite::params![run_id, limit as i64], |row| {
                    Ok(RawRunEvent {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        kind: row.get(2)?,
                        message: row.get(3)?,
                        tool_name: row.get(4)?,
                        tool_call_id: row.get(5)?,
                        metadata: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.reverse();
        raws.into_iter().map(map_run_event).collect()
    }

    pub async fn list_run_events_after(
        &self,
        run_id: &str,
        after_id: u64,
        limit: usize,
    ) -> Result<Vec<ManagedRunEvent>> {
        let run_id = run_id.to_owned();
        let after_id = i64::try_from(after_id)
            .map_err(|_| HermesError::Config("run event id out of range for i64".to_string()))?;
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRunEvent>> {
                let mut stmt = c.prepare(
                    "SELECT id, run_id, kind, message, tool_name, tool_call_id, metadata, created_at
                     FROM run_events
                     WHERE run_id = ?1 AND id > ?2
                     ORDER BY id ASC
                     LIMIT ?3",
                )?;
                stmt.query_map(rusqlite::params![run_id, after_id, limit as i64], |row| {
                    Ok(RawRunEvent {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        kind: row.get(2)?,
                        message: row.get(3)?,
                        tool_name: row.get(4)?,
                        tool_call_id: row.get(5)?,
                        metadata: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run_event).collect()
    }

    pub async fn list_run_artifacts(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<ManagedRunArtifact>> {
        let run_id = run_id.to_owned();
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRunArtifact>> {
                let mut stmt = c.prepare(
                    "SELECT id, run_id, kind, label, tool_name, tool_call_id, content, metadata, created_at
                     FROM run_artifacts
                     WHERE run_id = ?1
                     ORDER BY id ASC
                     LIMIT ?2",
                )?;
                stmt.query_map(rusqlite::params![run_id, limit as i64], |row| {
                    Ok(RawRunArtifact {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        kind: row.get(2)?,
                        label: row.get(3)?,
                        tool_name: row.get(4)?,
                        tool_call_id: row.get(5)?,
                        content: row.get(6)?,
                        metadata: row.get(7)?,
                        created_at: row.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run_artifact).collect()
    }

    pub async fn list_run_artifacts_tail(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<ManagedRunArtifact>> {
        let run_id = run_id.to_owned();
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRunArtifact>> {
                let mut stmt = c.prepare(
                    "SELECT id, run_id, kind, label, tool_name, tool_call_id, content, metadata, created_at
                     FROM run_artifacts
                     WHERE run_id = ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                stmt.query_map(rusqlite::params![run_id, limit as i64], |row| {
                    Ok(RawRunArtifact {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        kind: row.get(2)?,
                        label: row.get(3)?,
                        tool_name: row.get(4)?,
                        tool_call_id: row.get(5)?,
                        content: row.get(6)?,
                        metadata: row.get(7)?,
                        created_at: row.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        let mut raws = raws;
        raws.reverse();
        raws.into_iter().map(map_run_artifact).collect()
    }

    pub async fn load_run_replay_lineage(
        &self,
        run_id: &str,
        max_depth: usize,
    ) -> Result<Vec<ManagedRun>> {
        let mut lineage = Vec::new();
        let mut next_run_id = Some(run_id.to_string());
        let mut visited = HashSet::new();
        let depth_limit = max_depth.max(1);

        while let Some(current_run_id) = next_run_id.take() {
            if !visited.insert(current_run_id.clone()) {
                break;
            }

            let Some(run) = self.get_run(&current_run_id).await? else {
                break;
            };
            next_run_id = run.replay_of_run_id.clone();
            lineage.push(run);

            if lineage.len() >= depth_limit {
                break;
            }
        }

        lineage.reverse();
        Ok(lineage)
    }

    pub async fn get_latest_replay_child(
        &self,
        run_id: &str,
    ) -> Result<Option<(ManagedRun, usize)>> {
        let run_id = run_id.to_owned();
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<(RawRun, i64)>> {
                let mut stmt = c.prepare(
                    "SELECT id, agent_id, agent_version, status, model, session_id, prompt,
                            replay_of_run_id, started_at, updated_at, ended_at,
                            cancel_requested_at, terminal_status_hint, terminal_reason_hint,
                            owner_worker_id, owner_claim_token, owner_claimed_at,
                            owner_last_heartbeat_at, owner_lease_expires_at, last_error,
                            (SELECT COUNT(*) FROM runs child WHERE child.replay_of_run_id = ?1)
                     FROM runs
                     WHERE replay_of_run_id = ?1
                     ORDER BY updated_at DESC, started_at DESC, rowid DESC
                     LIMIT 1",
                )?;
                stmt.query_row(rusqlite::params![run_id], |row| {
                    Ok((
                        RawRun {
                            id: row.get(0)?,
                            agent_id: row.get(1)?,
                            agent_version: row.get(2)?,
                            status: row.get(3)?,
                            model: row.get(4)?,
                            session_id: row.get(5)?,
                            prompt: row.get(6)?,
                            replay_of_run_id: row.get(7)?,
                            started_at: row.get(8)?,
                            updated_at: row.get(9)?,
                            ended_at: row.get(10)?,
                            cancel_requested_at: row.get(11)?,
                            terminal_status_hint: row.get(12)?,
                            terminal_reason_hint: row.get(13)?,
                            owner_worker_id: row.get(14)?,
                            owner_claim_token: row.get(15)?,
                            owner_claimed_at: row.get(16)?,
                            owner_last_heartbeat_at: row.get(17)?,
                            owner_lease_expires_at: row.get(18)?,
                            last_error: row.get(19)?,
                        },
                        row.get(20)?,
                    ))
                })
                .optional()
            })
            .await
            .map_err(db_err)?;

        raw.map(|(raw_run, replay_count)| {
            let run = map_run(raw_run)?;
            let replay_count = usize::try_from(replay_count).map_err(|_| {
                HermesError::Config("replay child count out of range for usize".to_string())
            })?;
            Ok((run, replay_count))
        })
        .transpose()
    }

    pub async fn get_latest_replay_descendant(
        &self,
        run_id: &str,
        depth_limit: usize,
    ) -> Result<Option<(ManagedRun, usize)>> {
        if depth_limit == 0 {
            return Ok(None);
        }

        let mut current_run_id = run_id.to_string();
        let mut latest = None;
        let mut depth = 0usize;

        while depth < depth_limit {
            let Some((child, _)) = self.get_latest_replay_child(&current_run_id).await? else {
                break;
            };
            current_run_id = child.id.clone();
            depth += 1;
            latest = Some((child, depth));
        }

        Ok(latest)
    }

    pub async fn list_run_artifacts_with_replay_lineage(
        &self,
        run_id: &str,
        limit: usize,
        max_depth: usize,
    ) -> Result<(Vec<ManagedRun>, Vec<ManagedRunArtifact>)> {
        let lineage = self.load_run_replay_lineage(run_id, max_depth).await?;
        let limit = limit.max(1);
        let mut artifacts = Vec::new();

        for run in &lineage {
            artifacts.extend(self.list_run_artifacts_tail(&run.id, limit).await?);
        }

        if artifacts.len() > limit {
            artifacts = artifacts.split_off(artifacts.len() - limit);
        }

        Ok((lineage, artifacts))
    }

    pub async fn list_recent_run_events_by_kind(
        &self,
        kind: ManagedRunEventKind,
        limit: usize,
    ) -> Result<Vec<ManagedRunEvent>> {
        let kind = kind.as_str().to_string();
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRunEvent>> {
                let mut stmt = c.prepare(
                    "SELECT id, run_id, kind, message, tool_name, tool_call_id, metadata, created_at
                     FROM run_events
                     WHERE kind = ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                stmt.query_map(rusqlite::params![kind, limit as i64], |row| {
                    Ok(RawRunEvent {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        kind: row.get(2)?,
                        message: row.get(3)?,
                        tool_name: row.get(4)?,
                        tool_call_id: row.get(5)?,
                        metadata: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run_event).collect()
    }

    pub async fn get_latest_run_event_by_kind(
        &self,
        run_id: &str,
        kind: ManagedRunEventKind,
    ) -> Result<Option<ManagedRunEvent>> {
        let run_id = run_id.to_owned();
        let kind = kind.as_str().to_string();
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<RawRunEvent>> {
                c.query_row(
                    "SELECT id, run_id, kind, message, tool_name, tool_call_id, metadata, created_at
                     FROM run_events
                     WHERE run_id = ?1 AND kind = ?2
                     ORDER BY id DESC
                     LIMIT 1",
                    rusqlite::params![run_id, kind],
                    |row| {
                        Ok(RawRunEvent {
                            id: row.get(0)?,
                            run_id: row.get(1)?,
                            kind: row.get(2)?,
                            message: row.get(3)?,
                            tool_name: row.get(4)?,
                            tool_call_id: row.get(5)?,
                            metadata: row.get(6)?,
                            created_at: row.get(7)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(db_err)?;

        raw.map(map_run_event).transpose()
    }
}

#[derive(Debug)]
struct RawAgent {
    id: String,
    name: String,
    latest_version: i64,
    archived: i64,
    created_at: String,
    updated_at: String,
}

#[derive(Debug)]
struct RawAgentVersion {
    agent_id: String,
    version: i64,
    model: String,
    base_url: Option<String>,
    system_prompt: String,
    allowed_tools: String,
    allowed_skills: String,
    max_iterations: i64,
    temperature: f64,
    approval_policy: String,
    timeout_secs: i64,
    created_at: String,
}

#[derive(Debug)]
struct RawRun {
    id: String,
    agent_id: String,
    agent_version: i64,
    status: String,
    model: String,
    session_id: Option<String>,
    prompt: String,
    replay_of_run_id: Option<String>,
    started_at: String,
    updated_at: String,
    ended_at: Option<String>,
    cancel_requested_at: Option<String>,
    terminal_status_hint: Option<String>,
    terminal_reason_hint: Option<String>,
    owner_worker_id: Option<String>,
    owner_claim_token: Option<String>,
    owner_claimed_at: Option<String>,
    owner_last_heartbeat_at: Option<String>,
    owner_lease_expires_at: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug)]
struct RawRunEvent {
    id: i64,
    run_id: String,
    kind: String,
    message: Option<String>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    metadata: Option<String>,
    created_at: String,
}

#[derive(Debug)]
struct RawRunArtifact {
    id: i64,
    run_id: String,
    kind: String,
    label: String,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
    content: String,
    metadata: Option<String>,
    created_at: String,
}

#[derive(Debug)]
struct RawRunCleanupResource {
    run_id: String,
    entry_id: i64,
    kind: String,
    label: String,
    target_value: String,
    created_at: String,
    updated_at: String,
}

struct ReconciledIncompleteRun {
    status: ManagedRunStatus,
    event_kind: ManagedRunEventKind,
    event_message: Option<String>,
    event_metadata: Option<serde_json::Value>,
    last_error: Option<String>,
}

fn reconcile_incomplete_run(raw: &RawRun) -> ReconciledIncompleteRun {
    if let Some(status) = raw
        .terminal_status_hint
        .as_deref()
        .and_then(ManagedRunStatus::parse)
        .filter(|status| {
            matches!(
                status,
                ManagedRunStatus::Completed
                    | ManagedRunStatus::Cancelled
                    | ManagedRunStatus::Failed
                    | ManagedRunStatus::Interrupted
                    | ManagedRunStatus::TimedOut
            )
        })
    {
        let event_message = raw
            .terminal_reason_hint
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| terminal_intent_reconcile_message(&status).to_string());
        let last_error = if status == ManagedRunStatus::Completed {
            None
        } else {
            Some(event_message.clone())
        };
        return ReconciledIncompleteRun {
            status: status.clone(),
            event_kind: terminal_event_kind(&status),
            event_message: Some(event_message),
            event_metadata: None,
            last_error,
        };
    }

    if raw.cancel_requested_at.is_some() {
        let message = "gateway restarted after cancel was requested".to_string();
        ReconciledIncompleteRun {
            status: ManagedRunStatus::Cancelled,
            event_kind: ManagedRunEventKind::RunCancelled,
            event_message: Some(message.clone()),
            event_metadata: None,
            last_error: Some(message),
        }
    } else {
        let lease_expired = raw.owner_worker_id.is_some()
            || raw.owner_claim_token.is_some()
            || raw.owner_lease_expires_at.is_some();
        let message = if lease_expired {
            "managed run interrupted after worker lease expired during execution".to_string()
        } else {
            "managed run interrupted before managed run ownership was established".to_string()
        };
        let event_metadata = Some(if lease_expired {
            serde_json::json!({
                "cause": "lease_expired",
                "owner_worker_id": raw.owner_worker_id.as_deref(),
                "owner_claimed_at": raw.owner_claimed_at.as_deref(),
                "owner_last_heartbeat_at": raw.owner_last_heartbeat_at.as_deref(),
                "owner_lease_expires_at": raw.owner_lease_expires_at.as_deref(),
            })
        } else {
            serde_json::json!({
                "cause": "ownership_not_established",
            })
        });
        ReconciledIncompleteRun {
            status: ManagedRunStatus::Interrupted,
            event_kind: ManagedRunEventKind::RunInterrupted,
            event_message: Some(message.clone()),
            event_metadata,
            last_error: Some(message),
        }
    }
}

fn terminal_event_kind(status: &ManagedRunStatus) -> ManagedRunEventKind {
    match status {
        ManagedRunStatus::Completed => ManagedRunEventKind::RunCompleted,
        ManagedRunStatus::Cancelled => ManagedRunEventKind::RunCancelled,
        ManagedRunStatus::Failed => ManagedRunEventKind::RunFailed,
        ManagedRunStatus::Interrupted => ManagedRunEventKind::RunInterrupted,
        ManagedRunStatus::TimedOut => ManagedRunEventKind::RunTimedOut,
        ManagedRunStatus::Pending | ManagedRunStatus::Running => {
            unreachable!("non-terminal or unsupported status used for terminal event kind")
        }
    }
}

fn ownership_release_reason_str(status: &ManagedRunStatus) -> &'static str {
    match status {
        ManagedRunStatus::Completed => "completed",
        ManagedRunStatus::Failed => "failed",
        ManagedRunStatus::Cancelled => "cancelled",
        ManagedRunStatus::TimedOut => "timed_out",
        ManagedRunStatus::Interrupted => "interrupted",
        ManagedRunStatus::Pending | ManagedRunStatus::Running => "interrupted",
    }
}

fn ownership_release_message(worker_id: &str, status: &ManagedRunStatus) -> String {
    let reason = match status {
        ManagedRunStatus::Completed => "completed the run",
        ManagedRunStatus::Failed => "failed the run",
        ManagedRunStatus::Cancelled => "cancelled the run",
        ManagedRunStatus::TimedOut => "timed out the run",
        ManagedRunStatus::Interrupted => "lost ownership when the run was interrupted",
        ManagedRunStatus::Pending | ManagedRunStatus::Running => "stopped owning the run",
    };
    format!("worker {worker_id} released managed run ownership after it {reason}")
}

fn terminal_intent_reconcile_message(status: &ManagedRunStatus) -> &'static str {
    match status {
        ManagedRunStatus::Completed => {
            "gateway restarted while managed run completion was being recorded"
        }
        ManagedRunStatus::Cancelled => {
            "gateway restarted while managed run cancellation was being recorded"
        }
        ManagedRunStatus::Failed => {
            "gateway restarted while managed run failure was being recorded"
        }
        ManagedRunStatus::Interrupted => {
            "gateway restarted while managed run interruption was being recorded"
        }
        ManagedRunStatus::TimedOut => {
            "gateway restarted while managed run timeout was being recorded"
        }
        ManagedRunStatus::Pending | ManagedRunStatus::Running => {
            unreachable!("unsupported status used for terminal intent message")
        }
    }
}

fn db_err(e: impl std::fmt::Display) -> HermesError {
    HermesError::Config(e.to_string())
}

fn map_cleanup_resource_kind_from_durable(
    kind: DurableCleanupResourceKind,
) -> ManagedRunCleanupResourceKind {
    match kind {
        DurableCleanupResourceKind::Pid => ManagedRunCleanupResourceKind::Pid,
        DurableCleanupResourceKind::ProcessGroup => ManagedRunCleanupResourceKind::ProcessGroup,
        DurableCleanupResourceKind::BrowserSession => ManagedRunCleanupResourceKind::BrowserSession,
        DurableCleanupResourceKind::McpHttpResourceSubscription => {
            ManagedRunCleanupResourceKind::McpHttpResourceSubscription
        }
        DurableCleanupResourceKind::McpHttpSession => ManagedRunCleanupResourceKind::McpHttpSession,
    }
}

fn format_ts(value: &DateTime<Utc>) -> String {
    value.to_rfc3339()
}

fn parse_ts(value: &str, field: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|ts| ts.with_timezone(&Utc))
        .map_err(|e| HermesError::Config(format!("failed to parse {field}: {e}")))
}

fn parse_json_string_vec(value: &str, field: &str) -> Result<Vec<String>> {
    serde_json::from_str(value)
        .map_err(|e| HermesError::Config(format!("failed to parse {field}: {e}")))
}

fn map_agent(raw: RawAgent) -> Result<ManagedAgent> {
    Ok(ManagedAgent {
        id: raw.id,
        name: raw.name,
        latest_version: raw
            .latest_version
            .try_into()
            .map_err(|_| HermesError::Config("latest_version out of range for u32".to_string()))?,
        archived: raw.archived != 0,
        created_at: parse_ts(&raw.created_at, "agents.created_at")?,
        updated_at: parse_ts(&raw.updated_at, "agents.updated_at")?,
    })
}

fn map_agent_version(raw: RawAgentVersion) -> Result<ManagedAgentVersion> {
    Ok(ManagedAgentVersion {
        agent_id: raw.agent_id,
        version: raw
            .version
            .try_into()
            .map_err(|_| HermesError::Config("version out of range for u32".to_string()))?,
        model: raw.model,
        base_url: raw.base_url,
        system_prompt: raw.system_prompt,
        allowed_tools: parse_json_string_vec(&raw.allowed_tools, "agent_versions.allowed_tools")?,
        allowed_skills: parse_json_string_vec(
            &raw.allowed_skills,
            "agent_versions.allowed_skills",
        )?,
        max_iterations: raw
            .max_iterations
            .try_into()
            .map_err(|_| HermesError::Config("max_iterations out of range for u32".to_string()))?,
        temperature: raw.temperature,
        approval_policy: ManagedApprovalPolicy::parse(&raw.approval_policy).ok_or_else(|| {
            HermesError::Config(format!(
                "unknown approval policy in DB: {}",
                raw.approval_policy
            ))
        })?,
        timeout_secs: raw
            .timeout_secs
            .try_into()
            .map_err(|_| HermesError::Config("timeout_secs out of range for u32".to_string()))?,
        created_at: parse_ts(&raw.created_at, "agent_versions.created_at")?,
    })
}

fn map_run(raw: RawRun) -> Result<ManagedRun> {
    Ok(ManagedRun {
        id: raw.id,
        agent_id: raw.agent_id,
        agent_version: raw
            .agent_version
            .try_into()
            .map_err(|_| HermesError::Config("agent_version out of range for u32".to_string()))?,
        status: ManagedRunStatus::parse(&raw.status).ok_or_else(|| {
            HermesError::Config(format!("unknown run status in DB: {}", raw.status))
        })?,
        model: raw.model,
        session_id: raw.session_id,
        prompt: raw.prompt,
        replay_of_run_id: raw.replay_of_run_id,
        started_at: parse_ts(&raw.started_at, "runs.started_at")?,
        updated_at: parse_ts(&raw.updated_at, "runs.updated_at")?,
        ended_at: raw
            .ended_at
            .as_deref()
            .map(|value| parse_ts(value, "runs.ended_at"))
            .transpose()?,
        cancel_requested_at: raw
            .cancel_requested_at
            .as_deref()
            .map(|value| parse_ts(value, "runs.cancel_requested_at"))
            .transpose()?,
        last_error: raw.last_error,
    })
}

fn map_run_owner_snapshot(raw: RawRun) -> Result<Option<ManagedRunOwnerSnapshot>> {
    let Some(worker_id) = raw.owner_worker_id else {
        return Ok(None);
    };

    let claimed_at = raw
        .owner_claimed_at
        .as_deref()
        .map(|value| parse_ts(value, "runs.owner_claimed_at"))
        .transpose()?;
    let last_heartbeat_at = raw
        .owner_last_heartbeat_at
        .as_deref()
        .map(|value| parse_ts(value, "runs.owner_last_heartbeat_at"))
        .transpose()?;
    let lease_expires_at = raw
        .owner_lease_expires_at
        .as_deref()
        .map(|value| parse_ts(value, "runs.owner_lease_expires_at"))
        .transpose()?;
    let state = match lease_expires_at {
        Some(lease_expires_at) if lease_expires_at > Utc::now() => ManagedRunOwnerState::Active,
        Some(_) => ManagedRunOwnerState::Expired,
        None => ManagedRunOwnerState::Incomplete,
    };

    Ok(Some(ManagedRunOwnerSnapshot {
        worker_id,
        state,
        claimed_at,
        last_heartbeat_at,
        lease_expires_at,
    }))
}

fn map_run_event(raw: RawRunEvent) -> Result<ManagedRunEvent> {
    Ok(ManagedRunEvent {
        id: raw
            .id
            .try_into()
            .map_err(|_| HermesError::Config("run event id out of range for u64".to_string()))?,
        run_id: raw.run_id,
        kind: ManagedRunEventKind::parse(&raw.kind).ok_or_else(|| {
            HermesError::Config(format!("unknown run event kind in DB: {}", raw.kind))
        })?,
        message: raw.message,
        tool_name: raw.tool_name,
        tool_call_id: raw.tool_call_id,
        metadata: raw
            .metadata
            .as_deref()
            .map(|value| {
                serde_json::from_str(value).map_err(|e| {
                    HermesError::Config(format!("failed to parse run_events.metadata: {e}"))
                })
            })
            .transpose()?,
        created_at: parse_ts(&raw.created_at, "run_events.created_at")?,
    })
}

fn map_run_artifact(raw: RawRunArtifact) -> Result<ManagedRunArtifact> {
    Ok(ManagedRunArtifact {
        id: raw
            .id
            .try_into()
            .map_err(|_| HermesError::Config("run artifact id out of range for u64".to_string()))?,
        run_id: raw.run_id,
        kind: ManagedRunArtifactKind::parse(&raw.kind).ok_or_else(|| {
            HermesError::Config(format!("unknown run artifact kind in DB: {}", raw.kind))
        })?,
        label: raw.label,
        tool_name: raw.tool_name,
        tool_call_id: raw.tool_call_id,
        content: raw.content,
        metadata: raw
            .metadata
            .as_deref()
            .map(|value| {
                serde_json::from_str(value).map_err(|e| {
                    HermesError::Config(format!("failed to parse run_artifacts.metadata: {e}"))
                })
            })
            .transpose()?,
        created_at: parse_ts(&raw.created_at, "run_artifacts.created_at")?,
    })
}

fn map_run_cleanup_resource(raw: RawRunCleanupResource) -> Result<ManagedRunCleanupResource> {
    Ok(ManagedRunCleanupResource {
        run_id: raw.run_id,
        entry_id: raw.entry_id.try_into().map_err(|_| {
            HermesError::Config("cleanup resource entry id out of range for u64".to_string())
        })?,
        kind: ManagedRunCleanupResourceKind::parse(&raw.kind).ok_or_else(|| {
            HermesError::Config(format!("unknown cleanup resource kind in DB: {}", raw.kind))
        })?,
        label: raw.label,
        target_value: raw.target_value,
        created_at: parse_ts(&raw.created_at, "run_cleanup_resources.created_at")?,
        updated_at: parse_ts(&raw.updated_at, "run_cleanup_resources.updated_at")?,
    })
}

fn has_column(c: &rusqlite::Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[async_trait]
impl DurableCleanupRecorder for ManagedStore {
    async fn register(
        &self,
        session_id: &str,
        entry_id: u64,
        resource: DurableCleanupResource,
    ) -> std::result::Result<(), String> {
        self.upsert_run_cleanup_resource(
            session_id,
            entry_id,
            map_cleanup_resource_kind_from_durable(resource.kind),
            &resource.label,
            &resource.target_value,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    async fn unregister(&self, session_id: &str, entry_id: u64) -> std::result::Result<(), String> {
        self.delete_run_cleanup_resource(session_id, entry_id)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn temp_db() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        (dir, path)
    }

    #[tokio::test]
    async fn create_and_fetch_agent_round_trips() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("code-reviewer");
        store.create_agent(&agent).await.unwrap();

        let fetched = store.get_agent(&agent.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, agent.id);
        assert_eq!(fetched.name, "code-reviewer");
        assert_eq!(fetched.latest_version, 0);

        let by_name = store
            .get_agent_by_name("code-reviewer")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_name.id, agent.id);
    }

    #[tokio::test]
    async fn create_version_updates_latest_version_and_lists_desc() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("planner");
        store.create_agent(&agent).await.unwrap();

        let mut v1 = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o", "v1 prompt");
        v1.allowed_tools = vec!["read_file".into(), "search_files".into()];
        let mut v2 = ManagedAgentVersion::new(&agent.id, 2, "anthropic/claude-sonnet", "v2 prompt");
        v2.allowed_skills = vec!["git-review".into()];
        v2.approval_policy = ManagedApprovalPolicy::Deny;

        store.create_agent_version(&v1).await.unwrap();
        store.create_agent_version(&v2).await.unwrap();

        let agent_after = store.get_agent(&agent.id).await.unwrap().unwrap();
        assert_eq!(agent_after.latest_version, 2);

        let versions = store.list_agent_versions(&agent.id).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, 2);
        assert_eq!(versions[1].version, 1);
        assert_eq!(versions[0].allowed_skills, vec!["git-review".to_string()]);
        assert_eq!(versions[1].allowed_tools.len(), 2);
        assert_eq!(versions[0].approval_policy, ManagedApprovalPolicy::Deny);
    }

    #[tokio::test]
    async fn create_next_agent_version_increments_atomically() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("publisher");
        store.create_agent(&agent).await.unwrap();

        let mut draft_v1 = ManagedAgentVersionDraft::new("openai/gpt-4o-mini", "v1 prompt");
        draft_v1.allowed_tools = vec!["read_file".to_string()];
        draft_v1.max_iterations = 32;
        draft_v1.temperature = 0.1;
        draft_v1.timeout_secs = 120;
        let v1 = store
            .create_next_agent_version(&agent.id, &draft_v1)
            .await
            .unwrap();

        let mut draft_v2 = ManagedAgentVersionDraft::new("openai/gpt-4o", "v2 prompt");
        draft_v2.base_url = Some("https://example.com/v1".to_string());
        draft_v2.allowed_skills = vec!["deploy".to_string()];
        draft_v2.max_iterations = 64;
        draft_v2.temperature = 0.2;
        draft_v2.approval_policy = ManagedApprovalPolicy::Deny;
        draft_v2.timeout_secs = 240;
        let v2 = store
            .create_next_agent_version(&agent.id, &draft_v2)
            .await
            .unwrap();

        assert_eq!(v1.version, 1);
        assert_eq!(v2.version, 2);
        assert_eq!(v2.base_url.as_deref(), Some("https://example.com/v1"));
        assert_eq!(v2.allowed_skills, vec!["deploy".to_string()]);
        assert_eq!(v2.approval_policy, ManagedApprovalPolicy::Deny);

        let agent_after = store.get_agent(&agent.id).await.unwrap().unwrap();
        assert_eq!(agent_after.latest_version, 2);
    }

    #[tokio::test]
    async fn archive_agent_marks_flag_without_deleting_versions() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("archivable");
        store.create_agent(&agent).await.unwrap();
        let draft = ManagedAgentVersionDraft::new("openai/gpt-4o-mini", "v1 prompt");
        store
            .create_next_agent_version(&agent.id, &draft)
            .await
            .unwrap();

        store.archive_agent(&agent.id).await.unwrap();

        let archived = store.get_agent(&agent.id).await.unwrap().unwrap();
        assert!(archived.archived);

        let versions = store.list_agent_versions(&agent.id).await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, 1);
    }

    #[tokio::test]
    async fn create_run_and_update_status() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("runner");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "run prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        run.session_id = Some("msess_test".to_string());
        run.prompt = "review the latest diff".to_string();
        run.replay_of_run_id = Some("run_previous".to_string());
        store.create_run(&run).await.unwrap();

        store.request_run_cancel(&run.id, Utc::now()).await.unwrap();
        store
            .update_run_status(
                &run.id,
                ManagedRunStatus::Cancelled,
                Some("aborted by user"),
            )
            .await
            .unwrap();

        let fetched = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(fetched.status, ManagedRunStatus::Cancelled);
        assert_eq!(fetched.session_id.as_deref(), Some("msess_test"));
        assert_eq!(fetched.prompt, "review the latest diff");
        assert_eq!(fetched.replay_of_run_id.as_deref(), Some("run_previous"));
        assert!(fetched.cancel_requested_at.is_some());
        assert!(fetched.ended_at.is_some());
        assert_eq!(fetched.last_error.as_deref(), Some("aborted by user"));

        let listed = store.list_runs(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, run.id);
    }

    #[tokio::test]
    async fn append_and_list_run_events() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("observer");
        store.create_agent(&agent).await.unwrap();
        let version =
            ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "observe everything");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        store.create_run(&run).await.unwrap();

        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run created".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolCallStarted,
                    message: None,
                    tool_name: Some("read_file".to_string()),
                    tool_call_id: Some("call_123".to_string()),
                    metadata: Some(serde_json::json!({
                        "receipt_id": "rec_123",
                        "record_hash": "sha256:abc",
                    })),
                },
            )
            .await
            .unwrap();

        let events = store.list_run_events(&run.id, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, ManagedRunEventKind::RunCreated);
        assert_eq!(events[0].message.as_deref(), Some("managed run created"));
        assert_eq!(events[1].kind, ManagedRunEventKind::ToolCallStarted);
        assert_eq!(events[1].tool_name.as_deref(), Some("read_file"));
        assert_eq!(events[1].tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(
            events[1].metadata.as_ref().unwrap()["receipt_id"],
            "rec_123"
        );
        assert!(events[0].id < events[1].id);

        let tail = store.list_run_events_tail(&run.id, 1).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].kind, ManagedRunEventKind::ToolCallStarted);

        let after_first = store
            .list_run_events_after(&run.id, events[0].id, 10)
            .await
            .unwrap();
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].id, events[1].id);
    }

    #[tokio::test]
    async fn append_and_list_run_artifacts() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("artifact-roundtrip");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        store.create_run(&run).await.unwrap();

        let assistant = store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "Final answer".to_string(),
                    metadata: Some(serde_json::json!({
                        "checkpointed": true,
                    })),
                },
            )
            .await
            .unwrap();
        let tool = store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::ToolOutput,
                    label: "browser".to_string(),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: Some("call_browser_1".to_string()),
                    content: "{\"url\":\"https://example.com\"}".to_string(),
                    metadata: Some(serde_json::json!({
                        "role": "tool",
                    })),
                },
            )
            .await
            .unwrap();

        assert_eq!(assistant.kind, ManagedRunArtifactKind::AssistantOutput);
        assert_eq!(assistant.label, "assistant_output");
        assert_eq!(assistant.content, "Final answer");
        assert_eq!(
            assistant
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("checkpointed"))
                .and_then(|value| value.as_bool()),
            Some(true)
        );

        assert_eq!(tool.kind, ManagedRunArtifactKind::ToolOutput);
        assert_eq!(tool.label, "browser");
        assert_eq!(tool.tool_name.as_deref(), Some("browser"));
        assert_eq!(tool.tool_call_id.as_deref(), Some("call_browser_1"));

        let artifacts = store.list_run_artifacts(&run.id, 10).await.unwrap();
        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].id, assistant.id);
        assert_eq!(artifacts[0].kind, ManagedRunArtifactKind::AssistantOutput);
        assert_eq!(artifacts[1].id, tool.id);
        assert_eq!(artifacts[1].tool_name.as_deref(), Some("browser"));
    }

    #[tokio::test]
    async fn list_run_artifacts_with_replay_lineage_returns_root_first() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("artifact-lineage");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let root = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        store.create_run(&root).await.unwrap();

        let mut replay = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        replay.replay_of_run_id = Some(root.id.clone());
        store.create_run(&replay).await.unwrap();

        store
            .append_run_artifact(
                &root.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "from root".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();
        store
            .append_run_artifact(
                &replay.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "from replay".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        let (lineage, artifacts) = store
            .list_run_artifacts_with_replay_lineage(&replay.id, 10, 8)
            .await
            .unwrap();

        assert_eq!(lineage.len(), 2);
        assert_eq!(lineage[0].id, root.id);
        assert_eq!(lineage[1].id, replay.id);
        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].run_id, root.id);
        assert_eq!(artifacts[0].content, "from root");
        assert_eq!(artifacts[1].run_id, replay.id);
        assert_eq!(artifacts[1].content, "from replay");

        let (_lineage, limited_artifacts) = store
            .list_run_artifacts_with_replay_lineage(&replay.id, 1, 8)
            .await
            .unwrap();
        assert_eq!(limited_artifacts.len(), 1);
        assert_eq!(limited_artifacts[0].run_id, replay.id);
        assert_eq!(limited_artifacts[0].content, "from replay");
    }

    #[tokio::test]
    async fn list_run_artifacts_with_replay_lineage_uses_latest_artifacts_with_small_limits() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("artifact-lineage-tail");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let root = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        store.create_run(&root).await.unwrap();

        let mut replay = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        replay.replay_of_run_id = Some(root.id.clone());
        store.create_run(&replay).await.unwrap();

        for content in ["root-1", "root-2"] {
            store
                .append_run_artifact(
                    &root.id,
                    &ManagedRunArtifactDraft {
                        kind: ManagedRunArtifactKind::AssistantOutput,
                        label: "assistant_output".to_string(),
                        tool_name: None,
                        tool_call_id: None,
                        content: content.to_string(),
                        metadata: None,
                    },
                )
                .await
                .unwrap();
        }
        for content in ["replay-1", "replay-2"] {
            store
                .append_run_artifact(
                    &replay.id,
                    &ManagedRunArtifactDraft {
                        kind: ManagedRunArtifactKind::AssistantOutput,
                        label: "assistant_output".to_string(),
                        tool_name: None,
                        tool_call_id: None,
                        content: content.to_string(),
                        metadata: None,
                    },
                )
                .await
                .unwrap();
        }

        let (_lineage, limited_artifacts) = store
            .list_run_artifacts_with_replay_lineage(&replay.id, 2, 8)
            .await
            .unwrap();
        assert_eq!(limited_artifacts.len(), 2);
        assert_eq!(limited_artifacts[0].content, "replay-1");
        assert_eq!(limited_artifacts[1].content, "replay-2");
    }

    #[tokio::test]
    async fn list_recent_run_events_by_kind_returns_newest_matches_across_runs() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("event-query");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let mut run_a = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run_a.status = ManagedRunStatus::Failed;
        store.create_run(&run_a).await.unwrap();
        let mut run_b = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run_b.status = ManagedRunStatus::Failed;
        store.create_run(&run_b).await.unwrap();

        store
            .append_run_event(
                &run_a.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunMcpAdmissionRejected,
                    message: Some("first rejection".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({"code": "disabled"})),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run_b.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunFailed,
                    message: Some("ignore me".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run_b.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunMcpAdmissionRejected,
                    message: Some("second rejection".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({"code": "invalid_operator_policy"})),
                },
            )
            .await
            .unwrap();

        let events = store
            .list_recent_run_events_by_kind(ManagedRunEventKind::RunMcpAdmissionRejected, 10)
            .await
            .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].run_id, run_b.id);
        assert_eq!(events[0].message.as_deref(), Some("second rejection"));
        assert_eq!(events[1].run_id, run_a.id);
        assert_eq!(events[1].message.as_deref(), Some("first rejection"));
    }

    #[tokio::test]
    async fn get_latest_run_event_by_kind_returns_newest_match_for_run() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("event-latest");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "query");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Failed;
        store.create_run(&run).await.unwrap();

        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunMcpAdmissionRejected,
                    message: Some("first rejection".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({"code": "disabled"})),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunFailed,
                    message: Some("ignore me".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunMcpAdmissionRejected,
                    message: Some("second rejection".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({"code": "invalid_operator_policy"})),
                },
            )
            .await
            .unwrap();

        let latest = store
            .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunMcpAdmissionRejected)
            .await
            .unwrap()
            .expect("latest rejection event should exist");

        assert_eq!(latest.run_id, run.id);
        assert_eq!(latest.message.as_deref(), Some("second rejection"));
        assert_eq!(
            latest
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("code"))
                .and_then(|value| value.as_str()),
            Some("invalid_operator_policy")
        );
    }

    #[tokio::test]
    async fn reconcile_incomplete_runs_marks_ownerless_running_run_interrupted() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("reconcile-interrupted");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        store.create_run(&run).await.unwrap();

        let reconciled = store.reconcile_incomplete_runs().await.unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].status, ManagedRunStatus::Interrupted);
        assert_eq!(
            reconciled[0].last_error.as_deref(),
            Some("managed run interrupted before managed run ownership was established")
        );
        assert!(reconciled[0].ended_at.is_some());

        let stored = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::Interrupted);

        let events = store.list_run_events(&run.id, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ManagedRunEventKind::RunInterrupted);
        assert_eq!(
            events[0].message.as_deref(),
            Some("managed run interrupted before managed run ownership was established")
        );
        assert_eq!(
            events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("cause"))
                .and_then(|value| value.as_str()),
            Some("ownership_not_established")
        );
    }

    #[tokio::test]
    async fn reconcile_incomplete_runs_marks_cancel_requested_run_cancelled() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("reconcile-cancelled");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        store.create_run(&run).await.unwrap();
        store.request_run_cancel(&run.id, Utc::now()).await.unwrap();

        let reconciled = store.reconcile_incomplete_runs().await.unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].status, ManagedRunStatus::Cancelled);
        assert!(reconciled[0].cancel_requested_at.is_some());
        assert_eq!(
            reconciled[0].last_error.as_deref(),
            Some("gateway restarted after cancel was requested")
        );

        let stored = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::Cancelled);
        assert!(stored.cancel_requested_at.is_some());

        let events = store.list_run_events(&run.id, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ManagedRunEventKind::RunCancelled);
        assert_eq!(
            events[0].message.as_deref(),
            Some("gateway restarted after cancel was requested")
        );
    }

    #[tokio::test]
    async fn reconcile_incomplete_runs_skips_runs_with_live_owner_lease() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("reconcile-live-lease");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Pending;
        store.create_run(&run).await.unwrap();
        let claimed = store
            .claim_run_ownership(
                &run.id,
                "gw_live",
                "claim_live",
                Utc::now(),
                Utc::now() + chrono::Duration::seconds(60),
            )
            .await
            .unwrap();
        assert!(claimed);

        let reconciled = store.reconcile_incomplete_runs().await.unwrap();
        assert!(reconciled.is_empty());

        let stored = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::Running);

        let events = store.list_run_events(&run.id, 10).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn reconcile_incomplete_runs_marks_expired_owner_lease_run_interrupted() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("reconcile-expired-lease");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Pending;
        store.create_run(&run).await.unwrap();
        let claimed = store
            .claim_run_ownership(
                &run.id,
                "gw_expired",
                "claim_expired",
                Utc::now() - chrono::Duration::seconds(60),
                Utc::now() - chrono::Duration::seconds(15),
            )
            .await
            .unwrap();
        assert!(claimed);

        let reconciled = store.reconcile_incomplete_runs().await.unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].status, ManagedRunStatus::Interrupted);
        assert_eq!(
            reconciled[0].last_error.as_deref(),
            Some("managed run interrupted after worker lease expired during execution")
        );

        let stored = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::Interrupted);

        let events = store.list_run_events(&run.id, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, ManagedRunEventKind::RunInterrupted);
        assert_eq!(
            events[0].message.as_deref(),
            Some("managed run interrupted after worker lease expired during execution")
        );
        assert_eq!(
            events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("cause"))
                .and_then(|value| value.as_str()),
            Some("lease_expired")
        );
        assert_eq!(
            events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("owner_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_expired")
        );
        assert_eq!(events[1].kind, ManagedRunEventKind::RunOwnershipReleased);
        assert_eq!(
            events[1]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_expired")
        );
        assert_eq!(
            events[1]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(|value| value.as_str()),
            Some("interrupted")
        );
    }

    #[tokio::test]
    async fn list_interrupted_runs_pending_replay_excludes_empty_prompt_and_replayed_runs() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("interrupted-replay-candidates");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut eligible = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        eligible.status = ManagedRunStatus::Interrupted;
        eligible.prompt = "replay me".to_string();
        store.create_run(&eligible).await.unwrap();

        let mut empty_prompt = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        empty_prompt.status = ManagedRunStatus::Interrupted;
        empty_prompt.prompt = "   ".to_string();
        store.create_run(&empty_prompt).await.unwrap();

        let mut already_replayed = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        already_replayed.status = ManagedRunStatus::Interrupted;
        already_replayed.prompt = "source".to_string();
        store.create_run(&already_replayed).await.unwrap();

        let mut replay_child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        replay_child.status = ManagedRunStatus::Completed;
        replay_child.prompt = already_replayed.prompt.clone();
        replay_child.replay_of_run_id = Some(already_replayed.id.clone());
        store.create_run(&replay_child).await.unwrap();

        let candidates = store
            .list_interrupted_runs_pending_replay(10)
            .await
            .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, eligible.id);
    }

    #[tokio::test]
    async fn get_latest_replay_child_returns_latest_child_and_count() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("replay-child-query");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut source = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        source.status = ManagedRunStatus::Interrupted;
        store.create_run(&source).await.unwrap();

        let mut older_child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        older_child.status = ManagedRunStatus::Completed;
        older_child.replay_of_run_id = Some(source.id.clone());
        store.create_run(&older_child).await.unwrap();

        let mut latest_child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        latest_child.status = ManagedRunStatus::Running;
        latest_child.replay_of_run_id = Some(source.id.clone());
        store.create_run(&latest_child).await.unwrap();

        let (child, replay_count) = store
            .get_latest_replay_child(&source.id)
            .await
            .unwrap()
            .expect("replay child missing");
        assert_eq!(child.id, latest_child.id);
        assert_eq!(child.status, ManagedRunStatus::Running);
        assert_eq!(replay_count, 2);
    }

    #[tokio::test]
    async fn get_latest_replay_descendant_returns_leaf_and_depth() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("replay-descendant-query");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut source = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        source.status = ManagedRunStatus::Interrupted;
        store.create_run(&source).await.unwrap();

        let mut child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        child.status = ManagedRunStatus::Interrupted;
        child.replay_of_run_id = Some(source.id.clone());
        store.create_run(&child).await.unwrap();

        let mut grandchild = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        grandchild.status = ManagedRunStatus::Running;
        grandchild.replay_of_run_id = Some(child.id.clone());
        store.create_run(&grandchild).await.unwrap();

        let (descendant, depth) = store
            .get_latest_replay_descendant(&source.id, 8)
            .await
            .unwrap()
            .expect("replay descendant missing");
        assert_eq!(descendant.id, grandchild.id);
        assert_eq!(descendant.status, ManagedRunStatus::Running);
        assert_eq!(depth, 2);
    }

    #[tokio::test]
    async fn reconcile_incomplete_runs_preserves_timed_out_intent() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("reconcile-timeout-hint");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        store.create_run(&run).await.unwrap();
        store
            .record_run_terminal_intent(
                &run.id,
                ManagedRunStatus::TimedOut,
                Some("managed run timed out after 5s"),
            )
            .await
            .unwrap();

        let reconciled = store.reconcile_incomplete_runs().await.unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].status, ManagedRunStatus::TimedOut);
        assert!(reconciled[0].cancel_requested_at.is_none());
        assert_eq!(
            reconciled[0].last_error.as_deref(),
            Some("managed run timed out after 5s")
        );

        let stored = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::TimedOut);
        assert!(stored.cancel_requested_at.is_none());
        assert_eq!(
            stored.last_error.as_deref(),
            Some("managed run timed out after 5s")
        );

        let events = store.list_run_events(&run.id, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ManagedRunEventKind::RunTimedOut);
        assert_eq!(
            events[0].message.as_deref(),
            Some("managed run timed out after 5s")
        );
    }

    #[tokio::test]
    async fn reconcile_incomplete_runs_preserves_completed_intent() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("reconcile-completed-hint");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        store.create_run(&run).await.unwrap();
        store
            .record_run_terminal_intent(&run.id, ManagedRunStatus::Completed, None)
            .await
            .unwrap();

        let reconciled = store.reconcile_incomplete_runs().await.unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].status, ManagedRunStatus::Completed);
        assert!(reconciled[0].cancel_requested_at.is_none());
        assert!(reconciled[0].last_error.is_none());

        let stored = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::Completed);
        assert!(stored.cancel_requested_at.is_none());
        assert!(stored.last_error.is_none());

        let events = store.list_run_events(&run.id, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ManagedRunEventKind::RunCompleted);
        assert_eq!(
            events[0].message.as_deref(),
            Some("gateway restarted while managed run completion was being recorded")
        );
    }

    #[tokio::test]
    async fn durable_cleanup_recorder_round_trips_managed_run_resources() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("cleanup-recorder");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        store.create_run(&run).await.unwrap();

        DurableCleanupRecorder::register(
            &store,
            &run.id,
            41,
            DurableCleanupResource {
                kind: DurableCleanupResourceKind::ProcessGroup,
                label: "shell worker".to_string(),
                target_value: "4242".to_string(),
            },
        )
        .await
        .unwrap();

        let resources = store.list_run_cleanup_resources(&run.id).await.unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].entry_id, 41);
        assert_eq!(
            resources[0].kind,
            ManagedRunCleanupResourceKind::ProcessGroup
        );
        assert_eq!(resources[0].label, "shell worker");
        assert_eq!(resources[0].target_value, "4242");

        DurableCleanupRecorder::unregister(&store, &run.id, 41)
            .await
            .unwrap();

        let resources = store.list_run_cleanup_resources(&run.id).await.unwrap();
        assert!(resources.is_empty());
    }

    #[tokio::test]
    async fn durable_cleanup_recorder_round_trips_browser_session_resources() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("cleanup-recorder-browser");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        store.create_run(&run).await.unwrap();

        DurableCleanupRecorder::register(
            &store,
            &run.id,
            42,
            DurableCleanupResource {
                kind: DurableCleanupResourceKind::BrowserSession,
                label: "browser session state".to_string(),
                target_value: r#"{"root_pid":null,"user_data_dir":"/tmp/browser-profile"}"#
                    .to_string(),
            },
        )
        .await
        .unwrap();

        let resources = store.list_run_cleanup_resources(&run.id).await.unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].entry_id, 42);
        assert_eq!(
            resources[0].kind,
            ManagedRunCleanupResourceKind::BrowserSession
        );
        assert_eq!(resources[0].label, "browser session state");
    }

    #[tokio::test]
    async fn durable_cleanup_recorder_ignores_non_managed_sessions() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        DurableCleanupRecorder::register(
            &store,
            "session_plain_text",
            7,
            DurableCleanupResource {
                kind: DurableCleanupResourceKind::Pid,
                label: "orphan".to_string(),
                target_value: "999".to_string(),
            },
        )
        .await
        .unwrap();

        let resources = store
            .list_run_cleanup_resources("session_plain_text")
            .await
            .unwrap();
        assert!(resources.is_empty());
    }

    #[tokio::test]
    async fn list_terminal_run_cleanup_resources_filters_live_runs() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("cleanup-terminal-filter");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut interrupted = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.ended_at = Some(Utc::now());
        store.create_run(&interrupted).await.unwrap();
        store
            .upsert_run_cleanup_resource(
                &interrupted.id,
                1,
                ManagedRunCleanupResourceKind::ProcessGroup,
                "stale shell",
                "1001",
            )
            .await
            .unwrap();

        let mut running = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        running.status = ManagedRunStatus::Running;
        store.create_run(&running).await.unwrap();
        store
            .upsert_run_cleanup_resource(
                &running.id,
                2,
                ManagedRunCleanupResourceKind::ProcessGroup,
                "live shell",
                "1002",
            )
            .await
            .unwrap();

        let resources = store.list_terminal_run_cleanup_resources(10).await.unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].run_id, interrupted.id);
        assert_eq!(resources[0].entry_id, 1);
    }
}
