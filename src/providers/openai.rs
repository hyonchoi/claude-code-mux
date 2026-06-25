use super::{
    error::ProviderError, AnthropicProvider, AuthType, ContentBlock, ProviderResponse, Usage,
};
use crate::auth::{OAuthClient, OAuthConfig, TokenStore};
use crate::models::{AnthropicRequest, CountTokensRequest, CountTokensResponse, MessageContent};
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use bytes::Bytes;
use futures::stream::Stream;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use tokio::time::{timeout, Duration, Instant};

/// Official Codex instructions from OpenAI
/// Source: https://github.com/openai/codex (rust-v0.58.0)
const CODEX_INSTRUCTIONS: &str = include_str!("codex_instructions.md");

/// OpenAI Chat Completions request format
#[derive(Debug, Serialize)]
pub(crate) struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
}

/// OpenAI Responses API request format (for Codex models)
#[derive(Debug, Serialize)]
struct OpenAIResponsesRequest {
    model: String,
    input: OpenAIResponsesInput,
    /// System instructions for the model (required for ChatGPT Codex)
    instructions: String,
    /// Whether to store the conversation (must be false for ChatGPT backend)
    store: bool,
    /// Enable streaming responses
    stream: bool,
    // Note: ChatGPT Codex does NOT support max_output_tokens, max_tokens, temperature, top_p, stop
}

/// Input for Responses API can be string or array of messages
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenAIResponsesInput {
    Text(String),
    Messages(Vec<OpenAIResponsesMessage>),
}

/// Message format for Responses API
#[derive(Debug, Serialize)]
struct OpenAIResponsesMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

/// Content can be string or array of content parts
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum OpenAIContent {
    String(String),
    Parts(Vec<OpenAIContentPart>),
}

/// Content part (text or image_url)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum OpenAIContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OpenAIImageUrl },
}

/// Image URL object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OpenAIImageUrl {
    url: String,
}

/// Tool call in assistant message
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OpenAIToolCall {
    id: String,
    r#type: String, // "function"
    function: OpenAIFunctionCall,
}

/// Function call details
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OpenAIFunctionCall {
    name: String,
    arguments: String, // JSON string
}

/// Tool definition
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OpenAITool {
    r#type: String, // "function"
    function: OpenAIFunctionDef,
}

