use claude_code_mux::providers::{AuthType, ProviderConfig};

fn make_provider_config(
    rate_limit_rpm: Option<u32>,
    rate_limit_max_wait_ms: Option<u64>,
) -> ProviderConfig {
    ProviderConfig {
        name: "nvidia-nim".to_string(),
        provider_type: "nvidia-nim".to_string(),
        auth_type: AuthType::ApiKey,
        supported_beta_options: vec![],
        api_key: Some("test-key".to_string()),
        oauth_provider: None,
        project_id: None,
        location: None,
        base_url: Some("https://integrate.api.nvidia.com/v1".to_string()),
        models: vec![],
        enabled: Some(true),
        rate_limit_rpm,
        rate_limit_max_wait_ms,
    }
}

#[test]
fn test_provider_config_rate_limits_round_trip() {
    let config = make_provider_config(Some(40), Some(2_000));

    let serialized = toml::to_string(&config).expect("config should serialize");
    assert!(serialized.contains("rate_limit_rpm = 40"));
    assert!(serialized.contains("rate_limit_max_wait_ms = 2000"));

    let parsed: ProviderConfig = toml::from_str(&serialized).expect("config should parse");
    assert_eq!(parsed.rate_limit_rpm, Some(40));
    assert_eq!(parsed.rate_limit_max_wait_ms, Some(2_000));
}

#[test]
fn test_provider_config_rate_limits_omit_unset_fields() {
    let config = make_provider_config(None, None);

    let serialized = toml::to_string(&config).expect("config should serialize");
    assert!(!serialized.contains("rate_limit_rpm"));
    assert!(!serialized.contains("rate_limit_max_wait_ms"));

    let parsed: ProviderConfig = toml::from_str(&serialized).expect("config should parse");
    assert_eq!(parsed.rate_limit_rpm, None);
    assert_eq!(parsed.rate_limit_max_wait_ms, None);
}
