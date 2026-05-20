# Resolve All TODOs — Full Sweep Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clear all 11 open TODOS.md items — 2 P1 contracts, 5 P2 spec docs + 1 P2 code feature, 3 P3 code fixes — in a single PR.

**Architecture:** Docs-first (P1 contracts → P2 specs → code). Written contracts drive the OAuth refresh generalization and provider deactivation implementation. Code changes are confined to `src/server/mod.rs` and `src/server/admin.html`.

**Tech Stack:** Rust (axum, dashmap, tokio, chrono), vanilla JS (UIkit 3), Markdown for contracts

---

## File Structure

**New files:**
- `docs/contracts/rollback-contract.md` — P1 rollback toggle policy
- `docs/contracts/fallback-selection-policy.md` — P1 deterministic fallback ordering
- `docs/contracts/slo-contract.md` — P2 observability SLOs
- `docs/contracts/escalation-sla.md` — P2 checkpoint escalation chain
- `docs/contracts/benchmark-protocol.md` — P2 p95/p99 measurement protocol
- `docs/contracts/auth-validation-spec.md` — P2 incoming auth validation
- `docs/contracts/streaming-fallback-boundary.md` — P2 streaming boundary invariant

**Modified files:**
- `src/server/mod.rs` — cooldown helpers + DashMap in AppState + 3 Err branch patches + OAuth refresh generalization
- `src/server/admin.html` — `escapeJs()` function + onclick fix + `loadConfig()` response.ok fix
- `TODOS.md` — mark all 11 items resolved

---

## Task 1: Rollback Contract Document (P1)

**Files:**
- Create: `docs/contracts/rollback-contract.md`

- [ ] **Step 1: Create the contract file**

Write the following content to `docs/contracts/rollback-contract.md`:

```markdown
# Rollback Contract: Passthrough / Fallback Toggle

## Toggle Key

Config path: `server.enable_fallback` (boolean, default: `true`)

Full TOML example:
```toml
[server]
enable_fallback = false   # disable fallback/passthrough routing
```

## Scope

Global — applies to all providers and all model mappings simultaneously.
Per-provider granularity is out of scope for this contract.

## Propagation Semantics

**Restart-based. Hard stop. No graceful drain.**

- The toggle takes effect only after a server restart (`ccm` process restart).
- In-flight requests at the time of restart are dropped by the OS; the caller
  receives a connection reset. This is consistent with the existing restart
  behavior for all config changes.
- There is no graceful drain window. Adding one would require TOML + in-memory
  coordination, which is self-contradictory (TOML is durable; graceful drain
  requires in-memory state that survives past the config reload boundary).

## Verification Steps

1. Set `enable_fallback = false` in your TOML config file.
2. Send a POST `/api/restart` or restart the `ccm` process.
3. Send a request that would normally trigger fallback routing.
4. Expected: router returns a 502/503 with no fallback attempted.
5. Confirm in server logs: no `🔄 Trying mapping` lines appear after the restart.

## Out of Scope

- UI toggle in admin.html (TOML config only)
- Per-provider granularity
- Graceful drain / in-flight request completion
```

- [ ] **Step 2: Verify acceptance criteria**

Check the file contains all required elements:
  - Toggle key named (`enable_fallback`)
  - Scope defined (global)
  - Propagation semantics stated (restart-based, hard stop, no graceful drain)
  - Verification steps listed (5 steps)

- [ ] **Step 3: Commit**

```bash
git add docs/contracts/rollback-contract.md
git commit -m "docs: add P1 rollback contract — TOML toggle, restart-based, hard stop"
```

---

## Task 2: Fallback Selection Policy Document (P1)

**Files:**
- Create: `docs/contracts/fallback-selection-policy.md`

- [ ] **Step 1: Create the contract file**

Write the following content to `docs/contracts/fallback-selection-policy.md`:

```markdown
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

## Temporary Cooldowns (4xx Errors)

Separately from the static ordering, providers that return 401, 403, or 429
are placed on a temporary in-memory cooldown (60s for 401/403, 30s for 429).
Providers on cooldown are skipped in the fallback loop for the duration.
Cooldowns are lost on server restart.

This is an operational skip mechanism, not a health score. It does not change
the static priority order.
```

- [ ] **Step 2: Verify acceptance criteria**

Check the file contains:
  - Ordering rule stated (priority ascending)
  - Tie-break defined (lexical on provider name)
  - Explicitly states health state is deferred (not aspirational — current behavior only)
  - Cooldown mechanism documented

- [ ] **Step 3: Commit**

```bash
git add docs/contracts/fallback-selection-policy.md
git commit -m "docs: add P1 fallback selection policy — static priority + lexical tie-break"
```

---

## Task 3: Provider Deactivation — Helper Functions + Unit Tests (P1/P2)

This task introduces the two pure helper functions used by the cooldown feature,
with all 6 unit tests written before the implementation.

**Files:**
- Modify: `src/server/mod.rs`

**Background:** `ProviderError` is in `src/providers/error.rs`.
The variant you'll match is `ProviderError::ApiError { status: u16, message: String }`.
`dashmap` is already in `Cargo.toml` at line 58.

- [ ] **Step 1: Write 6 failing tests**

