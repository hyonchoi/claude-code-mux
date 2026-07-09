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
