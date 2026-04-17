//! Telegram bot adapter with long-polling.

use crate::message_split::split_telegram;
use async_trait::async_trait;
use hermes_core::{
    error::Result,
    platform::{ChatType, MessageEvent, PlatformAdapter, PlatformEvent},
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ─── Telegram API types ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
}

#[derive(Deserialize)]
struct TgMessage {
    #[allow(dead_code)]
    message_id: i64,
    from: Option<TgUser>,
    chat: TgChat,
    text: Option<String>,
    message_thread_id: Option<i64>,
}

#[derive(Deserialize)]
struct TgUser {
    id: i64,
    first_name: String,
    username: Option<String>,
}

#[derive(Deserialize)]
struct TgChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
}

// ─── Adapter ─────────────────────────────────────────────────────────────────

pub struct TelegramAdapter {
    token: SecretString,
    client: reqwest::Client,
    allowed_users: HashSet<String>,
    allow_all: bool,
}

impl TelegramAdapter {
    pub fn new(token: String, allowed_users: Vec<String>, allow_all: bool) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client");
        Self {
            token: SecretString::new(token.into()),
            client,
            allowed_users: allowed_users.into_iter().collect(),
            allow_all,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{}",
            self.token.expose_secret(),
            method
        )
    }

    fn is_authorized(&self, user: &TgUser) -> bool {
        if self.allow_all {
            return true;
        }
        let id_str = user.id.to_string();
        if self.allowed_users.contains(&id_str) {
            return true;
        }
        if let Some(username) = &user.username {
            if self.allowed_users.contains(username) {
                return true;
            }
        }
        false
    }

    fn map_chat_type(chat_type: &str) -> ChatType {
        match chat_type {
            "private" => ChatType::DirectMessage,
            "group" | "supergroup" => ChatType::Group,
            "channel" => ChatType::Channel,
            _ => ChatType::DirectMessage,
        }
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    fn platform_name(&self) -> &str {
        "telegram"
    }

    async fn run(self: Arc<Self>, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()> {
        info!("TelegramAdapter: starting long-poll loop");

        let mut offset: Option<i64> = None;
        // Backoff levels: 5, 10, 20, 40, 60 seconds
        let backoff_steps: &[u64] = &[5, 10, 20, 40, 60];
        let mut backoff_idx: usize = 0;

        loop {
            let mut req = self
                .client
                .get(self.api_url("getUpdates"))
                .query(&[("timeout", "30")]);

            if let Some(off) = offset {
                req = req.query(&[("offset", off.to_string())]);
            }

            let response = req.send().await;

            match response {
                Ok(resp) => {
                    match resp.json::<TgResponse<Vec<TgUpdate>>>().await {
                        Ok(tg_resp) if tg_resp.ok => {
                            // Reset backoff on success
                            backoff_idx = 0;

                            let updates = tg_resp.result.unwrap_or_default();

                            for update in &updates {
                                // Advance offset past this update
                                offset = Some(update.update_id + 1);

                                let msg = match &update.message {
                                    Some(m) => m,
                                    None => {
                                        debug!("Skipping non-message update {}", update.update_id);
                                        continue;
                                    }
                                };

                                // Only handle text messages
                                let text = match &msg.text {
                                    Some(t) => t.clone(),
                                    None => {
                                        debug!(
                                            "Skipping non-text message in update {}",
                                            update.update_id
                                        );
                                        continue;
                                    }
                                };

                                // Authorization check
                                let user = match &msg.from {
                                    Some(u) => u,
                                    None => {
                                        warn!(
                                            "Message without sender in update {}",
                                            update.update_id
                                        );
                                        continue;
                                    }
                                };

                                if !self.is_authorized(user) {
                                    warn!(
                                        "Unauthorized user id={} username={:?}",
                                        user.id, user.username
                                    );
                                    continue;
                                }

                                let event = MessageEvent {
                                    platform: "telegram".to_string(),
                                    chat_id: msg.chat.id.to_string(),
                                    user_id: user.id.to_string(),
                                    user_name: Some(
                                        user.username
                                            .clone()
                                            .unwrap_or_else(|| user.first_name.clone()),
                                    ),
                                    text,
                                    reply_to: None,
                                    chat_type: Self::map_chat_type(&msg.chat.chat_type),
                                    thread_id: msg.message_thread_id.map(|id| id.to_string()),
                                };

                                if event_tx.send(PlatformEvent::Message(event)).await.is_err() {
                                    info!("TelegramAdapter: event_tx closed, shutting down");
                                    return Ok(());
                                }
                            }
                        }
                        Ok(tg_resp) => {
                            error!(
                                "getUpdates returned ok=false: {:?}",
                                tg_resp.result.is_some()
                            );
                            let delay = backoff_steps[backoff_idx];
                            warn!("Backing off {}s (level {})", delay, backoff_idx);
                            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                            backoff_idx = (backoff_idx + 1).min(backoff_steps.len() - 1);
                        }
                        Err(e) => {
                            error!("Failed to deserialize getUpdates response: {e}");
                            let delay = backoff_steps[backoff_idx];
                            warn!("Backing off {}s (level {})", delay, backoff_idx);
                            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                            backoff_idx = (backoff_idx + 1).min(backoff_steps.len() - 1);
                        }
                    }
                }
                Err(e) => {
                    error!("getUpdates HTTP error: {e}");
                    let delay = backoff_steps[backoff_idx];
                    warn!("Backing off {}s (level {})", delay, backoff_idx);
                    tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                    backoff_idx = (backoff_idx + 1).min(backoff_steps.len() - 1);
                }
            }
        }
    }

    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()> {
        let chunks = split_telegram(response);
        for chunk in chunks {
            let mut params = vec![("chat_id", event.chat_id.clone()), ("text", chunk)];
            if let Some(thread_id) = &event.thread_id {
                params.push(("message_thread_id", thread_id.clone()));
            }
            let resp = self
                .client
                .post(self.api_url("sendMessage"))
                .form(&params)
                .send()
                .await
                .map_err(|e| hermes_core::error::HermesError::Config(e.to_string()))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(hermes_core::error::HermesError::Config(format!(
                    "sendMessage failed with status {status}: {body}"
                )));
            }
        }
        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_adapter(allow_all: bool, allowed_users: Vec<&str>) -> TelegramAdapter {
        TelegramAdapter::new(
            "fake_token".to_string(),
            allowed_users.into_iter().map(String::from).collect(),
            allow_all,
        )
    }

    fn make_user(id: i64, username: Option<&str>) -> TgUser {
        TgUser {
            id,
            first_name: "Test".to_string(),
            username: username.map(String::from),
        }
    }

    #[test]
    fn parse_tg_update_from_json() {
        let json = r#"
        {
            "update_id": 123456789,
            "message": {
                "message_id": 42,
                "from": {
                    "id": 111,
                    "first_name": "Alice",
                    "username": "alice"
                },
                "chat": {
                    "id": 111,
                    "type": "private"
                },
                "text": "Hello, bot!"
            }
        }
        "#;

        let update: TgUpdate = serde_json::from_str(json).expect("Failed to parse TgUpdate");
        assert_eq!(update.update_id, 123456789);
        let msg = update.message.expect("Expected message");
        assert_eq!(msg.message_id, 42);
        assert_eq!(msg.text.as_deref(), Some("Hello, bot!"));
        let user = msg.from.expect("Expected from");
        assert_eq!(user.id, 111);
        assert_eq!(user.first_name, "Alice");
        assert_eq!(user.username.as_deref(), Some("alice"));
        assert_eq!(msg.chat.id, 111);
        assert_eq!(msg.chat.chat_type, "private");
    }

    #[test]
    fn authorization_allow_all() {
        let adapter = make_adapter(true, vec![]);
        let user = make_user(999, Some("anyone"));
        assert!(adapter.is_authorized(&user));
    }

    #[test]
    fn authorization_allowlist_by_id() {
        let adapter = make_adapter(false, vec!["111"]);
        let allowed = make_user(111, None);
        let denied = make_user(222, None);
        assert!(adapter.is_authorized(&allowed));
        assert!(!adapter.is_authorized(&denied));
    }

    #[test]
    fn authorization_allowlist_by_username() {
        let adapter = make_adapter(false, vec!["alice"]);
        let allowed = make_user(111, Some("alice"));
        let denied = make_user(222, Some("bob"));
        assert!(adapter.is_authorized(&allowed));
        assert!(!adapter.is_authorized(&denied));
    }

    #[test]
    fn chat_type_mapping() {
        assert_eq!(
            TelegramAdapter::map_chat_type("private"),
            ChatType::DirectMessage
        );
        assert_eq!(TelegramAdapter::map_chat_type("group"), ChatType::Group);
        assert_eq!(
            TelegramAdapter::map_chat_type("supergroup"),
            ChatType::Group
        );
        assert_eq!(TelegramAdapter::map_chat_type("channel"), ChatType::Channel);
        // Unknown falls back to DM
        assert_eq!(
            TelegramAdapter::map_chat_type("unknown"),
            ChatType::DirectMessage
        );
    }
}
