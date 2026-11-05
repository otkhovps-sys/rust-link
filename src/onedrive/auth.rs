//! OAuth2 for Microsoft Graph: refresh-token workflow persisted in `config/onedrive.json`,
//! plus an interactive device-code login used to obtain the first refresh token.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::OneDriveError;

fn default_tenant() -> String {
    "consumers".into()
}

fn default_scope() -> String {
    "offline_access Files.ReadWrite".into()
}

/// Contents of `config/onedrive.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneDriveCredentials {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// `consumers` (personal OneDrive), `organizations`, `common` or a tenant GUID.
    #[serde(default = "default_tenant")]
    pub tenant: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorBody {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

const REFRESH_MARGIN_SECS: i64 = 300;

fn now_secs() -> i64 {
    crate::util::now_millis() / 1000
}

pub struct TokenManager {
    path: PathBuf,
    authority: String,
    http: reqwest::Client,
    creds: Mutex<OneDriveCredentials>,
}

impl TokenManager {
    pub async fn load(path: &Path, authority: &str, http: reqwest::Client) -> Result<Arc<Self>, OneDriveError> {
        let raw = tokio::fs::read_to_string(path).await.map_err(|e| {
            OneDriveError::Auth(format!(
                "cannot read {} ({e}); run `rust-link onedrive-login --client-id <APP_ID>`",
                path.display()
            ))
        })?;
        let creds: OneDriveCredentials =
            serde_json::from_str(&raw).map_err(|e| OneDriveError::Auth(format!("invalid {}: {e}", path.display())))?;
        if creds.refresh_token.as_deref().unwrap_or_default().is_empty() {
            return Err(OneDriveError::Auth(format!(
                "{} has no refresh_token; run `rust-link onedrive-login`",
                path.display()
            )));
        }
        Ok(Self::from_credentials(creds, path.to_path_buf(), authority, http))
    }

    pub fn from_credentials(
        creds: OneDriveCredentials,
        path: PathBuf,
        authority: &str,
        http: reqwest::Client,
    ) -> Arc<Self> {
        Arc::new(Self {
            path,
            authority: authority.trim_end_matches('/').to_string(),
            http,
            creds: Mutex::new(creds),
        })
    }

    /// A valid access token, refreshed when it expires within five minutes.
    pub async fn access_token(&self) -> Result<String, OneDriveError> {
        let mut creds = self.creds.lock().await;
        let valid = creds.expires_at.unwrap_or(0) - REFRESH_MARGIN_SECS > now_secs();
        if let (true, Some(token)) = (valid, creds.access_token.as_ref()) {
            return Ok(token.clone());
        }
        self.refresh(&mut creds).await?;
        Ok(creds.access_token.clone().unwrap_or_default())
    }

    /// Drop the cached access token (after a 401) so the next call refreshes it.
    pub async fn invalidate(&self) {
        self.creds.lock().await.expires_at = Some(0);
    }

    async fn refresh(&self, creds: &mut OneDriveCredentials) -> Result<(), OneDriveError> {
        let refresh_token = creds
            .refresh_token
            .clone()
            .ok_or_else(|| OneDriveError::Auth("missing refresh_token".into()))?;
        let mut form = vec![
            ("client_id", creds.client_id.clone()),
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token),
            ("scope", creds.scope.clone()),
        ];
        if let Some(secret) = &creds.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        if let Some(redirect) = &creds.redirect_uri {
            form.push(("redirect_uri", redirect.clone()));
        }
        let url = format!("{}/{}/oauth2/v2.0/token", self.authority, creds.tenant);
        let response = self.http.post(&url).form(&form).send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<OAuthErrorBody>(&body)
                .map(|e| format!("{}: {}", e.error, e.error_description.unwrap_or_default()))
                .unwrap_or(body);
            let hint = if detail.starts_with("invalid_grant") {
                " (refresh token expired or revoked: run `rust-link onedrive-login` again)"
            } else {
                ""
            };
            return Err(OneDriveError::Auth(format!(
                "token refresh failed with {status}: {detail}{hint}"
            )));
        }
        let token: TokenResponse = response.json().await?;
        creds.access_token = Some(token.access_token);
        creds.expires_at = Some(now_secs() + token.expires_in);
        if let Some(rotated) = token.refresh_token {
            creds.refresh_token = Some(rotated);
        }
        if let Err(e) = persist_credentials(&self.path, creds).await {
            warn!(path = %self.path.display(), %e, "cannot persist refreshed OneDrive token");
        }
        info!("OneDrive access token refreshed");
        Ok(())
    }
}

/// Atomic write (temp file + rename) with owner-only permissions.
pub async fn persist_credentials(path: &Path, creds: &OneDriveCredentials) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(creds).map_err(std::io::Error::other)?;
    tokio::fs::write(&tmp, json).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(&tmp, path).await
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
    message: Option<String>,
}

pub struct LoginRequest {
    pub credentials_path: PathBuf,
    pub authority: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub tenant: String,
    pub scope: String,
}

/// OAuth2 device authorization grant. The Azure app registration must allow public client
/// flows. Prints the user code, polls until the user signs in and writes the credentials file.
pub async fn device_code_login(req: LoginRequest) -> anyhow::Result<OneDriveCredentials> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let base = format!("{}/{}/oauth2/v2.0", req.authority.trim_end_matches('/'), req.tenant);
    let device: DeviceCodeResponse = http
        .post(format!("{base}/devicecode"))
        .form(&[("client_id", req.client_id.as_str()), ("scope", req.scope.as_str())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    println!();
    match &device.message {
        Some(message) => println!("{message}"),
        None => println!(
            "Open {} and enter the code {}",
            device.verification_uri, device.user_code
        ),
    }
    println!();

    let mut interval = Duration::from_secs(device.interval.unwrap_or(5).max(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("device code expired before sign-in completed");
        }
        let mut form = vec![
            ("client_id", req.client_id.clone()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code".to_string()),
            ("device_code", device.device_code.clone()),
        ];
        if let Some(secret) = &req.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let response = http.post(format!("{base}/token")).form(&form).send().await?;
        if response.status().is_success() {
            let token: TokenResponse = response.json().await?;
            let creds = OneDriveCredentials {
                client_id: req.client_id.clone(),
                client_secret: req.client_secret.clone(),
                tenant: req.tenant.clone(),
                scope: req.scope.clone(),
                redirect_uri: None,
                refresh_token: token.refresh_token,
                access_token: Some(token.access_token),
                expires_at: Some(now_secs() + token.expires_in),
            };
            if creds.refresh_token.is_none() {
                anyhow::bail!("no refresh token returned; make sure the scope includes offline_access");
            }
            persist_credentials(&req.credentials_path, &creds).await?;
            return Ok(creds);
        }
        let body: OAuthErrorBody = response.json().await?;
        match body.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += Duration::from_secs(5),
            other => anyhow::bail!(
                "device code login failed: {other}: {}",
                body.error_description.unwrap_or_default()
            ),
        }
    }
}
