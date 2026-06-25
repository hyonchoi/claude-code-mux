# Claude Code Mux Documentation

`ccm` is an HTTP proxy that speaks the Anthropic Messages API and routes each
request to one of many LLM providers, with priority-based fallback.

This documentation follows the [Diataxis](https://diataxis.fr/) framework: four
kinds of docs for four reader needs. Start with the tutorial if you are new.

## Tutorials (learning-oriented)

Start here. Walk from zero to a working, routed request.

- [Getting Started](tutorials/getting-started.md) - install `ccm`, configure one
  provider, and route your first request.

## How-to guides (task-oriented)

Recipes for specific tasks. These assume you already have `ccm` running.

- [OAuth setup](OAUTH_SETUP.md) - connect OAuth providers (Claude Pro/Max, OpenAI
  Codex, Gemini, Copilot).
- [OAuth testing](OAUTH_TESTING.md) - verify the OAuth flow end to end.
- [Gemini & Vertex AI integration](gemini-integration.md) - configure Google AI
  providers (API key, Code Assist OAuth, and Vertex AI).
- The README [Usage Guide](../README.md#usage-guide) covers adding providers and
  model mappings through the admin UI.

## Reference (information-oriented)

Complete, factual descriptions. Look here for exact fields, flags, and endpoints.

- [Configuration reference](reference/configuration.md) - every TOML table and
  field, with types and defaults.
- [Routing reference](reference/routing.md) - the priority pipeline, auto-map, and
  model resolution.
- [Provider reference](reference/providers.md) - every `provider_type`, its
  upstream format, base URL, and auth modes.
- [CLI reference](reference/cli.md) - the `ccm` subcommands, flags, and env vars.
- [HTTP API reference](reference/http-api.md) - the `/v1/*` inference endpoints and
  the `/api/*` admin and OAuth endpoints.

## Explanation (understanding-oriented)

The "why" behind the design.

- [Architecture](explanation/architecture.md) - the request lifecycle, the module
  layout, and the provider-adapter abstraction.
- [Why routing works this way](explanation/routing-design.md) - why auto-map runs
  last and why defined models skip it.
- [Provider fallback and cooldowns](explanation/provider-fallback.md) - failover,
  cooldowns, rate limiting, and the streaming boundary.

## Engineering contracts

Precise behavioral specs for backend contributors.

- [Fallback selection policy](contracts/fallback-selection-policy.md)
- [Streaming fallback boundary](contracts/streaming-fallback-boundary.md)
- [Auth validation spec](contracts/auth-validation-spec.md)
- [Rollback contract](contracts/rollback-contract.md)
- [SLO contract](contracts/slo-contract.md)
- [Escalation SLA](contracts/escalation-sla.md)
- [Benchmark protocol](contracts/benchmark-protocol.md)

## Admin UI internals

For contributors working on the admin UI (`src/server/admin.html`).

- [Design principles](design-principles.md)
- [localStorage state management](localstorage-state-management.md)
- [URL state management](url-state-management.md)
- [Screenshot guide](SCREENSHOT_GUIDE.md)
