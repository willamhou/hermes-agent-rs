//! Per-session agent task management and message routing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::sync::{RwLock, mpsc, oneshot};

use hermes_config::config::AppConfig;
use hermes_core::platform::{ChatType, MessageEvent, PlatformAdapter};
use hermes_core::tool::ApprovalDecision;

// ─── Public types ─────────────────────────────────────────────────────────────

/// Message routed to a session task, with a channel for sync response.
pub struct RoutedMessage {
    pub event: MessageEvent,
    pub response_tx: oneshot::Sender<String>,
}

/// Shared state across all gateway sessions (created once at startup).
pub struct SharedState {
    pub provider: Arc<dyn hermes_core::provider::Provider>,
    pub registry: Arc<hermes_tools::ToolRegistry>,
    pub tool_config: Arc<hermes_core::tool::ToolConfig>,
    pub skills: Option<Arc<RwLock<hermes_skills::SkillManager>>>,
    pub adapters: HashMap<String, Arc<dyn PlatformAdapter>>,
}

// ─── Internal types ───────────────────────────────────────────────────────────

/// Handle to a running session task.
struct SessionHandle {
    msg_tx: mpsc::Sender<RoutedMessage>,
    last_active: Arc<AtomicU64>,
}

// ─── SessionRouter ────────────────────────────────────────────────────────────

/// Routes incoming messages to per-session agent tasks.
#[derive(Clone)]
pub struct SessionRouter {
    sessions: Arc<DashMap<String, SessionHandle>>,
    shared: Arc<SharedState>,
    idle_timeout_secs: u64,
    max_sessions: usize,
    app_config: AppConfig,
}

impl SessionRouter {
    pub fn new(
        shared: Arc<SharedState>,
        idle_timeout_secs: u64,
        max_sessions: usize,
        app_config: AppConfig,
    ) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            shared,
            idle_timeout_secs,
            max_sessions,
            app_config,
        }
    }

    /// Route a message to the appropriate session. Creates session if needed.
    /// Returns the agent's response text.
    pub async fn route(&self, event: MessageEvent) -> String {
        let key = session_key(&event);

        // Create response channel
        let (response_tx, response_rx) = oneshot::channel();

        // Get or create session using Vacant/Occupied entry API to avoid TOCTOU
        let msg_tx = match self.sessions.entry(key.clone()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                e.get().last_active.store(epoch_secs(), Ordering::Relaxed);
                e.get().msg_tx.clone()
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                if self.sessions.len() >= self.max_sessions {
                    return "Error: maximum concurrent sessions reached".into();
                }
                let agent = match build_session_agent(&key, &self.shared, &self.app_config) {
                    Ok(a) => a,
                    Err(err) => return format!("Error: failed to create session: {err}"),
                };
                let (tx, rx) = mpsc::channel::<RoutedMessage>(32);
                let last_active = Arc::new(AtomicU64::new(epoch_secs()));
                let shared = Arc::clone(&self.shared);
                let la = Arc::clone(&last_active);

                tokio::spawn(async move {
                    session_task_with_agent(agent, rx, shared, la).await;
                });

                let handle = e.insert(SessionHandle {
                    msg_tx: tx,
                    last_active,
                });
                handle.msg_tx.clone()
            }
        };

        // Send message to session
        let routed = RoutedMessage { event, response_tx };
        if msg_tx.send(routed).await.is_err() {
            // Session task died; remove stale handle so next message recreates it
            self.sessions.remove(&key);
            return "Session error: task not running, session removed".into();
        }

        // Wait for response
        response_rx
            .await
            .unwrap_or_else(|_| "Session error: response dropped".into())
    }

    /// Remove sessions that have been idle past the timeout.
    pub fn cleanup_stale(&self) {
        let now = epoch_secs();
        self.sessions.retain(|_key, handle| {
            now - handle.last_active.load(Ordering::Relaxed) < self.idle_timeout_secs
        });
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Drop all session handles, which closes the msg_tx channels and stops session tasks.
    pub fn shutdown(&self) {
        self.sessions.clear();
    }
}

// ─── Session key derivation ───────────────────────────────────────────────────

pub fn session_key(event: &MessageEvent) -> String {
    match event.chat_type {
        ChatType::DirectMessage => format!("{}:dm:{}", event.platform, event.user_id),
        ChatType::Group => {
            format!(
                "{}:group:{}:{}",
                event.platform, event.chat_id, event.user_id
            )
        }
        ChatType::Channel => format!("{}:chan:{}", event.platform, event.chat_id),
    }
}

// ─── Session task ─────────────────────────────────────────────────────────────

/// Run a session task given an already-constructed Agent.
async fn session_task_with_agent(
    mut agent: hermes_agent::loop_runner::Agent,
    mut msg_rx: mpsc::Receiver<RoutedMessage>,
    shared: Arc<SharedState>,
    last_active: Arc<AtomicU64>,
) {
    let mut history = Vec::new();

    while let Some(routed) = msg_rx.recv().await {
        last_active.store(epoch_secs(), Ordering::Relaxed);

        // Gateway discards streaming deltas
        let (delta_tx, _) = mpsc::channel(64);

        let result = agent
            .run_conversation(&routed.event.text, &mut history, delta_tx)
            .await;
        let response = match result {
            Ok(text) => text,
            Err(e) => format!("Error: {e}"),
        };

        // Send response to originating platform
        if let Some(adapter) = shared.adapters.get(&routed.event.platform) {
            if let Err(e) = adapter.send_response(&routed.event, &response).await {
                tracing::warn!(
                    platform = %routed.event.platform,
                    "send_response failed: {e}"
                );
            }
        } else {
            tracing::warn!(
                platform = %routed.event.platform,
                "no adapter found for platform"
            );
        }

        // Send sync response (for API server oneshot)
        let _ = routed.response_tx.send(response);
    }

    tracing::debug!("session task ended");
    // Note: approval_rx closes when Agent drops here, ending the approval task.
}

