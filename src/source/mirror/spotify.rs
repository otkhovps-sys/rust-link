//! Spotify Web API (client credentials flow).

use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use super::{search_result, single, text, track_info, MirrorCollection, MirrorTrack};
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
}

impl SpotifyClient {
    pub fn new(cfg: SpotifyConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            token: Mutex::new(None),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.cfg.client_id.trim().is_empty() && !self.cfg.client_secret.trim().is_empty()
    }

    async fn token(&self, force_refresh: bool) -> Result<String, SourceError> {
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
        let lifetime = body.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
        let expires = Instant::now() + Duration::from_secs(lifetime.saturating_sub(60).max(30));
        *cached = Some((token.clone(), expires));
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
                StatusCode::NOT_FOUND => {
                    return Err(SourceError::Unavailable(
                        "Spotify: not found (playlists made by Spotify are not available to API apps)".into(),
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
        let mut url = Url::parse(&format!("{}/search", self.cfg.api_url.trim_end_matches('/')))
            .map_err(|e| SourceError::Parse(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("type", "track")
            .append_pair("limit", &self.cfg.search_limit.clamp(1, 50).to_string())
            .append_pair("market", self.cfg.country_code.trim());
        let v = self.get(url.as_str()).await?;
        let tracks = items(&v["tracks"])
            .iter()
            .filter_map(|t| parse_track(t, None, None))
            .collect();
        Ok(search_result(tracks))
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
        let mut tracks = Vec::new();
        self.pages(&playlist["tracks"], self.cfg.playlist_load_limit, |item| {
            tracks.extend(parse_track(&item["track"], None, None));
        })
        .await?;
        Ok(MirrorCollection {
            kind: "playlist",
            name: text(&playlist, "name").unwrap_or_else(|| "Unknown playlist".into()),
            url: text(&playlist["external_urls"], "spotify"),
            artwork_url: first_image(&playlist),
            author: text(&playlist["owner"], "display_name"),
            total_tracks: playlist["tracks"]["total"].as_u64(),
            tracks,
        }
        .into_load_result())
    }

    async fn artist(&self, id: &str) -> Result<LoadResult, SourceError> {
        let artist = self.get(&format!("/artists/{id}")).await?;
        let top = self.get(&format!("/artists/{id}/top-tracks?{}", self.market())).await?;
        let artwork = first_image(&artist);
        let name = text(&artist, "name").unwrap_or_else(|| "Unknown artist".into());
        let tracks = top["tracks"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
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
