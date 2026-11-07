//! Spotify Web API (client credentials flow).
//!
//! Since the February 2026 Web API changes (applied to existing apps on 2026-03-09), apps in
//! Development Mode get playlist entries only for playlists the signed-in user owns or
//! collaborates on, artist top tracks are gone and searches return at most 10 results. A
//! client credentials app has no user, so playlist links return metadata without tracks,
//! unless an account was signed in with `rust-link spotify-login` (see [`super::spotify_login`]).

use std::time::{Duration, Instant, SystemTime};

use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use url::Url;

use super::{
    search_result, single, spotify_login, spotify_login::SpotifyLogin, text, track_info, MirrorCollection, MirrorTrack,
};
use crate::{api::models::LoadResult, config::SpotifyConfig, source::SourceError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpotifyRef {
    Track(String),
    Album(String),
    Playlist(String),
    Artist(String),
}

impl SpotifyRef {
    fn new(kind: &str, id: &str) -> Option<Self> {
        let id = id.trim();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let id = id.to_string();
        Some(match kind {
            "track" => SpotifyRef::Track(id),
            "album" => SpotifyRef::Album(id),
            "playlist" => SpotifyRef::Playlist(id),
            "artist" => SpotifyRef::Artist(id),
            _ => return None,
        })
    }
}

/// `https://open.spotify.com/(intl-xx/)<kind>/<id>` or `spotify:<kind>:<id>`.
pub fn parse_ref(input: &str) -> Option<SpotifyRef> {
    if let Some(rest) = input.strip_prefix("spotify:") {
        let mut parts = rest.split(':');
        return SpotifyRef::new(parts.next()?, parts.next()?);
    }
    let url = Url::parse(input).ok()?;
    if !matches!(url.host_str(), Some("open.spotify.com" | "play.spotify.com")) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let segments = match segments.first() {
        Some(first) if first.starts_with("intl-") => &segments[1..],
        _ => &segments[..],
    };
    match segments {
        [kind, id, ..] => SpotifyRef::new(kind, id),
        _ => None,
    }
}

pub struct SpotifyClient {
    cfg: SpotifyConfig,
    http: reqwest::Client,
    token: Mutex<Option<(String, Instant)>>,
    user: Mutex<UserSession>,
}

/// The account signed in with `spotify-login`, read again whenever its file changes.
#[derive(Default)]
struct UserSession {
    login: Option<SpotifyLogin>,
    access: Option<(String, Instant)>,
    /// Modification time of the credentials file when it was last read.
    seen: Option<SystemTime>,
}

fn expiry(expires_in: Option<u64>) -> Instant {
    Instant::now() + Duration::from_secs(expires_in.unwrap_or(3600).saturating_sub(60).max(30))
}

async fn modified(path: &std::path::Path) -> Option<SystemTime> {
    tokio::fs::metadata(path).await.ok().and_then(|m| m.modified().ok())
}

impl SpotifyClient {
    pub fn new(cfg: SpotifyConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            token: Mutex::new(None),
            user: Mutex::new(UserSession::default()),
        }
    }

    /// Name of the signed-in Spotify account, if any.
    pub async fn signed_in_as(&self) -> Option<String> {
        let mut user = self.user.lock().await;
        self.reload_login(&mut user).await;
        user.login.as_ref().map(|l| l.label().to_string())
    }

    async fn reload_login(&self, user: &mut UserSession) {
        let path = &self.cfg.user_credentials_file;
        let seen = modified(path).await;
        if seen == user.seen {
            return;
        }
        user.seen = seen;
        user.access = None;
        user.login = match seen {
            None => None,
            Some(_) => match SpotifyLogin::load(path).await {
                Ok(login) => {
                    info!(account = login.label(), "Spotify: using the signed-in account");
                    Some(login)
                }
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "Spotify: ignoring the sign-in file");
                    None
                }
            },
        };
    }

    /// Access token of the signed-in account. On failure (sign-in revoked) the account is
    /// dropped until the file changes, and requests go out as the app again.
    async fn user_token(&self, force_refresh: bool) -> Option<String> {
        let mut user = self.user.lock().await;
        self.reload_login(&mut user).await;
        let login = user.login.clone()?;
        if let (Some((token, expires)), false) = (&user.access, force_refresh) {
            if Instant::now() < *expires {
                return Some(token.clone());
            }
        }
        match spotify_login::refresh(&self.http, &self.cfg, &login.refresh_token).await {
            Ok(tokens) => {
                if let Some(rotated) = tokens.refresh_token.filter(|t| *t != login.refresh_token) {
                    let updated = SpotifyLogin {
                        refresh_token: rotated,
                        ..login
                    };
                    let path = &self.cfg.user_credentials_file;
                    match updated.save(path).await {
                        Ok(()) => user.seen = modified(path).await,
                        Err(e) => warn!(error = %e, "Spotify: cannot store the new refresh token"),
                    }
                    user.login = Some(updated);
                }
                user.access = Some((tokens.access_token.clone(), expiry(tokens.expires_in)));
                Some(tokens.access_token)
            }
            Err(e) => {
                warn!(
                    account = login.label(),
                    error = %e,
                    "Spotify sign-in stopped working, continuing without it; run `rust-link spotify-login` again"
                );
                user.login = None;
                user.access = None;
                None
            }
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.cfg.client_id.trim().is_empty() && !self.cfg.client_secret.trim().is_empty()
    }

    async fn token(&self, force_refresh: bool) -> Result<String, SourceError> {
        if let Some(token) = self.user_token(force_refresh).await {
            return Ok(token);
        }
        self.app_token(force_refresh).await
    }

    /// Client credentials token of the app itself.
    async fn app_token(&self, force_refresh: bool) -> Result<String, SourceError> {
        let mut cached = self.token.lock().await;
        if let (Some((token, expires)), false) = (&*cached, force_refresh) {
            if Instant::now() < *expires {
                return Ok(token.clone());
            }
        }
        let response = self
            .http
            .post(format!("{}/api/token", self.cfg.accounts_url.trim_end_matches('/')))
            .basic_auth(self.cfg.client_id.trim(), Some(self.cfg.client_secret.trim()))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(SourceError::Unavailable(format!(
                "Spotify rejected the client credentials ({})",
                response.status()
            )));
        }
        let body: Value = response.json().await?;
        let token = text(&body, "access_token")
            .ok_or_else(|| SourceError::Parse("Spotify token response without access_token".into()))?;
        *cached = Some((token.clone(), expiry(body.get("expires_in").and_then(Value::as_u64))));
        Ok(token)
    }

    /// GET an API path (`/tracks/..`) or an absolute `next` URL.
    async fn get(&self, path_or_url: &str) -> Result<Value, SourceError> {
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_string()
        } else {
            format!("{}{}", self.cfg.api_url.trim_end_matches('/'), path_or_url)
        };
        for attempt in 0..2 {
            let token = self.token(attempt > 0).await?;
            let response = self.http.get(&url).bearer_auth(token).send().await?;
            match response.status() {
                StatusCode::UNAUTHORIZED if attempt == 0 => continue,
                StatusCode::NOT_FOUND => return Err(SourceError::Unavailable(
                    "Spotify: not found (private playlists and playlists made by Spotify cannot be read by API apps)"
                        .into(),
                )),
                StatusCode::FORBIDDEN => {
                    return Err(SourceError::Unavailable(
                        "Spotify refused the request (403): apps in Development Mode need the app owner to have \
                         Spotify Premium, and some endpoints are closed to them since 2026"
                            .into(),
                    ))
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    return Err(SourceError::Unavailable(
                        "Spotify rate limit reached, try again later".into(),
                    ))
                }
                status if !status.is_success() => {
                    return Err(SourceError::Unavailable(format!("Spotify API error {status}")))
                }
                _ => return Ok(response.json().await?),
            }
        }
        Err(SourceError::Unavailable("Spotify authentication failed".into()))
    }

    fn market(&self) -> String {
        format!("market={}", self.cfg.country_code.trim())
    }

    pub async fn load(&self, reference: SpotifyRef) -> Result<LoadResult, SourceError> {
        match reference {
            SpotifyRef::Track(id) => {
                let v = self.get(&format!("/tracks/{id}?{}", self.market())).await?;
                Ok(single(parse_track(&v, None, None)))
            }
            SpotifyRef::Album(id) => self.album(&id).await,
            SpotifyRef::Playlist(id) => self.playlist(&id).await,
            SpotifyRef::Artist(id) => self.artist(&id).await,
        }
    }

    pub async fn search(&self, query: &str) -> Result<LoadResult, SourceError> {
        if query.is_empty() {
            return Ok(LoadResult::empty());
        }
        let tracks = self
            .search_tracks(query, self.cfg.search_limit)
            .await?
            .iter()
            .filter_map(|t| parse_track(t, None, None))
            .collect();
        Ok(search_result(tracks))
    }

    /// Raw track objects of a search; Development Mode apps get at most 10.
    async fn search_tracks(&self, query: &str, limit: usize) -> Result<Vec<Value>, SourceError> {
        let mut url = Url::parse(&format!("{}/search", self.cfg.api_url.trim_end_matches('/')))
            .map_err(|e| SourceError::Parse(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("type", "track")
            .append_pair("limit", &limit.clamp(1, MAX_SEARCH_LIMIT).to_string())
            .append_pair("market", self.cfg.country_code.trim());
        let v = self.get(url.as_str()).await?;
        Ok(items(&v["tracks"]).to_vec())
    }

    /// Follow `next` links of a paging object, up to `pages` pages in total.
    async fn pages(&self, first: &Value, pages: usize, mut push: impl FnMut(&Value)) -> Result<(), SourceError> {
        for item in items(first) {
            push(item);
        }
        let mut next = text(first, "next");
        let mut loaded = 1;
        while let (Some(url), true) = (next.take(), loaded < pages.max(1)) {
            let page = self.get(&url).await?;
            for item in items(&page) {
                push(item);
            }
            next = text(&page, "next");
            loaded += 1;
        }
        Ok(())
    }

    async fn album(&self, id: &str) -> Result<LoadResult, SourceError> {
        let album = self.get(&format!("/albums/{id}?{}", self.market())).await?;
        let mut tracks = Vec::new();
        self.pages(&album["tracks"], self.cfg.album_load_limit, |t| {
            tracks.extend(parse_track(t, Some(&album), None));
        })
        .await?;
        Ok(MirrorCollection {
            kind: "album",
            name: text(&album, "name").unwrap_or_else(|| "Unknown album".into()),
            url: text(&album["external_urls"], "spotify"),
            artwork_url: first_image(&album),
            author: artist_names(&album),
            total_tracks: album["tracks"]["total"].as_u64(),
            tracks,
        }
        .into_load_result())
    }

    async fn playlist(&self, id: &str) -> Result<LoadResult, SourceError> {
        let playlist = self.get(&format!("/playlists/{id}?{}", self.market())).await?;
        let name = text(&playlist, "name").unwrap_or_else(|| "Unknown playlist".into());
        let Some(entries) = playlist_entries(&playlist) else {
            let reason = match self.signed_in_as().await {
                Some(account) => format!(
                    "Spotify only gives apps in Development Mode the tracks of playlists owned by or shared \
                     with the signed-in account ({account})"
                ),
                None => "since March 2026 Spotify only gives apps in Development Mode the tracks of playlists \
                         owned by a signed-in account; run `rust-link spotify-login` to sign yours in"
                    .to_string(),
            };
            return Err(SourceError::Unavailable(format!(
                "Spotify returned playlist \"{name}\" without its tracks: {reason}. Track and album links still work."
            )));
        };
        let mut tracks = Vec::new();
        self.pages(entries, self.cfg.playlist_load_limit, |entry| {
            tracks.extend(parse_track(entry_track(entry), None, None));
        })
        .await?;
        let total = entries["total"].as_u64();
        if tracks.is_empty() && total.unwrap_or(0) > 0 {
            debug!(%id, ?total, "Spotify playlist entries without readable tracks");
        }
        Ok(MirrorCollection {
            kind: "playlist",
            name,
            url: text(&playlist["external_urls"], "spotify"),
            artwork_url: first_image(&playlist),
            author: text(&playlist["owner"], "display_name"),
            total_tracks: total,
            tracks,
        }
        .into_load_result())
    }

    async fn artist(&self, id: &str) -> Result<LoadResult, SourceError> {
        let artist = self.get(&format!("/artists/{id}")).await?;
        let artwork = first_image(&artist);
        let name = text(&artist, "name").unwrap_or_else(|| "Unknown artist".into());
        let top = match self.get(&format!("/artists/{id}/top-tracks?{}", self.market())).await {
            Ok(top) => top["tracks"].as_array().cloned().unwrap_or_default(),
            // Closed to Development Mode apps in 2026: the artist's most relevant search hits.
            Err(e) => {
                debug!(%id, error = %e, "artist top tracks unavailable, searching instead");
                let found = self
                    .search_tracks(&format!("artist:\"{}\"", name.replace('"', "")), MAX_SEARCH_LIMIT)
                    .await?;
                let by_artist: Vec<Value> = found
                    .iter()
                    .filter(|t| {
                        t["artists"]
                            .as_array()
                            .is_some_and(|artists| artists.iter().any(|a| a["id"].as_str() == Some(id)))
                    })
                    .cloned()
                    .collect();
                if by_artist.is_empty() {
                    found
                } else {
                    by_artist
                }
            }
        };
        let tracks = top
            .iter()
            .filter_map(|t| parse_track(t, None, artwork.clone()))
            .collect();
        Ok(MirrorCollection {
            kind: "artist",
            name: format!("{name}'s Top Tracks"),
            url: text(&artist["external_urls"], "spotify"),
            artwork_url: artwork,
            author: Some(name),
            total_tracks: None,
            tracks,
        }
        .into_load_result())
    }
}

/// Largest search page Spotify gives apps in Development Mode.
const MAX_SEARCH_LIMIT: usize = 10;

/// Paging object of a playlist's entries: `items` since the 2026 API, `tracks` before. `None`
/// when Spotify left the entries out (playlists of other users, for Development Mode apps).
fn playlist_entries(playlist: &Value) -> Option<&Value> {
    ["items", "tracks"]
        .into_iter()
        .map(|key| &playlist[key])
        .find(|v| v.is_object() && v.get("items").is_some_and(Value::is_array))
}

/// Track of a playlist entry: `item` since the 2026 API, `track` before (now a boolean).
fn entry_track(entry: &Value) -> &Value {
    ["item", "track"]
        .into_iter()
        .map(|key| &entry[key])
        .find(|v| v.is_object())
        .unwrap_or(&Value::Null)
}

fn items(paging: &Value) -> &[Value] {
    paging["items"].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn first_image(v: &Value) -> Option<String> {
    v["images"]
        .as_array()
        .and_then(|images| images.first())
        .and_then(|i| text(i, "url"))
}

fn artist_names(v: &Value) -> Option<String> {
    let names: Vec<&str> = v["artists"]
        .as_array()?
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// Full or simplified track object; simplified album tracks take album data from `album`.
fn parse_track(v: &Value, album: Option<&Value>, artist_artwork: Option<String>) -> Option<MirrorTrack> {
    if v.get("is_local").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if v.get("type").and_then(Value::as_str).is_some_and(|t| t != "track") {
        return None;
    }
    let id = text(v, "id")?;
    let album = v.get("album").filter(|a| a.is_object()).or(album);
    let uri = text(&v["external_urls"], "spotify").unwrap_or_else(|| format!("https://open.spotify.com/track/{id}"));
    let info = track_info(
        "spotify",
        id,
        text(v, "name").unwrap_or_else(|| "Unknown title".into()),
        artist_names(v).unwrap_or_else(|| "Unknown artist".into()),
        v["duration_ms"].as_u64().unwrap_or(0),
        Some(uri),
        album.and_then(first_image),
        text(&v["external_ids"], "isrc"),
    );
    Some(MirrorTrack {
        info,
        album_name: album.and_then(|a| text(a, "name")),
        album_url: album.and_then(|a| text(&a["external_urls"], "spotify")),
        artist_url: v["artists"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| text(&a["external_urls"], "spotify")),
        artist_artwork_url: artist_artwork,
        preview_url: text(v, "preview_url"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spotify_links() {
        assert_eq!(
            parse_ref("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC?si=abc"),
            Some(SpotifyRef::Track("4uLU6hMCjMI75M1A2tKUQC".into()))
        );
        assert_eq!(
            parse_ref("https://open.spotify.com/intl-vi/album/6N9PS4QXF1D0OWPk0Sxtb4"),
            Some(SpotifyRef::Album("6N9PS4QXF1D0OWPk0Sxtb4".into()))
        );
        assert_eq!(
            parse_ref("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"),
            Some(SpotifyRef::Playlist("37i9dQZF1DXcBWIGoYBM5M".into()))
        );
        assert_eq!(
            parse_ref("https://open.spotify.com/artist/0gxyHStUsqpMadRV0Di1Qt"),
            Some(SpotifyRef::Artist("0gxyHStUsqpMadRV0Di1Qt".into()))
        );
        assert_eq!(parse_ref("https://open.spotify.com/episode/abc"), None);
        assert_eq!(parse_ref("https://example.com/track/abc"), None);
    }

    #[test]
    fn reads_playlist_entries_before_and_after_2026() {
        let old = serde_json::json!({"tracks": {"total": 1, "items": [{"track": {"id": "a", "type": "track"}}]}});
        let entries = playlist_entries(&old).unwrap();
        assert_eq!(entry_track(&entries["items"][0])["id"], "a");

        let new = serde_json::json!({"items": {"total": 1, "items": [{"track": true, "item": {"id": "b", "type": "track"}}]}});
        let entries = playlist_entries(&new).unwrap();
        assert_eq!(entry_track(&entries["items"][0])["id"], "b");

        // Playlists of other users, for Development Mode apps: metadata only.
        assert!(playlist_entries(&serde_json::json!({"name": "x", "items": {"total": 30}})).is_none());
        assert!(playlist_entries(&serde_json::json!({"name": "x"})).is_none());
    }

    #[test]
    fn maps_track_objects() {
        let v = serde_json::json!({
            "id": "4uLU6hMCjMI75M1A2tKUQC", "type": "track", "name": "Never Gonna Give You Up",
            "duration_ms": 213573, "external_ids": {"isrc": "GBARL9300135"},
            "artists": [{"name": "Rick Astley", "external_urls": {"spotify": "https://open.spotify.com/artist/0gx"}}],
            "album": {"name": "Whenever You Need Somebody", "images": [{"url": "https://i.scdn.co/a"}],
                      "external_urls": {"spotify": "https://open.spotify.com/album/6N9"}},
            "external_urls": {"spotify": "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"}
        });
        let track = parse_track(&v, None, None).unwrap();
        assert_eq!(track.info.source_name, "spotify");
        assert_eq!(track.info.length, 213_573);
        assert_eq!(track.info.isrc.as_deref(), Some("GBARL9300135"));
        assert_eq!(track.info.artwork_url.as_deref(), Some("https://i.scdn.co/a"));
        assert_eq!(track.album_name.as_deref(), Some("Whenever You Need Somebody"));
        assert_eq!(track.artist_url.as_deref(), Some("https://open.spotify.com/artist/0gx"));
        assert!(parse_track(&serde_json::json!({"id": "x", "is_local": true}), None, None).is_none());
        assert!(parse_track(&serde_json::json!({"id": "x", "type": "episode"}), None, None).is_none());
    }
}
