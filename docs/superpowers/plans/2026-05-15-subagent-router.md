# Subagent Router Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a configurable `router.subagent` model field that routes subagent requests (detected via `<CCM-SUBAGENT-MODEL>` tag) to a user-configured model, overriding the model name embedded in the tag.

**Architecture:** Three layers: (1) `RouterConfig` gains `subagent: Option<String>`; (2) `extract_subagent_model` becomes `handle_subagent_tag` — if config is set it routes there, otherwise it mutates `request.model` from the tag and falls through; (3) Admin UI adds a Subagent Model dropdown to the Router tab matching the existing think/background/websearch pattern.

**Tech Stack:** Rust (tokio/axum backend), plain HTML/JS admin UI (no framework), TOML config.

---

## File Map

| File | Change |
|------|--------|
| `src/cli/mod.rs` | Add `subagent: Option<String>` to `RouterConfig` (line 83–95) |
| `src/router/mod.rs` | Rename `extract_subagent_model` → `handle_subagent_tag`, update logic; restructure `route()` to capture `original_model` after subagent handling; add 3 tests; fix `create_test_config` |
| `src/server/admin.html` | Add Subagent Model card to Router tab; add `current-subagent` to status overview; update `loadRouterTab`, `renderOverview`, save handler |

---

## Task 1: Add `subagent` field to `RouterConfig`

**Files:**
- Modify: `src/cli/mod.rs:83–95`
- Modify: `src/router/mod.rs:236–250` (fix `create_test_config` to compile)

- [ ] **Step 1: Add `subagent` field to `RouterConfig`**

  In `src/cli/mod.rs`, replace the `RouterConfig` struct (lines 82–95):

  ```rust
  /// Router configuration
  #[derive(Debug, Clone, Deserialize, Serialize)]
  pub struct RouterConfig {
      pub default: String,
      pub subagent: Option<String>,
      pub background: Option<String>,
      pub think: Option<String>,
      pub websearch: Option<String>,
      /// Regex pattern for auto-mapping models (e.g., "^claude-").
      /// If empty/null, defaults to Claude models only.
      pub auto_map_regex: Option<String>,
      /// Regex pattern for detecting background tasks (e.g., "(?i)claude.*haiku").
      /// If empty/null, defaults to claude-haiku pattern.
      pub background_regex: Option<String>,
  }
  ```

- [ ] **Step 2: Fix `create_test_config` in router tests**

  In `src/router/mod.rs`, the test helper at lines 236–250 constructs `RouterConfig` by field — it will not compile with the new field. Update it to include `subagent: None`:

  ```rust
  fn create_test_config() -> AppConfig {
      AppConfig {
          server: ServerConfig::default(),
          router: RouterConfig {
              default: "default.model".to_string(),
              subagent: None,
              background: Some("background.model".to_string()),
              think: Some("think.model".to_string()),
              websearch: Some("websearch.model".to_string()),
              auto_map_regex: None,
              background_regex: None,
          },
          providers: vec![],
          models: vec![],
      }
  }
  ```

- [ ] **Step 3: Verify it compiles**

  ```bash
  cargo check
  ```

  Expected: no errors.

- [ ] **Step 4: Commit**

  ```bash
  git add src/cli/mod.rs src/router/mod.rs
  git commit -m "feat: add subagent field to RouterConfig"
  ```

---

## Task 2: Implement `handle_subagent_tag` and update routing logic

**Files:**
- Modify: `src/router/mod.rs`

### 2a: Write the three failing tests first

