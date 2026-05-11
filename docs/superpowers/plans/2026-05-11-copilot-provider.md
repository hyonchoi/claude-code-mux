# GitHub Copilot Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `copilot` provider type to claude-code-mux so GitHub Copilot subscribers can route requests through `api.individual.githubcopilot.com` using their existing subscription.

**Architecture:** Dedicated `CopilotProvider` (implementing `AnthropicProvider`) with a separate `github_copilot` auth module for device code flow and bearer token refresh. The provider converts Anthropic-format requests to OpenAI Chat Completions format using shared helpers exposed from `openai.rs`, injects 5 required Copilot headers, extracts the base URL from the bearer token's `proxy-ep` field, and saves tokens in the existing `TokenStore` (`refresh_token` = long-lived GitHub OAuth token, `access_token` = short-lived Copilot bearer).

**Tech Stack:** Rust, reqwest, axum, serde_json, tokio::sync::Mutex (concurrent refresh guard), chrono (token expiry), existing `OAuthToken`/`TokenStore`, existing Anthropic→OpenAI transform logic from `openai.rs`

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `src/auth/github_copilot.rs` | Create | Device code flow, GitHub polling, bearer token refresh, proxy-ep parsing |
| `src/providers/copilot.rs` | Create | `CopilotProvider` struct, trait impl, header injection, token refresh coordination |
| `src/auth/mod.rs` | Modify | Re-export `github_copilot` module |
| `src/providers/mod.rs` | Modify | Re-export `copilot` module |
| `src/providers/openai.rs` | Modify | Make `OpenAIRequest`/`OpenAIResponse` and transform methods `pub` for reuse |
| `src/providers/registry.rs` | Modify | Add `"copilot"` match arm |
| `src/server/oauth_handlers.rs` | Modify | Add `copilot_start` and `copilot_exchange` handlers |
| `src/server/mod.rs` | Modify | Register new copilot OAuth routes |
| `src/server/admin.html` | Modify | 7 targeted UI changes for device code modal |
| `config/example.toml` | Modify | Add commented copilot provider example |

---

## Task 1: Auth module — `src/auth/github_copilot.rs`

**Files:**
- Create: `src/auth/github_copilot.rs`

This module handles all GitHub-specific auth: starting the device flow, polling for authorization, exchanging for a Copilot bearer token, refreshing it, and parsing the `proxy-ep` field.

- [ ] **Step 1: Write failing unit tests**

```rust
// src/auth/github_copilot.rs — add tests module at the bottom of the file you'll create

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proxy_ep_standard() {
        let bearer = "tid=abc;exp=123;proxy-ep=proxy.individual.githubcopilot.com;sku=foo";
        assert_eq!(
            parse_proxy_ep(bearer),
            "https://api.individual.githubcopilot.com"
        );
    }

    #[test]
    fn test_parse_proxy_ep_missing_field_returns_fallback() {
        let bearer = "tid=abc;exp=123;sku=foo";
        assert_eq!(
            parse_proxy_ep(bearer),
            "https://api.individual.githubcopilot.com"
        );
    }

    #[test]
    fn test_parse_proxy_ep_no_proxy_prefix_passthrough() {
        let bearer = "tid=abc;proxy-ep=custom.endpoint.com";
        assert_eq!(parse_proxy_ep(bearer), "https://custom.endpoint.com");
    }
}
```

Create the file with just the test module (no impl yet) and verify it doesn't compile:

Run: `cargo test -p claude-code-mux auth::github_copilot 2>&1 | head -20`
Expected: compile error — `parse_proxy_ep` not defined

- [ ] **Step 2: Create `src/auth/github_copilot.rs` with full implementation**

