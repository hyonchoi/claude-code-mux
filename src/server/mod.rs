mod oauth_handlers;
mod openai_compat;

use crate::auth::TokenStore;
use crate::cli::AppConfig;
use crate::models::AnthropicRequest;
use crate::providers::ProviderRegistry;
use crate::router::Router;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, Sse},
        Html, IntoResponse, Response,
    },
    routing::{get, post},
    Form, Json, Router as AxumRouter,
};
use futures::stream::StreamExt;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, error, info, trace, warn};

// Background Copilot token refresh timing.
// Threshold must exceed the poll interval so a freshly-refreshed 30-min token is
// caught at the next poll instead of expiring silently between checks.
const COPILOT_POLL_SECS: u64 = 20 * 60;
const COPILOT_REFRESH_THRESHOLD_SECS: i64 = COPILOT_POLL_SECS as i64 + 5 * 60; // 25 min

/// Returns true when the token will expire before the next background refresh poll.
fn copilot_token_needs_background_refresh(token: &crate::auth::OAuthToken) -> bool {
    let remaining = token.expires_at.signed_duration_since(chrono::Utc::now());
    remaining < chrono::Duration::seconds(COPILOT_REFRESH_THRESHOLD_SECS)
}

/// Builds a refreshed OAuthToken from a Copilot response, preserving all
/// provider-specific fields from the original (enterprise_url, project_id, etc.).
fn build_refreshed_copilot_token(
    original: &crate::auth::OAuthToken,
    new_access_token: String,
    new_expires_at_unix: u64,
) -> crate::auth::OAuthToken {
    let expires_at =
        chrono::DateTime::from_timestamp(new_expires_at_unix.min(i64::MAX as u64) as i64, 0)
            .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::minutes(30));
    crate::auth::OAuthToken {
        provider_id: original.provider_id.clone(),
        access_token: new_access_token,
        refresh_token: original.refresh_token.clone(),
        expires_at,
        enterprise_url: original.enterprise_url.clone(),
        project_id: original.project_id.clone(),
    }
}

/// Returns the cooldown duration when a provider returns a triggering 4xx error.
/// 401/403 → 60 seconds, 429 → 30 seconds, all others → None (no deactivation).
fn cooldown_for_4xx(
    e: &crate::providers::error::ProviderError,
) -> Option<std::time::Duration> {
    if let crate::providers::error::ProviderError::ApiError { status, .. } = e {
        match *status {
            401 | 403 => Some(std::time::Duration::from_secs(60)),
            429 => Some(std::time::Duration::from_secs(30)),
            _ => None,
        }
    } else {
        None
    }
}

fn is_on_cooldown(
    cooldowns: &dashmap::DashMap<String, std::time::Instant>,
    provider: &str,
) -> bool {
    cooldowns
        .get(provider)
        .map_or(false, |until| std::time::Instant::now() < *until)
}

/// Constant-time byte comparison to prevent timing side-channel attacks on API keys.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Middleware that enforces the configured API key on protected routes.
/// If server.api_key is set, requests must include it via X-Api-Key or Authorization: Bearer.
/// If no api_key is configured, all requests are allowed (backwards compatible).
async fn require_api_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(ref expected) = state.config.server.api_key {
        let provided = headers
            .get("x-api-key")
            .map(|v| v.to_str().unwrap_or(""))
            .or_else(|| {
                headers.get("authorization").and_then(|v| {
                    let s = v.to_str().ok()?;
                    s.strip_prefix("Bearer ")
                        .or_else(|| s.strip_prefix("bearer "))
                })
            })
            .unwrap_or("");

        // Constant-time comparison to prevent timing side-channel attacks
        if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            warn!(
                "Request rejected: invalid or missing API key for {}",
                request.uri().path()
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(next.run(request).await)
}

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub router: Router,
    pub provider_registry: Arc<ProviderRegistry>,
    pub token_store: TokenStore,
    pub config_path: std::path::PathBuf,
    pub provider_cooldowns: Arc<dashmap::DashMap<String, std::time::Instant>>,
}

/// Strip beta options from an AnthropicRequest based on mapping configuration
fn strip_beta_options_from_request(
    request: &mut AnthropicRequest,
    strip_all: bool,
    strip_specific: &[String],
) {
    // If we should strip all beta options
    if strip_all {
        request.anthropic_beta_header = None;
        info!("📝 Stripped all beta options from request");
        return;
    }

    // If we have specific beta options to strip
    if !strip_specific.is_empty() {
        if let Some(ref beta_header) = request.anthropic_beta_header {
            // Parse comma-separated beta options and filter out the ones to strip
            let options: Vec<&str> = beta_header.split(',').map(|s| s.trim()).collect();
            let filtered: Vec<&str> = options
                .iter()
                .filter(|opt| !strip_specific.iter().any(|s| opt.starts_with(s.as_str())))
                .copied()
                .collect();

            if filtered.is_empty() {
                request.anthropic_beta_header = None;
                info!("📝 Stripped all specified beta options; header is now empty");
            } else if filtered.len() < options.len() {
                request.anthropic_beta_header = Some(filtered.join(", "));
                info!("📝 Stripped specific beta options: {:?}", strip_specific);
            }
        }
    }
}

/// Start the HTTP server
pub async fn start_server(
    config: AppConfig,
    config_path: std::path::PathBuf,
) -> anyhow::Result<()> {
    let router = Router::new(config.clone());

    // Initialize OAuth token store FIRST (needed by provider registry)
    let token_store = TokenStore::default()
        .map_err(|e| anyhow::anyhow!("Failed to initialize token store: {}", e))?;

    let existing_tokens = token_store.list_providers();
    if !existing_tokens.is_empty() {
        info!(
            "🔐 Loaded {} OAuth tokens from storage",
            existing_tokens.len()
        );
    }

    // Initialize provider registry from config (with token store)
    let provider_registry = Arc::new(
        ProviderRegistry::from_configs(&config.providers, Some(token_store.clone()))
            .map_err(|e| anyhow::anyhow!("Failed to initialize provider registry: {}", e))?,
    );

    info!(
        "📦 Loaded {} providers with {} models",
        provider_registry.list_providers().len(),
        provider_registry.list_models().len()
    );

    let state = Arc::new(AppState {
        config: config.clone(),
        router,
        provider_registry,
        token_store,
        config_path,
        provider_cooldowns: Arc::new(dashmap::DashMap::new()),
    });

    // Background task: proactively refresh Copilot OAuth bearer tokens every 20 minutes.
    // Copilot bearers have a ~30-minute TTL. Without this, idle providers (not in the
    // active fallback chain) never get refreshed and require full re-OAuth when re-enabled.
    {
        let bg_token_store = state.token_store.clone();
        let bg_providers = state.config.providers.clone();
        let bg_client = reqwest::Client::new();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(COPILOT_POLL_SECS));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                for provider_config in &bg_providers {
                    if provider_config.provider_type == "copilot" {
                        if let Some(token) = bg_token_store.get(&provider_config.name) {
                            if copilot_token_needs_background_refresh(&token) {
                                match crate::auth::github_copilot::refresh_copilot_token(
                                    &bg_client,
                                    &token.refresh_token,
                                )
                                .await
                                {
                                    Ok(resp) => {
                                        let updated = build_refreshed_copilot_token(
                                            &token,
                                            resp.token,
                                            resp.expires_at,
                                        );
                                        if let Err(e) = bg_token_store.save(updated) {
                                            warn!("Background refresh: failed to save Copilot token for '{}': {}", provider_config.name, e);
                                        } else {
                                            info!("Background refresh: renewed Copilot bearer for '{}'", provider_config.name);
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Background refresh: failed to renew Copilot bearer for '{}': {}", provider_config.name, e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Build router — public routes (no auth required)
    let public_routes = AxumRouter::new()
        .route("/", get(serve_admin))
        .route("/health", get(health_check))
        .route("/api/oauth/callback", get(oauth_handlers::oauth_callback))
        .route("/auth/callback", get(oauth_handlers::oauth_callback));

    // Protected routes (auth middleware applied when api_key is configured)
    let protected_routes = AxumRouter::new()
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_count_tokens))
        .route("/v1/chat/completions", post(handle_openai_chat_completions))
        .route("/api/models", get(get_models))
        .route("/api/providers", get(get_providers))
        .route("/api/models-config", get(get_models_config))
        .route("/api/config", get(get_config))
        .route("/api/config", post(update_config))
        .route("/api/config/json", get(get_config_json))
        .route("/api/config/json", post(update_config_json))
        .route("/api/restart", post(restart_server))
        // OAuth endpoints
        .route(
            "/api/oauth/authorize",
            post(oauth_handlers::oauth_authorize),
        )
        .route("/api/oauth/exchange", post(oauth_handlers::oauth_exchange))
        .route("/api/oauth/tokens", get(oauth_handlers::oauth_list_tokens))
        .route(
            "/api/oauth/tokens/delete",
            post(oauth_handlers::oauth_delete_token),
        )
        .route(
            "/api/oauth/tokens/refresh",
            post(oauth_handlers::oauth_refresh_token),
        )
        .route(
            "/api/oauth/copilot-start",
            post(oauth_handlers::copilot_start),
        )
        .route(
            "/api/oauth/copilot-exchange",
            post(oauth_handlers::copilot_exchange),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    let app = public_routes
        .merge(protected_routes)
        .with_state(state.clone());

    // Clone state before moving it
    let oauth_state = state;

    // Bind to main address
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&addr).await?;

    info!("🚀 Server listening on {}", addr);

    // Start OAuth callback server on port 1455 (required for OpenAI Codex)
    // This is necessary because OpenAI's OAuth app only allows localhost:1455/auth/callback
    tokio::spawn(async move {
        let oauth_callback_app = AxumRouter::new()
            .route("/auth/callback", get(oauth_handlers::oauth_callback))
            .with_state(oauth_state);

        let oauth_addr = "127.0.0.1:1455";
        match TcpListener::bind(oauth_addr).await {
            Ok(oauth_listener) => {
                info!("🔐 OAuth callback server listening on {}", oauth_addr);
                if let Err(e) = axum::serve(oauth_listener, oauth_callback_app).await {
                    error!("OAuth callback server error: {}", e);
                }
            }
            Err(e) => {
                // Don't fail if port 1455 is already in use - just warn
                error!(
                    "⚠️  Failed to bind OAuth callback server on {}: {}",
                    oauth_addr, e
                );
                error!("⚠️  OpenAI Codex OAuth will not work. Port 1455 must be available.");
            }
        }
    });

    // Start main server
    axum::serve(listener, app).await?;

    Ok(())
}

/// Serve Admin UI
async fn serve_admin() -> impl IntoResponse {
    Html(include_str!("admin.html"))
}

/// Health check endpoint
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "claude-code-mux"
    }))
}

/// REMOVED: This endpoint was for LiteLLM integration which has been removed.
/// Models are now managed through the provider registry and config.
async fn get_models(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::ParseError(
        "This endpoint has been removed. Use /api/models-config instead.".to_string(),
    ))
}

