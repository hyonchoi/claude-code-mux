# Test Plan: NVIDIA NIM Anthropic-Compatible Migration (additive)

Branch: feat/bearer-auth-type

## New codepaths under test

1. `AnthropicCompatibleProvider::nvidia_nim_anthropic_with_auth` constructor (new)
2. `registry.rs` `"nvidia-nim-anthropic"` match arm (new)
3. Shared `usage`-field-missing tolerant parsing fix in `AnthropicCompatibleProvider` response handling (new, but affects existing z.ai/minimax/vllm/sglang/zenmux/kimi-coding backends too)

## Required tests

- [ ] `test_nvidia_nim_anthropic_uses_bearer_auth` — construct with `AuthType::ApiKey`, assert `is_bearer_auth() == true` (registry.rs or anthropic_compatible.rs, mirrors existing `test_vllm_uses_bearer_auth`-style test)
- [ ] `test_nvidia_nim_anthropic_default_base_url` — no `base_url` configured, assert constructed request URL is `https://integrate.api.nvidia.com/v1/messages`, NOT `.../v1/v1/messages`
- [ ] `test_nvidia_nim_openai_compat_unaffected` — existing `"nvidia-nim"` provider_type still constructs `OpenAIProvider` and still targets `/v1/chat/completions` — regression guard proving the existing path is untouched
- [ ] `test_anthropic_compatible_response_missing_usage_field` — feed a synthetic Anthropic-shaped JSON response with no `usage` key through the parser, assert it succeeds with `Usage { input_tokens: 0, output_tokens: 0 }` instead of erroring — covers all `AnthropicCompatibleProvider` backends, not just NIM
- [ ] `test_nvidia_nim_anthropic_excluded_from_passthrough` — `should_use_passthrough_auth` returns `false` for a `"nvidia-nim-anthropic"` config even with `auth_type = Passthrough` set — extends the existing passthrough-eligibility test table in `src/server/mod.rs`

## Manual / follow-up verification (not automatable in CI)

- Real request against `integrate.api.nvidia.com/v1/messages` with the user's verified model — confirm tool_use and streaming behavior match Anthropic Messages shape in practice (already partially done by the user per the CEO-phase premise gate).

## Verify commands

```
cargo test providers::anthropic_compatible
cargo test providers::registry
cargo test server::mod
cargo test
```
