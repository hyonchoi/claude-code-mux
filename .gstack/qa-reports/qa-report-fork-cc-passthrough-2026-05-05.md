# QA Report — fork/cc-passthrough

**Date:** 2026-05-05  
**Branch:** fork/cc-passthrough  
**Mode:** Diff-aware (no URL — app not running, branch is documentation-only)  
**Scope:** 4 changed files (388 additions)  
**Duration:** ~10 minutes  
**No test framework detected.** Run `/qa` to bootstrap one and enable regression test generation.

---

## Summary

| Metric | Value |
|--------|-------|
| Files changed | 4 |
| Issues found | 5 |
| Critical | 0 |
| High | 1 |
| Medium | 2 |
| Low | 2 |
| Health Score | **72/100** |

**Top 3 things to fix:**
1. `Cargo.lock` listed in `.gitignore` while being tracked — silently blocks contributors staging lockfile updates
2. Spec file map references non-existent filenames for 6 of 8 target files — will cause confusion when implementation begins
3. `TODOS.md` is untracked on this branch despite being an active work artifact

---

## App Testing

No running application found on localhost:3000, :4000, :8080, :8181, :13456, :3456. The branch contains documentation and configuration changes only — no implementation code was changed. Browser-based testing of relay behavior is not applicable at this stage.

---

## Issues

### ISSUE-001 — `Cargo.lock` in `.gitignore` conflicts with tracked state
**Severity:** High  
**Category:** Configuration  

`.gitignore` line 3 lists `Cargo.lock`. But `Cargo.lock` is currently tracked by git (`git ls-files Cargo.lock` confirms). This contradiction means:
- `git add Cargo.lock` silently fails for contributors after dependency updates (the file is ignored, so staging requires `git add -f Cargo.lock`)
- New contributors may be confused when their lockfile changes don't appear in `git status`
- For a binary project, Cargo recommends committing `Cargo.lock` — the `.gitignore` entry is working against this

**Repro:**
```bash
git ls-files Cargo.lock          # → "Cargo.lock" (tracked)
git check-ignore -v Cargo.lock   # → ".gitignore:3:Cargo.lock" (also ignored)
# Attempt to stage after editing:
touch Cargo.lock && git add Cargo.lock  # → silently does nothing
```

**Expected:** Either remove `Cargo.lock` from `.gitignore` (binary project, should commit lockfile) or stop tracking it.

---

### ISSUE-002 — Spec file map uses wrong/non-existent filenames
**Severity:** Medium  
**Category:** Documentation  

The implementation spec's File Map (`docs/superpowers/specs/2026-05-05-claude-code-passthrough-spec.md`) references 8 files. 6 of those either don't exist or use the wrong name:

| Spec Reference | Actual State | Problem |
|---------------|--------------|---------|
| `src/server/relay.rs` | Does not exist | Actual handler is `src/server/openai_compat.rs` |
| `src/providers/anthropic.rs` | Does not exist | Actual file is `src/providers/anthropic_compatible.rs` |
| `src/providers/openai.rs` | ✅ Exists | Correct |
| `src/auth/passthrough.rs` | Does not exist | New file (OK — noted as new) |
| `src/router/fallback.rs` | Does not exist | Actual router is `src/router/mod.rs` |
| `src/error.rs` | Does not exist | Actual is `src/providers/error.rs` |
| `src/logging.rs` | Does not exist | New file (OK — noted as new) |
| `src/tests/passthrough.rs` | Does not exist | New file (OK — noted as new) |

The `relay.rs` and `anthropic.rs` naming suggest the implementor will create new files with these names rather than modifying the existing `openai_compat.rs` and `anthropic_compatible.rs`. The spec should either:
- Clarify which existing files get modified vs. which new files get created
- Or update the file names to match actuals

**Repro:** Cross-reference the File Map table against `find src -name "*.rs"`.

---

### ISSUE-003 — `TODOS.md` is untracked on this branch
**Severity:** Medium  
**Category:** Content  

`TODOS.md` exists locally with 5 active P1/P2 items directly related to the passthrough relay work, but it shows as `??` (untracked) in `git status` on `fork/cc-passthrough`. It was committed on a different branch (`dd3446c update todos`) that is not an ancestor of this branch.

