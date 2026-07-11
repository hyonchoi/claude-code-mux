use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Anthropic API request format
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Context management directives (e.g. clear_thinking_20251015). Passed
    /// through opaquely so providers that support it (e.g. claude-pt) receive
    /// the directive and apply it themselves (e.g. strip thinking signatures).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<serde_json::Value>,
    /// Output configuration (e.g. effort level for extended thinking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<serde_json::Value>,
    /// Caller-provided bearer token for passthrough mode. Never serialized.
    #[serde(skip)]
    pub passthrough_auth: Option<String>,
    /// anthropic-beta header value from incoming request. Never serialized.
    #[serde(skip)]
    pub anthropic_beta_header: Option<String>,
}

/// Message in the conversation
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

/// Message content can be string or array of content blocks
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// System prompt can be string or array of system blocks
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

/// System message block
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemBlock {
    pub r#type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<serde_json::Value>,
}

/// Tool result content can be string or array of content blocks
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

impl ToolResultContent {
    /// Convert to string (for OpenAI compatibility)
    pub fn to_string(&self) -> String {
        match self {
            ToolResultContent::Text(s) => s.clone(),
            ToolResultContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ToolResultBlock::Text { text, .. } => Some(text.clone()),
                    ToolResultBlock::Image { .. } => Some("[Image]".to_string()),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// Content blocks allowed in tool results
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum ToolResultBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
    #[serde(rename = "image")]
    Image { source: ImageSource },
}

/// Content block for multimodal messages
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

/// Image source for vision API
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ImageSource {
    pub r#type: String, // "base64" or "url"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Tool definition for function calling
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
}

/// Thinking/reasoning configuration for Plan Mode
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThinkingConfig {
    pub r#type: String, // "enabled" or "disabled"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// Token usage information
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Request for counting tokens
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CountTokensRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passthrough_auth: Option<String>,
}

/// Response for token counting
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CountTokensResponse {
    pub input_tokens: u32,
}

/// Router decision result
#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub model_name: String,
    pub route_type: RouteType,
}

/// Type of routing decision
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteType {
    WebSearch,
    Subagent,
    Think,
    Background,
    Default,
}

impl std::fmt::Display for RouteType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteType::WebSearch => write!(f, "web-search"),
            RouteType::Subagent => write!(f, "subagent"),
            RouteType::Think => write!(f, "think"),
            RouteType::Background => write!(f, "background"),
            RouteType::Default => write!(f, "default"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_block_cache_control_roundtrip() {
        let json = r#"{"cache_control":{"ttl":"1h","type":"ephemeral"},"text":"hi","type":"text"}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        let out = serde_json::to_string(&block).unwrap();
        assert!(
            out.contains("cache_control"),
            "cache_control missing from output: {}",
            out
        );
    }

    #[test]
    fn test_tool_result_block_text_cache_control_roundtrip() {
        let json = r#"{"cache_control":{"ttl":"1h","type":"ephemeral"},"text":"hi","type":"text"}"#;
        let block: ToolResultBlock = serde_json::from_str(json).unwrap();
        let out = serde_json::to_string(&block).unwrap();
        assert!(
            out.contains("cache_control"),
            "cache_control missing from ToolResultBlock output: {}",
            out
        );
    }
}
