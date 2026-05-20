# TODOS

## Follow-up TODOs (added 2026-05-19)

- Health-aware tie-breaking for fallback candidate selection (D14 from plan-eng-review)
- Standardize provider_type constants (D15 from plan-eng-review)
- Circuit breaker / exponential backoff for repeated OAuth refresh failures (open question from OAuth refresh design doc)


## [P1] Rollback Control Contract For Passthrough Relay
Resolved: 2026-05-19. See docs/contracts/rollback-contract.md.
What:
Define one canonical rollback kill-switch contract for passthrough/fallback behavior, including toggle key, scope, propagation semantics, and verification steps.
Why:
Incident response depends on deterministic rollback controls under pressure.
Pros:
- Faster, less ambiguous rollback execution.
- Better post-incident reproducibility and auditability.
Cons:
- Requires governance and ongoing documentation discipline.
Context:
Raised by outside-voice and adversarial review as an operability gap in release safety.
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P1
Depends on / blocked by:
- Existing rollout and rollback sections in CEO plan.

## [P2] Canonical Observability SLO Contract
Resolved: 2026-05-19. See docs/contracts/slo-contract.md.
What:
Consolidate observability wording into one canonical measurable SLO statement with alert thresholds and owner.
Why:
Duplicate/ambiguous SLO phrasing weakens release gate decisions.
Pros:
- Clear rollout pass/fail criteria.
- Better alignment between engineering and on-call.
Cons:
- Small process/documentation overhead.
Context:
Spec-review identified overlapping SLO intent despite improved telemetry schema.
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P2
Depends on / blocked by:
- Telemetry schema and runbook sections already present.

## [P2] Checkpoint Escalation SLA
Resolved: 2026-05-19. See docs/contracts/escalation-sla.md.
What:
Define explicit escalation owner chain and response SLAs for Checkpoint A/B blockers.
Why:
Avoid blocker drift and late unsafe ship pressure.
Pros:
- Faster risk decisions when checkpoints fail.
- Clear accountability for go/no-go calls.
Cons:
- Adds lightweight process overhead.
Context:
Outside-voice flagged missing escalation timing in dependency checkpoint governance.
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P2
Depends on / blocked by:
- Sequencing and ownership section in CEO plan.

## [P1] Deterministic Fallback Candidate Selection Policy
Resolved: 2026-05-19. See docs/contracts/fallback-selection-policy.md.
What:
Define deterministic fallback candidate ordering and tie-break policy (for example: static priority, then health, then stable lexical tie-break).
Why:
Nondeterministic candidate choice can produce inconsistent behavior and complicate incident forensics.
Pros:
- Reproducible fallback outcomes across equivalent conditions.
- Faster root-cause analysis during retries and failovers.
Cons:
- Adds policy complexity and governance overhead.
Context:
Added from /plan-eng-review outside-voice tension resolution (D11).
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P1
Depends on / blocked by:
- Phase 1 trust-boundary and retry-class contracts.

## [P2] Benchmark Measurement Protocol For p95/p99 Gates
Resolved: 2026-05-19. See docs/contracts/benchmark-protocol.md.
What:
Define a canonical performance measurement protocol for p95/p99/error-rate regression gates (traffic profile, duration, environment, baseline window).
Why:
Regression percentages are ambiguous without a fixed method, risking false pass/fail rollout decisions.
Pros:
- Comparable performance decisions across releases.
- Lower chance of noisy canary regressions being misinterpreted.
Cons:
- Requires harness/process definition and maintenance.
Context:
Added from /plan-eng-review outside-voice tension resolution (D12).
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P2
Depends on / blocked by:
- Existing performance budget and canary sections in CEO plan.

## [P2] Incoming Auth Validation Spec
Resolved: 2026-05-19. See docs/contracts/auth-validation-spec.md.
What:
Define the relay's incoming auth validation contract: accepted schemes (Bearer? API key?), precedence when both Authorization and X-API-Key are present, and whether server.api_key relay gate applies before passthrough check.
Why:
The passthrough spec says "missing auth → 401" and "malformed auth → 401" but no validation logic exists today. Behavior is implicit and tests are aspirational without a contract.
Pros:
- Deterministic auth rejection behavior.
- Clearer test cases and operator understanding of relay access control.
Cons:
- Small implementation + documentation overhead.
Context:
Raised by Codex outside-voice review (D13). Current handlers only read X-Provider header, not Authorization. Passthrough feature adds auth reading but leaves edge cases unspecified.
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P2
Depends on / blocked by:
- Passthrough auth implementation (this PR).

## [P3] Streaming Fallback Boundary Documentation
Resolved: 2026-05-19. See docs/contracts/streaming-fallback-boundary.md.
What:
Add explicit code comment and spec note: fallback is only possible before the first SSE byte is emitted. After the stream opens, the client connection is committed and mid-stream fallback is impossible.
Why:
Prevents future engineers from attempting mid-stream fallback, which would corrupt the SSE stream.
Pros:
- Saves a future debugging session.
- Correct invariant documented at the right place.
Cons:
- Minimal.
Context:
Raised by Codex outside-voice review (D14). Current streaming path re-wraps SSE bytes via Event::data() in server/mod.rs:707.
Effort estimate:
- Human team: XS
- CC+gstack: XS
Priority:
- P3
Depends on / blocked by:
- Streaming auth threading (this PR).

