use super::{AnthropicProvider, ProviderResponse, error::ProviderError};
use super::openai::{OpenAIProvider, OpenAIResponse};
use crate::auth::{TokenStore, OAuthToken};
use crate::auth::github_copilot::{parse_proxy_ep, refresh_copilot_token};
use crate::models::{AnthropicRequest, CountTokensRequest, CountTokensResponse, MessageContent};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::stream::Stream;
use reqwest::Client;
use std::pin::Pin;
use std::sync::Arc;
use futures::stream::TryStreamExt;

const COPILOT_HEADERS: &[(&str, &str)] = &[
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
    ("Openai-Intent", "conversation-edits"),
    ("X-Initiator", "user"),
];

pub struct CopilotProvider {
    name: String,
    models: Vec<String>,
    token_store: Option<TokenStore>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    client: Client,
}

impl CopilotProvider {
    pub fn new(name: String, models: Vec<String>, token_store: Option<TokenStore>) -> Self {
        Self {
            name,
            models,
            token_store,
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            client: Client::new(),
        }
    }

    async fn get_valid_copilot_token(&self) -> Result<String, ProviderError> {
        let token_store = self.token_store.as_ref().ok_or_else(|| {
            ProviderError::AuthError("TokenStore not configured for copilot provider".to_string())
        })?;

        // Fast path: check without lock
        if let Some(token) = token_store.get(&self.name) {
            if !token.needs_refresh() {
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
            if !token.needs_refresh() {
                return Ok(token.access_token.clone());
            }

            let github_token = token.refresh_token.clone();
            let copilot_resp = refresh_copilot_token(&self.client, &github_token).await.map_err(|e| {
                ProviderError::AuthError(format!("Failed to refresh Copilot token: {}", e))
            })?;

            let new_expires_at = chrono::DateTime::from_timestamp(
                copilot_resp.expires_at.min(i64::MAX as u64) as i64, 0
            ).unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(30));

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
    async fn send_message(&self, request: AnthropicRequest) -> Result<ProviderResponse, ProviderError> {
        let bearer = self.get_valid_copilot_token().await?;
        let base_url = parse_proxy_ep(&bearer);
        let url = format!("{}/chat/completions", base_url);

        let delegate = Self::make_delegate();
        let openai_request = delegate.transform_request(&request)?;

        let mut req_builder = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", bearer))
            .header("Content-Type", "application/json");

        for (key, value) in COPILOT_HEADERS {
            req_builder = req_builder.header(*key, *value);
        }

        let response = req_builder.json(&openai_request).send().await
            .map_err(ProviderError::HttpError)?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ProviderError::ApiError { status, message: error_text });
        }

        let response_text = response.text().await.map_err(ProviderError::HttpError)?;
        tracing::debug!("Copilot provider response: {}", response_text);

        let openai_response: OpenAIResponse = serde_json::from_str(&response_text)
            .map_err(|e| ProviderError::ApiError { status: 500, message: e.to_string() })?;

        Ok(delegate.transform_response(openai_response))
    }

    async fn send_message_stream(
        &self,
        request: AnthropicRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>, ProviderError> {
        let bearer = self.get_valid_copilot_token().await?;
        let base_url = parse_proxy_ep(&bearer);
        let url = format!("{}/chat/completions", base_url);

        let delegate = Self::make_delegate();
        let openai_request = delegate.transform_request(&request)?;

        let mut req_builder = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", bearer))
            .header("Content-Type", "application/json")
            .header("accept", "text/event-stream");

        for (key, value) in COPILOT_HEADERS {
            req_builder = req_builder.header(*key, *value);
        }

        let response = req_builder.json(&openai_request).send().await
            .map_err(ProviderError::HttpError)?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ProviderError::ApiError { status, message: error_text });
        }

        let stream = response.bytes_stream().map_err(ProviderError::HttpError);
        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, request: CountTokensRequest) -> Result<CountTokensResponse, ProviderError> {
        let mut total_chars = 0usize;

        if let Some(ref system) = request.system {
            let text = match system {
                crate::models::SystemPrompt::Text(t) => t.clone(),
                crate::models::SystemPrompt::Blocks(blocks) => {
                    blocks.iter().map(|b| b.text.clone()).collect::<Vec<_>>().join("\n")
                }
            };
            total_chars += text.len();
        }

        for msg in &request.messages {
            let text = match &msg.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Blocks(blocks) => blocks.iter()
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
        self.models.iter().any(|m| m == model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_copilot_headers_count() {
        assert_eq!(COPILOT_HEADERS.len(), 5);
        let keys: Vec<&str> = COPILOT_HEADERS.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"Editor-Version"));
        assert!(keys.contains(&"Editor-Plugin-Version"));
        assert!(keys.contains(&"Copilot-Integration-Id"));
        assert!(keys.contains(&"Openai-Intent"));
        assert!(keys.contains(&"X-Initiator"));
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
}