/// Get current routing configuration
async fn get_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "server": {
            "host": state.config.server.host,
            "port": state.config.server.port,
        },
        "router": {
            "default": state.config.router.default,
            "background": state.config.router.background,
            "think": state.config.router.think,
            "websearch": state.config.router.websearch,
            "subagent": state.config.router.subagent,
        }
    }))
}

/// Update configuration
#[derive(serde::Deserialize)]
struct ConfigUpdate {
    // Router models
    default_model: String,
    background_model: Option<String>,
    think_model: Option<String>,
    websearch_model: Option<String>,
    subagent_model: Option<String>,
}

async fn update_config(
    State(state): State<Arc<AppState>>,
    Form(update): Form<ConfigUpdate>,
) -> Result<Html<String>, AppError> {
    // Read current config
    let config_path = &state.config_path;
    let config_str = std::fs::read_to_string(config_path)
        .map_err(|e| AppError::ParseError(format!("Failed to read config: {}", e)))?;

    let mut config: toml::Value = toml::from_str(&config_str)
        .map_err(|e| AppError::ParseError(format!("Failed to parse config: {}", e)))?;

    // Update router section
    if let Some(router) = config.get_mut("router").and_then(|v| v.as_table_mut()) {
        router.insert(
            "default".to_string(),
            toml::Value::String(update.default_model),
        );

        if let Some(bg) = update.background_model {
            router.insert("background".to_string(), toml::Value::String(bg));
        }

        if let Some(think) = update.think_model {
            router.insert("think".to_string(), toml::Value::String(think));
        }

        if let Some(ws) = update.websearch_model {
            router.insert("websearch".to_string(), toml::Value::String(ws));
        }

        if let Some(ref v) = update.subagent_model {
            router.insert("subagent".to_string(), toml::Value::String(v.clone()));
        }
    }

    // Write back to file
    let new_config_str = toml::to_string_pretty(&config)
        .map_err(|e| AppError::ParseError(format!("Failed to serialize config: {}", e)))?;

    std::fs::write(config_path, new_config_str)
        .map_err(|e| AppError::ParseError(format!("Failed to write config: {}", e)))?;

    info!("✅ Configuration updated successfully");

    Ok(Html("<div class='px-4 py-3 rounded-xl bg-primary/20 border border-primary/50 text-foreground text-sm'>✅ Configuration saved successfully! Please restart the server to apply changes.</div>".to_string()))
}

/// Redact api_key values in provider configs before sending to clients.
/// Replaces actual keys with a boolean indicating whether a key is set.
fn redact_provider_api_keys(providers: &serde_json::Value) -> serde_json::Value {
    let mut result = providers.clone();
    if let Some(arr) = result.as_array_mut() {
        for provider in arr.iter_mut() {
            if let Some(obj) = provider.as_object_mut() {
                if let Some(api_key) = obj.remove("api_key") {
                    obj.insert(
                        "api_key_set".to_string(),
                        serde_json::Value::Bool(
                            !api_key.is_null() && !api_key.as_str().unwrap_or("").is_empty(),
                        ),
                    );
                }
            }
        }
    }
    result
}

/// Get providers configuration
async fn get_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers_json = serde_json::to_value(&state.config.providers).unwrap_or_default();
    Json(redact_provider_api_keys(&providers_json))
}

/// Get models configuration
async fn get_models_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.config.models.clone())
}

/// Get full configuration as JSON (for admin UI)
async fn get_config_json(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers_json = serde_json::to_value(&state.config.providers).unwrap_or_default();
    Json(serde_json::json!({
        "server": {
            "host": state.config.server.host,
            "port": state.config.server.port,
        },
        "router": {
            "default": state.config.router.default,
            "background": state.config.router.background,
            "think": state.config.router.think,
            "websearch": state.config.router.websearch,
            "subagent": state.config.router.subagent,
        },
        "providers": redact_provider_api_keys(&providers_json),
        "models": state.config.models,
    }))
}

/// Remove null values from JSON (TOML doesn't support null)
fn remove_null_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for (_, v) in map.iter_mut() {
                remove_null_values(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                remove_null_values(item);
            }
        }
        _ => {}
    }
}