At the bottom of the `mod tests { ... }` block in `src/server/mod.rs` (after line 2216),
add these tests:

```rust
#[test]
fn test_cooldown_for_4xx_returns_60s_for_401() {
    let e = crate::providers::error::ProviderError::ApiError {
        status: 401,
        message: "Unauthorized".into(),
    };
    assert_eq!(cooldown_for_4xx(&e), Some(std::time::Duration::from_secs(60)));
}

#[test]
fn test_cooldown_for_4xx_returns_30s_for_429() {
    let e = crate::providers::error::ProviderError::ApiError {
        status: 429,
        message: "Too Many Requests".into(),
    };
    assert_eq!(cooldown_for_4xx(&e), Some(std::time::Duration::from_secs(30)));
}

#[test]
fn test_cooldown_for_4xx_returns_none_for_500() {
    let e = crate::providers::error::ProviderError::ApiError {
        status: 500,
        message: "Internal Server Error".into(),
    };
    assert_eq!(cooldown_for_4xx(&e), None);
}

#[test]
fn test_cooldown_for_4xx_returns_none_for_non_api_error() {
    let e = crate::providers::error::ProviderError::AuthError("token missing".into());
    assert_eq!(cooldown_for_4xx(&e), None);
}

#[test]
fn test_is_on_cooldown_false_when_map_is_empty() {
    let cooldowns: dashmap::DashMap<String, std::time::Instant> = dashmap::DashMap::new();
    assert!(!is_on_cooldown(&cooldowns, "my-provider"));
}

#[test]
fn test_is_on_cooldown_true_when_cooldown_is_active() {
    let cooldowns: dashmap::DashMap<String, std::time::Instant> = dashmap::DashMap::new();
    cooldowns.insert(
        "my-provider".to_string(),
        std::time::Instant::now() + std::time::Duration::from_secs(60),
    );
    assert!(is_on_cooldown(&cooldowns, "my-provider"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test test_cooldown_for_4xx test_is_on_cooldown 2>&1 | tail -20
```

Expected: compile error — `cooldown_for_4xx` and `is_on_cooldown` not found.

- [ ] **Step 3: Add the two helper functions to `src/server/mod.rs`**

Place these two functions directly below `build_refreshed_copilot_token` (around line 55),
before `start_server`:

```rust
/// Returns the cooldown duration when a provider returns a triggering 4xx error.
/// 401/403 → 60 seconds, 429 → 30 seconds, all others → None (no deactivation).
fn cooldown_for_4xx(
    e: &crate::providers::error::ProviderError,
) -> Option<std::time::Duration> {
    if let crate::providers::error::ProviderError::ApiError { status, .. } = e {
        match *status {
            401 | 403 => Some(std::time::Duration::from_secs(60)),
            429 => Some(std::time::Duration::from_secs(30)),
            _ => None,
        }
    } else {
        None
    }
}

fn is_on_cooldown(
    cooldowns: &dashmap::DashMap<String, std::time::Instant>,
    provider: &str,
) -> bool {
    cooldowns
        .get(provider)
        .map_or(false, |until| std::time::Instant::now() < *until)
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test test_cooldown_for_4xx test_is_on_cooldown 2>&1 | tail -20
```

Expected: 6 tests pass.

- [ ] **Step 5: Run full test suite**

```bash
cargo test 2>&1 | tail -10
```

Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/server/mod.rs
git commit -m "feat: add cooldown_for_4xx and is_on_cooldown helpers with 6 unit tests"
```

---

## Task 4: Provider Deactivation — AppState + Err Branch Patches (P1/P2)

Add `provider_cooldowns` to `AppState`, initialize it in `start_server`, then patch
all 3 fallback Err branches across the three handlers (OpenAI-compat ~946, messages ~1198,
count_tokens ~1518).

**Files:**
- Modify: `src/server/mod.rs`

- [ ] **Step 1: Add `provider_cooldowns` field to the `AppState` struct**

The struct is at line 106. Change it from:

```rust
pub struct AppState {
    pub config: AppConfig,
    pub router: Router,
    pub provider_registry: Arc<ProviderRegistry>,
    pub token_store: TokenStore,
    pub config_path: std::path::PathBuf,
}
```

to:

```rust
pub struct AppState {
    pub config: AppConfig,
    pub router: Router,
    pub provider_registry: Arc<ProviderRegistry>,
    pub token_store: TokenStore,
    pub config_path: std::path::PathBuf,
    pub provider_cooldowns: Arc<dashmap::DashMap<String, std::time::Instant>>,
}
```

- [ ] **Step 2: Initialize `provider_cooldowns` in the `AppState` constructor**

The constructor is at line 180. Change:

```rust
    let state = Arc::new(AppState {
        config: config.clone(),
        router,
        provider_registry,
        token_store,
        config_path,
    });
```

to:

```rust
    let state = Arc::new(AppState {
        config: config.clone(),
        router,
        provider_registry,
        token_store,
        config_path,
        provider_cooldowns: Arc::new(dashmap::DashMap::new()),
    });
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo build 2>&1 | grep -E 'error|warning.*unused' | head -20
```

Expected: no errors (may be missing field warnings in test helpers — fix those if present).

- [ ] **Step 4: Patch Err branch in handler 1 — OpenAI-compat (`handle_openai_chat_completions`)**

The fallback loop is at line ~946. There is one Err branch at ~line 994.
Add the cooldown skip check at the TOP of the mapping loop body (before the provider
registry lookup), and add the cooldown set inside the Err arm.

Find the loop body that starts with:
```rust
            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