```rust
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// GitHub OAuth app client ID for Copilot device flow
/// (Visual Studio Code — GitHub Copilot Chat application)
const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const COPILOT_FALLBACK_BASE_URL: &str = "https://api.individual.githubcopilot.com";

// ── Device code response ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

// ── GitHub access token (result of device flow) ───────────────────────────────

#[derive(Debug, Deserialize)]
struct GitHubAccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    // interval increase for slow_down
    interval: Option<u64>,
}

// ── Copilot bearer token ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CopilotTokenResponse {
    pub token: String,
    pub expires_at: u64, // Unix timestamp (seconds)
}

// ── Poll result ───────────────────────────────────────────────────────────────

pub enum PollResult {
    Success(String), // GitHub OAuth access token
    Pending,
    Expired,
}

// ── Public functions ──────────────────────────────────────────────────────────

/// Start GitHub device code flow. Returns response with user_code and verification_uri.
pub async fn start_device_flow() -> Result<DeviceCodeResponse> {
    let client = Client::new();
    let response = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("client_id={}&scope=read:user", GITHUB_CLIENT_ID))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub device code request failed ({status}): {body}");
    }

    let device_response: DeviceCodeResponse = response.json().await?;
    Ok(device_response)
}

/// Poll GitHub's token endpoint for one round. Returns PollResult.
/// `interval` is the current polling interval in seconds (may increase on slow_down).
/// Returns updated interval so the caller can adjust for slow_down.
pub async fn poll_github_token_once(
    device_code: &str,
    interval: u64,
) -> Result<(PollResult, u64)> {
    let client = Client::new();
    let body = format!(
        "client_id={}&device_code={}&grant_type=urn:ietf:params:oauth:grant-type:device_code",
        GITHUB_CLIENT_ID, device_code
    );

    let response = client
        .post(GITHUB_ACCESS_TOKEN_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    let token_resp: GitHubAccessTokenResponse = response.json().await?;

    match token_resp.error.as_deref() {
        None => {
            // success
            let access_token = token_resp
                .access_token
                .ok_or_else(|| anyhow::anyhow!("GitHub returned no access_token"))?;
            Ok((PollResult::Success(access_token), interval))
        }
        Some("authorization_pending") => Ok((PollResult::Pending, interval)),
        Some("slow_down") => {
            let new_interval = interval + 5;
            Ok((PollResult::Pending, new_interval))
        }
        Some("expired_token") => Ok((PollResult::Expired, interval)),
        Some(other) => anyhow::bail!("GitHub token error: {other}"),
    }
}

/// Poll GitHub for authorization for up to `max_secs` seconds.
/// Returns the GitHub OAuth access token on success, or PollResult::Pending/Expired.
pub async fn poll_for_github_token(
    device_code: &str,
    mut interval: u64,
    max_secs: u64,
) -> Result<PollResult> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(max_secs);

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;

        if tokio::time::Instant::now() >= deadline {
            return Ok(PollResult::Pending);
        }

        let (result, new_interval) = poll_github_token_once(device_code, interval).await?;
        interval = new_interval;

        match result {
            PollResult::Success(token) => return Ok(PollResult::Success(token)),
            PollResult::Expired => return Ok(PollResult::Expired),
            PollResult::Pending => continue,
        }
    }
}

/// Exchange a GitHub OAuth access token for a Copilot bearer token.
pub async fn exchange_for_copilot_token(github_token: &str) -> Result<CopilotTokenResponse> {
    fetch_copilot_token(github_token).await
}

/// Refresh an existing Copilot bearer token using the stored GitHub OAuth token.
pub async fn refresh_copilot_token(github_token: &str) -> Result<CopilotTokenResponse> {
    fetch_copilot_token(github_token).await
}

/// Parse the `proxy-ep` field from a semicolon-delimited Copilot bearer token.
/// Returns `https://api.<rest>` for tokens containing `proxy-ep=proxy.<rest>`,
/// or the fallback URL if the field is absent.
pub fn parse_proxy_ep(bearer: &str) -> String {
    for field in bearer.split(';') {
        if let Some(val) = field.strip_prefix("proxy-ep=") {
            let api_host = val
                .strip_prefix("proxy.")
                .map(|s| format!("api.{}", s))
                .unwrap_or_else(|| val.to_string());
            return format!("https://{}", api_host);
        }
    }
    COPILOT_FALLBACK_BASE_URL.to_string()
}

// ── Private helpers ───────────────────────────────────────────────────────────

async fn fetch_copilot_token(github_token: &str) -> Result<CopilotTokenResponse> {
    let client = Client::new();
    let response = client
        .get(COPILOT_TOKEN_URL)
        .header("Authorization", format!("Bearer {}", github_token))
        .header("Editor-Version", "vscode/1.107.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Copilot token request failed ({status}): {body}");
    }

    let token_response: CopilotTokenResponse = response.json().await?;
    Ok(token_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proxy_ep_standard() {
        let bearer = "tid=abc;exp=123;proxy-ep=proxy.individual.githubcopilot.com;sku=foo";
        assert_eq!(
            parse_proxy_ep(bearer),
            "https://api.individual.githubcopilot.com"
        );
    }

    #[test]
    fn test_parse_proxy_ep_missing_field_returns_fallback() {
        let bearer = "tid=abc;exp=123;sku=foo";
        assert_eq!(
            parse_proxy_ep(bearer),
            "https://api.individual.githubcopilot.com"
        );
    }

    #[test]
    fn test_parse_proxy_ep_no_proxy_prefix_passthrough() {
        let bearer = "tid=abc;proxy-ep=custom.endpoint.com";
        assert_eq!(parse_proxy_ep(bearer), "https://custom.endpoint.com");
    }
}
```

- [ ] **Step 3: Add `github_copilot` to `src/auth/mod.rs`**

Open `src/auth/mod.rs` and add:
```rust
pub mod github_copilot;
```
After the existing `pub mod oauth;` line. The file should now be:
```rust
pub mod oauth;
pub mod token_store;
pub mod github_copilot;

