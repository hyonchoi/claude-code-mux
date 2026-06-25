# HTTP API Reference

`ccm` serves its HTTP API on `server.host:server.port` (default `127.0.0.1:13456`). All examples below use `http://localhost:13456`.

A second small server listens on port `1455` only to catch the OpenAI Codex OAuth redirect at `/auth/callback`.

The API has two route groups:

- **Public routes** need no API key.
- **Protected routes** need `server.api_key`, but only if you configured one. If `server.api_key` is unset, the proxy stays loopback-only: it binds only to `127.0.0.1`/`::1` (and refuses to start on a non-loopback address without a key), and it rejects any request whose `Host` header is not a loopback authority. So "no key" means "open from this machine," not "open to the network." A blank or whitespace-only `server.api_key` is treated as unset.

## Authentication

Protected routes accept the key two ways:

- `X-Api-Key: <key>`
- `Authorization: Bearer <key>`

```bash
curl http://localhost:13456/v1/messages \
  -H "X-Api-Key: $CCM_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{ ... }'
```

If `server.api_key` is not set, you can omit these headers, but the request must reach the proxy over loopback. When no key is configured, both the inference (`/v1/*`) and admin (`/api/*`) routes answer only requests whose `Host` header is a loopback authority (`localhost`, `127.0.0.0/8`, `::1`); anything else gets a 403. This DNS-rebinding guard keeps a browser page from rebinding an attacker hostname to `127.0.0.1` and driving `/v1/*` to spend tokens. Set `server.api_key` to authenticate the data plane from non-loopback origins.

## Inference endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/v1/messages` | Main Anthropic Messages endpoint. Routes through the pipeline, applies model-to-provider mappings with failover, supports streaming and non-streaming. |
| POST | `/v1/messages/count_tokens` | Anthropic token-count endpoint. Returns the resolved provider's count, e.g. `{"input_tokens": N}`. |
| POST | `/v1/chat/completions` | OpenAI Chat Completions-compatible endpoint. Converts the OpenAI-style request to Anthropic internally, then routes. |

`/v1/messages` honors the `X-Provider` header (restrict to one provider's mappings) and the `anthropic-beta` header. For Claude Code CLI callers it can forward a caller Bearer token (passthrough). `/v1/messages/count_tokens` does not honor `X-Provider`.

`/v1/chat/completions` accepts an OpenAI-style body: `model`, `messages`, `max_tokens`, `temperature`, `top_p`, `stop`, `stream`, `tools`, `tool_choice`.

### Non-streaming /v1/messages

```bash
curl http://localhost:13456/v1/messages \
  -H "X-Api-Key: $CCM_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet-4",
    "max_tokens": 1024,
    "messages": [
      {"role": "user", "content": "Hello, ccm!"}
    ]
  }'
```

### Streaming /v1/messages

Set `"stream": true` to receive Server-Sent Events.

```bash
curl -N http://localhost:13456/v1/messages \
  -H "X-Api-Key: $CCM_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet-4",
    "max_tokens": 1024,
    "stream": true,
    "messages": [
      {"role": "user", "content": "Stream me a haiku."}
    ]
  }'
```

### Pin a provider with X-Provider

```bash
curl http://localhost:13456/v1/messages \
  -H "X-Api-Key: $CCM_API_KEY" \
  -H "X-Provider: openai" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet-4",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hi"}]
  }'
```

## Admin & config endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/config` | Returns `{server:{host,port}, router:{default,background,think,websearch,subagent}}`. |
| POST | `/api/config` | Form-encoded. Updates only the `[router]` section. Returns an HTML snippet. |
| GET | `/api/config/json` | Full config: server, router, and providers (API keys redacted to an `api_key_set` boolean) plus models. The admin UI loads this. |
| POST | `/api/config/json` | JSON body. Writes providers, models, and router back to the TOML file. |
| GET | `/api/providers` | Providers array with API keys redacted (`api_key` becomes the `api_key_set` boolean). |
| GET | `/api/models-config` | The models config array. |
| GET | `/api/models` | Removed. Returns an error pointing to `/api/models-config`. |
| POST | `/api/restart` | Restarts the server process. |

Notes:

- **Config changes apply only after a restart.** The server reads the TOML file once at startup, so call `POST /api/restart` after any config write.
- On `POST /api/config/json`, a provider sent with `api_key_set: true` and no `api_key` keeps its existing stored key.

```bash
curl -X POST http://localhost:13456/api/restart \
  -H "X-Api-Key: $CCM_API_KEY"
```

## OAuth endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/api/oauth/authorize` | Body `{oauth_type}` (`max` \| `console` \| `openai-codex` \| `gemini`). Returns `{url, verifier, instructions}`. |
| POST | `/api/oauth/exchange` | Body `{code, verifier, provider_id, oauth_type?}`. Exchanges the code for tokens and stores them. Returns `{success, message, provider_id, expires_at}`. |
| GET | `/api/oauth/tokens` | Returns `[{provider_id, expires_at, is_expired, needs_refresh}]`. |
| POST | `/api/oauth/tokens/refresh` | Body `{provider_id}`. Refreshes that token. |
| POST | `/api/oauth/tokens/delete` | Body `{provider_id}`. Deletes that token. |
| POST | `/api/oauth/copilot-start` | Starts the GitHub device flow. Returns `{device_code, user_code, verification_uri, expires_in, interval}`. |
| POST | `/api/oauth/copilot-exchange` | Body `{provider_id, device_code}`. One poll attempt. Returns `{status: success\|pending\|expired, ...}`. |

Two public OAuth landing routes need no API key:

- `GET /api/oauth/callback` shows the auth code to copy (Gemini).
- `GET /auth/callback` is the same handler for the OpenAI Codex redirect.

## Health check

`GET /health` is public and returns JSON.

```bash
curl http://localhost:13456/health
# {"status":"ok","service":"claude-code-mux"}
```

`GET /` (also public) serves the admin UI as HTML.

## Related

- [Configuration reference](../reference/configuration.md)
- [OAuth testing](../OAUTH_TESTING.md)
