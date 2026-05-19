use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

/// GitHub OAuth app client ID for Copilot device flow
/// (Visual Studio Code — GitHub Copilot Chat application)
const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const COPILOT_FALLBACK_BASE_URL: &str = "https://api.individual.githubcopilot.com";

// ── Device code response ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

// ── GitHub access token (result of device flow) ───────────────────────────────

#[derive(Debug, Deserialize)]
struct GitHubAccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    // interval increase for slow_down
    interval: Option<u64>,
}

// ── Copilot bearer token ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CopilotTokenResponse {
    pub token: String,
    pub expires_at: u64, // Unix timestamp (seconds)
}

// ── Poll result ───────────────────────────────────────────────────────────────

pub enum PollResult {
    Success(String), // GitHub OAuth access token
    Pending,
    Expired,
}

// ── Public functions ──────────────────────────────────────────────────────────

/// Start GitHub device code flow. Returns response with user_code and verification_uri.
pub async fn start_device_flow(client: &Client) -> Result<DeviceCodeResponse> {
    let response = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("client_id={}&scope=read:user", GITHUB_CLIENT_ID))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub device code request failed ({status}): {body}");
    }

    let device_response: DeviceCodeResponse = response.json().await?;
    Ok(device_response)
}

/// Poll GitHub's token endpoint for one round. Returns PollResult.
/// `interval` is the current polling interval in seconds (may increase on slow_down).
/// Returns updated interval so the caller can adjust for slow_down.
pub async fn poll_github_token_once(
    client: &Client,
    device_code: &str,
    interval: u64,
) -> Result<(PollResult, u64)> {
    let body = format!(
        "client_id={}&device_code={}&grant_type=urn:ietf:params:oauth:grant-type:device_code",
        GITHUB_CLIENT_ID, device_code
    );

    let response = client
        .post(GITHUB_ACCESS_TOKEN_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub token poll failed ({status}): {body}");
    }

    let token_resp: GitHubAccessTokenResponse = response.json().await?;

    match token_resp.error.as_deref() {
        None => {
            // success
            let access_token = token_resp
                .access_token
                .ok_or_else(|| anyhow::anyhow!("GitHub returned no access_token"))?;
            Ok((PollResult::Success(access_token), interval))
        }
        Some("authorization_pending") => Ok((PollResult::Pending, interval)),
        Some("slow_down") => {
            let new_interval = token_resp.interval.unwrap_or(interval + 5).min(30);
            Ok((PollResult::Pending, new_interval))
        }
        Some("expired_token") => Ok((PollResult::Expired, interval)),
        Some(other) => anyhow::bail!("GitHub token error: {other}"),
    }
}

/// Poll GitHub for authorization for up to `max_secs` seconds.
/// Returns the GitHub OAuth access token on success, or PollResult::Pending/Expired.
pub async fn poll_for_github_token(
    client: &Client,
    device_code: &str,
    mut interval: u64,
    max_secs: u64,
) -> Result<PollResult> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(max_secs);

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;

        if tokio::time::Instant::now() >= deadline {
            return Ok(PollResult::Pending);
        }

        let (result, new_interval) = poll_github_token_once(client, device_code, interval).await?;
        interval = new_interval;

        match result {
            PollResult::Success(token) => return Ok(PollResult::Success(token)),
            PollResult::Expired => return Ok(PollResult::Expired),
            PollResult::Pending => continue,
        }
    }
}

/// Exchange a GitHub OAuth access token for a Copilot bearer token.
pub async fn exchange_for_copilot_token(
    client: &Client,
    github_token: &str,
) -> Result<CopilotTokenResponse> {
    fetch_copilot_token(client, github_token).await
}

/// Refresh an existing Copilot bearer token using the stored GitHub OAuth token.
pub async fn refresh_copilot_token(
    client: &Client,
    github_token: &str,
) -> Result<CopilotTokenResponse> {
    fetch_copilot_token(client, github_token).await
}

/// Parse the `proxy-ep` field from a semicolon-delimited Copilot bearer token.
/// Returns `https://api.<rest>` for tokens containing `proxy-ep=proxy.<rest>`,
/// or the fallback URL if the field is absent.
/// Rejects proxy-ep values that don't start with `proxy.` and end with `.githubcopilot.com`.
pub fn parse_proxy_ep(bearer: &str) -> String {
    for field in bearer.split(';') {
        if let Some(val) = field.strip_prefix("proxy-ep=") {
            // Only accept values starting with "proxy." to close the SSRF bypass where
            // values like "evil.githubcopilot.com" passed the ends_with check.
            let Some(rest) = val.strip_prefix("proxy.") else {
                tracing::warn!("Rejected proxy-ep without 'proxy.' prefix: {}", val);
                return COPILOT_FALLBACK_BASE_URL.to_string();
            };
            let api_host = format!("api.{}", rest);

            // SSRF guard: only allow *.githubcopilot.com hosts
            if !api_host.ends_with(".githubcopilot.com") {
                tracing::warn!("Rejected proxy-ep with unexpected host: {}", api_host);
                return COPILOT_FALLBACK_BASE_URL.to_string();
            }

            return format!("https://{}", api_host);
        }
    }
    COPILOT_FALLBACK_BASE_URL.to_string()
}

// ── Private helpers ───────────────────────────────────────────────────────────

async fn fetch_copilot_token(client: &Client, github_token: &str) -> Result<CopilotTokenResponse> {
    let response = client
        .get(COPILOT_TOKEN_URL)
        .header("Authorization", format!("Bearer {}", github_token))
        .header("Editor-Version", "vscode/1.107.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Copilot token request failed ({status}): {body}");
    }

    let token_response: CopilotTokenResponse = response.json().await?;
    Ok(token_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proxy_ep_standard() {
        let bearer = "tid=abc;exp=123;proxy-ep=proxy.individual.githubcopilot.com;sku=foo";
        assert_eq!(
            parse_proxy_ep(bearer),
            "https://api.individual.githubcopilot.com"
        );
    }

    #[test]
    fn test_parse_proxy_ep_missing_field_returns_fallback() {
        let bearer = "tid=abc;exp=123;sku=foo";
        assert_eq!(
            parse_proxy_ep(bearer),
            "https://api.individual.githubcopilot.com"
        );
    }

    #[test]
    fn test_parse_proxy_ep_no_proxy_prefix_rejected() {
        let bearer = "tid=abc;proxy-ep=custom.endpoint.com";
        assert_eq!(parse_proxy_ep(bearer), COPILOT_FALLBACK_BASE_URL);
    }

    #[test]
    fn test_parse_proxy_ep_ssrf_bypass_rejected() {
        // "evil.githubcopilot.com" ends with ".githubcopilot.com" but lacks "proxy." prefix
        let bearer = "tid=abc;proxy-ep=evil.githubcopilot.com";
        assert_eq!(parse_proxy_ep(bearer), COPILOT_FALLBACK_BASE_URL);
    }

    #[test]
    fn test_parse_proxy_ep_enterprise() {
        let bearer = "tid=abc;proxy-ep=proxy.enterprise.githubcopilot.com;sku=enterprise";
        assert_eq!(
            parse_proxy_ep(bearer),
            "https://api.enterprise.githubcopilot.com"
        );
    }
}
