# Changelog

All notable changes to Claude Code Mux will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.11-chy] - 2026-07-11

### Fixed
- **`cache_control` now preserved on `ToolResultBlock::Text` too** — the previous release fixed `cache_control` dropping on `ContentBlock::Text`, but missed the sibling `ToolResultBlock::Text` variant used inside `tool_result` content arrays. A client-supplied cache checkpoint on that block type was still silently discarded on deserialize/reserialize. Now closed.
- **`RedactedThinking` no longer leaked as visible text by the Gemini provider** — the Gemini provider was converting `redacted_thinking` blocks to plain visible text, defeating the redaction semantics of that block type. It's now silently skipped, matching Anthropic's intent that redacted content never surface to the client.
- **Assistant messages that become empty after thinking-block stripping are no longer sent malformed** — `apply_clear_thinking_directive` (added last release) could leave an assistant message with zero content blocks if it consisted solely of thinking blocks. That empty block array is now replaced with empty text to avoid provider-side 400 validation errors.
- **Stale doc comment corrected** — `apply_clear_thinking_directive`'s doc comment said it stripped "non-last" assistant turns; the code has always stripped all turns including the last. Comment now matches behavior.

## [0.8.10-chy] - 2026-07-10

### Fixed
- **`context_management` and `output_config` fields now pass through to providers** — requests that include Anthropic API fields like `context_management` (e.g. `clear_thinking_20251015`) or `output_config` (e.g. extended thinking effort level) were previously silently dropped because `AnthropicRequest` didn't model them. Both fields are now carried through opaquely so providers that support them receive the directive and apply it themselves. Affects all Anthropic-format providers.
- **`clear_thinking_20251015` now stripped client-side, including the last assistant turn** — the directive is meant to be applied server-side by Anthropic, but it only clears thinking blocks in cache-checkpointed segments. Prior turns sent as plain strings have no `cache_control`, so Anthropic can't identify them as cached and still validates (and rejects) fake signatures from vLLM. Thinking and redacted-thinking blocks are now stripped from every assistant message client-side — including the most recent one, which an earlier pass had left untouched — before the request reaches the provider.
- **`cache_control` no longer dropped from tool-result text blocks on the Anthropic round-trip** — `ContentBlock::Text` inside a `tool_result` didn't model `cache_control`, so a client-supplied cache checkpoint on that block was silently discarded during deserialize/reserialize. It's now preserved end-to-end.

### Added
- **Unknown request field warnings (deduplication-first)** — a new `warn_unknown_request_fields` check fires once per unknown JSON key per process lifetime and logs `⚠️ Unknown field in request (not forwarded): '<key>'`. Helps catch future Anthropic API additions before they cause silent behavior changes without producing continuous log spam under load. Each key is logged at most once via `OnceLock<DashSet>`.
- **TRACE-level logging of outgoing request bodies** — with `RUST_LOG=ccm=trace` (or provider-scoped equivalent), each Anthropic-compatible provider now logs the full outgoing JSON request body (both streaming and non-streaming) right before it's sent. Useful for diagnosing provider-specific request shape issues without needing a packet capture.

## [0.8.9-chy] - 2026-07-09

### Changed
- **BREAKING: `nvidia-nim` migrated from OpenAI-compatible to Anthropic-compatible** — `provider_type = "nvidia-nim"` now routes through NVIDIA's Anthropic Messages (`/v1/messages`) endpoint instead of OpenAI Chat Completions, authenticating with `Authorization: Bearer <nvapi-key>`. The default `base_url` changed from `https://integrate.api.nvidia.com/v1` to `https://integrate.api.nvidia.com`. If you have an explicit `base_url` override ending in `/v1` left over from the old config, it's now auto-corrected (with a warning log) instead of 404ing — see Fixed below. `nvidia-nim` is not eligible for passthrough auth (same as `z.ai`/`minimax`/`zenmux`/`kimi-coding`).

