use super::{AnthropicProvider, ProviderResponse, error::ProviderError, AuthType};
use crate::models::{AnthropicRequest, CountTokensRequest, CountTokensResponse};
use crate::auth::{TokenStore, OAuthClient, OAuthConfig};
use crate::server::{parse_anthropic_beta, validate_anthropic_beta};
use async_trait::async_trait;
use reqwest::Client;
use std::pin::Pin;
use futures::stream::Stream;
use bytes::Bytes;
use tracing::{debug, warn};

/// Generic Anthropic-compatible provider
/// Works with: Anthropic, OpenRouter, z.ai, Minimax, etc.
/// Any provider that accepts Anthropic Messages API format
pub struct AnthropicCompatibleProvider {
    name: String,
    api_key: String,
    base_url: String,
    client: Client,
    models: Vec<String>,
    /// Custom headers to add (e.g., "HTTP-Referer" for OpenRouter)
    custom_headers: Vec<(String, String)>,
    /// Authentication type (ApiKey, OAuth, or Passthrough)
    auth_type: AuthType,
    /// OAuth provider ID (if using OAuth instead of API key)
    oauth_provider: Option<String>,
    /// Token store for OAuth authentication
    token_store: Option<TokenStore>,
    /// Supported anthropic-beta options for this provider
    supported_beta_options: Vec<String>,
}

impl AnthropicCompatibleProvider {
    pub fn new(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
    ) -> Self {
        Self::new_with_options(
            name,
            api_key,
            base_url,
            models,
            oauth_provider,
            token_store,
            Vec::new(),
        )
    }

    /// Create with explicit auth type
    pub fn new_with_auth(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        auth_type: AuthType,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
    ) -> Self {
        Self::new_with_options_and_auth(
            name,
            api_key,
            base_url,
            models,
            auth_type,
            oauth_provider,
            token_store,
            Vec::new(),
        )
    }

    /// Create with explicit auth type and supported beta options
    pub fn new_with_options_and_auth(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        auth_type: AuthType,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
        supported_beta_options: Vec<String>,
    ) -> Self {
        Self {
            name,
            api_key,
            base_url,
            client: Client::new(),
            models,
            custom_headers: Vec::new(),
            auth_type,
            oauth_provider,
            token_store,
            supported_beta_options,
        }
    }

    /// Create with supported beta options
    pub fn new_with_options(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
        supported_beta_options: Vec<String>,
    ) -> Self {
        Self::new_with_options_and_auth(
            name,
            api_key,
            base_url,
            models,
            AuthType::ApiKey,
            oauth_provider,
            token_store,
            supported_beta_options,
        )
    }

    /// Create with custom headers
    pub fn with_headers(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        custom_headers: Vec<(String, String)>,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
    ) -> Self {
        Self::with_headers_and_auth(
            name,
            api_key,
            base_url,
            models,
            custom_headers,
            AuthType::ApiKey,
            oauth_provider,
            token_store,
        )
    }

    /// Create with custom headers and auth type
    pub fn with_headers_and_auth(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        custom_headers: Vec<(String, String)>,
        auth_type: AuthType,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
    ) -> Self {
        Self::with_headers_auth_and_options(
            name,
            api_key,
            base_url,
            models,
            custom_headers,
            auth_type,
            oauth_provider,
            token_store,
            Vec::new(),
        )
    }

    /// Create with custom headers, auth type, and supported beta options
    pub fn with_headers_auth_and_options(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        custom_headers: Vec<(String, String)>,
        auth_type: AuthType,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
        supported_beta_options: Vec<String>,
    ) -> Self {
        Self {
            name,
            api_key,
            base_url,
            client: Client::new(),
            models,
            custom_headers,
            auth_type,
            oauth_provider,
            token_store,
            supported_beta_options,
        }
    }