```

Add this block BEFORE it (but still inside the `for (idx, mapping) in sorted_mappings.iter().enumerate()` loop):

```rust
            if is_on_cooldown(&state.provider_cooldowns, &mapping.provider) {
                info!("⏭ Skipping provider {} (on cooldown)", mapping.provider);
                fallback_failures.push(format!("{}: on cooldown", mapping.provider));
                continue;
            }
```

Then find the Err arm in this handler that currently reads:
```rust
                    Err(e) => {
                        warn!(
                            "⚠️ Provider {} failed: {}, trying next fallback",
                            mapping.provider, e
                        );
                        fallback_failures.push(format!("{}: {}", mapping.provider, e));
                        continue;
                    }
```

Change it to:
```rust
                    Err(e) => {
                        warn!(
                            "⚠️ Provider {} failed: {}, trying next fallback",
                            mapping.provider, e
                        );
                        if let Some(duration) = cooldown_for_4xx(&e) {
                            state.provider_cooldowns.insert(
                                mapping.provider.clone(),
                                std::time::Instant::now() + duration,
                            );
                            warn!(
                                "⏸ Provider {} on cooldown for {}s",
                                mapping.provider,
                                duration.as_secs()
                            );
                        }
                        fallback_failures.push(format!("{}: {}", mapping.provider, e));
                        continue;
                    }
```

- [ ] **Step 5: Patch Err branches in handler 2 — messages (`handle_messages`)**

The fallback loop is at line ~1198. There are TWO Err arms in this handler:
- Streaming Err at ~line 1336
- Non-streaming Err at ~line 1357

Add the cooldown skip check at the top of the mapping loop body in the same way as Step 4.

For the **streaming Err arm** (currently):
```rust
                        Err(e) => {
                            warn!(
                                "⚠️ Provider {} streaming failed: {}, trying next fallback",
                                mapping.provider, e
                            );
                            fallback_failures.push(format!("{} (stream): {}", mapping.provider, e));
                            continue;
                        }
```

Change to:
```rust
                        Err(e) => {
                            warn!(
                                "⚠️ Provider {} streaming failed: {}, trying next fallback",
                                mapping.provider, e
                            );
                            if let Some(duration) = cooldown_for_4xx(&e) {
                                state.provider_cooldowns.insert(
                                    mapping.provider.clone(),
                                    std::time::Instant::now() + duration,
                                );
                                warn!(
                                    "⏸ Provider {} on cooldown for {}s",
                                    mapping.provider,
                                    duration.as_secs()
                                );
                            }
                            fallback_failures.push(format!("{} (stream): {}", mapping.provider, e));
                            continue;
                        }
```

For the **non-streaming Err arm** (currently):
```rust
                        Err(e) => {
                            warn!(
                                "⚠️ Provider {} failed: {}, trying next fallback",
                                mapping.provider, e
                            );
                            fallback_failures.push(format!("{}: {}", mapping.provider, e));
                            continue;
                        }
```

Change to:
```rust
                        Err(e) => {
                            warn!(
                                "⚠️ Provider {} failed: {}, trying next fallback",
                                mapping.provider, e
                            );
                            if let Some(duration) = cooldown_for_4xx(&e) {
                                state.provider_cooldowns.insert(
                                    mapping.provider.clone(),
                                    std::time::Instant::now() + duration,
                                );
                                warn!(
                                    "⏸ Provider {} on cooldown for {}s",
                                    mapping.provider,
                                    duration.as_secs()
                                );
                            }
                            fallback_failures.push(format!("{}: {}", mapping.provider, e));
                            continue;
                        }
```

- [ ] **Step 6: Patch Err branch in handler 3 — count_tokens (`handle_count_tokens`)**

The fallback loop is at line ~1518. One Err arm at ~line 1568.
Add the cooldown skip check at the top of the mapping loop body.

Find the Err arm (currently):
```rust
                    Err(e) => {
                        warn!(
                            "⚠️ Provider {} failed: {}, trying next fallback",
                            mapping.provider, e
                        );
                        continue;
                    }
```

Change to:
```rust
                    Err(e) => {
                        warn!(
                            "⚠️ Provider {} failed: {}, trying next fallback",
                            mapping.provider, e
                        );
                        if let Some(duration) = cooldown_for_4xx(&e) {
                            state.provider_cooldowns.insert(
                                mapping.provider.clone(),
                                std::time::Instant::now() + duration,
                            );
                            warn!(
                                "⏸ Provider {} on cooldown for {}s",
                                mapping.provider,
                                duration.as_secs()
                            );
                        }
                        continue;
                    }
