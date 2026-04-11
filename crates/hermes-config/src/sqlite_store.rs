//! SQLite-backed session store using tokio-rusqlite.

use std::path::Path;

use async_trait::async_trait;
use hermes_core::{
    error::{HermesError, Result},
    message::{Content, Message, Role, ToolCall},
    provider::TokenUsage,
    session::{SessionMeta, SessionStore},
};
use tokio_rusqlite::Connection;

use crate::config::hermes_home;

// ─── Schema ───────────────────────────────────────────────────────────────────

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL DEFAULT 'cli',
    model TEXT,
    system_prompt TEXT,
    cwd TEXT,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at TEXT,
    message_count INTEGER DEFAULT 0,
    tool_call_count INTEGER DEFAULT 0,
    input_tokens INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    title TEXT
);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    role TEXT NOT NULL,
    content TEXT,
    tool_calls TEXT,
    tool_call_id TEXT,
    tool_name TEXT,
    reasoning TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);
";

// ─── Store struct ─────────────────────────────────────────────────────────────

pub struct SqliteSessionStore {
    conn: Connection,
}

impl SqliteSessionStore {
    /// Open the default database at `hermes_home()/state.db`, creating the
    /// directory and schema if needed.
    pub async fn open() -> anyhow::Result<Self> {
        let db_path = hermes_home().join("state.db");
        Self::open_at(&db_path).await
    }

    /// Open a database at `path` — useful in tests with a temp directory.
    pub async fn open_at(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path).await?;

        conn.call(|c| -> rusqlite::Result<()> {
            c.execute_batch(SCHEMA)?;
            Ok(())
        })
        .await?;

        Ok(Self { conn })
    }
}

// ─── Helper: map tokio-rusqlite error ────────────────────────────────────────

fn db_err(e: impl std::fmt::Display) -> HermesError {
    HermesError::Config(e.to_string())
}

// ─── Role conversions ─────────────────────────────────────────────────────────

fn role_to_str(role: &Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
    }
}

fn role_from_str(s: &str) -> Role {
    match s {
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        "system" => Role::System,
        _ => Role::User,
    }
}

