# Subagent Router — Design Spec

**Date:** 2026-05-15  
**Status:** Approved

---

## Goal

Add a configurable subagent routing slot to the mux. When a request is detected as a subagent call (via the `<CCM-SUBAGENT-MODEL>` tag in the system prompt), route it to a model set in the admin UI / config file — instead of using the model name embedded inside the tag.

---

## Background

The mux already has `extract_subagent_model` in `src/router/mod.rs`, which reads `<CCM-SUBAGENT-MODEL>model-name</CCM-SUBAGENT-MODEL>` from `system[1].text`. Currently the tag does double duty: it **detects** that a request is a subagent call and **carries the target model name**. There is no way to override the target model from the admin UI.

---

## Architecture

Three layers change:

### 1. Config — `src/cli/mod.rs`

Add `subagent: Option<String>` to `RouterConfig`:

```toml
[router]
default  = "..."
subagent = "some-model"   # new, optional
think    = "..."
background = "..."
websearch  = "..."
```

`Option<String>` means the field is fully optional — existing configs without it are valid.

### 2. Router — `src/router/mod.rs`

Routing priority (unchanged positions, new slot added):

```
websearch > subagent (config) > think > background > auto-map > default
```

**Refactor `extract_subagent_model` → `handle_subagent_tag`:**

- **Tag absent** → return `None`, no side effects.
- **Tag present, `router.subagent` configured** → remove tag from text, return `Some(config_model)`. Caller routes there and returns early.
- **Tag present, `router.subagent` not configured** → remove tag from text, override `request.model` with the model name from inside the tag, return `None`. Routing falls through to think/background/auto-map/default with the updated model name.

The `route` method calls this before the think check, using the returned value to short-circuit:

```rust
if let Some(model) = self.handle_subagent_tag(request) {
    return Ok(RouteDecision { model_name: model, route_type: RouteType::Default });
}
```

### 3. Admin UI — `src/server/admin.html`

**Router tab** — add a "Subagent Model" `<select>` dropdown, visually grouped with Think/Background/WebSearch. Follows the identical load/save pattern:
- Populated from `get_models_config` response on tab load.
- Reads `config.router.subagent` on load.
- Writes `appState.config.router.subagent` on change.
- Persisted only when user clicks "저장" / "저장 & 재시작".

**Status overview card** — add `current-subagent` span alongside `current-think`, `current-background`, `current-websearch`.

---

## Data Flow

```
Incoming request
    │
    ▼
handle_subagent_tag(request)
    ├─ No tag → None (no side effect)
    ├─ Tag + config.subagent set → Some(config_model)  ──► route & return
    └─ Tag + no config → override request.model; None  ──► fall through
                                                              │
                                                              ▼
                                                      think / background /
                                                      auto-map / default
```

---

## Error Handling

No new error cases. `subagent: Option<String>` — absent field is valid TOML. Tag parsing reuses the existing compiled regex pattern (no changes to error handling there).

---

## Tests

Add in `src/router/mod.rs`:

| Test | Scenario | Expected |
|------|----------|----------|
| `test_subagent_config_overrides_tag` | Tag present (`model-from-tag`), `router.subagent = "config-model"` | Routes to `"config-model"` |
| `test_subagent_fallthrough_no_config` | Tag present (`haiku-model`), no `router.subagent`, background routing enabled | `request.model` mutated to `"haiku-model"`, then background route applies |
| `test_no_tag_no_subagent_routing` | No tag, `router.subagent` set | Subagent logic skipped; normal routing proceeds |

Existing tests are unaffected — the rename from `extract_subagent_model` to `handle_subagent_tag` is internal (private method).

---

## Out of Scope

- Custom subagent detection logic (e.g., regex-based detection independent of the tag).
- Per-request subagent model overrides via HTTP headers.
- UI for injecting the `<CCM-SUBAGENT-MODEL>` tag from the admin panel.
