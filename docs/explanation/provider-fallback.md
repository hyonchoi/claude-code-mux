# Provider Fallback and Cooldowns

## The problem

Any single upstream can let you down. It can rate-limit you, return an auth error, or just go down. If the proxy gave up the moment one provider failed, every blip upstream would become a failed request for your client. You want a request to fail over to the next provider on its own, so a struggling endpoint does not take the whole request with it.

## Priority-ordered mappings

Each `[[models]]` name maps to a list of `(provider, actual_model)` mappings, and each mapping has a priority. The server sorts by priority and tries them in order, priority 1 first. For each mapping it sets the upstream model name, picks the provider's adapter, translates the request, and sends it. The first success returns to the client. Any error moves on to the next mapping. If every mapping fails, the proxy returns an error that lists every failure so you can see what went wrong everywhere, not just at the last hop.

See [../reference/routing.md](../reference/routing.md) for the mapping config, and [../contracts/fallback-selection-policy.md](../contracts/fallback-selection-policy.md) for the exact selection contract.

## Cooldowns

Failing over is good, but blindly retrying a provider that just rejected you wastes time on every request. So a provider that recently failed gets put on a cooldown, and while it is on cooldown the resolver skips it entirely. That stops you from hammering a struggling endpoint and makes the next request fail over faster.

The cooldown length depends on the failure:

- **401 / 403 (auth)** - 240s cooldown. Auth problems do not fix themselves in a few seconds.
- **429 (rate limit)** - 120s cooldown. Back off, then try again sooner than an auth failure.
- **Other errors** - fail over to the next mapping, but do NOT cooldown. A one-off error should not sideline a provider.

Cooldowns are in-memory and keyed per provider name. They are shared across every model that routes to that provider, so if `provider-a` is cooling down, every model that lists `provider-a` skips it until the cooldown clears.

## Rate limiting vs cooldowns

Cooldowns react to upstream failures. Rate limiting is something the proxy does to itself on purpose, and the two are not the same thing.

A provider with `rate_limit_rpm` set gets a token-bucket limiter. When the bucket is empty, a request waits for a token up to `rate_limit_max_wait_ms` (default 2000). If no token arrives in that window, the request returns a `RateLimitTimeout` so it can fail over to the next mapping instead of blocking forever.

A `RateLimitTimeout` does NOT trigger a cooldown. The provider did not fail; you simply chose not to wait on your own rate limit any longer. Cooling it down would punish a healthy provider for your throttle setting, so it just fails over and the provider stays eligible for the next request.

## The streaming boundary

This is the most important trade-off in fallback. Fallover is only possible BEFORE the first byte of a streamed response goes out.

Each provider checks the upstream HTTP status before it returns the stream. A non-2xx upstream becomes an error right there, and that error fails over normally, just like a non-streaming failure. So far so good.

But once the stream has started (the provider has returned `Ok(stream)`), the response to the client has already begun. An error mid-stream cannot fail over to another provider. It surfaces as an error inside the SSE body instead.

This is deliberate. You cannot retry a half-sent stream on a different provider without confusing the client, which has already received part of a response. So the boundary is the first byte: before it, fail over freely; after it, the request is committed to that provider. See [../contracts/streaming-fallback-boundary.md](../contracts/streaming-fallback-boundary.md) for the exact contract.

## Forcing a provider with X-Provider

Sometimes you want to bypass fallback and hit exactly one provider, usually for testing. The `X-Provider` request header does that. It restricts resolution to that provider's mappings, so the request either uses that provider or fails. There is no fall-through to others.

## Trade-offs

- **Cooldowns are in-memory.** They are fast and need no storage, but they reset on restart and are not shared across multiple `ccm` instances.
- **Per-provider, shared across models.** One cooldown covers every model that uses the provider. That is usually what you want (a down provider is down for everyone), but it means one model's failure can sideline the provider for another model too.
- **The streaming commitment.** Failing over after the first byte is impossible by design. A provider that fails early in a long stream cannot be rescued; the client sees the error in the SSE body.
- **X-Provider removes the safety net.** Forcing a provider is great for testing, but you give up fallback for that request.

## See also

- [../reference/routing.md](../reference/routing.md) - mapping and provider config reference.
- [../contracts/fallback-selection-policy.md](../contracts/fallback-selection-policy.md) - the fallback selection contract.
- [../contracts/streaming-fallback-boundary.md](../contracts/streaming-fallback-boundary.md) - the streaming boundary contract.
