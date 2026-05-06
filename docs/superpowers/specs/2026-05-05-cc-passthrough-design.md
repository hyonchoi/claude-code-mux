# Spec: Claude OAuth Passthrough Relay

Date: 2026-05-05
Branch: fork/cc-passthrough
Status: APPROVED

## Goal

Requests arriving at the relay with a caller-provided `Authorization: Bearer <token>` header must preserve that token end-to-end when forwarding to upstream providers. The router may still rewrite the model name. Fallback behavior is unchanged from the non-passthrough path. Cross-provider fallback (e.g., anthropic → openai) is blocked for passthrough requests.

## Architecture

### Detection

In `handle_messages` and `handle_openai_chat_completions`, inspect the incoming `Authorization` header. If it starts with `Bearer `, extract the token string. Presence of a bearer token is the passthrough signal — no other opt-in mechanism is needed.

### Request model (`src/models/mod.rs`)

Add one field to `AnthropicRequest`:

```rust
#[serde(skip)]
pub passthrough_auth: Option<String>,
```

`#[serde(skip)]` ensures it is never serialized into the outgoing request body. It exists only for the lifetime of a single relay request.

### Handler changes (`src/server/mod.rs`)

After extracting the bearer token:

1. Set `passthrough_auth` on the request before passing it to the provider.
2. Filter `sorted_mappings` to providers whose `provider_type == "anthropic"` (looked up via `state.config.providers`). Non-anthropic mappings are skipped silently.
3. If the filtered list is empty, return `RoutingError("No anthropic-type provider mappings available for passthrough request")` immediately — do not enter the fallback loop.
4. In the direct-registry fallback path (no model config), if passthrough is active and the found provider is not anthropic-type, return a `ProviderError` rather than forwarding.

### Provider auth override (`src/providers/anthropic_compatible.rs`, `src/providers/openai.rs`)

`get_auth_header` gains an `override_auth: Option<&str>` parameter. At each callsite within the provider, pass `request.passthrough_auth.as_deref()`.

Logic inside `get_auth_header`:

```
if let Some(token) = override_auth {
    return Ok(format!("Bearer {}", token));
}
// existing internal auth logic follows
```

`GeminiProvider` is not modified — it is never selected in passthrough mode since its `provider_type` is `"gemini"`, not `"anthropic"`.

### Observability

At provider dispatch time, log:

```
info!("🔑 Passthrough auth active: original_model={}, target_provider={}", original_model, mapping.provider);
```

The existing `original_model` restore on the response is already in place and requires no change.

## Data Flow

```
Incoming request
  │  Authorization: Bearer <caller-token>
  ▼
handle_messages
  │  extract bearer → passthrough_auth = Some(token)
  ▼
Router (model rewrite, original_model saved)
  │
  ▼
Filter mappings → anthropic-type only
  │
  ▼
Provider dispatch loop
  │  AnthropicCompatibleProvider::get_auth_header(Some(token))
  │  → "Bearer <caller-token>"  (internal key ignored)
  ▼
Upstream (api.anthropic.com or configured base_url)
  │
  ▼
Response: model restored to original_model
```

## Constraints

- Non-passthrough requests: zero behavior change.
- `passthrough_auth` is never written to logs beyond the info line above (no token leakage in debug output).
- Model rewrite and `original_model` restoration remain unchanged.
- Fallback retry policy is identical to non-passthrough (retry on any error), but only within the filtered anthropic-type mapping list.

## Out of Scope

- Configurable fallback retry caps or backoff.
- Dedicated `AuthContext` type (deferred to future refactor per design doc open questions).
- Changes to `GeminiProvider` or Vertex AI providers.
- Changes to token counting behavior beyond what is needed for consistency.

## Test Plan

1. **Auth preservation** — construct an `AnthropicCompatibleProvider` with a known internal API key; send a request with `passthrough_auth = Some("sk-test-token")`; assert the outgoing HTTP `Authorization` header is `Bearer sk-test-token`, not the internal key.

2. **Provider-type filter** — configure a model with both an `"anthropic"` and an `"openai"` mapping; send a passthrough request; assert only the `"anthropic"` mapping is attempted (openai mapping is skipped).

3. **Model rewrite trace** — send a passthrough request with `model: "claude-opus-4"`; confirm response `model` field is `"claude-opus-4"` (restored from `original_model`); confirm the passthrough log line includes `original_model=claude-opus-4`.