pub use oauth::{OAuthClient, OAuthConfig, AuthorizationUrl, PKCEVerifier};
pub use token_store::{TokenStore, OAuthToken};
```

- [ ] **Step 4: Run tests and verify they pass**

Run: `cargo test auth::github_copilot::tests -v`
Expected output:
```
test auth::github_copilot::tests::test_parse_proxy_ep_missing_field_returns_fallback ... ok
test auth::github_copilot::tests::test_parse_proxy_ep_no_proxy_prefix_passthrough ... ok
test auth::github_copilot::tests::test_parse_proxy_ep_standard ... ok
```

- [ ] **Step 5: Commit**

```bash
git add src/auth/github_copilot.rs src/auth/mod.rs
git commit -m "feat: add github_copilot auth module with device flow and token functions"
```

---

## Task 2: Expose OpenAI helpers for reuse

**Files:**
- Modify: `src/providers/openai.rs`

`CopilotProvider` needs `OpenAIRequest`, `OpenAIResponse`, and related structs along with the `transform_request` / `transform_response` methods from `OpenAIProvider`. Currently these are private. We make them `pub` so they can be used from `copilot.rs`.

- [ ] **Step 1: Run existing OpenAI tests to establish baseline**

Run: `cargo test providers::openai -v 2>&1 | tail -20`
Expected: all tests pass (note current count).

- [ ] **Step 2: Make required types and methods pub in `src/providers/openai.rs`**

Change these struct declarations from private to `pub(crate)`:

Find the line `struct OpenAIRequest {` (around line 22) and change to `pub(crate) struct OpenAIRequest`.

Repeat for each of these structs — change `struct` to `pub(crate) struct`:
- `OpenAIRequest` (line ~22)
- `OpenAIContent` (line ~72)
- `OpenAIContentPart` (line ~80)
- `OpenAIImageUrl` (line ~90)
- `OpenAIToolCall` (line ~96)
- `OpenAIFunctionCall` (line ~103)
- `OpenAITool` (line ~110)
- `OpenAIFunctionDef` (line ~117)
- `OpenAIMessage` (line ~127)
- `OpenAIResponse` (line ~140)
- `OpenAIChoice` (line ~151)
- `OpenAIUsage` (line ~157)

Also make `transform_request` and `transform_response` pub(crate) by changing their `fn` to `pub(crate) fn`:
- `fn transform_request` (around line 702)
- `fn transform_response` (around line 878)

- [ ] **Step 3: Verify existing tests still pass**

Run: `cargo test providers::openai -v 2>&1 | tail -20`
Expected: same test count passes as in Step 1.

- [ ] **Step 4: Commit**

```bash
git add src/providers/openai.rs
git commit -m "feat: expose openai request/response types as pub(crate) for copilot provider reuse"
```

---

## Task 3: `src/providers/copilot.rs` — CopilotProvider

**Files:**
- Create: `src/providers/copilot.rs`

`CopilotProvider` wraps an internal `OpenAIProvider` for request/response transformation, handles Copilot-specific auth (bearer token refresh with mutex), injects required headers, and extracts the API base URL from the token.

- [ ] **Step 1: Write failing test for header injection**

Create `src/providers/copilot.rs` with just the test:

```rust
#[cfg(test)]
mod tests {
    // Test will be added after full implementation in step 2
    // (placeholder test to verify compile)
    #[test]
    fn test_placeholder() {}
}
```

Run: `cargo test providers::copilot -v`
Expected: `test_placeholder ... ok`

- [ ] **Step 2: Write the full `CopilotProvider` implementation**

Replace `src/providers/copilot.rs` with:

```rust
use super::{AnthropicProvider, ProviderResponse, Usage, error::ProviderError};
use super::openai::{OpenAIProvider, OpenAIRequest};
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

    /// Returns a valid Copilot bearer token, refreshing if needed.
    /// Uses double-checked locking to prevent concurrent refresh races.
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

        // Re-check after acquiring lock (another waiter may have already refreshed)
        if let Some(token) = token_store.get(&self.name) {
            if !token.needs_refresh() {
                return Ok(token.access_token.clone());
            }

            // Token still needs refresh — do it
            let github_token = token.refresh_token.clone();
            let copilot_resp = refresh_copilot_token(&github_token).await.map_err(|e| {
                ProviderError::AuthError(format!("Failed to refresh Copilot token: {}", e))
            })?;

            let new_expires_at = chrono::DateTime::from_timestamp(copilot_resp.expires_at as i64, 0)
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

    fn transform_request(request: &AnthropicRequest) -> Result<OpenAIRequest, ProviderError> {
        // Delegate to OpenAIProvider's pub(crate) transform_request via a temporary instance
        let delegate = OpenAIProvider::new(
            String::new(),
            String::new(),
            String::new(),
            vec![],
            None,
            None,
        );
        delegate.transform_request(request)
    }
}

#[async_trait]
impl AnthropicProvider for CopilotProvider {
    async fn send_message(&self, request: AnthropicRequest) -> Result<ProviderResponse, ProviderError> {
        let bearer = self.get_valid_copilot_token().await?;
        let base_url = parse_proxy_ep(&bearer);
        let url = format!("{}/chat/completions", base_url);

        let openai_request = Self::transform_request(&request)?;

        let mut req_builder = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", bearer))
            .header("Content-Type", "application/json");

        for (key, value) in COPILOT_HEADERS {
            req_builder = req_builder.header(*key, *value);
        }

        let response = req_builder.json(&openai_request).send().await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ProviderError::ApiError { status, message: error_text });
        }

        let response_text = response.text().await?;
        tracing::debug!("Copilot provider response: {}", response_text);

        // Delegate OpenAI→Anthropic response transform to OpenAIProvider
        let delegate = OpenAIProvider::new(
            String::new(), String::new(), String::new(), vec![], None, None,
        );
        let openai_response: super::openai::OpenAIResponse = serde_json::from_str(&response_text)
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

        let openai_request = Self::transform_request(&request)?;

        let mut req_builder = self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", bearer))
            .header("Content-Type", "application/json")
            .header("accept", "text/event-stream");

        for (key, value) in COPILOT_HEADERS {
            req_builder = req_builder.header(*key, *value);
        }

        let response = req_builder.json(&openai_request).send().await?;

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

    #[test]
    fn test_placeholder() {}
}
```

Note: `OpenAIResponse` and `transform_response` must be `pub(crate)` in openai.rs (done in Task 2) for this to compile.

- [ ] **Step 3: Add `copilot` to `src/providers/mod.rs`**

Open `src/providers/mod.rs` and add after `pub mod streaming;`:
```rust
pub mod copilot;
```

Also add at the bottom with the other re-exports:
```rust
pub use copilot::CopilotProvider;
```

- [ ] **Step 4: Run tests**

Run: `cargo test providers::copilot -v`
Expected:
```
test providers::copilot::tests::test_copilot_headers_count ... ok
test providers::copilot::tests::test_copilot_provider_supports_model ... ok
test providers::copilot::tests::test_placeholder ... ok
```

Run: `cargo build 2>&1 | head -30`
Expected: no errors.

- [ ] **Step 5: Commit**

```bash
git add src/providers/copilot.rs src/providers/mod.rs
git commit -m "feat: add CopilotProvider with token refresh and OpenAI request/response transform"
```

---

## Task 4: Wire into registry

**Files:**
- Modify: `src/providers/registry.rs`

Add the `"copilot"` match arm. The existing OAuth branch already sets `api_key` to `config.oauth_provider.clone().unwrap_or_else(|| config.name.clone())`, so no api_key bypass is needed — just add the match arm.

- [ ] **Step 1: Write a registry test**

Add to the `#[cfg(test)]` block at the bottom of `src/providers/registry.rs`:

