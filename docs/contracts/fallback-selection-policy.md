# Fallback Candidate Selection Policy

## Ordering Rule

Provider mappings for a model are sorted by **static priority** (ascending integer).
Lower priority number = tried first.

TOML example:
```toml
[[models]]
name = "claude-sonnet-4-5"

[[models.mappings]]
provider = "anthropic-primary"
actual_model = "claude-sonnet-4-5-20251022"
priority = 1

[[models.mappings]]
provider = "anthropic-backup"
actual_model = "claude-sonnet-4-5-20251022"
priority = 2
```

## Tie-Break Rule

When two mappings have equal priority, they are ordered by **stable lexical sort
on provider name** (ascending, byte order). This is the final tie-break; no
further randomization occurs.

Example: `anthropic-a` sorts before `anthropic-b`.

## X-Provider Header Override

A request may include `X-Provider: <name>` to force a specific provider,
bypassing the priority order. If the named provider is not in the model's
mapping list, the router returns a 400 RoutingError.

## What "Health" Means — Current Behavior

**Health state is not currently tracked.** Candidate ordering is static
(priority + lexical). There is no runtime health-score that changes based
on recent errors or latency.

Health-aware tie-breaking (e.g., "prefer provider with fewer recent errors")
is explicitly deferred. See TODOS.md for the follow-up item.

## Temporary Cooldowns

Separately from the static ordering, providers that return certain error
codes are placed on a temporary in-memory cooldown. Providers on cooldown
are skipped in the fallback loop for the duration. Cooldowns are lost on
server restart.

- **401 / 403 (auth)** - 240s cooldown.
- **429 (rate limit)** - 120s cooldown.
- **502 (bad gateway / empty response)** - 60s cooldown. This includes synthetic 502s the proxy generates when a provider returns HTTP 200 with an empty `choices` array.

This is an operational skip mechanism, not a health score. It does not change
the static priority order.
