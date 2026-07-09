<!-- /autoplan restore point: /Users/hyonchoi/.gstack/projects/hyonchoi-claude-code-mux/main-autoplan-restore-20260709-010000.md -->
# Plan: vLLM/SGLang Native Provider Implementation

## Context

TODOS.md P2 item: Add dedicated vLLM and SGLang provider types that speak the Anthropic-compatible API format (`/v1/messages`) instead of routing through the OpenAI-compatible `/v1/chat/completions` endpoint.

Both vLLM (0.8+) and SGLang (0.4+) now support Anthropic-format endpoints (`/v1/messages`) that natively emit Anthropic SSE events (thinking blocks, tool_use, text content) without an intermediate OpenAI translation layer.

## Problem Statement

The current path for self-hosted models via vLLM/SGLang routes through `openai` provider type → OpenAI `/v1/chat/completions` → OpenAI format → transform → Anthropic format. This translation layer causes:
1. Reasoning-only stops (P1 bug) — vLLM reasoning parser routes all tokens to `reasoning_content`, leaving `content: null`
2. No proper thinking blocks, tool_use, or usage metadata in streaming
3. Raw SSE byte passthrough with no transformation

## Premises (for user confirmation)

1. **vLLM/SGLang Anthropic endpoints are the right target.** Both engines support `/v1/messages` endpoints. The user has presumably verified this for their deployment.
2. **Users want a frictionless way to configure vLLM/SGLang.** Currently, users can already use `provider_type = "anthropic"` with a custom `base_url`, but this is undocumented and requires knowing the internal mechanics. Dedicated provider types make this discoverable and provide sensible defaults.
3. **The OpenAI streaming passthrough fix is a separate concern.** The P1 bug exists on the OpenAI path. Native Anthropic endpoints sidestep it, but don't fix it for users who must use the OpenAI-compatible endpoint.

## Implementation Approach

Both Claude subagent and Codex identified that `provider_type = "anthropic"` with custom `base_url` already routes to `{base_url}/v1/messages` — the exact endpoint vLLM/SGLang exposes. The plan reframes around what additional value dedicated `vllm`/`sglang` types provide beyond the existing generic path.

### What already works today

- `registry.rs:97` — `"anthropic"` match arm accepts any `base_url` from config
- `anthropic_compatible.rs:510` — `send_message` POSTs to `{base_url}/v1/messages`
- `anthropic_compatible.rs:743` — `send_message_stream` POSTs to `{base_url}/v1/messages` with streaming
- Format is Anthropic-native — no transformation layer

### What dedicated types add

1. **Discoverability** — User sees `provider_type = "vllm"` in docs, no need to know internals
2. **Sensible defaults** — Default `base_url` to common roots (`http://localhost:8000` for vLLM, `http://localhost:30000` for SGLang). Note: `AnthropicCompatibleProvider` appends `/v1/messages`, so the default is the server root, not `/v1`.
3. **Passthrough auth compatibility** — Currently only `provider_type == "anthropic"` gets passthrough auth. Dedicated types need `should_use_passthrough_auth` updated to include them.

### Changes

#### 1. `src/providers/anthropic_compatible.rs` — No changes needed

No factory methods needed. The registry inline pattern (matching `"anthropic"`, `"z.ai"`, etc.) is sufficient and keeps `base_url` from config.

**Beta header consideration:** `AnthropicCompatibleProvider` injects default `anthropic-beta` headers (`oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14`) when no beta header is supplied. vLLM/SGLang Anthropic-compatible endpoints may not accept these Claude-specific beta flags. **Auto-decide:** Document this in the config examples — users should set `supported_beta_options` to an empty list for vLLM/SGLang to suppress default beta headers. If vLLM/SGLang reject requests with beta headers, this is a P1 fix (add a `strip_beta_options` toggle at the provider config level).

#### 2. `src/providers/registry.rs` — Add provider type match arms

