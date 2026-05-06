mod openai_compat;
mod oauth_handlers;

use crate::cli::AppConfig;
use crate::models::AnthropicRequest;
use crate::router::Router;
use crate::providers::ProviderRegistry;
use crate::auth::TokenStore;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        Html, IntoResponse, Response, sse::{Event, Sse},
    },
    routing::{get, post},
    Form, Json, Router as AxumRouter,
};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};
use futures::stream::StreamExt;

/// Application state shared across handlers
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub router: Router,
    pub provider_registry: Arc<ProviderRegistry>,
    pub token_store: TokenStore,
    pub config_path: std::path::PathBuf,
}

/// Start the HTTP server
pub async fn start_server(config: AppConfig, config_path: std::path::PathBuf) -> anyhow::Result<()> {
    let router = Router::new(config.clone());

    // Initialize OAuth token store FIRST (needed by provider registry)
    let token_store = TokenStore::default()
        .map_err(|e| anyhow::anyhow!("Failed to initialize token store: {}", e))?;

    let existing_tokens = token_store.list_providers();
    if !existing_tokens.is_empty() {
        info!("🔐 Loaded {} OAuth tokens from storage", existing_tokens.len());
    }

    // Initialize provider registry from config (with token store)
    let provider_registry = Arc::new(
        ProviderRegistry::from_configs(&config.providers, Some(token_store.clone()))
            .map_err(|e| anyhow::anyhow!("Failed to initialize provider registry: {}", e))?
    );

    info!("📦 Loaded {} providers with {} models",
        provider_registry.list_providers().len(),
        provider_registry.list_models().len()
    );

    let state = Arc::new(AppState {
        config: config.clone(),
        router,
        provider_registry,
        token_store,
        config_path,
    });

    // Build router
    let app = AxumRouter::new()
        .route("/", get(serve_admin))
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_count_tokens))
        .route("/v1/chat/completions", post(handle_openai_chat_completions))
        .route("/health", get(health_check))
        .route("/api/models", get(get_models))
        .route("/api/providers", get(get_providers))
        .route("/api/models-config", get(get_models_config))
        .route("/api/config", get(get_config))
        .route("/api/config", post(update_config))
        .route("/api/config/json", get(get_config_json))
        .route("/api/config/json", post(update_config_json))
        .route("/api/restart", post(restart_server))
        // OAuth endpoints
        .route("/api/oauth/authorize", post(oauth_handlers::oauth_authorize))
        .route("/api/oauth/exchange", post(oauth_handlers::oauth_exchange))
        .route("/api/oauth/callback", get(oauth_handlers::oauth_callback))
        .route("/auth/callback", get(oauth_handlers::oauth_callback))  // OpenAI Codex uses this path
        .route("/api/oauth/tokens", get(oauth_handlers::oauth_list_tokens))
        .route("/api/oauth/tokens/delete", post(oauth_handlers::oauth_delete_token))
        .route("/api/oauth/tokens/refresh", post(oauth_handlers::oauth_refresh_token));

    // Clone state before moving it
    let oauth_state = state.clone();
    let app = app.with_state(state);

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
                error!("⚠️  Failed to bind OAuth callback server on {}: {}", oauth_addr, e);
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
async fn get_models(State(_state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::ParseError("This endpoint has been removed. Use /api/models-config instead.".to_string()))
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
        router.insert("default".to_string(), toml::Value::String(update.default_model));

        if let Some(bg) = update.background_model {
            router.insert("background".to_string(), toml::Value::String(bg));
        }

        if let Some(think) = update.think_model {
            router.insert("think".to_string(), toml::Value::String(think));
        }

        if let Some(ws) = update.websearch_model {
            router.insert("websearch".to_string(), toml::Value::String(ws));
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

/// Get providers configuration
async fn get_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.config.providers.clone())
}

/// Get models configuration
async fn get_models_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.config.models.clone())
}