```rust
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
        models: vec!["gpt-4o".to_string()],
        enabled: Some(true),
        supported_beta_options: vec![],
        rate_limit_rpm: None,
        rate_limit_max_wait_ms: None,
        project_id: None,
        location: None,
    };

    let result = ProviderRegistry::from_configs(&[config], None);
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());
    let registry = result.unwrap();
    assert!(registry.get_provider("my-copilot").is_some());
}
```

Run: `cargo test providers::registry::test_copilot_provider_registration -v`
Expected: FAIL — `"copilot"` not matched → `Unknown provider type: copilot`

- [ ] **Step 2: Add `"copilot"` arm to `ProviderRegistry::from_configs`**

Open `src/providers/registry.rs`. Find the `other => { return Err(...) }` arm at the end of the match block (around line 251). Insert before it:

```rust
"copilot" => Box::new(crate::providers::CopilotProvider::new(
    config.name.clone(),
    config.models.clone(),
    token_store.clone(),
)),
```

Also add the import at the top of the file if not already present:
```rust
use crate::providers::copilot::CopilotProvider;
```
(or use the full path inline as shown above)

- [ ] **Step 3: Run the test**

Run: `cargo test providers::registry::test_copilot_provider_registration -v`
Expected: `test providers::registry::test_copilot_provider_registration ... ok`