- [ ] **Step 1: Add imports and three failing tests**

  In `src/router/mod.rs`, add `SystemBlock, SystemPrompt` to the test imports at line 233:

  ```rust
  use crate::models::{Message, MessageContent, SystemBlock, SystemPrompt, ThinkingConfig};
  ```

  Then append the three tests after the last test (`test_no_auto_map_non_matching`, ending around line 418):

  ```rust
  #[test]
  fn test_subagent_config_overrides_tag() {
      let mut config = create_test_config();
      config.router.subagent = Some("config-model".to_string());
      let router = Router::new(config);

      let mut request = create_simple_request("Do a subagent task");
      request.system = Some(SystemPrompt::Blocks(vec![
          SystemBlock {
              r#type: "text".to_string(),
              text: "System prompt".to_string(),
              cache_control: None,
          },
          SystemBlock {
              r#type: "text".to_string(),
              text: "<CCM-SUBAGENT-MODEL>model-from-tag</CCM-SUBAGENT-MODEL>".to_string(),
              cache_control: None,
          },
      ]));

      let decision = router.route(&mut request).unwrap();
      assert_eq!(decision.model_name, "config-model");
      // Tag must be removed from text
      if let Some(SystemPrompt::Blocks(blocks)) = &request.system {
          assert!(!blocks[1].text.contains("<CCM-SUBAGENT-MODEL>"));
      }
  }

  #[test]
  fn test_subagent_fallthrough_no_config() {
      // create_test_config has background = Some("background.model") and
      // default background_regex matches "(?i)claude.*haiku"
      let config = create_test_config();
      let router = Router::new(config);

      let mut request = create_simple_request("Do a subagent task");
      // Tag carries a haiku model name
      request.system = Some(SystemPrompt::Blocks(vec![
          SystemBlock {
              r#type: "text".to_string(),
              text: "System prompt".to_string(),
              cache_control: None,
          },
          SystemBlock {
              r#type: "text".to_string(),
              text: "<CCM-SUBAGENT-MODEL>claude-3-5-haiku-20241022</CCM-SUBAGENT-MODEL>"
                  .to_string(),
              cache_control: None,
          },
      ]));

      let decision = router.route(&mut request).unwrap();
      // handle_subagent_tag mutates request.model → "claude-3-5-haiku-20241022"
      // background_regex matches → routes to background model
      assert_eq!(decision.route_type, RouteType::Background);
      assert_eq!(decision.model_name, "background.model");
  }

  #[test]
  fn test_no_tag_no_subagent_routing() {
      let mut config = create_test_config();
      config.router.subagent = Some("config-model".to_string());
      let router = Router::new(config);

      // No system prompt — no tag
      let mut request = create_simple_request("Regular request");
      // create_simple_request uses "claude-opus-4" which matches auto_map_regex (^claude-)
      let decision = router.route(&mut request).unwrap();
      assert_eq!(decision.route_type, RouteType::Default);
      assert_eq!(decision.model_name, "default.model"); // auto-mapped
  }
  ```

- [ ] **Step 2: Run tests to confirm they fail**

  ```bash
  cargo test -p claude-code-mux router::tests::test_subagent 2>&1 | tail -20
  ```

  Expected: FAILED (functions not defined yet).

### 2b: Implement the logic

- [ ] **Step 3: Replace `extract_subagent_model` with `handle_subagent_tag`**

  In `src/router/mod.rs`, replace the entire `extract_subagent_model` method (lines 192–227) with:

  ```rust
  /// Handle the CCM-SUBAGENT-MODEL tag in the system prompt.
  ///
  /// Returns Some(model) if router.subagent is configured (caller routes there).
  /// Returns None if not configured — but mutates request.model with the tag's value
  /// so routing falls through with the updated model name.
  /// Returns None with no side effects if no tag is found.
  fn handle_subagent_tag(&self, request: &mut AnthropicRequest) -> Option<String> {
      let system = request.system.as_mut()?;

      if let SystemPrompt::Blocks(blocks) = system {
          if blocks.len() < 2 {
              return None;
          }

          let second_block = &mut blocks[1];
          if !second_block.text.contains("<CCM-SUBAGENT-MODEL>") {
              return None;
          }

          let re = Regex::new(r"<CCM-SUBAGENT-MODEL>(.*?)</CCM-SUBAGENT-MODEL>")
              .expect("Invalid regex pattern");

          if let Some(captures) = re.captures(&second_block.text) {
              if let Some(model_match) = captures.get(1) {
                  let tag_model = model_match.as_str().to_string();
                  second_block.text = re.replace_all(&second_block.text, "").to_string();

                  // Config takes priority over the model name in the tag
                  if let Some(ref config_model) = self.config.router.subagent {
                      return Some(config_model.clone());
                  }

                  // No config: override request.model so routing continues with tag's value
                  request.model = tag_model;
                  return None;
              }
          }
      }

      None
  }
  ```