/// Get full configuration as JSON (for admin UI)
async fn get_config_json(State(state): State<Arc<AppState>>) -> impl IntoResponse {
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
        },
        "providers": state.config.providers,
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
    if let Some(providers) = new_config.get("providers") {
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
                    router_table.insert("websearch".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(bg) = router.get("background") {
                if let Some(s) = bg.as_str() {
                    router_table.insert("background".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(auto_map) = router.get("auto_map_regex") {
                if let Some(s) = auto_map.as_str() {
                    router_table.insert("auto_map_regex".to_string(), toml::Value::String(s.to_string()));
                }
            }
            if let Some(bg_regex) = router.get("background_regex") {
                if let Some(s) = bg_regex.as_str() {
                    router_table.insert("background_regex".to_string(), toml::Value::String(s.to_string()));
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
    use std::process::Command;
    use std::fs;

    // Get current executable path and PID
    let exe_path = std::env::current_exe()?;
    let current_pid = std::process::id();

    info!("Creating restart script for PID: {} on port: {}", current_pid, port);

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
{} start --port {} > /dev/null 2>&1 &
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

/// Returns true if the named provider has provider_type == "anthropic".
fn is_anthropic_provider(providers: &[crate::providers::ProviderConfig], name: &str) -> bool {
    providers
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.provider_type == "anthropic")
        .unwrap_or(false)
}

/// Handle /v1/chat/completions requests (OpenAI-compatible endpoint)
async fn handle_openai_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(openai_request): Json<openai_compat::OpenAIRequest>,
) -> Result<Response, AppError> {
    let model = openai_request.model.clone();
    info!("Received OpenAI-compatible request for model: {}", model);

    // Extract and validate bearer token for passthrough mode
    let passthrough_token = extract_bearer_token(&headers);

    if passthrough_token.is_some() {
        info!("🔑 Passthrough mode detected (caller-provided bearer token)");
    }

    // 1. Transform OpenAI request to Anthropic format
    let mut anthropic_request = openai_compat::transform_openai_to_anthropic(openai_request)
        .map_err(|e| AppError::ParseError(format!("Failed to transform OpenAI request: {}", e)))?;

    anthropic_request.passthrough_auth = passthrough_token.clone();

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
    if let Some(model_config) = state.config.models.iter().find(|m| m.name == decision.model_name) {
        info!("📋 Found {} provider mappings for model: {}", model_config.mappings.len(), decision.model_name);

        // Check for X-Provider header to override priority
        let forced_provider = headers
            .get("x-provider")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if let Some(ref provider_name) = forced_provider {
            info!("🎯 Using forced provider from X-Provider header: {}", provider_name);
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

        if passthrough_token.is_some() {
            sorted_mappings.retain(|m| is_anthropic_provider(&state.config.providers, &m.provider));
            if sorted_mappings.is_empty() {
                return Err(AppError::RoutingError(
                    "No anthropic-type provider mappings available for passthrough request".to_string()
                ));
            }
        }

        // Try each mapping in priority order (or just the forced one)
        for (idx, mapping) in sorted_mappings.iter().enumerate() {
            info!(
                "🔄 Trying mapping {}/{}: provider={}, actual_model={}",
                idx + 1,
                sorted_mappings.len(),
                mapping.provider,
                mapping.actual_model
            );

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Update model to actual model name
                anthropic_request.model = mapping.actual_model.clone();

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
                        info!("⚠️ Provider {} failed: {}, trying next fallback", mapping.provider, e);
                        continue;
                    }
                }
            } else {
                info!("⚠️ Provider {} not found in registry, trying next fallback", mapping.provider);
                continue;
            }
        }

        error!("❌ All provider mappings failed for model: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for model: {}",
            sorted_mappings.len(),
            decision.model_name
        )));
    } else {
        if passthrough_token.is_some() {
            return Err(AppError::RoutingError(
                "Passthrough auth requires explicit [[models]] configuration".to_string()
            ));
        }

        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state.provider_registry.get_provider_for_model(&decision.model_name) {
            info!("📦 Using provider from registry (direct lookup): {}", decision.model_name);

            // Update model to routed model
            anthropic_request.model = decision.model_name.clone();

            // Call provider
            let anthropic_response = provider.send_message(anthropic_request)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            // Transform to OpenAI format
            let openai_response = openai_compat::transform_anthropic_to_openai(
                anthropic_response,
                model,
            );

            return Ok(Json(openai_response).into_response());
        }

        error!("❌ No model mapping or provider found for model: {}", decision.model_name);
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
        tracing::debug!("📥 Incoming request body:\n{}", json_str);
    }

    // Extract and validate bearer token for passthrough mode
    let passthrough_token = extract_bearer_token(&headers);

    if passthrough_token.is_some() {
        info!("🔑 Passthrough mode detected (caller-provided bearer token)");
    }

    // 1. Parse request for routing decision (mutable for tag extraction)
    let mut request_for_routing: AnthropicRequest = serde_json::from_value(request_json.clone())
        .map_err(|e| {
            tracing::error!("❌ Failed to parse request: {}", e);
            AppError::ParseError(format!("Invalid request format: {}", e))
        })?;

    request_for_routing.passthrough_auth = passthrough_token.clone();

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
    if let Some(model_config) = state.config.models.iter().find(|m| m.name == decision.model_name) {
        info!("📋 Found {} provider mappings for model: {}", model_config.mappings.len(), decision.model_name);

        // Check for X-Provider header to override priority
        let forced_provider = headers
            .get("x-provider")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())  // Ignore empty strings
            .map(|s| s.to_string());

        if let Some(ref provider_name) = forced_provider {
            info!("🎯 Using forced provider from X-Provider header: {}", provider_name);
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

        // In passthrough mode, restrict to anthropic-type providers only
        if passthrough_token.is_some() {
            sorted_mappings.retain(|m| is_anthropic_provider(&state.config.providers, &m.provider));
            if sorted_mappings.is_empty() {
                return Err(AppError::RoutingError(
                    "No anthropic-type provider mappings available for passthrough request".to_string()
                ));
            }
        }

        // Try each mapping in priority order (or just the forced one)
        for (idx, mapping) in sorted_mappings.iter().enumerate() {
            info!(
                "🔄 Trying mapping {}/{}: provider={}, actual_model={}",
                idx + 1,
                sorted_mappings.len(),
                mapping.provider,
                mapping.actual_model
            );

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Trust the model mapping configuration - no need to validate

                // Parse request as Anthropic format
                let mut anthropic_request: AnthropicRequest = serde_json::from_value(request_json.clone())
                    .map_err(|e| AppError::ParseError(format!("Invalid request format: {}", e)))?;

                // Save original model name for response
                let original_model = anthropic_request.model.clone();

                // Update model to actual model name
                anthropic_request.model = mapping.actual_model.clone();

                // Update system if modified during routing
                anthropic_request.system = request_for_routing.system.clone();

                // Propagate passthrough auth into per-provider request
                anthropic_request.passthrough_auth = passthrough_token.clone();

                if passthrough_token.is_some() {
                    info!("🔑 Passthrough auth active: original_model={}, target_provider={}", original_model, mapping.provider);
                }

                // Check if streaming is requested
                let is_streaming = anthropic_request.stream == Some(true);

                if is_streaming {
                    // Streaming request
                    info!("🌊 Streaming request to provider: {}", mapping.provider);

                    match provider.send_message_stream(anthropic_request).await {
                        Ok(stream) => {
                            info!("✅ Streaming request started with provider: {}", mapping.provider);

                            // Convert byte stream to SSE response
                            // The provider returns raw bytes (SSE format), we pass them through
                            let sse_stream = stream.map(|result| {
                                result.map(|bytes| {
                                    // Convert bytes to string for SSE event
                                    let data = String::from_utf8_lossy(&bytes).to_string();
                                    Event::default().data(data)
                                }).map_err(|e| {
                                    error!("Stream error: {}", e);
                                    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                                })
                            });

                            return Ok(Sse::new(sse_stream).into_response());
                        }
                        Err(e) => {
                            info!("⚠️ Provider {} streaming failed: {}, trying next fallback", mapping.provider, e);
                            continue;
                        }
                    }
                } else {
                    // Non-streaming request (original behavior)
                    match provider.send_message(anthropic_request).await {
                        Ok(mut response) => {
                            // Restore original model name in response
                            response.model = original_model;
                            info!("✅ Request succeeded with provider: {}, response model: {}", mapping.provider, response.model);
                            return Ok(Json(response).into_response());
                        }
                        Err(e) => {
                            info!("⚠️ Provider {} failed: {}, trying next fallback", mapping.provider, e);
                            continue;
                        }
                    }
                }
            } else {
                info!("⚠️ Provider {} not found in registry, trying next fallback", mapping.provider);
                continue;
            }
        }

        error!("❌ All provider mappings failed for model: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for model: {}",
            sorted_mappings.len(),
            decision.model_name
        )));
    } else {
        // Passthrough requires explicit model mappings to enforce provider-type filtering
        if passthrough_token.is_some() {
            return Err(AppError::RoutingError(
                "Passthrough auth requires explicit [[models]] configuration".to_string()
            ));
        }

        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state.provider_registry.get_provider_for_model(&decision.model_name) {
            info!("📦 Using provider from registry (direct lookup): {}", decision.model_name);

            // Parse request as Anthropic format
            let mut anthropic_request: AnthropicRequest = serde_json::from_value(request_json.clone())
                .map_err(|e| AppError::ParseError(format!("Invalid request format: {}", e)))?;

            // Save original model name for response
            let original_model = anthropic_request.model.clone();

            // Update model to routed model
            anthropic_request.model = decision.model_name.clone();

            // Update system if modified during routing
            anthropic_request.system = request_for_routing.system.clone();

            // Call provider
            let mut provider_response = provider.send_message(anthropic_request)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            // Restore original model name in response
            provider_response.model = original_model;

            // Return provider response
            return Ok(Json(provider_response).into_response());
        }

        error!("❌ No model mapping or provider found for model: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "No model mapping or provider found for model: {}",
            decision.model_name
        )));
    }
}

