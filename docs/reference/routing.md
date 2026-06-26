# Routing Reference

`ccm` picks a target model for each request by running a fixed priority chain. The order is: WebSearch > Subagent > Think > Background > Auto-map > Default. The first stage that matches wins. After a model name is chosen, the server resolves it to a provider through that model's mappings, with cooldown skipping and failover.

## Priority pipeline

`ccm` checks each stage in order. WebSearch, Subagent, Think, and Background each short-circuit (they return as soon as they match). Auto-map is different: it only rewrites the model name and then falls through to Default. Auto-map runs only if none of the earlier stages matched.

### 1. WebSearch

- **Trigger:** `router.websearch` is set, and the request's `tools` array contains any tool whose `type` starts with `web_search`.
- **Result:** Route to `router.websearch`. Short-circuits.

### 2. Subagent

The request system prompt may carry a `cc_is_subagent=true` flag in any system block (typically the billing header block containing `x-anthropic-billing-header`). Legacy `<CCM-SUBAGENT-MODEL>` tags are stripped from `Blocks`-style system prompts for backward compatibility.

- **Trigger:** the `cc_is_subagent=true` flag is present in any system prompt block.
- **Result:**
  - If `router.subagent` is configured, route to `router.subagent`. Short-circuits.
  - Otherwise, the request falls through to Think, Background, Auto-map, and Default stages with the original model name.

### 3. Think

- **Trigger:** `router.think` is set, and the request has `thinking.type == "enabled"` (Claude Code Plan Mode).
- **Result:** Route to `router.think`. Short-circuits.

### 4. Background

- **Trigger:** `router.background` is set, and the model name matches `background_regex` (default `(?i)claude.*haiku`).
- **Result:** Route to `router.background`. Short-circuits.

This stage checks the original model name, captured before auto-map.

### 5. Auto-map

- **Trigger:** `auto_map_regex` (default `^claude-`) matches the request model, and the model is not an explicitly defined model.
- **Result:** Rewrite `request.model` to `router.default`, then fall through to Default. This stage never returns on its own.

**Explicitly defined model exception:** if the model name appears as a `[[models]].name` entry, the rewrite is skipped. The model keeps its name and resolves through its own mappings.

Notes on the regex: an empty or missing `auto_map_regex` falls back to `^claude-`. A custom invalid regex logs a warning and also falls back to `^claude-`.

### 6. Default

- **Trigger:** always (fallback).
- **Result:** Route to the current request model name. That is the auto-mapped value if it was rewritten, otherwise the original or defined name. The name resolves through that model's `[[models]]` mappings.

## Model resolution and fallback

Once routing produces a final model name, the server resolves it to a provider:

1. Find the `[[models]]` entry whose `name` matches the final model name.
2. Sort that entry's mappings by `priority` ascending (priority `1` is tried first).
3. Try each provider in order. Skip any provider that is on cooldown. On error, fail over to the next mapping.
4. Return the first success.

**X-Provider header.** Send `X-Provider: <name>` to restrict resolution to only that provider's mappings. The `count_tokens` path does not honor this override.

**Cooldowns.** When a provider fails, `ccm` may put it on a short cooldown so later requests skip it:

| Response | Cooldown |
| --- | --- |
| 401 / 403 | 240 seconds |
| 429 | 120 seconds |
| Other errors | No cooldown |

A `RateLimitTimeout` (the rate-limit bucket stays empty past the max wait) does not trigger a cooldown.

## Worked examples

These walkthroughs assume `router.default = "default-model"` and the defaults `auto_map_regex = "^claude-"` and `background_regex = "(?i)claude.*haiku"`.

### A haiku request hits Background before Auto-map

Request model: `claude-3-5-haiku-20241022`. `router.background` is set to `background-model`.

1. WebSearch: no `web_search` tool. Skip.
2. Subagent: no cc_is_subagent flag. Skip.
3. Think: no `thinking.enabled`. Skip.
4. Background: name matches `(?i)claude.*haiku`. **Route to `background-model`.** Short-circuits.

Auto-map never runs, even though the name also matches `^claude-`.

### A defined claude-* model skips Auto-map

Request model: `claude-sonnet-4`, and a `[[models]]` entry has `name = "claude-sonnet-4"`. `router.background` is unset (or the name does not match the background regex).

1. WebSearch, Subagent, Think: skip.
2. Background: not set, or no match. Skip.
3. Auto-map: name matches `^claude-`, but the model is explicitly defined, so the rewrite is **skipped**.
4. Default: route to `claude-sonnet-4`, resolved through its own `[[models]]` mappings.

### An undefined claude-* model is auto-mapped to the default

Request model: `claude-opus-4-20250101`, with no matching `[[models]]` entry and no background match.

1. WebSearch, Subagent, Think, Background: skip.
2. Auto-map: name matches `^claude-` and is not defined, so rewrite the model to `default-model`. Fall through.
3. Default: route to `default-model`, resolved through its mappings.

### A non-claude model falls straight to Default

Request model: `gpt-4o`.

1. WebSearch, Subagent, Think, Background: skip.
2. Auto-map: name does not match `^claude-`. No rewrite.
3. Default: route to `gpt-4o`, resolved through its `[[models]]` mappings.

## Related

- [Routing design](../explanation/routing-design.md)
- [Configuration reference](../reference/configuration.md)
- [Provider fallback](../explanation/provider-fallback.md)
