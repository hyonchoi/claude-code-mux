# Design: Passthrough Authentication for Anthropic Providers

**Date:** 2026-05-06  
**Status:** Design approved, awaiting implementation plan  
**Related:** Architecture review locked in `/plan-eng-review`, user requirements from `/office-hours`

## Executive Summary

Enable Claude Code CLI and other clients to authenticate directly with Anthropic using passthrough tokens, bypassing the proxy's own API keys. Support Claude Code CLI's beta options (thinking, 1m-context, interleaved_thinking, etc.) with model-specific filtering to prevent unsupported options from being sent to the API.

**Core insight:** One `AnthropicProvider` with multiple auth modes (ApiKey vs Passthrough) scales better than separate provider types and keeps the routing logic centralized.

## Problem Statement

**Current state:**
- The proxy routes all requests through pre-configured provider API keys
- Claude Code CLI has its own Anthropic API credentials and wants to use them directly
- Claude Code CLI wants to use beta features (thinking, extended context) that aren't yet available to all model configurations
- The proxy needs to validate that clients don't request options their model doesn't support

**User requirements:**
1. Passthrough mode: client provides bearer token → proxy uses it instead of api_key
2. API-key mode: unchanged (existing behavior)
3. OAuth: remove entirely from Anthropic provider
4. Beta options: pass through from Claude Code CLI, filter for non-CLI clients, validate against model capabilities
5. Option filtering: server-level, based on model metadata, reject unsupported options with HTTP 400

## Architecture

### Three Authentication Modes

The `AnthropicProvider` supports three auth types via the `AuthType` enum:

```
pub enum AuthType {
    ApiKey,       // Proxy holds the API key (existing)
    Passthrough,  // Client provides bearer token in request (new)
    // OAuth removed
}
```

Each mode has distinct behavior:

| Mode | Who provides credentials | Provider uses | Typical client |
|------|--------------------------|----------------|-----------------|
| ApiKey | Admin (TOML config) | `config.api_key` | Standard apps, tools |
| Passthrough | Client (Authorization header) | Request `Bearer <token>` | Claude Code CLI, direct API callers |

**Key rule:** API-key and passthrough modes have zero shared authentication logic. The provider instantiation (in `registry.rs`) matches on `auth_type` and selects the correct code path.

### Request Flow Diagram

```
Client Request (Claude Code CLI)
  │
  ├─ Headers: User-Agent: "claude-code/x.y.z" or "ClaudeDesktop/x.y.z"
  ├─ Headers: Authorization: Bearer <token>
  └─ Headers: anthropic-beta: thinking, 1m-context
  │
  ↓
Server Handler (handle_messages or handle_openai_chat_completions)
  │
  ├─ 1. Detect CLI source via is_claude_code_cli_request()
  │      ✓ true if User-Agent contains "claude-code/" or "ClaudeDesktop/"
  │      ✓ false otherwise (treat as non-CLI)
  │
  ├─ 2. Apply passthrough auth via apply_passthrough_auth()
  │      ✓ Extract bearer token from Authorization header
  │      ✓ Set request.passthrough_auth = token
  │      ✓ Error if token validation fails
  │
  ├─ 3. Extract anthropic-beta header via extract_anthropic_beta_header()
  │      ✓ Returns ["thinking", "1m-context"] or [] or error
  │
  ├─ 4. Validate beta options via validate_beta_options()
  │      ✓ Check against model.supported_beta_options
  │      ✓ Return 400 if ANY unsupported option (strict validation)
  │      ✓ Return filtered list if all valid
  │
  ├─ 5. Filter providers via filter_to_anthropic_providers()
  │      ✓ Retain only Anthropic-capable providers
  │      ✓ Use HashSet for O(1) lookup (performance optimization)
  │      ✓ Error 503 if no anthropic providers available
  │
  ├─ 6. Route to AnthropicProvider
  │      ✓ If passthrough_token: use token instead of api_key
  │      ✓ If api_key: use api_key, ignore token
  │      ✓ Set anthropic_beta_header field with filtered options
  │
  └─ 7. Provider builds HTTP request to Anthropic API
         ✓ Authorization: Bearer <token or api_key>
         ✓ anthropic-beta: <filtered options header>
         ✓ Send to Anthropic API
         │
         ↓
Response back to client
```

