# Skip Auto-Map When Model Is Explicitly Defined in Config

**Date:** 2026-06-25
**Status:** Approved (design)
**Component:** `src/router/mod.rs`

## Problem

The router's auto-map step (step 5) rewrites any request whose `model` matches
`auto_map_regex` (default `^claude-`) to `config.router.default`:

```rust
// src/router/mod.rs:140-147
if let Some(ref regex) = self.auto_map_regex {
    if regex.is_match(&request.model) {
        let old = request.model.clone();
        request.model = self.config.router.default.clone();
        debug!("🔀 Auto-mapped model '{}' → '{}'", old, request.model);
    }
}
```

This clobbers models that are **explicitly defined** in `config.models`. For
example, if a client requests `claude-haiku-4-5` and that name exists as a
`config.models[].name`, the `^claude-` regex still rewrites it to `default`,
discarding the user's explicit model definition. The routed model name is later
resolved against `config.models[].name` by exact match
(`src/server/mod.rs:1028`), so a defined model should reach its own provider
mappings rather than being absorbed into `default`.

## Goal

When the requested model exactly matches a `config.models[].name`, skip the
auto-map → `default` rewrite and pass the model through unchanged.

## Scope

- **In scope:** Step 5 (auto-map) only.
- **Out of scope:** Steps 1–4 (websearch, subagent, think, background) remain
  unchanged. They are unaffected by this design even when the requested model is
  an explicitly defined config model.

## Design

### Helper

Add a small read-only helper on `Router`:

```rust
/// True if `model` is an explicitly defined model in config.models.
fn is_defined_model(&self, model: &str) -> bool {
    self.config.models.iter().any(|m| m.name == model)
}
```

This mirrors the exact-match resolution already done at
`src/server/mod.rs:1028` (`m.name == decision.model_name`).

### Guarded auto-map

```rust
// 5. Auto-mapping (model name transformation FIRST)
if let Some(ref regex) = self.auto_map_regex {
    if regex.is_match(&request.model) && !self.is_defined_model(&request.model) {
        let old = request.model.clone();
        request.model = self.config.router.default.clone();
        debug!("🔀 Auto-mapped model '{}' → '{}'", old, request.model);
    } else if self.is_defined_model(&request.model) {
        debug!(
            "⏭️  Skipping auto-map: '{}' is an explicitly defined model",
            request.model
        );
    }
}
```

## Behavior

| Requested model | In `config.models`? | Matches `^claude-`? | Result |
| --- | --- | --- | --- |
| `claude-haiku-4-5` | No | Yes | Mapped to `default` (unchanged behavior) |
| `claude-haiku-4-5` | Yes | Yes | Passes through; resolves to its own mappings |
| `gpt-4o` | No | No | Passes through (unchanged behavior) |

## Data Flow

Unchanged. Step 5 still returns
`RouteDecision { model_name, route_type: RouteType::Default }`. The only
difference is that, for a defined model, `model_name` retains the
originally-requested name instead of `config.router.default`.

## Error Handling

None required. The change is a pure read of already-loaded config and introduces
no new failure modes.

## Testing

Add unit tests to the existing `router::tests` module:

1. **Defined model is not remapped:** a `claude-`-prefixed model present in
   `config.models` routes to itself, not `default`.
2. **Undefined model still maps (regression guard):** a `claude-`-prefixed model
   absent from `config.models` still maps to `default`.

Verification: `cargo test` and `cargo clippy`.