The TODOS describe open items that block or scope the implementation:
- [P1] Rollback Control Contract For Passthrough Relay
- [P1] Deterministic Fallback Candidate Selection Policy
- [P2] Canonical Observability SLO Contract
- [P2] Checkpoint Escalation SLA
- [P2] Benchmark Measurement Protocol

A reviewer picking up `fork/cc-passthrough` will not see these items unless they know to look in an untracked file.

**Expected:** Either cherry-pick the `dd3446c` commit or commit TODOS.md to this branch.

---

### ISSUE-004 — Spec example data uses stale model name `claude-4-5-sonnet`
**Severity:** Low  
**Category:** Documentation  

Two places in the spec use `claude-4-5-sonnet` as example model data:
- Error response JSON: `"original_model": "claude-4-5-sonnet"`
- Structured log example: `original: claude-4-5-sonnet, final: claude-instant`

The actual model IDs in this codebase use the format `claude-sonnet-4-6`, `claude-haiku-4-5-20250929` (from `config/claude-max-oauth.example.toml`). `claude-instant` is a legacy model no longer available.

These are example values only so they don't break anything, but they'll confuse implementors testing against real model IDs.

---

### ISSUE-005 — `rtk hook copilot` timeout of 5 seconds may be insufficient
**Severity:** Low  
**Category:** Configuration  

`.github/hooks/rtk-rewrite.json` sets `"timeout": 5` (seconds) for the `rtk hook copilot` PreToolUse hook. RTK's own docs note it can save 60-90% of tokens on cargo/docker commands — if `rtk` is processing a large `cargo test` or `cargo build` output buffer, 5 seconds may cause the hook to be killed mid-run, falling back to unfiltered output.

The CLAUDE.md global config uses no explicit timeout, which defaults to the Claude Code hook runner default (typically 30s).

**Suggested:** Increase to `"timeout": 30` to match global defaults, or remove the timeout to inherit the runner default.

---

## Scope: What This Branch Changes

| File | Type | Status |
|------|------|--------|
| `.github/copilot-instructions.md` | New | RTK + gstack/superpowers workflow for Copilot CLI |
| `.github/hooks/rtk-rewrite.json` | New | RTK PreToolUse hook for Copilot CLI |
| `.gitignore` | Modified | Added `.bg-shell/` and `.claude/settings.local.json` entries |
| `docs/superpowers/specs/2026-05-05-claude-code-passthrough-spec.md` | New | 321-line implementation spec for OAuth passthrough relay |

The `.gitignore` additions (`.bg-shell/`, `.claude/settings.local.json`) are correct and appropriate.  
The copilot instructions content matches the canonical gstack+superpowers workflow from CLAUDE.md.

---

## Health Score Breakdown

| Category | Score | Notes |
|----------|-------|-------|
| Console | 100 | N/A (no browser) |
| Links | 100 | N/A (no browser) |
| Visual | 100 | N/A (no browser) |
| Functional | 60 | ISSUE-001 blocks contributor workflow; ISSUE-003 hides P1 items |
| UX | 80 | ISSUE-002 creates implementor confusion on file targets |
| Performance | 90 | ISSUE-005 low risk |
| Content | 70 | ISSUE-004 stale example data; ISSUE-002 wrong filenames |
| Accessibility | 100 | N/A |

**Final: 72/100**

---

## Baseline

```json
{
  "date": "2026-05-05",
  "branch": "fork/cc-passthrough",
  "healthScore": 72,
  "issues": [
    {"id": "ISSUE-001", "title": "Cargo.lock in .gitignore conflicts with tracked state", "severity": "high", "category": "configuration"},
    {"id": "ISSUE-002", "title": "Spec file map uses wrong/non-existent filenames", "severity": "medium", "category": "documentation"},
    {"id": "ISSUE-003", "title": "TODOS.md is untracked on this branch", "severity": "medium", "category": "content"},
    {"id": "ISSUE-004", "title": "Spec uses stale model name claude-4-5-sonnet", "severity": "low", "category": "documentation"},
    {"id": "ISSUE-005", "title": "rtk hook timeout of 5s may be insufficient", "severity": "low", "category": "configuration"}
  ]
}
```
