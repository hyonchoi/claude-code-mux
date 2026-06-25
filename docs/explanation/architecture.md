# Architecture

## The problem

Claude Code (and other Anthropic-format clients) speak one API: the Anthropic Messages API. But the model you actually want behind that API might live anywhere. It could be a real Anthropic endpoint, an OpenAI Chat Completions endpoint, the OpenAI Responses/Codex API, a Gemini endpoint, or a Copilot-backed OAuth provider. Each of those upstreams has its own request shape, its own auth, and its own quirks. You do not want to teach the client about all of them. You want the client to stay simple and have something in the middle do the translating.

## The approach

`ccm` is that something in the middle. It is an HTTP proxy that speaks ONLY the Anthropic Messages API to clients. Every upstream difference is absorbed by a per-provider adapter inside the proxy. The client sends one format. The proxy picks a real model, picks a provider, translates the request into that provider's format, sends it, and translates the response back into Anthropic format.

This one choice (proxy speaks Anthropic, adapters translate) is what makes everything else possible. Routing, fallback, and cooldowns all operate on a single normalized request shape, so they never have to know which upstream a request will end up on.

## Request lifecycle

```
  Anthropic Messages request
            |
            v
  +-------------------------+
  | axum HTTP handler       |  optional API-key middleware
  | (src/server)            |
  +-------------------------+
            |
            v  parse into AnthropicRequest
  +-------------------------+
  | Router::route()         |  decides ONE final model name
  | (src/router)            |  websearch / subagent / think /
  +-------------------------+  background / auto-map / default
            |
            v  final model name
  +-------------------------+
  | model resolution        |  look up [[models]] by name,
  | (src/server)            |  sort that model's mappings by priority
  +-------------------------+
            |
            v  for each mapping in priority order:
  +-------------------------+      skip if provider on cooldown
  | provider adapter        |      set actual_model (upstream name)
  | (src/providers)         |      pick adapter from registry
  +-------------------------+
            |  translate Anthropic -> upstream format
            v
  +-------------------------+
  | upstream API            |  Anthropic / OpenAI Chat /
  | (real LLM endpoint)     |  OpenAI Responses / Gemini
  +-------------------------+
            |  translate response back to Anthropic format
            v
   first success returns to client
   any error -> apply cooldown -> fall over to next mapping
   all fail   -> error listing every failure
```

The proxy never asks the client to understand any of the upstream shapes. The adapter is the translation boundary. That is the whole point.

## Module layout

- `src/main.rs` - CLI entry (`start` / `stop` / `restart` / `status` / `model` / `init`). Reads `VERSION` via `include_str!` for the startup banner.
- `src/cli/` - config types (`AppConfig`, `ServerConfig`, `RouterConfig`, `ModelConfig`, `ModelMapping`), TOML loading, and environment-variable resolution.
- `src/router/` - `Router::route`, the priority pipeline, auto-map, and subagent tag handling. Decides the final model name. See [../explanation/routing-design.md](../explanation/routing-design.md).
- `src/providers/` - the `AnthropicProvider` trait, per-provider adapters, the registry that maps `provider_type` to an adapter, rate limiting (a governor token bucket), and SSE streaming.
- `src/server/` - the axum HTTP server, the `/v1/*` and `/api/*` handlers, model-to-provider resolution with cooldown and fallback, the single-file admin UI (`admin.html`), OAuth handlers, and background token refresh.
- `src/auth/` - the OAuth client (`OAuthClient`) and the on-disk token store (`~/.claude-code-mux/oauth_tokens.json`, `chmod 0600` on unix).

## The provider adapter abstraction

Every upstream is reached through the `AnthropicProvider` trait. The trait has `send_message`, `send_message_stream`, `count_tokens`, and `supports_model`. At startup a registry builds the right adapter for each provider based on its `provider_type`.

How much an adapter does depends on how far the upstream is from Anthropic:

- Anthropic-format providers are near pass-through.
- OpenAI-format providers translate both directions: the system prompt, `tool_use` and `tool_calls`, `tool_result` and `role:tool`, images and `image_url` data URLs, and `finish_reason` and `stop_reason`.
- Gemini providers translate roles, content, and tools (WebSearch becomes `googleSearch`, WebFetch becomes `urlContext`) and strip JSON-schema keys Gemini does not support.

The trade-off is clear. Adding a new upstream type means writing or extending an adapter. But clients never change, and routing and fallback never change, because they only ever see the Anthropic shape. You pay the cost once, in one place. See [../reference/providers.md](../reference/providers.md).

## Background OAuth refresh

Some OAuth providers can be configured but idle, serving no traffic. Their tokens would expire quietly and force a full re-login the next time you need them. To avoid that, the server runs a background loop. It polls every 20 minutes and refreshes any token whose remaining life is under 25 minutes. The loop uses one HTTP client with a 30s timeout (injected into the refresh path) so a hung provider cannot stall the whole refresh. Tokens live in `~/.claude-code-mux/oauth_tokens.json` with `0600` permissions. Writes are atomic: the store writes to a sibling temp file created `0600` from the start, `fsync`s it, then renames it over the target, so a crash mid-write can never truncate or lose the existing tokens.

## Security model

`ccm` splits its routes into a **control plane** (`/api/*` and the admin UI) and a **data plane** (`/v1/*`), and defends them differently because they have different threat models.

The control plane can rewrite config, run OAuth flows, and restart the process, so it is the sensitive surface. When no `api_key` is configured it has no per-request auth and leans entirely on the loopback bind. To keep that assumption from silently breaking, the server has a bind-time gate and a request-time gate that share one predicate (`control_plane_requires_loopback` in `src/server`): bind-time, it refuses to start if `host` is non-loopback and no `api_key` is set; request-time, a CSRF guard rejects any control-plane request whose `Host` header is not a loopback authority. The shared predicate means the two gates cannot drift out of agreement.

The data plane authenticates with the `api_key` header, so it is not a CSRF target and gets no Origin check. But when no `api_key` is set those routes are open, so a separate `data_plane_rebinding_guard` applies the same loopback-`Host` requirement to `/v1/*`. Without it, a DNS-rebinding page could point an attacker hostname at `127.0.0.1` and drive `/v1/*` to spend tokens and read model output. When an `api_key` IS set, the key is the gate and the guard is a no-op.

One more sharp edge: an empty-string `api_key` (often from an unset environment variable) is `Some("")`, which would pass a naive "is a key set?" check while failing every `is_none()` gate above. The config loader normalizes a blank or whitespace-only `api_key` to `None` before any gate reads it, so all three gates agree and fail closed (loopback-only).

## Trade-offs

- **One adapter per upstream.** New upstream shapes cost real work, but the cost is contained and clients stay simple.
- **Config is read at startup.** The server loads the TOML config once and does not reload it until restart. This keeps request handling fast and predictable (no per-request config reads, no mid-flight config races), but it means config changes need a restart to take effect. The admin UI works around this by caching state client-side and syncing on an explicit save.

## See also

- [../reference/routing.md](../reference/routing.md) - routing config reference.
- [../reference/providers.md](../reference/providers.md) - provider config reference.
- [../explanation/provider-fallback.md](../explanation/provider-fallback.md) - how fallback and cooldowns work.
