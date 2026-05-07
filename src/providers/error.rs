use thiserror::Error;

/// Provider-specific errors
#[derive(Error, Debug)]
pub enum ProviderError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("JSON serialization failed: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Model not supported by provider: {0}")]
    ModelNotSupported(String),

    #[error("Provider API error: {status} - {message}")]
    ApiError { status: u16, message: String },

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Authentication error: {0}")]
    AuthError(String),

    #[error("Rate limit timeout for provider '{provider}': exceeded wait budget ({max_wait_ms}ms) at {rpm} RPM")]
    RateLimitTimeout {
        provider: String,
        rpm: u32,
        max_wait_ms: u64,
    },
}