### Fixed
- **vLLM/SGLang auth header** — `provider_type = "vllm"` and `"sglang"` with `auth_type = "apikey"` now send `Authorization: Bearer <api_key>` instead of `x-api-key`. vLLM and SGLang authenticate the OpenAI way when started with `--api-key`, so the previous `x-api-key` header was rejected. Every other Anthropic-format provider (`anthropic`, `z.ai`, `minimax`, `zenmux`, `kimi-coding`) is unaffected and still sends `x-api-key`.
- **`AnthropicCompatibleProvider` no longer hard-fails on a missing `usage` field** — some Anthropic-compatible backends (confirmed for NVIDIA NIM) omit `usage`, or omit individual token counts within it, on some responses. Each of `input_tokens`/`output_tokens` is now defaulted to 0 independently when missing, instead of the whole response parse failing — or, in an earlier iteration of this fix, the whole `usage` object being zeroed out and silently discarding a real token count that was present alongside the missing one. Affects all Anthropic-format providers (`anthropic`, `z.ai`, `minimax`, `zenmux`, `kimi-coding`, `vllm`, `sglang`, `nvidia-nim`).
- **`nvidia-nim` auto-corrects a stale `/v1`-suffixed `base_url`** — configs carried over from the pre-migration OpenAI-compatible setup had `base_url` ending in `/v1`, which would now silently 404 against `/v1/v1/messages`. The registry strips a trailing `/v1` (with a warning log pointing at the corrected value) so upgrading doesn't require a manual config edit.

## [0.8.8-chy] - 2026-07-09

### Added
- **vLLM and SGLang provider types** — connect to self-hosted vLLM (0.8+) and SGLang (0.4+) instances that support the Anthropic-compatible API (`/v1/messages`). You can now route requests to your own vLLM or SGLang servers with zero format translation. Configure with `provider_type = "vllm"` or `provider_type = "sglang"` in your TOML config. Both support `apikey` and `passthrough` auth modes.

### Changed
- **Passthrough auth extended to vLLM and SGLang** — bearer token passthrough now works for vLLM and SGLang providers in addition to Anthropic-type providers. Caller-provided tokens are forwarded correctly when using `auth_type = "passthrough"`.

### Fixed
- **Beta headers note for self-hosted providers** — documentation now warns that `AnthropicCompatibleProvider` injects default `anthropic-beta` headers, which self-hosted vLLM/SGLang endpoints may not recognize. No config option to suppress yet — requires a code fix.

## [0.8.7-chy] - 2026-07-02

### Added
- **`strip_mid_conversation_system` per-mapping flag** — when enabled, mid-conversation `role:"system"` messages (rejected by targets like claude-sonnet-4-6 and non-Anthropic providers) are converted to user `<system-reminder>` blocks before dispatch. Text content is preserved; empty system messages are silently dropped; non-text content blocks are logged but not preserved. Role alternation is always preserved: reminders fold into the adjacent user turn, and if no user turn is available a synthesized one is inserted. Opt in per mapping with `strip_mid_conversation_system = true` in your TOML config.
- **Admin UI checkbox** — the new flag is exposed in all four mapping views (edit primary, edit fallback, add inline model, add separate model fallback).
- **Defense-in-depth guard in OpenAI provider** — if a residual `role:"system"` entry reaches `transform_request` despite normalization, it is skipped with a warning log rather than forwarded to a provider that will reject it.

### Fixed
- **Role alternation preserved during normalization** — an earlier implementation could produce consecutive same-role turns when multiple system messages appeared without an adjacent user turn. The rewrite uses a pending buffer and synthesized user turns so the Anthropic alternation invariant is never violated.
- **Messages restored correctly between fallback iterations** — the fallback provider loop now clones `original_messages` alongside `original_beta_header` so each retry starts from the pristine request, not from a partially-normalized one.
- **Multiple leading system messages collapsed** — consecutive `role:"system"` turns at the start of the conversation (before the first user turn) are now merged into a single synthesized user turn instead of generating multiple consecutive user turns.
- **Idempotence tightened for already-wrapped content** — a system message whose full content is a single well-formed `<system-reminder>` block is passed through untouched. Stray `</system-reminder>` tags inside the payload are escaped to prevent premature wrapper termination.
- **Admin UI helper text HTML entities** — the "system-reminder" label in checkbox helper text was displaying raw `<` / `>` characters; they are now properly HTML-entity-escaped.

### Changed
- **Admin UI checkbox block extracted to `renderMappingCheckboxes()` helper** — the strip-options checkbox group was duplicated four times; it is now a single function, eliminating ~80 lines of repetition.

## [0.8.6-chy] - 2026-06-26