    /// Get authentication header value. override_auth takes highest priority.
    async fn get_auth_header(&self, override_auth: Option<&str>) -> Result<String, ProviderError> {
        if let Some(token) = override_auth {
            // Validate passthrough token format (reject control characters)
            if token.chars().any(|c| c.is_control()) {
                return Err(ProviderError::AuthError(
                    "Bearer token contains invalid characters".to_string()
                ));
            }
            return Ok(token.to_string());
        }

        // Check if provider is configured for passthrough auth
        if self.auth_type == AuthType::Passthrough {
            return Err(ProviderError::AuthError(
                "Passthrough auth requires token from request headers".to_string()
            ));
        }

        if let Some(ref oauth_provider_id) = self.oauth_provider {
            if let Some(ref token_store) = self.token_store {
                if let Some(token) = token_store.get(oauth_provider_id) {
                    if token.needs_refresh() {
                        tracing::info!("🔄 Token for '{}' needs refresh, refreshing...", oauth_provider_id);
                        let config = OAuthConfig::anthropic();
                        let oauth_client = OAuthClient::new(config, token_store.clone());
                        match oauth_client.refresh_token(oauth_provider_id).await {
                            Ok(new_token) => {
                                tracing::info!("✅ Token refreshed successfully");
                                return Ok(new_token.access_token);
                            }
                            Err(e) => {
                                tracing::error!("❌ Failed to refresh token: {}", e);
                                return Err(ProviderError::AuthError(format!(
                                    "Failed to refresh OAuth token: {}", e
                                )));
                            }
                        }
                    } else {
                        return Ok(token.access_token);
                    }
                } else {
                    return Err(ProviderError::AuthError(format!(
                        "OAuth provider '{}' configured but no token found in store",
                        oauth_provider_id
                    )));
                }
            } else {
                return Err(ProviderError::AuthError(
                    "OAuth provider configured but TokenStore not available".to_string()
                ));
            }
        }

        Ok(self.api_key.clone())
    }

    /// Check if using OAuth authentication
    fn is_oauth(&self) -> bool {
        self.oauth_provider.is_some() && self.token_store.is_some()
    }

    /// Create Anthropic Native provider
    pub fn anthropic(api_key: String, models: Vec<String>) -> Self {
        Self::anthropic_with_auth(api_key, models, AuthType::ApiKey)
    }

    /// Create Anthropic Native provider with auth type
    pub fn anthropic_with_auth(api_key: String, models: Vec<String>, auth_type: AuthType) -> Self {
        Self::new_with_options_and_auth(
            "anthropic".to_string(),
            api_key,
            "https://api.anthropic.com".to_string(),
            models,
            auth_type,
            None,
            None,
            Vec::new(),
        )
    }

    /// Create OpenRouter provider
    pub fn openrouter(api_key: String, models: Vec<String>) -> Self {
        Self::openrouter_with_auth(api_key, models, AuthType::ApiKey)
    }

    /// Create OpenRouter provider with auth type
    pub fn openrouter_with_auth(api_key: String, models: Vec<String>, auth_type: AuthType) -> Self {
        Self::with_headers_auth_and_options(
            "openrouter".to_string(),
            api_key,
            "https://openrouter.ai/api".to_string(),
            models,
            vec![
                ("HTTP-Referer".to_string(), "https://github.com/bahkchanhee/claude-code-mux".to_string()),
                ("X-Title".to_string(), "Claude Code Mux".to_string()),
            ],
            auth_type,
            None,
            None,
            Vec::new(),
        )
    }

    /// Create z.ai provider (Anthropic-compatible)
    pub fn zai(api_key: String, models: Vec<String>, token_store: Option<TokenStore>) -> Self {
        Self::zai_with_auth(
            api_key,
            models,
            AuthType::ApiKey,
            token_store,
        )
    }

    /// Create z.ai provider with auth type
    pub fn zai_with_auth(api_key: String, models: Vec<String>, auth_type: AuthType, token_store: Option<TokenStore>) -> Self {
        Self::new_with_options_and_auth(
            "z.ai".to_string(),
            api_key,
            "https://api.z.ai/api/anthropic".to_string(),
            models,
            auth_type,
            None,
            token_store,
            Vec::new(),
        )
    }

    /// Create Minimax provider (Anthropic-compatible)
    pub fn minimax(api_key: String, models: Vec<String>, token_store: Option<TokenStore>) -> Self {
        Self::minimax_with_auth(
            api_key,
            models,
            AuthType::ApiKey,
            token_store,
        )
    }