// ─── SessionStore impl ────────────────────────────────────────────────────────

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create_session(&self, meta: &SessionMeta) -> Result<()> {
        let id = meta.id.clone();
        let source = meta.source.clone();
        let model = meta.model.clone();
        let system_prompt = meta.system_prompt.clone();
        let cwd = meta.cwd.clone();
        let started_at = meta.started_at.clone();
        let title = meta.title.clone();

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "INSERT INTO sessions \
                        (id, source, model, system_prompt, cwd, started_at, title) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![id, source, model, system_prompt, cwd, started_at, title],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }

    async fn end_session(&self, session_id: &str) -> Result<()> {
        let id = session_id.to_owned();
        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE sessions SET ended_at = datetime('now') WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }

    async fn append_message(&self, session_id: &str, msg: &Message) -> Result<i64> {
        let sid = session_id.to_owned();
        let role_str = role_to_str(&msg.role).to_owned();
        let content_text = msg.content.as_text_lossy();
        let is_tool = matches!(msg.role, Role::Tool);

        // Serialize tool_calls → Option<String>
        let tool_calls_json: Option<String> =
            if msg.tool_calls.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&msg.tool_calls).map_err(|e| {
                    HermesError::Config(format!("failed to serialize tool_calls: {e}"))
                })?)
            };

        let tool_call_id = msg.tool_call_id.clone();
        let tool_name = msg.name.clone();
        let reasoning = msg.reasoning.clone();

        self.conn
            .call(move |c| -> rusqlite::Result<i64> {
                c.execute(
                    "INSERT INTO messages \
                        (session_id, role, content, tool_calls, tool_call_id, tool_name, reasoning) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        sid,
                        role_str,
                        content_text,
                        tool_calls_json,
                        tool_call_id,
                        tool_name,
                        reasoning
                    ],
                )?;
                let row_id = c.last_insert_rowid();

                // Update session message_count (and tool_call_count for tool messages)
                if is_tool {
                    c.execute(
                        "UPDATE sessions \
                         SET message_count = message_count + 1, \
                             tool_call_count = tool_call_count + 1 \
                         WHERE id = (SELECT session_id FROM messages WHERE id = ?1)",
                        rusqlite::params![row_id],
                    )?;
                } else {
                    c.execute(
                        "UPDATE sessions \
                         SET message_count = message_count + 1 \
                         WHERE id = (SELECT session_id FROM messages WHERE id = ?1)",
                        rusqlite::params![row_id],
                    )?;
                }

                Ok(row_id)
            })
            .await
            .map_err(db_err)
    }

    async fn load_history(&self, session_id: &str) -> Result<Vec<Message>> {
        let sid = session_id.to_owned();
        self.conn
            .call(move |c| -> rusqlite::Result<Vec<Message>> {
                let mut stmt = c.prepare(
                    "SELECT role, content, tool_calls, tool_call_id, tool_name, reasoning \
                     FROM messages \
                     WHERE session_id = ?1 \
                     ORDER BY id ASC",
                )?;

                let rows = stmt.query_map(rusqlite::params![sid], |row| {
                    let role_str: String = row.get(0)?;
                    let content_str: Option<String> = row.get(1)?;
                    let tool_calls_str: Option<String> = row.get(2)?;
                    let tool_call_id: Option<String> = row.get(3)?;
                    let tool_name: Option<String> = row.get(4)?;
                    let reasoning: Option<String> = row.get(5)?;
                    Ok((
                        role_str,
                        content_str,
                        tool_calls_str,
                        tool_call_id,
                        tool_name,
                        reasoning,
                    ))
                })?;

                let mut messages = Vec::new();
                for row in rows {
                    let (role_str, content_str, tool_calls_str, tool_call_id, tool_name, reasoning) =
                        row?;

                    let role = role_from_str(&role_str);
                    let content = Content::Text(content_str.unwrap_or_default());

                    let tool_calls: Vec<ToolCall> = match tool_calls_str {
                        Some(json) => serde_json::from_str(&json).unwrap_or_default(),
                        None => vec![],
                    };

                    messages.push(Message {
                        role,
                        content,
                        tool_calls,
                        reasoning,
                        name: tool_name,
                        tool_call_id,
                    });
                }

                Ok(messages)
            })
            .await
            .map_err(db_err)
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        let sid = session_id.to_owned();
        self.conn
            .call(move |c| -> rusqlite::Result<Option<SessionMeta>> {
                let result = c.query_row(
                    "SELECT id, source, model, system_prompt, cwd, started_at, ended_at, \
                            message_count, tool_call_count, input_tokens, output_tokens, title \
                     FROM sessions \
                     WHERE id = ?1",
                    rusqlite::params![sid],
                    |row| {
                        Ok(SessionMeta {
                            id: row.get(0)?,
                            source: row.get(1)?,
                            model: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                            system_prompt: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                            cwd: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                            started_at: row.get(5)?,
                            ended_at: row.get(6)?,
                            message_count: row.get::<_, u32>(7)?,
                            tool_call_count: row.get::<_, u32>(8)?,
                            input_tokens: row.get::<_, u64>(9)?,
                            output_tokens: row.get::<_, u64>(10)?,
                            title: row.get(11)?,
                        })
                    },
                );

                match result {
                    Ok(meta) => Ok(Some(meta)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e),
                }
            })
            .await
            .map_err(db_err)
    }

    async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>> {
        self.conn
            .call(move |c| -> rusqlite::Result<Vec<SessionMeta>> {
                let mut stmt = c.prepare(
                    "SELECT id, source, model, system_prompt, cwd, started_at, ended_at, \
                            message_count, tool_call_count, input_tokens, output_tokens, title \
                     FROM sessions \
                     ORDER BY started_at DESC \
                     LIMIT ?1",
                )?;

                let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
                    Ok(SessionMeta {
                        id: row.get(0)?,
                        source: row.get(1)?,
                        model: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        system_prompt: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        cwd: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        started_at: row.get(5)?,
                        ended_at: row.get(6)?,
                        message_count: row.get::<_, u32>(7)?,
                        tool_call_count: row.get::<_, u32>(8)?,
                        input_tokens: row.get::<_, u64>(9)?,
                        output_tokens: row.get::<_, u64>(10)?,
                        title: row.get(11)?,
                    })
                })?;

                let mut sessions = Vec::new();
                for row in rows {
                    sessions.push(row?);
                }
                Ok(sessions)
            })
            .await
            .map_err(db_err)
    }

    async fn update_usage(&self, session_id: &str, usage: &TokenUsage) -> Result<()> {
        let sid = session_id.to_owned();
        let input = usage.input_tokens as i64;
        let output = usage.output_tokens as i64;

        self.conn
            .call(move |c| -> rusqlite::Result<()> {
                c.execute(
                    "UPDATE sessions \
                     SET input_tokens = input_tokens + ?1, \
                         output_tokens = output_tokens + ?2 \
                     WHERE id = ?3",
                    rusqlite::params![input, output, sid],
                )?;
                Ok(())
            })
            .await
            .map_err(db_err)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn test_create_and_get_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();

        let meta = make_meta("sess-1", "2024-01-01T00:00:00");
        store.create_session(&meta).await.unwrap();

        let got = store.get_session("sess-1").await.unwrap().unwrap();
        assert_eq!(got.id, "sess-1");
        assert_eq!(got.source, "cli");
        assert_eq!(got.model, "test-model");
        assert_eq!(got.system_prompt, "You are a test assistant.");
        assert_eq!(got.cwd, "/tmp");
        assert_eq!(got.title, Some("Test session".to_owned()));
        assert!(got.ended_at.is_none());
    }

    #[tokio::test]
    async fn test_append_and_load_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();

        store
            .create_session(&make_meta("sess-2", "2024-01-02T00:00:00"))
            .await
            .unwrap();

        let user_msg = Message::user("Hello");
        let asst_msg = Message::assistant("Hi there");
        let tool_msg = Message {
            role: Role::Tool,
            content: Content::Text("tool result".to_owned()),
            tool_calls: vec![],
            reasoning: None,
            name: Some("my_tool".to_owned()),
            tool_call_id: Some("tc-1".to_owned()),
        };

        store.append_message("sess-2", &user_msg).await.unwrap();
        store.append_message("sess-2", &asst_msg).await.unwrap();
        store.append_message("sess-2", &tool_msg).await.unwrap();

        let history = store.load_history("sess-2").await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content.as_text_lossy(), "Hello");
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[1].content.as_text_lossy(), "Hi there");
        assert_eq!(history[2].role, Role::Tool);
        assert_eq!(history[2].content.as_text_lossy(), "tool result");
        assert_eq!(history[2].tool_call_id, Some("tc-1".to_owned()));
        assert_eq!(history[2].name, Some("my_tool".to_owned()));
    }

    #[tokio::test]
    async fn test_message_with_tool_calls_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();

        store
            .create_session(&make_meta("sess-3", "2024-01-03T00:00:00"))
            .await
            .unwrap();

        let tool_call = ToolCall {
            id: "tc-42".to_owned(),
            name: "search".to_owned(),
            arguments: serde_json::json!({"query": "rust async"}),
        };

        let msg = Message {
            role: Role::Assistant,
            content: Content::Text("Using search tool".to_owned()),
            tool_calls: vec![tool_call],
            reasoning: Some("I should search".to_owned()),
            name: None,
            tool_call_id: None,
        };

        store.append_message("sess-3", &msg).await.unwrap();

        let history = store.load_history("sess-3").await.unwrap();
        assert_eq!(history.len(), 1);
        let loaded = &history[0];
        assert_eq!(loaded.tool_calls.len(), 1);
        assert_eq!(loaded.tool_calls[0].id, "tc-42");
        assert_eq!(loaded.tool_calls[0].name, "search");
        assert_eq!(
            loaded.tool_calls[0].arguments,
            serde_json::json!({"query": "rust async"})
        );
        assert_eq!(loaded.reasoning, Some("I should search".to_owned()));
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();

        store
            .create_session(&make_meta("s-a", "2024-01-01T00:00:00"))
            .await
            .unwrap();
        store
            .create_session(&make_meta("s-b", "2024-01-02T00:00:00"))
            .await
            .unwrap();
        store
            .create_session(&make_meta("s-c", "2024-01-03T00:00:00"))
            .await
            .unwrap();

        let list = store.list_sessions(2).await.unwrap();
        assert_eq!(list.len(), 2);
        // Most recent first
        assert_eq!(list[0].id, "s-c");
        assert_eq!(list[1].id, "s-b");
    }

    #[tokio::test]
    async fn test_end_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();

        store
            .create_session(&make_meta("sess-end", "2024-01-04T00:00:00"))
            .await
            .unwrap();

        let before = store.get_session("sess-end").await.unwrap().unwrap();
        assert!(before.ended_at.is_none());

        store.end_session("sess-end").await.unwrap();

        let after = store.get_session("sess-end").await.unwrap().unwrap();
        assert!(after.ended_at.is_some());
    }
}
