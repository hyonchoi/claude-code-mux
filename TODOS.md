# TODOS

## Follow-up TODOs (added 2026-05-19)

- Health-aware tie-breaking for fallback candidate selection (D14 from plan-eng-review)
- Standardize provider_type constants (D15 from plan-eng-review)
- Circuit breaker / exponential backoff for repeated OAuth refresh failures (open question from OAuth refresh design doc)
- X-Interaction-Id per-conversation UUID for Copilot provider (D8 from plan-eng-review)
- Persist VScode-SessionId/MachineId across proxy restarts (D9 from plan-eng-review outside-voice)


## [P1] Qwen/vLLM Reasoning-Only Responses Cause Conversation Stop
What:
When Qwen3.6-27B (via vLLM `/v1/chat/completions`) generates output entirely inside
thinking tags, vLLM's reasoning parser routes all tokens to the `reasoning` field,
leaving `content: null` and `finish_reason: "stop"`. Claude Code receives a turn that
ends with `end_turn` and displays "Cogitated for Ns" — the conversation halts until
the user manually types `continue`.

Why:
Users see intermittent, unexplained pauses mid-conversation. The model has work to do
(e.g. a pending tool call or follow-up message) but the turn ended prematurely, so
Claude Code waits for input. The problem resolves after manual `continue` because the
subsequent request elicits a response with non-empty `content`.

Evidence:
- 9 confirmed occurrences in `ccm.log` (all on streaming path for `qwen` provider,
  `finish_reason: "stop"`, `content: null`, no `tool_calls`, substantial `reasoning`).
- Non-streaming path (`openai.rs:972-973`) has reasoning→text fallback, but the
  streaming path (`openai.rs:1431-1437`) is a raw SSE byte passthrough with no
  transformation.

Root cause:
`send_message_stream` in `src/providers/openai.rs` passes through raw OpenAI SSE bytes
without transforming them to Anthropic SSE format. When the upstream returns only
`reasoning_content` deltas (no `content`), Claude Code interprets the stop as a natural
turn end with no visible output.

Fix options:
1. **vLLM-side:** Disable reasoning parser for Qwen3.6-27B or tune thinking-tag detection
   so not all output is captured as reasoning. Prevents empty `content` at source.
2. **ccm streaming transform:** Implement the `TODO` at `openai.rs:1431` — transform
   OpenAI SSE to Anthropic SSE so `reasoning_content` → thinking blocks and empty-content
   stops are handled.
3. **ccm non-streaming reasoning→thinking:** Convert reasoning fallback at
   `openai.rs:972-973` to emit a `Thinking` content block instead of `Text`, so Claude
   Code renders it correctly.

Impact:
Affects all conversations routed to Qwen/vLLM endpoints. Intermittent — occurs when the
model's generation happens to produce output only inside thinking tags.

Effort estimate:
- Human team: S (option 1, vLLM config change)
- CC+gstack: M (option 2, SSE transform implementation)
Priority:
- P1

## [P2] vLLM / SGLang Native Provider With Anthropic-Compatible API
**Completed:** v0.8.8-chy (2026-07-09)

What:
Add dedicated vLLM and SGLang provider types that speak Anthropic-compatible API format
instead of routing through the OpenAI-compatible `/v1/chat/completions` endpoint.
Both engines now support Anthropic-format endpoints (`/v1/messages`) that natively
emit Anthropic SSE events (thinking blocks, tool_use, text content) without an
intermediate OpenAI translation layer.

Why:
The current OpenAI-compatible path has a raw SSE passthrough (`openai.rs:1431-1437`)
that causes reasoning-only stops (see P1 entry above) and cannot properly translate
reasoning content, tool calls, or usage metadata. A native Anthropic-compatible path
eliminates the translation gap entirely.

Pros:
- No OpenAI↔Anthropic format conversion — zero translation bugs.
- Thinking blocks, tool_use, text content arrive as proper Anthropic SSE events.
- Reasoning parser no longer swallows all output into `reasoning_content`.
- Proper streaming support (no raw byte passthrough).
- Model cards, token usage, and finish_reasons arrive in native format.

Cons:
- Requires confirming Anthropic API compatibility for the specific vLLM/SGLang versions
  in use (vLLM 0.8+, SGLang 0.4+ have `--api-prefix /v1` with Anthropic support).
- Additional provider type in the config schema.

Context:
Related to P1 Qwen/vLLM reasoning-only responses — a native Anthropic path sidesteps
that class of issue entirely.

Effort estimate:
- Human team: S (verify vLLM/SGLang Anthropic endpoint support + config)
- CC+gstack: S (new provider type + routing, minimal — format is already Anthropic)
Priority:
- P2

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

