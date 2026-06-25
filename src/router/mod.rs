use crate::cli::AppConfig;
use crate::models::{AnthropicRequest, RouteDecision, RouteType, SystemPrompt};
use anyhow::Result;
use regex::Regex;
use tracing::{debug, info};

/// Router for intelligently selecting models based on request characteristics
#[derive(Clone)]
pub struct Router {
    config: AppConfig,
    auto_map_regex: Option<Regex>,
    background_regex: Option<Regex>,
}

impl Router {
    /// Create a new router with configuration
    pub fn new(config: AppConfig) -> Self {
        // Compile auto-map regex
        let auto_map_regex = config
            .router
            .auto_map_regex
            .as_ref()
            .and_then(|pattern| {
                if pattern.is_empty() {
                    // Empty string: use default Claude pattern
                    Some(Regex::new(r"^claude-").expect("Invalid default Claude regex"))
                } else {
                    // Custom pattern provided
                    match Regex::new(pattern) {
                        Ok(regex) => Some(regex),
                        Err(e) => {
                            eprintln!(
                                "Warning: Invalid auto_map_regex pattern '{}': {}",
                                pattern, e
                            );
                            eprintln!("Falling back to default Claude pattern");
                            Some(Regex::new(r"^claude-").expect("Invalid default Claude regex"))
                        }
                    }
                }
            })
            .or_else(|| {
                // None: use default Claude pattern for backward compatibility
                Some(Regex::new(r"^claude-").expect("Invalid default Claude regex"))
            });

        // Compile background-task regex
        let background_regex = config
            .router
            .background_regex
            .as_ref()
            .and_then(|pattern| {
                if pattern.is_empty() {
                    // Empty string: use default claude-haiku pattern
                    Some(
                        Regex::new(r"(?i)claude.*haiku").expect("Invalid default background regex"),
                    )
                } else {
                    // Custom pattern provided
                    match Regex::new(pattern) {
                        Ok(regex) => Some(regex),
                        Err(e) => {
                            eprintln!(
                                "Warning: Invalid background_regex pattern '{}': {}",
                                pattern, e
                            );
                            eprintln!("Falling back to default claude-haiku pattern");
                            Some(
                                Regex::new(r"(?i)claude.*haiku")
                                    .expect("Invalid default background regex"),
                            )
                        }
                    }
                }
            })
            .or_else(|| {
                // None: use default claude-haiku pattern for backward compatibility
                Some(Regex::new(r"(?i)claude.*haiku").expect("Invalid default background regex"))
            });

        Self {
            config,
            auto_map_regex,
            background_regex,
        }
    }

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

        // 5. Auto-mapping (model name transformation FIRST).
        // An explicitly defined model (config.models[].name) bypasses the
        // rewrite so it resolves to its own provider mappings.
        if let Some(ref regex) = self.auto_map_regex {
            if regex.is_match(&request.model) {
                if self.is_defined_model(&request.model) {
                    debug!(
                        "⏭️  Skipping auto-map: '{}' is an explicitly defined model",
                        request.model
                    );
                } else {
                    let old = request.model.clone();
                    request.model = self.config.router.default.clone();
                    debug!("🔀 Auto-mapped model '{}' → '{}'", old, request.model);
                }
            }
        }