Run: `cargo test providers::registry -v 2>&1 | tail -15`
Expected: all existing registry tests still pass.

- [ ] **Step 4: Commit**

```bash
git add src/providers/registry.rs
git commit -m "feat: register copilot provider type in ProviderRegistry"
```

---

## Task 5: New OAuth endpoints

**Files:**
- Modify: `src/server/oauth_handlers.rs`
- Modify: `src/server/mod.rs`

Add `POST /api/oauth/copilot-start` and `POST /api/oauth/copilot-exchange`. These do NOT use existing `OAuthAuthorizeRequest`/`OAuthExchangeRequest` structs.

- [ ] **Step 1: Add copilot handler functions to `src/server/oauth_handlers.rs`**

Add these imports at the top of `oauth_handlers.rs` (after existing imports):
```rust
use crate::auth::github_copilot::{
    start_device_flow, poll_for_github_token, exchange_for_copilot_token, PollResult,
};
use crate::auth::OAuthToken;
use chrono::Utc;
```

Then add these new structs and handlers at the end of the file (before the closing brace, after `oauth_callback`):

```rust
// ── Copilot device code flow ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CopilotStartRequest {
    pub provider_id: String,
}

#[derive(Debug, Serialize)]
pub struct CopilotStartResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
pub struct CopilotExchangeRequest {
    pub provider_id: String,
    pub device_code: String,
}

#[derive(Debug, Serialize)]
pub struct CopilotExchangeResponse {
    pub status: String, // "success" | "pending" | "expired"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
}

/// Start GitHub Copilot device code flow.
/// Returns the user_code + verification_uri for the admin UI to display.
pub async fn copilot_start(
    State(_state): State<Arc<AppState>>,
    Json(_req): Json<CopilotStartRequest>,
) -> Result<Json<CopilotStartResponse>, (StatusCode, String)> {
    let device_resp = start_device_flow().await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to start device flow: {}", e))
    })?;

    Ok(Json(CopilotStartResponse {
        device_code: device_resp.device_code,
        user_code: device_resp.user_code,
        verification_uri: device_resp.verification_uri,
        expires_in: device_resp.expires_in,
        interval: device_resp.interval,
    }))
}

/// Poll GitHub for device code authorization. Hard 60-second timeout per call.
/// On success: exchange for Copilot bearer token and save to TokenStore.
/// Returns {status: "success"}, {status: "pending"}, or {status: "expired"}.
pub async fn copilot_exchange(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CopilotExchangeRequest>,
) -> Result<Json<CopilotExchangeResponse>, (StatusCode, String)> {
    let poll_result = poll_for_github_token(&req.device_code, 5, 60)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Polling error: {}", e))
        })?;

    match poll_result {
        PollResult::Success(github_token) => {
            // Exchange GitHub token for Copilot bearer token
            let copilot_token = exchange_for_copilot_token(&github_token).await.map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Copilot token exchange failed: {}", e))
            })?;

            let expires_at =
                chrono::DateTime::from_timestamp(copilot_token.expires_at as i64, 0)
                    .unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(30));

            let oauth_token = OAuthToken {
                provider_id: req.provider_id.clone(),
                access_token: copilot_token.token,
                refresh_token: github_token,
                expires_at,
                enterprise_url: None,
                project_id: None,
            };

            state.token_store.save(oauth_token).map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save token: {}", e))
            })?;

            tracing::info!("✅ Copilot authentication successful for '{}'", req.provider_id);

            Ok(Json(CopilotExchangeResponse {
                status: "success".to_string(),
                provider_id: Some(req.provider_id),
            }))
        }
        PollResult::Pending => Ok(Json(CopilotExchangeResponse {
            status: "pending".to_string(),
            provider_id: None,
        })),
        PollResult::Expired => Ok(Json(CopilotExchangeResponse {
            status: "expired".to_string(),
            provider_id: None,
        })),
    }
}
```

- [ ] **Step 2: Register new routes in `src/server/mod.rs`**

Open `src/server/mod.rs`. Find the OAuth routes section (around line 124). Add after the existing `.route("/api/oauth/tokens/refresh", ...)` line:

