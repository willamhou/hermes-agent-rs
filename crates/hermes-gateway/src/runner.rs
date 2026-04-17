//! GatewayRunner — orchestrates adapters and routes messages through SessionRouter.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hermes_config::config::{AppConfig, GatewayConfig, hermes_home};
use hermes_core::platform::{PlatformAdapter, PlatformEvent};
use hermes_provider::create_provider;
use hermes_tools::ToolRegistry;
use secrecy::SecretString;
use tokio::sync::{RwLock, mpsc};

use crate::api_server::ApiServerAdapter;
use crate::session::{SessionRouter, SharedState};
use crate::telegram::TelegramAdapter;

pub struct GatewayRunner {
    gateway_config: GatewayConfig,
    app_config: AppConfig,
}

impl GatewayRunner {
    pub fn new(gateway_config: GatewayConfig, app_config: AppConfig) -> Self {
        Self {
            gateway_config,
            app_config,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // 1. Build shared state
        let api_key = self
            .app_config
            .api_key()
            .ok_or_else(|| anyhow::anyhow!("No API key configured"))?;
        let provider = create_provider(
            &self.app_config.model,
            SecretString::new(api_key.into()),
            None,
        )?;

        // Build tool registry — same tools as CLI, including inventory-registered ones
        let registry = Arc::new(ToolRegistry::from_inventory());

        let working_dir = std::env::current_dir()?;
        let tool_config = Arc::new(self.app_config.tool_config(working_dir));

        // Skills (optional — if skills dir exists)
        let skills = {
            let skills_dir = hermes_home().join("skills");
            if skills_dir.exists() {
                match hermes_skills::SkillManager::new(vec![skills_dir]) {
                    Ok(sm) => Some(Arc::new(RwLock::new(sm))),
                    Err(e) => {
                        tracing::warn!("failed to load skills: {e}");
                        None
                    }
                }
            } else {
                None
            }
        };

        // 2. Create adapters + event channel
        let (event_tx, mut event_rx) = mpsc::channel::<PlatformEvent>(256);
        let mut adapters: HashMap<String, Arc<dyn PlatformAdapter>> = HashMap::new();
        let mut adapter_handles = Vec::new();

        if let Some(ref tg_config) = self.gateway_config.telegram {
            let adapter = Arc::new(TelegramAdapter::new(
                tg_config.token.clone(),
                tg_config.allowed_users.clone(),
                tg_config.allow_all,
            ));
            adapters.insert(
                "telegram".into(),
                adapter.clone() as Arc<dyn PlatformAdapter>,
            );
            let tx = event_tx.clone();
            adapter_handles.push(tokio::spawn(async move { adapter.run(tx).await }));
            tracing::info!("telegram adapter enabled");
        }

        if let Some(ref api_config) = self.gateway_config.api_server {
            let adapter = Arc::new(ApiServerAdapter::new(api_config.clone()));
            adapters.insert("api".into(), adapter.clone() as Arc<dyn PlatformAdapter>);
            let tx = event_tx.clone();
            adapter_handles.push(tokio::spawn(async move { adapter.run(tx).await }));
            tracing::info!(addr = %api_config.bind_addr, "api server enabled");
        }

        drop(event_tx); // only adapters hold senders now

        // 3. Build session router
        let shared = Arc::new(SharedState {
            provider,
            registry,
            tool_config,
            skills,
            adapters,
        });
        let router = SessionRouter::new(
            shared,
            self.gateway_config.session_idle_timeout_secs,
            self.app_config.clone(),
        );

        // 4. Spawn idle cleanup task
        let cleanup_router = router.clone();
        let cleanup_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                cleanup_router.cleanup_stale();
            }
        });

        // 5. Main event loop
        tracing::info!("gateway started — waiting for messages");
        while let Some(event) = event_rx.recv().await {
            match event {
                PlatformEvent::Message(msg) => {
                    let r = router.clone();
                    tokio::spawn(async move {
                        r.route(msg).await;
                    });
                }
                PlatformEvent::Shutdown => break,
            }
        }

        // 6. Shutdown
        cleanup_handle.abort();
        for handle in adapter_handles {
            handle.abort();
        }
        router.shutdown();
        tracing::info!("gateway stopped");
        Ok(())
    }
}
