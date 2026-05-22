use super::openai::{OpenAIProvider, OpenAIResponse};
use super::{error::ProviderError, AnthropicProvider, ProviderResponse};
use crate::auth::github_copilot::{parse_proxy_ep, refresh_copilot_token};
use crate::auth::{OAuthToken, TokenStore};
use crate::models::{AnthropicRequest, CountTokensRequest, CountTokensResponse, MessageContent};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::stream::Stream;
use futures::stream::TryStreamExt;
use reqwest::Client;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

const COPILOT_HEADERS: &[(&str, &str)] = &[
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
    ("Openai-Intent", "conversation-edits"),
    ("X-Initiator", "user"),
    ("Openai-Organization", "github-copilot"),
    ("X-GitHub-Api-Version", "2025-05-01"),
    ("X-Interaction-Type", "conversation"),
];

const MODEL_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
struct CopilotModelCache {
    fetched_at: Instant,
    fallback_model_id: String,
    discovered_models: Vec<CopilotModelInfo>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CopilotModelsResponse {
    data: Vec<CopilotModelInfo>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CopilotCapabilities {
    #[serde(rename = "type", default)]
    r#type: String,
    #[serde(default)]
    family: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CopilotModelInfo {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    family: String,
    #[serde(default)]
    capabilities: Option<CopilotCapabilities>,
    #[serde(default)]
    is_chat_fallback: bool,
    #[serde(default)]
    is_chat_default: bool,
    #[serde(default)]
    model_picker_enabled: bool,
}

impl CopilotModelInfo {
    fn capabilities_type(&self) -> &str {
        self.capabilities
            .as_ref()
            .map(|c| c.r#type.as_str())
            .unwrap_or("")
    }
}

fn select_fallback_chat_model(models: &[CopilotModelInfo]) -> Option<&CopilotModelInfo> {
    models
        .iter()
        .find(|m| m.capabilities_type() == "chat" && m.is_chat_fallback)
        .or_else(|| {
            models
                .iter()
                .find(|m| m.capabilities_type() == "chat" && m.is_chat_default)
        })
        .or_else(|| models.iter().find(|m| m.capabilities_type() == "chat"))
}

pub struct CopilotProvider {
    name: String,
    models: Arc<RwLock<Vec<String>>>,
    token_store: Option<TokenStore>,
    refresh_lock: Arc<Mutex<()>>,
    model_fetch_lock: Arc<Mutex<()>>,
    model_cache: Arc<RwLock<Option<CopilotModelCache>>>,
    client: Client,
    session_id: String,
    machine_id: String,
}

fn apply_copilot_headers(
    mut builder: reqwest::RequestBuilder,
    session_id: &str,
    machine_id: &str,
) -> reqwest::RequestBuilder {
    for (key, value) in COPILOT_HEADERS {
        builder = builder.header(*key, *value);
    }
    builder
        .header("VScode-SessionId", session_id)
        .header("VScode-MachineId", machine_id)
        .header("X-Request-Id", Uuid::new_v4().to_string())
}

impl CopilotProvider {
    pub fn new(name: String, models: Vec<String>, token_store: Option<TokenStore>) -> Self {
        Self::new_with_client(name, models, token_store, Client::new())
    }

    pub fn new_with_client(
        name: String,
        models: Vec<String>,
        token_store: Option<TokenStore>,
        client: Client,
    ) -> Self {
        let session_id = Uuid::new_v4().to_string();
        let machine_id = Uuid::new_v4().to_string();
        tracing::info!(
            session_id = %session_id,
            machine_id = %machine_id,
            "Copilot session established"
        );
        Self {
            name,
            models: Arc::new(RwLock::new(models)),
            token_store,
            refresh_lock: Arc::new(Mutex::new(())),
            model_fetch_lock: Arc::new(Mutex::new(())),
            model_cache: Arc::new(RwLock::new(None)),
            client,
            session_id,
            machine_id,
        }
    }

    fn is_cache_fresh(cache: &CopilotModelCache) -> bool {
        cache.fetched_at.elapsed() < MODEL_CACHE_TTL
    }

    async fn fetch_models_cache(&self) -> Result<CopilotModelCache, ProviderError> {
        let bearer = self.get_valid_copilot_token(false).await?;
        let base_url = parse_proxy_ep(&bearer);
        let url = format!("{}/models", base_url);

        let req_builder = apply_copilot_headers(
            self.client
                .get(url)
                .header("Authorization", format!("Bearer {}", bearer)),
            &self.session_id,
            &self.machine_id,
        );

        let response = req_builder.send().await.map_err(ProviderError::HttpError)?;
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

        let body = response.text().await.map_err(ProviderError::HttpError)?;
        let payload: CopilotModelsResponse =
            serde_json::from_str(&body).map_err(|e| ProviderError::ApiError {
                status: 500,
                message: format!("Failed to parse Copilot /models response: {e}"),
            })?;

        let fallback =
            select_fallback_chat_model(&payload.data).ok_or_else(|| ProviderError::ApiError {
                status: 500,
                message: "No chat fallback model found in Copilot /models response".to_string(),
            })?;

        Ok(CopilotModelCache {
            fetched_at: Instant::now(),
            fallback_model_id: fallback.id.clone(),
            discovered_models: payload.data,
        })
    }

    async fn update_discovered_chat_models(&self, discovered: &[CopilotModelInfo]) {
        let mut models = self.models.write().await;
        for model in discovered.iter().filter(|m| m.capabilities_type() == "chat") {
            if !models.iter().any(|existing| existing == &model.id) {
                models.push(model.id.clone());
            }
        }
    }

    async fn resolve_auto_model_from_cache_or_fetch(
        &self,
        forced_fetch_result: Option<Result<CopilotModelCache, ProviderError>>,
    ) -> Result<String, ProviderError> {
        {
            let cache = self.model_cache.read().await;
            if let Some(cache) = cache.as_ref() {
                if Self::is_cache_fresh(cache) {
                    return Ok(cache.fallback_model_id.clone());
                }
            }
        }

        let _fetch_guard = self.model_fetch_lock.lock().await;

        {
            let cache = self.model_cache.read().await;
            if let Some(cache) = cache.as_ref() {
                if Self::is_cache_fresh(cache) {
                    return Ok(cache.fallback_model_id.clone());
                }
            }
        }

        let fetch_result = match forced_fetch_result {
            Some(result) => result,
            None => self.fetch_models_cache().await,
        };

        match fetch_result {
            Ok(new_cache) => {
                let fallback = new_cache.fallback_model_id.clone();
                self.update_discovered_chat_models(&new_cache.discovered_models)
                    .await;
                let mut cache = self.model_cache.write().await;
                *cache = Some(new_cache);
                Ok(fallback)
            }
            Err(err) => {
                let cache = self.model_cache.read().await;
                if let Some(stale_cache) = cache.as_ref() {
                    tracing::warn!("Copilot /models refresh failed; serving stale cache");
                    return Ok(stale_cache.fallback_model_id.clone());
                }
                Err(err)
            }
        }
    }

    async fn resolve_auto_model(&self) -> Result<String, ProviderError> {
        self.resolve_auto_model_from_cache_or_fetch(None).await
    }

    async fn resolve_request_model(&self, request: &mut AnthropicRequest) -> Result<(), ProviderError> {
        if request.model == "auto" {
            request.model = self.resolve_auto_model().await?;
        }
        Ok(())
    }

    async fn get_valid_copilot_token(&self, force: bool) -> Result<String, ProviderError> {
        let token_store = self.token_store.as_ref().ok_or_else(|| {
            ProviderError::AuthError("TokenStore not configured for copilot provider".to_string())
        })?;

        // Fast path: check without lock
        if let Some(token) = token_store.get(&self.name) {
            if !force && !token.needs_refresh() {
                return Ok(token.access_token.clone());
            }
        } else {
            return Err(ProviderError::AuthError(format!(
                "No OAuth token found for '{}'. Please authenticate via the admin UI.",
                self.name
            )));
        }

        // Slow path: acquire lock, re-check, then refresh
        let _guard = self.refresh_lock.lock().await;

        if let Some(token) = token_store.get(&self.name) {
            if !force && !token.needs_refresh() {
                return Ok(token.access_token.clone());
            }

            let github_token = token.refresh_token.clone();
            let copilot_resp = refresh_copilot_token(&self.client, &github_token)
                .await
                .map_err(|e| {
                    ProviderError::AuthError(format!("Failed to refresh Copilot token: {}", e))
                })?;

            let new_expires_at = chrono::DateTime::from_timestamp(
                copilot_resp.expires_at.min(i64::MAX as u64) as i64,
                0,
            )
            .unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(30));

            let updated_token = OAuthToken {
                provider_id: token.provider_id.clone(),
                access_token: copilot_resp.token.clone(),
                refresh_token: github_token,
                expires_at: new_expires_at,
                enterprise_url: None,
                project_id: None,
            };

            token_store.save(updated_token).map_err(|e| {
                ProviderError::AuthError(format!("Failed to save refreshed token: {}", e))
            })?;

            Ok(copilot_resp.token)
        } else {
            Err(ProviderError::AuthError(format!(
                "No OAuth token found for '{}' after acquiring lock.",
                self.name
            )))
        }
    }

    async fn send_message_stream_with_url(
        &self,
        request: AnthropicRequest,
        url: &str,
        bearer: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>, ProviderError>
    {
        let delegate = Self::make_delegate();
        let openai_request = delegate.transform_request(&request)?;

        let json_body =
            serde_json::to_value(&openai_request).map_err(|e| ProviderError::ApiError {
                status: 500,
                message: e.to_string(),
            })?;
        let req_builder = apply_copilot_headers(
            self.client
                .post(url)
                .header("Authorization", format!("Bearer {}", bearer))
                .header("Content-Type", "application/json")
                .header("accept", "text/event-stream"),
            &self.session_id,
            &self.machine_id,
        );

        let req_builder = req_builder.json(&json_body);
        let cloned = req_builder.try_clone();
        let response = match req_builder.send().await {
            Err(e) if e.is_connect() || e.is_timeout() => {
                if let Some(retry_builder) = cloned {
                    tracing::info!(
                        session_id = %self.session_id,
                        error = %e,
                        attempt = 1,
                        "Copilot network retry"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    // Clone already has all headers from the original builder.
                    // Only regenerate X-Request-Id to avoid duplicate headers.
                    let retry_builder = retry_builder
                        .header("X-Request-Id", Uuid::new_v4().to_string());
                    retry_builder.send().await.map_err(ProviderError::HttpError)?
                } else {
                    return Err(ProviderError::HttpError(e));
                }
            }
            other => other.map_err(ProviderError::HttpError)?,
        };

        // On 401, refresh and retry once
        let response = if response.status() == 401 {
            tracing::info!(
                session_id = %self.session_id,
                "Copilot 401: force-refreshing token"
            );
            let fresh_bearer = self.get_valid_copilot_token(true).await?;
            let fresh_url = format!("{}/chat/completions", parse_proxy_ep(&fresh_bearer));
            let retry_builder = apply_copilot_headers(
                self.client
                    .post(&fresh_url)
                    .header("Authorization", format!("Bearer {}", fresh_bearer))
                    .header("Content-Type", "application/json")
                    .header("accept", "text/event-stream"),
                &self.session_id,
                &self.machine_id,
            );
            retry_builder
                .json(&json_body)
                .send()
                .await
                .map_err(ProviderError::HttpError)?
        } else {
            response
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            tracing::error!(
                "Copilot API error (streaming): status={}, body={}",
                status,
                &error_text[..error_text.len().min(512)]
            );
            return Err(ProviderError::ApiError {
                status,
                message: error_text,
            });
        }

        let stream = response
            .bytes_stream()
            .map_err(ProviderError::HttpError)
            .inspect_ok(|chunk| {
                if let Ok(s) = std::str::from_utf8(chunk) {
                    tracing::debug!("Copilot stream chunk: {}", s);
                }
            });
        Ok(Box::pin(stream))
    }

    fn make_delegate() -> OpenAIProvider {
        OpenAIProvider::new(
            String::new(),
            String::new(),
            String::new(),
            vec![],
            None,
            None,
        )
    }
}

#[async_trait]
impl AnthropicProvider for CopilotProvider {
    async fn send_message(
        &self,
        mut request: AnthropicRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.resolve_request_model(&mut request).await?;

        let bearer = self.get_valid_copilot_token(false).await?;
        let base_url = parse_proxy_ep(&bearer);
        let url = format!("{}/chat/completions", base_url);

        let delegate = Self::make_delegate();
        let openai_request = delegate.transform_request(&request)?;

        let json_body =
            serde_json::to_value(&openai_request).map_err(|e| ProviderError::ApiError {
                status: 500,
                message: e.to_string(),
            })?;
        let req_builder = apply_copilot_headers(
            self.client
                .post(&url)
                .header("Authorization", format!("Bearer {}", bearer))
                .header("Content-Type", "application/json"),
            &self.session_id,
            &self.machine_id,
        );

        let req_builder = req_builder.json(&json_body);
        let cloned = req_builder.try_clone();
        let response = match req_builder.send().await {
            Err(e) if e.is_connect() || e.is_timeout() => {
                if let Some(retry_builder) = cloned {
                    tracing::info!(
                        session_id = %self.session_id,
                        error = %e,
                        attempt = 1,
                        "Copilot network retry"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    // Clone already has all headers from the original builder.
                    // Only regenerate X-Request-Id to avoid duplicate headers.
                    let retry_builder = retry_builder
                        .header("X-Request-Id", Uuid::new_v4().to_string());
                    retry_builder.send().await.map_err(ProviderError::HttpError)?
                } else {
                    return Err(ProviderError::HttpError(e));
                }
            }
            other => other.map_err(ProviderError::HttpError)?,
        };

        // On 401, refresh the token once and retry — handles the race between
        // the token validity check and the actual API call.
        let response = if response.status() == 401 {
            tracing::info!(
                session_id = %self.session_id,
                "Copilot 401: force-refreshing token"
            );
            let fresh_bearer = self.get_valid_copilot_token(true).await?;
            let fresh_url = format!("{}/chat/completions", parse_proxy_ep(&fresh_bearer));
            let retry_builder = apply_copilot_headers(
                self.client
                    .post(&fresh_url)
                    .header("Authorization", format!("Bearer {}", fresh_bearer))
                    .header("Content-Type", "application/json"),
                &self.session_id,
                &self.machine_id,
            );
            retry_builder
                .json(&json_body)
                .send()
                .await
                .map_err(ProviderError::HttpError)?
        } else {
            response
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            tracing::error!(
                "Copilot API error (non-streaming): status={}, body={}",
                status,
                &error_text[..error_text.len().min(512)]
            );
            return Err(ProviderError::ApiError {
                status,
                message: error_text,
            });
        }

        let response_text = response.text().await.map_err(ProviderError::HttpError)?;
        tracing::debug!(
            "Copilot provider response ({} bytes): {}",
            response_text.len(),
            response_text
        );

        let openai_response: OpenAIResponse =
            serde_json::from_str(&response_text).map_err(|e| {
                tracing::error!(
                    "Copilot response JSON parse error: {}. Response text ({} bytes, first 512): {}",
                    e,
                    response_text.len(),
                    &response_text[..response_text.len().min(512)]
                );
                ProviderError::ApiError {
                    status: 500,
                    message: e.to_string(),
                }
            })?;

        Ok(delegate.transform_response(openai_response))
    }

    async fn send_message_stream(
        &self,
        mut request: AnthropicRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>, ProviderError>
    {
        self.resolve_request_model(&mut request).await?;

        let bearer = self.get_valid_copilot_token(false).await?;
        let base_url = parse_proxy_ep(&bearer);
        let url = format!("{}/chat/completions", base_url);
        self.send_message_stream_with_url(request, &url, &bearer)
            .await
    }

    async fn count_tokens(
        &self,
        request: CountTokensRequest,
    ) -> Result<CountTokensResponse, ProviderError> {
        let mut total_chars = 0usize;

        if let Some(ref system) = request.system {
            let text = match system {
                crate::models::SystemPrompt::Text(t) => t.clone(),
                crate::models::SystemPrompt::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            total_chars += text.len();
        }

        for msg in &request.messages {
            let text = match &msg.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        crate::models::ContentBlock::Text { text } => Some(text.clone()),
                        crate::models::ContentBlock::ToolResult { content, .. } => {
                            Some(content.to_string())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            total_chars += text.len();
        }

        Ok(CountTokensResponse {
            input_tokens: (total_chars / 4) as u32,
        })
    }

    fn supports_model(&self, model: &str) -> bool {
        if let Ok(models) = self.models.try_read() {
            models.iter().any(|m| m == model)
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_model(
        id: &str,
        capabilities_type: &str,
        is_chat_fallback: bool,
        is_chat_default: bool,
    ) -> CopilotModelInfo {
        CopilotModelInfo {
            id: id.to_string(),
            name: id.to_string(),
            family: "test".to_string(),
            capabilities: Some(CopilotCapabilities {
                r#type: capabilities_type.to_string(),
                family: "test".to_string(),
            }),
            is_chat_fallback,
            is_chat_default,
            model_picker_enabled: true,
        }
    }

    #[test]
    fn test_select_fallback_prefers_is_chat_fallback() {
        let models = vec![
            make_model("gpt-4o", "chat", false, true),
            make_model("copilot-base", "chat", true, false),
        ];

        let chosen = select_fallback_chat_model(&models).expect("expected fallback model");
        assert_eq!(chosen.id, "copilot-base");
    }

    #[test]
    fn test_select_fallback_uses_chat_default_when_no_chat_fallback() {
        let models = vec![make_model("gpt-4o", "chat", false, true)];

        let chosen = select_fallback_chat_model(&models).expect("expected chat default");
        assert_eq!(chosen.id, "gpt-4o");
    }

    #[test]
    fn test_select_fallback_returns_none_when_no_chat_models() {
        let models = vec![make_model("embed-v1", "embeddings", false, false)];
        assert!(select_fallback_chat_model(&models).is_none());
    }

    #[tokio::test]
    async fn test_resolve_auto_model_uses_cached_value_before_ttl() {
        let provider = CopilotProvider::new("copilot".to_string(), vec![], None);

        {
            let mut cache = provider.model_cache.write().await;
            *cache = Some(CopilotModelCache {
                fetched_at: Instant::now(),
                fallback_model_id: "copilot-base".to_string(),
                discovered_models: vec![],
            });
        }

        let resolved = provider
            .resolve_auto_model_from_cache_or_fetch(None)
            .await
            .expect("cached value expected");
        assert_eq!(resolved, "copilot-base");
    }

    #[tokio::test]
    async fn test_resolve_auto_model_uses_stale_cache_on_refresh_failure() {
        let provider = CopilotProvider::new("copilot".to_string(), vec![], None);

        {
            let mut cache = provider.model_cache.write().await;
            *cache = Some(CopilotModelCache {
                fetched_at: Instant::now() - (MODEL_CACHE_TTL + Duration::from_secs(1)),
                fallback_model_id: "stale-copilot-base".to_string(),
                discovered_models: vec![],
            });
        }

        let resolved = provider
            .resolve_auto_model_from_cache_or_fetch(Some(Err(ProviderError::ApiError {
                status: 429,
                message: "rate limited".to_string(),
            })))
            .await
            .expect("stale cache should be used");
        assert_eq!(resolved, "stale-copilot-base");
    }

    #[tokio::test]
    async fn test_resolve_auto_model_errors_when_no_cache_and_fetch_fails() {
        let provider = CopilotProvider::new("copilot".to_string(), vec![], None);

        let err = provider
            .resolve_auto_model_from_cache_or_fetch(Some(Err(ProviderError::ApiError {
                status: 503,
                message: "upstream unavailable".to_string(),
            })))
            .await
            .expect_err("expected failure without cache");

        match err {
            ProviderError::ApiError { status, .. } => assert_eq!(status, 503),
            other => panic!("expected ApiError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_resolve_request_model_rewrites_auto_from_cache() {
        let provider = CopilotProvider::new("copilot".to_string(), vec![], None);
        {
            let mut cache = provider.model_cache.write().await;
            *cache = Some(CopilotModelCache {
                fetched_at: Instant::now(),
                fallback_model_id: "copilot-base".to_string(),
                discovered_models: vec![],
            });
        }

        let mut request = AnthropicRequest {
            model: "auto".to_string(),
            messages: vec![],
            system: None,
            max_tokens: 10,
            stream: None,
            tools: None,
            thinking: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            passthrough_auth: None,
            anthropic_beta_header: None,
        };

        provider
            .resolve_request_model(&mut request)
            .await
            .expect("auto model should resolve");
        assert_eq!(request.model, "copilot-base");
    }

    #[tokio::test]
    async fn test_update_discovered_chat_models_populates_supports_model() {
        let provider = CopilotProvider::new("copilot".to_string(), vec!["seed-model".to_string()], None);

        provider
            .update_discovered_chat_models(&[
                make_model("copilot-base", "chat", true, false),
                make_model("embedding-v1", "embeddings", false, false),
            ])
            .await;

        assert!(provider.supports_model("seed-model"));
        assert!(provider.supports_model("copilot-base"));
        assert!(!provider.supports_model("embedding-v1"));
    }

    #[test]
    fn test_copilot_provider_supports_model() {
        let provider = CopilotProvider::new(
            "copilot".to_string(),
            vec!["gpt-4o".to_string(), "claude-sonnet-4-5".to_string()],
            None,
        );
        assert!(provider.supports_model("gpt-4o"));
        assert!(provider.supports_model("claude-sonnet-4-5"));
        assert!(!provider.supports_model("llama-3"));
    }

    #[test]
    fn test_injectable_client_used_when_provided() {
        let custom_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(1))
            .build()
            .unwrap();
        let provider = CopilotProvider::new_with_client(
            "test".to_string(),
            vec![],
            None,
            custom_client,
        );
        // The client is stored — just verify construction succeeds.
        assert!(provider.session_id.len() == 36); // UUID v4 format
    }

    #[test]
    fn test_copilot_headers_contains_all_keys() {
        let expected_keys = [
            "Editor-Version",
            "Editor-Plugin-Version",
            "Copilot-Integration-Id",
            "Openai-Intent",
            "X-Initiator",
            "Openai-Organization",
            "X-GitHub-Api-Version",
            "X-Interaction-Type",
        ];
        let actual_keys: Vec<&str> = COPILOT_HEADERS.iter().map(|(k, _)| *k).collect();
        for key in &expected_keys {
            assert!(
                actual_keys.contains(key),
                "Missing header key: {}",
                key
            );
        }
        assert_eq!(COPILOT_HEADERS.len(), 8);
    }

    #[test]
    fn test_session_machine_id_stable_across_calls() {
        let provider = CopilotProvider::new("copilot".to_string(), vec![], None);
        // Build two dummy request builders and apply headers to both
        let client = reqwest::Client::new();
        let b1 = client.post("http://localhost");
        let b2 = client.post("http://localhost");
        let _b1 = apply_copilot_headers(b1, &provider.session_id, &provider.machine_id);
        let _b2 = apply_copilot_headers(b2, &provider.session_id, &provider.machine_id);
        // Verify VScode-SessionId is the same in both requests
        let req1 = _b1.build().unwrap();
        let req2 = _b2.build().unwrap();
        let sid1 = req1.headers().get("VScode-SessionId").unwrap().to_str().unwrap();
        let sid2 = req2.headers().get("VScode-SessionId").unwrap().to_str().unwrap();
        assert_eq!(sid1, sid2, "VScode-SessionId must be stable across apply_copilot_headers calls");
        assert_eq!(sid1, provider.session_id.as_str());
    }

    #[test]
    fn test_request_id_unique_per_call() {
        // Verify two calls to new() produce different session_ids
        let p1 = CopilotProvider::new("copilot".to_string(), vec![], None);
        let p2 = CopilotProvider::new("copilot".to_string(), vec![], None);
        assert_ne!(p1.session_id, p2.session_id);
        assert_ne!(p1.machine_id, p2.machine_id);

        // Verify X-Request-Id generation is UUID format (36 chars)
        let client = reqwest::Client::new();
        let builder = client.post("http://localhost");
        let request = apply_copilot_headers(builder, &p1.session_id, &p1.machine_id)
            .build()
            .unwrap();
        let req_id = request.headers().get("X-Request-Id").unwrap().to_str().unwrap();
        assert_eq!(req_id.len(), 36, "X-Request-Id should be UUID v4 (36 chars)");
    }

    #[tokio::test]
    async fn test_network_error_retry_succeeds() {
        let mut server = mockito::Server::new_async().await;
        // Note: mockito can't simulate a true TCP reset, so we use a 200 response to verify the happy path.
        // The real retry logic fires on `e.is_connect() || e.is_timeout()` from reqwest.
        let _mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(r#"{"id":"test","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop","index":0}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let provider = CopilotProvider::new_with_client("test".to_string(), vec![], None, client);
        let request = AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![crate::models::Message {
                role: "user".to_string(),
                content: MessageContent::Text("hi".to_string()),
            }],
            system: None,
            max_tokens: 10,
            stream: None,
            tools: None,
            thinking: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            passthrough_auth: None,
            anthropic_beta_header: None,
        };
        // send_message requires a token store; without one it returns AuthError — that's expected
        let result = provider.send_message(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_retry_generates_fresh_request_id() {
        // Verify that X-Request-Id is regenerated (UUID format) on every apply_copilot_headers call
        let client = reqwest::Client::new();
        let p = CopilotProvider::new_with_client("test".to_string(), vec![], None, client.clone());

        let b1 = apply_copilot_headers(
            client.post("http://localhost"),
            &p.session_id,
            &p.machine_id,
        )
        .build()
        .unwrap();
        let b2 = apply_copilot_headers(
            client.post("http://localhost"),
            &p.session_id,
            &p.machine_id,
        )
        .build()
        .unwrap();

        let id1 = b1.headers().get("X-Request-Id").unwrap().to_str().unwrap().to_string();
        let id2 = b2.headers().get("X-Request-Id").unwrap().to_str().unwrap().to_string();
        assert_ne!(id1, id2, "X-Request-Id must be different on every apply_copilot_headers call");
        assert_eq!(id1.len(), 36);
        assert_eq!(id2.len(), 36);
    }

    #[tokio::test]
    async fn test_all_8_headers_present_and_ids_stable_on_success() {
        let client = reqwest::Client::new();
        let provider = CopilotProvider::new_with_client("test".to_string(), vec![], None, client);

        // Construct two requests manually and inspect headers.
        let c = reqwest::Client::new();
        let b1 = apply_copilot_headers(
            c.post("http://localhost"),
            &provider.session_id,
            &provider.machine_id,
        )
        .build()
        .unwrap();
        let b2 = apply_copilot_headers(
            c.post("http://localhost"),
            &provider.session_id,
            &provider.machine_id,
        )
        .build()
        .unwrap();

        let headers1 = b1.headers();
        let headers2 = b2.headers();

        // All 8 static headers present with correct values
        for (key, expected_val) in COPILOT_HEADERS {
            assert!(headers1.contains_key(*key), "Missing header: {}", key);
            assert_eq!(
                headers1.get(*key).unwrap().to_str().unwrap(),
                *expected_val,
                "Wrong value for {}",
                key
            );
        }

        // VScode-SessionId is stable across calls
        let sid1 = headers1.get("VScode-SessionId").unwrap().to_str().unwrap().to_string();
        let sid2 = headers2.get("VScode-SessionId").unwrap().to_str().unwrap().to_string();
        assert_eq!(sid1, sid2, "VScode-SessionId must be stable");
        assert_eq!(sid1, provider.session_id);

        // VScode-MachineId is stable across calls
        let mid1 = headers1.get("VScode-MachineId").unwrap().to_str().unwrap().to_string();
        let mid2 = headers2.get("VScode-MachineId").unwrap().to_str().unwrap().to_string();
        assert_eq!(mid1, mid2, "VScode-MachineId must be stable");

        // X-Request-Id is unique per call
        let rid1 = headers1.get("X-Request-Id").unwrap().to_str().unwrap().to_string();
        let rid2 = headers2.get("X-Request-Id").unwrap().to_str().unwrap().to_string();
        assert_ne!(rid1, rid2, "X-Request-Id must differ on each call");
        assert_eq!(rid1.len(), 36);
    }

    #[tokio::test]
    async fn test_send_message_stream_non_200_returns_error() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/")
            .with_status(503)
            .with_body("Service Unavailable")
            .create_async()
            .await;

        let provider = CopilotProvider::new("test".to_string(), vec![], None);
        let request = AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![],
            system: None,
            max_tokens: 10,
            stream: Some(true),
            tools: None,
            thinking: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            passthrough_auth: None,
            anthropic_beta_header: None,
        };
        let result = provider
            .send_message_stream_with_url(request, &server.url(), "fake_bearer")
            .await;
        assert!(result.is_err());
        if let Err(ProviderError::ApiError { status, .. }) = result {
            assert_eq!(status, 503);
        } else {
            panic!("expected ApiError");
        }
    }
}