## [P2] Temporary Provider Deactivation on Triggering Errors
Resolved: 2026-05-19. DashMap<String, Instant> in AppState. 401/403=240s, 429=120s, 502=60s. Patched all 3 fallback loops in src/server/mod.rs.
What:
When a provider returns a triggering error (auth failure, rate limit, or bad gateway), mark it as temporarily deactivated for a cooldown period and skip it in subsequent requests until the cooldown expires.
Why:
Avoids hammering a provider that is rate-limiting (429), has invalid credentials (401/403), or is returning empty/bad responses (502), reducing wasted latency on doomed fallback attempts.
Pros:
- Faster fallback path: skips known-bad providers immediately.
- Reduces noise in provider error logs during outage periods.
Cons:
- Requires shared mutable state in AppState (e.g. `DashMap<String, Instant>`).
- Cooldown duration policy needs tuning per error code (401/403=240s, 429=120s, 502=60s).
Context:
Raised in conversation 2026-05-19. Implementation point: `Err(e)` branch in the provider fallback loop at `src/server/mod.rs`. 502 cooldown added in v0.8.6-chy to handle synthetic 502s from providers returning empty choices arrays.
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

## [P1] Strip `role: "system"` Messages When Redirecting Across Models/Providers
**Completed:** v0.8.7-chy (2026-07-02)

What:
When the mux redirects a request to a different model than the client targeted — a
non-Anthropic provider, or a different Anthropic model (e.g. opus-4-8 -> sonnet-4-6,
sonnet-5 -> sonnet-4-6) — any standalone `role: "system"` message in the `messages`
array must be normalized away. Newer Claude Code payloads (opus-4.8, sonnet-5) emit
SessionStart hook context as a mid-conversation `{role:"system", content:<str>}`
message; sonnet-4.6 and non-Anthropic providers do not accept that role in `messages`
(the Messages API requires alternating user/assistant turns). Transform: wrap the
content in `<system-reminder>...</system-reminder>`, convert to a `role:"user"` text
block, and merge it into the nearest preceding user message (to preserve alternation);
then drop the `system` message. Do NOT hoist it to the top-level `system` field — it is
turn-positional, and `system[]` blocks carry `cache_control:{ttl:1h}` that appended
content would disturb.

Implementation:
- `normalize_mid_conversation_system()` in `src/server/mod.rs` — runs per-mapping before dispatch in `handle_messages`, `handle_openai_chat_completions`, and `handle_count_tokens`
- `strip_mid_conversation_system` flag on `[[models.mappings]]` — opt-in per mapping
- Role alternation preserved: orphans buffered and prepended to next user turn; synthesized user turn inserted when next turn is assistant/boundary
- Defense-in-depth warning in `src/providers/openai.rs` — drops residual role:system with tracing::warn
- `#[derive(PartialEq)]` added to message model types in `src/models/mod.rs` to support test assertions
- Admin UI checkbox added to all mapping render views in `src/server/admin.html`
- Unit tests: 7+ test cases covering merge, orphan prepend, multi-system, blocks content, empty system, stray closing tag, already-wrapped passthrough, count_tokens, role alternation, and OpenAI residual skip
Why:
Redirecting to a model/provider that rejects `role:"system"` yields a 400 and silently
breaks the fallback/routing feature for these newer payload shapes.
Pros:
- Cross-model/provider redirects accept the payload; reproduces the exact shape Claude
  Code natively sends sonnet-4.6.
- Preserves the injected hook context instead of dropping it.
Cons:
- Requires a message-normalization pass in the request-rewrite path (router/providers),
  plus a rule for when to apply it (routed model != client-requested model, or target is
  a non-Anthropic provider).
- Edge cases: no preceding user message (prepend to the following user turn or synthesize
  one); multiple `system` messages; content already wrapped.
Context:
Identified 2026-07-01 from snapshot analysis in snapshots/REQUEST-STRUCTURE-DIFF.md.
`sonnet-5.json` messages[1] = {role:"system", len 13984} (SessionStart hook); `sonnet-4-6.json`
carries the same class of content as user-role `<system-reminder>` blocks merged into
messages[0]. Implementation point: request-rewrite / model-mapping in `src/router/` or
`src/providers/`. Apply when the routed model differs from the client-requested model
(opus-4-8 -> sonnet-4-6, sonnet-5 -> sonnet-4-6, etc.) or the target is non-Anthropic.
Effort estimate:
- Human team: S
- CC+gstack: S
Priority:
- P1
Depends on / blocked by:
- None.
