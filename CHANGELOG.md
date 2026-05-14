# Changelog

All notable changes to Claude Code Mux will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0.0] - 2026-05-13

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

[0.6.0]: https://github.com/9j/claude-code-mux/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/9j/claude-code-mux/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/9j/claude-code-mux/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/9j/claude-code-mux/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/9j/claude-code-mux/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/9j/claude-code-mux/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/9j/claude-code-mux/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/9j/claude-code-mux/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/9j/claude-code-mux/releases/tag/v0.1.0