### Component Changes

**New functions in `src/server/mod.rs`:**

1. **`is_claude_code_cli_request(headers: &HeaderMap) -> bool`**
   - Detects CLI clients by User-Agent header
   - Looks for: "claude-code/" or "ClaudeDesktop/" substring (case-insensitive)
   - Returns false if User-Agent missing (safe default: non-CLI)
   - Used to decide whether to unconditionally pass beta options

2. **`extract_anthropic_beta_header(headers: &HeaderMap) -> Result<Vec<String>, AppError>`**
   - Parses comma-separated list from `anthropic-beta` request header
   - Returns `Ok([])` if header missing
   - Returns `Err` if syntax invalid (e.g., "thinking,," or unclosed quotes)
   - Error message: "Invalid anthropic-beta header format: {details}"

3. **`validate_beta_options(requested: &[String], model: &ModelConfig) -> Result<Vec<String>, AppError>`**
   - Filters requested options against `model.supported_beta_options`
   - Uses **strict validation**: returns error if ANY option unsupported
   - Error message: `"Option '{option}' not supported for model '{model_name}'"`
   - Returns `Ok(filtered_list)` where filtered_list ⊆ requested (subset)
   - Logs both requested and filtered options for debugging

4. **`apply_passthrough_auth(request: &mut AnthropicRequest, headers: &HeaderMap) -> Result<(), AppError>`**
   - Calls existing `extract_bearer_token()` function (line 1020)
   - Sets `request.passthrough_auth = token`
   - Reuses existing token validation (8KB limit, safe character check)
   - Returns error if validation fails

5. **`filter_to_anthropic_providers(mappings: &mut Vec<ModelMapping>, providers: &[ProviderConfig]) -> Result<(), AppError>`**
   - Refactored from inline logic at lines 712-719 in current handle_messages
   - **Performance optimization:** Build HashSet of anthropic provider names once, do O(1) lookups (instead of O(n) per mapping)
   - Retains only mappings with anthropic-capable providers
   - Returns error if no providers remain (HTTP 503 Service Unavailable)

**Modified handlers in `src/server/mod.rs`:**

**`handle_messages()`** — main HTTP request entry point
```
1. apply_passthrough_auth() → extract bearer token
2. is_claude_code_cli_request() → detect CLI
3. IF (is_cli OR has_passthrough_token):
     a. extract_anthropic_beta_header() → parse options
     b. validate_beta_options() → filter against model.supported_beta_options
     c. request.anthropic_beta_header = filtered_options
4. filter_to_anthropic_providers() → route to anthropic-capable provider
5. Send request to selected provider
   - Provider uses passthrough_auth if present, else api_key
   - Provider includes anthropic_beta_header in HTTP request to Anthropic
```

**`handle_openai_chat_completions()`** — secondary entry point
- Same logic as handle_messages (applied to OpenAI-formatted requests routed to Anthropic)

**Data structure extensions:**

1. **`src/providers/mod.rs` — AuthType enum**
   ```rust
   pub enum AuthType {
       ApiKey,       // Existing: proxy holds api_key
       Passthrough,  // New: client provides bearer token
       // OAuth removed entirely
   }
   ```

2. **`src/providers/mod.rs` — ProviderConfig struct**
   ```rust
   pub struct ProviderConfig {
       // ... existing fields (name, auth_type, etc.)
       pub supported_beta_options: Vec<String>,  // NEW
       // Example: vec!["thinking".to_string(), "1m-context".to_string()]
   }
   ```