// ─── Agent construction ───────────────────────────────────────────────────────

fn build_session_agent(
    session_id: &str,
    shared: &SharedState,
    app_config: &AppConfig,
) -> hermes_core::error::Result<hermes_agent::loop_runner::Agent> {
    use hermes_agent::{
        compressor::CompressionConfig,
        loop_runner::{Agent, AgentConfig},
    };
    use hermes_memory::MemoryManager;

    let memory_dir = hermes_config::config::hermes_home().join("memories");
    let memory = MemoryManager::new(memory_dir, None).map_err(|e| {
        hermes_core::error::HermesError::Config(format!("failed to create memory: {e}"))
    })?;

    // Gateway: auto-allow all tool approvals (no interactive UI).
    // The approval task ends naturally when approval_rx closes (Agent drop).
    let (approval_tx, mut approval_rx) = mpsc::channel::<hermes_core::tool::ApprovalRequest>(8);
    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            let _ = req.response_tx.send(ApprovalDecision::Allow);
        }
    });

    Ok(Agent::new(AgentConfig {
        provider: Arc::clone(&shared.provider),
        registry: Arc::clone(&shared.registry),
        max_iterations: app_config.max_iterations,
        system_prompt: "You are Hermes, a helpful AI assistant.".into(),
        session_id: session_id.into(),
        working_dir: std::env::current_dir().unwrap_or_default(),
        approval_tx,
        tool_config: Arc::clone(&shared.tool_config),
        memory,
        skills: shared.skills.clone(),
        compression: CompressionConfig::default(),
        delegation_depth: 0,
        clarify_tx: None,
    }))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(
        platform: &str,
        chat_id: &str,
        user_id: &str,
        chat_type: ChatType,
    ) -> MessageEvent {
        MessageEvent {
            platform: platform.into(),
            chat_id: chat_id.into(),
            user_id: user_id.into(),
            user_name: None,
            text: "hello".into(),
            reply_to: None,
            chat_type,
            thread_id: None,
        }
    }

    #[test]
    fn test_session_key_dm() {
        let event = make_event("tg", "chat1", "user1", ChatType::DirectMessage);
        assert_eq!(session_key(&event), "tg:dm:user1");
    }

    #[test]
    fn test_session_key_group() {
        let event = make_event("tg", "chat1", "user1", ChatType::Group);
        assert_eq!(session_key(&event), "tg:group:chat1:user1");
    }

    #[test]
    fn test_session_key_channel() {
        let event = make_event("tg", "chat1", "user1", ChatType::Channel);
        assert_eq!(session_key(&event), "tg:chan:chat1");
    }

    #[test]
    fn test_epoch_secs() {
        let ts = epoch_secs();
        // Must be a plausible Unix timestamp (after 2020, before 2100)
        assert!(ts > 1_577_836_800, "timestamp too small: {ts}");
        assert!(ts < 4_102_444_800, "timestamp too large: {ts}");
    }

    #[test]
    fn test_cleanup_stale() {
        use dashmap::DashMap;
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        let sessions: Arc<DashMap<String, SessionHandle>> = Arc::new(DashMap::new());

        // Insert a handle with a very old last_active timestamp
        let (tx, _rx) = mpsc::channel::<RoutedMessage>(1);
        let old_ts = Arc::new(AtomicU64::new(0)); // epoch 0 → definitely stale
        sessions.insert(
            "stale-session".to_string(),
            SessionHandle {
                msg_tx: tx,
                last_active: old_ts,
            },
        );

        assert_eq!(sessions.len(), 1);

        let idle_timeout_secs = 1800u64;
        let now = epoch_secs();
        sessions.retain(|_key, handle| {
            now - handle.last_active.load(Ordering::Relaxed) < idle_timeout_secs
        });

        assert_eq!(sessions.len(), 0, "stale session should have been removed");
    }

    #[test]
    fn test_cleanup_keeps_active() {
        use dashmap::DashMap;
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        let sessions: Arc<DashMap<String, SessionHandle>> = Arc::new(DashMap::new());

        // Insert a handle with the current timestamp (active)
        let (tx, _rx) = mpsc::channel::<RoutedMessage>(1);
        let recent_ts = Arc::new(AtomicU64::new(epoch_secs()));
        sessions.insert(
            "active-session".to_string(),
            SessionHandle {
                msg_tx: tx,
                last_active: recent_ts,
            },
        );

        assert_eq!(sessions.len(), 1);

        let idle_timeout_secs = 1800u64;
        let now = epoch_secs();
        sessions.retain(|_key, handle| {
            now - handle.last_active.load(Ordering::Relaxed) < idle_timeout_secs
        });

        assert_eq!(sessions.len(), 1, "active session should be kept");
    }
}
