<!-- /autoplan restore point: /Users/hyonchoi/.gstack/projects/9j-claude-code-mux/main-autoplan-restore-20260709-163603.md -->
# Plan: vLLM/SGLang auth header fix

## Rough plan (as given)

vllm/sglang requires 'Authorization: Bearer <api-key>' http header instead of x-api-key.

## Problem

`AnthropicCompatibleProvider` (src/providers/anthropic_compatible.rs) picks the
outbound auth header based on `auth_type`:

- `AuthType::Passthrough` or OAuth → `Authorization: Bearer <token>`
- `AuthType::ApiKey` (the default, and what vllm/sglang configs use today) → `x-api-key: <key>`

vLLM and SGLang's Anthropic-compatible `/v1/messages` endpoints authenticate via
`Authorization: Bearer <api-key>` (OpenAI-style), not `x-api-key` (native Anthropic
style). Configs with `provider_type = "vllm"` or `"sglang"` and a real `--api-key`
on the server side currently send the wrong header and get rejected.

vLLM/SGLang were registered in registry.rs (PR #7, merged) reusing the generic
`AnthropicCompatibleProvider`. The header logic was written for Anthropic-shaped
providers (Anthropic, OpenRouter, z.ai, Minimax, NVIDIA NIM, kimi-coding) and never
special-cased vllm/sglang's OpenAI-style bearer auth.

## Proposed fix

Add a per-instance flag to `AnthropicCompatibleProvider` that forces
`Authorization: Bearer` regardless of `auth_type` (Passthrough/OAuth already force
Bearer; this extends the same header for ApiKey-configured vllm/sglang instances).
Set the flag only for `"vllm"` and `"sglang"` in `registry.rs`. Extract the
duplicated 3x header-selection condition (`send_message`, `stream`, `count_tokens`)
into one `fn uses_bearer_auth(&self) -> bool` helper while touching this code.

## Files likely touched

- `src/providers/anthropic_compatible.rs` — add flag + builder method + helper fn, 3 call sites
- `src/providers/registry.rs` — set flag for vllm/sglang factories
- `src/providers/registry.rs` tests — update/add auth header assertions
- `config/models.example.toml` — comment note if behavior changes defaults
- `CHANGELOG.md` / docs — note the fix

---

## Phase 1: CEO Review (SELECTIVE EXPANSION, auto-decided)

### External verification (premise check)

WebSearch confirms the premise, previously unverified in the rough plan:
- vLLM: when started with `--api-key`, the `/v1` endpoints require `Authorization: Bearer <key>` — `x-api-key` is rejected. ([vLLM Security docs](https://docs.vllm.ai/en/latest/usage/security/))
- The identical bug class already exists in Ollama: [ollama/ollama#16922](https://github.com/ollama/ollama/issues/16922) — "Anthropic-compatible /v1/messages endpoint does not accept x-api-key authentication."
- LM Studio's Anthropic-compat shim accepts *either* header, so it would not hit this bug ([LM Studio docs](https://lmstudio.ai/docs/developer/anthropic-compat)).
- vLLM's native `/v1/messages` Anthropic-compat support has an open feature request ([vllm-project/vllm#21313](https://github.com/vllm-project/vllm/issues/21313)) — some deployments may front vLLM with an adapter; behavior should still be verified against the specific deployed version before merge.

### Dual Voices (CEO — strategy challenge)

**CLAUDE SUBAGENT (CEO — strategic independence):**
Flagged the proposed `forced_bearer: bool` flag as wrong-shaped — the real axis is "Anthropic-compatible endpoint using OpenAI-style bearer auth" vs "Anthropic-native x-api-key," not "which provider name." Recommended a `HeaderStyle` enum over a per-vendor boolean. Also flagged the premise as unverified (now addressed above) and noted Ollama hits the identical bug (confirmed above), which is real ecosystem-risk signal.

**CODEX SAYS (CEO — strategy challenge):**
Agreed the `forced-bearer-auth` flag is "a smell — encodes an exception, not a model," and independently recommended the same shape: an explicit `auth_scheme`/`AnthropicAuthHeader::{XApiKey, Bearer}` enum instead of a boolean. Explicitly disagreed with broadening scope to Ollama/TGI/LM Studio now ("speculative scope") — recommends making the internal shape general enough that adding another bearer-auth provider later is a registry data change, not a new conditional, while keeping this PR scoped to vLLM/SGLang.

```
CEO DUAL VOICES — CONSENSUS TABLE:
═══════════════════════════════════════════════════════════════════
  Dimension                            Claude   Codex   Consensus
  ───────────────────────────────────── ──────── ─────── ──────────
  1. Premises valid?                    Unverified→now verified (WebSearch)  CONFIRMED
  2. Right problem to solve?            Yes      Yes     CONFIRMED
  3. Scope calibration correct?         Flag ecosystem risk  Keep scoped to vllm/sglang  DISAGREE → resolved (see below)
  4. Alternatives sufficiently explored? No (gap) No (gap) CONFIRMED gap, now fixed
  5. Competitive/market risks covered?  Ollama has same bug  N/A     CONFIRMED (external verify)
  6. 6-month trajectory sound?          Only if enum adopted Only if enum adopted CONFIRMED (with enum)
═══════════════════════════════════════════════════════════════════
```

**Taste decision resolved (auto-decided, P3 pragmatic + P5 explicit over clever):** Adopt the `HeaderStyle`/`auth_scheme` enum (Claude+Codex consensus on shape) but do NOT add Ollama/TGI/LM Studio support in this PR (Codex's explicit scope objection + it's outside what the user asked for). Deferred to TODOS.md as a follow-up: "generalize forced-bearer header style to other self-hosted OpenAI-convention providers (Ollama, TGI) when support is added."

### NOT in scope
- Ollama/TGI/LM Studio provider support or header-style generalization beyond vLLM/SGLang — deferred to TODOS.md (not requested, no existing registry entries for these providers).
- Changing `AuthType` (ApiKey/OAuth/Passthrough) semantics — orthogonal concern (which credential value to send), untouched by this fix (which header name to send it in).

### What already exists
- `AuthType::Passthrough` and OAuth paths already force `Authorization: Bearer` — the new header-style concept only changes behavior for the `ApiKey`-configured path, reusing the existing Bearer-formatting code (`format!("Bearer {}", auth_value)`) already present at all 3 call sites.
- `new_with_options_and_auth` builder pattern already exists on `AnthropicCompatibleProvider` — the new field slots into the same builder, no new construction path needed.

### Revised implementation approach (supersedes "Proposed fix" above)
Add a `header_style: AnthropicAuthHeaderStyle` field (`XApiKey` default, `Bearer` variant) to `AnthropicCompatibleProvider`, replacing the ad-hoc bool. Header selection at all 3 call sites becomes: `Bearer` if `auth_type == Passthrough || is_oauth() || header_style == Bearer`, else `x-api-key`. Set `header_style: Bearer` only for `"vllm"` and `"sglang"` in `registry.rs`; every other provider factory keeps the default (`XApiKey`), preserving current behavior for Anthropic/OpenRouter/z.ai/Minimax/NVIDIA NIM/kimi-coding.

---

## Phase 3: Eng Review (dual voices)

### Architecture

```
AnthropicCompatibleProvider
  auth_type: AuthType (ApiKey | OAuth | Passthrough)   ─┐
  header_style: AnthropicAuthHeaderStyle (XApiKey*/Bearer) ─┴─▶ is_bearer_auth() ──▶ header name
                                                                  │
  3 call sites (send_message / stream / count_tokens) ───────────┘
  each currently duplicates: `if auth_type==Passthrough || is_oauth() { Bearer } else { x-api-key }`
```

Dependency graph: `registry.rs::create_provider` constructs `AnthropicCompatibleProvider` directly for `"anthropic"`/`"vllm"`/`"sglang"`, and indirectly (via `new_with_options_and_auth`) for `"z.ai"`/`"minimax"`/`"zenmux"`/`"kimi-coding"` through their dedicated `*_with_auth` constructors. A new **positional** constructor param would ripple through all ~8 call sites in `anthropic_compatible.rs` plus the 3 direct ones in `registry.rs`. A **builder method** (`.with_header_style(...)`, mirroring the existing `.with_rate_limit_config(...)` chain already used at the vllm/sglang call sites) touches zero existing signatures.

### Dual Voices (Eng — architecture challenge)

**CLAUDE SUBAGENT (eng — independent review):** Confirmed the positional-param risk is real (counted 8+ call sites into `new_with_options_and_auth`). Recommended `.with_header_style(Bearer)` builder method, defaulting the field to `XApiKey` in the base constructor — "strictly safer than threading a new positional arg through 8 functions." Confirmed Passthrough/OAuth + `header_style=Bearer` is redundant-but-harmless (OR logic), not a conflict. Flagged that header logic isn't currently exposed for testing — recommend a `pub(crate) fn effective_header_style(&self)` (or equivalent) so tests can assert behavior without a live HTTP mock. Endorsed the stored-enum-on-struct shape over keying off `provider_type` string at request time (would resurrect the exact scattered-conditional smell the CEO phase rejected the bool for). Noted a config-mismatch footgun (not a vuln): a user manually setting `provider_type = "vllm"` while pointing `base_url` at a real Anthropic-native endpoint would now get Bearer and the request would simply fail — worth a doc note in `config/models.example.toml`.

**CODEX SAYS (eng — architecture challenge):** Independently converged on the same builder-method shape and the same "don't key off provider_type at request time" conclusion. Additionally flagged: (1) the 3-call-site duplication of the header condition should be centralized into one helper (`is_bearer_auth()` / `apply_auth_header()`) rather than copy-pasted a 4th time — "three copies invite future drift between send_message, stream, and count_tokens"; (2) tests must assert the **actual outbound header**, not just provider registration/construction, and enumerated 6 specific cases (vllm/sglang static-key Bearer, default provider still x-api-key, passthrough vllm uses caller token as Bearer not configured key, OAuth still Bearer, count_tokens path); (3) a genuinely new edge case — `custom_headers` (the `Vec<(String, String)>` used for things like OpenRouter's `HTTP-Referer`) is applied via `req_builder.header(key, value)` *after* the auth header at all 3 call sites; if a vllm/sglang config's `custom_headers` ever included an `"Authorization"` or `"x-api-key"` entry, reqwest would send a duplicate header rather than override. Pre-existing risk, but newly worth a regression test given this PR touches the exact code path.

```
ENG DUAL VOICES — CONSENSUS TABLE:
═══════════════════════════════════════════════════════════════════
  Dimension                            Claude   Codex   Consensus
  ───────────────────────────────────── ──────── ─────── ──────────
  1. Architecture sound?                Builder method  Builder method  CONFIRMED
  2. Test coverage sufficient?          Gap: no header-assert tests  Gap: same + custom_headers  CONFIRMED gap, now planned
  3. Performance risks addressed?       N/A (no perf surface)  N/A     CONFIRMED (no findings)
  4. Security threats covered?          Config-mismatch footgun (not vuln)  Custom-header dup risk  CONFIRMED, both addressed below
  5. Error paths handled?               Existing AuthError paths unaffected  Same  CONFIRMED
  6. Deployment risk manageable?        Additive, backward compatible  Same  CONFIRMED
═══════════════════════════════════════════════════════════════════
```

**Auto-decided (P5 explicit + P3 pragmatic):** Adopt `.with_header_style(Bearer)` builder method (not a positional param). Extract the duplicated 3-call-site condition into one `fn is_bearer_auth(&self) -> bool` helper. Add a test-only accessor or keep `is_bearer_auth()` `pub(crate)` so registry tests can assert it directly without a live HTTP mock — avoids the cost of standing up a mock server for a header-name check.

### Section 2: Error & Rescue Map
No new error paths. `get_auth_header()`'s existing `ProviderError::AuthError` cases (passthrough-without-token, oauth-provider-missing-token) are unchanged — `header_style` only affects which header name wraps the resolved auth value, not how that value is resolved or validated. **No issues found.**

### Section 4: Data Flow & Interaction Edge Cases
```
  auth_type + header_style ──▶ is_bearer_auth() ──▶ header name
    [Passthrough]        → true  (existing)
    [OAuth]               → true  (existing)
    [ApiKey, XApiKey]     → false → x-api-key (existing default, unchanged)
    [ApiKey, Bearer]      → true  → Authorization: Bearer  (NEW — vllm/sglang path)
```
No nil/empty/error shadow paths introduced — `header_style` is a plain enum set once at construction, never user/request-input-derived.

### Section 6: Test Review
```
NEW CODEPATHS:
  - is_bearer_auth() helper (extracted from 3x duplicated condition)
  - header_style field on AnthropicCompatibleProvider (XApiKey default)
  - registry.rs: header_style=Bearer set for "vllm"/"sglang" factories

TEST COVERAGE:
  [GAP] vllm/sglang + ApiKey auth → sends Authorization: Bearer, not x-api-key   [Unit — assert via is_bearer_auth()]
  [GAP] anthropic/openrouter/zai/minimax/kimi-coding + ApiKey → still x-api-key (regression guard)  [Unit]
  [GAP] vllm/sglang + Passthrough auth → still Bearer with caller token, not configured key  [Unit — extends existing test_vllm_passthrough_auth_registration]
  [GAP] vllm/sglang + OAuth → still Bearer  [Unit, if OAuth+vllm is a supported combination — else document why not]
  [GAP] custom_headers containing "Authorization" alongside header_style=Bearer → document/test precedence (Codex finding)  [Unit or explicit doc note if deferred]

COVERAGE: 0/5 new paths tested today (all GAPS — this is the point of the fix)
```
Test plan artifact written below (Test Plan Artifact section).

### Section 7: Performance Review
No new allocations, no new I/O, no new hot path. `is_bearer_auth()` is a cheap enum/bool comparison evaluated once per request alongside the existing check it replaces. **No issues found.**

### Test Plan Artifact
Written to `~/.gstack/projects/9j-claude-code-mux/hyonchoi-main-eng-review-test-plan-20260709-164118.md`.

### TODOS.md updates
- **T0 (P3, deferred):** Generalize the bearer/x-api-key header-style concept to other self-hosted OpenAI-convention providers (Ollama — confirmed via [ollama/ollama#16922](https://github.com/ollama/ollama/issues/16922) — and TGI) when support for those providers is added. Not built now: no registry entries exist for them yet, and both CEO-phase models agreed broadening now is speculative scope.

### Implementation Tasks
Synthesized from CEO + Eng review findings above.

- [ ] **T1 (P1, human: ~30min / CC: ~5min)** — AnthropicCompatibleProvider — Add `AnthropicAuthHeaderStyle` enum (`XApiKey` default/`Bearer`) + `.with_header_style()` builder method, extract `is_bearer_auth()` helper across all 3 call sites
  - Surfaced by: Eng dual voices — positional-param risk (8+ call sites) + 3x condition duplication
  - Files: `src/providers/anthropic_compatible.rs`
  - Verify: `cargo test providers::anthropic_compatible`
- [ ] **T2 (P1, human: ~10min / CC: ~2min)** — registry.rs — Set `header_style=Bearer` only for vllm/sglang provider factories via `.with_header_style(Bearer)`
  - Surfaced by: CEO+Eng consensus — scope narrowly to vllm/sglang
  - Files: `src/providers/registry.rs`
  - Verify: `cargo test providers::registry`
- [ ] **T3 (P1, human: ~30min / CC: ~10min)** — tests — Assert actual header sent: vllm/sglang+ApiKey→Bearer, other providers+ApiKey→x-api-key (regression), vllm/sglang+Passthrough→Bearer with caller token
  - Surfaced by: Codex eng review — existing tests only assert registration, never header contents
  - Files: `src/providers/anthropic_compatible.rs`, `src/providers/registry.rs`
  - Verify: `cargo test`
- [ ] **T4 (P2, human: ~10min / CC: ~2min)** — docs — Document custom_headers vs auth-header precedence and the vllm-pointed-at-real-Anthropic-endpoint config-mismatch footgun
  - Surfaced by: Codex + Claude subagent — custom_headers duplication risk, config-mismatch footgun
  - Files: `config/models.example.toml`
  - Verify: manual review
- [ ] **T5 (P3, human: ~5min / CC: ~2min)** — docs — Note fix in CHANGELOG.md
  - Surfaced by: repo convention (git log shows every user-facing fix gets a CHANGELOG entry)
  - Files: `CHANGELOG.md`
  - Verify: manual review

### Completion Summary
```
+====================================================================+
|            /autoplan REVIEW — COMPLETION SUMMARY                   |
+====================================================================+
| CEO Review           | SELECTIVE EXPANSION, premise verified (WebSearch) |
| CEO Dual Voices      | Codex + Claude subagent, 1 taste decision resolved |
| Eng Review           | Architecture/Test/Perf sections run, 0 open findings|
| Eng Dual Voices      | Codex + Claude subagent, converged on builder+helper|
| Design Review        | SKIPPED — no UI scope detected                     |
| DX Review            | SKIPPED — no developer-facing scope detected       |
| NOT in scope         | written (2 items)                                  |
| What already exists  | written                                            |
| TODOS.md updates     | 1 item proposed (T0, generalization)               |
| Test plan artifact   | written to disk                                    |
| Implementation Tasks | 5 (3 P1, 1 P2, 1 P3)                                |
| Unresolved decisions | 0                                                   |
+====================================================================+
```

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| CEO Review | `/plan-ceo-review` (via autoplan) | Scope & strategy | 1 | CLEAR | Premise unverified→verified, enum-over-bool adopted, scope held to vllm/sglang |
| Codex Review | `/codex review` (dual voice, both phases) | Independent 2nd opinion | 2 | CLEAR | Converged with Claude subagent both phases |
| Eng Review | `/plan-eng-review` (via autoplan) | Architecture & tests (required) | 1 | CLEAR | 5 implementation tasks, 0 critical gaps |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | SKIPPED | No UI scope |
| DX Review | `/plan-devex-review` | Developer experience gaps | 0 | SKIPPED | No developer-facing scope |

**CROSS-MODEL:** Claude subagent and Codex independently converged on the same fix shape in both phases (enum over bool in CEO phase; builder method + centralized helper in Eng phase) without seeing each other's output — high-confidence signal this is the right shape.
**VERDICT:** CEO + ENG CLEARED — ready to implement.

NO UNRESOLVED DECISIONS

---

# Addendum: NVIDIA NIM Anthropic-Compatible Migration (added 2026-07-09, via /autoplan)

## Rough plan (as given)

"migrate nvidia-nim from openai-compatible to anthropic-compatible (I have confirmed,
nvidia-nim supports anthropic api endpoints). with bearer auth."

## Premise Gate (Phase 1, not auto-decided)

**D1 asked directly:** the codebase's `nvidia-nim` provider_type defaults `base_url` to
`https://integrate.api.nvidia.com/v1` — NVIDIA's *hosted* build.nvidia.com catalog, not
a self-hosted NIM container. Public docs describe a native Anthropic `/v1/messages`
endpoint only for self-hosted NIM; most guides for the hosted catalog use third-party
adapters (LiteLLM) to bridge Anthropic→OpenAI, suggesting the hosted catalog has
historically been OpenAI-only.

**User response:** "I confirmed they support /v1/messages endpoint." Taken as ground
truth for the specific model(s) the user tested. Premise gate: **PASSED for reachability**,
but see User Challenge below — heterogeneity across NVIDIA's multi-model hosted catalog
was not addressed by this confirmation and remains an open strategic question.

## Phase 1: CEO Review (SELECTIVE EXPANSION, auto-decided except User Challenge)

### Dual Voices (CEO — strategy challenge)

**CLAUDE SUBAGENT (CEO — strategic independence):** Flagged the proposal as a
lateral rewrite of a working, tested integration (5+ CHANGELOG entries, admin UI
support, test-plan history since 2026-05) with no named capability gain over the
current OpenAI-compat path. Unverified premise: NVIDIA's hosted catalog spans many
heterogeneous backend models (Llama, Mistral, Nemotron, etc.), each potentially with
different tool-calling/streaming maturity — one verified model does not establish
catalog-wide `/v1/messages` parity. 6-month regret: a user on a different NIM-hosted
model gets silently malformed tool calls/dropped content, misdiagnosed as "our bug."
Recommended: spike-first with a committed request/response fixture, and/or an
additive opt-in shape rather than a wholesale swap.

**CODEX SAYS (CEO — strategy challenge):** Independently converged on the same
critique. "The dangerous premise is: one hosted endpoint accepted one Anthropic-shaped
request, therefore nvidia-nim should globally become Anthropic-compatible. That does
not follow." Named the same alternatives: spike fixture, config-level opt-in
(`provider_type = "nvidia-nim-anthropic"`), or fixing isolated OpenAI-compat gaps
directly. Framed the strategic question as "what stable contract does `nvidia-nim`
promise users?" — a wholesale swap breaks that contract for existing OpenAI-compat
users.

```
CEO DUAL VOICES — CONSENSUS TABLE:
═══════════════════════════════════════════════════════════════════
  Dimension                            Claude   Codex   Consensus
  ───────────────────────────────────── ──────── ─────── ──────────
  1. Premises valid?                    Unverified at catalog scale  Same  CONFIRMED gap
  2. Right problem to solve?            No named capability gain    Same  CONFIRMED gap
  3. Scope calibration correct?         Additive opt-in, not swap   Same  CONFIRMED (both agree)
  4. Alternatives sufficiently explored? No (gap, now fixed here)   Same  CONFIRMED gap, now fixed
  5. Competitive/market risks covered?  N/A                         N/A   N/A
  6. 6-month trajectory sound?          Only if additive + fixtures Same  CONFIRMED (with additive shape)
═══════════════════════════════════════════════════════════════════
```

### USER CHALLENGE (not auto-decided — surfaced at final gate)

**What the user asked for:** migrate `nvidia-nim` (the existing provider_type) from
OpenAI-compatible to Anthropic-compatible, with bearer auth — i.e., change what
`provider_type = "nvidia-nim"` does for existing users.

**What both models recommend instead:** do NOT change the existing `"nvidia-nim"`
provider_type's behavior. Add a new, separate, opt-in provider_type
(`"nvidia-nim-anthropic"`) that existing users are not silently moved onto. Existing
`nvidia-nim` configs keep working exactly as today (OpenAI-compat, unaffected).

**Why:** NVIDIA's hosted catalog serves many different backend models with
independently varying Anthropic-Messages-API fidelity (tool_use blocks, streaming
event shapes, `usage` field presence — the last one has a *confirmed prior bug* in
this exact codebase for the OpenAI-compat path per CHANGELOG.md). The user verified
one model/endpoint combination works; that doesn't establish it for the catalog. A
silent behavior change on `nvidia-nim` risks breaking already-configured users the
moment they're on a NIM-hosted model that doesn't have full Anthropic parity, with no
fallback short of a revert.

**What context we might be missing:** the user may only ever use one specific
NIM-hosted model, in which case the catalog-heterogeneity risk is moot for their case
— but other `ccm` users configuring `nvidia-nim` with a different model would inherit
the risk if the swap changes shared default behavior.

**If we're wrong (i.e., a straight swap was actually fine), the cost of building the
additive version instead is:** one extra provider_type string, one extra constructor
function (~15 lines, mirrors `zai_with_auth`), and one extra registry match arm.
Marginal cost is low; this is why both models recommend it regardless of whether the
heterogeneity risk ever materializes.

**Your call** — proceed with the additive shape (recommended by both models), or
confirm you want the existing `nvidia-nim` provider_type's default behavior changed
directly.

**RESOLVED (user decision, 2026-07-09):** Direct swap. The user's original direction
stands — change `provider_type = "nvidia-nim"` itself to construct
`AnthropicCompatibleProvider` with `.with_header_style(Bearer)`, replacing the
`OpenAIProvider` path entirely, rather than adding a separate opt-in provider_type.
User has direct knowledge of which NIM-hosted model(s) they route to and accepted
the catalog-heterogeneity risk both models flagged. Implementation Tasks below are
updated to reflect this (T1/T2 now modify the existing `"nvidia-nim"` arm in place
rather than adding a new one). The base_url bare-host fix, shared usage-field
defensive parsing fix, and passthrough-auth exclusion findings apply unchanged
regardless of shape and remain in scope.

### NOT in scope
- Verifying every NVIDIA NIM-hosted model's individual tool-use/streaming fidelity
  against the Anthropic Messages spec — infeasible to test exhaustively; addressed
  instead by keeping this additive/opt-in and defensive on parse (see Eng phase).
- Removing or deprecating the existing OpenAI-compatible `nvidia-nim` path — out of
  scope, no evidence it's broken for any currently-supported model.

### What already exists
- `AnthropicAuthHeaderStyle::{XApiKey, Bearer}` + `.with_header_style()` builder +
  `is_bearer_auth()` helper already exist on `AnthropicCompatibleProvider`
  (added by the vLLM/SGLang fix earlier on this branch) — the new provider needs zero
  new auth-header plumbing, just `.with_header_style(Bearer)` at construction.
- The `*_with_auth` per-vendor constructor pattern (`zai_with_auth`, `minimax_with_auth`,
  `zenmux_with_auth`, `kimi_coding_with_auth`) is an established, low-risk template to
  copy for `nvidia_nim_anthropic_with_auth`.
- `count_tokens` already falls back to character-based estimation for every
  non-`"anthropic"`-named instance (anthropic_compatible.rs:726) — no live
  `/v1/messages/count_tokens` call is made for vllm/sglang/z.ai/minimax/zenmux/kimi
  today, so the new nvidia-nim-anthropic instance inherits this safe default
  automatically; no NIM-specific count_tokens verification needed.
- `should_use_passthrough_auth` (server/mod.rs:1314) already deliberately excludes
  z.ai/minimax/zenmux/kimi-coding from passthrough eligibility despite being
  AnthropicCompatibleProvider-backed — the new type follows the same precedent
  (excluded by default, not a new gap).

## Phase 3: Eng Review (dual voices)

### Architecture

```
registry.rs::from_configs match on provider_type:
  "nvidia-nim"            (existing, unchanged) → OpenAIProvider   (default, OpenAI Chat Completions)
  "nvidia-nim-anthropic"  (NEW)                 → AnthropicCompatibleProvider::nvidia_nim_anthropic_with_auth(...)
                                                     .with_header_style(Bearer)
                                                     .with_rate_limit_config(...)
```
Mirrors the existing `"z.ai"`/`"minimax"`/`"zenmux"`/`"kimi-coding"` arms exactly —
one new match arm, one new constructor, zero changes to existing call sites.

### Dual Voices (Eng — architecture challenge)

**CLAUDE SUBAGENT (eng — independent review):** Architecture confirmed sound and
consistent with the established pattern — low blast radius, one-arm addition.
Flagged two concrete correctness risks from reading the actual code:
(1) `AnthropicCompatibleProvider::send_message` builds the URL as
`format!("{}/v1/messages", self.base_url)` — copying the existing `nvidia-nim`
default base_url (`https://integrate.api.nvidia.com/v1`, which already ends in `/v1`)
verbatim into the new constructor produces `.../v1/v1/messages` (guaranteed 404). The
new constructor's default must be the bare host (`https://integrate.api.nvidia.com`),
matching the z.ai/minimax/vllm convention. (2) `ProviderResponse.usage` is a
*required*, non-`Option` `Usage{input_tokens: u32, output_tokens: u32}` struct with
strict `serde_json::from_str` and zero defensive fallback. CHANGELOG.md confirms
NVIDIA NIM's OpenAI Chat Completions path has *historically omitted the usage field
entirely* (fixed there with `Option<OpenAIUsage>`). If any NIM-hosted model's
Anthropic-compat response ever omits `usage`, this new path fails the *entire*
response parse — strictly worse than the graceful zero-fallback behavior on the
existing OpenAI-compat path.

**CODEX SAYS (eng — architecture challenge):** Independently confirmed both findings
by reading the same code paths, and extended finding (2): this is not NVIDIA-specific
— it's a *latent, pre-existing bug in `AnthropicCompatibleProvider` shared by every
backend it already serves* (z.ai/minimax/vllm/sglang/zenmux/kimi-coding), since
`ProviderResponse.usage` has always been required. Recommended fixing it at the
shared deserialization layer (map missing `usage` to zero) rather than only for the
new NIM path — in blast radius of the file already being touched, low effort.
Additional findings: (3) passthrough-auth exclusion should be explicit and tested,
not incidental — `Authorization: Bearer <nvapi-key>` (header_style) is orthogonal to
`AuthType::Passthrough` (whose token, forwarded or configured); conflating them risks
a future change accidentally forwarding an unrelated caller token to NVIDIA. (4) Add
a config example with the correct bare-host default so users don't confuse
`nvidia-nim` (OpenAI-compat, `/v1`-suffixed default) with `nvidia-nim-anthropic`
(Anthropic-compat, bare-host default).

```
ENG DUAL VOICES — CONSENSUS TABLE:
═══════════════════════════════════════════════════════════════════
  Dimension                            Claude   Codex   Consensus
  ───────────────────────────────────── ──────── ─────── ──────────
  1. Architecture sound?                Yes, one-arm addition   Yes  CONFIRMED
  2. Test coverage sufficient?          Gap: URL/usage/passthrough tests missing  Same + broader scope  CONFIRMED gap, now planned
  3. Performance risks addressed?       N/A (no perf surface)   N/A  CONFIRMED (no findings)
  4. Security threats covered?          N/A                     Passthrough-token conflation risk  CONFIRMED, addressed below
  5. Error paths handled?               usage-field hard-fail gap  Same, + shared-bug scope  CONFIRMED gap, now planned
  6. Deployment risk manageable?        Additive, zero regression to existing nvidia-nim  Same  CONFIRMED
═══════════════════════════════════════════════════════════════════
```

**Auto-decided (P2 boil lakes + P5 explicit):** Fix the `usage`-field strictness at
the shared `AnthropicCompatibleProvider` response-parsing layer (not NIM-specific) —
in blast radius of the file already being touched by this change, <1 day effort,
benefits every existing backend (z.ai/minimax/vllm/sglang/zenmux/kimi-coding) for
free. Codex's exact recommendation adopted: map a missing/malformed `usage` object to
`Usage{0,0}` instead of hard-failing the whole response parse.

**Auto-decided (P5 explicit):** New constructor `nvidia_nim_anthropic_with_auth`
defaults `base_url` to `"https://integrate.api.nvidia.com"` (bare host, no `/v1`
suffix) — matches z.ai/minimax/vllm convention, avoids the double-`/v1` bug.

**Auto-decided (P3 pragmatic, matches existing precedent):** `nvidia-nim-anthropic`
is excluded from `should_use_passthrough_auth`'s match arm — same treatment as
z.ai/minimax/zenmux/kimi-coding. Add an explicit test asserting this, per Codex's
finding, so a future broadening doesn't silently include it.

### Section 3: Test Review
```
NEW CODEPATHS:
  - registry.rs "nvidia-nim-anthropic" match arm
  - anthropic_compatible.rs::nvidia_nim_anthropic_with_auth constructor
  - AnthropicCompatibleProvider response parsing: usage-field tolerant fallback (shared fix)

TEST COVERAGE:
  [GAP] nvidia-nim-anthropic + ApiKey → sends Authorization: Bearer, not x-api-key   [Unit — assert via is_bearer_auth()]
  [GAP] nvidia-nim-anthropic default base_url → posts to https://integrate.api.nvidia.com/v1/messages (not /v1/v1/messages)  [Unit — assert constructed URL, regression guard for the double-/v1 bug]
  [GAP] existing "nvidia-nim" (OpenAI-compat) unaffected — still constructs OpenAIProvider, still hits /v1/chat/completions  [Unit — regression guard]
  [GAP] AnthropicCompatibleProvider response missing `usage` field → parses successfully with Usage{0,0}, not a hard failure  [Unit — covers z.ai/minimax/vllm/sglang/zenmux/kimi-coding too, not just NIM]
  [GAP] nvidia-nim-anthropic excluded from should_use_passthrough_auth (returns false even with auth_type=Passthrough configured)  [Unit, extends existing passthrough-eligibility test table]

COVERAGE: 0/5 new paths tested today (all GAPS — this is the point of the fix)
```

### Section 7: Performance Review
No new allocations, no new I/O, no new hot path. One additional `match` arm
(compile-time dispatch) and one additional enum-tag check in the shared response
parser. **No issues found.**

### Test Plan Artifact
Written to `~/.gstack/projects/9j-claude-code-mux/hyonchoi-feat-bearer-auth-type-nvidia-nim-anthropic-test-plan-20260709-171647.md`.

### TODOS.md updates
None — all identified work is in-scope and scheduled as Implementation Tasks below
(the usage-field fix is in blast radius, not deferred).

### Implementation Tasks
Synthesized from CEO + Eng review findings above. **Updated per user decision:
direct swap (Option B), not additive** — `provider_type = "nvidia-nim"` itself
changes.

- [ ] **T1 (P1, human: ~20min / CC: ~5min)** — anthropic_compatible.rs — Add
  `nvidia_nim_with_auth(api_key, models, auth_type, token_store)` constructor
  (Anthropic-compatible), default base_url `https://integrate.api.nvidia.com` (bare
  host — NOT the current `/v1`-suffixed default, which would double up), mirroring
  `zai_with_auth`
  - Surfaced by: Eng dual voices — established pattern, double-`/v1` bug avoidance
  - Files: `src/providers/anthropic_compatible.rs`
  - Verify: `cargo test providers::anthropic_compatible`
- [ ] **T2 (P1, human: ~10min / CC: ~2min)** — registry.rs — Change the existing
  `"nvidia-nim"` match arm to call T1's constructor + `.with_header_style(Bearer)` +
  `.with_rate_limit_config(...)`, replacing the current `OpenAIProvider`
  construction entirely (direct swap, per user decision — see resolved User
  Challenge above)
  - Surfaced by: user decision (Option B) overriding CEO dual-voice recommendation
  - Files: `src/providers/registry.rs`
  - Verify: `cargo test providers::registry`
- [ ] **T3 (P1, human: ~30min / CC: ~10min)** — anthropic_compatible.rs — Fix
  `usage`-field strictness in response parsing: tolerate missing/malformed `usage`,
  default to `Usage{0,0}` instead of hard-failing `serde_json::from_str` on the whole
  response
  - Surfaced by: Codex eng review — shared latent bug across all
    AnthropicCompatibleProvider backends, not NIM-specific; confirmed via CHANGELOG.md
    precedent (same bug class already fixed once for OpenAI-compat path). Now higher
    priority given the direct swap puts `nvidia-nim` on this path unconditionally.
  - Files: `src/providers/anthropic_compatible.rs`
  - Verify: `cargo test providers::anthropic_compatible` (add a missing-usage fixture test)
- [ ] **T4 (P1, human: ~20min / CC: ~5min)** — tests — Assert: bearer header sent,
  correct default URL (`/v1/messages` not `/v1/v1/messages`), `usage` fallback,
  passthrough exclusion. Update/replace the now-stale
  `test_nvidia_nim_uses_openai_chat_completions_endpoint` test (registry.rs ~line
  595) since `nvidia-nim` no longer uses OpenAI Chat Completions.
  - Surfaced by: Eng dual voices consensus — GAPs identified in Section 3, plus
    direct-swap requires updating the now-incorrect existing regression test
  - Files: `src/providers/anthropic_compatible.rs`, `src/providers/registry.rs`, `src/server/mod.rs`
  - Verify: `cargo test`
- [ ] **T5 (P2, human: ~15min / CC: ~5min)** — config — Update
  `config/templates/nvidia-nim.toml`, `config/default.example.toml`, and
  `docs/reference/providers.md` (currently states `nvidia-nim` → "OpenAI Chat
  Completions") to reflect the new bare-host `base_url` and Anthropic-compatible
  behavior; note this is a **breaking change** for anyone with an explicit
  `base_url = ".../v1"` override in their config (now double-`/v1`)
  - Surfaced by: Codex eng review (discoverability) + direct-swap breaking-change risk
  - Files: `config/templates/nvidia-nim.toml`, `config/default.example.toml`, `docs/reference/providers.md`
  - Verify: manual review
- [ ] **T6 (P2, human: ~5min / CC: ~2min)** — docs — Note the breaking behavior
  change in CHANGELOG.md (existing `nvidia-nim` users on OpenAI-compat move to
  Anthropic-compat on upgrade; anyone with an explicit `/v1`-suffixed `base_url`
  override must drop the suffix)
  - Surfaced by: repo convention + direct-swap breaking-change risk (bumped from P3
    to P2 given this is no longer purely additive)
  - Files: `CHANGELOG.md`
  - Verify: manual review

### Completion Summary
```
+====================================================================+
|            /autoplan REVIEW — COMPLETION SUMMARY (addendum)        |
+====================================================================+
| CEO Review           | SELECTIVE EXPANSION; User Challenge resolved (direct swap)|
| CEO Dual Voices      | Codex + Claude subagent, both recommended additive;       |
|                       | user chose direct swap with full context                  |
| Eng Review           | Architecture/Test/Perf sections run, 1 shared bug found  |
| Eng Dual Voices      | Codex + Claude subagent, converged on all findings       |
| Design Review        | SKIPPED — no UI scope detected                            |
| DX Review            | Folded into Eng findings (naming/docs discoverability,    |
|                       | T5/T6) rather than a separate dual-voice pass — P4 DRY,   |
|                       | avoids duplicating Eng finding #6/#7 verbatim              |
| NOT in scope          | written (2 items)                                          |
| What already exists   | written (4 items)                                          |
| TODOS.md updates      | none — all work scheduled as Implementation Tasks          |
| Test plan artifact    | written to disk                                            |
| Implementation Tasks  | 6 (5 P1/P2 mix, direct-swap shape — see updated T1-T6)      |
| Unresolved decisions  | 0 — resolved by user                                       |
+====================================================================+
```

## GSTACK REVIEW REPORT (addendum)

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| CEO Review | `/plan-ceo-review` (via autoplan) | Scope & strategy | 1 | RESOLVED (user: direct swap) | Both models recommended additive shape; user chose direct swap with full context |
| Codex Review | `/codex review` (dual voice, both phases) | Independent 2nd opinion | 2 | CLEAR | Converged with Claude subagent both phases |
| Eng Review | `/plan-eng-review` (via autoplan) | Architecture & tests (required) | 1 | CLEAR | 6 implementation tasks, 1 shared latent bug found (usage-field strictness) |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | SKIPPED | No UI scope |
| DX Review | `/plan-devex-review` | Developer experience gaps | 0 | FOLDED INTO ENG | Naming/docs discoverability covered by T5/T6 |

**CROSS-MODEL:** Claude subagent and Codex independently converged on the same
critique in both phases without seeing each other's output — CEO phase (additive
shape over direct swap) and Eng phase (double-`/v1` bug, usage-field strictness,
passthrough exclusion) — high-confidence signal.

**VERDICT:** ENG CLEARED. User Challenge RESOLVED — direct swap approved by user.
Ready to implement per T1-T6 (updated for direct-swap shape).

NO UNRESOLVED DECISIONS.

<!-- AUTONOMOUS DECISION LOG (addendum) -->
## Decision Audit Trail (NVIDIA NIM addendum)

| # | Phase | Decision | Classification | Principle | Rationale | Rejected |
|---|-------|----------|-----------------|-----------|-----------|----------|
| 1 | CEO | Direct swap of existing `nvidia-nim` provider_type (user overrode both models' additive recommendation) | User Challenge (resolved by user, not auto-decided) | — | User has direct knowledge of which NIM-hosted model(s) they route to; accepted catalog-heterogeneity risk both models flagged | Additive `nvidia-nim-anthropic` provider_type |
| 2 | Eng | New constructor defaults `base_url` to bare host `https://integrate.api.nvidia.com` | Mechanical | P5 explicit | Avoids double-`/v1` 404 bug; matches z.ai/minimax/vllm convention | Copying existing nvidia-nim's `/v1`-suffixed default |
| 3 | Eng | Fix `usage`-field strictness in shared `AnthropicCompatibleProvider` response parser (not NIM-specific) | Taste (in-scope expansion) | P2 boil lakes | In blast radius of file already touched, <1 day effort, benefits all 6 existing backends | Scoping the fix to only the new NIM path |
| 4 | Eng | Exclude `nvidia-nim-anthropic` from `should_use_passthrough_auth`, add explicit test | Mechanical | P3 pragmatic | Matches existing precedent (z.ai/minimax/zenmux/kimi-coding already excluded) | Adding it to the passthrough-eligible list |
| 5 | CEO/Eng | DX review folded into Eng findings rather than a separate dual-voice phase | Mechanical | P4 DRY | Avoids duplicating Eng finding #6/#7 (naming/docs discoverability) verbatim | Full separate DX dual-voice phase |
| 6 | Eng | `count_tokens` needs no NIM-specific handling | Mechanical | — | Already falls back to character estimation for all non-`"anthropic"`-named instances | Adding a live count_tokens call for NIM |