/// Handle /v1/messages/count_tokens requests
async fn handle_count_tokens(
    State(state): State<Arc<AppState>>,
    Json(request_json): Json<serde_json::Value>,
) -> Result<Response, AppError> {
    let model = request_json.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
    info!("Received count_tokens request for model: {}", model);

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
        passthrough_auth: None,
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
    if let Some(model_config) = state.config.models.iter().find(|m| m.name == decision.model_name) {
        info!("📋 Found {} provider mappings for token counting: {}", model_config.mappings.len(), decision.model_name);

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

            // Try to get provider from registry
            if let Some(provider) = state.provider_registry.get_provider(&mapping.provider) {
                // Trust the model mapping configuration - no need to validate

                // Update model to actual model name
                let mut count_request_for_provider = count_request.clone();
                count_request_for_provider.model = mapping.actual_model.clone();

                // Call provider's count_tokens
                match provider.count_tokens(count_request_for_provider).await {
                    Ok(response) => {
                        info!("✅ Token count succeeded with provider: {}", mapping.provider);
                        return Ok(Json(response).into_response());
                    }
                    Err(e) => {
                        info!("⚠️ Provider {} failed: {}, trying next fallback", mapping.provider, e);
                        continue;
                    }
                }
            } else {
                info!("⚠️ Provider {} not found in registry, trying next fallback", mapping.provider);
                continue;
            }
        }

        error!("❌ All provider mappings failed for token counting: {}", decision.model_name);
        return Err(AppError::ProviderError(format!(
            "All {} provider mappings failed for token counting: {}",
            sorted_mappings.len(),
            decision.model_name
        )));
    } else {
        // No model mapping found, try direct provider registry lookup (backward compatibility)
        if let Ok(provider) = state.provider_registry.get_provider_for_model(&decision.model_name) {
            info!("📦 Using provider from registry (direct lookup) for token counting: {}", decision.model_name);

            // Update model to routed model
            let mut count_request_for_provider = count_request.clone();
            count_request_for_provider.model = decision.model_name.clone();

            // Call provider's count_tokens
            let response = provider.count_tokens(count_request_for_provider)
                .await
                .map_err(|e| AppError::ProviderError(e.to_string()))?;

            info!("✅ Token count completed via provider");
            return Ok(Json(response).into_response());
        }

        error!("❌ No model mapping or provider found for token counting: {}", decision.model_name);
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
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::RoutingError(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::ParseError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AppError::ProviderError(msg) => (StatusCode::BAD_GATEWAY, msg),
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
        }
    }
}