```rust
.route("/api/oauth/copilot-start", post(oauth_handlers::copilot_start))
.route("/api/oauth/copilot-exchange", post(oauth_handlers::copilot_exchange))
```

- [ ] **Step 3: Build and verify**

Run: `cargo build 2>&1 | head -30`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add src/server/oauth_handlers.rs src/server/mod.rs
git commit -m "feat: add copilot-start and copilot-exchange OAuth endpoints"
```

---

## Task 6: Admin UI — 7 changes to `src/server/admin.html`

**Files:**
- Modify: `src/server/admin.html`

Make all 7 changes. Each change is small and targeted.

### Change 1: Add Copilot radio button to provider type picker

- [ ] **Step 1.1: Find insertion point**

The last radio button in the provider type grid ends around line 919 (Baseten `</label>`). Find the line that closes the provider type grid:
```html
                                </div>
                            </div>

                            <!-- Step 2: Basic Info -->
```

Insert the Copilot radio **before** that closing `</div></div>` block (after the Baseten `</label>`):

```html
                                    <label class="cursor-pointer">
                                        <input
                                            type="radio"
                                            name="provider_type"
                                            value="copilot"
                                            class="peer sr-only"
                                        />
                                        <div
                                            class="p-6 border-2 border-gray-200 rounded-xl peer-checked:border-blue-600 peer-checked:bg-blue-50 hover:border-gray-300 transition-all"
                                        >
                                            <div class="text-xl font-bold mb-1">
                                                GitHub Copilot
                                            </div>
                                            <div class="text-sm text-gray-600">
                                                GPT-4o, Claude via Copilot subscription
                                            </div>
                                        </div>
                                    </label>