    /// Create Minimax provider with auth type
    pub fn minimax_with_auth(api_key: String, models: Vec<String>, auth_type: AuthType, token_store: Option<TokenStore>) -> Self {
        Self::new_with_options_and_auth(
            "minimax".to_string(),
            api_key,
            "https://api.minimax.io/anthropic".to_string(),
            models,
            auth_type,
            None,
            token_store,
            Vec::new(),
        )
    }

    /// Create ZenMux provider (Anthropic-compatible proxy)
    pub fn zenmux(api_key: String, models: Vec<String>, token_store: Option<TokenStore>) -> Self {
        Self::zenmux_with_auth(
            api_key,
            models,
            AuthType::ApiKey,
            token_store,
        )
    }

    /// Create ZenMux provider with auth type
    pub fn zenmux_with_auth(api_key: String, models: Vec<String>, auth_type: AuthType, token_store: Option<TokenStore>) -> Self {
        Self::new_with_options_and_auth(
            "zenmux".to_string(),
            api_key,
            "https://zenmux.ai/api/anthropic".to_string(),
            models,
            auth_type,
            None,
            token_store,
            Vec::new(),
        )
    }

    /// Create Kimi For Coding provider (Anthropic-compatible)
    pub fn kimi_coding(api_key: String, models: Vec<String>, token_store: Option<TokenStore>) -> Self {
        Self::kimi_coding_with_auth(
            api_key,
            models,
            AuthType::ApiKey,
            token_store,
        )
    }

    /// Create Kimi For Coding provider with auth type
    pub fn kimi_coding_with_auth(api_key: String, models: Vec<String>, auth_type: AuthType, token_store: Option<TokenStore>) -> Self {
        Self::new_with_options_and_auth(
            "kimi-coding".to_string(),
            api_key,
            "https://api.kimi.com/coding".to_string(),
            models,
            auth_type,
            None,
            token_store,
            Vec::new(),
        )
    }
}

