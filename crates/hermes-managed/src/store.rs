use std::path::Path;

use chrono::{DateTime, Utc};
use hermes_config::hermes_home;
use hermes_core::error::{HermesError, Result};
use rusqlite::OptionalExtension;
use tokio_rusqlite::Connection;

use crate::types::{
    ManagedAgent, ManagedAgentVersion, ManagedAgentVersionDraft, ManagedApprovalPolicy, ManagedRun,
    ManagedRunEvent, ManagedRunEventDraft, ManagedRunEventKind, ManagedRunStatus,
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
    prompt TEXT NOT NULL DEFAULT '',
    replay_of_run_id TEXT,
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    ended_at TEXT,
    cancel_requested_at TEXT,
    terminal_status_hint TEXT,
    terminal_reason_hint TEXT,
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
            if !has_column(c, "runs", "replay_of_run_id")? {
                c.execute("ALTER TABLE runs ADD COLUMN replay_of_run_id TEXT", [])?;
            }
            if !has_column(c, "runs", "terminal_status_hint")? {
                c.execute("ALTER TABLE runs ADD COLUMN terminal_status_hint TEXT", [])?;
            }
            if !has_column(c, "runs", "terminal_reason_hint")? {
                c.execute("ALTER TABLE runs ADD COLUMN terminal_reason_hint TEXT", [])?;
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
                        (id, agent_id, agent_version, status, model, prompt, replay_of_run_id,
                         started_at, updated_at, ended_at, cancel_requested_at, terminal_status_hint,
                         terminal_reason_hint, last_error)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, NULL, ?12)",
                    rusqlite::params![
                        id,
                        agent_id,
                        agent_version,
                        status,
                        model,
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

    pub async fn get_run(&self, run_id: &str) -> Result<Option<ManagedRun>> {
        let run_id = run_id.to_owned();
        let raw = self
            .conn
            .call(move |c| -> rusqlite::Result<Option<RawRun>> {
                c.query_row(
                    "SELECT id, agent_id, agent_version, status, model, prompt, replay_of_run_id,
                            started_at, updated_at, ended_at, cancel_requested_at, terminal_status_hint,
                            terminal_reason_hint, last_error
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
                            prompt: row.get(5)?,
                            replay_of_run_id: row.get(6)?,
                            started_at: row.get(7)?,
                            updated_at: row.get(8)?,
                            ended_at: row.get(9)?,
                            cancel_requested_at: row.get(10)?,
                            terminal_status_hint: row.get(11)?,
                            terminal_reason_hint: row.get(12)?,
                            last_error: row.get(13)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .map_err(db_err)?;

        raw.map(map_run).transpose()
    }

    pub async fn list_runs(&self, limit: usize) -> Result<Vec<ManagedRun>> {
        let raws = self
            .conn
            .call(move |c| -> rusqlite::Result<Vec<RawRun>> {
                let mut stmt = c.prepare(
                    "SELECT id, agent_id, agent_version, status, model, prompt, replay_of_run_id,
                            started_at, updated_at, ended_at, cancel_requested_at, terminal_status_hint,
                            terminal_reason_hint, last_error
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
                        prompt: row.get(5)?,
                        replay_of_run_id: row.get(6)?,
                        started_at: row.get(7)?,
                        updated_at: row.get(8)?,
                        ended_at: row.get(9)?,
                        cancel_requested_at: row.get(10)?,
                        terminal_status_hint: row.get(11)?,
                        terminal_reason_hint: row.get(12)?,
                        last_error: row.get(13)?,
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
                    "SELECT id, agent_id, agent_version, status, model, prompt, replay_of_run_id,
                            started_at, updated_at, ended_at, cancel_requested_at, terminal_status_hint,
                            terminal_reason_hint, last_error
                     FROM runs
                     WHERE status IN (?1, ?2)
                     ORDER BY started_at ASC",
                )?;
                let mut raws = stmt
                    .query_map(rusqlite::params![pending, running], |row| {
                        Ok(RawRun {
                            id: row.get(0)?,
                            agent_id: row.get(1)?,
                            agent_version: row.get(2)?,
                            status: row.get(3)?,
                            model: row.get(4)?,
                            prompt: row.get(5)?,
                            replay_of_run_id: row.get(6)?,
                            started_at: row.get(7)?,
                            updated_at: row.get(8)?,
                            ended_at: row.get(9)?,
                            cancel_requested_at: row.get(10)?,
                            terminal_status_hint: row.get(11)?,
                            terminal_reason_hint: row.get(12)?,
                            last_error: row.get(13)?,
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

                    tx.execute(
                        "UPDATE runs
                         SET status = ?1,
                             updated_at = ?2,
                             ended_at = COALESCE(ended_at, ?3),
                             terminal_status_hint = NULL,
                             terminal_reason_hint = NULL,
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
                    tx.execute(
                        "INSERT INTO run_events
                            (run_id, kind, message, tool_name, tool_call_id, metadata, created_at)
                         VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4)",
                        rusqlite::params![
                            raw.id,
                            kind_str,
                            reconciled.event_message,
                            reconciled_at
                        ],
                    )?;

                    raw.status = status_str;
                    raw.updated_at = reconciled_at.clone();
                    raw.ended_at = Some(reconciled_at.clone());
                    raw.last_error = reconciled.last_error;
                }
                tx.commit()?;

                Ok(raws)
            })
            .await
            .map_err(db_err)?;

        raws.into_iter().map(map_run).collect()
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
    prompt: String,
    replay_of_run_id: Option<String>,
    started_at: String,
    updated_at: String,
    ended_at: Option<String>,
    cancel_requested_at: Option<String>,
    terminal_status_hint: Option<String>,
    terminal_reason_hint: Option<String>,
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

struct ReconciledIncompleteRun {
    status: ManagedRunStatus,
    event_kind: ManagedRunEventKind,
    event_message: Option<String>,
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
            last_error,
        };
    }

    if raw.cancel_requested_at.is_some() {
        let message = "gateway restarted after cancel was requested".to_string();
        ReconciledIncompleteRun {
            status: ManagedRunStatus::Cancelled,
            event_kind: ManagedRunEventKind::RunCancelled,
            event_message: Some(message.clone()),
            last_error: Some(message),
        }
    } else {
        let message = "gateway restarted before managed run completed".to_string();
        ReconciledIncompleteRun {
            status: ManagedRunStatus::Failed,
            event_kind: ManagedRunEventKind::RunFailed,
            event_message: Some(message.clone()),
            last_error: Some(message),
        }
    }
}

fn terminal_event_kind(status: &ManagedRunStatus) -> ManagedRunEventKind {
    match status {
        ManagedRunStatus::Completed => ManagedRunEventKind::RunCompleted,
        ManagedRunStatus::Cancelled => ManagedRunEventKind::RunCancelled,
        ManagedRunStatus::Failed => ManagedRunEventKind::RunFailed,
        ManagedRunStatus::TimedOut => ManagedRunEventKind::RunTimedOut,
        ManagedRunStatus::Pending | ManagedRunStatus::Running => {
            unreachable!("non-terminal or unsupported status used for terminal event kind")
        }
    }
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
    async fn reconcile_incomplete_runs_marks_running_run_failed() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("reconcile-failed");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        store.create_run(&run).await.unwrap();

        let reconciled = store.reconcile_incomplete_runs().await.unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].status, ManagedRunStatus::Failed);
        assert_eq!(
            reconciled[0].last_error.as_deref(),
            Some("gateway restarted before managed run completed")
        );
        assert!(reconciled[0].ended_at.is_some());

        let stored = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::Failed);

        let events = store.list_run_events(&run.id, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ManagedRunEventKind::RunFailed);
        assert_eq!(
            events[0].message.as_deref(),
            Some("gateway restarted before managed run completed")
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
}