```

### Change 2: Update `updateOAuthLabel()` for copilot

- [ ] **Step 2.1: Find the `gemini` branch in `updateOAuthLabel()`** (around line 4141)

The function has:
```javascript
                } else if (providerType === "gemini") {
                    oauthLabel.textContent = "OAuth (Google AI Pro/Ultra)";
                    ...
                } else {
                    oauthLabel.textContent = "OAuth (Claude Pro/Max)";
```

Add a new `else if` block between the `gemini` block and the final `else`:

```javascript
                } else if (providerType === "copilot") {
                    oauthLabel.textContent = "OAuth (GitHub Copilot)";
                    oauthDescription.textContent =
                        "Free for GitHub Copilot subscribers";
                    step1Instruction.textContent =
                        "Click the button below to authenticate with your GitHub Copilot account.";
                    step2Instructions.innerHTML = `
                        <li>A code will appear — enter it at github.com/login/device</li>
                        <li>Log in to your GitHub account and authorize</li>
                        <li>Return here and click "I've Authorized"</li>
                    `;
                } else {
```

### Change 3: Intercept `startOAuthFlow()` for copilot

- [ ] **Step 3.1: Find `async function startOAuthFlow()` (around line 4327)**

At the very start of the function body (after the opening `{` and before `try {`), add:

```javascript
            async function startOAuthFlow() {
                const providerType = document.querySelector(
                    'input[name="provider_type"]:checked',
                )?.value;

                // Copilot uses device code flow — route to dedicated handler
                if (providerType === "copilot") {
                    return startCopilotFlow();
                }

                try {
```

(The existing `try {` was already there — just insert the if-block before it. Do NOT remove the providerType declaration at the top of the existing function — merge them: the existing function already has its own `const providerType` inside the try block. Move the `const providerType` declaration to be the first line of the function, then add the if-check, then the try.)

The resulting start of the function should be:
```javascript
            async function startOAuthFlow() {
                const providerType = document.querySelector(
                    'input[name="provider_type"]:checked',
                )?.value;

                if (providerType === "copilot") {
                    return startCopilotFlow();
                }

                try {
                    // Determine oauth_type based on provider type
                    let oauth_type = "max"; // default to anthropic max

                    if (providerType === "openai") {
                        oauth_type = "openai-codex";
                    } else if (providerType === "gemini") {
                        oauth_type = "gemini";
                    }
                    // ... rest of existing function (remove the inner const providerType that was there)
```

### Change 4: Add `startCopilotFlow()` function

- [ ] **Step 4.1: Add after `startOAuthFlow()` closing brace (around line 4427)**

```javascript
            async function startCopilotFlow() {
                // Clear any stale session state
                sessionStorage.removeItem("copilot_device_code");
                sessionStorage.removeItem("copilot_provider_id");
                sessionStorage.removeItem("copilot_expires_at");

                const providerName =
                    document.querySelector('input[name="provider_name"]').value || "copilot";

                try {
                    const response = await fetch("/api/oauth/copilot-start", {
                        method: "POST",
                        headers: { "Content-Type": "application/json" },
                        body: JSON.stringify({ provider_id: providerName }),
                    });
                    if (!response.ok) throw new Error("Failed to start Copilot auth");
                    const { user_code, verification_uri, device_code, expires_in, interval } =
                        await response.json();

                    // Show device code in step-2 area
                    document.getElementById("oauth-step-1").classList.add("hidden");
                    document.getElementById("oauth-step-2").classList.remove("hidden");
                    document.getElementById("oauth-step2-instructions").innerHTML = `
                        <li>
                            Go to <a href="${escapeHtml(verification_uri)}" target="_blank"
                                class="text-blue-600 underline">${escapeHtml(verification_uri)}</a>
                            (opening automatically...)
                        </li>
                        <li>Enter code: <strong class="font-mono text-lg bg-gray-100 px-2 py-1 rounded">${escapeHtml(user_code)}</strong></li>
                        <li>Log in to GitHub and authorize</li>
                        <li>Click "I've Authorized" below</li>
                    `;

                    // Auto-open GitHub device page
                    window.open(verification_uri, "GitHub Copilot Auth", "width=600,height=800");

                    // Store state for exchange step
                    sessionStorage.setItem("copilot_device_code", device_code);
                    sessionStorage.setItem("copilot_provider_id", providerName);
                    sessionStorage.setItem(
                        "copilot_expires_at",
                        String(Date.now() + expires_in * 1000),
                    );

                    notifySuccess(
                        "Enter the code on GitHub, then click 'I\\'ve Authorized'.",
                    );
                } catch (error) {
                    notifyError(`Failed to start Copilot auth: ${error.message}`);
                }
            }
```

Note: `escapeHtml` is a utility already present in admin.html. If it is not present, add this helper before the function:
```javascript
function escapeHtml(str) {
    return str.replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;");
}
```
Check first: `grep -n "function escapeHtml" src/server/admin.html`

### Change 5: Add `completeCopilotFlow()` function

- [ ] **Step 5.1: Add after `startCopilotFlow()` closing brace**

```javascript
            async function completeCopilotFlow() {
                const deviceCode = sessionStorage.getItem("copilot_device_code");
                const providerId = sessionStorage.getItem("copilot_provider_id");
                const expiresAt = parseInt(
                    sessionStorage.getItem("copilot_expires_at") || "0",
                );

                if (!deviceCode || !providerId) {
                    notifyError("Session expired. Please restart the flow.");
                    return;
                }
                if (Date.now() > expiresAt) {
                    notifyError("Device code expired. Please start the flow again.");
                    document.getElementById("oauth-step-1").classList.remove("hidden");
                    document.getElementById("oauth-step-2").classList.add("hidden");
                    return;
                }

                notifySuccess("Checking authorization...");

                while (Date.now() < expiresAt) {
                    try {
                        const response = await fetch("/api/oauth/copilot-exchange", {
                            method: "POST",
                            headers: { "Content-Type": "application/json" },
                            body: JSON.stringify({
                                provider_id: providerId,
                                device_code: deviceCode,
                            }),
                        });
                        if (!response.ok) throw new Error("Exchange request failed");
                        const data = await response.json();

                        if (data.status === "success") {
                            document
                                .getElementById("oauth-step-2")
                                .classList.add("hidden");
                            document
                                .getElementById("oauth-step-3")
                                .classList.remove("hidden");
                            sessionStorage.setItem("oauth_provider_id", providerId);
                            notifySuccess(
                                `GitHub Copilot authenticated! Token saved for ${providerId}`,
                            );
                            return;
                        } else if (data.status === "expired") {
                            notifyError(
                                "Device code expired. Please start the flow again.",
                            );
                            document
                                .getElementById("oauth-step-1")
                                .classList.remove("hidden");
                            document
                                .getElementById("oauth-step-2")
                                .classList.add("hidden");
                            return;
                        }
                        // status === "pending": server already waited 60s, loop continues
                    } catch (error) {
                        notifyError(`Exchange failed: ${error.message}`);
                        return;
                    }
                }

                notifyError("Authorization timed out. Please start the flow again.");
            }
```

### Change 6: Modify `completeOAuthFlow()` to dispatch for copilot

- [ ] **Step 6.1: Find `async function completeOAuthFlow()` (around line 4429)**

At the very start of the function body (after the opening `{` and before `try {`), add:

```javascript
            async function completeOAuthFlow() {
                // Dispatch to Copilot flow if device code is in session
                if (sessionStorage.getItem("copilot_device_code")) {
                    return completeCopilotFlow();
                }

                try {
                    // ... rest of existing completeOAuthFlow unchanged
```

### Change 7: Clear copilot session state on cancel

- [ ] **Step 7.1: Find `function cancelOAuthFlow()` (around line 4506)**

Add three `sessionStorage.removeItem` calls inside the existing `cancelOAuthFlow` function:

```javascript
            function cancelOAuthFlow() {
                // Reset to initial state
                document.getElementById("oauth-step-2").classList.add("hidden");
                document
                    .getElementById("oauth-step-1")
                    .classList.remove("hidden");
                document.getElementById("oauth-code-input").value = "";
                sessionStorage.removeItem("oauth_verifier");
                // Clear copilot device code state
                sessionStorage.removeItem("copilot_device_code");
                sessionStorage.removeItem("copilot_provider_id");
                sessionStorage.removeItem("copilot_expires_at");
                notifySuccess("OAuth flow canceled");
            }
```

- [ ] **Step 7.2: Build to verify HTML is still valid**

Run: `cargo build 2>&1 | grep -E "error|warning.*admin" | head -20`
Expected: no errors referencing admin.html

- [ ] **Step 7.3: Commit**

```bash
git add src/server/admin.html
git commit -m "feat: add GitHub Copilot device code flow to admin UI"
```

---

## Task 7: Config example and final verification

**Files:**
- Modify: `config/example.toml`

- [ ] **Step 1: Add copilot example to `config/example.toml`**

Find the commented provider example block near the bottom of the file and add after it:

```toml
# GitHub Copilot provider (authenticate via the admin UI — no API key needed)
# base_url is derived from the bearer token at runtime — do not set it.
# [[providers]]
# name = "copilot"
# provider_type = "copilot"
# auth_type = "oauth"
# oauth_provider = "copilot"
# models = ["gpt-4o", "claude-sonnet-4-5"]
```

- [ ] **Step 2: Run full test suite**

Run: `cargo test 2>&1 | tail -30`
Expected: all tests pass. Note any new failures and fix before committing.

- [ ] **Step 3: Run `cargo clippy` for lint**

Run: `cargo clippy 2>&1 | grep "^error" | head -20`
Expected: no errors (warnings are acceptable if pre-existing).

- [ ] **Step 4: Final commit**

```bash
git add config/example.toml
git commit -m "docs: add GitHub Copilot provider example to config"
```

---

## Self-Review

### Spec coverage check

| Spec requirement | Covered by |
|-----------------|-----------|
| `parse_proxy_ep` with `proxy.` prefix → `api.` prefix | Task 1, Step 2 |
| `parse_proxy_ep` missing field → fallback URL | Task 1, Step 2 tests |
| Device code flow: start → poll → exchange | Task 1 (`start_device_flow`, `poll_for_github_token`, `exchange_for_copilot_token`) |
| `slow_down` error increments interval by 5s | Task 1 (`poll_github_token_once`) |
| `expired_token` returns `PollResult::Expired` | Task 1 (`poll_github_token_once`) |
| Hard 60s timeout per `copilot-exchange` call | Task 5 (`poll_for_github_token(device_code, 5, 60)`) |
| 5 required Copilot headers on every request | Task 3 (`COPILOT_HEADERS` const) |
| Concurrent refresh mutex | Task 3 (`refresh_lock: Arc<tokio::sync::Mutex<()>>`) |
| Double-checked locking pattern | Task 3 (`get_valid_copilot_token`) |
| `OAuthToken` field mapping (access=bearer, refresh=github) | Task 5 (`copilot_exchange` handler) |
| `count_tokens` char-based estimate | Task 3 |
| `supports_model` matches models list | Task 3 |
| Registry `"copilot"` arm | Task 4 |
| `/api/oauth/copilot-start` endpoint | Task 5 |
| `/api/oauth/copilot-exchange` endpoint | Task 5 |
| `{status: "success|pending|expired"}` response shape | Task 5 |
| 7 admin UI changes | Task 6 (Changes 1-7) |
| sessionStorage cleanup on cancel | Task 6 (Change 7) |
| sessionStorage cleanup at start of new flow | Task 6 (Change 4) |
| config/example.toml commented example | Task 7 |
| No changes to existing OAuth handlers | No task modifies `oauth_authorize`/`oauth_exchange` handlers |
| `is_anthropic_compatible_provider` — no change needed | Explicitly excluded from spec |

All spec requirements are covered. No gaps found.