impl std::error::Error for AppError {}

/// Extract and validate Bearer token from Authorization header
/// 
/// Validates:
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
                return None;
            }
            
            Some(token.to_string())
        })
}

#[cfg(test)]
mod tests {
    use crate::providers::ProviderConfig;
    use super::is_anthropic_provider;

    fn make_configs() -> Vec<ProviderConfig> {
        vec![
            ProviderConfig {
                name: "ant1".to_string(),
                provider_type: "anthropic".to_string(),
                auth_type: Default::default(),
                api_key: Some("k".to_string()),
                oauth_provider: None,
                project_id: None,
                location: None,
                base_url: None,
                models: vec![],
                enabled: Some(true),
            },
            ProviderConfig {
                name: "oai1".to_string(),
                provider_type: "openai".to_string(),
                auth_type: Default::default(),
                api_key: Some("k".to_string()),
                oauth_provider: None,
                project_id: None,
                location: None,
                base_url: None,
                models: vec![],
                enabled: Some(true),
            },
        ]
    }

    #[test]
    fn test_is_anthropic_provider_returns_true_for_anthropic_type() {
        let configs = make_configs();
        assert!(is_anthropic_provider(&configs, "ant1"));
    }

    #[test]
    fn test_is_anthropic_provider_returns_false_for_openai_type() {
        let configs = make_configs();
        assert!(!is_anthropic_provider(&configs, "oai1"));
    }

