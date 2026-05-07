use claude_code_mux::models::{AnthropicRequest, Message, MessageContent};
use claude_code_mux::providers::error::ProviderError;
use claude_code_mux::providers::{AnthropicCompatibleProvider, AnthropicProvider};
use mockito::Server;
use serde_json::json;

fn make_request() -> AnthropicRequest {
    AnthropicRequest {
        model: "meta-llama-3.1-8b-instruct".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("hello".to_string()),
        }],
        max_tokens: 64,
        thinking: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: None,
        stream: Some(false),
        metadata: None,
        system: None,
        tools: None,
        passthrough_auth: None,
        anthropic_beta_header: None,
    }
}

fn make_success_body(model: &str) -> String {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "model": model,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 2}
    })
    .to_string()
}

#[tokio::test]
async fn test_wait_timeout_allows_manual_next_provider_attempt() {
    let mut primary_server = Server::new_async().await;
    let mut fallback_server = Server::new_async().await;

    let _primary_mock = primary_server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(make_success_body("meta-llama-3.1-8b-instruct"))
        .create_async()
        .await;

    let _fallback_mock = fallback_server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(make_success_body("meta-llama-3.1-8b-instruct"))
        .create_async()
        .await;

    let primary = AnthropicCompatibleProvider::new(
        "primary".to_string(),
        "test-key".to_string(),
        primary_server.url(),
        vec!["meta-llama-3.1-8b-instruct".to_string()],
        None,
        None,
    )
    .with_rate_limit_config(Some(1), Some(1));

    let fallback = AnthropicCompatibleProvider::new(
        "fallback".to_string(),
        "test-key".to_string(),
        fallback_server.url(),
        vec!["meta-llama-3.1-8b-instruct".to_string()],
        None,
        None,
    );

    let first = primary.send_message(make_request()).await;
    assert!(first.is_ok());

    let second = primary.send_message(make_request()).await;
    assert!(matches!(
        second,
        Err(ProviderError::RateLimitTimeout { .. })
    ));

    let fallback_result = fallback.send_message(make_request()).await;
    assert!(fallback_result.is_ok());
}
