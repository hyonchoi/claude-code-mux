# Claude OAuth Passthrough Relay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When an incoming request carries `Authorization: Bearer <token>`, preserve that token as the upstream auth credential and restrict fallback to `"anthropic"`-type providers only.

**Architecture:** Add `passthrough_auth: Option<String>` to `AnthropicRequest` (skipped during serialization). Handlers extract the bearer token from the incoming `Authorization` header and set this field. Providers check the field in `get_auth_header` and use it in place of internal credentials. The handler filters provider mappings to `provider_type == "anthropic"` when passthrough is active.

**Tech Stack:** Rust, Axum, reqwest, serde, async-trait

---

### Task 1: Add `passthrough_auth` to `AnthropicRequest`

**Files:**
- Modify: `src/models/mod.rs:6-28`
- Modify: `src/router/mod.rs:253-269` (test helper)
- Modify: `src/server/mod.rs:806-819` (count_tokens routing request)
- Modify: `src/server/openai_compat.rs:202-215` (transform function)

- [ ] **Step 1: Add the field to the struct**

In `src/models/mod.rs`, add after the `tools` field (line 27):

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Caller-provided bearer token for passthrough mode. Never serialized.
    #[serde(skip)]
    pub passthrough_auth: Option<String>,
}
```

- [ ] **Step 2: Fix the router test helper**

In `src/router/mod.rs`, add `passthrough_auth: None` to `create_simple_request`:

```rust
    fn create_simple_request(text: &str) -> AnthropicRequest {
        AnthropicRequest {
            model: "claude-opus-4".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text(text.to_string()),
            }],
            max_tokens: 1024,
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
        }
    }
```

- [ ] **Step 3: Fix count_tokens routing request construction**

In `src/server/mod.rs`, add `passthrough_auth: None` to the `AnthropicRequest` literal at line 806:

```rust
    let mut routing_request = AnthropicRequest {
        model: count_request.model.clone(),
        messages: count_request.messages.clone(),
        max_tokens: 1024,
        system: count_request.system.clone(),
        tools: count_request.tools.clone(),
        thinking: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: None,
        stream: None,
        metadata: None,
        passthrough_auth: None,
    };
```

- [ ] **Step 4: Fix the OpenAI-to-Anthropic transform**

In `src/server/openai_compat.rs`, add `passthrough_auth: None` to the `AnthropicRequest` literal returned by `transform_openai_to_anthropic` (around line 202):

```rust
    Ok(AnthropicRequest {
        model: openai_req.model,
        messages,
        max_tokens: openai_req.max_tokens.unwrap_or(4096),
        thinking: None,
        temperature: openai_req.temperature,
        top_p: openai_req.top_p,
        top_k: None,
        stop_sequences: openai_req.stop,
        stream: openai_req.stream,
        metadata: None,
        system: system_prompt,
        tools: None,
        passthrough_auth: None,
    })
```

- [ ] **Step 5: Verify it compiles**

```bash
cargo build 2>&1
```

Expected: no errors. All existing struct literals now compile.

- [ ] **Step 6: Run existing tests**

```bash
cargo test 2>&1
```

Expected: all existing tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/models/mod.rs src/router/mod.rs src/server/mod.rs src/server/openai_compat.rs
git commit -m "feat: add passthrough_auth field to AnthropicRequest"
```

---

### Task 2: Override auth in `AnthropicCompatibleProvider`

**Files:**
- Modify: `src/providers/anthropic_compatible.rs:71-116` (`get_auth_header`)
- Modify: `src/providers/anthropic_compatible.rs:202-266` (`send_message`)
- Modify: `src/providers/anthropic_compatible.rs:268-351` (`count_tokens`)
- Modify: `src/providers/anthropic_compatible.rs:353-415` (`send_message_stream`)

- [ ] **Step 1: Write the failing unit test**

