//! LLM provider implementations (OpenAI, Anthropic, OpenRouter)

pub mod anthropic;
pub mod openai;
pub mod responses;
pub mod retry;
pub mod sse;
pub mod tool_assembler;

use std::sync::Arc;

use anyhow::Result;
use hermes_core::provider::{ModelInfo, ModelPricing, Provider};
use secrecy::SecretString;

use crate::{
    anthropic::{AnthropicConfig, AnthropicProvider},
    openai::{AuthStyle, OpenAiConfig, OpenAiProvider},
    responses::ResponsesProvider,
};

// ─── Model info helpers ───────────────────────────────────────────────────────

/// Default `ModelInfo` for Anthropic models.
pub fn anthropic_model_info(model: &str) -> ModelInfo {
    ModelInfo {
        id: model.to_string(),
        provider: "anthropic".to_string(),
        max_context: 200_000,
        max_output: 8192,
        supports_tools: true,
        supports_vision: true,
        supports_reasoning: true,
        supports_caching: true,
        pricing: ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: 0.3,
            cache_create_per_mtok: 3.75,
        },
    }
}

/// Default `ModelInfo` for OpenAI models.
pub fn openai_model_info(model: &str) -> ModelInfo {
    ModelInfo {
        id: model.to_string(),
        provider: "openai".to_string(),
        max_context: 128_000,
        max_output: 4096,
        supports_tools: true,
        supports_vision: true,
        supports_reasoning: false,
        supports_caching: false,
        pricing: ModelPricing {
            input_per_mtok: 5.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: 0.5,
            cache_create_per_mtok: 1.25,
        },
    }
}

/// Default `ModelInfo` for generic/unknown providers (treated as OpenAI-compatible).
pub fn generic_model_info(provider: &str, model: &str) -> ModelInfo {
    ModelInfo {
        id: model.to_string(),
        provider: provider.to_string(),
        max_context: 128_000,
        max_output: 4096,
        supports_tools: true,
        supports_vision: false,
        supports_reasoning: false,
        supports_caching: false,
        pricing: ModelPricing {
            input_per_mtok: 1.0,
            output_per_mtok: 3.0,
            cache_read_per_mtok: 0.0,
            cache_create_per_mtok: 0.0,
        },
    }
}

/// Default `ModelInfo` for OpenAI Responses API models.
pub fn responses_model_info(model: &str) -> ModelInfo {
    ModelInfo {
        id: model.to_string(),
        provider: "openai-codex".to_string(),
        max_context: 200_000,
        max_output: 16_384,
        supports_tools: true,
        supports_vision: true,
        supports_reasoning: true,
        supports_caching: false,
        pricing: ModelPricing {
            input_per_mtok: 5.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: 0.0,
            cache_create_per_mtok: 0.0,
        },
    }
}

// ─── Factory ──────────────────────────────────────────────────────────────────