/// Update configuration via JSON (for admin UI)
async fn update_config_json(
    State(state): State<Arc<AppState>>,
    Json(mut new_config): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Remove null values (TOML doesn't support null)
    remove_null_values(&mut new_config);

    // Write back to config file
    let config_path = &state.config_path;

    // Read current config
    let config_str = std::fs::read_to_string(config_path)
        .map_err(|e| AppError::ParseError(format!("Failed to read config: {}", e)))?;

    let mut config: toml::Value = toml::from_str(&config_str)
        .map_err(|e| AppError::ParseError(format!("Failed to parse config: {}", e)))?;

    // Update providers section
    if let Some(providers) = new_config.get_mut("providers") {
        // Restore redacted api_keys: the GET endpoint replaces api_key with api_key_set
        // (a boolean). When saving back, strip api_key_set and restore the actual key
        // from the current config for any provider where it was redacted.
        if let Some(arr) = providers.as_array_mut() {
            let current_providers = config.get("providers");
            for provider in arr.iter_mut() {
                if let Some(obj) = provider.as_object_mut() {
                    let api_key_set = obj.remove("api_key_set");
                    if matches!(api_key_set, Some(serde_json::Value::Bool(true)))
                        && !obj.contains_key("api_key")
                    {
                        // Find the matching provider in the current config and restore its key
                        if let Some(name) = obj
                            .get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                        {
                            if let Some(toml::Value::Array(current_arr)) = current_providers {
                                for current in current_arr {
                                    if let toml::Value::Table(t) = current {
                                        if t.get("name").and_then(|v| v.as_str()) == Some(&name) {
                                            if let Some(toml::Value::String(key)) = t.get("api_key")
                                            {
                                                obj.insert(
                                                    "api_key".to_string(),
                                                    serde_json::Value::String(key.clone()),
                                                );
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Convert from serde_json::Value to toml::Value
        let providers_toml: toml::Value = serde_json::from_str(&providers.to_string())
            .map_err(|e| AppError::ParseError(format!("Failed to convert providers: {}", e)))?;

        if let Some(table) = config.as_table_mut() {
            table.insert("providers".to_string(), providers_toml);
        }
    }

    // Update models section
    if let Some(models) = new_config.get("models") {
        // Convert from serde_json::Value to toml::Value
        let models_toml: toml::Value = serde_json::from_str(&models.to_string())
            .map_err(|e| AppError::ParseError(format!("Failed to convert models: {}", e)))?;

        if let Some(table) = config.as_table_mut() {
            table.insert("models".to_string(), models_toml);
        }
    }

    // Update router section if provided
    if let Some(router) = new_config.get("router") {
        if let Some(router_table) = config.get_mut("router").and_then(|v| v.as_table_mut()) {
            if let Some(default) = router.get("default") {
                if let Some(s) = default.as_str() {
                    router_table.insert("default".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(think) = router.get("think") {
                if let Some(s) = think.as_str() {
                    router_table.insert("think".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(ws) = router.get("websearch") {
                if let Some(s) = ws.as_str() {
                    router_table
                        .insert("websearch".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(bg) = router.get("background") {
                if let Some(s) = bg.as_str() {
                    router_table
                        .insert("background".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(subagent) = router.get("subagent") {
                match subagent.as_str() {
                    Some(s) if !s.is_empty() => {
                        router_table
                            .insert("subagent".to_string(), toml::Value::String(s.to_string()));
                    }
                    _ => {
                        router_table.remove("subagent");
                    }
                }
            }
            if let Some(auto_map) = router.get("auto_map_regex") {
                if let Some(s) = auto_map.as_str() {
                    router_table.insert(
                        "auto_map_regex".to_string(),
                        toml::Value::String(s.to_string()),
                    );
                }
            }
            if let Some(bg_regex) = router.get("background_regex") {
                if let Some(s) = bg_regex.as_str() {
                    router_table.insert(
                        "background_regex".to_string(),
                        toml::Value::String(s.to_string()),
                    );
                }
            }
        }
    }

    // Write back to file
    let new_config_str = toml::to_string_pretty(&config)
        .map_err(|e| AppError::ParseError(format!("Failed to serialize config: {}", e)))?;

    std::fs::write(config_path, new_config_str)
        .map_err(|e| AppError::ParseError(format!("Failed to write config: {}", e)))?;

    info!("✅ Configuration updated successfully via admin UI");

    Ok(Json(serde_json::json!({
        "status": "success",
        "message": "Configuration saved successfully"
    })))
}

/// Restart server automatically using shell script
async fn restart_server(State(state): State<Arc<AppState>>) -> Response {
    info!("🔄 Server restart requested via UI");

    let port = state.config.server.port;

    // Create a shell script to handle restart
    match create_and_execute_restart_script(port) {
        Ok(_) => {
            info!("✅ Restart script initiated");

            let response = Html("<div class='px-4 py-3 rounded-xl bg-green-500/20 border border-green-500/50 text-foreground text-sm'><strong>✅ Server restarting...</strong><br/>Shutting down current instance and starting new one.</div>").into_response();

            // Shutdown current process after a short delay
            tokio::spawn(async {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                info!("Shutting down for restart...");
                std::process::exit(0);
            });

            response
        }
        Err(e) => {
            error!("Failed to initiate restart: {}", e);
            Html(format!("<div class='px-4 py-3 rounded-xl bg-red-500/20 border border-red-500/50 text-foreground text-sm'><strong>❌ Restart failed</strong><br/>Error: {}</div>", e)).into_response()
        }
    }
}

/// Create and execute a shell script that waits for shutdown and restarts
fn create_and_execute_restart_script(port: u16) -> std::io::Result<()> {
    use std::fs;
    use std::process::Command;

    // Get current executable path and PID
    let exe_path = std::env::current_exe()?;
    let current_pid = std::process::id();

    info!(
        "Creating restart script for PID: {} on port: {}",
        current_pid, port
    );

    #[cfg(unix)]
    {
        // Create shell script
        let script_content = format!(
            r#"#!/bin/bash
# Wait for old process to exit
while kill -0 {} 2>/dev/null; do
    sleep 0.1
done
# Start new server
'{}' start --port {} > /dev/null 2>&1 &
"#,
            current_pid,
            exe_path.display(),
            port
        );

        let script_path = "/tmp/ccm_restart.sh";
        fs::write(script_path, script_content)?;

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(script_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(script_path, perms)?;
        }

        // Execute script in background
        Command::new("sh")
            .arg(script_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        info!("Restart script started");
    }

    #[cfg(windows)]
    {
        // Create batch script for Windows
        let script_content = format!(
            r#"@echo off
:wait
tasklist /FI "PID eq {}" 2>NUL | find /I /N "ccm.exe">NUL
if "%ERRORLEVEL%"=="0" (
    timeout /t 1 /nobreak > nul
    goto wait
)
start "" "{}" start --port {}
"#,
            current_pid,
            exe_path.display(),
            port
        );

        let script_path = std::env::temp_dir().join("ccm_restart.bat");
        fs::write(&script_path, script_content)?;

        // Execute batch file
        Command::new("cmd")
            .args(&["/C", "start", "/B", script_path.to_str().unwrap()])
            .spawn()?;
    }

    Ok(())
}

/// Returns true if the named provider should receive passthrough auth.
/// Only anthropic-type providers with auth_type=passthrough are eligible.
/// Other providers (copilot, nvidia-nim, apikey/oauth anthropic) use their own auth.
fn should_use_passthrough_auth(providers: &[crate::providers::ProviderConfig], name: &str) -> bool {
    providers
        .iter()
        .find(|p| p.name == name)
        .map(|p| {
            p.provider_type == "anthropic"
                && matches!(p.auth_type, crate::providers::AuthType::Passthrough)
        })
        .unwrap_or(false)
}

/// Detects Claude Code CLI requests via User-Agent header matching
pub fn is_claude_code_cli_request(headers: &HeaderMap) -> bool {
    if let Some(user_agent) = headers.get(header::USER_AGENT) {
        if let Ok(ua_str) = user_agent.to_str() {
            let ua_lower = ua_str.to_lowercase();
            ua_lower.contains("claude-code/")
                || ua_lower.contains("claude-cli/")
                || ua_lower.contains("claudedesktop/")
        } else {
            false
        }
    } else {
        false
    }
}

/// Parses anthropic-beta header in CSV format
/// Returns list of beta options or error if invalid
pub fn parse_anthropic_beta(header_value: &str) -> Result<Vec<String>, String> {
    tracing::debug!(
        "parse_anthropic_beta: starting parse of header: '{}'",
        header_value
    );

    let options = header_value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    if options.is_empty() {
        tracing::debug!(
            "parse_anthropic_beta: empty header value provided (header_value='{}')",
            header_value
        );
        return Err("anthropic-beta header is empty".to_string());
    }

    tracing::debug!(
        "parse_anthropic_beta: parsed {} options from header",
        options.len()
    );
    for (i, option) in options.iter().enumerate() {
        tracing::debug!("parse_anthropic_beta: option[{}] = '{}'", i, option);
    }
    tracing::debug!("parse_anthropic_beta: parsed options: {:?}", options);
    Ok(options)
}

/// Validates anthropic-beta options against supported model options
pub fn validate_anthropic_beta(
    beta_options: &[String],
    supported_options: &[String],
    model_name: &str,
) -> Result<(), String> {
    tracing::debug!(
        "validate_anthropic_beta: starting validation for model '{}' with {} requested options",
        model_name,
        beta_options.len()
    );
    tracing::debug!(
        "validate_anthropic_beta: supported options for model '{}': {:?}",
        model_name,
        supported_options
    );

    for (i, option) in beta_options.iter().enumerate() {
        tracing::debug!(
            "validate_anthropic_beta: checking option[{}] = '{}' for model '{}'",
            i,
            option,
            model_name
        );

        if !supported_options.contains(option) {
            tracing::warn!(
                "validate_anthropic_beta: option '{}' NOT found in supported list for model '{}'. Supported options: {:?}",
                option,
                model_name,
                supported_options
            );
            return Err(format!(
                "Option '{}' not supported for model '{}'",
                option, model_name
            ));
        }

        tracing::debug!(
            "validate_anthropic_beta: option[{}] '{}' is VALID for model '{}'",
            i,
            option,
            model_name
        );
    }

    tracing::debug!(
        "validate_anthropic_beta: ALL {} options VALIDATED successfully for model '{}'",
        beta_options.len(),
        model_name
    );
    Ok(())
}

/// Handle /v1/chat/completions requests (OpenAI-compatible endpoint)
async fn handle_openai_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(openai_request): Json<openai_compat::OpenAIRequest>,
) -> Result<Response, AppError> {
    let model = openai_request.model.clone();
    info!("Received OpenAI-compatible request for model: {}", model);

    // Extract and validate bearer token for passthrough mode.
    // Only CC CLI requests are eligible; a present-but-invalid Bearer header is rejected.
    let passthrough_token = if is_claude_code_cli_request(&headers) {
        let token = extract_bearer_token(&headers);
        if token.is_none() && has_bearer_prefix(&headers) {
            return Err(AppError::AuthError(
                "Invalid Bearer token format".to_string(),
            ));
        }
        if token.is_none() && !has_bearer_prefix(&headers) {
            debug!("🔑 CC CLI request has no Bearer header — passthrough skipped");
        }
        token
    } else {
        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>");
        debug!(
            "🔑 Non-CC-CLI request (User-Agent: '{}') — passthrough skipped",
            ua
        );
        None
    };

    if passthrough_token.is_some() {
        info!("🔑 Passthrough mode detected (caller-provided bearer token)");
    }

    // 1. Transform OpenAI request to Anthropic format
    let mut anthropic_request = openai_compat::transform_openai_to_anthropic(openai_request)
        .map_err(|e| AppError::ParseError(format!("Failed to transform OpenAI request: {}", e)))?;

    anthropic_request.passthrough_auth = None; // Set per-mapping in the fallback loop
    anthropic_request.anthropic_beta_header = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if let Some(ref beta_header) = anthropic_request.anthropic_beta_header {
        tracing::debug!(
            "OpenAI request handler: extracted anthropic-beta header: '{}'",
            beta_header
        );
    } else {
        tracing::debug!("OpenAI request handler: no anthropic-beta header found in request");
    }

    info!("Transformed OpenAI request to Anthropic format");

    // 2. Route the request (may modify system prompt to remove CCM-SUBAGENT-MODEL tag)
    let decision = state
        .router
        .route(&mut anthropic_request)
        .map_err(|e| AppError::RoutingError(e.to_string()))?;

    info!(
        "🎯 Routed to: {} ({})",
        decision.model_name, decision.route_type
    );

    // 3. Try model mappings with fallback (1:N mapping)
    if let Some(model_config) = state
        .config
        .models
        .iter()
        .find(|m| m.name == decision.model_name)
    {
        info!(
            "📋 Found {} provider mappings for model: {}",
            model_config.mappings.len(),
            decision.model_name
        );

        // Check for X-Provider header to override priority
        let forced_provider = headers
            .get("x-provider")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if let Some(ref provider_name) = forced_provider {
            info!(
                "🎯 Using forced provider from X-Provider header: {}",
                provider_name
            );
        }

        // Sort mappings by priority (or filter by forced provider)
        let mut sorted_mappings = model_config.mappings.clone();

        if let Some(ref provider_name) = forced_provider {
            // Filter to only the specified provider
            sorted_mappings.retain(|m| m.provider == *provider_name);
            if sorted_mappings.is_empty() {
                return Err(AppError::RoutingError(format!(
                    "Provider '{}' not found in mappings for model '{}'",
                    provider_name, decision.model_name
                )));
            }
        } else {
            // Use priority ordering
            sorted_mappings.sort_by_key(|m| m.priority);
        }

        // Save original beta header to restore for each mapping attempt
        let original_beta_header = anthropic_request.anthropic_beta_header.clone();

        // Try each mapping in priority order (or just the forced one)
        let mut fallback_failures = Vec::new();
        for (idx, mapping) in sorted_mappings.iter().enumerate() {
            info!(
                "🔄 Trying mapping {}/{}: provider={}, actual_model={}",
                idx + 1,
                sorted_mappings.len(),
                mapping.provider,
                mapping.actual_model
            );

            if is_on_cooldown(&state.provider_cooldowns, &mapping.provider) {
                info!("⏭ Skipping provider {} (on cooldown)", mapping.provider);
                fallback_failures.push(format!("{}: on cooldown", mapping.provider));
                continue;
            }

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Update model to actual model name
                anthropic_request.model = mapping.actual_model.clone();

                // Set passthrough auth per-mapping: only passthrough-type anthropic providers
                // should receive the caller's bearer token; others use their own auth
                anthropic_request.passthrough_auth =
                    if should_use_passthrough_auth(&state.config.providers, &mapping.provider) {
                        passthrough_token.clone()
                    } else {
                        None
                    };

                // Restore original beta header before applying mapping-specific stripping
                anthropic_request.anthropic_beta_header = original_beta_header.clone();

                // Strip beta options if configured in the mapping
                strip_beta_options_from_request(
                    &mut anthropic_request,
                    mapping.strip_beta_options,
                    &mapping.strip_specific_beta,
                );

                // Check if streaming is requested
                let is_streaming = anthropic_request.stream == Some(true);

                if is_streaming {
                    // Streaming not fully implemented for OpenAI format yet
                    info!("⚠️ Streaming requested but not fully supported for OpenAI format, falling back to non-streaming");
                }

                // Non-streaming request
                match provider.send_message(anthropic_request.clone()).await {
                    Ok(anthropic_response) => {
                        info!("✅ Request succeeded with provider: {}", mapping.provider);

                        // Transform Anthropic response to OpenAI format
                        let openai_response = openai_compat::transform_anthropic_to_openai(
                            anthropic_response,
                            model.clone(),
                        );

                        return Ok(Json(openai_response).into_response());
                    }
                    Err(e) => {
                        warn!(
                            "⚠️ Provider {} failed: {}, trying next fallback",
                            mapping.provider, e
                        );
                        if let Some(duration) = cooldown_for_4xx(&e) {
                            state.provider_cooldowns.insert(
                                mapping.provider.clone(),
                                std::time::Instant::now() + duration,
                            );
                            warn!(
                                "⏸ Provider {} on cooldown for {}s",
                                mapping.provider,
                                duration.as_secs()
                            );
                        }
                        fallback_failures.push(format!("{}: {}", mapping.provider, e));
                        continue;
                    }
                }
            } else {
                warn!(
                    "⚠️ Provider {} not found in registry, trying next fallback",
                    mapping.provider
                );
                fallback_failures.push(format!(
                    "{}: provider not found in registry",
                    mapping.provider
                ));
                continue;
            }
        }

        error!(
            "❌ All provider mappings failed for model: {}. Last failures: {}",
            decision.model_name,
            fallback_failures.join(" | ")
        );
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for model: {} ({})",
            sorted_mappings.len(),
            decision.model_name,
            fallback_failures.join(" | ")
        )));
    } else {
        if passthrough_token.is_some() {
            return Err(AppError::RoutingError(
                "Passthrough auth requires explicit [[models]] configuration".to_string(),
            ));
        }

        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state
            .provider_registry
            .get_provider_for_model(&decision.model_name)
        {
            info!(
                "📦 Using provider from registry (direct lookup): {}",
                decision.model_name
            );

            // Update model to routed model
            anthropic_request.model = decision.model_name.clone();

            // Call provider
            let anthropic_response = provider
                .send_message(anthropic_request)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            // Transform to OpenAI format
            let openai_response =
                openai_compat::transform_anthropic_to_openai(anthropic_response, model);

            return Ok(Json(openai_response).into_response());
        }

        error!(
            "❌ No model mapping or provider found for model: {}",
            decision.model_name
        );
        return Err(AppError::ProviderError(format!(
            "No model mapping or provider found for model: {}",
            decision.model_name
        )));
    }
}

/// Handle /v1/messages requests (both streaming and non-streaming)
async fn handle_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request_json): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    let model = request_json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");
    info!("Received request for model: {}", model);

    // DEBUG: Log request body for debugging
    if let Ok(json_str) = serde_json::to_string_pretty(&request_json) {
        trace!("📥 Incoming request body:\n{}", json_str);
    }

    // Extract and validate bearer token for passthrough mode.
    // Only CC CLI requests are eligible; a present-but-invalid Bearer header is rejected.
    let passthrough_token = if is_claude_code_cli_request(&headers) {
        let token = extract_bearer_token(&headers);
        if token.is_none() && has_bearer_prefix(&headers) {
            return Err(AppError::AuthError(
                "Invalid Bearer token format".to_string(),
            ));
        }
        if token.is_none() && !has_bearer_prefix(&headers) {
            debug!("🔑 CC CLI request has no Bearer header — passthrough skipped");
        }
        token
    } else {
        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>");
        debug!(
            "🔑 Non-CC-CLI request (User-Agent: '{}') — passthrough skipped",
            ua
        );
        None
    };

    if passthrough_token.is_some() {
        info!("🔑 Passthrough mode detected (caller-provided bearer token)");
    }

    let incoming_beta_header = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    // 1. Parse request for routing decision (mutable for tag extraction)
    let mut request_for_routing: AnthropicRequest = serde_json::from_value(request_json.clone())
        .map_err(|e| {
            tracing::error!("❌ Failed to parse request: {}", e);
            AppError::ParseError(format!("Invalid request format: {}", e))
        })?;

    request_for_routing.passthrough_auth = passthrough_token.clone();
    request_for_routing.anthropic_beta_header = incoming_beta_header.clone();

    // 2. Route the request (may modify system prompt to remove CCM-SUBAGENT-MODEL tag)
    let decision = state
        .router
        .route(&mut request_for_routing)
        .map_err(|e| AppError::RoutingError(e.to_string()))?;

    info!(
        "🎯 Routed to: {} ({})",
        decision.model_name, decision.route_type
    );

    // 3. Try model mappings with fallback (1:N mapping)
    if let Some(model_config) = state
        .config
        .models
        .iter()
        .find(|m| m.name == decision.model_name)
    {
        info!(
            "📋 Found {} provider mappings for model: {}",
            model_config.mappings.len(),
            decision.model_name
        );

        // Check for X-Provider header to override priority
        let forced_provider = headers
            .get("x-provider")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty()) // Ignore empty strings
            .map(|s| s.to_string());

        if let Some(ref provider_name) = forced_provider {
            info!(
                "🎯 Using forced provider from X-Provider header: {}",
                provider_name
            );
        }

        // Sort mappings by priority (or filter by forced provider)
        let mut sorted_mappings = model_config.mappings.clone();

        if let Some(ref provider_name) = forced_provider {
            // Filter to only the specified provider
            sorted_mappings.retain(|m| m.provider == *provider_name);
            if sorted_mappings.is_empty() {
                return Err(AppError::RoutingError(format!(
                    "Provider '{}' not found in mappings for model '{}'",
                    provider_name, decision.model_name
                )));
            }
        } else {
            // Use priority ordering
            sorted_mappings.sort_by_key(|m| m.priority);
        }

        // In passthrough mode, each mapping decides whether to use passthrough auth:
        // - anthropic + auth_type=passthrough → use passthrough bearer
        // - all others (apikey, oauth, copilot, nvidia-nim) → ignore passthrough, use own auth
        // All mappings stay in the fallback list regardless.

        // Try each mapping in priority order (or just the forced one)
        let mut fallback_failures = Vec::new();
        for (idx, mapping) in sorted_mappings.iter().enumerate() {
            info!(
                "🔄 Trying mapping {}/{}: provider={}, actual_model={}",
                idx + 1,
                sorted_mappings.len(),
                mapping.provider,
                mapping.actual_model
            );

            if is_on_cooldown(&state.provider_cooldowns, &mapping.provider) {
                info!("⏭ Skipping provider {} (on cooldown)", mapping.provider);
                fallback_failures.push(format!("{}: on cooldown", mapping.provider));
                continue;
            }

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Trust the model mapping configuration - no need to validate

                // Parse request as Anthropic format
                let mut anthropic_request: AnthropicRequest =
                    serde_json::from_value(request_json.clone()).map_err(|e| {
                        AppError::ParseError(format!("Invalid request format: {}", e))
                    })?;

                // Save original model name for response
                let original_model = anthropic_request.model.clone();

                // Update model to actual model name
                anthropic_request.model = mapping.actual_model.clone();

                // Propagate passthrough auth and beta header before stripping, so the
                // strip logic can see the actual header value (anthropic_beta_header is
                // #[serde(skip)] and therefore always None after JSON deserialization)
                anthropic_request.passthrough_auth =
                    if should_use_passthrough_auth(&state.config.providers, &mapping.provider) {
                        passthrough_token.clone()
                    } else {
                        None
                    };
                anthropic_request.anthropic_beta_header = incoming_beta_header.clone();

                // Strip beta options if configured in the mapping
                strip_beta_options_from_request(
                    &mut anthropic_request,
                    mapping.strip_beta_options,
                    &mapping.strip_specific_beta,
                );

                // Update system if modified during routing
                anthropic_request.system = request_for_routing.system.clone();

                if passthrough_token.is_some() {
                    info!(
                        "🔑 Passthrough auth active: original_model={}, target_provider={}",
                        original_model, mapping.provider
                    );
                }

                // Check if streaming is requested
                let is_streaming = anthropic_request.stream == Some(true);

                if is_streaming {
                    // Streaming request
                    info!("🌊 Streaming request to provider: {}", mapping.provider);

                    match provider.send_message_stream(anthropic_request).await {
                        Ok(stream) => {
                            info!(
                                "✅ Streaming request started with provider: {}",
                                mapping.provider
                            );

                            // Convert byte stream to SSE response
                            // The provider returns raw bytes (SSE format), we pass them through
                            let sse_stream = stream.map(|result| {
                                result
                                    .map(|bytes| {
                                        // Convert bytes to string for SSE event
                                        let data = String::from_utf8_lossy(&bytes).to_string();
                                        Event::default().data(data)
                                    })
                                    .map_err(|e| {
                                        error!("Stream error: {}", e);
                                        std::io::Error::new(
                                            std::io::ErrorKind::Other,
                                            e.to_string(),
                                        )
                                    })
                            });

                            return Ok(Sse::new(sse_stream).into_response());
                        }
                        Err(e) => {
                            warn!(
                                "⚠️ Provider {} streaming failed: {}, trying next fallback",
                                mapping.provider, e
                            );
                            if let Some(duration) = cooldown_for_4xx(&e) {
                                state.provider_cooldowns.insert(
                                    mapping.provider.clone(),
                                    std::time::Instant::now() + duration,
                                );
                                warn!(
                                    "⏸ Provider {} on cooldown for {}s",
                                    mapping.provider,
                                    duration.as_secs()
                                );
                            }
                            fallback_failures.push(format!("{} (stream): {}", mapping.provider, e));
                            continue;
                        }
                    }
                } else {
                    // Non-streaming request (original behavior)
                    match provider.send_message(anthropic_request).await {
                        Ok(mut response) => {
                            // Restore original model name in response
                            response.model = original_model;
                            info!(
                                "✅ Request succeeded with provider: {}, response model: {}",
                                mapping.provider, response.model
                            );
                            return Ok(Json(response).into_response());
                        }
                        Err(e) => {
                            warn!(
                                "⚠️ Provider {} failed: {}, trying next fallback",
                                mapping.provider, e
                            );
                            if let Some(duration) = cooldown_for_4xx(&e) {
                                state.provider_cooldowns.insert(
                                    mapping.provider.clone(),
                                    std::time::Instant::now() + duration,
                                );
                                warn!(
                                    "⏸ Provider {} on cooldown for {}s",
                                    mapping.provider,
                                    duration.as_secs()
                                );
                            }
                            fallback_failures.push(format!("{}: {}", mapping.provider, e));
                            continue;
                        }
                    }
                }
            } else {
                warn!(
                    "⚠️ Provider {} not found in registry, trying next fallback",
                    mapping.provider
                );
                fallback_failures.push(format!(
                    "{}: provider not found in registry",
                    mapping.provider
                ));
                continue;
            }
        }

        error!(
            "❌ All provider mappings failed for model: {}. Last failures: {}",
            decision.model_name,
            fallback_failures.join(" | ")
        );
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for model: {} ({})",
            sorted_mappings.len(),
            decision.model_name,
            fallback_failures.join(" | ")
        )));
    } else {
        // Passthrough requires explicit model mappings to enforce provider-type filtering
        if passthrough_token.is_some() {
            return Err(AppError::RoutingError(
                "Passthrough auth requires explicit [[models]] configuration".to_string(),
            ));
        }

        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state
            .provider_registry
            .get_provider_for_model(&decision.model_name)
        {
            info!(
                "📦 Using provider from registry (direct lookup): {}",
                decision.model_name
            );

            // Parse request as Anthropic format
            let mut anthropic_request: AnthropicRequest =
                serde_json::from_value(request_json.clone())
                    .map_err(|e| AppError::ParseError(format!("Invalid request format: {}", e)))?;

            // Save original model name for response
            let original_model = anthropic_request.model.clone();

            // Update model to routed model
            anthropic_request.model = decision.model_name.clone();

            // Update system if modified during routing
            anthropic_request.system = request_for_routing.system.clone();

            // Call provider
            let mut provider_response = provider
                .send_message(anthropic_request)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            // Restore original model name in response
            provider_response.model = original_model;

            // Return provider response
            return Ok(Json(provider_response).into_response());
        }

        error!(
            "❌ No model mapping or provider found for model: {}",
            decision.model_name
        );
        return Err(AppError::ProviderError(format!(
            "No model mapping or provider found for model: {}",
            decision.model_name
        )));
    }
}

/// Handle /v1/messages/count_tokens requests
async fn handle_count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request_json): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    let model = request_json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");
    info!("Received count_tokens request for model: {}", model);

    let passthrough_token = if is_claude_code_cli_request(&headers) {
        let token = extract_bearer_token(&headers);
        if token.is_none() && has_bearer_prefix(&headers) {
            return Err(AppError::AuthError(
                "Invalid Bearer token format".to_string(),
            ));
        }
        if token.is_none() && !has_bearer_prefix(&headers) {
            debug!("🔑 CC CLI count_tokens request has no Bearer header — passthrough skipped");
        }
        token
    } else {
        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>");
        debug!(
            "🔑 Non-CC-CLI count_tokens request (User-Agent: '{}') — passthrough skipped",
            ua
        );
        None
    };

    if passthrough_token.is_some() {
        info!("🔑 Passthrough mode detected for count_tokens (caller-provided bearer token)");
    }

    // 1. Parse as CountTokensRequest first
    use crate::models::CountTokensRequest;
    let count_request: CountTokensRequest = serde_json::from_value(request_json.clone())
        .map_err(|e| AppError::ParseError(format!("Invalid count_tokens request format: {}", e)))?;

    // 2. Create a minimal AnthropicRequest for routing
    let mut routing_request = AnthropicRequest {
        model: count_request.model.clone(),
        messages: count_request.messages.clone(),
        max_tokens: 1024, // Dummy value for routing
        system: count_request.system.clone(),
        tools: count_request.tools.clone(),
        thinking: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: None,
        stream: None,
        metadata: None,
        passthrough_auth: passthrough_token.clone(),
        anthropic_beta_header: None,
    };
    let decision = state
        .router
        .route(&mut routing_request)
        .map_err(|e| AppError::RoutingError(e.to_string()))?;

    info!(
        "🧮 Routed count_tokens: {} → {} ({})",
        model, decision.model_name, decision.route_type
    );

    // 3. Try model mappings with fallback (1:N mapping)
    if let Some(model_config) = state
        .config
        .models
        .iter()
        .find(|m| m.name == decision.model_name)
    {
        info!(
            "📋 Found {} provider mappings for token counting: {}",
            model_config.mappings.len(),
            decision.model_name
        );

        // Sort mappings by priority
        let mut sorted_mappings = model_config.mappings.clone();
        sorted_mappings.sort_by_key(|m| m.priority);

        // Try each mapping in priority order
        for (idx, mapping) in sorted_mappings.iter().enumerate() {
            info!(
                "🔄 Trying token count mapping {}/{}: provider={}, actual_model={}",
                idx + 1,
                sorted_mappings.len(),
                mapping.provider,
                mapping.actual_model
            );

            if is_on_cooldown(&state.provider_cooldowns, &mapping.provider) {
                info!("⏭ Skipping provider {} (on cooldown)", mapping.provider);
                continue;
            }

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Trust the model mapping configuration - no need to validate

                // Update model to actual model name and include passthrough auth if present
                let mut count_request_for_provider = count_request.clone();
                count_request_for_provider.model = mapping.actual_model.clone();
                count_request_for_provider.passthrough_auth =
                    if should_use_passthrough_auth(&state.config.providers, &mapping.provider) {
                        passthrough_token.clone()
                    } else {
                        None
                    };

                // Call provider's count_tokens
                match provider.count_tokens(count_request_for_provider).await {
                    Ok(response) => {
                        info!(
                            "✅ Token count succeeded with provider: {}",
                            mapping.provider
                        );
                        return Ok(Json(response).into_response());
                    }
                    Err(e) => {
                        warn!(
                            "⚠️ Provider {} failed: {}, trying next fallback",
                            mapping.provider, e
                        );
                        if let Some(duration) = cooldown_for_4xx(&e) {
                            state.provider_cooldowns.insert(
                                mapping.provider.clone(),
                                std::time::Instant::now() + duration,
                            );
                            warn!(
                                "⏸ Provider {} on cooldown for {}s",
                                mapping.provider,
                                duration.as_secs()
                            );
                        }
                        continue;
                    }
                }
            } else {
                warn!(
                    "⚠️ Provider {} not found in registry, trying next fallback",
                    mapping.provider
                );
                continue;
            }
        }

        error!(
            "❌ All provider mappings failed for token counting: {}",
            decision.model_name
        );
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for token counting: {}",
            sorted_mappings.len(),
            decision.model_name
        )));
    } else {
        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state
            .provider_registry
            .get_provider_for_model(&decision.model_name)
        {
            info!(
                "📦 Using provider from registry (direct lookup) for token counting: {}",
                decision.model_name
            );

            // Update model to routed model and include passthrough auth if eligible
            let mut count_request_for_provider = count_request.clone();
            count_request_for_provider.model = decision.model_name.clone();
            // Look up the provider name from [[models]] config to decide passthrough eligibility
            let provider_name = state
                .config
                .models
                .iter()
                .find(|m| m.name == decision.model_name)
                .and_then(|m| m.mappings.first())
                .map(|m| m.provider.clone());
            count_request_for_provider.passthrough_auth = if let Some(ref pname) = provider_name {
                if should_use_passthrough_auth(&state.config.providers, pname) {
                    passthrough_token.clone()
                } else {
                    None
                }
            } else {
                None
            };

            // Call provider's count_tokens
            let response = provider
                .count_tokens(count_request_for_provider)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            info!("✅ Token count completed via provider");
            return Ok(Json(response).into_response());
        }

        error!(
            "❌ No model mapping or provider found for token counting: {}",
            decision.model_name
        );
        return Err(AppError::ProviderError(format!(
            "No model mapping or provider found for token counting: {}",
            decision.model_name
        )));
    }
}

/// Application error types
#[derive(Debug)]
pub enum AppError {
    RoutingError(String),
    ParseError(String),
    ProviderError(String),
    AuthError(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::RoutingError(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::ParseError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AppError::ProviderError(msg) => (StatusCode::BAD_GATEWAY, msg),
            AppError::AuthError(msg) => (StatusCode::UNAUTHORIZED, msg),
        };

        let body = Json(serde_json::json!({
            "error": {
                "type": "error",
                "message": message
            }
        }));

        (status, body).into_response()
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::RoutingError(msg) => write!(f, "Routing error: {}", msg),
            AppError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            AppError::ProviderError(msg) => write!(f, "Provider error: {}", msg),
            AppError::AuthError(msg) => write!(f, "Auth error: {}", msg),
        }
    }
}

impl std::error::Error for AppError {}

/// Extract and validate Bearer token from Authorization header
///
/// Validates:
/// Returns true if the Authorization header is present and starts with "Bearer ".
/// Used to detect malformed bearer tokens (present but invalid) vs absent header.
fn has_bearer_prefix(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.len() >= 7 && s[..7].eq_ignore_ascii_case("Bearer "))
        .unwrap_or(false)
}

/// - Header exists and starts with "Bearer " (case-insensitive)
/// - Token is non-empty after trimming
/// - Token contains only valid characters (alphanumeric, dash, underscore, dot, tilde, plus, slash, equals)
/// - Token length <= 8192 bytes
///
/// Returns None if header is missing or invalid.
fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            // Case-insensitive Bearer prefix check
            if s.len() < 7 {
                return None;
            }
            if !s[..7].eq_ignore_ascii_case("Bearer ") {
                return None;
            }

            let token = s[7..].trim();

            // Reject empty tokens
            if token.is_empty() {
                return None;
            }

            // Reject tokens exceeding max length (8KB)
            if token.len() > 8192 {
                return None;
            }

            // Validate token contains only safe characters
            // Bearer tokens can contain: alphanumeric, - _ . ~ + / =
            // Reject any control characters or CRLF
            if !token.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '=')
            }) {
                // Log the first rejected character to help diagnose token format issues
                if let Some(bad_char) = token.chars().find(|&c| {
                    !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '=')
                }) {
                    tracing::warn!("🔑 Bearer token rejected: contains disallowed character U+{:04X} — passthrough will not activate", bad_char as u32);
                }
                return None;
            }

            Some(token.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderConfig;

    fn make_configs() -> Vec<ProviderConfig> {
        vec![
            ProviderConfig {
                name: "ant1".to_string(),
                provider_type: "anthropic".to_string(),
                auth_type: Default::default(),
                supported_beta_options: vec![],
                api_key: Some("k".to_string()),
                oauth_provider: None,
                project_id: None,
                location: None,
                base_url: None,
                models: vec![],
                enabled: Some(true),
                rate_limit_rpm: None,
                rate_limit_max_wait_ms: None,
            },
            ProviderConfig {
                name: "nim1".to_string(),
                provider_type: "nvidia-nim".to_string(),
                auth_type: Default::default(),
                supported_beta_options: vec![],
                api_key: Some("k".to_string()),
                oauth_provider: None,
                project_id: None,
                location: None,
                base_url: Some("https://integrate.api.nvidia.com/v1".to_string()),
                models: vec![],
                enabled: Some(true),
                rate_limit_rpm: None,
                rate_limit_max_wait_ms: None,
            },
            ProviderConfig {
                name: "oai1".to_string(),
                provider_type: "openai".to_string(),
                auth_type: Default::default(),
                supported_beta_options: vec![],
                api_key: Some("k".to_string()),
                oauth_provider: None,
                project_id: None,
                location: None,
                base_url: None,
                models: vec![],
                enabled: Some(true),
                rate_limit_rpm: None,
                rate_limit_max_wait_ms: None,
            },
            ProviderConfig {
                name: "ant-pt".to_string(),
                provider_type: "anthropic".to_string(),
                auth_type: crate::providers::AuthType::Passthrough,
                supported_beta_options: vec![],
                api_key: None,
                oauth_provider: None,
                project_id: None,
                location: None,
                base_url: None,
                models: vec![],
                enabled: Some(true),
                rate_limit_rpm: None,
                rate_limit_max_wait_ms: None,
            },
            ProviderConfig {
                name: "cop1".to_string(),
                provider_type: "copilot".to_string(),
                auth_type: crate::providers::AuthType::OAuth,
                supported_beta_options: vec![],
                api_key: None,
                oauth_provider: Some("copilot".to_string()),
                project_id: None,
                location: None,
                base_url: None,
                models: vec![],
                enabled: Some(true),
                rate_limit_rpm: None,
                rate_limit_max_wait_ms: None,
            },
        ]
    }

    #[test]
    fn test_should_use_passthrough_auth_returns_true_for_anthropic_passthrough() {
        let configs = make_configs();
        assert!(should_use_passthrough_auth(&configs, "ant-pt"));
    }

    #[test]
    fn test_should_use_passthrough_auth_returns_false_for_anthropic_apikey() {
        let configs = make_configs();
        assert!(!should_use_passthrough_auth(&configs, "ant1"));
    }

    #[test]
    fn test_should_use_passthrough_auth_returns_false_for_nvidia_nim() {
        let configs = make_configs();
        assert!(!should_use_passthrough_auth(&configs, "nim1"));
    }

    #[test]
    fn test_should_use_passthrough_auth_returns_false_for_copilot() {
        let configs = make_configs();
        assert!(!should_use_passthrough_auth(&configs, "cop1"));
    }

    #[test]
    fn test_should_use_passthrough_auth_returns_false_for_openai() {
        let configs = make_configs();
        assert!(!should_use_passthrough_auth(&configs, "oai1"));
    }

    #[test]
    fn test_should_use_passthrough_auth_returns_false_for_unknown() {
        let configs = make_configs();
        assert!(!should_use_passthrough_auth(&configs, "unknown"));
    }

    #[test]
    fn test_should_use_passthrough_auth_returns_false_for_anthropic_oauth() {
        let configs = vec![ProviderConfig {
            name: "ant-oauth".to_string(),
            provider_type: "anthropic".to_string(),
            auth_type: crate::providers::AuthType::OAuth,
            supported_beta_options: vec![],
            api_key: None,
            oauth_provider: Some("anthropic".to_string()),
            project_id: None,
            location: None,
            base_url: None,
            models: vec![],
            enabled: Some(true),
            rate_limit_rpm: None,
            rate_limit_max_wait_ms: None,
        }];
        assert!(!should_use_passthrough_auth(&configs, "ant-oauth"));
    }

    #[test]
    fn test_passthrough_auth_set_per_mapping_not_filtered() {
        // Verifies the fix: all providers stay in the fallback list and passthrough_auth
        // is set per-mapping. Under the old code, copilot was filtered out of
        // sorted_mappings entirely when a passthrough token was present.
        let configs = make_configs();
        let token = Some("caller-token".to_string());

        // anthropic+Passthrough: receives the caller's bearer token
        let ant_pt_auth = if should_use_passthrough_auth(&configs, "ant-pt") {
            token.clone()
        } else {
            None
        };

        // copilot: stays in the fallback list but gets None (uses its own OAuth)
        let cop1_auth = if should_use_passthrough_auth(&configs, "cop1") {
            token.clone()
        } else {
            None
        };

        assert_eq!(ant_pt_auth, token);
        assert_eq!(cop1_auth, None);
    }

    #[test]
    fn test_extract_bearer_token_with_valid_token() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer valid-token-123".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, Some("valid-token-123".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_case_insensitive() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "bearer valid-token-456".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, Some("valid-token-456".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_mixed_case() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "BeArEr valid-token-789".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, Some("valid-token-789".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_missing_header() {
        let headers = axum::http::HeaderMap::new();
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, None);
    }

    #[test]
    fn test_extract_bearer_token_empty_value() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer ".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, None);
    }

    #[test]
    fn test_extract_bearer_token_whitespace_only() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer    ".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, None);
    }

    #[test]
    fn test_extract_bearer_token_trims_whitespace() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer   valid-token   ".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, Some("valid-token".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_rejects_invalid_prefix() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Basic dXNlcjpwYXNz".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, None);
    }

    #[test]
    fn test_extract_bearer_token_with_special_chars() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert!(token.is_some());
    }

    // Note: Tests for control characters (newline, null byte, CRLF) are not included
    // because axum::http::HeaderValue::parse() itself rejects these at the HTTP library level.
    // Our validator adds defense-in-depth at the application level in case values bypass
    // the HTTP parser (e.g., from direct function calls with unsanitized input).

    #[test]
    fn test_extract_bearer_token_rejects_excessive_length() {
        let mut headers = axum::http::HeaderMap::new();
        let long_token = "a".repeat(8193);
        let auth_header = format!("Bearer {}", long_token);
        headers.insert(
            axum::http::header::AUTHORIZATION,
            auth_header.parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, None);
    }

    #[test]
    fn test_extract_bearer_token_accepts_max_length() {
        let mut headers = axum::http::HeaderMap::new();
        let long_token = "a".repeat(8192);
        let auth_header = format!("Bearer {}", long_token);
        headers.insert(
            axum::http::header::AUTHORIZATION,
            auth_header.parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert!(token.is_some());
    }

    #[test]
    fn test_extract_bearer_token_rejects_invalid_chars() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer token<script>alert(1)</script>".parse().unwrap(),
        );
        let token = super::extract_bearer_token(&headers);
        assert_eq!(token, None);
    }

    #[test]
    fn test_redact_provider_api_keys_with_key_set() {
        let providers = serde_json::json!([
            {"name": "test", "api_key": "secret123"}
        ]);
        let result = redact_provider_api_keys(&providers);
        let obj = &result[0];
        assert!(obj.get("api_key").is_none(), "api_key should be removed");
        assert_eq!(obj["api_key_set"], serde_json::Value::Bool(true));
    }

    #[test]
    fn test_redact_provider_api_keys_with_empty_key() {
        let providers = serde_json::json!([
            {"name": "test", "api_key": ""}
        ]);
        let result = redact_provider_api_keys(&providers);
        assert_eq!(result[0]["api_key_set"], serde_json::Value::Bool(false));
    }

    #[test]
    fn test_redact_provider_api_keys_with_null_key() {
        let providers = serde_json::json!([
            {"name": "test", "api_key": null}
        ]);
        let result = redact_provider_api_keys(&providers);
        assert_eq!(result[0]["api_key_set"], serde_json::Value::Bool(false));
    }

    #[test]
    fn test_redact_provider_api_keys_no_key_field() {
        let providers = serde_json::json!([
            {"name": "test"}
        ]);
        let result = redact_provider_api_keys(&providers);
        // No api_key field, so no api_key_set field added
        assert!(result[0].get("api_key").is_none());
        assert!(result[0].get("api_key_set").is_none());
    }

    #[test]
    fn test_redact_provider_api_keys_multiple_providers() {
        let providers = serde_json::json!([
            {"name": "p1", "api_key": "secret1"},
            {"name": "p2", "api_key": ""},
            {"name": "p3"},
        ]);
        let result = redact_provider_api_keys(&providers);
        assert_eq!(result[0]["api_key_set"], serde_json::Value::Bool(true));
        assert_eq!(result[1]["api_key_set"], serde_json::Value::Bool(false));
        assert!(result[2].get("api_key_set").is_none());
    }

    #[test]
    fn test_redact_provider_api_keys_non_array_input_unchanged() {
        let non_array = serde_json::json!({"api_key": "secret"});
        let result = redact_provider_api_keys(&non_array);
        // Non-array input passes through unchanged
        assert_eq!(result["api_key"], "secret");
    }

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn test_constant_time_eq_not_equal() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"much_longer"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_build_refreshed_copilot_token_preserves_enterprise_url() {
        let original = crate::auth::OAuthToken {
            provider_id: "copilot-ent".to_string(),
            access_token: "old-bearer".to_string(),
            refresh_token: "github-pat".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            enterprise_url: Some("https://my-org.copilot.github.com".to_string()),
            project_id: None,
        };
        let refreshed = build_refreshed_copilot_token(
            &original,
            "new-bearer".to_string(),
            (chrono::Utc::now() + chrono::Duration::minutes(30)).timestamp() as u64,
        );
        assert_eq!(refreshed.enterprise_url, original.enterprise_url);
        assert_eq!(refreshed.provider_id, original.provider_id);
        assert_eq!(refreshed.refresh_token, original.refresh_token);
        assert_eq!(refreshed.access_token, "new-bearer");
    }

    #[test]
    fn test_build_refreshed_copilot_token_preserves_project_id() {
        let original = crate::auth::OAuthToken {
            provider_id: "gemini-dev".to_string(),
            access_token: "old-token".to_string(),
            refresh_token: "refresh-tok".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            enterprise_url: None,
            project_id: Some("my-gcp-project-123".to_string()),
        };
        let refreshed = build_refreshed_copilot_token(
            &original,
            "new-token".to_string(),
            (chrono::Utc::now() + chrono::Duration::minutes(30)).timestamp() as u64,
        );
        assert_eq!(refreshed.project_id, original.project_id);
        assert_eq!(refreshed.enterprise_url, None);
    }

    #[test]
    fn test_copilot_token_needs_background_refresh_near_expiry() {
        // Token expires in 20 min — below the 25-min threshold → should refresh
        let token = crate::auth::OAuthToken {
            provider_id: "copilot".to_string(),
            access_token: "bearer".to_string(),
            refresh_token: "github-pat".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(20),
            enterprise_url: None,
            project_id: None,
        };
        assert!(copilot_token_needs_background_refresh(&token));
    }

    #[test]
    fn test_copilot_token_needs_background_refresh_fresh_token() {
        // Token expires in 60 min — well above the 25-min threshold → should not refresh
        let token = crate::auth::OAuthToken {
            provider_id: "copilot".to_string(),
            access_token: "bearer".to_string(),
            refresh_token: "github-pat".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(60),
            enterprise_url: None,
            project_id: None,
        };
        assert!(!copilot_token_needs_background_refresh(&token));
    }

    #[test]
    fn test_copilot_token_needs_background_refresh_already_expired() {
        // Token already expired → must refresh
        let token = crate::auth::OAuthToken {
            provider_id: "copilot".to_string(),
            access_token: "bearer".to_string(),
            refresh_token: "github-pat".to_string(),
            expires_at: chrono::Utc::now() - chrono::Duration::minutes(5),
            enterprise_url: None,
            project_id: None,
        };
        assert!(copilot_token_needs_background_refresh(&token));
    }

    #[test]
    fn test_cooldown_for_4xx_returns_60s_for_401() {
        let e = crate::providers::error::ProviderError::ApiError {
            status: 401,
            message: "Unauthorized".into(),
        };
        assert_eq!(cooldown_for_4xx(&e), Some(std::time::Duration::from_secs(60)));
    }

    #[test]
    fn test_cooldown_for_4xx_returns_30s_for_429() {
        let e = crate::providers::error::ProviderError::ApiError {
            status: 429,
            message: "Too Many Requests".into(),
        };
        assert_eq!(cooldown_for_4xx(&e), Some(std::time::Duration::from_secs(30)));
    }

    #[test]
    fn test_cooldown_for_4xx_returns_none_for_500() {
        let e = crate::providers::error::ProviderError::ApiError {
            status: 500,
            message: "Internal Server Error".into(),
        };
        assert_eq!(cooldown_for_4xx(&e), None);
    }

    #[test]
    fn test_cooldown_for_4xx_returns_none_for_non_api_error() {
        let e = crate::providers::error::ProviderError::AuthError("token missing".into());
        assert_eq!(cooldown_for_4xx(&e), None);
    }

    #[test]
    fn test_is_on_cooldown_false_when_map_is_empty() {
        let cooldowns: dashmap::DashMap<String, std::time::Instant> = dashmap::DashMap::new();
        assert!(!is_on_cooldown(&cooldowns, "my-provider"));
    }

    #[test]
    fn test_is_on_cooldown_true_when_cooldown_is_active() {
        let cooldowns: dashmap::DashMap<String, std::time::Instant> = dashmap::DashMap::new();
        cooldowns.insert(
            "my-provider".to_string(),
            std::time::Instant::now() + std::time::Duration::from_secs(60),
        );
        assert!(is_on_cooldown(&cooldowns, "my-provider"));
    }
}