Add this test module at the end of `src/providers/anthropic_compatible.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify the test fails**

```bash
cargo test -p claude-code-mux anthropic_compatible::tests 2>&1
```

Expected: compile error — `get_auth_header` does not accept arguments yet.

- [ ] **Step 3: Update `get_auth_header` signature**

Replace the existing `get_auth_header` method (lines 71–116 of `src/providers/anthropic_compatible.rs`) with:

```rust
    /// Get authentication header value. override_auth takes highest priority.
    async fn get_auth_header(&self, override_auth: Option<&str>) -> Result<String, ProviderError> {
        if let Some(token) = override_auth {
            return Ok(token.to_string());
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
```

- [ ] **Step 4: Update `send_message` callsite**

In `send_message` (around line 202), replace:

```rust
        let auth_value = self.get_auth_header().await?;
```

with:

```rust
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;
```

And replace the `if self.is_oauth()` block with:

```rust
        if override_auth.is_some() || self.is_oauth() {
            req_builder = req_builder
                .header("Authorization", format!("Bearer {}", auth_value))
                .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14");
            tracing::debug!("🔐 Using OAuth Bearer token for {}", self.name);
        } else {
            req_builder = req_builder.header("x-api-key", auth_value);
        }
```

- [ ] **Step 5: Update `count_tokens` callsite**

In `count_tokens` (around line 274), the method receives a `CountTokensRequest`, not an `AnthropicRequest`. Pass `None` here — count_tokens does not carry passthrough_auth:

```rust
        let auth_value = self.get_auth_header(None).await?;
```

And the `if self.is_oauth()` block stays unchanged (no passthrough for count_tokens in this path).

- [ ] **Step 6: Update `send_message_stream` callsite**

In `send_message_stream` (around line 362), replace:

```rust
        let auth_value = self.get_auth_header().await?;
```

with:

```rust
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;
```

And replace the `if self.is_oauth()` block with:

```rust
        if override_auth.is_some() || self.is_oauth() {
            req_builder = req_builder
                .header("Authorization", format!("Bearer {}", auth_value))
                .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14");
            tracing::debug!("🔐 Using OAuth Bearer token for streaming on {}", self.name);
        } else {
            req_builder = req_builder.header("x-api-key", auth_value);
        }
```

- [ ] **Step 7: Run the tests**

```bash
cargo test -p claude-code-mux anthropic_compatible::tests 2>&1
```

Expected: both tests pass.

- [ ] **Step 8: Run all tests**

```bash
cargo test 2>&1
```

Expected: all tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/providers/anthropic_compatible.rs
git commit -m "feat: add passthrough_auth override to AnthropicCompatibleProvider"
```

---

### Task 3: Override auth in `OpenAIProvider`

**Files:**
- Modify: `src/providers/openai.rs:504-550` (`get_auth_header`)
- Modify: `src/providers/openai.rs:835+` (`send_message` callsite)
- Modify: `src/providers/openai.rs:1050+` (`send_message_stream` callsite)

- [ ] **Step 1: Write the failing unit test**

Add this test module at the end of `src/providers/openai.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify the test fails**

```bash
cargo test -p claude-code-mux openai::tests 2>&1
```

Expected: compile error — `get_auth_header` does not accept arguments yet.

- [ ] **Step 3: Update `get_auth_header` signature**

Replace the existing `get_auth_header` method (lines 504–550 of `src/providers/openai.rs`) with:

```rust
    /// Get authentication header value. override_auth takes highest priority.
    async fn get_auth_header(&self, override_auth: Option<&str>) -> Result<String, ProviderError> {
        if let Some(token) = override_auth {
            return Ok(token.to_string());
        }

        if let Some(ref oauth_provider_id) = self.oauth_provider {
            if let Some(ref token_store) = self.token_store {
                if let Some(token) = token_store.get(oauth_provider_id) {
                    if token.needs_refresh() {
                        tracing::info!("🔄 Token for '{}' needs refresh, refreshing...", oauth_provider_id);
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
```

- [ ] **Step 4: Update `send_message` callsite**

In `send_message` (around line 837), replace:

```rust
        let auth_value = self.get_auth_header().await?;
```

with:

```rust
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;
```

- [ ] **Step 5: Update `send_message_stream` callsite**

In `send_message_stream` (around line 1057), replace:

```rust
        let auth_value = self.get_auth_header().await?;
```

with:

```rust
        let override_auth = request.passthrough_auth.as_deref();
        let auth_value = self.get_auth_header(override_auth).await?;
```

- [ ] **Step 6: Run the tests**

```bash
cargo test -p claude-code-mux openai::tests 2>&1
```

Expected: both tests pass.

- [ ] **Step 7: Run all tests**

```bash
cargo test 2>&1
```

Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/providers/openai.rs
git commit -m "feat: add passthrough_auth override to OpenAIProvider"
```

---

### Task 4: Passthrough detection and mapping filter in handlers

**Files:**
- Modify: `src/server/mod.rs` (`handle_messages`, `handle_openai_chat_completions`)

This task adds a helper function and two changes to each handler: (a) extract bearer token, (b) filter and guard the mapping list.

- [ ] **Step 1: Write failing unit test for the provider-type filter helper**

Add this test module in `src/server/mod.rs` (at the bottom of the file, before the last `}`):

```rust
#[cfg(test)]
mod tests {
    use crate::providers::ProviderConfig;
    use super::is_anthropic_provider;

    fn make_configs() -> Vec<ProviderConfig> {
        vec![
            ProviderConfig {
                name: "ant1".to_string(),
                provider_type: "anthropic".to_string(),
                auth_type: Default::default(),
                api_key: Some("k".to_string()),
                oauth_provider: None,
                project_id: None,
                location: None,
                base_url: None,
                models: vec![],
                enabled: Some(true),
            },
            ProviderConfig {
                name: "oai1".to_string(),
                provider_type: "openai".to_string(),
                auth_type: Default::default(),
                api_key: Some("k".to_string()),
                oauth_provider: None,
                project_id: None,
                location: None,
                base_url: None,
                models: vec![],
                enabled: Some(true),
            },
        ]
    }

    #[test]
    fn test_is_anthropic_provider_returns_true_for_anthropic_type() {
        let configs = make_configs();
        assert!(is_anthropic_provider(&configs, "ant1"));
    }

    #[test]
    fn test_is_anthropic_provider_returns_false_for_openai_type() {
        let configs = make_configs();
        assert!(!is_anthropic_provider(&configs, "oai1"));
    }

    #[test]
    fn test_is_anthropic_provider_returns_false_for_unknown_name() {
        let configs = make_configs();
        assert!(!is_anthropic_provider(&configs, "unknown"));
    }
}
```

- [ ] **Step 2: Run to verify the test fails**

```bash
cargo test -p claude-code-mux server::tests 2>&1
```

Expected: compile error — `is_anthropic_provider` is not defined yet.

- [ ] **Step 3: Add the helper function**

Add this function in `src/server/mod.rs`, just before `handle_openai_chat_completions` (around line 461):

```rust
/// Returns true if the named provider has provider_type == "anthropic".
fn is_anthropic_provider(providers: &[crate::providers::ProviderConfig], name: &str) -> bool {
    providers
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.provider_type == "anthropic")
        .unwrap_or(false)
}
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p claude-code-mux server::tests 2>&1
```

Expected: all three filter tests pass.

- [ ] **Step 5: Update `handle_messages` — extract bearer token**

In `handle_messages` (around line 603), after extracting `model` and before parsing the request, add:

```rust
    // Extract caller-provided bearer token for passthrough mode
    let passthrough_token: Option<String> = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    if passthrough_token.is_some() {
        info!("🔑 Passthrough mode detected (caller-provided bearer token)");
    }
```

- [ ] **Step 6: Update `handle_messages` — set `passthrough_auth` on routing request**

After the routing request is parsed (around line 620), add:

```rust
    request_for_routing.passthrough_auth = passthrough_token.clone();
```

- [ ] **Step 7: Update `handle_messages` — filter mappings and guard empty list**

In the model-config branch of `handle_messages` (around line 650), after `sorted_mappings` is built and before the provider-override filter, add the anthropic-type filter for passthrough mode:

```rust
        // In passthrough mode, restrict to anthropic-type providers only
        if passthrough_token.is_some() {
            sorted_mappings.retain(|m| is_anthropic_provider(&state.config.providers, &m.provider));
            if sorted_mappings.is_empty() {
                return Err(AppError::RoutingError(
                    "No anthropic-type provider mappings available for passthrough request".to_string()
                ));
            }
        }
```

Place this block after the `sorted_mappings.sort_by_key(|m| m.priority);` line and before the loop.

- [ ] **Step 8: Update `handle_messages` — set `passthrough_auth` on per-iteration request**

Inside the fallback loop in `handle_messages`, where `anthropic_request` is parsed from `request_json` (around line 684), add after the parse:

```rust
                // Propagate passthrough auth into per-provider request
                anthropic_request.passthrough_auth = passthrough_token.clone();
```

- [ ] **Step 9: Update `handle_messages` — guard the direct-registry fallback path**

In the `else` branch (no model config found, around line 756), before calling `get_provider_for_model`, add:

```rust
        // Passthrough requires explicit model mappings to enforce provider-type filtering
        if passthrough_token.is_some() {
            return Err(AppError::RoutingError(
                "Passthrough auth requires explicit [[models]] configuration".to_string()
            ));
        }
```

- [ ] **Step 10: Update `handle_openai_chat_completions` — same changes**

Apply the identical pattern to `handle_openai_chat_completions` (around line 462):

After the model name extraction, add bearer extraction:

```rust
    let passthrough_token: Option<String> = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    if passthrough_token.is_some() {
        info!("🔑 Passthrough mode detected (caller-provided bearer token)");
    }
```

After `transform_openai_to_anthropic`, set the field:

```rust
    anthropic_request.passthrough_auth = passthrough_token.clone();
```

After `sorted_mappings.sort_by_key(|m| m.priority);`, add the filter+guard:

```rust
        if passthrough_token.is_some() {
            sorted_mappings.retain(|m| is_anthropic_provider(&state.config.providers, &m.provider));
            if sorted_mappings.is_empty() {
                return Err(AppError::RoutingError(
                    "No anthropic-type provider mappings available for passthrough request".to_string()
                ));
            }
        }
```

In the direct-registry fallback `else` branch, add the guard:

```rust
        if passthrough_token.is_some() {
            return Err(AppError::RoutingError(
                "Passthrough auth requires explicit [[models]] configuration".to_string()
            ));
        }
```

- [ ] **Step 11: Update `handle_messages` — add the passthrough log line**

Inside the fallback loop, after `anthropic_request.passthrough_auth = passthrough_token.clone();` (Step 8), add:

```rust
                if passthrough_token.is_some() {
                    info!("🔑 Passthrough auth active: original_model={}, target_provider={}", original_model, mapping.provider);
                }
```

- [ ] **Step 12: Build and run all tests**

```bash
cargo build 2>&1
cargo test 2>&1
```

Expected: builds cleanly, all tests pass.

- [ ] **Step 13: Commit**

```bash
git add src/server/mod.rs
git commit -m "feat: detect passthrough bearer token and filter to anthropic-type providers"
```

---

### Task 5: Passthrough-specific integration tests

**Files:**
- Modify: `src/server/mod.rs` (extend `tests` module from Task 4)

These tests verify the filter behavior and model-rewrite trace using in-process logic only — no live HTTP.

- [ ] **Step 1: Write the provider-filter integration test**

In the `tests` module already added to `src/server/mod.rs` in Task 4, add:

```rust
    #[test]
    fn test_passthrough_filter_excludes_non_anthropic_mappings() {
        use crate::cli::ModelMapping;

        let configs = make_configs(); // ant1=anthropic, oai1=openai

        let mappings = vec![
            ModelMapping { provider: "ant1".to_string(), actual_model: "claude-opus-4-5".to_string(), priority: 1 },
            ModelMapping { provider: "oai1".to_string(), actual_model: "gpt-4o".to_string(), priority: 2 },
        ];

        let filtered: Vec<_> = mappings
            .into_iter()
            .filter(|m| is_anthropic_provider(&configs, &m.provider))
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider, "ant1");
    }

    #[test]
    fn test_passthrough_filter_empty_when_no_anthropic_mappings() {
        use crate::cli::ModelMapping;

        let configs = make_configs(); // ant1=anthropic, oai1=openai

        let mappings = vec![
            ModelMapping { provider: "oai1".to_string(), actual_model: "gpt-4o".to_string(), priority: 1 },
        ];

        let filtered: Vec<_> = mappings
            .into_iter()
            .filter(|m| is_anthropic_provider(&configs, &m.provider))
            .collect();

        assert!(filtered.is_empty());
    }
```

- [ ] **Step 2: Check `ModelMapping` struct path**

Verify the import path for `ModelMapping`:

```bash
grep -rn "struct ModelMapping\|pub struct ModelMapping" /Users/hyonchoi/Work/claude-code-mux/src/
```

Expected: should be in `src/cli/mod.rs`. Adjust the import in the test if the path differs.

- [ ] **Step 3: Run the tests**

```bash
cargo test -p claude-code-mux server::tests 2>&1
```

Expected: all five tests pass (3 from Task 4 + 2 new).

- [ ] **Step 4: Run all tests**

```bash
cargo test 2>&1
```

Expected: all tests pass with no regressions.

- [ ] **Step 5: Verify model rewrite trace (manual)**

The `original_model` restoration is existing behavior already in the mapping loop. Verify it works end-to-end with passthrough by running:

```bash
cargo run -- start &
curl -s -X POST http://localhost:3000/v1/messages \
  -H "Authorization: Bearer sk-test-passthrough" \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-opus-4","messages":[{"role":"user","content":"hi"}],"max_tokens":10}' \
  | jq '.model'
```

Expected output: `"claude-opus-4"` (the original model name, not the remapped internal name). Also check logs for: `🔑 Passthrough auth active: original_model=claude-opus-4`.

- [ ] **Step 6: Commit**

```bash
git add src/server/mod.rs
git commit -m "test: add passthrough provider-filter tests"
```
