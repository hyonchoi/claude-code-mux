mod oauth_handlers;
mod openai_compat;

use crate::auth::TokenStore;
use crate::cli::AppConfig;
use crate::models::{AnthropicRequest, ContentBlock, Message, MessageContent};
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

// Background OAuth token refresh timing (Copilot, Gemini, OpenAI, Anthropic).
// Threshold must exceed the poll interval so a freshly-refreshed 30-min token is
// caught at the next poll instead of expiring silently between checks.
const OAUTH_POLL_SECS: u64 = 20 * 60;

/// Returns true when the token will expire before the next background refresh poll.
fn needs_background_refresh(token: &crate::auth::OAuthToken, poll_secs: u64) -> bool {
    let threshold = chrono::Duration::seconds(poll_secs as i64 + 5 * 60);
    let remaining = token.expires_at.signed_duration_since(chrono::Utc::now());
    remaining < threshold
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

/// Returns the cooldown duration when a provider returns a triggering error.
/// 401/403 → 240s, 429 → 120s, 502 (incl. synthetic empty-choices) → 60s, others → None.
fn cooldown_for_4xx(e: &crate::providers::error::ProviderError) -> Option<std::time::Duration> {
    if let crate::providers::error::ProviderError::ApiError { status, .. } = e {
        match *status {
            401 | 403 => Some(std::time::Duration::from_secs(240)),
            429 => Some(std::time::Duration::from_secs(120)),
            502 => Some(std::time::Duration::from_secs(60)),
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

/// Places `provider` on cooldown when the error warrants one (see `cooldown_for_4xx`).
fn apply_cooldown(
    cooldowns: &dashmap::DashMap<String, std::time::Instant>,
    provider: &str,
    e: &crate::providers::error::ProviderError,
) {
    if let Some(duration) = cooldown_for_4xx(e) {
        cooldowns.insert(provider.to_string(), std::time::Instant::now() + duration);
        warn!(
            "⏸ Provider {} on cooldown for {}s",
            provider,
            duration.as_secs()
        );
    }
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

/// Returns true when the Origin header's authority matches the Host header.
/// Origin is "scheme://host[:port]"; Host is "host[:port]". A missing scheme
/// separator means we compare the raw values.
fn origin_matches_host(origin: &str, host: &str) -> bool {
    let origin_authority = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    !host.is_empty() && origin_authority == host
}

/// Returns true when the Host header authority is a loopback name.
///
/// Accepts `host` or `host:port` for IPv4/hostnames (e.g. `127.0.0.1:3000`,
/// `localhost`), bare IPv6 (`::1`), and the bracketed IPv6 form (`[::1]`,
/// `[::1]:3000`). The whole 127.0.0.0/8 range and `::1` count as loopback.
///
/// Loopback IPs are recognised by parsing the authority as an `IpAddr` and
/// calling `is_loopback()` — NOT by a string prefix. A prefix check such as
/// `starts_with("127.")` would also match attacker-controlled names like
/// `127.0.0.1.nip.io` (nip.io/sslip.io resolve such names to 127.0.0.1),
/// reopening the DNS-rebinding bypass this guard exists to close.
fn host_authority_is_loopback(host: &str) -> bool {
    use std::net::{IpAddr, Ipv6Addr};
    let host = host.trim();
    // Bracketed IPv6 literal: [::1] or [::1]:port
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((inner, _)) => inner
                .parse::<Ipv6Addr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false),
            None => false,
        };
    }
    // Bare IP (covers bare IPv6 like `::1` and IPv4 without a port).
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip.is_loopback();
    }
    // Otherwise `host` or `host:port` for IPv4/hostnames.
    let bare = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bare.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// CSRF defense for browser-originated requests on the control plane.
///
/// Two layers:
/// 1. When no `api_key` is configured the control plane trusts the loopback bind
///    for access control. A DNS-rebinding attacker can point an attacker-owned
///    name at 127.0.0.1 and make a browser issue same-origin requests (Origin and
///    Host both the attacker name), so Origin==Host is not enough. We additionally
///    require the `Host` header itself to be a loopback authority, for every
///    method — reads of `/api/config`/`/api/oauth/tokens` can exfiltrate too.
/// 2. On state-changing methods we keep the cross-origin check: a browser
///    cross-site request carries an `Origin` that won't match the proxy's `Host`.
///
/// Non-browser clients (Claude Code, curl, SDKs) send no `Origin` and hit a
/// loopback `Host`, so they are unaffected.
async fn csrf_guard(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    use axum::http::Method;
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Layer 1: unauthenticated control plane must only answer loopback Hosts.
    if control_plane_requires_loopback(host, state.config.server.api_key.as_deref()) {
        warn!(
            "Request rejected: non-loopback Host '{}' on unauthenticated control plane to {} (possible DNS rebinding)",
            host,
            request.uri().path()
        );
        return Err(StatusCode::FORBIDDEN);
    }

    // Layer 2: reject cross-origin state-changing browser requests.
    let is_state_changing = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if is_state_changing {
        if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            if !origin_matches_host(origin, host) {
                warn!(
                    "Request rejected: cross-origin state-changing request to {} (origin={}, host={})",
                    request.uri().path(),
                    origin,
                    host
                );
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }
    Ok(next.run(request).await)
}

/// DNS-rebinding guard for the data plane.
///
/// The data plane authenticates via an explicit `api_key` header, so it is not a
/// CSRF target and — unlike the control plane — gets NO Origin check (a
/// cross-origin browser client with a valid key is legitimate). But when no
/// `api_key` is configured, [`require_api_key`] allows everything, so a
/// DNS-rebinding page could rebind an attacker hostname to 127.0.0.1 and drive
/// `/v1/*` to spend tokens and read model output. Closing that: when no api_key,
/// the `Host` must be a loopback authority — the same predicate the bind-time and
/// control-plane gates use ([`control_plane_requires_loopback`]). When an api_key
/// IS set this is a no-op; the key is the gate and non-loopback Hosts are fine.
async fn data_plane_rebinding_guard(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if control_plane_requires_loopback(host, state.config.server.api_key.as_deref()) {
        warn!(
            "Request rejected: non-loopback Host '{}' on unauthenticated data plane to {} (possible DNS rebinding)",
            host,
            request.uri().path()
        );
        return Err(StatusCode::FORBIDDEN);
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

/// Merge a `<system-reminder>` block into an existing user turn's content.
/// `append=true` places it after the turn's existing content (used when the
/// reminder followed the user turn); `append=false` places it before (used
/// when the reminder preceded the user turn), preserving chronological order.
fn merge_reminder_into_user(content: &mut MessageContent, reminder: ContentBlock, append: bool) {
    match content {
        MessageContent::Text(existing) => {
            let existing_block = ContentBlock::Text {
                text: existing.clone(),
            };
            *content = MessageContent::Blocks(if append {
                vec![existing_block, reminder]
            } else {
                vec![reminder, existing_block]
            });
        }
        MessageContent::Blocks(blocks) => {
            if append {
                blocks.push(reminder);
            } else {
                blocks.insert(0, reminder);
            }
        }
    }
}

/// Extract a `<system-reminder>` block from a mid-conversation `role:"system"`
/// message. Returns `None` when there is no text content to carry (empty or
/// non-text-only), in which case the system message is simply dropped.
fn system_message_to_reminder(msg: &Message) -> Option<ContentBlock> {
    let system_text = match &msg.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Blocks(blocks) => {
            let non_text = blocks.iter().filter(|b| !matches!(b, ContentBlock::Text { .. })).count();
            let text = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if non_text > 0 {
                info!("📝 Mid-conversation system message has {} non-text block(s) — not preserved in the normalized <system-reminder> block", non_text);
            }
            text
        }
    };

    if system_text.trim().is_empty() {
        info!("📝 Dropped mid-conversation system message with empty text content");
        return None;
    }

    let trimmed = system_text.trim();
    // Already a single well-formed reminder -> pass through untouched.
    let already_wrapped = trimmed.starts_with("<system-reminder>")
        && trimmed.ends_with("</system-reminder>")
        && trimmed.matches("</system-reminder>").count() == 1;
    let wrapped = if already_wrapped {
        system_text
    } else {
        // Neutralize any stray closing tag in the content so it can't
        // prematurely terminate the wrapper we are about to add.
        let safe = system_text.replace("</system-reminder>", "<\\/system-reminder>");
        format!("<system-reminder>\n{}\n</system-reminder>", safe)
    };
    Some(ContentBlock::Text { text: wrapped })
}

/// Normalize mid-conversation `role:"system"` messages into user-role
/// `<system-reminder>` blocks. Targets like sonnet-4.6 and non-Anthropic
/// providers reject `role:"system"` inside the `messages` array.
///
/// Each reminder is folded into the user turn it belongs to so the pass never
/// introduces two consecutive same-role turns (which Anthropic-format targets
/// reject with "roles must alternate"):
///   - a system message directly after a user turn -> appended to that turn
///   - otherwise the reminder is buffered and prepended to the NEXT user turn
///   - if the next turn is not a user (assistant, or end of conversation) the
///     buffered reminders become a synthesized user turn there, which keeps
///     alternation valid between assistant turns and at boundaries
fn normalize_mid_conversation_system(request: &mut AnthropicRequest) {
    normalize_mid_conversation_system_messages(&mut request.messages);
}

/// Messages-level normalization shared by `AnthropicRequest` and
/// `CountTokensRequest` (both carry `Vec<Message>`).
fn normalize_mid_conversation_system_messages(messages: &mut Vec<Message>) {
    let original = std::mem::take(messages);
    let mut result: Vec<Message> = Vec::with_capacity(original.len());
    // Reminders from orphan system messages (no preceding user turn to append
    // to), waiting to attach to the next user turn.
    let mut pending: Vec<ContentBlock> = Vec::new();

    for msg in original {
        if msg.role == "system" {
            if let Some(reminder) = system_message_to_reminder(&msg) {
                match result.last_mut() {
                    // Directly after a user turn: append in place.
                    Some(prev) if prev.role == "user" => {
                        merge_reminder_into_user(&mut prev.content, reminder, true);
                    }
                    // No adjacent user turn: defer to the next one.
                    _ => pending.push(reminder),
                }
                info!("📝 Normalized mid-conversation system message into user <system-reminder> block");
            }
            continue;
        }

        if msg.role == "user" && !pending.is_empty() {
            // Prepend buffered reminders (in original order) before this user's
            // content, then flush.
            let mut user_msg = msg;
            for reminder in pending.drain(..).rev() {
                merge_reminder_into_user(&mut user_msg.content, reminder, false);
            }
            result.push(user_msg);
            continue;
        }

        // Upcoming turn is not a user; materialize any buffered reminders as a
        // synthesized user turn so alternation stays valid.
        if !pending.is_empty() {
            result.push(Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(std::mem::take(&mut pending)),
            });
        }
        result.push(msg);
    }

    // Trailing reminders (system messages at the very end).
    if !pending.is_empty() {
        if let Some(last) = result.last_mut() {
            if last.role == "user" {
                for reminder in std::mem::take(&mut pending) {
                    merge_reminder_into_user(&mut last.content, reminder, true);
                }
            }
        }
        if !pending.is_empty() {
            result.push(Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(std::mem::take(&mut pending)),
            });
        }
    }

    *messages = result;
}

async fn refresh_provider_if_needed(
    provider_config: &crate::providers::ProviderConfig,
    token_store: &crate::auth::TokenStore,
    client: &reqwest::Client,
) {
    if provider_config.auth_type != crate::providers::AuthType::OAuth {
        return;
    }
    let provider_id = provider_config
        .oauth_provider
        .clone()
        .unwrap_or_else(|| provider_config.name.clone());
    let token = match token_store.get(&provider_id) {
        Some(t) => t,
        None => return,
    };
    if !needs_background_refresh(&token, OAUTH_POLL_SECS) {
        return;
    }
    match provider_config.provider_type.as_str() {
        "copilot" => {
            match crate::auth::github_copilot::refresh_copilot_token(client, &token.refresh_token)
                .await
            {
                Ok(resp) => {
                    let updated =
                        build_refreshed_copilot_token(&token, resp.token, resp.expires_at);
                    if let Err(e) = token_store.save(updated) {
                        warn!(
                            "Background refresh: failed to save Copilot token for '{}': {}",
                            provider_id, e
                        );
                    } else {
                        info!(
                            "Background refresh: renewed Copilot bearer for '{}'",
                            provider_id
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Background refresh: failed to renew Copilot bearer for '{}': {}",
                        provider_id, e
                    );
                }
            }
        }
        "gemini" | "openai" | "anthropic" => {
            let oauth_config = match provider_config.provider_type.as_str() {
                "gemini" => crate::auth::OAuthConfig::gemini(),
                "openai" => crate::auth::OAuthConfig::openai_codex(),
                _ => crate::auth::OAuthConfig::anthropic(),
            };
            let oauth_client = crate::auth::OAuthClient::with_client(
                oauth_config,
                token_store.clone(),
                client.clone(),
            );
            match oauth_client.refresh_token(&provider_id).await {
                Ok(_) => {
                    info!(
                        "Background refresh: renewed {} OAuth token for '{}'",
                        provider_config.provider_type, provider_id
                    );
                }
                Err(e) => {
                    warn!(
                        "Background refresh: failed to renew {} OAuth token for '{}': {}",
                        provider_config.provider_type, provider_id, e
                    );
                }
            }
        }
        _ => {}
    }
}

/// Treat a blank or whitespace-only `server.api_key` as unset.
///
/// An empty string is `Some("")`, which would defeat every `api_key.is_none()`
/// control-plane gate (bind guard + CSRF Layer 1) while `require_api_key`'s
/// `constant_time_eq("", "")` authorizes every request — serving the control
/// plane unauthenticated on all interfaces. Operators hit this by templating an
/// empty env var into `api_key`. Normalizing once, before the config is shared,
/// makes all three gates agree and fail closed (loopback-only).
fn normalize_api_key(api_key: Option<String>) -> Option<String> {
    match api_key {
        Some(k) if k.trim().is_empty() => None,
        other => other,
    }
}

/// The control plane trusts the loopback bind for access control only when no
/// `api_key` is configured. Returns true when this bind/request must therefore
/// be confined to a loopback authority. Shared verbatim by the bind-time gate
/// ([`control_plane_bind_guard`]) and the request-time gate ([`csrf_guard`]) so
/// the api_key-coupling AND the loopback definition stay in lockstep — neither
/// half can drift between the two sites.
fn control_plane_requires_loopback(host: &str, api_key: Option<&str>) -> bool {
    api_key.is_none() && !host_authority_is_loopback(host)
}

/// Bind-time guard for the control plane.
///
/// Returns `Err(explanation)` when binding to `host` without an `api_key` would
/// expose the unauthenticated admin/control API on a non-loopback address.
/// Uses [`control_plane_requires_loopback`], the same predicate [`csrf_guard`]
/// enforces at request time, so the bind-time and request-time gates cannot
/// silently diverge.
fn control_plane_bind_guard(host: &str, api_key: Option<&str>) -> Result<(), String> {
    if control_plane_requires_loopback(host, api_key) {
        return Err(format!(
            "Refusing to bind to non-loopback host '{}' without server.api_key set — \
             the admin/control API would be exposed unauthenticated. \
             Set server.api_key in your config, or bind to 127.0.0.1.",
            host
        ));
    }
    Ok(())
}

/// Start the HTTP server
pub async fn start_server(
    config: AppConfig,
    config_path: std::path::PathBuf,
) -> anyhow::Result<()> {
    // Normalize a blank/whitespace-only api_key to None BEFORE any gate reads it,
    // so the bind guard, csrf_guard, and require_api_key all agree. An empty
    // string would otherwise be Some("") — passing every is_none() gate while
    // constant_time_eq("","") authorizes all requests, fully opening the control
    // plane. AppState is built from this config below, so normalizing here covers
    // the request-time gates too.
    let mut config = config;
    if config
        .server
        .api_key
        .as_deref()
        .is_some_and(|k| k.trim().is_empty())
    {
        warn!(
            "server.api_key is set but empty/whitespace — treating as UNSET (loopback-only). \
             Set a non-empty key to authenticate the control plane on a shared address."
        );
    }
    config.server.api_key = normalize_api_key(config.server.api_key);

    // Security: refuse to expose an unauthenticated control plane on a non-loopback
    // address. Without an api_key every /api/* route (config rewrite, OAuth token
    // delete/refresh, restart) is open to anyone who can reach the port.
    // Reuse the same loopback definition as csrf_guard so the bind-time gate and
    // the request-time gate cannot silently diverge.
    control_plane_bind_guard(&config.server.host, config.server.api_key.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;
    if config.server.api_key.is_none() {
        warn!(
            "⚠️  No server.api_key configured — the control plane is UNAUTHENTICATED (loopback only). \
             Set server.api_key before binding to a non-loopback address or sharing this machine."
        );
    }

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

    // Background task: proactively refresh OAuth bearer tokens every 20 minutes.
    // Covers Copilot (~30-min TTL), Gemini, OpenAI, and Anthropic OAuth providers.
    // Without this, idle providers never get refreshed and require full re-OAuth when re-enabled.
    {
        let bg_token_store = state.token_store.clone();
        let bg_providers = state.config.providers.clone();
        let bg_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build background refresh HTTP client");
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(OAUTH_POLL_SECS));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                for provider_config in &bg_providers {
                    refresh_provider_if_needed(provider_config, &bg_token_store, &bg_client).await;
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

    // Data-plane routes: the LLM proxy endpoints. Authenticated by an explicit
    // `api_key` header (not ambient cookies), so they are NOT a CSRF target and
    // get no Origin check — a cross-origin browser client with a valid api_key,
    // or a non-browser client reaching a non-loopback Host, must not be 403'd.
    // They DO get the DNS-rebinding guard: when no api_key is configured,
    // require_api_key allows everything, so without it a rebinding page could
    // drive /v1/* to spend tokens and exfiltrate model output via a loopback bind.
    let data_plane_routes = AxumRouter::new()
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_count_tokens))
        .route("/v1/chat/completions", post(handle_openai_chat_completions))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            data_plane_rebinding_guard,
        ));

    // Control-plane routes: config rewrite, OAuth token management, restart.
    // These trust the loopback bind for access control when no api_key is set, so
    // they get the full CSRF + DNS-rebinding guard in addition to require_api_key.
    let control_plane_routes = AxumRouter::new()
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
        ))
        // CSRF defense runs first (outermost): reject cross-origin browser writes
        // and non-loopback Hosts (DNS rebinding) before the api_key check, so a
        // malicious page can't drive the control plane even when no api_key is set.
        .layer(middleware::from_fn_with_state(state.clone(), csrf_guard));

    let app = public_routes
        .merge(data_plane_routes)
        .merge(control_plane_routes)
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

    // Create a shell script to handle restart. Forward the config path so the
    // new process boots with the same config the running one used.
    match create_and_execute_restart_script(port, Some(state.config_path.as_path())) {
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

/// POSIX-safe single-quote: wrap `s` in single quotes, escaping any embedded
/// single quote as `'\''`. Without this, an executable or config path containing
/// a `'` would break out of the script's quoting — shell injection in a script
/// the server then executes as itself.
#[cfg(unix)]
fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Create and execute a shell script that waits for shutdown and restarts.
/// `config_path` (when present) is forwarded as `--config` so a UI-triggered
/// restart boots with the same config the running process used, instead of
/// silently falling back to the default config/providers/auth posture.
fn create_and_execute_restart_script(
    port: u16,
    config_path: Option<&std::path::Path>,
) -> std::io::Result<()> {
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
        // Build the start command with shell-safe quoting on every interpolated
        // path, forwarding --config when known so the new process matches the old.
        let exe_q = sh_single_quote(&exe_path.to_string_lossy());
        let config_arg = match config_path {
            Some(p) => format!(" --config {}", sh_single_quote(&p.to_string_lossy())),
            None => String::new(),
        };
        let script_content = format!(
            r#"#!/bin/bash
# Wait for old process to exit
while kill -0 {pid} 2>/dev/null; do
    sleep 0.1
done
# Start new server
{exe} start --port {port}{config_arg} > /dev/null 2>&1 &
"#,
            pid = current_pid,
            exe = exe_q,
            port = port,
            config_arg = config_arg,
        );

        // Write the script into the user-owned config dir, not a world-writable
        // /tmp path. A fixed /tmp/ccm_restart.sh lets any other local user
        // symlink or race-replace the script that we then execute as ourselves.
        // If no home dir resolves, fail closed rather than degrading to a shared
        // temp dir — falling back to std::env::temp_dir() would reopen exactly
        // that predictable-path symlink/TOCTOU vector for the script we exec.
        let dir = dirs::home_dir()
            .map(|h| h.join(".claude-code-mux"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Cannot resolve home directory to write the restart script safely; \
                     refusing to fall back to a world-writable temp path. Restart manually.",
                )
            })?;
        fs::create_dir_all(&dir)?;
        let script_path = dir.join("restart.sh");

        // Create with 0700 (owner-only) from the start; executed via `sh <path>`
        // so no execute bit is required.
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o700)
                .open(&script_path)?;
            file.write_all(script_content.as_bytes())?;
        }

        // Execute script in background
        Command::new("sh")
            .arg(&script_path)
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
tasklist /FI "PID eq {pid}" 2>NUL | find /I /N "ccm.exe">NUL
if "%ERRORLEVEL%"=="0" (
    timeout /t 1 /nobreak > nul
    goto wait
)
start "" "{exe}" start --port {port}{config_arg}
"#,
            pid = current_pid,
            exe = exe_path.display(),
            port = port,
            config_arg = match config_path {
                Some(p) => format!(" --config \"{}\"", p.display()),
                None => String::new(),
            },
        );

        // Write into the user-owned config dir, not a shared temp path. A fixed
        // name in a world-writable %TEMP% lets another local user plant/replace
        // the .bat we then execute as ourselves. Fail closed if no home dir
        // resolves rather than degrading to env::temp_dir() (mirrors the Unix path).
        let dir = dirs::home_dir()
            .map(|h| h.join(".claude-code-mux"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Cannot resolve home directory to write the restart script safely; \
                     refusing to fall back to a world-writable temp path. Restart manually.",
                )
            })?;
        fs::create_dir_all(&dir)?;
        let script_path = dir.join("ccm_restart.bat");
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

        // Save originals to restore for each mapping attempt (normalization mutates messages)
        let original_beta_header = anthropic_request.anthropic_beta_header.clone();
        let original_messages = anthropic_request.messages.clone();

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

                // Restore originals before applying mapping-specific transforms
                anthropic_request.anthropic_beta_header = original_beta_header.clone();
                anthropic_request.messages = original_messages.clone();

                // Strip beta options if configured in the mapping
                strip_beta_options_from_request(
                    &mut anthropic_request,
                    mapping.strip_beta_options,
                    &mapping.strip_specific_beta,
                );

                // Normalize mid-conversation system messages if configured
                if mapping.strip_mid_conversation_system {
                    normalize_mid_conversation_system(&mut anthropic_request);
                }

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
                        apply_cooldown(&state.provider_cooldowns, &mapping.provider, &e);
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

                // Normalize mid-conversation system messages if configured
                // Must run AFTER the system reassignment so the final messages
                // array state is what gets normalized
                if mapping.strip_mid_conversation_system {
                    normalize_mid_conversation_system(&mut anthropic_request);
                }

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
                            apply_cooldown(&state.provider_cooldowns, &mapping.provider, &e);
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
                            apply_cooldown(&state.provider_cooldowns, &mapping.provider, &e);
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

                // Normalize mid-conversation system messages if configured
                // (count_tokens has the same alternation requirement as generation)
                if mapping.strip_mid_conversation_system {
                    normalize_mid_conversation_system_messages(
                        &mut count_request_for_provider.messages,
                    );
                }

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
                        apply_cooldown(&state.provider_cooldowns, &mapping.provider, &e);
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
    use crate::models::{AnthropicRequest, ContentBlock, Message, MessageContent};
    use crate::providers::ProviderConfig;

    #[test]
    fn test_origin_matches_host() {
        // Same-origin admin UI request: allowed.
        assert!(origin_matches_host(
            "http://127.0.0.1:13456",
            "127.0.0.1:13456"
        ));
        assert!(origin_matches_host(
            "https://localhost:8080",
            "localhost:8080"
        ));
        // Cross-site request from a malicious page: rejected.
        assert!(!origin_matches_host(
            "http://evil.example",
            "127.0.0.1:13456"
        ));
        // Port mismatch: rejected.
        assert!(!origin_matches_host(
            "http://127.0.0.1:9999",
            "127.0.0.1:13456"
        ));
        // Empty host: never a match (fail closed).
        assert!(!origin_matches_host("http://127.0.0.1:13456", ""));
        // Scheme-less Origin (no "://"): raw values compared directly.
        assert!(origin_matches_host("127.0.0.1:13456", "127.0.0.1:13456"));
        assert!(!origin_matches_host("evil.example", "127.0.0.1:13456"));
    }

    #[test]
    fn test_host_authority_is_loopback() {
        // Loopback IPv4, with and without port, plus the whole 127.0.0.0/8 range.
        assert!(host_authority_is_loopback("127.0.0.1"));
        assert!(host_authority_is_loopback("127.0.0.1:13456"));
        assert!(host_authority_is_loopback("127.1.2.3:8080"));
        // localhost (case-insensitive) and bracketed IPv6 loopback.
        assert!(host_authority_is_loopback("localhost"));
        assert!(host_authority_is_loopback("LocalHost:3000"));
        assert!(host_authority_is_loopback("[::1]"));
        assert!(host_authority_is_loopback("[::1]:13456"));

        // DNS-rebinding defense: an attacker name that resolves to 127.0.0.1
        // still presents its own Host, which is NOT loopback — must be rejected
        // even though origin_matches_host(attacker, attacker) would be true.
        assert!(!host_authority_is_loopback("attacker.example.com:13456"));
        assert!(!host_authority_is_loopback("attacker.example.com"));
        // Non-loopback IPs and empty host: rejected (fail closed).
        assert!(!host_authority_is_loopback("10.0.0.5:13456"));
        assert!(!host_authority_is_loopback("192.168.1.20"));
        assert!(!host_authority_is_loopback("[2001:db8::1]:13456"));
        assert!(!host_authority_is_loopback(""));
        // A name that merely starts with "127" but isn't the loopback IP.
        assert!(!host_authority_is_loopback("127host.evil.com"));

        // Bare IPv6 loopback (no brackets) is accepted; bare non-loopback IPv6 is not.
        assert!(host_authority_is_loopback("::1"));
        assert!(!host_authority_is_loopback("2001:db8::1"));

        // Regression (DNS-rebinding bypass): names that START WITH "127." but are
        // NOT a loopback IP. nip.io/sslip.io resolve these to 127.0.0.1, so a
        // prefix check like starts_with("127.") would wrongly accept them and
        // reopen the bypass. They must be rejected — the Host is the attacker name.
        assert!(!host_authority_is_loopback("127.0.0.1.nip.io"));
        assert!(!host_authority_is_loopback("127.0.0.1.nip.io:13456"));
        assert!(!host_authority_is_loopback("127.0.0.1.attacker.com"));
    }

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
        assert!(needs_background_refresh(&token, OAUTH_POLL_SECS));
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
        assert!(!needs_background_refresh(&token, OAUTH_POLL_SECS));
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
        assert!(needs_background_refresh(&token, OAUTH_POLL_SECS));
    }

    #[test]
    fn test_cooldown_for_4xx_returns_240s_for_401() {
        let e = crate::providers::error::ProviderError::ApiError {
            status: 401,
            message: "Unauthorized".into(),
        };
        assert_eq!(
            cooldown_for_4xx(&e),
            Some(std::time::Duration::from_secs(240))
        );
    }

    #[test]
    fn test_cooldown_for_4xx_returns_120s_for_429() {
        let e = crate::providers::error::ProviderError::ApiError {
            status: 429,
            message: "Too Many Requests".into(),
        };
        assert_eq!(
            cooldown_for_4xx(&e),
            Some(std::time::Duration::from_secs(120))
        );
    }

    #[test]
    fn test_cooldown_for_4xx_returns_60s_for_502() {
        let e = crate::providers::error::ProviderError::ApiError {
            status: 502,
            message: "provider 'test' returned a response with no choices".into(),
        };
        assert_eq!(
            cooldown_for_4xx(&e),
            Some(std::time::Duration::from_secs(60))
        );
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

    #[test]
    fn test_apply_cooldown_inserts_when_error_warrants() {
        let cooldowns: dashmap::DashMap<String, std::time::Instant> = dashmap::DashMap::new();
        let e = crate::providers::error::ProviderError::ApiError {
            status: 401,
            message: "Unauthorized".into(),
        };
        apply_cooldown(&cooldowns, "my-provider", &e);
        assert!(is_on_cooldown(&cooldowns, "my-provider"));
    }

    #[test]
    fn test_apply_cooldown_noop_when_error_does_not_warrant() {
        let cooldowns: dashmap::DashMap<String, std::time::Instant> = dashmap::DashMap::new();
        let e = crate::providers::error::ProviderError::ApiError {
            status: 500,
            message: "Internal Server Error".into(),
        };
        apply_cooldown(&cooldowns, "my-provider", &e);
        assert!(!is_on_cooldown(&cooldowns, "my-provider"));
        assert!(cooldowns.is_empty());
    }

    #[test]
    fn test_needs_background_refresh_returns_true_when_near_expiry() {
        let token = crate::auth::OAuthToken {
            provider_id: "test".into(),
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
            enterprise_url: None,
            project_id: None,
        };
        assert!(needs_background_refresh(&token, OAUTH_POLL_SECS));
    }

    #[test]
    fn test_needs_background_refresh_returns_false_for_fresh_token() {
        let token = crate::auth::OAuthToken {
            provider_id: "test".into(),
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(2),
            enterprise_url: None,
            project_id: None,
        };
        assert!(!needs_background_refresh(&token, OAUTH_POLL_SECS));
    }

    #[test]
    fn test_needs_background_refresh_returns_true_for_expired_token() {
        let token = crate::auth::OAuthToken {
            provider_id: "test".into(),
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: chrono::Utc::now() - chrono::Duration::minutes(5),
            enterprise_url: None,
            project_id: None,
        };
        assert!(needs_background_refresh(&token, OAUTH_POLL_SECS));
    }

    #[test]
    fn test_normalize_api_key_blank_becomes_none() {
        // The exploit vector: Some("") would pass api_key.is_none() gates as
        // "configured" while authorizing every request. Must collapse to None.
        assert_eq!(normalize_api_key(Some("".into())), None);
        assert_eq!(normalize_api_key(Some("   ".into())), None);
        assert_eq!(normalize_api_key(Some("\t\n".into())), None);
        // Real keys (including ones with surrounding content) are preserved.
        assert_eq!(
            normalize_api_key(Some("secret".into())),
            Some("secret".into())
        );
        assert_eq!(normalize_api_key(None), None);
    }

    #[test]
    fn test_bind_guard_rejects_empty_api_key_on_non_loopback() {
        // A blank api_key normalizes to None, so the bind guard must treat a
        // non-loopback bind as unauthenticated and refuse it.
        let normalized = normalize_api_key(Some("".into()));
        assert!(control_plane_bind_guard("0.0.0.0", normalized.as_deref()).is_err());
    }

    // --- control_plane_bind_guard (start_server bind-time gate) ---------------

    #[test]
    fn test_bind_guard_rejects_non_loopback_without_api_key() {
        // The exact condition start_server refuses: open control plane on a
        // routable address. Must fail closed.
        let err = control_plane_bind_guard("0.0.0.0", None)
            .expect_err("non-loopback bind without api_key must be refused");
        assert!(err.contains("0.0.0.0"));
        assert!(control_plane_bind_guard("192.168.1.10", None).is_err());
        // DNS-rebinding name that resolves to loopback is still its own Host.
        assert!(control_plane_bind_guard("127.0.0.1.nip.io", None).is_err());
    }

    #[test]
    fn test_bind_guard_allows_loopback_without_api_key() {
        assert!(control_plane_bind_guard("127.0.0.1", None).is_ok());
        assert!(control_plane_bind_guard("localhost", None).is_ok());
        assert!(control_plane_bind_guard("::1", None).is_ok());
    }

    #[test]
    fn test_bind_guard_allows_non_loopback_with_api_key() {
        // With an api_key, require_api_key protects the control plane, so a
        // routable bind is permitted.
        assert!(control_plane_bind_guard("0.0.0.0", Some("secret")).is_ok());
    }

    // --- csrf_guard middleware wiring (end-to-end via tower) ------------------

    use crate::cli::ServerConfig;
    use tower::util::ServiceExt; // for `oneshot`

    fn make_app_state(api_key: Option<String>) -> Arc<AppState> {
        let mut config = AppConfig {
            server: ServerConfig::default(),
            router: crate::cli::RouterConfig {
                default: "default.model".to_string(),
                subagent: None,
                background: None,
                think: None,
                websearch: None,
                auto_map_regex: None,
                background_regex: None,
            },
            providers: vec![],
            models: vec![],
        };
        config.server.api_key = api_key;
        let temp = tempfile::TempDir::new().unwrap();
        let token_store = TokenStore::new(temp.path().join("tokens.json")).unwrap();
        // Keep the TempDir alive for the lifetime of the test by leaking it;
        // tests are short-lived processes so this is fine and avoids threading
        // the guard through the returned Arc.
        std::mem::forget(temp);
        Arc::new(AppState {
            config: config.clone(),
            router: Router::new(config),
            provider_registry: Arc::new(ProviderRegistry::new()),
            token_store,
            config_path: std::path::PathBuf::from("/tmp/ccm-test-config.toml"),
            provider_cooldowns: Arc::new(dashmap::DashMap::new()),
        })
    }

    fn csrf_app(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route("/api/config", axum::routing::get(|| async { "ok" }))
            .route("/api/config", axum::routing::post(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(state, csrf_guard))
    }

    async fn run_request(state: Arc<AppState>, req: Request<Body>) -> StatusCode {
        csrf_app(state).oneshot(req).await.unwrap().status()
    }

    #[test]
    fn test_sh_single_quote_escapes_embedded_quote() {
        // Plain path: simple wrap.
        assert_eq!(sh_single_quote("/usr/bin/ccm"), "'/usr/bin/ccm'");
        // A path containing a single quote must not break out of the quoting —
        // ' becomes '\'' so the restart script can't be injected.
        assert_eq!(sh_single_quote("/tmp/a'b"), "'/tmp/a'\\''b'");
    }

    fn dp_app(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route("/v1/messages", axum::routing::post(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state,
                data_plane_rebinding_guard,
            ))
    }

    async fn run_dp_request(state: Arc<AppState>, req: Request<Body>) -> StatusCode {
        dp_app(state).oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn test_data_plane_guard_rejects_non_loopback_unauthenticated() {
        // No api_key + non-loopback Host (DNS rebinding) → 403, closing the
        // token-spend / model-output exfil vector on /v1/*.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header(header::HOST, "attacker.example.com")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run_dp_request(make_app_state(None), req).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn test_data_plane_guard_allows_loopback_unauthenticated() {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header(header::HOST, "127.0.0.1:3456")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run_dp_request(make_app_state(None), req).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn test_data_plane_guard_noop_when_api_key_configured() {
        // With an api_key, the key is the gate — a non-loopback Host is fine, so
        // legitimate cross-origin/remote api_key clients are never 403'd here.
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header(header::HOST, "proxy.internal:3456")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run_dp_request(make_app_state(Some("secret".into())), req).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn test_csrf_guard_layer1_rejects_non_loopback_host_unauthenticated() {
        // No api_key + non-loopback Host (DNS rebinding) → 403, even for a read.
        let req = Request::builder()
            .method("GET")
            .uri("/api/config")
            .header(header::HOST, "attacker.example.com")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run_request(make_app_state(None), req).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn test_csrf_guard_layer1_allows_loopback_host_unauthenticated() {
        // No api_key + loopback Host → pass-through read.
        let req = Request::builder()
            .method("GET")
            .uri("/api/config")
            .header(header::HOST, "127.0.0.1:3456")
            .body(Body::empty())
            .unwrap();
        assert_eq!(run_request(make_app_state(None), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_csrf_guard_layer1_skipped_when_api_key_configured() {
        // With an api_key, layer 1 is skipped — require_api_key (a separate
        // middleware) handles auth, so csrf_guard lets a non-loopback Host pass.
        let req = Request::builder()
            .method("GET")
            .uri("/api/config")
            .header(header::HOST, "proxy.internal:3456")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run_request(make_app_state(Some("secret".into())), req).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn test_csrf_guard_layer2_rejects_cross_origin_state_change() {
        // Loopback Host but a cross-site Origin on a POST → 403.
        let req = Request::builder()
            .method("POST")
            .uri("/api/config")
            .header(header::HOST, "127.0.0.1:3456")
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run_request(make_app_state(None), req).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn test_csrf_guard_layer2_enforced_even_with_api_key() {
        // Layer 2 (cross-origin reject) applies regardless of api_key — an
        // authenticated client is still blocked from cross-site state changes.
        let req = Request::builder()
            .method("POST")
            .uri("/api/config")
            .header(header::HOST, "127.0.0.1:3456")
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run_request(make_app_state(Some("secret".into())), req).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn test_csrf_guard_layer2_allows_same_origin_state_change() {
        // Same-origin admin UI POST → allowed.
        let req = Request::builder()
            .method("POST")
            .uri("/api/config")
            .header(header::HOST, "127.0.0.1:3456")
            .header(header::ORIGIN, "http://127.0.0.1:3456")
            .body(Body::empty())
            .unwrap();
        assert_eq!(run_request(make_app_state(None), req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_csrf_guard_layer2_allows_state_change_without_origin() {
        // Non-browser client (curl/SDK) sends no Origin on a loopback Host → allowed.
        let req = Request::builder()
            .method("POST")
            .uri("/api/config")
            .header(header::HOST, "127.0.0.1:3456")
            .body(Body::empty())
            .unwrap();
        assert_eq!(run_request(make_app_state(None), req).await, StatusCode::OK);
    }

    // --- normalize_mid_conversation_system tests ---

    fn make_req(msgs: Vec<Message>) -> AnthropicRequest {
        AnthropicRequest {
            model: "test".to_string(),
            messages: msgs,
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
    fn test_normalize_single_system_message_merges_into_preceding_user() {
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text("hook context".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "user");
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "hello"));
                if let ContentBlock::Text { text } = &blocks[1] {
                    assert!(text.contains("<system-reminder>"));
                    assert!(text.contains("hook context"));
                    assert!(text.contains("</system-reminder>"));
                } else {
                    panic!("Expected Text block for system-reminder");
                }
            }
            _ => panic!("Expected Blocks content after merge"),
        }
        assert_eq!(req.messages[1].role, "assistant");
    }

    #[test]
    fn test_normalize_system_message_no_preceding_user_prepends_user() {
        let mut req = make_req(vec![
            Message { role: "system".to_string(), content: MessageContent::Text("hook".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "user");
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                if let ContentBlock::Text { text } = &blocks[0] {
                    assert!(text.contains("<system-reminder>"));
                    assert!(text.contains("hook"));
                    assert!(text.contains("</system-reminder>"));
                } else {
                    panic!("Expected Text block");
                }
            }
            _ => panic!("Expected Blocks content"),
        }
    }

    #[test]
    fn test_normalize_multiple_system_messages() {
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text("first hook".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text("second hook".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages.len(), 2);
        let all_text = match &req.messages[0].content {
            MessageContent::Blocks(blocks) => blocks.iter().filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            }).collect::<Vec<_>>().join(" "),
            _ => panic!("Expected Blocks"),
        };
        assert!(all_text.contains("first hook"));
        assert!(all_text.contains("second hook"));
    }

    #[test]
    fn test_normalize_blocks_content_system_message() {
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Blocks(vec![
                ContentBlock::Text { text: "block content".to_string() },
            ]) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages.len(), 2);
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                let reminder_text = blocks.iter().filter_map(|b| match b {
                    ContentBlock::Text { text } if text.contains("<system-reminder>") => Some(text.clone()),
                    _ => None,
                }).collect::<Vec<_>>().join("");
                assert!(reminder_text.contains("block content"));
            }
            _ => panic!("Expected Blocks"),
        }
    }

    #[test]
    fn test_normalize_no_system_messages_is_noop() {
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        let messages_before = req.messages.clone();
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages, messages_before);
    }

    #[test]
    fn test_normalize_already_wrapped_is_idempotent() {
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text(
                "<system-reminder>already wrapped</system-reminder>".to_string()
            ) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        for msg in &req.messages {
            match &msg.content {
                MessageContent::Text(t) => {
                    assert!(!t.contains("<system-reminder><system-reminder>"),
                        "Should not double-wrap: {}", t);
                }
                MessageContent::Blocks(blocks) => {
                    for b in blocks {
                        if let ContentBlock::Text { text } = b {
                            assert!(!text.contains("<system-reminder><system-reminder>"),
                                "Should not double-wrap: {}", text);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_normalize_multiple_leading_system_messages_merge_into_single_user() {
        // Multiple system messages before any user turn must collapse into ONE
        // synthesized user turn, not multiple consecutive user turns.
        let mut req = make_req(vec![
            Message { role: "system".to_string(), content: MessageContent::Text("hook1".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text("hook2".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages.len(), 2, "should be [user, assistant], not multiple user turns");
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.messages[1].role, "assistant");
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2, "both system messages should be blocks in the same user turn");
                for block in blocks {
                    if let ContentBlock::Text { text } = block {
                        assert!(text.contains("<system-reminder>"));
                    } else {
                        panic!("Expected Text blocks");
                    }
                }
                // hook1 comes before hook2
                if let ContentBlock::Text { text } = &blocks[0] { assert!(text.contains("hook1")); }
                if let ContentBlock::Text { text } = &blocks[1] { assert!(text.contains("hook2")); }
            }
            _ => panic!("Expected Blocks content"),
        }
    }

    #[test]
    fn test_normalize_empty_system_message_is_dropped() {
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text("".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages.len(), 2);
        match &req.messages[0].content {
            MessageContent::Text(t) => assert_eq!(t, "hello"),
            MessageContent::Blocks(blocks) => assert_eq!(blocks.len(), 1),
        }
    }

    #[test]
    fn test_normalize_leading_system_then_user_merges_forward_preserves_alternation() {
        // [system, user, assistant] must NOT become [user, user, assistant].
        // The reminder merges forward into the following user turn.
        let mut req = make_req(vec![
            Message { role: "system".to_string(), content: MessageContent::Text("hook ctx".to_string()) },
            Message { role: "user".to_string(), content: MessageContent::Text("real user msg".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("hi".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        let roles: Vec<&str> = req.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"], "must preserve alternation, got {:?}", roles);
        // Reminder precedes the user's real text (chronological order).
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                if let ContentBlock::Text { text } = &blocks[0] {
                    assert!(text.contains("<system-reminder>"));
                    assert!(text.contains("hook ctx"));
                } else {
                    panic!("Expected reminder block first");
                }
                assert!(matches!(&blocks[1], ContentBlock::Text { text } if text == "real user msg"));
            }
            _ => panic!("Expected Blocks content"),
        }
    }

    #[test]
    fn test_normalize_no_two_consecutive_same_role_turns_invariant() {
        // Exhaustive-ish shapes that previously produced consecutive same-role turns.
        let shapes: Vec<Vec<(&str, &str)>> = vec![
            vec![("system", "s"), ("user", "u"), ("assistant", "a")],
            vec![("system", "s"), ("system", "s2"), ("user", "u"), ("assistant", "a")],
            vec![("user", "u"), ("assistant", "a"), ("system", "s"), ("user", "u2")],
            vec![("user", "u"), ("assistant", "a"), ("system", "s"), ("assistant", "a2")],
            vec![("user", "u"), ("system", "s"), ("assistant", "a")],
        ];
        for shape in shapes {
            let msgs = shape.iter().map(|(role, text)| Message {
                role: role.to_string(),
                content: MessageContent::Text(text.to_string()),
            }).collect::<Vec<_>>();
            let mut req = make_req(msgs);
            normalize_mid_conversation_system(&mut req);
            let roles: Vec<&str> = req.messages.iter().map(|m| m.role.as_str()).collect();
            for w in req.messages.windows(2) {
                assert_ne!(w[0].role, w[1].role,
                    "consecutive same-role turns for shape {:?} -> {:?}", shape, roles);
            }
            // No role:system may survive.
            assert!(req.messages.iter().all(|m| m.role != "system"),
                "residual system role for shape {:?}", shape);
        }
    }

    #[test]
    fn test_normalize_system_between_assistants_synthesizes_in_place() {
        // [user, assistant, system, assistant] -> reminder becomes its own user
        // turn between the two assistants, preserving alternation.
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("response".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text("hook".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("follow-up".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        let roles: Vec<&str> = req.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"], "got {:?}", roles);
        // The synthesized user turn (index 2) carries the reminder.
        match &req.messages[2].content {
            MessageContent::Blocks(blocks) => {
                assert!(blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("hook"))));
            }
            _ => panic!("Expected Blocks content for synthesized turn"),
        }
    }

    #[test]
    fn test_normalize_system_after_assistant_merges_into_following_user_no_reorder() {
        // [user, assistant, system, user] -> reminder attaches to the trailing
        // user (its real neighbor), not yanked back before the assistant.
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("first".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("reply".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text("hook".to_string()) },
            Message { role: "user".to_string(), content: MessageContent::Text("second".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        let roles: Vec<&str> = req.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user"], "got {:?}", roles);
        // First user is untouched (no reorder into it).
        assert!(matches!(&req.messages[0].content, MessageContent::Text(t) if t == "first"));
        // Trailing user got the reminder prepended before its text.
        match &req.messages[2].content {
            MessageContent::Blocks(blocks) => {
                assert!(matches!(&blocks[0], ContentBlock::Text { text } if text.contains("hook")));
                assert!(matches!(&blocks[1], ContentBlock::Text { text } if text == "second"));
            }
            _ => panic!("Expected Blocks content for trailing user"),
        }
    }

    #[test]
    fn test_strip_beta_options_still_works() {
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
        ]);
        req.anthropic_beta_header = Some("max-tokens-3-5-sonnet-2024-07-15,other-beta".to_string());
        strip_beta_options_from_request(&mut req, true, &[]);
        assert!(req.anthropic_beta_header.is_none());
    }

    #[test]
    fn test_normalize_system_message_with_non_text_blocks_drops_image() {
        // Non-text blocks (e.g. image) in a system message are silently dropped;
        // only text content survives in the <system-reminder>.
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("hello".to_string()) },
            Message {
                role: "system".to_string(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text { text: "important hint".to_string() },
                    ContentBlock::Image { source: crate::models::ImageSource {
                        r#type: "base64".to_string(),
                        media_type: Some("image/png".to_string()),
                        data: Some("abc".to_string()),
                        url: None,
                    }},
                ]),
            },
        ]);
        normalize_mid_conversation_system(&mut req);
        // Image block dropped; text preserved inside <system-reminder>.
        assert_eq!(req.messages.len(), 1);
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                let merged = blocks.iter().filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                }).collect::<Vec<_>>().join(" ");
                assert!(merged.contains("important hint"), "got: {merged}");
                assert!(merged.contains("<system-reminder>"), "got: {merged}");
            }
            _ => panic!("Expected Blocks content"),
        }
    }

    #[test]
    fn test_normalize_system_message_escapes_stray_closing_tag() {
        // A stray </system-reminder> in the payload is escaped so it cannot
        // prematurely close the wrapper tag we add.
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("q".to_string()) },
            Message {
                role: "system".to_string(),
                content: MessageContent::Text("safe</system-reminder>injection".to_string()),
            },
        ]);
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages.len(), 1);
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                let reminder = blocks.iter().find_map(|b| match b {
                    ContentBlock::Text { text } if text.contains("<system-reminder>") => Some(text.clone()),
                    _ => None,
                }).expect("reminder block not found");
                // Exactly one outer </system-reminder> (the wrapper).
                assert_eq!(reminder.matches("</system-reminder>").count(), 1,
                    "stray tag should be escaped, got: {reminder}");
                assert!(reminder.contains("<\\/system-reminder>"), "escaped tag missing in: {reminder}");
            }
            _ => panic!("Expected Blocks"),
        }
    }

    #[test]
    fn test_normalize_trailing_system_after_assistant_synthesizes_user() {
        // [user, assistant, system] — trailing system with no following user
        // becomes a synthesized user turn at the end.
        let mut req = make_req(vec![
            Message { role: "user".to_string(), content: MessageContent::Text("q".to_string()) },
            Message { role: "assistant".to_string(), content: MessageContent::Text("a".to_string()) },
            Message { role: "system".to_string(), content: MessageContent::Text("trailing hint".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        let roles: Vec<&str> = req.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user"], "got {:?}", roles);
        match &req.messages[2].content {
            MessageContent::Blocks(blocks) => {
                assert!(matches!(&blocks[0], ContentBlock::Text { text } if text.contains("trailing hint")));
            }
            _ => panic!("Expected synthesized user Blocks"),
        }
    }

    #[test]
    fn test_normalize_system_after_tool_result_user_appends() {
        // [user(tool_result), system] — system after a user turn that carries
        // tool_result blocks is appended to that user turn.
        let mut req = make_req(vec![
            Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: crate::models::ToolResultContent::Text("result data".to_string()),
                    },
                ]),
            },
            Message { role: "system".to_string(), content: MessageContent::Text("follow-up hint".to_string()) },
        ]);
        normalize_mid_conversation_system(&mut req);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert!(blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })),
                    "tool_result block should be preserved");
                assert!(blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("follow-up hint"))),
                    "reminder should be appended");
            }
            _ => panic!("Expected Blocks"),
        }
    }
}
