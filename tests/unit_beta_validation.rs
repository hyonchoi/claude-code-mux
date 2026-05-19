#[cfg(test)]
mod tests {
    use claude_code_mux::server::{parse_anthropic_beta, validate_anthropic_beta};

    #[test]
    fn test_parse_single_beta_option() {
        let result = parse_anthropic_beta("vision-2024-10-22");
        assert_eq!(result.unwrap(), vec!["vision-2024-10-22"]);
    }

    #[test]
    fn test_parse_multiple_beta_options() {
        let result = parse_anthropic_beta("vision-2024-10-22, thinking-2024-11-20");
        let options = result.unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0], "vision-2024-10-22");
        assert_eq!(options[1], "thinking-2024-11-20");
    }

    #[test]
    fn test_parse_beta_with_extra_whitespace() {
        let result = parse_anthropic_beta("  vision-2024-10-22  ,  thinking-2024-11-20  ");
        let options = result.unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0], "vision-2024-10-22");
        assert_eq!(options[1], "thinking-2024-11-20");
    }

    #[test]
    fn test_parse_empty_beta_fails() {
        let result = parse_anthropic_beta("");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_supported_options() {
        let beta_options = vec!["vision-2024-10-22".to_string()];
        let supported = vec![
            "vision-2024-10-22".to_string(),
            "thinking-2024-11-20".to_string(),
        ];
        let result = validate_anthropic_beta(&beta_options, &supported, "claude-opus");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_unsupported_option_fails() {
        let beta_options = vec!["unsupported-option".to_string()];
        let supported = vec!["vision-2024-10-22".to_string()];
        let result = validate_anthropic_beta(&beta_options, &supported, "claude-opus");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported-option"));
    }
}
