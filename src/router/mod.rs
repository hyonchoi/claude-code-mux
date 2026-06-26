use crate::cli::AppConfig;
use crate::models::{AnthropicRequest, RouteDecision, RouteType, SystemPrompt};
use anyhow::Result;
use regex::Regex;
use tracing::{debug, info};

static SUBAGENT_TAG_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

const SUBAGENT_FLAG: &str = "cc_is_subagent=true";

fn get_subagent_tag_re() -> &'static Regex {
    SUBAGENT_TAG_RE.get_or_init(|| {
        Regex::new(r"<CCM-SUBAGENT-MODEL>[^<]*</CCM-SUBAGENT-MODEL>").expect("Invalid regex")
    })
}

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

        // 2. Subagent (billing header flag)
        if let Some(ref subagent_model) = self.config.router.subagent {
            if self.is_subagent_request(request) {
                // Strip any CCM-SUBAGENT-MODEL tags as a courtesy (backward compat)
                self.strip_subagent_tags(request);
                info!(
                    "🤖 Routing to subagent model (cc_is_subagent detected, config override): {}",
                    subagent_model
                );
                return Ok(RouteDecision {
                    model_name: subagent_model.clone(),
                    route_type: RouteType::Subagent,
                });
            }
        }

        // Also strip any legacy CCM-SUBAGENT-MODEL tags even if no subagent config
        self.strip_subagent_tags(request);

        // Capture model name for background task detection.
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

    /// Detect subagent requests by checking for cc_is_subagent=true in any
    /// system prompt block (typically the billing header block).
    fn is_subagent_request(&self, request: &AnthropicRequest) -> bool {
        match &request.system {
            Some(SystemPrompt::Blocks(blocks)) => {
                for block in blocks.iter() {
                    if block.text.contains(SUBAGENT_FLAG) {
                        return true;
                    }
                }
                false
            }
            // Text variant: scan for the flag as a fallback
            Some(SystemPrompt::Text(text)) => text.contains(SUBAGENT_FLAG),
            None => false,
        }
    }

    /// Strip legacy CCM-SUBAGENT-MODEL tags from all system blocks (backward compat).
    fn strip_subagent_tags(&self, request: &mut AnthropicRequest) {
        if let Some(SystemPrompt::Blocks(blocks)) = &mut request.system {
            for block in blocks.iter_mut() {
                if block.text.contains("<CCM-SUBAGENT-MODEL>") {
                    block.text = get_subagent_tag_re()
                        .replace_all(&block.text, "")
                        .to_string();
                }
            }
        } else if let Some(SystemPrompt::Text(text)) = &mut request.system {
            if text.contains("<CCM-SUBAGENT-MODEL>") {
                *text = get_subagent_tag_re().replace_all(text, "").to_string();
            }
        }
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
    fn test_subagent_billing_header_detection() {
        let mut config = create_test_config();
        config.router.subagent = Some("config-model".to_string());
        let router = Router::new(config);

        let mut request = create_simple_request("Do a subagent task");
        request.system = Some(SystemPrompt::Blocks(vec![
            SystemBlock {
                r#type: "text".to_string(),
                text: "x-anthropic-billing-header: cc_version=2.1.193.942; cc_entrypoint=cli; cc_is_subagent=true;".to_string(),
                cache_control: None,
            },
            SystemBlock {
                r#type: "text".to_string(),
                text: "You are Claude Code, Anthropic's official CLI for Claude.".to_string(),
                cache_control: None,
            },
            SystemBlock {
                r#type: "text".to_string(),
                text: "You are an agent for Claude Code...".to_string(),
                cache_control: None,
            },
        ]));

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Subagent);
        assert_eq!(decision.model_name, "config-model");
    }

    #[test]
    fn test_subagent_no_config_not_routed() {
        // When router.subagent is not configured, subagent detection is skipped
        // and the request falls through to later routing steps.
        let config = create_test_config(); // subagent is None in test config
        let router = Router::new(config);

        let mut request = create_simple_request("Do a subagent task");
        request.system = Some(SystemPrompt::Blocks(vec![
            SystemBlock {
                r#type: "text".to_string(),
                text: "x-anthropic-billing-header: cc_is_subagent=true;".to_string(),
                cache_control: None,
            },
            SystemBlock {
                r#type: "text".to_string(),
                text: "You are Claude Code.".to_string(),
                cache_control: None,
            },
        ]));

        let decision = router.route(&mut request).unwrap();
        // Should NOT route to subagent (no config), falls through to default
        assert_ne!(decision.route_type, RouteType::Subagent);
        // The model name "claude-opus-4" matches auto_map_regex → auto-mapped to default
        assert_eq!(decision.model_name, "default.model");
    }

    #[test]
    fn test_no_subagent_flag_not_routed() {
        let mut config = create_test_config();
        config.router.subagent = Some("config-model".to_string());
        let router = Router::new(config);

        let mut request = create_simple_request("Regular request");
        request.system = Some(SystemPrompt::Blocks(vec![
            SystemBlock {
                r#type: "text".to_string(),
                text: "x-anthropic-billing-header: cc_version=2.1.193.8ed; cc_entrypoint=cli;"
                    .to_string(),
                cache_control: None,
            },
            SystemBlock {
                r#type: "text".to_string(),
                text: "You are Claude Code.".to_string(),
                cache_control: None,
            },
        ]));

        let decision = router.route(&mut request).unwrap();
        assert_ne!(decision.route_type, RouteType::Subagent);
    }

    #[test]
    fn test_legacy_subagent_tag_stripped() {
        let config = create_test_config();
        let router = Router::new(config);

        let mut request = create_simple_request("Task");
        request.system = Some(SystemPrompt::Blocks(vec![
            SystemBlock {
                r#type: "text".to_string(),
                text: "header".to_string(),
                cache_control: None,
            },
            SystemBlock {
                r#type: "text".to_string(),
                text: "some text <CCM-SUBAGENT-MODEL>model-from-tag</CCM-SUBAGENT-MODEL> more text"
                    .to_string(),
                cache_control: None,
            },
        ]));

        router.route(&mut request).unwrap();

        // Tag should be stripped from the text
        if let Some(SystemPrompt::Blocks(blocks)) = &request.system {
            for block in blocks.iter() {
                assert!(!block.text.contains("<CCM-SUBAGENT-MODEL>"));
            }
        }
    }

    #[test]
    fn test_subagent_text_system_prompt_detection() {
        // Code path: is_subagent_request with SystemPrompt::Text variant
        let mut config = create_test_config();
        config.router.subagent = Some("config-model".to_string());
        let router = Router::new(config);

        let mut request = create_simple_request("Do a subagent task");
        request.system = Some(SystemPrompt::Text(
            "x-anthropic-billing-header: cc_is_subagent=true;".to_string(),
        ));

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Subagent);
        assert_eq!(decision.model_name, "config-model");
    }

    #[test]
    fn test_strip_multiple_legacy_tags_in_same_block() {
        // Code path: strip_subagent_tags with replace_all on multiple tags
        let config = create_test_config();
        let router = Router::new(config);

        let mut request = create_simple_request("Task");
        request.system = Some(SystemPrompt::Blocks(vec![
            SystemBlock {
                r#type: "text".to_string(),
                text: "<CCM-SUBAGENT-MODEL>model1</CCM-SUBAGENT-MODEL> and <CCM-SUBAGENT-MODEL>model2</CCM-SUBAGENT-MODEL>".to_string(),
                cache_control: None,
            },
        ]));

        router.route(&mut request).unwrap();

        if let Some(SystemPrompt::Blocks(blocks)) = &request.system {
            for block in blocks.iter() {
                assert!(
                    !block.text.contains("<CCM-SUBAGENT-MODEL>"),
                    "Tag not stripped: {}",
                    block.text
                );
            }
        }
    }

    #[test]
    fn test_subagent_route_also_strips_legacy_tag() {
        // Code path: billing header triggers subagent route, then strip_subagent_tags
        // also removes legacy CCM-SUBAGENT-MODEL tags on the same request
        let mut config = create_test_config();
        config.router.subagent = Some("config-model".to_string());
        let router = Router::new(config);

        let mut request = create_simple_request("Do a subagent task");
        request.system = Some(SystemPrompt::Blocks(vec![
            SystemBlock {
                r#type: "text".to_string(),
                text: "x-anthropic-billing-header: cc_is_subagent=true;".to_string(),
                cache_control: None,
            },
            SystemBlock {
                r#type: "text".to_string(),
                text: "<CCM-SUBAGENT-MODEL>model-from-tag</CCM-SUBAGENT-MODEL>".to_string(),
                cache_control: None,
            },
        ]));

        let decision = router.route(&mut request).unwrap();
        assert_eq!(decision.route_type, RouteType::Subagent);
        // Legacy tag must also be stripped
        if let Some(SystemPrompt::Blocks(blocks)) = &request.system {
            for block in blocks.iter() {
                assert!(
                    !block.text.contains("<CCM-SUBAGENT-MODEL>"),
                    "Legacy tag not stripped: {}",
                    block.text
                );
            }
        }
    }

    #[test]
    fn test_strip_subagent_tags_text_variant() {
        // Code path: strip_subagent_tags with SystemPrompt::Text containing legacy tag
        let config = create_test_config();
        let router = Router::new(config);

        let mut request = create_simple_request("Task");
        request.system = Some(SystemPrompt::Text(
            "Some context <CCM-SUBAGENT-MODEL>old-model</CCM-SUBAGENT-MODEL> trailing".to_string(),
        ));

        router.route(&mut request).unwrap();

        if let Some(SystemPrompt::Text(text)) = &request.system {
            assert!(
                !text.contains("<CCM-SUBAGENT-MODEL>"),
                "Legacy tag not stripped from Text: {}",
                text
            );
        } else {
            panic!("System prompt variant changed unexpectedly");
        }
    }
}