```

- [ ] **Step 7: Verify all 3 Err branches are patched**

```bash
grep -n 'provider_cooldowns' src/server/mod.rs
```

Expected: at minimum 7 hits — 1 in AppState, 1 in constructor, 3× `is_on_cooldown` (skip checks), 3× `.insert(` (cooldown sets), plus the test hits.

- [ ] **Step 8: Run tests**

```bash
cargo test 2>&1 | tail -10
```

Expected: all tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/server/mod.rs
git commit -m "feat: add provider cooldowns (401/403=60s, 429=30s) across all 3 fallback loops"
```

---

## Task 5: OAuth Refresh Generalization (P2)

Extract `needs_background_refresh`, extract `refresh_provider_if_needed`, extend the background
loop to cover gemini/openai/anthropic OAuth providers alongside copilot.

**Files:**
- Modify: `src/server/mod.rs`

**Key facts:**
- `COPILOT_POLL_SECS = 20 * 60 = 1200` (already defined at line 29)
- `COPILOT_REFRESH_THRESHOLD_SECS = COPILOT_POLL_SECS as i64 + 5 * 60` (already at line 30)
- `crate::auth::OAuthConfig::gemini()`, `::openai_codex()`, `::anthropic()` are all valid constructors
- `crate::auth::OAuthClient::new(config, token_store)` is the constructor
- `oauth_client.refresh_token(&provider_id).await` internally saves the refreshed token — no manual save needed
- `copilot` branch DOES need manual save via `bg_token_store.save()` (unchanged)
- Guard: skip providers where `auth_type != AuthType::OAuth` to avoid spurious warnings

- [ ] **Step 1: Write 3 failing tests**

Add these tests to the `mod tests { ... }` block in `src/server/mod.rs`:

```rust
#[test]
fn test_needs_background_refresh_returns_true_when_near_expiry() {
    let token = crate::auth::OAuthToken {
        provider_id: "test".into(),
        access_token: "tok".into(),
        refresh_token: "ref".into(),
        expires_at: chrono::Utc::now() + chrono::Duration::minutes(10), // < 25 min threshold
        enterprise_url: None,
        project_id: None,
    };
    assert!(needs_background_refresh(&token, COPILOT_POLL_SECS));
}

#[test]
fn test_needs_background_refresh_returns_false_for_fresh_token() {
    let token = crate::auth::OAuthToken {
        provider_id: "test".into(),
        access_token: "tok".into(),
        refresh_token: "ref".into(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(2), // > 25 min threshold
        enterprise_url: None,
        project_id: None,
    };
    assert!(!needs_background_refresh(&token, COPILOT_POLL_SECS));
}

#[test]
fn test_needs_background_refresh_returns_true_for_expired_token() {
    let token = crate::auth::OAuthToken {
        provider_id: "test".into(),
        access_token: "tok".into(),
        refresh_token: "ref".into(),
        expires_at: chrono::Utc::now() - chrono::Duration::minutes(5), // already expired
        enterprise_url: None,
        project_id: None,
    };
    assert!(needs_background_refresh(&token, COPILOT_POLL_SECS));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test test_needs_background_refresh 2>&1 | tail -10
```

Expected: compile error — `needs_background_refresh` not found.

- [ ] **Step 3: Add `needs_background_refresh` and replace `copilot_token_needs_background_refresh`**

In `src/server/mod.rs`, find the existing function:

```rust
fn copilot_token_needs_background_refresh(token: &crate::auth::OAuthToken) -> bool {
    let remaining = token.expires_at.signed_duration_since(chrono::Utc::now());
    remaining < chrono::Duration::seconds(COPILOT_REFRESH_THRESHOLD_SECS)
}
```

Replace it with the generic version:

```rust
fn needs_background_refresh(token: &crate::auth::OAuthToken, poll_secs: u64) -> bool {
    let threshold = chrono::Duration::seconds(poll_secs as i64 + 5 * 60);
    let remaining = token.expires_at.signed_duration_since(chrono::Utc::now());
    remaining < threshold
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test test_needs_background_refresh 2>&1 | tail -10
```

Expected: 3 tests pass.

- [ ] **Step 5: Extract `refresh_provider_if_needed` from the background loop**

Add this async function to `src/server/mod.rs` just before `start_server`:

```rust
async fn refresh_provider_if_needed(
    provider_config: &crate::providers::ProviderConfig,
    token_store: &crate::auth::TokenStore,
    client: &reqwest::Client,
) {
    if provider_config.auth_type != crate::providers::AuthType::OAuth {
        return;
    }
    let provider_id = provider_config
        .oauth_provider
        .clone()
        .unwrap_or_else(|| provider_config.name.clone());
    let token = match token_store.get(&provider_id) {
        Some(t) => t,
        None => return,
    };
    if !needs_background_refresh(&token, COPILOT_POLL_SECS) {
        return;
    }
    match provider_config.provider_type.as_str() {
        "copilot" => {
            match crate::auth::github_copilot::refresh_copilot_token(client, &token.refresh_token)
                .await
            {
                Ok(resp) => {
                    let updated =
                        build_refreshed_copilot_token(&token, resp.token, resp.expires_at);
                    if let Err(e) = token_store.save(updated) {
                        warn!(
                            "Background refresh: failed to save Copilot token for '{}': {}",
                            provider_id, e
                        );
                    } else {
                        info!(
                            "Background refresh: renewed Copilot bearer for '{}'",
                            provider_id
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Background refresh: failed to renew Copilot bearer for '{}': {}",
                        provider_id, e
                    );
                }
            }
        }
        "gemini" | "openai" | "anthropic" => {
            let oauth_config = match provider_config.provider_type.as_str() {
                "gemini" => crate::auth::OAuthConfig::gemini(),
                "openai" => crate::auth::OAuthConfig::openai_codex(),
                _ => crate::auth::OAuthConfig::anthropic(),
            };
            let oauth_client = crate::auth::OAuthClient::new(oauth_config, token_store.clone());
            match oauth_client.refresh_token(&provider_id).await {
                Ok(_) => {
                    info!(
                        "Background refresh: renewed {} OAuth token for '{}'",
                        provider_config.provider_type, provider_id
                    );
                }
                Err(e) => {
                    warn!(
                        "Background refresh: failed to renew {} OAuth token for '{}': {}",
                        provider_config.provider_type, provider_id, e
                    );
                }
            }
        }
        _ => {}
    }
}
```

- [ ] **Step 6: Replace the background task body to use `refresh_provider_if_needed`**

Find the background task spawned in `start_server` (around line 191–232).
Replace the entire `loop { ... }` body:

```rust
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(COPILOT_POLL_SECS));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                for provider_config in &bg_providers {
                    refresh_provider_if_needed(provider_config, &bg_token_store, &bg_client)
                        .await;
                }
            }
        });
```

Note: `bg_client` is already declared above (`let bg_client = reqwest::Client::new();`).
Remove `bg_client` from the closure capture list if the compiler complains — it's captured
automatically via `move`.

- [ ] **Step 7: Run `cargo clippy` and `cargo test`**

```bash
cargo clippy 2>&1 | grep -E 'error|warning' | grep -v 'unused import' | head -20
cargo test 2>&1 | tail -10
```

Expected: clippy clean, all tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/server/mod.rs
git commit -m "feat: generalize background OAuth refresh to cover gemini/openai/anthropic providers"
```

---

## Task 6: P2 Spec Documents (5 files)

**Files:**
- Create: `docs/contracts/slo-contract.md`
- Create: `docs/contracts/escalation-sla.md`
- Create: `docs/contracts/benchmark-protocol.md`
- Create: `docs/contracts/auth-validation-spec.md`
- Create: `docs/contracts/streaming-fallback-boundary.md`

- [ ] **Step 1: Write `docs/contracts/slo-contract.md`**

```markdown
# Canonical Observability SLO Contract

## Service Level Objectives

### Latency SLO

**p99 end-to-end request latency < 5000ms** measured at the relay (from request receipt
to first byte of response), excluding provider streaming duration.

Measurement point: relay handler entry → first response byte out.
Excludes: streaming body delivery time after first byte.

### Error Rate SLO

**Error rate < 1%** over any rolling 5-minute window.

"Error" is defined as a response where all fallback providers failed and the relay
returned 5xx to the caller. Provider-level 4xx errors that triggered fallback do not
count as relay errors if another provider succeeded.

## Alert Thresholds

| Signal | Threshold | Severity |
|--------|-----------|----------|
| p99 latency | > 3000ms for 5 min | warning |
| p99 latency | > 5000ms for 2 min | critical |
| Error rate | > 0.5% for 5 min | warning |
| Error rate | > 1% for 2 min | critical |

## Alert Owner

**Primary:** On-call engineer (rotated weekly).
**Escalation:** Project maintainer if unacknowledged within 15 minutes.
```

- [ ] **Step 2: Write `docs/contracts/escalation-sla.md`**

```markdown
# Checkpoint Escalation SLA

## What Is a Checkpoint

A checkpoint is a dependency gate that must pass before a release can proceed.

Examples of checkpoints:
- OAuth providers for all configured provider types refresh correctly after server start
- All fallback providers in the active chain respond within SLO
- Admin UI loads without JS errors

## Escalation Chain

| Level | Owner | Response SLA | Trigger |
|-------|-------|-------------|---------|
| L1 | On-call engineer | 15 minutes | Checkpoint fails in staging or prod |
| L2 | Project maintainer | 30 minutes | L1 unresponsive or cannot resolve |
| L3 | Repo owner | 60 minutes | L2 cannot resolve; release decision needed |

## Go/No-Go Decision Authority

**L3 (Repo owner)** has final go/no-go authority for releases when a checkpoint
has not cleared.

L1 and L2 may unblock minor releases (hotfixes) without L3 approval if the
failing checkpoint is explicitly scoped as non-blocking for the release.

## Checkpoint Verification Steps

Before each release:
1. Start a clean server with the release binary and the production TOML config.
2. Confirm all configured OAuth providers exchange tokens successfully.
3. Send one test request per configured model mapping; verify 200 response.
4. Check server logs for any ERROR lines within the first 60 seconds.
5. If all pass → checkpoint cleared. If any fail → L1 escalation triggered.
```

- [ ] **Step 3: Write `docs/contracts/benchmark-protocol.md`**

```markdown
# Benchmark Measurement Protocol for p95/p99 Gates

## Purpose

This protocol defines how latency benchmarks are run to produce valid p95/p99 measurements.
Benchmarks not following this protocol must not be used to gate releases.

## Warmup

- **Warmup iterations:** 100 requests before recording any samples.
- Warmup requests are discarded entirely; they exist to fill JIT caches and connection pools.

## Sample Size

- **Minimum sample size:** 1000 requests after warmup.
- Samples must be collected within a single continuous run (no pausing between samples).

## Environment Requirements

- Run on a machine with no other significant CPU load (< 10% baseline CPU).
- Network: localhost loopback only (no external provider calls during benchmark — use a
  stub/mock provider that returns a fixed response in < 1ms).
- Do not run benchmarks in shared CI environments. Use a dedicated benchmark machine or
  a dedicated CI job that pins CPU affinity.

## Confidence Interval

Report p95 and p99 with a 95% confidence interval computed via bootstrap resampling
(1000 bootstrap samples). If the confidence interval width > 20% of the point estimate,
the sample size is insufficient — double the sample count and re-run.

## Bimodal Distribution Handling

After collecting samples, check for bimodal distribution:
- Plot a histogram with 50 bins.
- If two distinct peaks are visible (separated by a trough where frequency < 25% of the
  lower peak), the distribution is bimodal.
- **If bimodal: report both modes separately** (mean and p99 of each cluster) and
  flag the benchmark result as **INCONCLUSIVE**.
- An INCONCLUSIVE benchmark must NOT be used to gate a release without human review.

## Reporting Format

```
Benchmark: <name>
Date: YYYY-MM-DD
Samples: N (after M warmup)
p50: Xms [CI: ±Yms]
p95: Xms [CI: ±Yms]
p99: Xms [CI: ±Yms]
Bimodal: YES/NO
Result: PASS / FAIL / INCONCLUSIVE
Gate threshold: p99 < Zms
```
```

- [ ] **Step 4: Write `docs/contracts/auth-validation-spec.md`**

```markdown
# Incoming Auth Validation Spec

## Accepted Auth Schemes

The relay accepts the following authentication schemes for incoming requests:

| Scheme | Header | Example |
|--------|--------|---------|
| Bearer token | `Authorization: Bearer <token>` | `Authorization: Bearer sk-ant-...` |
| API key header | `X-Api-Key: <key>` | `X-Api-Key: my-server-key` |

Both schemes are read and validated. Only one needs to be present.

## Precedence When Both Headers Are Present

When a request includes both `Authorization: Bearer <token>` AND `X-Api-Key: <key>`:
- **`Authorization: Bearer`** takes precedence for passthrough auth (relayed to upstream).
- **`X-Api-Key`** is used for relay gate validation (server.api_key check).
- The two headers serve different purposes and are not in conflict.

## Relay Gate Order

The `server.api_key` relay gate is checked **before** the passthrough auth header is
read. If the relay gate rejects the request (wrong or missing API key), the request
never reaches the provider routing or passthrough logic.

Order of operations for an incoming request:
1. Auth middleware checks `X-Api-Key` against `server.api_key` (if configured).
2. If gate passes (or no `server.api_key` configured), handler runs.
3. Handler reads `Authorization: Bearer` for passthrough auth (if applicable).

## Error Response Format

All auth validation failures return:

```
HTTP 401 Unauthorized
Content-Type: text/plain
Body: "Unauthorized"
```

No error detail is included in the response body to avoid leaking configuration state.

## Missing or Malformed Auth

- Missing `X-Api-Key` when `server.api_key` is configured → 401
- Wrong `X-Api-Key` value → 401
- `Authorization` header present but not `Bearer <token>` format → passthrough auth
  is treated as absent (not an error at the relay level; upstream provider will reject)
```

- [ ] **Step 5: Write `docs/contracts/streaming-fallback-boundary.md`**

```markdown
# Streaming Fallback Boundary

## The Invariant

**Once the first SSE byte is emitted to the client, mid-stream fallback is
architecturally impossible.**

The relay commits to a provider when it begins streaming the response. The client
HTTP connection is already open and bytes have been sent. There is no mechanism to
switch providers partway through a stream without terminating and re-opening the
client connection.

## Before First Byte: Fallback Is Possible

If a streaming provider call fails **before any SSE byte has been sent to the client**,
the relay falls back to the next provider in priority order — identical to non-streaming
fallback behavior.

Implementation: `src/server/mod.rs` around line 1270. The `send_message_stream()` call
returns a `Result` before any bytes flow. If it returns `Err`, the fallback loop continues.

## After First Byte: No Recovery

If a provider fails **mid-stream** (after at least one SSE byte has been sent),
the relay has no recovery path. The stream terminates and the client receives a
truncated or malformed response. The client must retry the full request.

This is intentional and not a bug. Adding mid-stream recovery would require buffering
the entire stream (defeating the purpose of streaming) or proxy-level connection
re-establishment (complex, error-prone, not in scope).

## Error Codes That Trigger Fallback vs Passthrough

| Scenario | Behavior |
|----------|----------|
| `send_message_stream()` returns `Err` before first byte | Fallback to next provider |
| Stream starts then provider drops connection | Stream terminates; client retries |
| Provider returns non-2xx before first SSE byte | Fallback if `Err`, passthrough if `Ok` with error body |

## Reference

See `src/server/mod.rs` ~line 1303 (`send_message_stream` call) for the exact
boundary in code.
```

- [ ] **Step 6: Verify acceptance criteria for all 5 docs**

For each file, check the acceptance criterion from the design doc:
- `slo-contract.md`: one p99 statement ✓, one error rate statement ✓, one named alert owner ✓
- `escalation-sla.md`: escalation chain named with response times per level ✓
- `benchmark-protocol.md`: warmup count ✓, minimum sample size ✓, bimodal handling rule ✓
- `auth-validation-spec.md`: scheme list ✓, precedence rule ✓, relay gate order ✓, error shape ✓
- `streaming-fallback-boundary.md`: boundary documented ✓, "no mid-stream recovery" invariant explicit ✓

- [ ] **Step 7: Commit**

```bash
git add docs/contracts/
git commit -m "docs: add 5 P2 spec documents (SLO, escalation, benchmark, auth-validation, streaming boundary)"
```

---

## Task 7: Fix admin.html JS String Injection (P3)

**Files:**
- Modify: `src/server/admin.html` (lines 4359, 4365)

**Context:** `escapeHtml()` only escapes HTML entities (e.g., `<`, `&`). HTML entities
are decoded by the browser before JavaScript execution, so a provider ID like `o'reilly`
would produce `onclick="deleteOAuthToken('o'reilly')"` — breaking the JS attribute.
Fix: add `escapeJs()` and use it in these two onclick strings.

The two onclick attributes are at:
- Line 4359: `onclick="refreshOAuthToken('${escapeHtml(token.provider_id)}')"` 
- Line 4365: `onclick="deleteOAuthToken('${escapeHtml(token.provider_id)}')"` 

- [ ] **Step 1: Add `escapeJs()` function after `escapeHtml()` in `admin.html`**

Find `escapeHtml` at ~line 2200:
```javascript
            function escapeHtml(text) {
                const div = document.createElement("div");
                div.textContent = text;
                return div.innerHTML;
            }
```

Add `escapeJs` immediately after it:
```javascript
            function escapeJs(text) {
                return String(text)
                    .replace(/\\/g, '\\\\')
                    .replace(/'/g, "\\'")
                    .replace(/"/g, '\\"');
            }
```

- [ ] **Step 2: Replace `escapeHtml` with `escapeJs` in the two onclick attributes**

Change line 4359 from:
```
onclick="refreshOAuthToken('${escapeHtml(token.provider_id)}')"
```
to:
```
onclick="refreshOAuthToken('${escapeJs(token.provider_id)}')"
```

Change line 4365 from:
```
onclick="deleteOAuthToken('${escapeHtml(token.provider_id)}')"
```
to:
```
onclick="deleteOAuthToken('${escapeJs(token.provider_id)}')"
```

- [ ] **Step 3: Verify in browser DevTools**

Start the server:
```bash
cargo run -- --config config/example.toml
```

Open `http://localhost:3000` → navigate to the OAuth Tokens tab.

In the browser DevTools console, run:
```javascript
// Simulate a provider ID with a single quote — the fix should handle it cleanly
escapeJs("o'reilly")
// Expected: "o\\'reilly"
escapeJs('normal-provider')
// Expected: "normal-provider"
```

Click the Refresh and Delete buttons for any provider. Expected: no JS error in console,
the button invokes the correct function.

- [ ] **Step 4: Commit**

```bash
git add src/server/admin.html
git commit -m "fix: replace escapeHtml with escapeJs in OAuth token onclick handlers"
```

---

## Task 8: Fix admin.html loadConfig() Missing response.ok Check (P3)

**Files:**
- Modify: `src/server/admin.html` (~line 2230)

**Context:** `loadConfig()` calls `await response.json()` without first checking
`response.ok`. When the user cancels the `UIkit.modal.prompt` for the API key,
`apiFetch()` returns the original 401 response. If the 401 body is not valid JSON
(or is empty), `response.json()` throws, causing an unhandled rejection that shows
as a cryptic error rather than a clear "API key required" message.

- [ ] **Step 1: Update `loadConfig()` to check `response.ok` before parsing JSON**

Find the current `loadConfig()` at ~line 2230:
```javascript
            async function loadConfig() {
                try {
                    const response = await apiFetch("/api/config/json");
                    const config = await response.json();
                    appState.config = config;
                    appState.loaded = true;
                    saveToLocalStorage(config);
                    return config;
                } catch (error) {
                    console.error("Failed to load config:", error);
                    notifyError("Failed to load configuration");
                    return null;
                }
            }
```

Replace with:
```javascript
            async function loadConfig() {
                try {
                    const response = await apiFetch("/api/config/json");
                    if (!response.ok) {
                        console.warn("loadConfig: server returned", response.status, "— API key may be required or was cancelled");
                        notifyWarning("API key required to load configuration");
                        return null;
                    }
                    const config = await response.json();
                    appState.config = config;
                    appState.loaded = true;
                    saveToLocalStorage(config);
                    return config;
                } catch (error) {
                    console.error("Failed to load config:", error);
                    notifyError("Failed to load configuration");
                    return null;
                }
            }
```

- [ ] **Step 2: Verify in browser DevTools**

Start the server with an API key configured:
```toml
[server]
api_key = "test-key"
```

```bash
cargo run -- --config config/example.toml
```

Open `http://localhost:3000`. When the API key prompt appears, click Cancel.

Expected in DevTools console:
```
loadConfig: server returned 401 — API key may be required or was cancelled
```

No unhandled rejection. A `notifyWarning` toast should appear.

- [ ] **Step 3: Commit**

```bash
git add src/server/admin.html
git commit -m "fix: add response.ok check in loadConfig() before calling response.json()"
```

---

## Task 9: Update TODOS.md

**Files:**
- Modify: `TODOS.md`

- [ ] **Step 1: Mark all 11 items as resolved in TODOS.md**

For each item, add a `Resolved:` line below the priority line. Exact additions:

**[P1] Rollback Control Contract** → add:
```
Resolved: 2026-05-19. See docs/contracts/rollback-contract.md.
```

**[P1] Deterministic Fallback Candidate Selection Policy** → add:
```
Resolved: 2026-05-19. See docs/contracts/fallback-selection-policy.md.
```

**[P2] Canonical Observability SLO Contract** → add:
```
Resolved: 2026-05-19. See docs/contracts/slo-contract.md.
```

**[P2] Checkpoint Escalation SLA** → add:
```
Resolved: 2026-05-19. See docs/contracts/escalation-sla.md.
```

**[P2] Benchmark Measurement Protocol for p95/p99 Gates** → add:
```
Resolved: 2026-05-19. See docs/contracts/benchmark-protocol.md.
```

**[P2] Incoming Auth Validation Spec** → add:
```
Resolved: 2026-05-19. See docs/contracts/auth-validation-spec.md.
```

**[P2] Background OAuth Refresh Generalization** → add:
```
Resolved: 2026-05-19. Implemented in src/server/mod.rs via needs_background_refresh() and refresh_provider_if_needed(). Covers gemini, openai, anthropic, copilot.
```

**[P2] Temporary Provider Deactivation on 4xx Errors** → add:
```
Resolved: 2026-05-19. DashMap<String, Instant> in AppState. 401/403=60s, 429=30s. Patched all 3 fallback loops in src/server/mod.rs.
```

**[P3] Streaming Fallback Boundary Documentation** → add:
```
Resolved: 2026-05-19. See docs/contracts/streaming-fallback-boundary.md.
```

**[P3] admin.html onclick Attribute JS-String Injection** → add:
```
Resolved: 2026-05-19. Added escapeJs() and replaced escapeHtml() in onclick handlers at lines 4359 and 4365 in src/server/admin.html.
```

**[P3] admin.html loadConfig() Missing response.ok Check After 401 Cancel** → add:
```
Resolved: 2026-05-19. Added response.ok guard in loadConfig() in src/server/admin.html.
```

Also add at the top of TODOS.md under any existing header:
```
## Follow-up TODOs (added this session)

- Health-aware tie-breaking for fallback candidate selection (D14 from plan-eng-review)
- Standardize provider_type constants (D15 from plan-eng-review)
- Circuit breaker / exponential backoff for repeated OAuth refresh failures (open question from OAuth refresh design doc)
```

- [ ] **Step 2: Verify all 11 items have Resolved lines**

```bash
grep -c 'Resolved:' TODOS.md
```

Expected: 11

- [ ] **Step 3: Commit**

```bash
git add TODOS.md
git commit -m "docs: mark all 11 TODOS resolved, add 3 follow-up items"
```

---

## Self-Review

### Spec Coverage Check

| Design Doc Requirement | Task |
|------------------------|------|
| Rollback Contract (P1) | Task 1 |
| Fallback Selection Policy (P1) | Task 2 |
| Provider Deactivation — DashMap + 6 tests (D7) | Tasks 3+4 |
| Patch all 3 Err branches (D12) | Task 4 |
| OAuth Refresh Generalization + 3 tests (D8) | Task 5 |
| Canonical Observability SLO | Task 6 |
| Checkpoint Escalation SLA | Task 6 |
| Benchmark Protocol | Task 6 |
| Incoming Auth Validation Spec | Task 6 |
| Streaming Fallback Boundary | Task 6 |
| escapeJs fix in admin.html (D5) | Task 7 |
| loadConfig response.ok fix (T8) | Task 8 |
| TODOS.md all 11 resolved | Task 9 |
| Follow-up TODOs: health tie-break, provider_type constants (D14, D15) | Task 9 |

All 13 requirements covered.

### Placeholder Scan

No TBD/TODO/placeholder in code steps. All code blocks contain complete, compilable implementations.

### Type Consistency

- `cooldown_for_4xx` takes `&crate::providers::error::ProviderError` — consistent with Err branch type.
- `is_on_cooldown` takes `&dashmap::DashMap<String, std::time::Instant>` — matches `provider_cooldowns` field type.
- `needs_background_refresh` takes `&crate::auth::OAuthToken` — consistent with existing test helpers that construct `OAuthToken` directly.
- `refresh_provider_if_needed` uses `crate::providers::ProviderConfig` and `crate::auth::TokenStore` — consistent with existing background loop variables.
- `OAuthConfig::gemini()`, `::openai_codex()`, `::anthropic()` verified to exist in `src/auth/oauth.rs` (lines 110, 87, 58).