    #[test]
    fn test_is_anthropic_provider_returns_false_for_unknown_name() {
        let configs = make_configs();
        assert!(!is_anthropic_provider(&configs, "unknown"));
    }

    #[test]
    fn test_passthrough_filter_excludes_non_anthropic_mappings() {
        use crate::cli::ModelMapping;

        let configs = make_configs(); // ant1=anthropic, oai1=openai

        let mappings = vec![
            ModelMapping { provider: "ant1".to_string(), actual_model: "claude-opus-4-5".to_string(), priority: 1 },
            ModelMapping { provider: "oai1".to_string(), actual_model: "gpt-4o".to_string(), priority: 2 },
        ];

        let filtered: Vec<_> = mappings
            .into_iter()
            .filter(|m| is_anthropic_provider(&configs, &m.provider))
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider, "ant1");
    }

    #[test]
    fn test_passthrough_filter_empty_when_no_anthropic_mappings() {
        use crate::cli::ModelMapping;

        let configs = make_configs(); // ant1=anthropic, oai1=openai

        let mappings = vec![
            ModelMapping { provider: "oai1".to_string(), actual_model: "gpt-4o".to_string(), priority: 1 },
        ];

        let filtered: Vec<_> = mappings
            .into_iter()
            .filter(|m| is_anthropic_provider(&configs, &m.provider))
            .collect();

        assert!(filtered.is_empty());
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
}
