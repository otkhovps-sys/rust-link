//! Deezer public API (metadata only, no account).

use serde_json::Value;
use url::Url;

use super::{search_result, single, text, track_info, MirrorCollection, MirrorTrack};
use crate::{api::models::LoadResult, config::DeezerConfig, source::SourceError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeezerRef {
    Track(String),
    Album(String),
    Playlist(String),
    Artist(String),
}

pub fn is_deezer_host(url: &Url) -> bool {
    matches!(
        url.host_str().map(|h| h.trim_start_matches("www.")),
        Some("deezer.com" | "link.deezer.com" | "deezer.page.link")
    )
}

/// Short share links redirect to a regular deezer.com URL.
pub fn is_short_link(url: &Url) -> bool {
    matches!(url.host_str(), Some("link.deezer.com" | "deezer.page.link"))
}

/// `https://www.deezer.com/(<lang>/)<track|album|playlist|artist>/<id>`.
pub fn parse_ref(url: &Url) -> Option<DeezerRef> {
    if url.host_str().map(|h| h.trim_start_matches("www.")) != Some("deezer.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let rest = match segments.first() {
        Some(first) if first.len() == 2 => &segments[1..],
        _ => &segments[..],
    };
    let [kind, id, ..] = rest else { return None };
    if !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = id.to_string();
    Some(match *kind {
        "track" => DeezerRef::Track(id),
        "album" => DeezerRef::Album(id),
        "playlist" => DeezerRef::Playlist(id),
        "artist" => DeezerRef::Artist(id),
        _ => return None,
    })
}

pub struct DeezerClient {
    cfg: DeezerConfig,
    http: reqwest::Client,
}

impl DeezerClient {
    pub fn new(cfg: DeezerConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    async fn get(&self, path: &str) -> Result<Value, SourceError> {
        let url = format!("{}{}", self.cfg.api_url.trim_end_matches('/'), path);
        let v: Value = self.http.get(url).send().await?.error_for_status()?.json().await?;
        if let Some(error) = v.get("error") {
            let message = text(error, "message").unwrap_or_else(|| "unknown error".into());
            return Err(SourceError::Unavailable(format!("Deezer: {message}")));
        }
        Ok(v)
    }

    /// Follow a short share link to the canonical URL.
    pub async fn expand(&self, url: &Url) -> Result<Option<DeezerRef>, SourceError> {
        let response = self.http.get(url.clone()).send().await?;
        Ok(parse_ref(response.url()))
    }

    pub async fn load(&self, reference: DeezerRef) -> Result<LoadResult, SourceError> {
        match reference {
            DeezerRef::Track(id) => Ok(single(parse_track(
                &self.get(&format!("/track/{id}")).await?,
                None,
                None,
            ))),
            DeezerRef::Album(id) => {
                let album = self.get(&format!("/album/{id}")).await?;
                let tracks = list(&album["tracks"])
                    .iter()
                    .filter_map(|t| parse_track(t, Some(&album), None))
                    .collect();
                Ok(MirrorCollection {
                    kind: "album",
                    name: text(&album, "title").unwrap_or_else(|| "Unknown album".into()),
                    url: text(&album, "link"),
                    artwork_url: text(&album, "cover_xl"),
                    author: text(&album["artist"], "name"),
                    total_tracks: album["nb_tracks"].as_u64(),
                    tracks,
                }
                .into_load_result())
            }
            DeezerRef::Playlist(id) => {
                let playlist = self.get(&format!("/playlist/{id}")).await?;
                let mut tracks = Vec::new();
                for page in 0..self.cfg.playlist_load_limit.max(1) {
                    let items = self
                        .get(&format!("/playlist/{id}/tracks?limit=100&index={}", page * 100))
                        .await?;
                    let batch = list(&items);
                    tracks.extend(batch.iter().filter_map(|t| parse_track(t, None, None)));
                    if batch.len() < 100 || items.get("next").is_none() {
                        break;
                    }
                }
                Ok(MirrorCollection {
                    kind: "playlist",
                    name: text(&playlist, "title").unwrap_or_else(|| "Unknown playlist".into()),
                    url: text(&playlist, "link"),
                    artwork_url: text(&playlist, "picture_xl"),
                    author: text(&playlist["creator"], "name"),
                    total_tracks: playlist["nb_tracks"].as_u64(),
                    tracks,
                }
                .into_load_result())
            }
            DeezerRef::Artist(id) => {
                let artist = self.get(&format!("/artist/{id}")).await?;
                let top = self.get(&format!("/artist/{id}/top?limit=50")).await?;
                let artwork = text(&artist, "picture_xl");
                let name = text(&artist, "name").unwrap_or_else(|| "Unknown artist".into());
                let tracks = list(&top)
                    .iter()
                    .filter_map(|t| parse_track(t, None, artwork.clone()))
                    .collect();
                Ok(MirrorCollection {
                    kind: "artist",
                    name: format!("{name}'s Top Tracks"),
                    url: text(&artist, "link"),
                    artwork_url: artwork,
                    author: Some(name),
                    total_tracks: None,
                    tracks,
                }
                .into_load_result())
            }
        }
    }

    pub async fn search(&self, query: &str) -> Result<LoadResult, SourceError> {
        if query.is_empty() {
            return Ok(LoadResult::empty());
        }
        let limit = self.cfg.search_limit.clamp(1, 50);
        let encoded = percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC);
        let v = self.get(&format!("/search?q={encoded}&limit={limit}")).await?;
        Ok(search_result(
            list(&v).iter().filter_map(|t| parse_track(t, None, None)).collect(),
        ))
    }

    /// `dzisrc:<ISRC>`.
    pub async fn by_isrc(&self, isrc: &str) -> Result<LoadResult, SourceError> {
        let isrc = isrc.trim();
        if isrc.is_empty() {
            return Ok(LoadResult::empty());
        }
        Ok(single(parse_track(
            &self.get(&format!("/track/isrc:{isrc}")).await?,
            None,
            None,
        )))
    }

    /// ISRC of a track: album, playlist and search entries do not include it.
    pub async fn isrc_of(&self, id: &str) -> Result<Option<String>, SourceError> {
        Ok(text(&self.get(&format!("/track/{id}")).await?, "isrc"))
    }
}

fn list(v: &Value) -> &[Value] {
    v["data"].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn parse_track(v: &Value, album: Option<&Value>, artist_artwork: Option<String>) -> Option<MirrorTrack> {
    if v.get("type").and_then(Value::as_str).is_some_and(|t| t != "track") {
        return None;
    }
    let id = match &v["id"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    let album = v.get("album").filter(|a| a.is_object()).or(album);
    let album_url = album.and_then(|a| match &a["id"] {
        Value::Number(n) => Some(format!("https://www.deezer.com/album/{n}")),
        _ => text(a, "link"),
    });
    let info = track_info(
        "deezer",
        id.clone(),
        text(v, "title").unwrap_or_else(|| "Unknown title".into()),
        text(&v["artist"], "name").unwrap_or_else(|| "Unknown artist".into()),
        v["duration"].as_u64().unwrap_or(0) * 1000,
        Some(text(v, "link").unwrap_or_else(|| format!("https://www.deezer.com/track/{id}"))),
        album.and_then(|a| text(a, "cover_xl")),
        text(v, "isrc"),
    );
    Some(MirrorTrack {
        info,
        album_name: album.and_then(|a| text(a, "title")),
        album_url,
        artist_url: text(&v["artist"], "link"),
        artist_artwork_url: artist_artwork.or_else(|| text(&v["artist"], "picture_xl")),
        preview_url: text(v, "preview"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_deezer_links() {
        let parse = |s: &str| parse_ref(&Url::parse(s).unwrap());
        assert_eq!(
            parse("https://www.deezer.com/fr/track/3135556"),
            Some(DeezerRef::Track("3135556".into()))
        );
        assert_eq!(
            parse("https://deezer.com/album/302127"),
            Some(DeezerRef::Album("302127".into()))
        );
        assert_eq!(
            parse("https://www.deezer.com/en/playlist/908622995?utm=x"),
            Some(DeezerRef::Playlist("908622995".into()))
        );
        assert_eq!(
            parse("https://www.deezer.com/artist/27"),
            Some(DeezerRef::Artist("27".into()))
        );
        assert_eq!(parse("https://www.deezer.com/track/abc"), None);
        assert!(is_short_link(&Url::parse("https://link.deezer.com/s/30abc").unwrap()));
    }

    #[test]
    fn maps_tracks() {
        let v = serde_json::json!({"id": 3135556, "type": "track", "title": "Harder, Better, Faster, Stronger",
            "duration": 224, "isrc": "GBDUW0000059", "link": "https://www.deezer.com/track/3135556",
            "preview": "https://cdns-preview.dzcdn.net/x.mp3",
            "artist": {"name": "Daft Punk", "link": "https://www.deezer.com/artist/27"},
            "album": {"id": 302127, "title": "Discovery", "cover_xl": "https://e-cdns-images.dzcdn.net/c.jpg"}});
        let track = parse_track(&v, None, None).unwrap();
        assert_eq!(track.info.identifier, "3135556");
        assert_eq!(track.info.length, 224_000);
        assert_eq!(track.album_url.as_deref(), Some("https://www.deezer.com/album/302127"));
        assert_eq!(
            track.info.artwork_url.as_deref(),
            Some("https://e-cdns-images.dzcdn.net/c.jpg")
        );
    }
}