#[async_trait]
impl AnthropicProvider for AnthropicCompatibleProvider {
    async fn send_message(&self, request: AnthropicRequest) -> Result<ProviderResponse, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);

        // Get authentication header value (API key or OAuth token)
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;

        // Build request with authentication
        let mut req_builder = self.client
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json");

        // Set auth header based on OAuth vs API key
        if override_auth.is_some() || self.is_oauth() {
            req_builder = req_builder
                .header("Authorization", format!("Bearer {}", auth_value));
            tracing::debug!("🔐 Using OAuth Bearer token for {}", self.name);
        } else {
            req_builder = req_builder.header("x-api-key", auth_value);
        }

        // Add anthropic-beta header if provided
        if let Some(beta_header) = &request.anthropic_beta_header {
            debug!("{}: custom beta options provided, parsing and validating: '{}'", self.name, beta_header);
            
            // Parse the beta header (CSV format → individual options)
            match parse_anthropic_beta(beta_header) {
                Ok(parsed_options) => {
                    debug!("{}: parse_anthropic_beta succeeded, got {} options", self.name, parsed_options.len());
                    
                    // Validate options if provider has a supported list
                    if !self.supported_beta_options.is_empty() {
                        debug!("{}: provider has {} supported beta options, validating against model '{}'", 
                               self.name, self.supported_beta_options.len(), request.model);
                        
                        match validate_anthropic_beta(&parsed_options, &self.supported_beta_options, &request.model) {
                            Ok(()) => {
                                debug!("{}: beta options VALIDATED successfully for model '{}', applying header", 
                                       self.name, request.model);
                                req_builder = req_builder.header("anthropic-beta", beta_header);
                            }
                            Err(validation_error) => {
                                warn!("{}: beta VALIDATION FAILED for model '{}': {}. Falling back to defaults", 
                                      self.name, request.model, validation_error);
                                // Fall back to defaults on validation failure
                                let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
                                debug!("{}: applying default beta header: '{}'", self.name, default_beta);
                                req_builder = req_builder.header("anthropic-beta", default_beta);
                            }
                        }
                    } else {
                        // No supported list configured, accept the header as-is
                        debug!("{}: provider has NO supported beta options list configured, accepting header as-is", self.name);
                        req_builder = req_builder.header("anthropic-beta", beta_header);
                    }
                }
                Err(parse_error) => {
                    warn!("{}: parse_anthropic_beta FAILED: {}. Falling back to defaults", self.name, parse_error);
                    // Fall back to defaults on parse failure
                    let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
                    debug!("{}: applying default beta header: '{}'", self.name, default_beta);
                    req_builder = req_builder.header("anthropic-beta", default_beta);
                }
            }
        } else {
            let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
            debug!("{}: no anthropic-beta header in request, using defaults: '{}'", self.name, default_beta);
            req_builder = req_builder.header("anthropic-beta", default_beta);
        }

        // Add custom headers (for OpenRouter, etc.)
        for (key, value) in &self.custom_headers {
            req_builder = req_builder.header(key, value);
        }

        // Send request (pass-through, no transformation needed!)
        let response = req_builder
            .json(&request)
            .send()
            .await?;

        // Check for errors
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());

            // If 401 and using OAuth, token might be invalid/expired
            if status == 401 && self.is_oauth() {
                tracing::warn!("🔄 Received 401, OAuth token may be invalid or expired");
            }

            return Err(ProviderError::ApiError {
                status,
                message: format!("{} API error: {}", self.name, error_text),
            });
        }

        // Get response body as text for debugging
        let response_text = response.text().await?;
        tracing::debug!("{} provider response body: {}", self.name, response_text);

        // Try to parse the response (already in Anthropic format!)
        let provider_response: ProviderResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                tracing::error!("Failed to parse {} response: {}", self.name, e);
                tracing::error!("Response body was: {}", response_text);
                e
            })?;

        Ok(provider_response)
    }

    async fn count_tokens(&self, request: CountTokensRequest) -> Result<CountTokensResponse, ProviderError> {
        // For Anthropic native, use their count_tokens endpoint
        if self.name == "anthropic" {
            let url = format!("{}/v1/messages/count_tokens", self.base_url);

            // Get authentication
            let auth_value = self.get_auth_header(None).await?;

            let mut req_builder = self.client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("Content-Type", "application/json");

            // Set auth header
            if self.is_oauth() {
                req_builder = req_builder
                    .header("Authorization", format!("Bearer {}", auth_value))
                    .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14");
            } else {
                req_builder = req_builder.header("x-api-key", auth_value);
            }

            let response = req_builder
                .json(&request)
                .send()
                .await?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(ProviderError::ApiError {
                    status,
                    message: error_text,
                });
            }

            let count_response: CountTokensResponse = response.json().await?;
            return Ok(count_response);
        }

        // For other providers, use character-based estimation
        let mut total_chars = 0;

        if let Some(ref system) = request.system {
            let system_text = match system {
                crate::models::SystemPrompt::Text(text) => text.clone(),
                crate::models::SystemPrompt::Blocks(blocks) => {
                    blocks.iter().map(|b| b.text.clone()).collect::<Vec<_>>().join("\n")
                }
            };
            total_chars += system_text.len();
        }

        for msg in &request.messages {
            use crate::models::MessageContent;
            let content = match &msg.content {
                MessageContent::Text(text) => text.clone(),
                MessageContent::Blocks(blocks) => {
                    blocks.iter()
                        .filter_map(|block| {
                            match block {
                                crate::models::ContentBlock::Text { text } => Some(text.clone()),
                                crate::models::ContentBlock::ToolResult { content, .. } => {
                                    Some(content.to_string())
                                }
                                crate::models::ContentBlock::Thinking { thinking, .. } => {
                                    Some(thinking.clone())
                                }
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            };
            total_chars += content.len();
        }

        let estimated_tokens = (total_chars / 4) as u32;

        Ok(CountTokensResponse {
            input_tokens: estimated_tokens,
        })
    }

    async fn send_message_stream(
        &self,
        request: AnthropicRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>, ProviderError> {
        use futures::stream::TryStreamExt;

        let url = format!("{}/v1/messages", self.base_url);

        // Get authentication header value
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;

        // Build request with authentication
        let mut req_builder = self.client
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json");

        // Set auth header based on OAuth vs API key
        if override_auth.is_some() || self.is_oauth() {
            req_builder = req_builder
                .header("Authorization", format!("Bearer {}", auth_value));
            tracing::debug!("🔐 Using OAuth Bearer token for streaming on {}", self.name);
        } else {
            req_builder = req_builder.header("x-api-key", auth_value);
        }

        // Add anthropic-beta header if provided
        if let Some(beta_header) = &request.anthropic_beta_header {
            debug!("{} [stream]: custom beta options provided, parsing and validating: '{}'", self.name, beta_header);
            
            // Parse the beta header (CSV format → individual options)
            match parse_anthropic_beta(beta_header) {
                Ok(parsed_options) => {
                    debug!("{} [stream]: parse_anthropic_beta succeeded, got {} options", self.name, parsed_options.len());
                    
                    // Validate options if provider has a supported list
                    if !self.supported_beta_options.is_empty() {
                        debug!("{} [stream]: provider has {} supported beta options, validating against model '{}'", 
                               self.name, self.supported_beta_options.len(), request.model);
                        
                        match validate_anthropic_beta(&parsed_options, &self.supported_beta_options, &request.model) {
                            Ok(()) => {
                                debug!("{} [stream]: beta options VALIDATED successfully for model '{}', applying header", 
                                       self.name, request.model);
                                req_builder = req_builder.header("anthropic-beta", beta_header);
                            }
                            Err(validation_error) => {
                                warn!("{} [stream]: beta VALIDATION FAILED for model '{}': {}. Falling back to defaults", 
                                      self.name, request.model, validation_error);
                                // Fall back to defaults on validation failure
                                let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
                                debug!("{} [stream]: applying default beta header: '{}'", self.name, default_beta);
                                req_builder = req_builder.header("anthropic-beta", default_beta);
                            }
                        }
                    } else {
                        // No supported list configured, accept the header as-is
                        debug!("{} [stream]: provider has NO supported beta options list configured, accepting header as-is", self.name);
                        req_builder = req_builder.header("anthropic-beta", beta_header);
                    }
                }
                Err(parse_error) => {
                    warn!("{} [stream]: parse_anthropic_beta FAILED: {}. Falling back to defaults", self.name, parse_error);
                    // Fall back to defaults on parse failure
                    let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
                    debug!("{} [stream]: applying default beta header: '{}'", self.name, default_beta);
                    req_builder = req_builder.header("anthropic-beta", default_beta);
                }
            }
        } else {
            let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
            debug!("{} [stream]: no anthropic-beta header in request, using defaults: '{}'", self.name, default_beta);
            req_builder = req_builder.header("anthropic-beta", default_beta);
        }

        // Add custom headers
        for (key, value) in &self.custom_headers {
            req_builder = req_builder.header(key, value);
        }

        // Send request with stream=true
        let response = req_builder
            .json(&request)
            .send()
            .await?;

        // Check for errors
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());

            if status == 401 && self.is_oauth() {
                tracing::warn!("🔄 Received 401 on streaming, OAuth token may be invalid or expired");
            }

            return Err(ProviderError::ApiError {
                status,
                message: format!("{} API error: {}", self.name, error_text),
            });
        }

        // Return the byte stream directly
        let stream = response.bytes_stream().map_err(|e| ProviderError::HttpError(e));

        Ok(Box::pin(stream))
    }

    fn supports_model(&self, model: &str) -> bool {
        self.models.iter().any(|m| m == model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provider() -> AnthropicCompatibleProvider {
        AnthropicCompatibleProvider::new(
            "test".to_string(),
            "internal-api-key".to_string(),
            "https://api.anthropic.com".to_string(),
            vec![],
            None,
            None,
        )
    }

    #[tokio::test]
    async fn test_get_auth_header_uses_override_when_provided() {
        let provider = make_provider();
        let result = provider.get_auth_header(Some("caller-token")).await.unwrap();
        assert_eq!(result, "caller-token");
    }

    #[tokio::test]
    async fn test_get_auth_header_falls_back_to_api_key_when_no_override() {
        let provider = make_provider();
        let result = provider.get_auth_header(None).await.unwrap();
        assert_eq!(result, "internal-api-key");
    }
}