- [ ] **Step 4: Update `route()` to call `handle_subagent_tag` and reposition `original_model`**

  Replace the entire `route` method (lines 88–156) with:

  ```rust
  /// Route an incoming request to the appropriate model
  /// Priority: websearch > subagent > think > background > auto-map > default
  pub fn route(&self, request: &mut AnthropicRequest) -> Result<RouteDecision> {
      // 1. WebSearch (HIGHEST PRIORITY - tool-based detection)
      if let Some(ref websearch_model) = self.config.router.websearch {
          if self.has_web_search_tool(request) {
              info!("🔍 Routing to websearch model (web_search tool detected)");
              return Ok(RouteDecision {
                  model_name: websearch_model.clone(),
                  route_type: RouteType::WebSearch,
              });
          }
      }

      // 2. Subagent Model (system prompt tag)
      if let Some(model) = self.handle_subagent_tag(request) {
          info!(
              "🤖 Routing to subagent model (CCM-SUBAGENT-MODEL tag, config override): {}",
              model
          );
          return Ok(RouteDecision {
              model_name: model,
              route_type: RouteType::Default,
          });
      }

      // Capture model name after subagent tag may have mutated it.
      // Background detection uses this so the tag's model name is respected.
      let original_model = request.model.clone();

      // 3. Think mode (Plan Mode / Reasoning)
      if let Some(ref think_model) = self.config.router.think {
          if self.is_plan_mode(request) {
              info!("🧠 Routing to think model (Plan Mode detected)");
              return Ok(RouteDecision {
                  model_name: think_model.clone(),
                  route_type: RouteType::Think,
              });
          }
      }

      // 4. Background tasks (check against model name before auto-mapping)
      if let Some(ref background_model) = self.config.router.background {
          if self.is_background_task(&original_model) {
              debug!("🔄 Routing to background model");
              return Ok(RouteDecision {
                  model_name: background_model.clone(),
                  route_type: RouteType::Background,
              });
          }
      }

      // 5. Auto-mapping (model name transformation FIRST)
      if let Some(ref regex) = self.auto_map_regex {
          if regex.is_match(&request.model) {
              let old = request.model.clone();
              request.model = self.config.router.default.clone();
              debug!("🔀 Auto-mapped model '{}' → '{}'", old, request.model);
          }
      }

      // 6. Default fallback
      debug!("✅ Using model: {}", request.model);
      Ok(RouteDecision {
          model_name: request.model.clone(),
          route_type: RouteType::Default,
      })
  }
  ```

- [ ] **Step 5: Run all router tests**

  ```bash
  cargo test -p claude-code-mux router 2>&1 | tail -30
  ```

  Expected: all tests pass, including the three new ones.

- [ ] **Step 6: Commit**

  ```bash
  git add src/router/mod.rs
  git commit -m "feat: implement handle_subagent_tag with config-first routing"
  ```

---

## Task 3: Admin UI — Subagent Model dropdown

**Files:**
- Modify: `src/server/admin.html`

The admin HTML has four touch points. Make all four changes before verifying.

- [ ] **Step 1: Add `current-subagent` to the status overview card**

  The overview card ends at line ~324 with the WebSearch row having no `border-b`. Add `border-b` to that row and append a Subagent row after it.

  Find this block (lines 315–324):
  ```html
                              <div class="flex justify-between items-center py-3">
                                  <span class="text-gray-600"
                                      >WebSearch Model</span
                                  >
                                  <span
                                      class="font-semibold"
                                      id="current-websearch"
                                      >-</span
                                  >
                              </div>
  ```

  Replace with:
  ```html
                              <div class="flex justify-between items-center py-3 border-b">
                                  <span class="text-gray-600"
                                      >WebSearch Model</span
                                  >
                                  <span
                                      class="font-semibold"
                                      id="current-websearch"
                                      >-</span
                                  >
                              </div>
                              <div class="flex justify-between items-center py-3">
                                  <span class="text-gray-600"
                                      >Subagent Model</span
                                  >
                                  <span
                                      class="font-semibold"
                                      id="current-subagent"
                                      >-</span
                                  >
                              </div>
  ```

- [ ] **Step 2: Add Subagent Model card to the Router tab form**

  The Router tab form ends at line ~1739 (`</form>`). The websearch card occupies lines 1728–1738. Insert the subagent card between the websearch card and `</form>`:

  Find this exact block (lines 1728–1739):
  ```html
                          <div class="card">
                              <h2 class="text-xl font-bold mb-6">
                                  WebSearch Model
                              </h2>
                              <p class="text-gray-600 mb-6">
                                  Model used when web search is needed
                              </p>
                              <select name="websearch_model" class="input-field">
                                  <option value="">Not configured</option>
                              </select>
                          </div>
                      </form>
  ```

  Replace with:
  ```html
                          <div class="card">
                              <h2 class="text-xl font-bold mb-6">
                                  WebSearch Model
                              </h2>
                              <p class="text-gray-600 mb-6">
                                  Model used when web search is needed
                              </p>
                              <select name="websearch_model" class="input-field">
                                  <option value="">Not configured</option>
                              </select>
                          </div>

                          <div class="card">
                              <h2 class="text-xl font-bold mb-6">
                                  Subagent Model
                              </h2>
                              <p class="text-gray-600 mb-6">
                                  Model used for subagent requests (overrides CCM-SUBAGENT-MODEL tag)
                              </p>
                              <select name="subagent_model" class="input-field">
                                  <option value="">Not configured</option>
                              </select>
                          </div>
                      </form>
  ```

  Note: `populateModelSelects` uses `select[name$="_model"]` so the new select is automatically populated with models — no other change needed there.