        // 6. Default fallback
        debug!("✅ Using model: {}", request.model);
        Ok(RouteDecision {
            model_name: request.model.clone(),
            route_type: RouteType::Default,
        })
    }

    /// Check if request has web_search tool (tool-based detection)
    /// Following claude-code-router pattern: checks if tools array contains web_search type
    fn has_web_search_tool(&self, request: &AnthropicRequest) -> bool {
        if let Some(ref tools) = request.tools {
            tools.iter().any(|tool| {
                tool.r#type
                    .as_ref()
                    .map(|t| t.starts_with("web_search"))
                    .unwrap_or(false)
            })
        } else {
            false
        }
    }

    /// Check if request is Plan Mode by detecting thinking field
    fn is_plan_mode(&self, request: &AnthropicRequest) -> bool {
        request
            .thinking
            .as_ref()
            .map(|t| t.r#type == "enabled")
            .unwrap_or(false)
    }

    /// True if `model` is an explicitly defined model in config.models.
    fn is_defined_model(&self, model: &str) -> bool {
        self.config.models.iter().any(|m| m.name == model)
    }

    /// Detect background tasks using regex pattern
    /// Uses background_regex from config (defaults to claude-haiku pattern)
    fn is_background_task(&self, model: &str) -> bool {
        if let Some(ref regex) = self.background_regex {
            regex.is_match(model)
        } else {
            false
        }
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{RouterConfig, ServerConfig};
    use crate::models::{Message, MessageContent, SystemBlock, SystemPrompt, ThinkingConfig};

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

    fn create_simple_request(text: &str) -> AnthropicRequest {
        AnthropicRequest {
            model: "claude-opus-4".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text(text.to_string()),
            }],
            max_tokens: 1024,
            thinking: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            metadata: None,
            system: None,
            tools: None,
            passthrough_auth: None,
            anthropic_beta_header: None,
        }
    }

    #[test]
    fn test_plan_mode_detection() {
        let config = create_test_config();
        let router = Router::new(config);

        let mut request = create_simple_request("Explain quantum computing");
        request.thinking = Some(ThinkingConfig {
            r#type: "enabled".to_string(),
            budget_tokens: Some(10_000),
        });

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Think);
        assert_eq!(decision.model_name, "think.model");
    }

    #[test]
    fn test_background_task_detection() {
        let config = create_test_config();
        let router = Router::new(config);

        // Create request with haiku model
        let mut request = create_simple_request("Hello");
        request.model = "claude-3-5-haiku-20241022".to_string();

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Background);
        assert_eq!(decision.model_name, "background.model");
    }

    #[test]
    fn test_default_routing() {
        let mut config = create_test_config();
        config.router.background = None; // Disable background routing
        let router = Router::new(config);

        let mut request = create_simple_request("Write a function to sort an array");

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Default);
        assert_eq!(decision.model_name, "default.model");
    }

    #[test]
    fn test_routing_priority() {
        let config = create_test_config();
        let router = Router::new(config);

        // Think has highest priority
        let mut request = create_simple_request("Explain complex topic");
        request.thinking = Some(ThinkingConfig {
            r#type: "enabled".to_string(),
            budget_tokens: Some(10_000),
        });

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Think); // Think wins
    }

    #[test]
    fn test_websearch_tool_detection() {
        let config = create_test_config();
        let router = Router::new(config);

        let mut request = create_simple_request("Search the web for latest news");
        request.tools = Some(vec![crate::models::Tool {
            r#type: Some("web_search_2025_04".to_string()),
            name: Some("web_search".to_string()),
            description: Some("Search the web".to_string()),
            input_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {}
            })),
        }]);

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::WebSearch);
        assert_eq!(decision.model_name, "websearch.model");
    }

    #[test]
    fn test_websearch_has_highest_priority() {
        let config = create_test_config();
        let router = Router::new(config);

        // WebSearch should win even if thinking is enabled
        let mut request = create_simple_request("Search and explain");
        request.thinking = Some(ThinkingConfig {
            r#type: "enabled".to_string(),
            budget_tokens: Some(10_000),
        });
        request.tools = Some(vec![crate::models::Tool {
            r#type: Some("web_search".to_string()),
            name: None,
            description: None,
            input_schema: None,
        }]);

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::WebSearch); // WebSearch wins over Think
        assert_eq!(decision.model_name, "websearch.model");
    }

    #[test]
    fn test_auto_map_claude_models() {
        let config = create_test_config();
        let router = Router::new(config);

        // Test Claude model auto-mapping (default pattern)
        let mut request = create_simple_request("Hello");
        request.model = "claude-3-5-sonnet-20241022".to_string();

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Default);
        assert_eq!(decision.model_name, "default.model"); // Auto-mapped to default
    }

    #[test]
    fn test_auto_map_custom_regex() {
        let mut config = create_test_config();
        config.router.auto_map_regex = Some("^(claude-|gpt-)".to_string());
        let router = Router::new(config);

        // Test GPT model auto-mapping with custom regex
        let mut request = create_simple_request("Hello");
        request.model = "gpt-4".to_string();

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Default);
        assert_eq!(decision.model_name, "default.model"); // Auto-mapped to default
    }

    #[test]
    fn test_no_auto_map_non_matching() {
        let config = create_test_config();
        let router = Router::new(config);

        // Test non-Claude model (should not auto-map, use model name as-is)
        let mut request = create_simple_request("Hello");
        request.model = "glm-4.6".to_string();

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Default);
        assert_eq!(decision.model_name, "glm-4.6"); // Uses original model name (no auto-mapping)
    }

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
}
