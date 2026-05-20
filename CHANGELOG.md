# Changelog

All notable changes to Claude Code Mux will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.3-chy] - 2026-05-20

### Fixed
- **Copilot 400 cascade** — after 3–6 turns, the Copilot API was returning `400 Bad Request` with no body because the proxy was sending requests without the VSCode session tracking headers the API requires. Fixed by adding `VScode-SessionId` and `VScode-MachineId` (stable UUIDs per proxy session), `X-Request-Id` (fresh UUID per request), `Openai-Organization`, `X-GitHub-Api-Version`, and `X-Interaction-Type`. Upgrade to this version to stop mid-session 400 errors.
- **Copilot 401 retry race** — if a Copilot token was between the 5-minute refresh gate and actual expiry, a 401 retry would re-check `needs_refresh()`, find the token still valid, and return the same stale token — failing again silently. Fixed by force-invalidating the cached token on any 401 response.

### Added
- **Copilot network retry** — single quiet retry on connect-level errors (timeout, connection reset, DNS failure). The retry regenerates a fresh `X-Request-Id`. Mid-stream failures are not retried (once SSE headers are accepted, the connection is committed).
- Structured `tracing` log lines for all three recovery paths — check logs with `RUST_LOG=ccm=info cargo run`:
  - `Copilot session established [session_id=..., machine_id=...]` — confirms fix is active on startup
  - `Copilot network retry [attempt=1]` — confirms single-retry path fired
  - `Copilot 401: force-refreshing token [session_id=...]` — confirms force-invalidate fired

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