- [ ] **Step 3: Update `loadRouterTab` to populate the subagent select**

  `loadRouterTab` runs from lines 2354–2380. After line 2379 (`websearchSelect.value = ...`), before the closing `}` at line 2380, insert:

  Find (lines 2369–2380):
  ```javascript
                          const websearchSelect = document.querySelector(
                              '[name="websearch_model"]',
                          );

                          if (defaultSelect)
                              defaultSelect.value = config.router.default || "";
                          if (thinkSelect) thinkSelect.value = config.router.think || "";
                          if (backgroundSelect)
                              backgroundSelect.value = config.router.background || "";
                          if (websearchSelect)
                              websearchSelect.value = config.router.websearch || "";
                      }
  ```

  Replace with:
  ```javascript
                          const websearchSelect = document.querySelector(
                              '[name="websearch_model"]',
                          );
                          const subagentSelect = document.querySelector(
                              '[name="subagent_model"]',
                          );

                          if (defaultSelect)
                              defaultSelect.value = config.router.default || "";
                          if (thinkSelect) thinkSelect.value = config.router.think || "";
                          if (backgroundSelect)
                              backgroundSelect.value = config.router.background || "";
                          if (websearchSelect)
                              websearchSelect.value = config.router.websearch || "";
                          if (subagentSelect)
                              subagentSelect.value = config.router.subagent || "";
                      }
  ```

- [ ] **Step 4: Update `renderOverview` to display `current-subagent`**

  `renderOverview` updates the status card (lines 3184–3208). After line 3197 (`config.router.websearch || "Not configured"`), add:

  Find (lines 3196–3198):
  ```javascript
                          document.getElementById("current-websearch").textContent =
                              config.router.websearch || "Not configured";

                          // Update server info
  ```

  Replace with:
  ```javascript
                          document.getElementById("current-websearch").textContent =
                              config.router.websearch || "Not configured";
                          document.getElementById("current-subagent").textContent =
                              config.router.subagent || "Not configured";

                          // Update server info
  ```

- [ ] **Step 5: Update the router form save handler**

  The debounced save handler runs around lines 3311–3359. Add `subagentModel` extraction and state update alongside the existing three optional models.

  Find (lines 3314–3350):
  ```javascript
                              const thinkModel = formData.get("think_model");
                              const backgroundModel =
                                  formData.get("background_model");
                              const websearchModel =
                                  formData.get("websearch_model");

                              console.log("FormData values:", {
                                  default: defaultModel,
                                  think: thinkModel,
                                  background: backgroundModel,
                                  websearch: websearchModel,
                              });

                              if (!defaultModel) return; // Skip if required field is empty

                              // Update router config in state
                              appState.config.router.default = defaultModel;

                              if (thinkModel) {
                                  appState.config.router.think = thinkModel;
                              } else {
                                  delete appState.config.router.think;
                              }

                              if (backgroundModel) {
                                  appState.config.router.background =
                                      backgroundModel;
                              } else {
                                  delete appState.config.router.background;
                              }

                              if (websearchModel) {
                                  appState.config.router.websearch =
                                      websearchModel;
                              } else {
                                  delete appState.config.router.websearch;
                              }
  ```

  Replace with:
  ```javascript
                              const thinkModel = formData.get("think_model");
                              const backgroundModel =
                                  formData.get("background_model");
                              const websearchModel =
                                  formData.get("websearch_model");
                              const subagentModel =
                                  formData.get("subagent_model");

                              console.log("FormData values:", {
                                  default: defaultModel,
                                  think: thinkModel,
                                  background: backgroundModel,
                                  websearch: websearchModel,
                                  subagent: subagentModel,
                              });

                              if (!defaultModel) return; // Skip if required field is empty

                              // Update router config in state
                              appState.config.router.default = defaultModel;

                              if (thinkModel) {
                                  appState.config.router.think = thinkModel;
                              } else {
                                  delete appState.config.router.think;
                              }

                              if (backgroundModel) {
                                  appState.config.router.background =
                                      backgroundModel;
                              } else {
                                  delete appState.config.router.background;
                              }

                              if (websearchModel) {
                                  appState.config.router.websearch =
                                      websearchModel;
                              } else {
                                  delete appState.config.router.websearch;
                              }

                              if (subagentModel) {
                                  appState.config.router.subagent =
                                      subagentModel;
                              } else {
                                  delete appState.config.router.subagent;
                              }
  ```

- [ ] **Step 6: Run all tests to confirm backend still passes**

  ```bash
  cargo test 2>&1 | tail -20
  ```

  Expected: all tests pass.

- [ ] **Step 7: Commit**

  ```bash
  git add src/server/admin.html
  git commit -m "feat: add Subagent Model dropdown to admin Router tab"
  ```
