//! Spotify sign-in (authorization code flow).
//!
//! Since March 2026 Spotify gives apps in Development Mode the entries of a playlist only
//! when the request is made for an account that owns or collaborates on it. `rust-link
//! spotify-login` signs such an account in once and stores its refresh token; the node then
//! calls the Web API on that account's behalf.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{bail, Context};
use axum::{extract::Query, response::Html, routing::get, Router};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener,
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{config::SpotifyConfig, source::SourceError};

/// Reading the account's own private playlists and the ones shared with it.
pub const SCOPES: &str = "playlist-read-private playlist-read-collaborative";

/// What `spotify-login` stores (`sources.spotify.userCredentialsFile`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotifyLogin {
    pub refresh_token: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub scope: String,
    /// Unix seconds.
    #[serde(default)]
    pub created_at: i64,
}

impl SpotifyLogin {
    pub async fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = tokio::fs::read(path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        let login: Self = serde_json::from_slice(&raw).with_context(|| format!("invalid {}", path.display()))?;
        if login.refresh_token.trim().is_empty() {
            bail!("{} has no refresh token", path.display());
        }
        Ok(login)
    }

    pub async fn save(&self, path: &Path) -> std::io::Result<()> {
        crate::util::write_private_json(path, self).await
    }

    /// Display name, or the account id when the profile has none.
    pub fn label(&self) -> &str {
        if self.display_name.is_empty() {
            &self.user_id
        } else {
            &self.display_name
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Page the account owner opens to allow the app.
pub fn authorize_url(cfg: &SpotifyConfig, redirect_uri: &str, state: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(&format!("{}/authorize", cfg.accounts_url.trim_end_matches('/')))?;
    url.query_pairs_mut()
        .append_pair("client_id", cfg.client_id.trim())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("state", state)
        .append_pair("show_dialog", "true");
    Ok(url)
}

async fn token_request(
    http: &reqwest::Client,
    cfg: &SpotifyConfig,
    form: &[(&str, &str)],
) -> Result<TokenResponse, SourceError> {
    let response = http
        .post(format!("{}/api/token", cfg.accounts_url.trim_end_matches('/')))
        .basic_auth(cfg.client_id.trim(), Some(cfg.client_secret.trim()))
        .form(form)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body: Value = response.json().await.unwrap_or_default();
        let reason = body["error_description"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("no details");
        return Err(SourceError::Unavailable(format!(
            "Spotify refused the sign-in ({status}: {reason})"
        )));
    }
    Ok(response.json().await?)
}

pub async fn exchange_code(
    http: &reqwest::Client,
    cfg: &SpotifyConfig,
    code: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, SourceError> {
    token_request(
        http,
        cfg,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ],
    )
    .await
}

/// A fresh access token; Spotify may also hand out a new refresh token.
pub async fn refresh(
    http: &reqwest::Client,
    cfg: &SpotifyConfig,
    refresh_token: &str,
) -> Result<TokenResponse, SourceError> {
    token_request(
        http,
        cfg,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
    )
    .await
}

/// Trade the code for tokens, look the account up and store the refresh token.
pub async fn complete_login(
    http: &reqwest::Client,
    cfg: &SpotifyConfig,
    code: &str,
    redirect_uri: &str,
    path: &Path,
) -> anyhow::Result<SpotifyLogin> {
    let tokens = exchange_code(http, cfg, code, redirect_uri).await?;
    let refresh_token = tokens
        .refresh_token
        .context("Spotify answered without a refresh token")?;
    let me: Value = http
        .get(format!("{}/me", cfg.api_url.trim_end_matches('/')))
        .bearer_auth(&tokens.access_token)
        .send()
        .await?
        .error_for_status()
        .context("cannot read the signed-in Spotify profile")?
        .json()
        .await?;
    let login = SpotifyLogin {
        refresh_token,
        user_id: me["id"].as_str().unwrap_or_default().to_string(),
        display_name: me["display_name"].as_str().unwrap_or_default().to_string(),
        scope: tokens.scope.unwrap_or_default(),
        created_at: crate::util::now_millis() / 1000,
    };
    login
        .save(path)
        .await
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(login)
}

/// The code from what the user pasted: the address the browser ended on (its `state` must
/// match), or the bare code.
pub fn code_from_input(input: &str, state: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("nothing pasted".into());
    }
    if let Ok(url) = Url::parse(input) {
        return code_from_query(url.query_pairs().into_owned().collect(), state);
    }
    let bare = input.len() >= 20
        && input
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if bare {
        Ok(input.to_string())
    } else {
        Err("paste the whole address shown by the browser after signing in".into())
    }
}

fn code_from_query(query: HashMap<String, String>, state: &str) -> Result<String, String> {
    if let Some(error) = query.get("error") {
        return Err(format!("Spotify did not allow the sign-in: {error}"));
    }
    if query.get("state").map(String::as_str) != Some(state) {
        return Err("the address belongs to another sign-in attempt; start again".into());
    }
    query
        .get("code")
        .filter(|c| !c.is_empty())
        .cloned()
        .ok_or_else(|| "the address has no code".into())
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>rust-link</title>\
<body style=\"font-family:sans-serif;margin:3em\"><h2>Spotify sign-in received</h2>\
<p>You can close this tab and go back to rust-link.</p></body>";

/// Serve the redirect URI's path until the browser comes back with a code (or an error).
pub async fn receive_code(listener: TcpListener, path: &str, state: &str) -> Result<String, String> {
    type Slot = Arc<Mutex<Option<oneshot::Sender<Result<String, String>>>>>;
    let (tx, rx) = oneshot::channel();
    let slot: Slot = Arc::new(Mutex::new(Some(tx)));
    let state = state.to_string();
    let handler_slot = Arc::clone(&slot);
    let app = Router::new().route(
        path,
        get(move |Query(query): Query<HashMap<String, String>>| {
            let slot = Arc::clone(&handler_slot);
            let state = state.clone();
            async move {
                let result = code_from_query(query, &state);
                let page = match &result {
                    Ok(_) => DONE_PAGE.to_string(),
                    Err(e) => format!("<!doctype html><meta charset=utf-8><p>{e}</p>"),
                };
                if let Some(tx) = slot.lock().unwrap().take() {
                    let _ = tx.send(result);
                }
                Html(page)
            }
        }),
    );
    let stop = CancellationToken::new();
    let server_stop = stop.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { server_stop.cancelled().await })
            .await;
    });
    let result = rx.await.unwrap_or_else(|_| Err("sign-in page closed".into()));
    // Let the browser receive its page before the listener goes away.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop.cancel();
    let _ = server.await;
    result
}

