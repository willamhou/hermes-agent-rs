use std::time::Duration;

use hermes_config::config::AppConfig;
use hermes_core::error::{HermesError, Result};
use reqwest::{Client, StatusCode};
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedManagedVersionDefaults {
    pub model: String,
    pub base_url: Option<String>,
    pub inherited_model: bool,
    pub inherited_base_url: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedModelPreflight {
    Verified,
    Skipped(String),
}

pub fn resolve_managed_version_defaults(
    model: Option<&str>,
    base_url: Option<&str>,
    app_config: &AppConfig,
) -> Result<ResolvedManagedVersionDefaults> {
    let explicit_model = normalize_optional_string(model);
    let inherited_model = explicit_model.is_none();
    let model = explicit_model
        .or_else(|| normalize_optional_string(Some(app_config.model.as_str())))
        .ok_or_else(|| {
            HermesError::Config("managed agent version model is required".to_string())
        })?;

    let explicit_base_url = normalize_optional_string(base_url);
    let inherited_base_url = explicit_base_url.is_none()
        && app_config
            .base_url
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    let base_url =
        explicit_base_url.or_else(|| normalize_optional_string(app_config.base_url.as_deref()));

    Ok(ResolvedManagedVersionDefaults {
        model,
        base_url,
        inherited_model,
        inherited_base_url,
    })
}

pub async fn preflight_managed_model(
    app_config: &AppConfig,
    model: &str,
    base_url: Option<&str>,
) -> Result<ManagedModelPreflight> {
    let resolved = resolve_managed_version_defaults(Some(model), base_url, app_config)?;
    let (provider, model_id) = split_model_provider(&resolved.model);
    if provider == "anthropic" {
        return Ok(ManagedModelPreflight::Skipped(
            "provider preflight is not implemented for anthropic yet".to_string(),
        ));
    }

    let api_key = app_config
        .api_key_for_model(&resolved.model)
        .ok_or_else(|| {
            HermesError::Config(format!(
                "no API key configured for managed model: {}",
                resolved.model
            ))
        })?;

    let models_url = format!(
        "{}/models",
        effective_base_url(&provider, resolved.base_url.as_deref()).trim_end_matches('/')
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| HermesError::Config(format!("failed to build preflight HTTP client: {e}")))?;

    let response = client
        .get(&models_url)
        .header(
            "Authorization",
            format!("Bearer {}", api_key.expose_secret()),
        )
        .send()
        .await
        .map_err(|e| {
            HermesError::Config(format!(
                "managed model preflight failed to reach {}: {e}",
                models_url
            ))
        })?;

    let status = response.status();
    let body = response.text().await.map_err(|e| {
        HermesError::Config(format!(
            "managed model preflight failed to read /models response: {e}"
        ))
    })?;

    if !status.is_success() {
        return Err(HermesError::Config(format!(
            "managed model preflight failed for {} ({}): {}",
            resolved.model,
            status.as_u16(),
            extract_error_message(&body)
        )));
    }

    let available_models = parse_model_ids(&body).ok_or_else(|| {
        HermesError::Config(format!(
            "managed model preflight returned an unexpected /models payload from {}",
            models_url
        ))
    })?;

    if available_models
        .iter()
        .any(|candidate| candidate == &model_id)
    {
        return Ok(ManagedModelPreflight::Verified);
    }

    let sample = available_models
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if sample.is_empty() {
        String::new()
    } else {
        format!(" Available models include: {sample}")
    };

    Err(HermesError::Config(format!(
        "managed model preflight could not find model {} at {}.{}",
        model_id, models_url, suffix
    )))
}

fn normalize_optional_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn split_model_provider(model: &str) -> (String, String) {
    match model.split_once('/') {
        Some((provider, model_id)) => (provider.to_ascii_lowercase(), model_id.to_string()),
        None => ("openai".to_string(), model.to_string()),
    }
}

fn effective_base_url(provider: &str, base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(match provider {
            "openrouter" => "https://openrouter.ai/api/v1",
            "anthropic" => "https://api.anthropic.com/v1",
            _ => "https://api.openai.com/v1",
        })
        .to_string()
}

#[derive(Deserialize)]
struct ModelsEnvelope {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

fn parse_model_ids(body: &str) -> Option<Vec<String>> {
    serde_json::from_str::<ModelsEnvelope>(body)
        .ok()
        .map(|parsed| parsed.data.into_iter().map(|entry| entry.id).collect())
}

fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message").and_then(|message| message.as_str()))
                .or_else(|| value.get("message").and_then(|message| message.as_str()))
                .map(ToOwned::to_owned)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                format!("HTTP {}", StatusCode::INTERNAL_SERVER_ERROR.as_u16())
            } else {
                trimmed.to_string()
            }
        })
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{Arc, LazyLock, Mutex},
    };

    use axum::{Json, Router, http::HeaderMap, routing::get};
    use serde_json::json;
    use tokio::sync::oneshot;

    use super::*;

    static ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var(name).ok();
            unsafe {
                std::env::set_var(name, value);
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.name, value) },
                None => unsafe { std::env::remove_var(self.name) },
            }
        }
    }

    fn listener_bind_is_unavailable(err: &io::Error) -> bool {
        matches!(
            err.kind(),
            io::ErrorKind::PermissionDenied
                | io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::Unsupported
        )
    }

    async fn bind_test_listener() -> Option<tokio::net::TcpListener> {
        match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => Some(listener),
            Err(err) if listener_bind_is_unavailable(&err) => {
                eprintln!("skipping network-bound managed publish test: {err}");
                None
            }
            Err(err) => panic!("failed to bind test listener: {err}"),
        }
    }

    async fn spawn_models_server(
        model_ids: Vec<&'static str>,
        status: StatusCode,
    ) -> Option<(
        String,
        Arc<Mutex<Vec<Option<String>>>>,
        tokio::task::JoinHandle<()>,
    )> {
        let headers_seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let app = Router::new().route(
            "/v1/models",
            get({
                let headers_seen = Arc::clone(&headers_seen);
                move |headers: HeaderMap| {
                    let headers_seen = Arc::clone(&headers_seen);
                    let payload = json!({
                        "object": "list",
                        "data": model_ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
                    });
                    async move {
                        headers_seen.lock().unwrap().push(
                            headers
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .map(ToOwned::to_owned),
                        );
                        (status, Json(payload))
                    }
                }
            }),
        );

        let listener = bind_test_listener().await?;
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let server = axum::serve(listener, app);
            tokio::select! {
                _ = server => {}
                _ = shutdown_rx => {}
            }
        });

        std::mem::forget(shutdown_tx);
        Some((format!("http://{addr}/v1"), headers_seen, handle))
    }

    #[test]
    fn resolve_managed_version_defaults_inherits_model_and_base_url() {
        let app_config = AppConfig {
            model: "openai/gpt-4o-mini".to_string(),
            base_url: Some("https://models.example/v1".to_string()),
            ..AppConfig::default()
        };

        let resolved = resolve_managed_version_defaults(None, None, &app_config).unwrap();
        assert_eq!(resolved.model, "openai/gpt-4o-mini");
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://models.example/v1")
        );
        assert!(resolved.inherited_model);
        assert!(resolved.inherited_base_url);
    }

    #[tokio::test]
    async fn preflight_managed_model_verifies_openai_compatible_model() {
        let _guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");

        let Some((base_url, headers_seen, handle)) =
            spawn_models_server(vec!["gpt-4o-mini", "gpt-4.1"], StatusCode::OK).await
        else {
            return;
        };
        let app_config = AppConfig {
            model: "openai/gpt-4o-mini".to_string(),
            base_url: Some(base_url),
            ..AppConfig::default()
        };

        let outcome = preflight_managed_model(&app_config, "openai/gpt-4o-mini", None)
            .await
            .unwrap();
        assert_eq!(outcome, ManagedModelPreflight::Verified);
        assert_eq!(
            headers_seen.lock().unwrap().as_slice(),
            &[Some("Bearer test-openai-key".to_string())]
        );

        handle.abort();
    }

    #[tokio::test]
    async fn preflight_managed_model_rejects_unknown_model() {
        let _guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");

        let Some((base_url, _headers_seen, handle)) =
            spawn_models_server(vec!["gpt-4.1"], StatusCode::OK).await
        else {
            return;
        };
        let app_config = AppConfig {
            model: "openai/gpt-4o-mini".to_string(),
            base_url: Some(base_url),
            ..AppConfig::default()
        };

        let err = preflight_managed_model(&app_config, "openai/gpt-4o-mini", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not find model gpt-4o-mini"));

        handle.abort();
    }

    #[tokio::test]
    async fn preflight_managed_model_skips_anthropic() {
        let app_config = AppConfig::default();
        let outcome =
            preflight_managed_model(&app_config, "anthropic/claude-sonnet-4-20250514", None)
                .await
                .unwrap();
        assert_eq!(
            outcome,
            ManagedModelPreflight::Skipped(
                "provider preflight is not implemented for anthropic yet".to_string()
            )
        );
    }
}
