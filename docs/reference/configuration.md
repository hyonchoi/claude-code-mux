# Configuration Reference

`ccm` reads a single TOML config file. By default it lives at `~/.claude-code-mux/config.toml`, and `ccm` creates it if it is missing. You point at a different file with `--config <path>`. This page lists every table and field you can set.

## [server]

All fields are optional.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `port` | u16 | `13456` | TCP port the proxy listens on. The generated default config and all examples use `13456`. (The code default is `3456` and applies only if you delete the field.) |
| `host` | String | `"127.0.0.1"` | Address the proxy binds to. With no `api_key` set, `ccm` refuses to bind to a non-loopback address and exits with an error — see [Security](#security-binding-and-api_key). |
| `api_key` | String | unset | If set, every protected request (the `/v1/*` and `/api/*` routes) must send this key. If unset, the proxy stays loopback-only (it binds only to `127.0.0.1`/`::1` and rejects requests whose `Host` header is not a loopback authority). A blank or whitespace-only value is treated as unset. See [Security](#security-binding-and-api_key). |
| `log_level` | String | `"info"` | Log verbosity for the server. |

When `api_key` is set, a client presents it with either header:

```
X-Api-Key: your-secret-key
Authorization: Bearer your-secret-key
```

The comparison runs in constant time. The value supports environment variable substitution (a value starting with `$`).

### Security: binding and api_key

`ccm` separates two route groups and protects them differently:

- **Control plane** (`/api/*`, the admin UI) is the sensitive surface: it can rewrite config, drive OAuth, and restart the process. With no `api_key` it has no per-request auth, so it relies on the loopback bind. To keep that guarantee honest, `ccm` **refuses to start** (exits with an error) when `host` is a non-loopback address and `api_key` is unset. Set an `api_key` before binding to `0.0.0.0` or a LAN address.
- **Data plane** (`/v1/*`) authenticates with the `api_key` header. When no `api_key` is set, those routes are open, so `ccm` applies a DNS-rebinding guard: it serves them only when the request `Host` header is a loopback authority. This stops a malicious web page from rebinding an attacker hostname to `127.0.0.1` and driving `/v1/*` to spend your tokens. When an `api_key` IS set, the key is the gate and non-loopback `Host` values are allowed.

A blank or whitespace-only `api_key` (including one resolved from an empty environment variable) is normalized to **unset** before any of these gates run, so an empty key never silently disables auth while passing the "key is set" check. `ccm` logs a warning when it does this.

## [server.timeouts]

All fields are optional.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `api_timeout_ms` | u64 | `600000` | Upstream request timeout in milliseconds (10 minutes). |
| `connect_timeout_ms` | u64 | `10000` | Connection timeout in milliseconds (10 seconds). |

## [router]

`default` is required. The rest are optional.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `default` | String | required | Fallback model name used when no other rule matches. |
| `subagent` | String | unset | Overrides the `<CCM-SUBAGENT-MODEL>` tag. |
| `background` | String | unset | Model for background requests. |
| `think` | String | unset | Model for thinking/reasoning requests. |
| `websearch` | String | unset | Model for web-search requests. |
| `auto_map_regex` | String | `"^claude-"` | Regex that decides which incoming model names get auto-mapped. Empty or missing falls back to `^claude-`. |
| `background_regex` | String | `"(?i)claude.*haiku"` | Regex that flags a request as background. Empty or missing falls back to `(?i)claude.*haiku`. |

## [[providers]]

An array of upstream providers. Each provider is referenced by name from model mappings.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | String | required | Unique name. Model mappings reference this. |
| `provider_type` | String | required | Upstream type. See [providers.md](../reference/providers.md). |
| `auth_type` | enum | `"api_key"` | One of `api_key`, `oauth`, `passthrough`. |
| `api_key` | String | unset | Required when `auth_type = "api_key"`. Supports `"$ENV_VAR"`. |
| `oauth_provider` | String | unset | Required when `auth_type = "oauth"`. Names a stored token. |
| `base_url` | String | per-type default | Upstream base URL. Each `provider_type` has its own default. |
| `enabled` | bool | `true` | Set `false` to skip this provider at startup. |
| `rate_limit_rpm` | u32 | unset | Requests-per-minute budget. `0` is rejected for enabled providers. |
| `rate_limit_max_wait_ms` | u64 | `2000` (when rpm set) | When the rate-limit bucket is empty, a request waits up to this long, then fails over to the next mapping. `0` is rejected. |
| `supported_beta_options` | array of String | `[]` | Beta options this provider accepts. |
| `project_id` | String | unset | Vertex AI only. |
| `location` | String | unset | Vertex AI only. |
| `models` | array | unset | Legacy and deprecated. Prefer `[[models]]` mappings. |

## [[models]] and [[models.mappings]]

`[[models]]` declares an external model name your clients send. Each model holds an ordered list of `[[models.mappings]]`.

`[[models]]` fields:

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | String | required | The external model name clients send in the API request. |

`[[models.mappings]]` fields:

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `priority` | u32 | required | Try order. `1` is highest priority and is tried first. |
| `provider` | String | required | References a `[[providers]].name`. |
| `actual_model` | String | required | The model name sent upstream to that provider. |
| `strip_beta_options` | bool | `false` | Strip all beta options before sending upstream. |
| `strip_specific_beta` | array of String | `[]` | Strip only these beta options. Overrides `strip_beta_options` when set. |

## Model resolution

Resolution runs in this order:

1. A client sends a request with model `X`.
2. The router may rewrite `X` (for example to the `background` or `subagent` model) based on the `[router]` rules and regexes.
3. The server finds the `[[models]]` entry whose `name` equals the final model.
4. It sorts that model's mappings by `priority` ascending.
5. It tries each mapping in order. It skips any provider currently on cooldown and fails over to the next mapping on error.
6. It returns the first success.

To force one provider, send the `X-Provider: <name>` request header. The server then uses only the mappings that point at that provider.

## Environment variable substitution

Any `api_key` or `server.api_key` value that starts with `$` is read from that environment variable. For example, `api_key = "$OPENAI_API_KEY"` reads `OPENAI_API_KEY` from the environment.

A missing environment variable for an enabled provider is a hard error at startup. Disabled providers are skipped, so their variables can be absent.

## Minimal working example

```toml
[server]
port = 13456
host = "127.0.0.1"
log_level = "info"

[router]
default = "claude-sonnet"

[[providers]]
name = "anthropic"
provider_type = "anthropic"
auth_type = "api_key"
api_key = "$ANTHROPIC_API_KEY"

[[models]]
name = "claude-sonnet"

  [[models.mappings]]
  priority = 1
  provider = "anthropic"
  actual_model = "claude-sonnet-4-20250514"
```

## Related

- [Provider Reference](../reference/providers.md)
- [Routing Reference](../reference/routing.md)
- [Architecture](../explanation/architecture.md)