fn open_browser(url: &str) -> bool {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    command.stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok()
}

/// `rust-link spotify-login`: open the consent page, then take the code from the loopback
/// redirect or from the address pasted into the terminal (node on another machine).
pub async fn interactive_login(cfg: &SpotifyConfig, open: bool) -> anyhow::Result<SpotifyLogin> {
    if cfg.client_id.trim().is_empty() || cfg.client_secret.trim().is_empty() {
        bail!("set sources.spotify.clientId and clientSecret in application.yml first");
    }
    let redirect_uri = cfg.login_redirect_uri.trim().to_string();
    let redirect = Url::parse(&redirect_uri).context("sources.spotify.loginRedirectUri is not a URL")?;
    let state: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    let url = authorize_url(cfg, &redirect_uri, &state)?;

    println!();
    println!("1. In https://developer.spotify.com/dashboard open your app, Settings, Edit, and add");
    println!("   this Redirect URI exactly, then Save:");
    println!();
    println!("   {redirect_uri}");
    println!();
    println!("2. Sign in with the Spotify account whose playlists the bot should read, here:");
    println!();
    println!("   {url}");
    println!();
    println!("3. If the browser is on another machine than rust-link, it ends on an error page");
    println!("   after you agree: copy that page's whole address and paste it here.");
    println!();
    if open && open_browser(url.as_str()) {
        println!("(opened in your browser)");
    }

    let loopback = matches!(redirect.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    let listener = match (loopback, redirect.port_or_known_default()) {
        (true, Some(port)) => {
            let ip = if redirect.host_str() == Some("[::1]") {
                IpAddr::from([0u16, 0, 0, 0, 0, 0, 0, 1])
            } else {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            };
            match TcpListener::bind(SocketAddr::new(ip, port)).await {
                Ok(listener) => Some(listener),
                Err(e) => {
                    println!("(port {port} is busy: {e}; paste the address instead)");
                    None
                }
            }
        }
        _ => None,
    };
    let from_browser = async {
        match listener {
            Some(listener) => receive_code(listener, redirect.path(), &state).await,
            None => std::future::pending().await,
        }
    };
    let from_terminal = async {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if line.trim().is_empty() => continue,
                Ok(Some(line)) => match code_from_input(&line, &state) {
                    Ok(code) => return Ok(code),
                    Err(e) => println!("{e}"),
                },
                _ => return std::future::pending().await,
            }
        }
    };
    let code = tokio::select! {
        code = from_browser => code,
        code = from_terminal => code,
    }
    .map_err(|e| anyhow::anyhow!(e))?;

    let http = super::http_client(30)?;
    complete_login(&http, cfg, &code, &redirect_uri, &cfg.user_credentials_file).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_from_pasted_input() {
        let url = "http://127.0.0.1:8888/callback?code=AQB-code_123&state=abc";
        assert_eq!(code_from_input(url, "abc").unwrap(), "AQB-code_123");
        assert!(code_from_input(url, "other").is_err(), "state must match");
        assert!(
            code_from_input("http://127.0.0.1:8888/callback?error=access_denied&state=abc", "abc")
                .unwrap_err()
                .contains("access_denied")
        );
        let bare = "AQBxYz-0123456789_abcdefghij";
        assert_eq!(code_from_input(bare, "abc").unwrap(), bare);
        assert!(code_from_input("hello", "abc").is_err());
    }

    #[test]
    fn authorize_url_asks_for_playlist_scopes() {
        let cfg = SpotifyConfig {
            client_id: "id123".into(),
            ..Default::default()
        };
        let url = authorize_url(&cfg, "http://127.0.0.1:8888/callback", "st").unwrap();
        let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(
            url.as_str().split('?').next(),
            Some("https://accounts.spotify.com/authorize")
        );
        assert_eq!(query["client_id"], "id123");
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["redirect_uri"], "http://127.0.0.1:8888/callback");
        assert_eq!(query["scope"], SCOPES);
        assert_eq!(query["state"], "st");
    }

    #[tokio::test]
    async fn loopback_redirect_delivers_the_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiting = tokio::spawn(async move { receive_code(listener, "/callback", "st").await });
        let page = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{addr}/callback?code=the-code&state=st"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(page.contains("sign-in received"));
        assert_eq!(waiting.await.unwrap().unwrap(), "the-code");
    }
}