/// Function definition
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OpenAIFunctionDef {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OpenAIContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// OpenAI Chat Completions response format
#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIResponse {
    id: String,
    #[serde(default, rename = "object")]
    _object: String,
    model: String,
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIChoice {
    message: OpenAIMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

/// OpenAI Responses API response format (for Codex models)
#[derive(Debug, Deserialize)]
struct OpenAIResponsesResponse {
    id: String,
    model: String,
    output: Vec<ResponsesOutput>,
    usage: ResponsesUsage,
}

#[derive(Debug, Deserialize)]
struct ResponsesOutput {
    #[serde(rename = "type")]
    output_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Vec<ResponsesContentBlock>>,
}

#[derive(Debug, Deserialize)]
struct ResponsesContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesUsage {
    input_tokens: u32,
    output_tokens: u32,
}

/// OpenAI provider implementation
pub struct OpenAIProvider {
    name: String,
    api_key: String,
    auth_type: AuthType,
    base_url: String,
    client: Client,
    models: Vec<String>,
    custom_headers: Vec<(String, String)>,
    /// OAuth provider ID (if using OAuth instead of API key)
    oauth_provider: Option<String>,
    /// Token store for OAuth authentication
    token_store: Option<TokenStore>,
    /// Rate limit in requests per minute
    rate_limit_rpm: Option<u32>,
    /// Maximum wait time before allowing the next request to fail over
    rate_limit_max_wait_ms: Option<u64>,
    /// Token-bucket limiter for this provider instance
    rate_limiter: Option<Arc<DefaultDirectRateLimiter>>,
}

impl OpenAIProvider {
    pub fn new(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
    ) -> Self {
        Self::new_with_auth(
            name,
            api_key,
            base_url,
            models,
            AuthType::ApiKey,
            oauth_provider,
            token_store,
        )
    }

    pub fn new_with_auth(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        auth_type: AuthType,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
    ) -> Self {
        Self {
            name,
            api_key,
            auth_type,
            base_url,
            client: Client::new(),
            models,
            custom_headers: Vec::new(),
            oauth_provider,
            token_store,
            rate_limit_rpm: None,
            rate_limit_max_wait_ms: None,
            rate_limiter: None,
        }
    }

    /// Set the rate limit in requests per minute.
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
                .unwrap_or(2_000)
        });
        self.rate_limiter = rate_limit_rpm
            .and_then(NonZeroU32::new)
            .map(|rpm| Arc::new(RateLimiter::direct(Quota::per_minute(rpm))));
        self
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

        let max_wait_ms = self.rate_limit_max_wait_ms.unwrap_or(2_000);
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
                    tracing::debug!(
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
                tracing::warn!(
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

    /// Check if the model is a Codex model that requires /v1/responses endpoint
    fn is_codex_model(model: &str) -> bool {
        model.to_lowercase().contains("codex")
    }

    /// Parse SSE (Server-Sent Events) response from ChatGPT Codex
    fn parse_sse_response(sse_text: &str) -> Result<Vec<ContentBlock>, ProviderError> {
        // Find the response.completed event and extract both reasoning and message
        let lines: Vec<&str> = sse_text.lines().collect();

        for (i, line) in lines.iter().enumerate() {
            if line.starts_with("event: response.completed") {
                // Next line should be data: {...}
                if i + 1 < lines.len() {
                    let data_line = lines[i + 1];
                    if data_line.starts_with("data: ") {
                        let json_str = &data_line[6..]; // Skip "data: "
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {
                            // Extract both reasoning and message from response.output array
                            // Note: Codex models have reasoning at output[0], message at output[1]
                            if let Some(response) = json.get("response") {
                                if let Some(output) =
                                    response.get("output").and_then(|v| v.as_array())
                                {
                                    let mut content_blocks = Vec::new();

                                    // Extract reasoning and message in order
                                    for output_item in output {
                                        if let Some(output_type) =
                                            output_item.get("type").and_then(|v| v.as_str())
                                        {
                                            if let Some(content) = output_item
                                                .get("content")
                                                .and_then(|v| v.as_array())
                                            {
                                                if let Some(first_content) = content.first() {
                                                    if let Some(text) = first_content
                                                        .get("text")
                                                        .and_then(|v| v.as_str())
                                                    {
                                                        match output_type {
                                                            "reasoning" => {
                                                                // Convert OpenAI reasoning to Claude thinking block
                                                                content_blocks.push(
                                                                    ContentBlock::Thinking {
                                                                        thinking: text.to_string(),
                                                                        signature: String::new(), // OpenAI doesn't have signature
                                                                    },
                                                                );
                                                            }
                                                            "message" => {
                                                                content_blocks.push(
                                                                    ContentBlock::Text {
                                                                        text: text.to_string(),
                                                                    },
                                                                );
                                                            }
                                                            _ => {}
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    if !content_blocks.is_empty() {
                                        return Ok(content_blocks);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Err(ProviderError::ApiError {
            status: 500,
            message: "Failed to parse SSE response: no content found".to_string(),
        })
    }

    /// Transform Anthropic request to OpenAI Responses API format
    fn transform_to_responses_request(
        &self,
        request: &AnthropicRequest,
    ) -> Result<OpenAIResponsesRequest, ProviderError> {
        // Use official Codex instructions (system message is handled separately in user messages if needed)
        let instructions = CODEX_INSTRUCTIONS.to_string();

        // Convert messages to Responses API input format
        let mut messages = Vec::new();

        // Add system message as a user message if present (Codex doesn't have separate system role)
        if let Some(ref system) = request.system {
            let system_text = match system {
                crate::models::SystemPrompt::Text(text) => text.clone(),
                crate::models::SystemPrompt::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            // Prepend system message as user message
            messages.push(OpenAIResponsesMessage {
                role: "user".to_string(),
                content: Some(system_text),
            });
        }

        // Transform messages
        for msg in &request.messages {
            let content = match &msg.content {
                MessageContent::Text(text) => text.clone(),
                MessageContent::Blocks(blocks) => {
                    let text = blocks
                        .iter()
                        .filter_map(|block| match block {
                            crate::models::ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    // Responses API requires content, use empty string if none
                    if text.is_empty() {
                        String::new()
                    } else {
                        text
                    }
                }
            };

            messages.push(OpenAIResponsesMessage {
                role: msg.role.clone(),
                content: Some(content), // Always provide content
            });
        }

        Ok(OpenAIResponsesRequest {
            model: request.model.clone(),
            input: OpenAIResponsesInput::Messages(messages),
            instructions,
            store: false, // Required: ChatGPT backend requires store=false
            stream: true, // Required: ChatGPT Codex requires stream=true
        })
    }

    pub fn with_headers(
        name: String,
        api_key: String,
        base_url: String,
        models: Vec<String>,
        custom_headers: Vec<(String, String)>,
        oauth_provider: Option<String>,
        token_store: Option<TokenStore>,
    ) -> Self {
        Self {
            name,
            api_key,
            auth_type: AuthType::ApiKey,
            base_url,
            client: Client::new(),
            models,
            custom_headers,
            oauth_provider,
            token_store,
            rate_limit_rpm: None,
            rate_limit_max_wait_ms: None,
            rate_limiter: None,
        }
    }

    /// OpenRouter - OpenAI-compatible with optional referer headers
    pub fn openrouter(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::with_headers(
            name,
            api_key,
            "https://openrouter.ai/api/v1".to_string(),
            models,
            vec![
                (
                    "HTTP-Referer".to_string(),
                    "https://github.com/bahkchanhee/claude-code-mux".to_string(),
                ),
                ("X-Title".to_string(), "Claude Code Mux".to_string()),
            ],
            None,
            None,
        )
    }

    /// Deepinfra - Fully OpenAI-compatible
    pub fn deepinfra(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::new(
            name,
            api_key,
            "https://api.deepinfra.com/v1/openai".to_string(),
            models,
            None,
            None,
        )
    }

    /// NovitaAI - OpenAI-compatible with source header
    pub fn novita(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::with_headers(
            name,
            api_key,
            "https://api.novita.ai/v3/openai".to_string(),
            models,
            vec![("X-Novita-Source".to_string(), "claude-code-mux".to_string())],
            None,
            None,
        )
    }

    /// Baseten - OpenAI-compatible
    pub fn baseten(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::new(
            name,
            api_key,
            "https://inference.baseten.co/v1".to_string(),
            models,
            None,
            None,
        )
    }

    /// Together AI - OpenAI-compatible
    pub fn together(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::new(
            name,
            api_key,
            "https://api.together.xyz/v1".to_string(),
            models,
            None,
            None,
        )
    }

    /// Fireworks AI - OpenAI-compatible
    pub fn fireworks(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::new(
            name,
            api_key,
            "https://api.fireworks.ai/inference/v1".to_string(),
            models,
            None,
            None,
        )
    }

    /// Groq - Fast OpenAI-compatible inference
    pub fn groq(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::new(
            name,
            api_key,
            "https://api.groq.com/openai/v1".to_string(),
            models,
            None,
            None,
        )
    }

    /// Nebius - OpenAI-compatible
    pub fn nebius(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::new(
            name,
            api_key,
            "https://api.studio.nebius.ai/v1".to_string(),
            models,
            None,
            None,
        )
    }

    /// Cerebras - Fast OpenAI-compatible inference
    pub fn cerebras(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::new(
            name,
            api_key,
            "https://api.cerebras.ai/v1".to_string(),
            models,
            None,
            None,
        )
    }

    pub fn moonshot(name: String, api_key: String, models: Vec<String>) -> Self {
        Self::new(
            name,
            api_key,
            "https://api.moonshot.cn/v1".to_string(),
            models,
            None,
            None,
        )
    }

    /// Get authentication token value for Authorization Bearer header.
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
                        let config = OAuthConfig::openai_codex();
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

    /// Extract ChatGPT account ID from JWT access token
    fn extract_account_id(access_token: &str) -> Option<String> {
        // JWT format: header.payload.signature
        let parts: Vec<&str> = access_token.split('.').collect();
        if parts.len() != 3 {
            return None;
        }

        // Decode the payload (base64url)
        let payload = parts[1];
        let decoded = general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
        let json_str = String::from_utf8(decoded).ok()?;

        // Parse JSON and extract chatgpt_account_id from the correct claim path
        let json: serde_json::Value = serde_json::from_str(&json_str).ok()?;
        json.get("https://api.openai.com/auth")?
            .get("chatgpt_account_id")?
            .as_str()
            .map(|s| s.to_string())
    }

    /// Transform Anthropic request to OpenAI format
    pub(crate) fn transform_request(
        &self,
        request: &AnthropicRequest,
    ) -> Result<OpenAIRequest, ProviderError> {
        let mut openai_messages = Vec::new();

        // Add system message if present
        if let Some(ref system) = request.system {
            let system_text = match system {
                crate::models::SystemPrompt::Text(text) => text.clone(),
                crate::models::SystemPrompt::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            openai_messages.push(OpenAIMessage {
                role: "system".to_string(),
                content: Some(OpenAIContent::String(system_text)),
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Transform messages
        for msg in &request.messages {
            match &msg.content {
                MessageContent::Text(text) => {
                    // Simple text message
                    openai_messages.push(OpenAIMessage {
                        role: msg.role.clone(),
                        content: Some(OpenAIContent::String(text.clone())),
                        reasoning: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                MessageContent::Blocks(blocks) => {
                    // Check if we have any tool results - they need separate messages
                    let tool_results: Vec<_> = blocks
                        .iter()
                        .filter_map(|block| {
                            if let crate::models::ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                            } = block
                            {
                                Some((tool_use_id.clone(), content.to_string()))
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Extract tool_calls from ToolUse blocks
                    let tool_calls: Vec<_> = blocks
                        .iter()
                        .filter_map(|block| {
                            if let crate::models::ContentBlock::ToolUse { id, name, input } = block
                            {
                                Some(OpenAIToolCall {
                                    id: id.clone(),
                                    r#type: "function".to_string(),
                                    function: OpenAIFunctionCall {
                                        name: name.clone(),
                                        arguments: serde_json::to_string(input).unwrap_or_default(),
                                    },
                                })
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Build content parts (text and images, excluding tool use/result)
                    let mut content_parts = Vec::new();
                    for block in blocks {
                        match block {
                            crate::models::ContentBlock::Text { text } => {
                                content_parts.push(OpenAIContentPart::Text { text: text.clone() });
                            }
                            crate::models::ContentBlock::Image { source } => {
                                // Convert Anthropic image format to OpenAI format
                                let url = if source.r#type == "base64" {
                                    // data:image/{media_type};base64,{data}
                                    let media_type = source
                                        .media_type
                                        .as_ref()
                                        .map(|s| s.as_str())
                                        .unwrap_or("image/png");
                                    let data =
                                        source.data.as_ref().map(|s| s.as_str()).unwrap_or("");
                                    format!("data:{};base64,{}", media_type, data)
                                } else if let Some(url) = &source.url {
                                    url.clone()
                                } else {
                                    continue; // Skip invalid image sources
                                };

                                content_parts.push(OpenAIContentPart::ImageUrl {
                                    image_url: OpenAIImageUrl { url },
                                });
                            }
                            crate::models::ContentBlock::ToolUse { .. } => {
                                // Already handled in tool_calls
                            }
                            crate::models::ContentBlock::ToolResult { .. } => {
                                // Will be handled as separate messages below
                            }
                            crate::models::ContentBlock::Thinking { .. } => {
                                // OpenAI doesn't have thinking blocks, skip
                            }
                        }
                    }

                    // Add main message with content and/or tool_calls
                    if !content_parts.is_empty() || !tool_calls.is_empty() {
                        let content = if content_parts.is_empty() {
                            None
                        } else if content_parts.len() == 1 {
                            // Single text part - use string format for compatibility
                            if let OpenAIContentPart::Text { text } = &content_parts[0] {
                                Some(OpenAIContent::String(text.clone()))
                            } else {
                                Some(OpenAIContent::Parts(content_parts.clone()))
                            }
                        } else {
                            Some(OpenAIContent::Parts(content_parts))
                        };

                        openai_messages.push(OpenAIMessage {
                            role: msg.role.clone(),
                            content,
                            reasoning: None,
                            tool_calls: if tool_calls.is_empty() {
                                None
                            } else {
                                Some(tool_calls)
                            },
                            tool_call_id: None,
                        });
                    }

                    // Add separate tool result messages
                    for (tool_use_id, result_content) in tool_results {
                        openai_messages.push(OpenAIMessage {
                            role: "tool".to_string(),
                            content: Some(OpenAIContent::String(result_content)),
                            reasoning: None,
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id),
                        });
                    }
                }
            }
        }

        // Transform tools if present
        let tools = request.tools.as_ref().map(|anthropic_tools| {
            anthropic_tools
                .iter()
                .filter_map(|tool| {
                    // Anthropic tools have name, description, input_schema
                    Some(OpenAITool {
                        r#type: "function".to_string(),
                        function: OpenAIFunctionDef {
                            name: tool.name.as_ref()?.clone(),
                            description: tool.description.clone(),
                            parameters: tool.input_schema.clone(),
                        },
                    })
                })
                .collect()
        });

        Ok(OpenAIRequest {
            model: request.model.clone(),
            messages: openai_messages,
            max_tokens: Some(request.max_tokens),
            temperature: request.temperature,
            top_p: request.top_p,
            stop: request.stop_sequences.clone(),
            stream: request.stream,
            tools,
            tool_choice: None, // TODO: Add tool_choice support if needed
        })
    }

    /// Transform OpenAI response to Anthropic format
    pub(crate) fn transform_response(&self, response: OpenAIResponse) -> ProviderResponse {
        let choice = response
            .choices
            .into_iter()
            .next()
            .expect("OpenAI response must have at least one choice");

        // Extract text from content or reasoning (for GLM models via Cerebras)
        let text = if let Some(content) = choice.message.content {
            match content {
                OpenAIContent::String(s) => s,
                OpenAIContent::Parts(parts) => {
                    // Extract text from all text parts
                    parts
                        .iter()
                        .filter_map(|part| {
                            if let OpenAIContentPart::Text { text } = part {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
        } else if let Some(reasoning) = choice.message.reasoning {
            reasoning
        } else {
            String::new()
        };

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }

        if let Some(tool_calls) = choice.message.tool_calls {
            for tool_call in tool_calls {
                if tool_call.r#type != "function" {
                    continue;
                }

                let input =
                    serde_json::from_str(&tool_call.function.arguments).unwrap_or_else(|_| {
                        serde_json::Value::String(tool_call.function.arguments.clone())
                    });

                content.push(ContentBlock::ToolUse {
                    id: tool_call.id,
                    name: tool_call.function.name,
                    input,
                });
            }
        }

        if content.is_empty() {
            content.push(ContentBlock::Text {
                text: String::new(),
            });
        }

        let stop_reason = choice
            .finish_reason
            .as_ref()
            .map(|reason| match reason.as_str() {
                "stop" => "end_turn".to_string(),
                "length" => "max_tokens".to_string(),
                "tool_calls" => "tool_use".to_string(),
                "content_filter" => "stop_sequence".to_string(),
                _ => reason.clone(),
            });

        ProviderResponse {
            id: response.id,
            r#type: "message".to_string(),
            role: "assistant".to_string(),
            content,
            model: response.model,
            stop_reason,
            stop_sequence: None,
            usage: Usage {
                input_tokens: response.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
                output_tokens: response.usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
            },
        }
    }

    /// Transform Responses API response to Anthropic format
    fn transform_responses_response(&self, response: OpenAIResponsesResponse) -> ProviderResponse {
        // Extract text from output messages
        let text = response
            .output
            .iter()
            .filter(|output| output.output_type == "message")
            .filter_map(|output| output.content.as_ref())
            .flat_map(|content_blocks| {
                content_blocks
                    .iter()
                    .filter(|block| block.block_type == "output_text")
                    .filter_map(|block| block.text.clone())
            })
            .collect::<Vec<_>>()
            .join("\n");

        ProviderResponse {
            id: response.id,
            r#type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![ContentBlock::Text { text }],
            model: response.model,
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: Usage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
            },
        }
    }
}

#[async_trait]
impl AnthropicProvider for OpenAIProvider {
    async fn send_message(
        &self,
        request: AnthropicRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.await_rate_limit_permit().await?;

        // Get authentication token (API key or OAuth)
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;

        // Determine base URL: OAuth uses ChatGPT backend, API key uses configured base_url
        let base_url = if self.is_oauth() {
            "https://chatgpt.com/backend-api"
        } else {
            &self.base_url
        };

        // Check if we should use Responses API endpoint:
        // - OAuth: Always use /codex/responses for all models
        // - API Key: Only use /responses for models containing "codex"
        let use_responses_api = if self.is_oauth() {
            true // OAuth always uses Codex endpoint
        } else {
            Self::is_codex_model(&request.model) // API Key only for codex models
        };

        if use_responses_api {
            // Use /v1/responses endpoint for Codex models
            let responses_request = self.transform_to_responses_request(&request)?;

            // OAuth (ChatGPT Codex) uses /codex/responses, API Key uses /responses
            let endpoint = if self.is_oauth() {
                "/codex/responses"
            } else {
                "/responses"
            };
            let url = format!("{}{}", base_url, endpoint);

            tracing::debug!(
                "Using {} endpoint for Codex model: {}",
                endpoint,
                request.model
            );

            let mut req_builder = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", auth_value))
                .header("Content-Type", "application/json")
                .header("accept", "text/event-stream");

            // For OAuth (ChatGPT Codex), add Codex-specific headers
            if self.is_oauth() {
                if let Some(account_id) = Self::extract_account_id(&auth_value) {
                    req_builder = req_builder
                        .header("chatgpt-account-id", account_id)
                        .header("OpenAI-Beta", "responses=experimental")
                        .header("originator", "codex_cli_rs")
                        // Browser-like headers to avoid Cloudflare bot detection
                        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                        .header("Origin", "https://chatgpt.com")
                        .header("Referer", "https://chatgpt.com/")
                        .header("sec-ch-ua", "\"Google Chrome\";v=\"131\", \"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\"")
                        .header("sec-ch-ua-mobile", "?0")
                        .header("sec-ch-ua-platform", "\"macOS\"")
                        .header("sec-fetch-dest", "empty")
                        .header("sec-fetch-mode", "cors")
                        .header("sec-fetch-site", "same-origin");
                    tracing::debug!(
                        "🔐 Using OAuth Bearer token for ChatGPT Codex on {}",
                        self.name
                    );
                }
            }

            // Add custom headers
            for (key, value) in &self.custom_headers {
                req_builder = req_builder.header(key, value);
            }

            tracing::trace!(
                provider = %self.name,
                url = %url,
                method = "POST",
                headers_authorization = "Bearer ***",
                headers_content_type = "application/json",
                headers_accept = "text/event-stream",
                headers_custom_keys = ?self.custom_headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
                "OpenAI request headers (Responses API)"
            );

            let response = req_builder.json(&responses_request).send().await?;

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                tracing::error!("Responses API error ({}): {}", status, error_text);
                return Err(ProviderError::ApiError {
                    status,
                    message: error_text,
                });
            }

            let response_text = response.text().await?;
            tracing::debug!("Responses API response body: {}", response_text);

            // Parse SSE (Server-Sent Events) format
            // Format: event: xxx\ndata: {...}\n\n
            // This extracts both reasoning (converted to thinking) and message blocks
            let content_blocks = Self::parse_sse_response(&response_text)?;

            // Return direct response (SSE doesn't need transform)
            Ok(ProviderResponse {
                id: "sse-response".to_string(),
                r#type: "message".to_string(),
                role: "assistant".to_string(),
                content: content_blocks,
                model: request.model.clone(),
                stop_reason: Some("end_turn".to_string()),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 0, // SSE doesn't provide token counts
                    output_tokens: 0,
                },
            })
        } else {
            // Use standard /v1/chat/completions endpoint for non-Codex models
            let openai_request = self.transform_request(&request)?;
            let url = format!("{}/chat/completions", base_url);

            let mut req_builder = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", auth_value))
                .header("Content-Type", "application/json");

            // For OAuth (ChatGPT), add account-specific headers
            if self.is_oauth() {
                if let Some(account_id) = Self::extract_account_id(&auth_value) {
                    req_builder = req_builder
                        .header("chatgpt-account-id", account_id)
                        // Browser-like headers to avoid Cloudflare bot detection
                        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                        .header("Origin", "https://chatgpt.com")
                        .header("Referer", "https://chatgpt.com/")
                        .header("sec-ch-ua", "\"Google Chrome\";v=\"131\", \"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\"")
                        .header("sec-ch-ua-mobile", "?0")
                        .header("sec-ch-ua-platform", "\"macOS\"")
                        .header("sec-fetch-dest", "empty")
                        .header("sec-fetch-mode", "cors")
                        .header("sec-fetch-site", "same-origin");
                    tracing::debug!("🔐 Using OAuth Bearer token for ChatGPT on {}", self.name);
                }
            }

            // Add custom headers (for OpenRouter, NovitaAI, etc.)
            for (key, value) in &self.custom_headers {
                req_builder = req_builder.header(key, value);
            }

            tracing::trace!(
                provider = %self.name,
                url = %url,
                method = "POST",
                headers_authorization = "Bearer ***",
                headers_content_type = "application/json",
                headers_custom_keys = ?self.custom_headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
                "OpenAI request headers (Chat Completions)"
            );

            let response = req_builder.json(&openai_request).send().await?;

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

            // Get response body as text for debugging
            let response_text = response.text().await?;
            tracing::debug!("OpenAI provider response body: {}", response_text);

            // Try to parse the response
            let openai_response: OpenAIResponse =
                serde_json::from_str(&response_text).map_err(|e| {
                    tracing::error!("Failed to parse OpenAI response: {}", e);
                    tracing::error!("Response body was: {}", response_text);
                    e
                })?;

            Ok(self.transform_response(openai_response))
        }
    }

    async fn count_tokens(
        &self,
        request: CountTokensRequest,
    ) -> Result<CountTokensResponse, ProviderError> {
        // For OpenAI, we'll use tiktoken-rs for local token counting
        // This is a placeholder - actual implementation would use tiktoken

        // Rough estimate: ~4 chars per token
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
            let content = match &msg.content {
                MessageContent::Text(text) => text.clone(),
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        crate::models::ContentBlock::Text { text } => Some(text.clone()),
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
        request: AnthropicRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>, ProviderError>
    {
        use futures::stream::TryStreamExt;

        self.await_rate_limit_permit().await?;

        // Get authentication token (API key or OAuth)
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;

        // Determine base URL: OAuth uses ChatGPT backend, API key uses configured base_url
        let base_url = if self.is_oauth() {
            "https://chatgpt.com/backend-api"
        } else {
            &self.base_url
        };

        // Check if this is a Codex model
        let is_codex = Self::is_codex_model(&request.model);

        let (url, request_body) = if is_codex {
            // Use /v1/responses endpoint for Codex models
            tracing::debug!(
                "Using /v1/responses endpoint for Codex model (streaming): {}",
                request.model
            );
            let responses_request = self.transform_to_responses_request(&request)?;
            let body = serde_json::to_value(&responses_request)
                .map_err(|e| ProviderError::SerializationError(e))?;
            (format!("{}/responses", base_url), body)
        } else {
            // Use standard /v1/chat/completions endpoint
            let openai_request = self.transform_request(&request)?;
            let body = serde_json::to_value(&openai_request)
                .map_err(|e| ProviderError::SerializationError(e))?;
            (format!("{}/chat/completions", base_url), body)
        };

        // Send streaming request
        let mut req_builder = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", auth_value))
            .header("Content-Type", "application/json")
            .header("accept", "text/event-stream");

        // For OAuth (ChatGPT Codex), add Codex-specific headers
        if self.is_oauth() && is_codex {
            if let Some(account_id) = Self::extract_account_id(&auth_value) {
                req_builder = req_builder
                    .header("chatgpt-account-id", account_id)
                    .header("OpenAI-Beta", "responses=experimental")
                    .header("originator", "codex_cli_rs");
                tracing::debug!(
                    "🔐 Using OAuth Bearer token for ChatGPT Codex streaming on {}",
                    self.name
                );
            }
        } else if self.is_oauth() {
            // For non-Codex OAuth (if needed in the future)
            if let Some(account_id) = Self::extract_account_id(&auth_value) {
                req_builder = req_builder.header("chatgpt-account-id", account_id);
                tracing::debug!("🔐 Using OAuth Bearer token for streaming on {}", self.name);
            }
        }

        tracing::trace!(
            provider = %self.name,
            url = %url,
            method = "POST",
            is_codex = is_codex,
            is_oauth = self.is_oauth(),
            headers_authorization = "Bearer ***",
            headers_content_type = "application/json",
            headers_accept = "text/event-stream",
            headers_custom_keys = ?self.custom_headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            "OpenAI request headers (streaming)"
        );

        let response = req_builder.json(&request_body).send().await?;

        // Check for errors
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

        // TODO: Transform OpenAI SSE format to Anthropic SSE format
        // For now, just pass through the stream
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

    fn make_provider() -> OpenAIProvider {
        OpenAIProvider::new(
            "test".to_string(),
            "internal-api-key".to_string(),
            "https://api.openai.com/v1".to_string(),
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
        let provider = OpenAIProvider::new_with_auth(
            "test".to_string(),
            "internal-api-key".to_string(),
            "https://api.openai.com/v1".to_string(),
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
    fn test_with_rate_limit_config_sets_fields() {
        let provider = make_provider().with_rate_limit_config(Some(40), Some(1500));
        assert_eq!(provider.rate_limit_rpm, Some(40));
        assert_eq!(provider.rate_limit_max_wait_ms, Some(1500));
        assert!(provider.rate_limiter.is_some());
    }

    #[test]
    fn test_transform_response_preserves_tool_calls_as_tool_use() {
        let provider = make_provider();
        let response: OpenAIResponse = serde_json::from_str(
            r#"{
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "model": "z-ai/glm-5.1",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "running tool",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "Bash",
                                "arguments": "{\"command\":\"echo hi\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            }"#,
        )
        .unwrap();

        let transformed = provider.transform_response(response);
        assert_eq!(transformed.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(transformed.content.len(), 2);

        match &transformed.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "Bash");
                assert_eq!(
                    input.get("command").and_then(|v| v.as_str()),
                    Some("echo hi")
                );
            }
            other => panic!("Expected ToolUse block, got {:?}", other),
        }
    }

    #[test]
    fn test_transform_response_maps_stop_reason_stop_to_end_turn() {
        let provider = make_provider();
        let response: OpenAIResponse = serde_json::from_str(
            r#"{
                "id": "chatcmpl-124",
                "object": "chat.completion",
                "model": "z-ai/glm-5.1",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "done"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            }"#,
        )
        .unwrap();

        let transformed = provider.transform_response(response);
        assert_eq!(transformed.stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn test_rate_limit_can_be_disabled() {
        let provider = make_provider().with_rate_limit(None);
        assert_eq!(provider.rate_limit_rpm, None);
        assert_eq!(provider.rate_limit_max_wait_ms, None);
        assert!(provider.rate_limiter.is_none());
        provider.await_rate_limit_permit().await.unwrap();
    }
}