### Fixed
- **OpenAI-compatible providers no longer crash on empty choices** — some upstreams (soft rate-limits, content filters) return HTTP 200 with an empty `choices` array. Previously this panicked the worker thread via `.expect()`. The proxy now returns a 502 error and falls back to the next provider in the chain.
- **Empty-choices providers are temporarily cooled down** — when a provider returns an empty-choices response, it is placed on a 60-second cooldown so subsequent requests route to healthy providers instead of repeatedly hitting a provider that is soft-rejecting.

## [0.8.5-chy] - 2026-06-26

### Fixed
- **Missing usage field in Responses API path** — the same nullable-usage fix applied to Chat Completions (nvidia-nim compat) was not tested for the Responses API path. Added test coverage for both paths.

### Changed
- **Subagent detection flag extracted to constant** — `cc_is_subagent=true` is now a named `SUBAGENT_FLAG` constant, eliminating magic string duplication across the router.

## [0.8.4-chy] - 2026-06-25

### Added
- **Defined models bypass auto-map** — when a requested model matches your `auto_map_regex` (e.g. `^claude-`) *and* its name is declared in a `[[models]]` block, the proxy no longer rewrites it to the default model. It keeps its own name and resolves through its own provider mappings. Higher-priority routing (websearch, subagent, think, background) still applies first; this only changes the auto-map step. Lets you keep `auto_map_regex = "^claude-"` for generic Claude traffic while routing a specific `claude-*` model exactly where you point it. See "Auto-mapping" in the README routing logic section.