Add `"vllm"` and `"sglang"` cases. **Critical:** unlike cloud providers (z.ai, minimax) with fixed URLs, vLLM/SGLang are self-hosted — users override `base_url`. Use `config.base_url.clone().unwrap_or_else(|| default_url)` so the config value is honored:

```rust
"vllm" => Box::new(
    AnthropicCompatibleProvider::new_with_options_and_auth(
        config.name.clone(),
        api_key,
        config.base_url.clone().unwrap_or_else(|| "http://localhost:8000".to_string()),
        config.models.clone(),
        config.auth_type.clone(),
        config.oauth_provider.clone(),
        token_store.clone(),
        Vec::new(),
    ).with_rate_limit_config(config.rate_limit_rpm, config.rate_limit_max_wait_ms),
),
"sglang" => Box::new(
    AnthropicCompatibleProvider::new_with_options_and_auth(
        config.name.clone(),
        api_key,
        config.base_url.clone().unwrap_or_else(|| "http://localhost:30000".to_string()),
        config.models.clone(),
        config.auth_type.clone(),
        config.oauth_provider.clone(),
        token_store.clone(),
        Vec::new(),
    ).with_rate_limit_config(config.rate_limit_rpm, config.rate_limit_max_wait_ms),
),
```

No factory methods needed in `anthropic_compatible.rs` — the registry inline pattern (matching `"anthropic"`, `"z.ai"`, etc.) is sufficient and keeps `base_url` from config.

#### 3. `src/server/mod.rs` — Extend passthrough auth support

Update `should_use_passthrough_auth` to include `vllm` and `sglang` provider types:

```rust
fn should_use_passthrough_auth(providers: &[ProviderConfig], name: &str) -> bool {
    providers.iter().find(|p| p.name == name).map(|p| {
        matches!(p.provider_type.as_str(), "anthropic" | "vllm" | "sglang")
            && matches!(p.auth_type, AuthType::Passthrough)
    }).unwrap_or(false)
}
```

#### 4. `config/models.example.toml` — Add example configs

```toml
# vLLM Anthropic-compatible provider (self-hosted)
[[providers]]
name = "vllm-local"
provider_type = "vllm"
# base_url = "http://localhost:8000"  # Default, override if different
# AnthropicCompatibleProvider appends /v1/messages automatically
auth_type = "ApiKey"  # Self-hosted: use ApiKey with a dummy key, or Passthrough for Claude Code
api_key = "none"      # Placeholder — self-hosted vLLM typically ignores this
# supported_beta_options = []  # Empty = no default beta headers sent
models = ["qwen3.6-27b", "deepseek-r1-32b"]

# SGLang Anthropic-compatible provider (self-hosted)
[[providers]]
name = "sglang-local"
provider_type = "sglang"
# base_url = "http://localhost:30000"  # Default, override if different
# AnthropicCompatibleProvider appends /v1/messages automatically
auth_type = "ApiKey"  # Self-hosted: use ApiKey with a dummy key, or Passthrough for Claude Code
api_key = "none"      # Placeholder — self-hosted SGLang typically ignores this
# supported_beta_options = []  # Empty = no default beta headers sent
models = ["qwen3.6-27b", "deepseek-r1-32b"]
```

#### 5. `docs/reference/configuration.md` — Document new provider types

Add `vllm` and `sglang` to the provider type reference with:
- Default base URL
- Auth types supported
- Link to vLLM/SGLang Anthropic API docs

#### 6. `src/server/admin.html` — Add provider type options

Add `vllm` and `sglang` to the provider type dropdown selector.

#### 7. Tests

Add registry tests following `test_copilot_provider_registration` pattern for both types.

## NOT in scope

- Modifying the OpenAI provider's streaming passthrough (separate P1 fix)
- vLLM/SGLang installation or deployment guides
- Benchmarking or performance tuning
- Model-specific configuration (handled by vLLM/SGLang themselves)
- Version detection or health probes for vLLM/SGLang instances

