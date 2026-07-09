# Provider Reference

`ccm` routes requests to many upstream providers. You pick one with the `provider_type` field on a `[[providers]]` entry. This page lists every recognized type, its upstream format, default base URL, and supported auth modes. An unknown `provider_type` is an error at startup.

| provider_type | Upstream format | Default base URL | Auth modes |
| --- | --- | --- | --- |
| `anthropic` | Anthropic Messages | `https://api.anthropic.com` | api_key, passthrough |
| `z.ai` | Anthropic Messages | `https://api.z.ai/api/anthropic` | api_key, passthrough |
| `minimax` | Anthropic Messages | `https://api.minimax.io/anthropic` | api_key, passthrough |
| `zenmux` | Anthropic Messages | `https://zenmux.ai/api/anthropic` | api_key, passthrough |
| `kimi-coding` | Anthropic Messages | `https://api.kimi.com/coding` | api_key, passthrough |
| `vllm` | Anthropic Messages | `http://localhost:8000` | api_key, passthrough |
| `sglang` | Anthropic Messages | `http://localhost:30000` | api_key, passthrough |
| `openai` | OpenAI Chat Completions / Responses | `https://api.openai.com/v1` | api_key, oauth, passthrough |
| `nvidia-nim` | OpenAI Chat Completions | `https://integrate.api.nvidia.com/v1` | api_key, passthrough |
| `openrouter` | OpenAI Chat Completions | `https://openrouter.ai/api/v1` | api_key, passthrough |
| `deepinfra` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `novita` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `baseten` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `together` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `fireworks` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `groq` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `nebius` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `cerebras` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `moonshot` | OpenAI Chat Completions | provider preset | api_key, passthrough |
| `gemini` | Google (AI Studio or Code Assist) | `https://generativelanguage.googleapis.com/v1beta` | api_key, oauth |
| `vertex-ai` | Google Vertex AI | `https://{location}-aiplatform.googleapis.com/v1` | ADC |
| `copilot` | OpenAI Chat Completions | GitHub Copilot backend | oauth |

## Anthropic-format providers

These speak the Anthropic Messages API natively, so `ccm` passes requests through without translation. The types are `anthropic`, `z.ai`, `minimax`, `zenmux`, `kimi-coding`, `vllm`, and `sglang`.

Only the `anthropic` type (when the provider name is `anthropic`) does real upstream token counting at `/v1/messages/count_tokens`. The others estimate token counts.

`vllm` and `sglang` are Anthropic-compatible providers ([AnthropicCompatibleProvider](https://docs.anthropic.com/en/docs/ai-safety-and-controls/bedrock/bedrock-anthropic-compatible-api)). They connect to self-hosted vLLM (0.8+) and SGLang (0.4+) instances respectively. [vLLM docs](https://docs.vllm.com/). [SGLang docs](https://sgl-project.github.io/).

## OpenAI-format providers

These translate between the Anthropic Messages API and the OpenAI Chat Completions API.

`openai` picks its endpoint at runtime:

- Chat Completions (`/chat/completions`) by default.
- Responses API (`/responses`) when the model name contains `codex`.
- The ChatGPT Codex backend (`https://chatgpt.com/backend-api`) when the provider uses OAuth.

`nvidia-nim` (base `https://integrate.api.nvidia.com/v1`) is the canonical rate-limited provider. NVIDIA NIM often allows 40 requests per minute, so set `rate_limit_rpm = 40` and let `ccm` fail over when the budget runs out.

`openrouter` (base `https://openrouter.ai/api/v1`) adds `HTTP-Referer` and `X-Title` headers on each request.

`deepinfra`, `novita`, `baseten`, `together`, `fireworks`, `groq`, `nebius`, `cerebras`, and `moonshot` are OpenAI-compatible presets. Each has its own distinct base URL and uses api-key auth.

## Google

`gemini` runs in one of two modes:

- API-key mode (AI Studio), base `https://generativelanguage.googleapis.com/v1beta`. The key goes in the `?key=` query parameter.
- OAuth mode (Code Assist), base `https://cloudcode-pa.googleapis.com/v1internal`.

See [gemini-integration.md](../gemini-integration.md) for setup.

`vertex-ai` uses Application Default Credentials (ADC). Set `project_id` and `location`. The base URL is `https://{location}-aiplatform.googleapis.com/v1`.

## GitHub Copilot

`copilot` uses OAuth through the GitHub device-code flow. It supports `actual_model = "auto"`, which resolves against Copilot's `/models` API. `ccm` caches that model list for 10 minutes.

## Choosing auth_type

| auth_type | How it authenticates | Use when |
| --- | --- | --- |
| `api_key` | Uses the configured `api_key` (supports `$ENV_VAR`). | You have a static API key for the provider. |
| `oauth` | Uses a stored token named by `oauth_provider`, set up through the admin UI / OAuth flow. | The provider uses OAuth (Copilot, ChatGPT Codex, Gemini Code Assist). |
| `passthrough` | Forwards the caller's `Bearer` token upstream. | You want clients to supply their own upstream token. |

Passthrough is honored only for passthrough-type providers. A token containing control characters is rejected.

## Related

- [Configuration Reference](../reference/configuration.md)
- [Gemini Integration](../gemini-integration.md)
- [OAuth Setup](../OAUTH_SETUP.md)