3. **`src/models/mod.rs` — AnthropicRequest struct**
   ```rust
   pub struct AnthropicRequest {
       // ... existing fields (messages, model, etc.)
       pub anthropic_beta_header: Option<String>,  // NEW
       // Example: Some("thinking, 1m-context")
       // Marked with #[serde(skip)] — not serialized to Anthropic JSON
   }
   ```

4. **`src/providers/registry.rs` — Provider instantiation**
   - Match on `auth_type` to select code path
   - For `AuthType::Passthrough`: use bearer token from request, ignore api_key field
   - For `AuthType::ApiKey`: use api_key from config, ignore request token

## Error Handling

### Strict Validation Errors (HTTP 400 Bad Request)

**Malformed anthropic-beta header:**
```
Client sends: anthropic-beta: "thinking,,"
Server returns: 400 Bad Request
Message: "Invalid anthropic-beta header format: unexpected empty option"
```

**Unsupported beta option:**
```
Client sends: anthropic-beta: thinking
Model supports: ["1m-context"]
Server returns: 400 Bad Request
Message: "Option 'thinking' not supported for model 'claude-3-opus'"
Logged: requested_options=["thinking"], supported_options=["1m-context"]
```

### Graceful Defaults (No Errors)

| Scenario | Behavior |
|----------|----------|
| Missing User-Agent header | Treated as non-CLI (standard default) |
| Missing anthropic-beta header | Treated as zero beta options requested |
| Missing Authorization header | Uses api_key if available (standard default) |
| Both bearer token AND api_key available | API-key wins, bearer token ignored |
| Empty bearer token | Rejected (existing validation in extract_bearer_token) |

### Service Errors (HTTP 503 Service Unavailable)

**No anthropic providers available:**
```
After filtering, no anthropic providers remain
Server returns: 503 Service Unavailable
Message: "No anthropic providers available for this request"
```

## Model Capabilities Metadata

**TOML configuration:**
```toml
[[models]]
name = "claude-3-opus-20250219"
provider = "anthropic-main"
supported_beta_options = ["thinking", "1m-context", "interleaved_thinking"]

[[models]]
name = "claude-3-sonnet-20250229"
provider = "anthropic-main"
supported_beta_options = ["1m-context"]
```

**Bootstrap at startup:**
1. Fetch available models from Anthropic API (`GET /v1/models`)
2. Extract `supported_beta_options` from API response
3. Merge with TOML config: **TOML overrides API** (allows pinning/overriding)
4. Store in `ModelConfig` (single source of truth)

**Failure handling:**
- If API fetch fails: continue with TOML-only metadata (graceful degradation)
- If TOML missing `supported_beta_options`: default to empty list (no beta options allowed)

## Testing Strategy

### Unit Tests

**`is_claude_code_cli_request()` — 6 cases**
- "claude-code/1.0.0" → true
- "claude-code/2.3.4-beta" → true
- "ClaudeDesktop/1.2.3" → true
- "my-app/1.0" → false
- Missing User-Agent header → false
- Case variations ("CLAUDE-CODE", "claudedesktop") → true (case-insensitive)

**`extract_anthropic_beta_header()` — 4 cases**
- Valid: "thinking, 1m-context" → `["thinking", "1m-context"]`
- Missing header → `[]`
- Malformed: "thinking,," → Error
- Single option: "thinking" → `["thinking"]`

**`validate_beta_options()` — 3 cases**
- All supported: requested=["thinking"], supported=["thinking", "1m-context"] → `["thinking"]`
- Unsupported: requested=["thinking"], supported=["1m-context"] → Error 400
- Empty list: requested=[] → `[]`

**`apply_passthrough_auth()` — 3 cases**
- Valid bearer token → sets passthrough_auth field
- Invalid token format → Error
- Missing Authorization header → passthrough_auth = None

