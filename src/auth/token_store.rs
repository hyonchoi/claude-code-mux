use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// OAuth token information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    /// Provider ID (e.g., "claude-max", "anthropic-oauth")
    pub provider_id: String,
    /// OAuth access token
    pub access_token: String,
    /// OAuth refresh token
    pub refresh_token: String,
    /// Token expiration time (UTC)
    pub expires_at: DateTime<Utc>,
    /// Optional enterprise URL for GitHub Copilot Enterprise
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise_url: Option<String>,
    /// Optional Google Cloud project ID for Gemini Code Assist API
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

impl OAuthToken {
    /// Check if token is expired
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    /// Check if token will expire soon (within 5 minutes)
    pub fn needs_refresh(&self) -> bool {
        let now = Utc::now();
        let buffer = chrono::Duration::minutes(5);
        now + buffer >= self.expires_at
    }
}

/// Token storage - persists to JSON file
#[derive(Debug, Clone)]
pub struct TokenStore {
    /// Path to token storage file
    file_path: PathBuf,
    /// In-memory cache of tokens
    tokens: Arc<RwLock<HashMap<String, OAuthToken>>>,
}

impl TokenStore {
    /// Create a new token store
    /// Loads existing tokens from file if it exists
    pub fn new(file_path: PathBuf) -> Result<Self> {
        let tokens = if file_path.exists() {
            let content = fs::read_to_string(&file_path).context("Failed to read token file")?;
            serde_json::from_str(&content).context("Failed to parse token file")?
        } else {
            HashMap::new()
        };

        Ok(Self {
            file_path,
            tokens: Arc::new(RwLock::new(tokens)),
        })
    }

    /// Get default token store path
    /// ~/.claude-code-mux/oauth_tokens.json
    pub fn default_path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Failed to get home directory")?;
        let config_dir = home.join(".claude-code-mux");
        fs::create_dir_all(&config_dir).context("Failed to create config directory")?;
        Ok(config_dir.join("oauth_tokens.json"))
    }

    /// Create a token store at the default location
    pub fn default() -> Result<Self> {
        let path = Self::default_path()?;
        Self::new(path)
    }

    /// Save token for a provider
    pub fn save(&self, token: OAuthToken) -> Result<()> {
        let provider_id = token.provider_id.clone();

        // Update in-memory cache
        {
            let mut tokens = self.tokens.write().unwrap();
            tokens.insert(provider_id, token);
        }

        // Persist to file
        self.persist()?;

        Ok(())
    }

    /// Get token for a provider
    pub fn get(&self, provider_id: &str) -> Option<OAuthToken> {
        let tokens = self.tokens.read().unwrap();
        tokens.get(provider_id).cloned()
    }

    /// Remove token for a provider
    pub fn remove(&self, provider_id: &str) -> Result<()> {
        {
            let mut tokens = self.tokens.write().unwrap();
            tokens.remove(provider_id);
        }

        // Persist to file
        self.persist()?;

        Ok(())
    }

    /// List all provider IDs that have tokens
    pub fn list_providers(&self) -> Vec<String> {
        let tokens = self.tokens.read().unwrap();
        tokens.keys().cloned().collect()
    }

    /// Get all tokens
    pub fn all(&self) -> HashMap<String, OAuthToken> {
        let tokens = self.tokens.read().unwrap();
        tokens.clone()
    }

    /// Persist tokens to file
    fn persist(&self) -> Result<()> {
        let tokens = self.tokens.read().unwrap();
        let json = serde_json::to_string_pretty(&*tokens).context("Failed to serialize tokens")?;

        // Atomic replace: write to a sibling temp file, fsync it, then rename
        // over the target. A crash, disk-full, or interrupted write can only
        // damage the temp file — never truncate the live token store, which
        // would lose every provider's refresh token and force a full re-auth.
        // On Unix the temp is created 0600 from the start so there is no window
        // where the token file is world-readable (fs::write would create at the
        // process umask, typically 0644). The rename inherits the temp's 0600
        // inode, so a pre-existing looser-mode target ends up 0600 too.
        let file_name = self
            .file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("oauth_tokens.json");
        let tmp_path =
            self.file_path
                .with_file_name(format!(".{}.{}.tmp", file_name, std::process::id()));

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .context("Failed to open temp token file")?;
            file.write_all(json.as_bytes())
                .context("Failed to write temp token file")?;
            file.sync_all().context("Failed to fsync temp token file")?;
        }

        #[cfg(not(unix))]
        {
            fs::write(&tmp_path, &json).context("Failed to write temp token file")?;
        }

        fs::rename(&tmp_path, &self.file_path).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            anyhow::Error::new(e).context("Failed to atomically replace token file")
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_token_store() {
        let temp_dir = TempDir::new().unwrap();
        let token_path = temp_dir.path().join("tokens.json");
        let store = TokenStore::new(token_path).unwrap();

        let token = OAuthToken {
            provider_id: "test-provider".to_string(),
            access_token: "access-123".to_string(),
            refresh_token: "refresh-456".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            enterprise_url: None,
            project_id: None,
        };

        store.save(token.clone()).unwrap();

        let retrieved = store.get("test-provider").unwrap();
        assert_eq!(retrieved.access_token, "access-123");
        assert_eq!(retrieved.refresh_token, "refresh-456");

        store.remove("test-provider").unwrap();
        assert!(store.get("test-provider").is_none());
    }

    #[test]
    fn test_token_expiration() {
        let expired_token = OAuthToken {
            provider_id: "test".to_string(),
            access_token: "token".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: Utc::now() - chrono::Duration::hours(1),
            enterprise_url: None,
            project_id: None,
        };

        assert!(expired_token.is_expired());
        assert!(expired_token.needs_refresh());

        let valid_token = OAuthToken {
            provider_id: "test".to_string(),
            access_token: "token".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            enterprise_url: None,
            project_id: None,
        };

        assert!(!valid_token.is_expired());
        assert!(!valid_token.needs_refresh());
    }

    #[cfg(unix)]
    #[test]
    fn test_save_creates_file_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let token_path = temp_dir.path().join("tokens.json");
        let store = TokenStore::new(token_path.clone()).unwrap();

        let token = OAuthToken {
            provider_id: "p".to_string(),
            access_token: "a".to_string(),
            refresh_token: "r".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            enterprise_url: None,
            project_id: None,
        };
        store.save(token).unwrap();

        // Newly created token file must be owner read/write only (no group/other bits).
        let mode = fs::metadata(&token_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn test_save_tightens_preexisting_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = TempDir::new().unwrap();
        let token_path = temp_dir.path().join("tokens.json");

        // Simulate a pre-existing world-readable token file.
        fs::write(&token_path, "{}").unwrap();
        let mut perms = fs::metadata(&token_path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&token_path, perms).unwrap();
        assert_eq!(
            fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
            0o644
        );

        let store = TokenStore::new(token_path.clone()).unwrap();
        store
            .save(OAuthToken {
                provider_id: "p".to_string(),
                access_token: "a".to_string(),
                refresh_token: "r".to_string(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
                enterprise_url: None,
                project_id: None,
            })
            .unwrap();

        // persist() must tighten the existing file down to 0600.
        let mode = fs::metadata(&token_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
