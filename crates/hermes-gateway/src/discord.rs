//! Discord bot adapter using serenity.

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use hermes_core::{
    error::Result,
    platform::{ChatType, MessageEvent, PlatformAdapter, PlatformEvent},
};
use secrecy::{ExposeSecret, SecretString};
use serenity::all::{
    ChannelId, Context, CreateMessage, EventHandler, GatewayIntents, Message, Ready,
};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::message_split::split_discord;

// ─── Shared HTTP handle ──────────────────────────────────────────────────────

type SharedHttp = Arc<RwLock<Option<Arc<serenity::all::Http>>>>;

// ─── Adapter ─────────────────────────────────────────────────────────────────

pub struct DiscordAdapter {
    token: SecretString,
    allowed_users: HashSet<String>,
    allow_all: bool,
    /// Populated by the serenity `Ready` handler; used by `send_response`.
    http: SharedHttp,
}

impl DiscordAdapter {
    pub fn new(token: String, allowed_users: Vec<String>, allow_all: bool) -> Self {
        Self {
            token: SecretString::new(token.into()),
            allowed_users: allowed_users.into_iter().collect(),
            allow_all,
            http: Arc::new(RwLock::new(None)),
        }
    }
}

// ─── Serenity event handler ──────────────────────────────────────────────────

struct Handler {
    event_tx: mpsc::Sender<PlatformEvent>,
    allowed_users: HashSet<String>,
    allow_all: bool,
    http_slot: SharedHttp,
}

impl Handler {
    fn is_authorized(&self, user_id: &str, user_name: &str) -> bool {
        self.allow_all
            || self.allowed_users.contains(user_id)
            || self.allowed_users.contains(user_name)
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("DiscordAdapter: connected as {}", ready.user.name);
        let mut slot = self.http_slot.write().await;
        *slot = Some(ctx.http.clone());
    }

    async fn message(&self, _ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let user_id = msg.author.id.to_string();
        let user_name = msg.author.name.clone();

        if !self.is_authorized(&user_id, &user_name) {
            warn!("Unauthorized Discord user id={user_id} name={user_name}");
            return;
        }

        let text = msg.content.clone();
        if text.is_empty() {
            debug!("Skipping empty Discord message from {user_id}");
            return;
        }

        let chat_type = if msg.guild_id.is_some() {
            ChatType::Channel
        } else {
            ChatType::DirectMessage
        };

        let thread_id = msg.thread.as_ref().map(|t| t.id.to_string());

        let event = MessageEvent {
            platform: "discord".to_string(),
            chat_id: msg.channel_id.to_string(),
            user_id,
            user_name: Some(user_name),
            text,
            reply_to: msg.referenced_message.as_ref().map(|r| r.id.to_string()),
            chat_type,
            thread_id,
        };

        if self
            .event_tx
            .send(PlatformEvent::Message(event))
            .await
            .is_err()
        {
            info!("DiscordAdapter: event_tx closed");
        }
    }
}

// ─── PlatformAdapter impl ────────────────────────────────────────────────────

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    fn platform_name(&self) -> &str {
        "discord"
    }

    async fn run(self: Arc<Self>, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()> {
        info!("DiscordAdapter: starting");

        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let handler = Handler {
            event_tx,
            allowed_users: self.allowed_users.clone(),
            allow_all: self.allow_all,
            http_slot: Arc::clone(&self.http),
        };

        let mut client = serenity::all::Client::builder(self.token.expose_secret(), intents)
            .event_handler(handler)
            .await
            .map_err(|e| {
                hermes_core::error::HermesError::Config(format!("Discord client build failed: {e}"))
            })?;

        if let Err(e) = client.start().await {
            error!("Discord client error: {e}");
            return Err(hermes_core::error::HermesError::Config(format!(
                "Discord client error: {e}"
            )));
        }

        Ok(())
    }

    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()> {
        let http = {
            let guard = self.http.read().await;
            guard.clone().ok_or_else(|| {
                hermes_core::error::HermesError::Config("Discord HTTP client not ready".to_string())
            })?
        };

        let channel_id: u64 = event.chat_id.parse().map_err(|e| {
            hermes_core::error::HermesError::Config(format!("invalid channel id: {e}"))
        })?;
        let channel = ChannelId::new(channel_id);

        let chunks = split_discord(response);
        for chunk in chunks {
            let msg = CreateMessage::new().content(chunk);
            channel.send_message(&http, msg).await.map_err(|e| {
                hermes_core::error::HermesError::Config(format!("Discord send_message failed: {e}"))
            })?;
        }

        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler(allow_all: bool, allowed: Vec<&str>) -> Handler {
        let (tx, _rx) = mpsc::channel(1);
        Handler {
            event_tx: tx,
            allowed_users: allowed.into_iter().map(String::from).collect(),
            allow_all,
            http_slot: Arc::new(RwLock::new(None)),
        }
    }

    #[test]
    fn authorization_allow_all() {
        let h = make_handler(true, vec![]);
        assert!(h.is_authorized("999", "anyone"));
    }

    #[test]
    fn authorization_by_id() {
        let h = make_handler(false, vec!["111"]);
        assert!(h.is_authorized("111", "alice"));
        assert!(!h.is_authorized("222", "bob"));
    }

    #[test]
    fn authorization_by_username() {
        let h = make_handler(false, vec!["alice"]);
        assert!(h.is_authorized("111", "alice"));
        assert!(!h.is_authorized("222", "bob"));
    }

    #[test]
    fn authorization_denied() {
        let h = make_handler(false, vec!["111", "alice"]);
        assert!(!h.is_authorized("333", "charlie"));
    }

    #[test]
    fn platform_name() {
        let a = DiscordAdapter::new("t".into(), vec![], true);
        assert_eq!(PlatformAdapter::platform_name(&a), "discord");
    }
}