**`filter_to_anthropic_providers()` — 2 cases**
- Mixed providers → retains only anthropic-capable
- No anthropic providers → Error 503

### Integration/E2E Tests

**User flows to verify:**

1. **Claude Code CLI with passthrough + beta options**
   - Request headers: User-Agent: "claude-code/1.0", Authorization: Bearer <token>, anthropic-beta: thinking
   - Expected: Request succeeds, Anthropic API receives thinking option

2. **Non-CLI client with bearer token** (should NOT get beta options)
   - Request headers: User-Agent: "my-app/1.0", Authorization: Bearer <token>, anthropic-beta: thinking
   - Expected: Beta option silently dropped, request succeeds without beta

3. **API-key auth with bearer token present** (API-key takes precedence)
   - Provider is api_key type, client sends bearer token
   - Expected: Request uses api_key, bearer token ignored, request succeeds

4. **Error: unsupported beta option**
   - Request: anthropic-beta: thinking, model doesn't support it
   - Expected: HTTP 400 with message "Option 'thinking' not supported for model '{model}'"

5. **Error: no anthropic providers available**
   - All non-anthropic providers match, after filtering no anthropic remains
   - Expected: HTTP 503 "No anthropic providers available for this request"

6. **Regression: existing non-passthrough requests**
   - Standard API-key request without bearer token
   - Expected: Request succeeds via api_key (unchanged behavior)

### Coverage Goals

- **Unit test coverage:** 100% of new functions and error paths
- **Integration test coverage:** All user-facing error messages verified
- **Regression test coverage:** Existing auth modes (API-key, non-passthrough) unchanged
- **Model capability test:** Verify filtering works for various option combinations

## Deployment & Rollout

**Prerequisites:**
- TOML config updated with `supported_beta_options` per model
- Anthropic API endpoint for fetching models confirmed (`/v1/models`)
- Claude Code CLI User-Agent format verified ("claude-code/x.y.z" or "ClaudeDesktop/x.y.z")

**Release process:**
1. Deploy code with new helper functions and validation logic
2. Update TOML config to include `supported_beta_options` for each model
3. Enable passthrough auth in ProviderConfig (set `auth_type = Passthrough` for client-facing providers)
4. Monitor logs for:
   - First passthrough requests arriving (log: "🔑 Passthrough mode detected")
   - Beta option filtering (log: "filtered beta options: {options}")
   - Any validation errors (log: unsupported options, malformed headers)

**Rollback plan:**
- Remove `auth_type = Passthrough` from TOML config (revert to API-key only)
- Clients fall back to using proxy's API keys
- No code rollback needed (helper functions are dormant if not used)

## Out of Scope

1. **Streaming support changes** — passthrough respects existing streaming logic
2. **Admin UI for model capabilities** — TOML editing only, UI updates deferred
3. **OAuth removal from other providers** — only Anthropic provider affected
4. **P1-P3 TODOS** — deferred to parallel work (rollback control, observability, etc.)
5. **Rate limiting per auth type** — existing per-key limits apply, no new limits
6. **Observability/metrics** — logging added, structured metrics deferred
7. **Multi-region deployment** — passthrough works in current single-region setup

## Assumptions & Dependencies

**Verified assumptions:**
- User-Agent format: "claude-code/x.y.z" or "ClaudeDesktop/x.y.z" ✓ (confirmed by user)
- Strict validation: HTTP 400 if unsupported option ✓ (user approved D11)
- Single auth mode per provider ✓ (architectural decision D1)

**External dependencies:**
- Anthropic API `/v1/models` endpoint exists and returns `supported_beta_options` (will verify in implementation)
- Bearer token format matches existing `extract_bearer_token()` validation (existing code)

**Known quirks:**
- Current code has `passthrough_auth` field in AnthropicRequest (pre-existing) — will reuse
- Current code has passthrough filtering at lines 712-719 — will refactor into helper
- OAuth was placeholder in Anthropic provider — safe to remove entirely