## [P2] Background OAuth Refresh Generalization
Resolved: 2026-05-19. Implemented in src/server/mod.rs via needs_background_refresh() and refresh_provider_if_needed(). Covers gemini, openai, anthropic, copilot.
What:
Generalize the background Copilot token refresh task to cover all OAuth-based providers
(Gemini, OpenAI-compatible, Anthropic-compatible) that share the same idle-expiry problem.
Why:
The 25ceb60 fix prevents idle Copilot tokens from expiring silently. The same token-expiry
problem affects any OAuth provider that isn't in the active fallback chain and receives no
requests. The fix only handles Copilot's GitHub-token → bearer exchange path.
Pros:
- Eliminates idle-expiry for Gemini/OpenAI/Anthropic OAuth providers.
- Centralizes refresh logic in one background task instead of per-provider on-demand only.
Cons:
- Requires routing the refresh call per provider type (Copilot: refresh_copilot_token();
  others: OAuthClient::refresh_token()). Non-trivial to get right without regression.
Context:
Raised by /plan-eng-review (D2, 2026-05-18). Current fix in 25ceb60 is Copilot-specific.
Standard OAuth tokens (Google, OpenAI, etc.) can be refreshed via OAuthClient::refresh_token().
Threshold approach: use remaining_time < POLL_INTERVAL + buffer (not hardcoded 5-min
needs_refresh()), which adapts correctly to any token TTL.
Effort estimate:
- Human team: M
- CC+gstack: S
Priority:
- P2
Depends on / blocked by:
- Background Copilot refresh fix (25ceb60) as the pattern to generalize.

## [P3] admin.html onclick Attribute JS-String Injection
Resolved: 2026-05-19. Added escapeJs() and replaced escapeHtml() in onclick handlers at lines 4359 and 4365 in src/server/admin.html.
What:
Lines ~4353/4359 in admin.html build inline onclick attributes with un-escaped provider IDs:
`onclick="deleteOAuthToken('${providerId}')"`. escapeHtml() only escapes HTML entities,
not JS string characters. A provider ID containing a single quote breaks the attribute.
Why:
A provider named `o'reilly` would generate invalid JS in the attribute string, causing
a silent parse error and the delete button becoming non-functional.
Pros:
- Fixes a latent attribute injection bug.
- Consistent with defensive coding already practiced elsewhere in admin.html.
Cons:
- Requires either a dedicated JS-string escaper or a data-* attribute + event delegation refactor.
Context:
Found by Codex outside-voice during /plan-eng-review of the browser dialog refactor (2026-05-18).
Pre-existing bug, not introduced by the dialog refactor PR.
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P3
Depends on / blocked by:
- None.

## [P2] Temporary Provider Deactivation on 4xx Errors
Resolved: 2026-05-19. DashMap<String, Instant> in AppState. 401/403=60s, 429=30s. Patched all 3 fallback loops in src/server/mod.rs.
What:
When a provider returns a 4xx error, mark it as temporarily deactivated for a cooldown period and skip it in subsequent requests until the cooldown expires.
Why:
Avoids hammering a provider that is rate-limiting (429) or has invalid credentials (401/403), reducing wasted latency on doomed fallback attempts.
Pros:
- Faster fallback path: skips known-bad providers immediately.
- Reduces noise in provider error logs during outage periods.
Cons:
- Requires shared mutable state in AppState (e.g. `DashMap<String, Instant>`).
- Cooldown duration policy needs tuning per error code (e.g. 60s for 429, longer for 401/403).
Context:
Raised in conversation 2026-05-19. Implementation point: `Err(e)` branch in the provider fallback loop at `src/server/mod.rs` ~line 1049. Differentiate 4xx vs 5xx — 5xx may be transient and should not trigger deactivation.
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P2
Depends on / blocked by:
- None.

## [P3] admin.html loadConfig() Missing response.ok Check After 401 Cancel
Resolved: 2026-05-19. Added response.ok guard in loadConfig() in src/server/admin.html.
What:
In apiFetch(), when the user cancels the API key prompt, the original 401 response is
returned. loadConfig() at ~line 2230 calls `await response.json()` without checking
`response.ok` first. If the server's 401 body is not valid JSON, this throws a parse error.
Why:
Silent or cryptic failure when the user cancels the API key prompt during page load.
Pros:
- Explicit error handling: clear "cancelled" state vs. "parse failed" state.
- Better UX: show "API key required" message rather than unhandled rejection.
Cons:
- Small change but touches the startup auth flow.
Context:
Found by Codex outside-voice during /plan-eng-review of the browser dialog refactor (2026-05-18).
Pre-existing bug, not introduced by the dialog refactor PR.
Effort estimate:
- Human team: XS
- CC+gstack: XS
Priority:
- P3
Depends on / blocked by:
- None (can be fixed independently after the dialog refactor lands).
