#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, header};
    use claude_code_mux::server::is_claude_code_cli_request;

    #[test]
    fn test_claude_code_cli_detection_with_version() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            "claude-code/1.0.0".parse().unwrap(),
        );
        assert!(is_claude_code_cli_request(&headers));
    }

    #[test]
    fn test_claude_desktop_cli_detection() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            "ClaudeDesktop/2.1.0".parse().unwrap(),
        );
        assert!(is_claude_code_cli_request(&headers));
    }

    #[test]
    fn test_case_insensitive_detection() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            "CLAUDE-CODE/1.0.0".parse().unwrap(),
        );
        assert!(is_claude_code_cli_request(&headers));
    }

    #[test]
    fn test_claude_cli_detection_with_version() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            "claude-cli/1.2.3".parse().unwrap(),
        );
        assert!(is_claude_code_cli_request(&headers));
    }

    #[test]
    fn test_non_cli_user_agent() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            "Mozilla/5.0".parse().unwrap(),
        );
        assert!(!is_claude_code_cli_request(&headers));
    }

    #[test]
    fn test_no_user_agent() {
        let headers = HeaderMap::new();
        assert!(!is_claude_code_cli_request(&headers));
    }
}