/// Create a provider from a `"provider/model"` string.
///
/// Routing rules:
/// - `"anthropic/<model>"` → [`AnthropicProvider`]
/// - `"openai/<model>"` → [`OpenAiProvider`] against `https://api.openai.com/v1`
/// - `"openai-codex/<model>"` / `"openai-responses/<model>"` → [`ResponsesProvider`]
/// - `"openrouter/<model>"` → [`OpenAiProvider`] against `https://openrouter.ai/api/v1`
/// - `"<unknown>/<model>"` → [`OpenAiProvider`] using `base_url` (or OpenAI default)
/// - No slash → treated as an OpenAI model name
///
/// `base_url` overrides the default endpoint when provided and non-empty.
pub fn create_provider(
    model_string: &str,
    api_key: SecretString,
    base_url: Option<&str>,
) -> Result<Arc<dyn Provider>> {
    let (provider_name, model) = match model_string.split_once('/') {
        Some((p, m)) => (p, m),
        None => ("openai", model_string),
    };

    match provider_name {
        "anthropic" => {
            let url = base_url
                .filter(|s| !s.is_empty())
                .unwrap_or("https://api.anthropic.com/v1")
                .to_string();
            let config = AnthropicConfig {
                base_url: url,
                api_key,
                model: model.to_string(),
                api_version: "2023-06-01".to_string(),
                max_thinking_tokens: None,
            };
            let info = anthropic_model_info(model);
            Ok(Arc::new(AnthropicProvider::new(config, info)?))
        }
        "openrouter" => {
            let url = base_url
                .filter(|s| !s.is_empty())
                .unwrap_or("https://openrouter.ai/api/v1")
                .to_string();
            let config = OpenAiConfig {
                base_url: url,
                api_key,
                model: model.to_string(),
                org_id: None,
                auth_style: AuthStyle::Bearer,
            };
            let info = generic_model_info("openrouter", model);
            Ok(Arc::new(OpenAiProvider::new(config, info)?))
        }
        "openai" => {
            let url = base_url
                .filter(|s| !s.is_empty())
                .unwrap_or("https://api.openai.com/v1")
                .to_string();
            let config = OpenAiConfig {
                base_url: url,
                api_key,
                model: model.to_string(),
                org_id: None,
                auth_style: AuthStyle::Bearer,
            };
            let info = openai_model_info(model);
            Ok(Arc::new(OpenAiProvider::new(config, info)?))
        }
        "openai-codex" | "openai-responses" => {
            let url = base_url
                .filter(|s| !s.is_empty())
                .unwrap_or("https://api.openai.com/v1")
                .to_string();
            let config = OpenAiConfig {
                base_url: url,
                api_key,
                model: model.to_string(),
                org_id: None,
                auth_style: AuthStyle::Bearer,
            };
            let info = responses_model_info(model);
            Ok(Arc::new(ResponsesProvider::new(config, info)?))
        }
        unknown => {
            // Unknown providers default to OpenAI-compatible
            let url = base_url
                .filter(|s| !s.is_empty())
                .unwrap_or("https://api.openai.com/v1")
                .to_string();
            let config = OpenAiConfig {
                base_url: url,
                api_key,
                model: model.to_string(),
                org_id: None,
                auth_style: AuthStyle::Bearer,
            };
            let info = generic_model_info(unknown, model);
            Ok(Arc::new(OpenAiProvider::new(config, info)?))
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_anthropic_provider() {
        let p = create_provider(
            "anthropic/claude-sonnet-4-20250514",
            SecretString::new("sk-ant-key".into()),
            None,
        )
        .expect("should create anthropic provider");
        let info = p.model_info();
        assert_eq!(info.provider, "anthropic");
        assert_eq!(info.id, "claude-sonnet-4-20250514");
        assert!(info.supports_caching);
        assert!(info.supports_reasoning);
    }

    #[test]
    fn create_openai_provider() {
        let p = create_provider(
            "openai/gpt-4o",
            SecretString::new("sk-openai-key".into()),
            None,
        )
        .expect("should create openai provider");
        let info = p.model_info();
        assert_eq!(info.provider, "openai");
        assert_eq!(info.id, "gpt-4o");
    }

    #[test]
    fn create_openrouter_provider() {
        let p = create_provider(
            "openrouter/meta-llama/llama-3.1-8b-instruct",
            SecretString::new("sk-or-key".into()),
            None,
        )
        .expect("should create openrouter provider");
        let info = p.model_info();
        assert_eq!(info.provider, "openrouter");
        assert_eq!(info.id, "meta-llama/llama-3.1-8b-instruct");
    }

    #[test]
    fn create_openai_responses_provider() {
        let p = create_provider(
            "openai-codex/gpt-5",
            SecretString::new("sk-openai-key".into()),
            None,
        )
        .expect("should create responses provider");
        let info = p.model_info();
        assert_eq!(info.provider, "openai-codex");
        assert_eq!(info.id, "gpt-5");
        assert!(info.supports_reasoning);
    }

    #[test]
    fn create_unknown_provider_defaults_to_openai() {
        let p = create_provider(
            "mistral/mistral-large",
            SecretString::new("some-key".into()),
            None,
        )
        .expect("unknown provider should fall through to openai-compat");
        let info = p.model_info();
        assert_eq!(info.provider, "mistral");
        assert_eq!(info.id, "mistral-large");
    }

    #[test]
    fn no_slash_defaults_to_openai() {
        let p = create_provider("gpt-4o-mini", SecretString::new("sk-key".into()), None)
            .expect("no-slash model should default to openai");
        let info = p.model_info();
        assert_eq!(info.provider, "openai");
        assert_eq!(info.id, "gpt-4o-mini");
    }
}
