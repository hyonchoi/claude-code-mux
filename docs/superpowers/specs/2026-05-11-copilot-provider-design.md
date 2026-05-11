# Spec: GitHub Copilot Provider with OAuth (Device Code Flow)

Date: 2026-05-11
Branch: fork/main
Source: gstack office-hours design (hyonchoi-fork-main-design-20260511-140104.md)

---

## Goal

Add a `copilot` provider type to claude-code-mux that lets GitHub Copilot subscribers route
requests through `api.individual.githubcopilot.com` using their existing subscription —
no separate API key required. Claude Code (and any Anthropic-API-compatible client) sends
requests to mux; mux authenticates via GitHub's device code flow and proxies to Copilot's
OpenAI-compatible endpoint.

---

## Architecture

Two new source files, six modified files.

### New files

| File | Purpose |
|------|---------|
| `src/auth/github_copilot.rs` | Device code flow, GitHub token polling, Copilot bearer token refresh, proxy-ep URL parsing |
| `src/providers/copilot.rs` | `CopilotProvider` implementing `AnthropicProvider` trait, header injection, request routing |

### Modified files

| File | Change |
|------|--------|
| `src/auth/mod.rs` | Re-export `github_copilot` module |
| `src/providers/mod.rs` | Re-export `copilot` module |
| `src/providers/registry.rs` | Add `"copilot"` match arm; bypass api_key extraction block |
| `src/server/oauth_handlers.rs` | Add `POST /api/oauth/copilot-start` and `POST /api/oauth/copilot-exchange` |
| `src/server/admin.html` | 7 targeted changes for device code modal (see Admin UI section) |
| `config/example.toml` | Add commented copilot provider example |

---

## Authentication Flow

Three stages — GitHub OAuth device code → GitHub access token → Copilot bearer token.

### Stage 1: Start device flow (admin action)

Admin clicks "Start OAuth" in the admin UI.

`POST /api/oauth/copilot-start { provider_id: string }`

Server calls `POST https://github.com/login/device/code` with client_id.
Returns: `{ device_code, user_code, verification_uri, expires_in, interval }`.
Nothing is stored server-side at this point.

### Stage 2: User authorizes on GitHub

Admin UI shows `user_code` and auto-opens `verification_uri` in a popup window.
User visits GitHub, enters the code, and authorizes.

### Stage 3: Exchange (admin clicks "I've Authorized")

`POST /api/oauth/copilot-exchange { provider_id: string, device_code: string }`

Server polls GitHub's token endpoint for up to 60 seconds (hard timeout per call).
- `authorization_pending` → sleep `interval`, continue
- `slow_down` → `interval += 5s`, sleep, continue
- `expired_token` → return `{ status: "expired" }` — admin UI must restart the flow
- Timeout (60s elapsed) → return `{ status: "pending" }` — admin UI re-calls this endpoint
- Success → exchange GitHub access token for Copilot bearer token, save `OAuthToken`, return `{ status: "success", provider_id }`

The admin UI polls `copilot-exchange` until it receives `"success"` or `"expired"`.

---

## Data Flow: Per-Request Execution

```
Client (Claude Code)
  → mux POST /v1/messages (Anthropic format)
      → CopilotProvider.send_message()
          → get_valid_copilot_token()   // check expiry; refresh if needed (with mutex)
          → parse_proxy_ep(bearer)      // extract base URL from token
          → convert AnthropicRequest → OpenAIRequest (reuse openai.rs logic)
          → inject 5 required headers
          → POST {base_url}/chat/completions
          → convert OpenAIResponse → AnthropicResponse
  → Client receives Anthropic-format response
```

Streaming path is identical but uses `send_message_stream()` and `OpenAIProvider::parse_sse_response`.

---

## Token Storage

Uses existing `OAuthToken` struct — no schema changes required.

```
OAuthToken {
  provider_id:    config.name,             // e.g., "copilot" — TokenStore key
  access_token:   <copilot_bearer>,        // short-lived (~30 min)
  refresh_token:  <github_oauth_token>,    // long-lived GitHub access token
  expires_at:     <bearer_token_expiry>,
  enterprise_url: None,
  project_id:     None,
}
```

On refresh: update `access_token` and `expires_at` only. `refresh_token` and `provider_id` remain unchanged. Save via `token_store.save()` (full overwrite, same key).

### Concurrent refresh guard

`CopilotProvider` holds `Arc<tokio::sync::Mutex<()>> refresh_lock`.

`get_valid_copilot_token()`:
1. Check token expiry without lock (fast path — avoids contention on every request)
2. If expired: acquire `refresh_lock`
3. Re-check expiry (second waiter skips refresh if first already refreshed)
4. Call `refresh_copilot_token()` via `GET https://api.github.com/copilot_internal/v2/token`

Refresh request headers: `Authorization: Bearer {github_oauth_token}`, `Editor-Version: vscode/1.107.0`, `Copilot-Integration-Id: vscode-chat`, `User-Agent: GitHubCopilotChat/0.35.0`.

---

## Base URL Extraction

Parse from bearer token on each request (cheap string scan, no extra storage).

Token format: `tid=...;exp=...;proxy-ep=proxy.individual.githubcopilot.com;...`

```rust
fn parse_proxy_ep(bearer: &str) -> String {
    for field in bearer.split(';') {
        if let Some(val) = field.strip_prefix("proxy-ep=") {
            let api_host = val
                .strip_prefix("proxy.")
                .map(|s| format!("api.{}", s))
                .unwrap_or_else(|| val.to_string());
            return format!("https://{}", api_host);
        }
    }
    "https://api.individual.githubcopilot.com".to_string()
}
```

---

## Required Per-Request Headers

Injected by `CopilotProvider` on every request:

