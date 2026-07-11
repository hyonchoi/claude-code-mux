use super::{error::ProviderError, AnthropicProvider, AuthType, ProviderResponse};
use crate::auth::{OAuthClient, OAuthConfig, TokenStore};
use crate::models::{AnthropicRequest, CountTokensRequest, CountTokensResponse};
use crate::server::{parse_anthropic_beta, validate_anthropic_beta};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::Stream;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use reqwest::Client;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use tokio::time::{timeout, Duration, Instant};
use tracing::{debug, warn};

pub(crate) const DEFAULT_RATE_LIMIT_MAX_WAIT_MS: u64 = 2_000;

/// Which header carries the API key for statically-configured (non-OAuth,
/// non-passthrough) auth. Anthropic-native and most Anthropic-compatible
/// providers expect `x-api-key`; self-hosted OpenAI-convention servers like
/// vLLM/SGLang (run with `--api-key`) reject that and require
/// `Authorization: Bearer <key>` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnthropicAuthHeaderStyle {
    #[default]
    XApiKey,
    Bearer,
}

/// Parse an upstream response body as `ProviderResponse`, tolerating a missing or
/// malformed `usage` object. `ProviderResponse.usage` is a required (non-`Option`)
/// field; some Anthropic-compatible backends (confirmed for NVIDIA NIM's OpenAI
/// Chat Completions path, see CHANGELOG.md) omit usage entirely on some responses.
/// Rather than failing the whole parse over a missing token count, default each of
/// `input_tokens`/`output_tokens` to 0 independently when absent or malformed —
/// a response with `{"input_tokens": 500}` keeps the 500, only `output_tokens`
/// gets defaulted, instead of the whole usage object being zeroed out (adversarial
/// review flagged the earlier all-or-nothing version as silently destroying real
/// token counts, which feeds directly into billing/quota reporting).
fn parse_provider_response(
    response_text: &str,
    provider_name: &str,
) -> Result<ProviderResponse, ProviderError> {
    let mut value: serde_json::Value = serde_json::from_str(response_text).map_err(|e| {
        tracing::error!("Failed to parse {} response: {}", provider_name, e);
        tracing::error!("Response body was: {}", response_text);
        ProviderError::from(e)
    })?;

    let usage_is_object = value.get("usage").is_some_and(|u| u.is_object());
    let input_tokens = value
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64());
    let output_tokens = value
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64());

    if !usage_is_object || input_tokens.is_none() || output_tokens.is_none() {
        tracing::warn!(
            "{} response missing/malformed usage field(s) — defaulting missing token counts to 0 (input_tokens={:?}, output_tokens={:?})",
            provider_name,
            input_tokens,
            output_tokens
        );
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "usage".to_string(),
                serde_json::json!({
                    "input_tokens": input_tokens.unwrap_or(0),
                    "output_tokens": output_tokens.unwrap_or(0)
                }),
            );
        }
    }

    serde_json::from_value(value).map_err(|e| {
        tracing::error!("Failed to parse {} response: {}", provider_name, e);
        e.into()
    })
}

