#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_cli_request_with_passthrough_auth() {
        // Set up: start server with Anthropic provider in passthrough mode
        // Send request with Bearer token and Claude Code CLI user agent
        // Assert: token is passed through to Anthropic API
        // This is a placeholder structure; actual implementation depends on test harness
    }

    #[tokio::test]
    async fn test_cli_request_with_valid_beta_options() {
        // Set up: server configured with model supporting vision-2024-10-22
        // Send: CLI request with anthropic-beta: vision-2024-10-22
        // Assert: header is passed through
    }

    #[tokio::test]
    async fn test_cli_request_with_invalid_beta_options() {
        // Set up: server with model NOT supporting unsupported-option
        // Send: CLI request with anthropic-beta: unsupported-option
        // Assert: HTTP 400 with error message
    }

    #[tokio::test]
    async fn test_non_cli_request_drops_beta() {
        // Set up: regular HTTP client (not Claude Code CLI)
        // Send: request with anthropic-beta header
        // Assert: header is NOT passed through
    }

    #[tokio::test]
    async fn test_api_key_auth_ignores_passthrough() {
        // Set up: provider configured with ApiKey auth
        // Send: CLI request with Bearer token
        // Assert: API key is used, token is ignored
    }
}
