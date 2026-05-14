use super::gemini::GeminiProvider;
use super::{
    error::ProviderError, AnthropicCompatibleProvider, AnthropicProvider, OpenAIProvider,
    ProviderConfig,
};
use crate::auth::TokenStore;
use std::collections::HashMap;
use std::sync::Arc;

/// Provider registry that manages all configured providers
pub struct ProviderRegistry {
    /// Map of provider name -> provider instance
    providers: HashMap<String, Arc<Box<dyn AnthropicProvider>>>,
    /// Map of model name -> provider name for fast lookup
    model_to_provider: HashMap<String, String>,
    /// Map of provider name -> provider_type string for type validation
    provider_types: HashMap<String, String>,
}

impl ProviderRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            model_to_provider: HashMap::new(),
            provider_types: HashMap::new(),
        }
    }

    /// Load providers from configuration
    pub fn from_configs(
        configs: &[ProviderConfig],
        token_store: Option<TokenStore>,
    ) -> Result<Self, ProviderError> {
        let mut registry = Self::new();

        for config in configs {
            // Skip disabled providers
            if !config.is_enabled() {
                continue;
            }

            if config.rate_limit_rpm == Some(0) {
                return Err(ProviderError::ConfigError(format!(
                    "Provider '{}' has invalid rate_limit_rpm=0 (use null to disable or a value > 0)",
                    config.name
                )));
            }

            if config.rate_limit_rpm.is_some() && config.rate_limit_max_wait_ms == Some(0) {
                return Err(ProviderError::ConfigError(format!(
                    "Provider '{}' has invalid rate_limit_max_wait_ms=0 (use null for default or a value > 0)",
                    config.name
                )));
            }

            // Get API key - required for API key auth, skipped for OAuth and Passthrough
            let api_key = match &config.auth_type {
                super::AuthType::ApiKey => config.api_key.clone().ok_or_else(|| {
                    ProviderError::ConfigError(format!(
                        "Provider '{}' requires api_key for ApiKey auth",
                        config.name
                    ))
                })?,
                super::AuthType::OAuth => {
                    // OAuth providers will handle authentication differently
                    // For now, use a placeholder - will be replaced with token
                    config
                        .oauth_provider
                        .clone()
                        .unwrap_or_else(|| config.name.clone())
                }
                super::AuthType::Passthrough => {
                    // Passthrough auth - token comes from request header
                    // Use placeholder since actual token is provided at request time
                    "passthrough".to_string()
                }
            };

            // Create provider instance based on type
            let provider: Box<dyn AnthropicProvider> = match config.provider_type.as_str() {
                // OpenAI
                "openai" => Box::new(OpenAIProvider::new_with_auth(
                    config.name.clone(),
                    api_key,
                    config
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                    config.models.clone(),
                    config.auth_type.clone(),
                    config.oauth_provider.clone(),
                    token_store.clone(),
                )),

                // Anthropic-compatible providers
                "anthropic" => Box::new(
                    AnthropicCompatibleProvider::new_with_options_and_auth(
                        config.name.clone(),
                        api_key,
                        config
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
                        config.models.clone(),
                        config.auth_type.clone(),
                        config.oauth_provider.clone(),
                        token_store.clone(),
                        config.supported_beta_options.clone(),
                    )
                    .with_rate_limit_config(config.rate_limit_rpm, config.rate_limit_max_wait_ms),
                ),
                "nvidia-nim" => Box::new(
                    OpenAIProvider::new_with_auth(
                        config.name.clone(),
                        api_key,
                        config
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://integrate.api.nvidia.com/v1".to_string()),
                        config.models.clone(),
                        config.auth_type.clone(),
                        config.oauth_provider.clone(),
                        token_store.clone(),
                    )
                    .with_rate_limit_config(config.rate_limit_rpm, config.rate_limit_max_wait_ms),
                ),
                "z.ai" => Box::new(
                    AnthropicCompatibleProvider::zai_with_auth(
                        api_key,
                        config.models.clone(),
                        config.auth_type.clone(),
                        token_store.clone(),
                    )
                    .with_rate_limit_config(config.rate_limit_rpm, config.rate_limit_max_wait_ms),
                ),
                "minimax" => Box::new(
                    AnthropicCompatibleProvider::minimax_with_auth(
                        api_key,
                        config.models.clone(),
                        config.auth_type.clone(),
                        token_store.clone(),
                    )
                    .with_rate_limit_config(config.rate_limit_rpm, config.rate_limit_max_wait_ms),
                ),
                "zenmux" => Box::new(
                    AnthropicCompatibleProvider::zenmux_with_auth(
                        api_key,
                        config.models.clone(),
                        config.auth_type.clone(),
                        token_store.clone(),
                    )
                    .with_rate_limit_config(config.rate_limit_rpm, config.rate_limit_max_wait_ms),
                ),
                "kimi-coding" => Box::new(
                    AnthropicCompatibleProvider::kimi_coding_with_auth(
                        api_key,
                        config.models.clone(),
                        config.auth_type.clone(),
                        token_store.clone(),
                    )
                    .with_rate_limit_config(config.rate_limit_rpm, config.rate_limit_max_wait_ms),
                ),

                // OpenAI-compatible providers
                "openrouter" => Box::new(OpenAIProvider::openrouter(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "deepinfra" => Box::new(OpenAIProvider::deepinfra(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "novita" => Box::new(OpenAIProvider::novita(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "baseten" => Box::new(OpenAIProvider::baseten(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "together" => Box::new(OpenAIProvider::together(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "fireworks" => Box::new(OpenAIProvider::fireworks(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "groq" => Box::new(OpenAIProvider::groq(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "nebius" => Box::new(OpenAIProvider::nebius(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "cerebras" => Box::new(OpenAIProvider::cerebras(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),
                "moonshot" => Box::new(OpenAIProvider::moonshot(
                    config.name.clone(),
                    api_key,
                    config.models.clone(),
                )),

                // Google Gemini (supports OAuth, API Key, Vertex AI)
                "gemini" => {
                    let api_key_opt = if config.auth_type == super::AuthType::ApiKey {
                        Some(api_key.clone())
                    } else {
                        None
                    };

                    Box::new(GeminiProvider::new(
                        config.name.clone(),
                        api_key_opt,
                        config.base_url.clone(),
                        config.models.clone(),
                        HashMap::new(), // custom headers
                        config.oauth_provider.clone(),
                        token_store.clone(),
                        None, // No project_id/location for Gemini (AI Studio/OAuth only)
                        None,
                    ))
                }

                "vertex-ai" => {
                    // Vertex AI provider (separate from Gemini)
                    // Uses Google Cloud Vertex AI with ADC authentication
                    Box::new(GeminiProvider::new(
                        config.name.clone(),
                        None, // No API key for Vertex AI (uses ADC)
                        config.base_url.clone(),
                        config.models.clone(),
                        HashMap::new(), // custom headers
                        None,           // No OAuth for Vertex AI
                        token_store.clone(),
                        config.project_id.clone(), // GCP project ID
                        config.location.clone(),   // GCP location
                    ))
                }

                // GitHub Copilot (OAuth-based)
                "copilot" => Box::new(crate::providers::CopilotProvider::new(
                    config.name.clone(),
                    config.models.clone(),
                    token_store.clone(),
                )),

                other => {
                    return Err(ProviderError::ConfigError(format!(
                        "Unknown provider type: {}",
                        other
                    )));
                }
            };

            // NOTE: models field in provider config is deprecated
            // Model mappings are now defined in [[models]] section
            // We only register the provider by name

            // Add provider to registry with debug logging
            let provider_name = config.name.clone();
            let beta_options = &config.supported_beta_options;

            if !beta_options.is_empty() {
                tracing::debug!(
                    "registering provider '{}': supported beta options: {:?}",
                    provider_name,
                    beta_options
                );
            } else {
                tracing::debug!(
                    "registering provider '{}': no specific beta options configured",
                    provider_name
                );
            }

            registry.provider_types.insert(provider_name.clone(), config.provider_type.clone());
            registry.providers.insert(provider_name, Arc::new(provider));
        }

        Ok(registry)
    }

    /// Get a provider by name
    pub fn get_provider(&self, name: &str) -> Option<Arc<Box<dyn AnthropicProvider>>> {
        self.providers.get(name).cloned()
    }

    /// Get a provider for a specific model
    pub fn get_provider_for_model(
        &self,
        model: &str,
    ) -> Result<Arc<Box<dyn AnthropicProvider>>, ProviderError> {
        // First, check if we have a direct model → provider mapping
        if let Some(provider_name) = self.model_to_provider.get(model) {
            if let Some(provider) = self.providers.get(provider_name) {
                return Ok(provider.clone());
            }
        }

        // If no direct mapping, search through all providers
        for provider in self.providers.values() {
            if provider.supports_model(model) {
                return Ok(provider.clone());
            }
        }

        Err(ProviderError::ModelNotSupported(model.to_string()))
    }

    /// List all available models
    pub fn list_models(&self) -> Vec<String> {
        self.model_to_provider.keys().cloned().collect()
    }

    /// List all providers
    pub fn list_providers(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AnthropicRequest, ContentBlock, Message, MessageContent};

    #[test]
    fn test_empty_registry() {
        let registry = ProviderRegistry::new();
        assert!(registry.list_models().is_empty());
        assert!(registry.list_providers().is_empty());
    }

    #[test]
    fn test_get_provider_for_model_not_found() {
        let registry = ProviderRegistry::new();
        let result = registry.get_provider_for_model("gpt-4");
        assert!(result.is_err());
    }

    #[test]
    fn test_nvidia_nim_provider_config_with_rate_limit() {
        let config = ProviderConfig {
            name: "nvidia-nim".to_string(),
            provider_type: "nvidia-nim".to_string(),
            auth_type: Default::default(),
            supported_beta_options: vec![],
            api_key: Some("test-key".to_string()),
            oauth_provider: None,
            project_id: None,
            location: None,
            base_url: Some("https://integrate.api.nvidia.com/v1".to_string()),
            models: vec!["meta-llama-3.1-405b-instruct".to_string()],
            enabled: Some(true),
            rate_limit_rpm: Some(40),
            rate_limit_max_wait_ms: Some(2000),
        };

        // Verify provider configuration has rate limit set
        assert_eq!(config.name, "nvidia-nim");
        assert_eq!(config.provider_type, "nvidia-nim");
        assert_eq!(config.rate_limit_rpm, Some(40));
        assert_eq!(
            config.base_url,
            Some("https://integrate.api.nvidia.com/v1".to_string())
        );
    }

    #[test]
    fn test_provider_registry_with_nvidia_nim() {
        let config = ProviderConfig {
            name: "nvidia-nim".to_string(),
            provider_type: "nvidia-nim".to_string(),
            auth_type: Default::default(),
            supported_beta_options: vec![],
            api_key: Some("test-key".to_string()),
            oauth_provider: None,
            project_id: None,
            location: None,
            base_url: Some("https://integrate.api.nvidia.com/v1".to_string()),
            models: vec!["meta-llama-3.1-405b-instruct".to_string()],
            enabled: Some(true),
            rate_limit_rpm: Some(40),
            rate_limit_max_wait_ms: Some(2000),
        };

        let registry = ProviderRegistry::from_configs(&[config], None);
        assert!(registry.is_ok());

        let registry = registry.unwrap();
        let provider = registry.get_provider("nvidia-nim");
        assert!(provider.is_some());
    }

    #[test]
    fn test_rate_limit_rpm_optional() {
        let config = ProviderConfig {
            name: "anthropic".to_string(),
            provider_type: "anthropic".to_string(),
            auth_type: Default::default(),
            supported_beta_options: vec![],
            api_key: Some("test-key".to_string()),
            oauth_provider: None,
            project_id: None,
            location: None,
            base_url: None,
            models: vec![],
            enabled: Some(true),
            rate_limit_rpm: None,
            rate_limit_max_wait_ms: None,
        };

        // Verify providers without rate_limit_rpm work fine
        assert_eq!(config.rate_limit_rpm, None);

        let registry = ProviderRegistry::from_configs(&[config], None);
        assert!(registry.is_ok());
    }

    #[test]
    fn test_rate_limit_rpm_zero_is_rejected() {
        let config = ProviderConfig {
            name: "nvidia-nim".to_string(),
            provider_type: "nvidia-nim".to_string(),
            auth_type: Default::default(),
            supported_beta_options: vec![],
            api_key: Some("test-key".to_string()),
            oauth_provider: None,
            project_id: None,
            location: None,
            base_url: Some("https://integrate.api.nvidia.com/v1".to_string()),
            models: vec!["meta-llama-3.1-405b-instruct".to_string()],
            enabled: Some(true),
            rate_limit_rpm: Some(0),
            rate_limit_max_wait_ms: Some(1000),
        };

        let err = ProviderRegistry::from_configs(&[config], None)
            .err()
            .expect("expected config error for rate_limit_rpm=0");
        match err {
            ProviderError::ConfigError(msg) => {
                assert!(msg.contains("rate_limit_rpm=0"));
            }
            other => panic!("expected config error, got: {other:?}"),
        }
    }

    #[test]
    fn test_rate_limit_max_wait_zero_is_rejected() {
        let config = ProviderConfig {
            name: "nvidia-nim".to_string(),
            provider_type: "nvidia-nim".to_string(),
            auth_type: Default::default(),
            supported_beta_options: vec![],
            api_key: Some("test-key".to_string()),
            oauth_provider: None,
            project_id: None,
            location: None,
            base_url: Some("https://integrate.api.nvidia.com/v1".to_string()),
            models: vec!["meta-llama-3.1-405b-instruct".to_string()],
            enabled: Some(true),
            rate_limit_rpm: Some(1),
            rate_limit_max_wait_ms: Some(0),
        };

        let err = ProviderRegistry::from_configs(&[config], None)
            .err()
            .expect("expected config error for rate_limit_max_wait_ms=0");
        match err {
            ProviderError::ConfigError(msg) => {
                assert!(msg.contains("rate_limit_max_wait_ms=0"));
            }
            other => panic!("expected config error, got: {other:?}"),
        }
    }

    #[test]
    fn test_disabled_provider_with_invalid_rate_limit_is_skipped() {
        let config = ProviderConfig {
            name: "disabled-provider".to_string(),
            provider_type: "anthropic".to_string(),
            auth_type: Default::default(),
            supported_beta_options: vec![],
            api_key: Some("test-key".to_string()),
            oauth_provider: None,
            project_id: None,
            location: None,
            base_url: None,
            models: vec![],
            enabled: Some(false),
            rate_limit_rpm: Some(0),
            rate_limit_max_wait_ms: Some(0),
        };

        let registry = ProviderRegistry::from_configs(&[config], None);
        assert!(registry.is_ok(), "disabled providers should be skipped");
    }

    async fn start_nim_mock_server() -> std::net::SocketAddr {
        use axum::{routing::post, Json, Router};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Json(serde_json::json!({
                    "id": "chatcmpl-1",
                    "object": "chat.completion",
                    "model": "meta-llama-3.1-405b-instruct",
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "nim ok"
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2
                    }
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        addr
    }

    #[tokio::test]
    async fn test_nvidia_nim_uses_openai_chat_completions_endpoint() {
        let addr = start_nim_mock_server().await;
        let config = ProviderConfig {
            name: "nvidia-nim".to_string(),
            provider_type: "nvidia-nim".to_string(),
            auth_type: Default::default(),
            supported_beta_options: vec![],
            api_key: Some("test-key".to_string()),
            oauth_provider: None,
            project_id: None,
            location: None,
            base_url: Some(format!("http://{addr}/v1")),
            models: vec!["meta-llama-3.1-405b-instruct".to_string()],
            enabled: Some(true),
            rate_limit_rpm: Some(40),
            rate_limit_max_wait_ms: Some(2000),
        };

        let registry = ProviderRegistry::from_configs(&[config], None).unwrap();
        let provider = registry.get_provider("nvidia-nim").unwrap();
        let request = AnthropicRequest {
            model: "meta-llama-3.1-405b-instruct".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text("hello".to_string()),
            }],
            max_tokens: 64,
            thinking: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            metadata: None,
            system: None,
            tools: None,
            passthrough_auth: None,
            anthropic_beta_header: None,
        };

        let response = provider.send_message(request).await.unwrap();
        assert_eq!(response.model, "meta-llama-3.1-405b-instruct");
        assert!(matches!(
            response.content.as_slice(),
            [ContentBlock::Text { text }] if text == "nim ok"
        ));
    }

    #[test]
    fn test_copilot_provider_registration() {
        use crate::providers::{AuthType, ProviderConfig};

        let config = ProviderConfig {
            name: "my-copilot".to_string(),
            provider_type: "copilot".to_string(),
            auth_type: AuthType::OAuth,
            oauth_provider: Some("my-copilot".to_string()),
            api_key: None,
            base_url: None,
            project_id: None,
            location: None,
            models: vec!["gpt-4o".to_string()],
            enabled: Some(true),
            supported_beta_options: vec![],
            rate_limit_rpm: None,
            rate_limit_max_wait_ms: None,
        };

        let result = ProviderRegistry::from_configs(&[config], None);
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());
        let registry = result.unwrap();
        assert!(registry.get_provider("my-copilot").is_some());
    }
}