/// Generic Anthropic-compatible provider
/// Works with: Anthropic, OpenRouter, z.ai, Minimax, NVIDIA NIM, etc.
/// Any provider that accepts Anthropic Messages API format
pub struct AnthropicCompatibleProvider {
    name: String,
    api_key: String,
    base_url: String,
    client: Client,
    models: Vec<String>,
    /// Custom headers to add (e.g., "HTTP-Referer" for OpenRouter). Applied after the
    /// auth header at each call site — a custom header named "Authorization" or
    /// "x-api-key" would be sent alongside the auth header (reqwest does not dedupe),
    /// not override it. Not currently user-configurable (only set by hardcoded
    /// constructors like `openrouter_with_auth`), so this is a latent risk, not a
    /// reachable bug today.
    custom_headers: Vec<(String, String)>,
    /// Authentication type (ApiKey, OAuth, or Passthrough)
    auth_type: AuthType,
    /// Header style for statically-configured API-key auth (x-api-key vs Bearer).
    /// Only affects the `AuthType::ApiKey` path — Passthrough and OAuth always use Bearer.
    header_style: AnthropicAuthHeaderStyle,
    /// OAuth provider ID (if using OAuth instead of API key)
    oauth_provider: Option<String>,
    /// Token store for OAuth authentication
    token_store: Option<TokenStore>,
    /// Supported anthropic-beta options for this provider
    supported_beta_options: Vec<String>,
    /// Rate limit in requests per minute (e.g., 40 for NVIDIA NIM)
    /// If set, outbound message and stream requests are throttled at this budget.
    rate_limit_rpm: Option<u32>,
    /// Maximum wait time before failing over to the next mapping.
    rate_limit_max_wait_ms: Option<u64>,
    /// Token-bucket limiter for this provider instance.
    rate_limiter: Option<Arc<DefaultDirectRateLimiter>>,
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
            header_style: AnthropicAuthHeaderStyle::default(),
            oauth_provider,
            token_store,
            supported_beta_options,
            rate_limit_rpm: None,
            rate_limit_max_wait_ms: None,
            rate_limiter: None,
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
            header_style: AnthropicAuthHeaderStyle::default(),
            oauth_provider,
            token_store,
            supported_beta_options,
            rate_limit_rpm: None,
            rate_limit_max_wait_ms: None,
            rate_limiter: None,
        }
    }

    /// Set the rate limit in requests per minute (useful for NVIDIA NIM: 40 rpm)
    pub fn with_rate_limit(mut self, rate_limit_rpm: Option<u32>) -> Self {
        self = self.with_rate_limit_config(rate_limit_rpm, None);
        self
    }

    /// Set rate-limit parameters for this provider instance.
    pub fn with_rate_limit_config(
        mut self,
        rate_limit_rpm: Option<u32>,
        rate_limit_max_wait_ms: Option<u64>,
    ) -> Self {
        self.rate_limit_rpm = rate_limit_rpm;
        self.rate_limit_max_wait_ms = rate_limit_rpm.map(|_| {
            rate_limit_max_wait_ms
                .filter(|max_wait_ms| *max_wait_ms > 0)
                .unwrap_or(DEFAULT_RATE_LIMIT_MAX_WAIT_MS)
        });
        self.rate_limiter = rate_limit_rpm
            .and_then(NonZeroU32::new)
            .map(|rpm| Arc::new(RateLimiter::direct(Quota::per_minute(rpm))));
        self
    }

    /// Set the header style for statically-configured API-key auth (x-api-key vs Bearer).
    /// Used by self-hosted OpenAI-convention providers (vLLM, SGLang) that reject x-api-key.
    pub fn with_header_style(mut self, header_style: AnthropicAuthHeaderStyle) -> Self {
        self.header_style = header_style;
        self
    }

    /// Whether outbound requests should carry `Authorization: Bearer <token>` instead of
    /// `x-api-key`. True for Passthrough and OAuth (always) or when this provider's
    /// `header_style` is explicitly set to `Bearer` (vLLM/SGLang).
    pub(crate) fn is_bearer_auth(&self) -> bool {
        self.auth_type == AuthType::Passthrough
            || self.is_oauth()
            || self.header_style == AnthropicAuthHeaderStyle::Bearer
    }

    async fn await_rate_limit_permit(&self) -> Result<(), ProviderError> {
        let Some(rpm) = self.rate_limit_rpm else {
            return Ok(());
        };

        if rpm == 0 {
            return Err(ProviderError::ConfigError(format!(
                "Provider '{}' has invalid rate_limit_rpm=0",
                self.name
            )));
        }

        let max_wait_ms = self
            .rate_limit_max_wait_ms
            .unwrap_or(DEFAULT_RATE_LIMIT_MAX_WAIT_MS);
        let limiter = self.rate_limiter.as_ref().ok_or_else(|| {
            ProviderError::ConfigError(format!(
                "Provider '{}' rate limiter not initialized",
                self.name
            ))
        })?;

        let started = Instant::now();
        match timeout(Duration::from_millis(max_wait_ms), limiter.until_ready()).await {
            Ok(()) => {
                let waited = started.elapsed().as_millis() as u64;
                if waited > 0 {
                    debug!(
                        provider = %self.name,
                        rpm = rpm,
                        waited_ms = waited,
                        max_wait_ms = max_wait_ms,
                        "Rate limiter delayed provider request",
                    );
                }
                Ok(())
            }
            Err(_) => {
                warn!(
                    provider = %self.name,
                    rpm = rpm,
                    waited_ms = max_wait_ms,
                    max_wait_ms = max_wait_ms,
                    "Rate limiter wait budget exceeded; allowing fallback",
                );
                Err(ProviderError::RateLimitTimeout {
                    provider: self.name.clone(),
                    rpm,
                    max_wait_ms,
                })
            }
        }
    }

    /// Get authentication token value.
    /// Caller override is only honored when this provider is configured for passthrough auth.
    async fn get_auth_header(&self, override_auth: Option<&str>) -> Result<String, ProviderError> {
        if self.auth_type == AuthType::Passthrough {
            if let Some(token) = override_auth {
                // Validate passthrough token format (reject control characters)
                if token.chars().any(|c| c.is_control()) {
                    return Err(ProviderError::AuthError(
                        "Bearer token contains invalid characters".to_string(),
                    ));
                }
                return Ok(token.to_string());
            }
            return Err(ProviderError::AuthError(
                "Passthrough auth requires token from request headers".to_string(),
            ));
        }

        if let Some(ref oauth_provider_id) = self.oauth_provider {
            if let Some(ref token_store) = self.token_store {
                if let Some(token) = token_store.get(oauth_provider_id) {
                    if token.needs_refresh() {
                        tracing::info!(
                            "🔄 Token for '{}' needs refresh, refreshing...",
                            oauth_provider_id
                        );
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
                                    "Failed to refresh OAuth token: {}",
                                    e
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
                    "OAuth provider configured but TokenStore not available".to_string(),
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
                (
                    "HTTP-Referer".to_string(),
                    "https://github.com/bahkchanhee/claude-code-mux".to_string(),
                ),
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
        Self::zai_with_auth(api_key, models, AuthType::ApiKey, token_store)
    }

    /// Create z.ai provider with auth type
    pub fn zai_with_auth(
        api_key: String,
        models: Vec<String>,
        auth_type: AuthType,
        token_store: Option<TokenStore>,
    ) -> Self {
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
        Self::minimax_with_auth(api_key, models, AuthType::ApiKey, token_store)
    }

    /// Create Minimax provider with auth type
    pub fn minimax_with_auth(
        api_key: String,
        models: Vec<String>,
        auth_type: AuthType,
        token_store: Option<TokenStore>,
    ) -> Self {
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
        Self::zenmux_with_auth(api_key, models, AuthType::ApiKey, token_store)
    }

    /// Create ZenMux provider with auth type
    pub fn zenmux_with_auth(
        api_key: String,
        models: Vec<String>,
        auth_type: AuthType,
        token_store: Option<TokenStore>,
    ) -> Self {
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
    pub fn kimi_coding(
        api_key: String,
        models: Vec<String>,
        token_store: Option<TokenStore>,
    ) -> Self {
        Self::kimi_coding_with_auth(api_key, models, AuthType::ApiKey, token_store)
    }

    /// Create Kimi For Coding provider with auth type
    pub fn kimi_coding_with_auth(
        api_key: String,
        models: Vec<String>,
        auth_type: AuthType,
        token_store: Option<TokenStore>,
    ) -> Self {
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

/// Strip thinking blocks from all non-last assistant turns when the request
/// carries a `clear_thinking_20251015` context_management directive. The
/// directive is meant to be applied server-side by Anthropic, but it only
/// clears blocks in cache-checkpointed segments. Prior user turns sent as
/// plain strings have no cache_control, so Anthropic cannot identify them as
/// cached and still validates (and rejects) fake signatures from vLLM. We
/// apply the same transformation client-side so the provider never sees them.
fn apply_clear_thinking_directive(request: &mut AnthropicRequest) {
    use crate::models::{ContentBlock, MessageContent};

    let has_directive = request
        .context_management
        .as_ref()
        .and_then(|cm| cm.get("edits"))
        .and_then(|e| e.as_array())
        .is_some_and(|edits| {
            edits.iter().any(|edit| {
                edit.get("type").and_then(|t| t.as_str())
                    == Some("clear_thinking_20251015")
            })
        });

    if !has_directive {
        return;
    }

    // Strip thinking blocks from ALL assistant messages, including the last.
    for msg in request.messages.iter_mut() {
        if msg.role != "assistant" {
            continue;
        }
        if let MessageContent::Blocks(blocks) = &mut msg.content {
            blocks.retain(|b| {
                !matches!(
                    b,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                )
            });
        }
    }
}

#[async_trait]
impl AnthropicProvider for AnthropicCompatibleProvider {
    async fn send_message(
        &self,
        mut request: AnthropicRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        apply_clear_thinking_directive(&mut request);
        self.await_rate_limit_permit().await?;

        let url = format!("{}/v1/messages", self.base_url);

        // Get authentication header value (API key or OAuth token)
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;

        // Build request with authentication
        let mut req_builder = self
            .client
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json");

        // Set auth header based on OAuth/passthrough/header_style
        if self.is_bearer_auth() {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", auth_value));
            tracing::debug!("🔐 Using Bearer token for {}", self.name);
        } else {
            req_builder = req_builder.header("x-api-key", auth_value);
        }

        // Add anthropic-beta header if provided
        if let Some(beta_header) = &request.anthropic_beta_header {
            debug!(
                "{}: custom beta options provided, parsing and validating: '{}'",
                self.name, beta_header
            );

            // Parse the beta header (CSV format → individual options)
            match parse_anthropic_beta(beta_header) {
                Ok(parsed_options) => {
                    debug!(
                        "{}: parse_anthropic_beta succeeded, got {} options",
                        self.name,
                        parsed_options.len()
                    );

                    // Validate options if provider has a supported list
                    if !self.supported_beta_options.is_empty() {
                        debug!("{}: provider has {} supported beta options, validating against model '{}'", 
                               self.name, self.supported_beta_options.len(), request.model);

                        match validate_anthropic_beta(
                            &parsed_options,
                            &self.supported_beta_options,
                            &request.model,
                        ) {
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
                                debug!(
                                    "{}: applying default beta header: '{}'",
                                    self.name, default_beta
                                );
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
                    warn!(
                        "{}: parse_anthropic_beta FAILED: {}. Falling back to defaults",
                        self.name, parse_error
                    );
                    // Fall back to defaults on parse failure
                    let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
                    debug!(
                        "{}: applying default beta header: '{}'",
                        self.name, default_beta
                    );
                    req_builder = req_builder.header("anthropic-beta", default_beta);
                }
            }
        } else {
            let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
            debug!(
                "{}: no anthropic-beta header in request, using defaults: '{}'",
                self.name, default_beta
            );
            req_builder = req_builder.header("anthropic-beta", default_beta);
        }

        // Add custom headers (for OpenRouter, etc.)
        for (key, value) in &self.custom_headers {
            req_builder = req_builder.header(key, value);
        }

        // Send request (pass-through, no transformation needed!)
        if tracing::enabled!(tracing::Level::TRACE) {
            if let Ok(json) = serde_json::to_string_pretty(&request) {
                tracing::trace!("{} outgoing request JSON:\n{}", self.name, json);
            }
        }
        let response = req_builder.json(&request).send().await?;

        // Check for errors
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());

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
        parse_provider_response(&response_text, &self.name)
    }

    async fn count_tokens(
        &self,
        request: CountTokensRequest,
    ) -> Result<CountTokensResponse, ProviderError> {
        // For Anthropic native, use their count_tokens endpoint
        if self.name == "anthropic" {
            let url = format!("{}/v1/messages/count_tokens", self.base_url);

            // Get authentication - use passthrough token if provided
            let override_auth = request.passthrough_auth.as_deref();
            let auth_value = self.get_auth_header(override_auth).await?;

            let mut req_builder = self
                .client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("Content-Type", "application/json");

            // Set auth header. Note: this branch only runs for self.name == "anthropic"
            // (see the guard above), so vLLM/SGLang never reach this call site today.
            if self.is_bearer_auth() {
                req_builder = req_builder
                    .header("Authorization", format!("Bearer {}", auth_value))
                    .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14");
            } else {
                req_builder = req_builder.header("x-api-key", auth_value);
            }

            let response = req_builder.json(&request).send().await?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
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
                crate::models::SystemPrompt::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            total_chars += system_text.len();
        }

        for msg in &request.messages {
            use crate::models::MessageContent;
            let content = match &msg.content {
                MessageContent::Text(text) => text.clone(),
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        crate::models::ContentBlock::Text { text, .. } => Some(text.clone()),
                        crate::models::ContentBlock::ToolResult { content, .. } => {
                            Some(content.to_string())
                        }
                        crate::models::ContentBlock::Thinking { thinking, .. } => {
                            Some(thinking.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
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
        mut request: AnthropicRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>, ProviderError>
    {
        use futures::stream::TryStreamExt;

        apply_clear_thinking_directive(&mut request);
        self.await_rate_limit_permit().await?;

        let url = format!("{}/v1/messages", self.base_url);

        // Get authentication header value
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;

        // Build request with authentication
        let mut req_builder = self
            .client
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json");

        // Set auth header based on OAuth/passthrough/header_style
        if self.is_bearer_auth() {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", auth_value));
            tracing::debug!("🔐 Using Bearer token for streaming on {}", self.name);
        } else {
            req_builder = req_builder.header("x-api-key", auth_value);
        }

        // Add anthropic-beta header if provided
        if let Some(beta_header) = &request.anthropic_beta_header {
            debug!(
                "{} [stream]: custom beta options provided, parsing and validating: '{}'",
                self.name, beta_header
            );

            // Parse the beta header (CSV format → individual options)
            match parse_anthropic_beta(beta_header) {
                Ok(parsed_options) => {
                    debug!(
                        "{} [stream]: parse_anthropic_beta succeeded, got {} options",
                        self.name,
                        parsed_options.len()
                    );

                    // Validate options if provider has a supported list
                    if !self.supported_beta_options.is_empty() {
                        debug!("{} [stream]: provider has {} supported beta options, validating against model '{}'", 
                               self.name, self.supported_beta_options.len(), request.model);

                        match validate_anthropic_beta(
                            &parsed_options,
                            &self.supported_beta_options,
                            &request.model,
                        ) {
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
                                debug!(
                                    "{} [stream]: applying default beta header: '{}'",
                                    self.name, default_beta
                                );
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
                    warn!(
                        "{} [stream]: parse_anthropic_beta FAILED: {}. Falling back to defaults",
                        self.name, parse_error
                    );
                    // Fall back to defaults on parse failure
                    let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
                    debug!(
                        "{} [stream]: applying default beta header: '{}'",
                        self.name, default_beta
                    );
                    req_builder = req_builder.header("anthropic-beta", default_beta);
                }
            }
        } else {
            let default_beta = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
            debug!(
                "{} [stream]: no anthropic-beta header in request, using defaults: '{}'",
                self.name, default_beta
            );
            req_builder = req_builder.header("anthropic-beta", default_beta);
        }

        // Add custom headers
        for (key, value) in &self.custom_headers {
            req_builder = req_builder.header(key, value);
        }

        // Send request with stream=true
        if tracing::enabled!(tracing::Level::TRACE) {
            if let Ok(json) = serde_json::to_string_pretty(&request) {
                tracing::trace!("{} outgoing stream request JSON:\n{}", self.name, json);
            }
        }
        let response = req_builder.json(&request).send().await?;

        // Check for errors
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());

            if status == 401 && self.is_oauth() {
                tracing::warn!(
                    "🔄 Received 401 on streaming, OAuth token may be invalid or expired"
                );
            }

            return Err(ProviderError::ApiError {
                status,
                message: format!("{} API error: {}", self.name, error_text),
            });
        }

        // Return the byte stream directly
        let stream = response
            .bytes_stream()
            .map_err(|e| ProviderError::HttpError(e));

        Ok(Box::pin(stream))
    }

    fn supports_model(&self, model: &str) -> bool {
        self.models.iter().any(|m| m == model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Message, MessageContent};
    use std::time::Duration;
    use tokio::time::sleep;

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
    async fn test_get_auth_header_ignores_override_for_api_key_auth() {
        let provider = make_provider();
        let result = provider
            .get_auth_header(Some("caller-token"))
            .await
            .unwrap();
        assert_eq!(result, "internal-api-key");
    }

    #[tokio::test]
    async fn test_get_auth_header_falls_back_to_api_key_when_no_override() {
        let provider = make_provider();
        let result = provider.get_auth_header(None).await.unwrap();
        assert_eq!(result, "internal-api-key");
    }

    #[tokio::test]
    async fn test_get_auth_header_uses_override_for_passthrough_auth() {
        let provider = AnthropicCompatibleProvider::new_with_auth(
            "test".to_string(),
            "internal-api-key".to_string(),
            "https://api.anthropic.com".to_string(),
            vec![],
            AuthType::Passthrough,
            None,
            None,
        );
        let result = provider
            .get_auth_header(Some("caller-token"))
            .await
            .unwrap();
        assert_eq!(result, "caller-token");
    }

    #[test]
    fn test_default_header_style_is_x_api_key() {
        let provider = make_provider();
        assert!(!provider.is_bearer_auth());
    }

    #[test]
    fn test_header_style_bearer_forces_bearer_auth_under_api_key() {
        let provider = AnthropicCompatibleProvider::new(
            "vllm-test".to_string(),
            "internal-api-key".to_string(),
            "http://localhost:8000".to_string(),
            vec![],
            None,
            None,
        )
        .with_header_style(AnthropicAuthHeaderStyle::Bearer);
        assert!(provider.is_bearer_auth());
    }

    #[test]
    fn test_other_providers_keep_x_api_key_by_default() {
        // Regression guard: providers that don't opt into Bearer header style
        // (anthropic, openrouter, z.ai, minimax, kimi-coding) must keep sending
        // x-api-key under AuthType::ApiKey. nvidia-nim now opts into Bearer
        // (see registry.rs "nvidia-nim" arm) since it migrated to Anthropic-compat.
        let provider = AnthropicCompatibleProvider::anthropic("key".to_string(), vec![]);
        assert!(!provider.is_bearer_auth());
    }

    #[test]
    fn test_oauth_provider_forces_bearer_auth_regardless_of_header_style() {
        // is_bearer_auth() ORs three conditions: Passthrough, OAuth, or explicit
        // Bearer header_style. Passthrough and header_style each have dedicated
        // tests; this covers the third — is_oauth() (oauth_provider.is_some() &&
        // token_store.is_some()) — which was previously only exercised
        // indirectly through get_auth_header tests, never against is_bearer_auth().
        let token_store = TokenStore::new(
            std::env::temp_dir().join(format!("ccm-test-oauth-tokens-{}.json", std::process::id())),
        )
        .unwrap();

        let provider = AnthropicCompatibleProvider::new(
            "copilot-test".to_string(),
            "unused-api-key".to_string(),
            "https://api.githubcopilot.com".to_string(),
            vec![],
            Some("copilot".to_string()),
            Some(token_store),
        );

        assert!(provider.is_bearer_auth());
    }

    #[tokio::test]
    async fn test_passthrough_auth_stays_bearer_regardless_of_header_style() {
        // header_style is irrelevant once auth_type is Passthrough — the caller's
        // token is always sent as Bearer, never the configured api_key.
        let provider = AnthropicCompatibleProvider::new_with_auth(
            "vllm-passthrough".to_string(),
            "internal-api-key".to_string(),
            "http://localhost:8000".to_string(),
            vec![],
            AuthType::Passthrough,
            None,
            None,
        )
        .with_header_style(AnthropicAuthHeaderStyle::Bearer);
        assert!(provider.is_bearer_auth());
        let result = provider
            .get_auth_header(Some("caller-token"))
            .await
            .unwrap();
        assert_eq!(result, "caller-token");
    }

    #[test]
    fn test_nvidia_nim_provider_with_rate_limit() {
        let provider = AnthropicCompatibleProvider::new(
            "nvidia-nim".to_string(),
            "nvidia-api-key".to_string(),
            "https://integrate.api.nvidia.com/v1".to_string(),
            vec!["meta-llama-3.1-405b-instruct".to_string()],
            None,
            None,
        )
        .with_rate_limit(Some(40));

        // Verify provider is created with rate limit
        assert_eq!(provider.rate_limit_rpm, Some(40));
        assert_eq!(
            provider.rate_limit_max_wait_ms,
            Some(DEFAULT_RATE_LIMIT_MAX_WAIT_MS)
        );
    }

    #[test]
    fn test_anthropic_compatible_provider_without_rate_limit() {
        let provider = AnthropicCompatibleProvider::new(
            "anthropic".to_string(),
            "api-key".to_string(),
            "https://api.anthropic.com".to_string(),
            vec![],
            None,
            None,
        );

        // Verify providers without rate limit have None
        assert_eq!(provider.rate_limit_rpm, None);
    }

    #[test]
    fn test_rate_limit_can_be_set_via_with_rate_limit_method() {
        let provider = AnthropicCompatibleProvider::new(
            "test".to_string(),
            "key".to_string(),
            "https://example.com".to_string(),
            vec![],
            None,
            None,
        );
        assert_eq!(provider.rate_limit_rpm, None);

        let provider_with_limit = provider.with_rate_limit(Some(50));
        assert_eq!(provider_with_limit.rate_limit_rpm, Some(50));
        assert_eq!(
            provider_with_limit.rate_limit_max_wait_ms,
            Some(DEFAULT_RATE_LIMIT_MAX_WAIT_MS)
        );
    }

    #[test]
    fn test_zero_max_wait_uses_default() {
        let provider = AnthropicCompatibleProvider::new(
            "nvidia-nim".to_string(),
            "key".to_string(),
            "https://example.com".to_string(),
            vec![],
            None,
            None,
        )
        .with_rate_limit_config(Some(10), Some(0));

        assert_eq!(
            provider.rate_limit_max_wait_ms,
            Some(DEFAULT_RATE_LIMIT_MAX_WAIT_MS)
        );
    }

    #[tokio::test]
    async fn test_rate_limiter_timeout_returns_fallback_error() {
        let provider = AnthropicCompatibleProvider::new(
            "nvidia-nim".to_string(),
            "key".to_string(),
            "https://example.com".to_string(),
            vec![],
            None,
            None,
        )
        .with_rate_limit_config(Some(1), Some(1));

        provider.await_rate_limit_permit().await.unwrap();
        let err = provider.await_rate_limit_permit().await.unwrap_err();

        match err {
            ProviderError::RateLimitTimeout {
                provider,
                rpm,
                max_wait_ms,
            } => {
                assert_eq!(provider, "nvidia-nim");
                assert_eq!(rpm, 1);
                assert_eq!(max_wait_ms, 1);
            }
            other => panic!("expected rate-limit timeout, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_count_tokens_not_throttled_by_rate_limit() {
        let provider = AnthropicCompatibleProvider::new(
            "nvidia-nim".to_string(),
            "key".to_string(),
            "https://example.com".to_string(),
            vec![],
            None,
            None,
        )
        .with_rate_limit_config(Some(1), Some(1));

        provider.await_rate_limit_permit().await.unwrap();
        sleep(Duration::from_millis(2)).await;

        let request = CountTokensRequest {
            model: "meta-llama-3.1-8b-instruct".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text("hello".to_string()),
            }],
            system: None,
            tools: None,
            passthrough_auth: None,
        };

        let response = provider.count_tokens(request).await.unwrap();
        assert!(response.input_tokens > 0);
    }

    #[tokio::test]
    async fn test_stream_rate_limiter_timeout_returns_fallback_error() {
        let provider = AnthropicCompatibleProvider::new(
            "nvidia-nim".to_string(),
            "key".to_string(),
            "https://example.com".to_string(),
            vec![],
            None,
            None,
        )
        .with_rate_limit_config(Some(1), Some(1));

        provider.await_rate_limit_permit().await.unwrap();

        let request = AnthropicRequest {
            model: "meta-llama-3.1-8b-instruct".to_string(),
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
            stream: Some(true),
            metadata: None,
            system: None,
            tools: None,
            context_management: None,
            output_config: None,
            passthrough_auth: None,
            anthropic_beta_header: None,
        };

        let result = provider.send_message_stream(request).await;
        assert!(matches!(
            result,
            Err(ProviderError::RateLimitTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn test_send_message_stream_sends_bearer_auth_header() {
        // Regression guard: send_message_stream() duplicates the header-selection
        // logic in send_message() (is_bearer_auth() branch). Confirm the streaming
        // path actually sends the Bearer header on the wire, not just that the
        // non-streaming path does.
        use axum::{http::HeaderMap, routing::post, Router};
        use std::sync::{Arc, Mutex};
        use tokio::net::TcpListener;

        let captured_auth = Arc::new(Mutex::new(None::<String>));
        let captured_auth_clone = captured_auth.clone();

        let app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap| {
                let captured = captured_auth_clone.clone();
                async move {
                    *captured.lock().unwrap() = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    "event: message_stop\ndata: {}\n\n"
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let provider = AnthropicCompatibleProvider::new(
            "vllm-stream-test".to_string(),
            "internal-api-key".to_string(),
            format!("http://{addr}"),
            vec![],
            None,
            None,
        )
        .with_header_style(AnthropicAuthHeaderStyle::Bearer);

        let request = AnthropicRequest {
            model: "qwen2.5-72b".to_string(),
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
            stream: Some(true),
            metadata: None,
            system: None,
            tools: None,
            context_management: None,
            output_config: None,
            passthrough_auth: None,
            anthropic_beta_header: None,
        };

        let stream = provider.send_message_stream(request).await.unwrap();
        // Drain the stream so the request actually completes on the mock server.
        use futures::stream::StreamExt;
        let _ = stream.collect::<Vec<_>>().await;

        assert_eq!(
            captured_auth.lock().unwrap().as_deref(),
            Some("Bearer internal-api-key")
        );
    }

    #[test]
    fn test_parse_provider_response_missing_usage_defaults_to_zero() {
        // NVIDIA NIM (and potentially other Anthropic-compatible backends) has a
        // confirmed history of omitting usage on some responses (CHANGELOG.md).
        // A missing usage field must not fail the whole parse.
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "some-model",
            "stop_reason": "end_turn",
        })
        .to_string();

        let response = parse_provider_response(&body, "nvidia-nim").unwrap();
        assert_eq!(response.usage.input_tokens, 0);
        assert_eq!(response.usage.output_tokens, 0);
    }

    #[test]
    fn test_parse_provider_response_malformed_usage_defaults_to_zero() {
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "some-model",
            "stop_reason": "end_turn",
            "usage": "not-an-object",
        })
        .to_string();

        let response = parse_provider_response(&body, "nvidia-nim").unwrap();
        assert_eq!(response.usage.input_tokens, 0);
        assert_eq!(response.usage.output_tokens, 0);
    }

    #[test]
    fn test_parse_provider_response_preserves_partial_usage_defaults_missing_field() {
        // Regression guard (adversarial review): a usage object present but missing
        // ONE field (e.g. output_tokens omitted mid-stream-to-non-stream fallback)
        // must keep the field that IS present, not zero out the whole object —
        // silently discarding a real input_tokens count would corrupt billing/quota
        // reporting for a response that was mostly valid.
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "some-model",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5},
        })
        .to_string();

        let response = parse_provider_response(&body, "nvidia-nim").unwrap();
        assert_eq!(response.usage.input_tokens, 5);
        assert_eq!(response.usage.output_tokens, 0);
    }

    #[test]
    fn test_parse_provider_response_preserves_output_tokens_when_input_missing() {
        // Mirror of the above for the other field, proving the fix isn't
        // input_tokens-specific.
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "some-model",
            "stop_reason": "end_turn",
            "usage": {"output_tokens": 7},
        })
        .to_string();

        let response = parse_provider_response(&body, "nvidia-nim").unwrap();
        assert_eq!(response.usage.input_tokens, 0);
        assert_eq!(response.usage.output_tokens, 7);
    }

    #[test]
    fn test_parse_provider_response_preserves_present_usage() {
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "some-model",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 34},
        })
        .to_string();

        let response = parse_provider_response(&body, "nvidia-nim").unwrap();
        assert_eq!(response.usage.input_tokens, 12);
        assert_eq!(response.usage.output_tokens, 34);
    }

    #[test]
    fn test_parse_provider_response_invalid_json_still_errors() {
        let result = parse_provider_response("not json", "nvidia-nim");
        assert!(result.is_err());
    }
}