### Changed
- **Subagent detection uses billing header instead of system prompt tags** — the router now detects subagent requests by scanning for `cc_is_subagent=true` in the system prompt (any block, not just the billing header). The old `<CCM-SUBAGENT-MODEL>` tag mechanism has been retired: tags are still stripped from `Blocks`-style prompts for backward compatibility, but they no longer influence routing. When `router.subagent` is not configured, the request falls through to Think/Background/Auto-map/Default — the previous fall-through behavior (extracting the tag's model name) has been removed.

### Security
- **Control plane now refuses to run unauthenticated on a public address** — if `server.api_key` is unset, `ccm` will not bind the admin/control API (`/api/*`: config rewrite, OAuth token read/delete/refresh, restart) to a non-loopback host. Set `server.api_key` to expose it on a shared address, or bind to `127.0.0.1`. **Behavior change:** an existing deployment that bound to `0.0.0.0` with no `api_key` will now refuse to start instead of running wide open.
- **A blank `api_key` is now treated as unset** — `server.api_key = ""` (or whitespace, e.g. an empty templated env var) previously counted as "configured," skipping the loopback/auth gates while still authorizing every request — fully opening the control plane. Blank keys are now normalized to unset and fall back to loopback-only, with a startup warning.
- **DNS-rebinding defense for both planes** — the control plane validates that the request `Host` is a real loopback authority (parsed as an IP, not a `127.`-prefix string, so `127.0.0.1.nip.io` is rejected) and rejects cross-origin state-changing browser requests. The LLM data plane (`/v1/*`) now gets the same loopback-`Host` check when no `api_key` is set, so a rebinding page can no longer drive it to spend tokens or read model output. Non-browser clients (no `Origin`, loopback `Host`) are unaffected.
- **Credentials written owner-only and atomically** — the OAuth token store is created `0600` from the start (no world-readable window) and persisted via a temp file + fsync + atomic rename, so a crash or full disk can no longer truncate `oauth_tokens.json` and force a full re-auth. On-disk restart-script permissions were hardened too.
- **Safer UI restart** — the restart script is written to your user-owned config dir (never a world-writable `/tmp`/`%TEMP%` path), fails closed if no home dir resolves, shell-escapes every interpolated path, and now forwards `--config` so a UI restart reboots with the same config the running process used instead of silently falling back to defaults.
- **Hardened CI/release** — GitHub Actions are pinned to commit SHAs, release inputs are passed via environment (no shell injection), and the release tag/version sources are kept in sync.
- Streaming trace headers are redacted and OpenAI trace logging records custom header **keys only** (never values), preventing secret exposure at `RUST_LOG=ccm=trace`.

### Fixed
- **OAuth token refresh could hang on a slow provider** — `OAuthClient` previously used an HTTP client with no timeout, so a stalled provider endpoint could block a token refresh indefinitely and let an idle token expire anyway. `OAuthClient::new` now builds a 30s-timeout client for all callers, and the background refresh loop injects its shared timeout-bearing client. A slow provider now fails fast instead of wedging refresh.
- **Startup banner showed the wrong version** — `ccm start` printed the crate version from `Cargo.toml` (0.7.0) instead of the project `VERSION` file, so the reported version was stale. The banner now reads `VERSION` directly, making it the single source of truth.
- **OpenAI Chat Completions provider panicked when upstream omitted `usage`** — some providers (e.g. NVIDIA NIM) don't include the `usage` field in Chat Completions responses. The field is now `Option<OpenAIUsage>` with a default, and token counts fall back to `0` when missing. The Responses API path is unaffected.

### For contributors
- Extracted shared helpers in `src/server/mod.rs` (`apply_cooldown`, `control_plane_requires_loopback` + `control_plane_bind_guard` so the bind-time and request-time gates share one predicate, `data_plane_rebinding_guard`, `normalize_api_key`) and a `parse_models_cache` helper for Copilot `/models`. Added unit + middleware (tower `oneshot`) test coverage for the CSRF/DNS-rebinding guards, bind gate, blank-key normalization, atomic token writes, and shell-quote escaping.

## [0.8.3-chy] - 2026-05-20

### Fixed
- **Copilot 400 cascade** — after 3–6 turns, the Copilot API was returning `400 Bad Request` with no body because the proxy was sending requests without the VSCode session tracking headers the API requires. Fixed by adding `VScode-SessionId` and `VScode-MachineId` (stable UUIDs per proxy session), `X-Request-Id` (fresh UUID per request), `Openai-Organization`, `X-GitHub-Api-Version`, and `X-Interaction-Type`. Upgrade to this version to stop mid-session 400 errors.
- **Copilot 401 retry race** — if a Copilot token was between the 5-minute refresh gate and actual expiry, a 401 retry would re-check `needs_refresh()`, find the token still valid, and return the same stale token — failing again silently. Fixed by force-invalidating the cached token on any 401 response.
- **Copilot auto-model lock contention false negatives** — model discovery previously used a lock upgrade path that could treat normal contention as a failure and skip model resolution. The path now retries safely and avoids spurious resolution misses under concurrent requests.

### Added
- **Copilot network retry** — single quiet retry on connect-level errors (timeout, connection reset, DNS failure). The retry regenerates a fresh `X-Request-Id`. Mid-stream failures are not retried (once SSE headers are accepted, the connection is committed).
- Structured `tracing` log lines for all three recovery paths — check logs with `RUST_LOG=ccm=info cargo run`:
  - `Copilot session established [session_id=..., machine_id=...]` — confirms fix is active on startup
  - `Copilot network retry [attempt=1]` — confirms single-retry path fired
  - `Copilot 401: force-refreshing token [session_id=...]` — confirms force-invalidate fired
- **Copilot auto-model resolution** — `model: "auto"` now resolves against Copilot's `/models` API with a short TTL cache, then forwards a concrete model upstream. Users now get deterministic model selection instead of endpoint-dependent behavior.
- Structured `tracing` for model selection decisions (`auto` resolution path, cache refresh, and fallback behavior), making it easier to diagnose which model was chosen and why.

### Changed
- **Provider cooldown durations** — 401/403 cooldown raised from 60 s to 240 s; 429 cooldown raised from 30 s to 120 s. Providers that hit auth failures or rate limits now stay out of rotation longer, reducing hammering on struggling endpoints.
- **Copilot model passthrough** — removed `auto` model special-casing. Previously, sending `model: "auto"` to the Copilot endpoint stripped the model field from the upstream request. The model field is now forwarded as-is for all values.

## [0.8.2-chy] - 2026-05-19

### Added
- **Provider cooldowns** — when a provider returns 401/403, it's skipped for 60 seconds; 429 skips it for 30 seconds. You get faster fallback to the next provider instead of hammering a rate-limited or auth-failed endpoint
- **Generalized background OAuth refresh** — background token refresh now covers all OAuth providers (Anthropic, Gemini, OpenAI-compatible), not just Copilot. Idle OAuth tokens no longer expire silently when a provider isn't in the active request path
- **Operational contracts** — seven spec documents in `docs/contracts/`: rollback contract, SLO, escalation SLA, fallback selection policy, benchmark protocol, auth validation spec, and streaming fallback boundary

### Fixed
- `escapeJs()` replaces `escapeHtml()` in OAuth token onclick handlers — prevents JS string injection if a provider ID contains quotes or backslashes
- `loadConfig()` now checks `response.ok` before calling `response.json()` — shows a toast warning instead of crashing when the server requires an API key

### For contributors
- Renamed `COPILOT_POLL_SECS` → `OAUTH_POLL_SECS` to reflect that the poll interval now governs all OAuth providers
- TODOS.md: 8 outstanding items resolved, 3 new follow-up items added

## [0.8.1-chy] - 2026-05-15

### Added
- **Configurable Subagent Model** — set `router.subagent` in your config (or via the admin Router tab) to route all subagent requests to a specific model, overriding the model name embedded in the `<CCM-SUBAGENT-MODEL>` tag
- Subagent Model dropdown added to the Router tab in the admin UI, matching the Think/Background/WebSearch pattern
- `current-subagent` display added to the status overview card
- Admin UI now uses UIKit modal dialogs and toast notifications instead of browser-native `alert()`/`confirm()`/`prompt()` popups — no more dialog boxes that block the whole browser tab

### Changed
- Subagent routing now has two modes: when `router.subagent` is set, that model wins; when unset, the tag's model name falls through to think/background/auto-map/default routing as before — no behavior change for existing configs

## [0.8.0-chy] - 2026-05-13

### Added
- **GitHub Copilot provider** — use your GitHub Copilot subscription as an AI
  backend via OAuth device code flow; supports GPT-4o, o3-mini, Claude Sonnet,
  and other models exposed by the Copilot API
- `github_copilot` auth module with device code flow, token polling, and automatic
  Copilot token refresh using the stored GitHub OAuth token
- `copilot-start` and `copilot-exchange` API endpoints for initiating and completing
  the GitHub device code authorization flow
- GitHub Copilot device code UI in the admin panel (one-click flow with polling)
- Example config at `config/copilot.example.toml`

### Changed
- API key authentication middleware (`require_api_key`) now uses constant-time
  comparison to prevent timing side-channel attacks; all protected routes require
  the configured `server.api_key` when set
- Admin UI `apiFetch()` wrapper automatically attaches `X-Api-Key` header from
  session storage and prompts the user on 401 responses
- Provider API keys are redacted (replaced with `api_key_set: bool`) in all admin
  API responses; keys are no longer sent to the browser
- Token data removed from debug logs; only response size is logged
- OpenAI provider request/response types promoted to `pub(crate)` for reuse by
  the Copilot provider

### Fixed
- Copilot token `expires_at` is now validated to be within a 24-hour window;
  malformed values (zero, past, or far-future timestamps) fall back to a safe
  30-minute default with a warning log
- XSS in OAuth callback: `error`, `error_description`, and `code` query params
  are HTML-escaped before rendering
- SSRF protection: Copilot proxy endpoint is validated to end with
  `.githubcopilot.com` before use; arbitrary endpoints are rejected

## [0.7.0-chy] - 2026-05-05

### Added
- Claude OAuth Passthrough Relay — callers can pass their own bearer tokens instead of using internal API keys
- Bearer token detection and validation in incoming requests
- Provider-type filtering for passthrough requests (anthropic-type only for security)
- `passthrough_auth` field to `AnthropicRequest` model
- `override_auth` parameter to provider `get_auth_header()` methods
- Bearer token validation to prevent header injection attacks
- Comprehensive passthrough relay specification and implementation plan
- Integration tests for passthrough provider-type filtering
- Library exports to enable example builds
- **NVIDIA NIM Provider Support** — cloud API access to Meta Llama, Mistral, and other open-source models
- NVIDIA NIM rate limiting enforcement (40 requests/minute) with configurable max wait timeout and fallback
- NVIDIA NIM provider option in admin UI for easy setup
- Rate limit field (`rate_limit_rpm`) to provider configuration for rate-limited providers

### Changed
- Handler auth flow now detects and preserves caller-provided bearer tokens
- Fallback behavior restricted for passthrough requests (anthropic-type providers only)
- Enhanced security validation on incoming Authorization headers

### Fixed
- Added missing tempfile dev-dependency for token_store tests

## [0.6.0] - 2025-11-19

### Added
- Google Gemini provider with OAuth 2.0 support (Google AI Pro/Ultra via Code Assist API)
- Separate Vertex AI provider for GCP platform with multi-model support
- Three authentication methods for Gemini: OAuth, API Key (AI Studio), and Vertex AI (ADC)
- Anthropic to Gemini API format transformation
  - System prompts to systemInstruction
  - Message conversion (user/assistant to user/model)
  - Content blocks (text, image, thinking)
  - Tools/functions to functionDeclarations
  - Generation config mapping (temperature, top_p, top_k, max_tokens)
- Gemini to Anthropic response transformation
- OAuth token refresh logic for Gemini provider
- Admin UI support for Gemini and Vertex AI providers
- Comprehensive Gemini/Vertex AI integration documentation
- Project ID and location configuration for Vertex AI
- OAuth token store with project_id field for Gemini

### Changed
- Separated Vertex AI as distinct provider type from Gemini
- Enhanced OAuth flow to support Google's standard OAuth 2.0 parameters
- Updated OAuth handlers with loadCodeAssist API integration for project_id retrieval

## [0.5.0] - 2025-11-19

### Added
- OpenAI ChatGPT Plus/Pro OAuth 2.0 authentication support
- GPT-5.1 and GPT-5.1 Codex model support via OpenAI OAuth
- OpenAI Codex Responses API integration (`/codex/responses` endpoint)
- Reasoning block to thinking block conversion for Codex models
- Separate OAuth callback server on port 1455 for OpenAI OAuth
- Official OpenAI Codex instructions from rust-v0.58.0
- Browser-like headers for Cloudflare bypass (native-tls)
- SSE (Server-Sent Events) response parsing for streaming
- JWT token decoding to extract ChatGPT account_id
- Admin UI support for OpenAI OAuth flow (similar to Anthropic OAuth)

### Changed
- Switched from rustls-tls to native-tls for better compatibility
- Enhanced OpenAI provider to support both API Key and OAuth authentication
- Updated OAuth handlers to support "openai-codex" type
- Improved SSE parsing to extract both reasoning and message content blocks

### Fixed
- OpenAI Codex model streaming with proper endpoint routing
- PKCE state/verifier separation for OpenAI OAuth compatibility
- Reasoning block handling in gpt-5.1-codex responses

## [0.4.3] - 2025-11-17

### Added
- CI and Latest Release badges to README
- FAQ section with 6 common questions
- CHANGELOG.md with full version history
- Collapsible screenshots with descriptive captions
- Collapsible provider details section

### Changed
- Restructured README for better onboarding flow (moved comparison section to bottom)
- Compressed Supported Providers section with summary
- Updated performance metrics with actual measurements (6MB vs 156MB)
- Improved OAuth description to focus on Claude Pro/Max compatibility

### Fixed
- Memory usage comparison (updated from 10x to accurate 25x difference)

## [0.4.2] - 2025-11-17

### Fixed
- Use rustls instead of native-tls for better cross-compilation support

### Changed
- Added automated release workflow for GitHub releases

## [0.4.1] - 2025-11-17

### Fixed
- Use `/v1/responses` endpoint for Codex model streaming requests

## [0.4.0] - 2025-11-17

### Added
- OpenAI Responses API support for Codex models (gpt-5-turbo, etc.)
- Automatic endpoint routing based on model type

## [0.3.0] - 2025-11-17

### Added
- OpenAI-compatible `/v1/chat/completions` endpoint
- Support for OpenAI format requests alongside Anthropic format

### Fixed
- Router tab auto-save logging improvements

## [0.2.0] - 2025-11-17

### Added
- Documentation improvements
- Engaging intro tagline in README

## [0.1.0] - 2025-11-17

### Added
- Initial release of Claude Code Mux
- High-performance AI routing proxy built in Rust
- Anthropic Messages API compatibility (`/v1/messages`)
- Intelligent model routing (default, think, background, websearch)
- Provider failover with priority-based routing
- Streaming support (SSE)
- Web-based admin UI with auto-save
- OAuth 2.0 authentication for Anthropic
- Multi-provider support (16+ providers)
- Auto-mapping with regex patterns
- TOML-based configuration
- Token counting endpoint (`/v1/messages/count_tokens`)

[0.8.8-chy]: https://github.com/9j/claude-code-mux/compare/v0.8.7-chy...v0.8.8-chy
[0.7.0]: https://github.com/9j/claude-code-mux/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/9j/claude-code-mux/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/9j/claude-code-mux/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/9j/claude-code-mux/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/9j/claude-code-mux/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/9j/claude-code-mux/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/9j/claude-code-mux/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/9j/claude-code-mux/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/9j/claude-code-mux/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/9j/claude-code-mux/releases/tag/v0.1.0