<!-- AUTONOMOUS DECISION LOG -->
## Decision Audit Trail

| # | Phase | Decision | Classification | Principle | Rationale | Rejected |
|---|-------|----------|-----------|-----------|----------|----------|
| 1 | CEO | Scope: dedicated types, not generic refactor | Taste | P5 (explicit) | Dedicated types are discoverable; generic "anthropic" with custom base_url works but is undocumented | Generic "anthropic-compatible" type — harder to discover, no engine-specific defaults |
| 2 | CEO | Passthrough auth extension for vllm/sglang | Mechanical | P1 (completeness) | Without this, passthrough auth breaks for self-hosted setups | No alternative — passthrough is the common auth pattern |
| 3 | CEO | Include admin UI dropdown update | Mechanical | P2 (boil lakes) | In blast radius, <1d effort | Defer — users can configure via TOML, but admin UI is the primary surface |
| 4 | Eng | Inline registry construction, no factory methods | Mechanical | P5 (explicit) | Self-hosted base_url must come from config, not hardcoded factory | Factory methods in anthropic_compatible.rs — would hardcode base_url |
| 5 | Eng | Beta header doc, not runtime toggle | Mechanical | P3 (pragmatic) | Most vLLM/SGLang tolerate unknown headers; documenting is faster than adding a toggle | Runtime strip_beta toggle at provider level — extra complexity |
| 6 | Eng | Fix base_url default to root (not /v1) | Mechanical | P1 (completeness) | AnthropicCompatibleProvider appends /v1/messages, double /v1 is a bug | Keep /v1 in default — would produce /v1/v1/messages |
| 7 | DX | ApiKey + dummy key as default auth | Mechanical | P5 (explicit) | Passthrough only works for Claude Code CLI; ApiKey with dummy is universal | Passthrough — fails for non-Claude-Code callers |

---

## GSTACK REVIEW REPORT

**Status:** APPROVED
**Date:** 2026-07-09T01:00:00Z
**Branch:** main
**Commit:** 3d33a13

### Summary

Add `vllm` and `sglang` provider types that route through `AnthropicCompatibleProvider` to the native `/v1/messages` Anthropic-compatible endpoint, eliminating the OpenAI translation layer.

### Scope

**In scope:**
- 2 new provider types in `registry.rs` (vllm, sglang)
- Extend `should_use_passthrough_auth` in `server/mod.rs`
- Config examples in `models.example.toml`
- Docs update in `configuration.md`
- Admin UI dropdown update
- Tests (registry + passthrough auth)

**NOT in scope:**
- OpenAI provider streaming passthrough fix (separate P1)
- vLLM/SGLang version detection or health probes
- Generic "no auth" auth type
- Capability detection (auto-select endpoint per engine)

### Review Scores

| Phase | Score | Issues |
|-------|-------|--------|
| CEO | Clean | 0 unresolved |
| Eng | Clean | 0 unresolved (3 found, all fixed) |
| DX | 7.8/10 | 0 unresolved |

### Dual Voices

| Phase | Claude | Codex | Consensus |
|-------|--------|-------|-----------|
| CEO | 6 findings | 8 findings | 6/6 confirmed |
| Eng | 0 critical | 3 critical (base_url, auth, beta) | 3/6 confirmed, 3 fixed |

### Key Findings Fixed

1. **base_url double /v1** — `AnthropicCompatibleProvider` appends `/v1/messages`, default was `localhost:8000/v1` → fixed to `localhost:8000`
2. **Auth guidance** — Passthrough only works for Claude Code CLI → docs default to ApiKey + dummy key
3. **Beta headers** — Default Claude beta flags may not be recognized by vLLM/SGLang → documented `supported_beta_options = []` to suppress

### Implementation Effort

Estimated: **S** (single afternoon, ~5 files, no new infrastructure)

### Next Step

`/ship` when ready to implement and create the PR.