```
Editor-Version: vscode/1.107.0
Editor-Plugin-Version: copilot-chat/0.35.0
Copilot-Integration-Id: vscode-chat
Openai-Intent: conversation-edits
X-Initiator: user
```

`X-Initiator` is always `"user"` because the Anthropic API requires requests to end with a user message.

---

## Registry Wiring

New match arm in `ProviderRegistry::from_configs()`:

```rust
"copilot" => Box::new(CopilotProvider::new(
    config.name.clone(),
    config.models.clone(),
    token_store.clone(),
)),
```

The `"copilot"` arm is reached only when `auth_type = OAuth`, so `api_key` is already set
to `config.oauth_provider.clone().unwrap_or_else(|| config.name.clone())` by the existing
OAuth branch — no special bypass needed. `token_store` lookup key = `config.name`.

---

## New API Endpoints

Both endpoints are new routes — they do NOT reuse `OAuthAuthorizeRequest` / `OAuthExchangeRequest` structs and do NOT modify existing `/api/oauth/authorize` or `/api/oauth/exchange` handlers.

### `POST /api/oauth/copilot-start`

Request: `{ provider_id: string }`
Response: `{ user_code, verification_uri, device_code, expires_in, interval }`

Calls GitHub device code endpoint. Returns all fields needed by the admin UI.

### `POST /api/oauth/copilot-exchange`

Request: `{ provider_id: string, device_code: string }`
Response:
- `{ status: "success", provider_id }` — authenticated, token saved
- `{ status: "pending" }` — 60s elapsed, user hasn't authorized yet; admin UI re-calls
- `{ status: "expired" }` — GitHub returned `expired_token`; admin UI must restart

---

## Admin UI Changes (7 changes to `src/server/admin.html`)

1. **Add Copilot radio** to provider type picker (after Gemini radio, ~line 708)
2. **`updateOAuthLabel()`** — add `copilot` branch: update label, description, step instructions
3. **`startOAuthFlow()`** — early-return for `providerType === "copilot"` → call `startCopilotFlow()`
4. **Add `startCopilotFlow()`** — calls `/api/oauth/copilot-start`, shows `user_code`, auto-opens `verification_uri` popup, stores `{device_code, provider_id, expires_at}` in `sessionStorage`, rewires "Complete OAuth" button to `completeCopilotFlow`
5. **Add `completeCopilotFlow()`** — polls `/api/oauth/copilot-exchange` in a loop until `success` or `expired`; shows progress via `notifySuccess`
6. **`completeOAuthFlow()`** — dispatch to `completeCopilotFlow()` if `copilot_device_code` in sessionStorage
7. **`cancelOAuthFlow()`** (and top of `startCopilotFlow()`) — clear `copilot_device_code`, `copilot_provider_id`, `copilot_expires_at` from sessionStorage

---

## Config Shape

```toml
[[providers]]
name = "copilot"
provider_type = "copilot"
auth_type = "oauth"
oauth_provider = "copilot"
models = ["gpt-4o", "claude-sonnet-4-5"]
# base_url is derived from the bearer token at runtime — do not set
```

---

## Implementation Notes

### Reused from existing codebase

- `OpenAIRequest` / `OpenAIResponse` structs (`src/providers/openai.rs`)
- `OpenAIProvider::parse_sse_response` for streaming
- `AnthropicRequest` → OpenAI conversion logic from `openai.rs`
- `TokenStore` (no schema changes)
- `tiktoken_rs` for `count_tokens` (already in `Cargo.toml`)
- `ProviderRegistry::from_configs` OAuth branch for api_key handling

### `count_tokens` implementation

Delegate to tiktoken-based estimation identical to `OpenAIProvider`. Returns approximate count.

### `supports_model` implementation

Match against the `models` list from `ProviderConfig`. Return `true` if the model name is in the list.

### `is_anthropic_compatible_provider` in `src/server/mod.rs`

No change needed. This function only matches `"anthropic"` and `"nvidia-nim"` (passthrough-mode providers). Copilot uses OAuth, not passthrough, so it correctly returns `false`.

---

## Test Coverage

### Unit tests (in source files)

| Test | Location |
|------|---------|
| `parse_proxy_ep` — standard case (`proxy.individual...` → `api.individual...`) | `src/auth/github_copilot.rs` |
| `parse_proxy_ep` — missing `proxy-ep` field → fallback URL | `src/auth/github_copilot.rs` |
| `parse_proxy_ep` — non-`proxy.` prefix passthrough | `src/auth/github_copilot.rs` |
| `CopilotProvider` header injection — all 5 headers present on mock request | `src/providers/copilot.rs` |
| `poll_for_github_token` — `slow_down` increments interval | `src/auth/github_copilot.rs` |
| `poll_for_github_token` — `expired_token` returns `Err` | `src/auth/github_copilot.rs` |

### Integration tests

| Test | Location |
|------|---------|
| `POST /api/oauth/copilot-start` returns `{ user_code, verification_uri, device_code, expires_in, interval }` shape | `tests/` |
| `POST /api/oauth/copilot-exchange` returns `{ status: "pending" }` on 60s timeout (mock GitHub) | `tests/` |

---

## Out of Scope

- Enterprise GitHub (company.ghe.com)
- Calling Copilot `/models` API on login (use `ProviderConfig.models` list)
- Per-request `Openai-Intent` variation (always `conversation-edits`)
- SSE-based auth status endpoint

---

## Success Criteria

1. `cargo build` passes with no errors after adding `copilot` to provider registry
2. Admin UI completes the device code flow: click "Start OAuth" → see `user_code` → authorize on GitHub → provider saved
3. A Claude Code request routed to a `copilot` provider hits `api.individual.githubcopilot.com/chat/completions` with all 5 required headers
4. Bearer token auto-refreshes at ~30 min without re-authentication
