# Skip Auto-Map For Defined Models Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a request's `model` exactly matches a `config.models[].name`, skip the auto-map → `default` rewrite so the explicitly defined model passes through.

**Architecture:** A single guard in the router's step 5 (auto-map). Add a read-only `Router::is_defined_model` helper that checks `config.models` for an exact name match, and skip the rewrite when it returns true. No new types, no new failure modes.

**Tech Stack:** Rust, `cargo test`, `cargo clippy`. Existing `regex` + `tracing` already in `src/router/mod.rs`.

## Global Constraints

- Change is confined to `src/router/mod.rs`; do not touch steps 1–4 (websearch/subagent/think/background).
- Model match is an exact string equality on `name`, mirroring `src/server/mod.rs:1028` (`m.name == decision.model_name`).
- `ModelConfig` fields (from `src/cli/mod.rs:100-105`): `name: String`, `mappings: Vec<ModelMapping>`.

---

### Task 1: Skip auto-map for explicitly defined models

**Files:**
- Modify: `src/router/mod.rs:140-147` (step 5 auto-map block)
- Modify: `src/router/mod.rs` (add `is_defined_model` helper method inside `impl Router`)
- Test: `src/router/mod.rs` (`#[cfg(test)] mod tests`, around line 404-416)

**Interfaces:**
- Consumes: `self.config.models: Vec<ModelConfig>` where `ModelConfig { name: String, mappings: Vec<ModelMapping> }`; `self.auto_map_regex: Option<Regex>`; `request.model: String`.
- Produces: `fn is_defined_model(&self, model: &str) -> bool` — true iff some `config.models[].name == model`.

- [ ] **Step 1: Write the failing tests**

Add these two tests to the `tests` module in `src/router/mod.rs` (place them right after `test_no_auto_map_non_matching`, near line 416):

```rust
#[test]
fn test_defined_model_skips_auto_map() {
    // A claude-* model that IS defined in config.models must pass through,
    // not be rewritten to default.
    let mut config = create_test_config();
    config.models = vec![crate::cli::ModelConfig {
        name: "claude-haiku-4-5".to_string(),
        mappings: vec![],
    }];
    let router = Router::new(config);

    let mut request = create_simple_request("Hello");
    request.model = "claude-haiku-4-5".to_string();

    let decision = router.route(&mut request).unwrap();
    assert_eq!(decision.route_type, RouteType::Default);
    assert_eq!(decision.model_name, "claude-haiku-4-5"); // NOT remapped to default
}

#[test]
fn test_undefined_claude_model_still_auto_maps() {
    // Regression guard: a claude-* model NOT in config.models still maps to default.
    let mut config = create_test_config();
    config.models = vec![crate::cli::ModelConfig {
        name: "claude-haiku-4-5".to_string(),
        mappings: vec![],
    }];
    let router = Router::new(config);

    let mut request = create_simple_request("Hello");
    request.model = "claude-3-5-sonnet-20241022".to_string(); // not in config.models

    let decision = router.route(&mut request).unwrap();
    assert_eq!(decision.route_type, RouteType::Default);
    assert_eq!(decision.model_name, "default.model"); // still auto-mapped
}
```

Note: `test_defined_model_skips_auto_map` uses `claude-haiku-4-5`, which does NOT match the background regex `(?i)claude.*haiku`... actually it DOES match `claude.*haiku`. To keep this test isolated to step 5, disable background routing in the test by adding `config.router.background = None;` before constructing the router. Update the test body accordingly:

```rust
#[test]
fn test_defined_model_skips_auto_map() {
    let mut config = create_test_config();
    config.router.background = None; // isolate step 5 from background routing
    config.models = vec![crate::cli::ModelConfig {
        name: "claude-haiku-4-5".to_string(),
        mappings: vec![],
    }];
    let router = Router::new(config);

    let mut request = create_simple_request("Hello");
    request.model = "claude-haiku-4-5".to_string();

    let decision = router.route(&mut request).unwrap();
    assert_eq!(decision.route_type, RouteType::Default);
    assert_eq!(decision.model_name, "claude-haiku-4-5");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib router::tests::test_defined_model_skips_auto_map -- --nocapture`
Expected: FAIL — `assert_eq!` panics, left `"default.model"`, right `"claude-haiku-4-5"` (auto-map currently rewrites it).

`test_undefined_claude_model_still_auto_maps` will already PASS (it asserts current behavior) — that is expected; it is the regression guard.

- [ ] **Step 3: Add the `is_defined_model` helper**

Inside `impl Router` (e.g. right after the `route` method ends near line 155), add:

```rust
/// True if `model` is an explicitly defined model in config.models.
fn is_defined_model(&self, model: &str) -> bool {
    self.config.models.iter().any(|m| m.name == model)
}
```

- [ ] **Step 4: Guard the auto-map rewrite**

Replace the step 5 block at `src/router/mod.rs:140-147`:

```rust
        // 5. Auto-mapping (model name transformation FIRST)
        if let Some(ref regex) = self.auto_map_regex {
            if regex.is_match(&request.model) {
                let old = request.model.clone();
                request.model = self.config.router.default.clone();
                debug!("🔀 Auto-mapped model '{}' → '{}'", old, request.model);
            }
        }
```

with:

```rust
        // 5. Auto-mapping (model name transformation FIRST).
        // An explicitly defined model (config.models[].name) bypasses the
        // rewrite so it resolves to its own provider mappings.
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

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib router::tests -- --nocapture`
Expected: PASS — including both new tests and all pre-existing router tests (`test_auto_map_claude_models`, `test_no_auto_map_non_matching`, etc.).

- [ ] **Step 6: Lint**

Run: `cargo clippy --lib`
Expected: no new warnings from `src/router/mod.rs`.

- [ ] **Step 7: Commit**

```bash
git add src/router/mod.rs
git commit -m "feat(router): skip auto-map when model is explicitly defined in config

An explicitly defined config.models[].name now bypasses the
auto-map -> default rewrite (step 5), so it resolves to its own
provider mappings instead of being clobbered to the default model.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```
