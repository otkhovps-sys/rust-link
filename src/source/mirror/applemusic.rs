//! Apple Music catalog API, authenticated with your own MusicKit developer token
//! (`sources.appleMusic.mediaApiToken`, created in an Apple Developer account).

use reqwest::StatusCode;
use serde_json::Value;
use url::Url;

use super::{search_result, single, text, track_info, MirrorCollection, MirrorTrack};
use crate::{api::models::LoadResult, config::AppleMusicConfig, source::SourceError};

const ARTWORK_SIZE: &str = "600";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppleKind {
    Song,
    Album,
    Playlist,
    Artist,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleRef {
    pub kind: AppleKind,
    pub storefront: Option<String>,
    pub id: String,
}

/// `https://music.apple.com/<cc>/<album|song|playlist|artist>/<slug>/<id>` (`?i=` on an
/// album link selects one song).
pub fn parse_ref(url: &Url) -> Option<AppleRef> {
    if url.host_str() != Some("music.apple.com") {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let (storefront, rest) = match segments.split_first() {
        Some((first, rest)) if first.len() == 2 => (Some(first.to_ascii_lowercase()), rest),
        _ => (None, &segments[..]),
    };
    let kind = match *rest.first()? {
        "song" => AppleKind::Song,
        "album" => AppleKind::Album,
        "playlist" => AppleKind::Playlist,
        "artist" => AppleKind::Artist,
        _ => return None,
    };
    let id = rest.last().filter(|_| rest.len() >= 2)?.to_string();
    if kind == AppleKind::Album {
        if let Some((_, song)) = url.query_pairs().find(|(k, _)| k == "i") {
            return Some(AppleRef {
                kind: AppleKind::Song,
                storefront,
                id: song.into_owned(),
            });
        }
    }
    Some(AppleRef { kind, storefront, id })
}

pub struct AppleMusicClient {
    cfg: AppleMusicConfig,
    http: reqwest::Client,
}

impl AppleMusicClient {
    pub fn new(cfg: AppleMusicConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    pub fn is_configured(&self) -> bool {
        !self.cfg.media_api_token.trim().is_empty()
    }

    fn token(&self) -> Result<&str, SourceError> {
        let token = self.cfg.media_api_token.trim();
        if token.is_empty() {
            return Err(SourceError::Unavailable(
                "Apple Music needs sources.appleMusic.mediaApiToken (a MusicKit developer token)".into(),
            ));
        }
        Ok(token)
    }

    async fn get(&self, path_or_url: &str) -> Result<Value, SourceError> {
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_string()
        } else {
            format!("{}{}", self.cfg.api_url.trim_end_matches('/'), path_or_url)
        };
        let response = self.http.get(&url).bearer_auth(self.token()?).send().await?;
        match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(SourceError::Unavailable(
                "Apple Music rejected sources.appleMusic.mediaApiToken (expired or invalid)".into(),
            )),
            StatusCode::NOT_FOUND => Err(SourceError::Unavailable("Apple Music: not found".into())),
            status if !status.is_success() => Err(SourceError::Unavailable(format!("Apple Music API error {status}"))),
            _ => Ok(response.json().await?),
        }
    }

    fn storefront(&self, from_url: Option<&str>) -> String {
        from_url
            .map(str::to_string)
            .unwrap_or_else(|| self.cfg.country_code.to_ascii_lowercase())
    }

    pub async fn load(&self, reference: AppleRef) -> Result<LoadResult, SourceError> {
        let sf = self.storefront(reference.storefront.as_deref());
        let id = &reference.id;
        match reference.kind {
            AppleKind::Song => {
                let v = self.get(&format!("/v1/catalog/{sf}/songs/{id}")).await?;
                Ok(single(first_data(&v).and_then(|s| parse_song(s, None))))
            }
            AppleKind::Album => {
                let v = self.get(&format!("/v1/catalog/{sf}/albums/{id}")).await?;
                self.collection(&v, "album", self.cfg.album_load_limit).await
            }
            AppleKind::Playlist => {
                let v = self.get(&format!("/v1/catalog/{sf}/playlists/{id}")).await?;
                self.collection(&v, "playlist", self.cfg.playlist_load_limit).await
            }
            AppleKind::Artist => {
                let artist = self.get(&format!("/v1/catalog/{sf}/artists/{id}")).await?;
                let top = self
                    .get(&format!("/v1/catalog/{sf}/artists/{id}/view/top-songs"))
                    .await?;
                let attrs = first_data(&artist).map(|a| &a["attributes"]);
                let name = attrs
                    .and_then(|a| text(a, "name"))
                    .unwrap_or_else(|| "Unknown artist".into());
                let artwork = attrs.and_then(artwork_url);
                let tracks = data(&top)
                    .iter()
                    .filter_map(|s| parse_song(s, artwork.clone()))
                    .collect();
                Ok(MirrorCollection {
                    kind: "artist",
                    name: format!("{name}'s Top Tracks"),
                    url: attrs.and_then(|a| text(a, "url")),
                    artwork_url: artwork,
                    author: Some(name),
                    total_tracks: None,
                    tracks,
                }
                .into_load_result())
            }
        }
    }

    async fn collection(&self, v: &Value, kind: &'static str, pages: usize) -> Result<LoadResult, SourceError> {
        let Some(item) = first_data(v) else {
            return Ok(LoadResult::empty());
        };
        let attrs = &item["attributes"];
        let relation = &item["relationships"]["tracks"];
        let mut tracks: Vec<MirrorTrack> = data(relation).iter().filter_map(|s| parse_song(s, None)).collect();
        let mut next = text(relation, "next");
        let mut loaded = 1;
        while let (Some(path), true) = (next.take(), loaded < pages.max(1)) {
            let page = self.get(&path).await?;
            tracks.extend(data(&page).iter().filter_map(|s| parse_song(s, None)));
            next = text(&page, "next");
            loaded += 1;
        }
        Ok(MirrorCollection {
            kind,
            name: text(attrs, "name").unwrap_or_else(|| format!("Unknown {kind}")),
            url: text(attrs, "url"),
            artwork_url: artwork_url(attrs),
            author: text(attrs, "artistName").or_else(|| text(attrs, "curatorName")),
            total_tracks: attrs["trackCount"].as_u64(),
            tracks,
        }
        .into_load_result())
    }

    pub async fn search(&self, query: &str) -> Result<LoadResult, SourceError> {
        if query.is_empty() {
            return Ok(LoadResult::empty());
        }
        let sf = self.storefront(None);
        let mut url = Url::parse(&format!(
            "{}/v1/catalog/{sf}/search",
            self.cfg.api_url.trim_end_matches('/')
        ))
        .map_err(|e| SourceError::Parse(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("term", query)
            .append_pair("types", "songs")
            .append_pair("limit", &self.cfg.search_limit.clamp(1, 25).to_string());
        let v = self.get(url.as_str()).await?;
        let tracks = data(&v["results"]["songs"])
            .iter()
            .filter_map(|s| parse_song(s, None))
            .collect();
        Ok(search_result(tracks))
    }
}

fn data(v: &Value) -> &[Value] {
    v["data"].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn first_data(v: &Value) -> Option<&Value> {
    data(v).first()
}

fn artwork_url(attrs: &Value) -> Option<String> {
    text(&attrs["artwork"], "url").map(|u| u.replace("{w}", ARTWORK_SIZE).replace("{h}", ARTWORK_SIZE))
}

fn parse_song(v: &Value, artist_artwork: Option<String>) -> Option<MirrorTrack> {
    if v.get("type").and_then(Value::as_str).is_some_and(|t| t != "songs") {
        return None;
    }
    let id = text(v, "id")?;
    let attrs = &v["attributes"];
    let url = text(attrs, "url");
    let info = track_info(
        "applemusic",
        id,
        text(attrs, "name").unwrap_or_else(|| "Unknown title".into()),
        text(attrs, "artistName").unwrap_or_else(|| "Unknown artist".into()),
        attrs["durationInMillis"].as_u64().unwrap_or(0),
        url.clone(),
        artwork_url(attrs),
        text(attrs, "isrc"),
    );
    Some(MirrorTrack {
        info,
        album_name: text(attrs, "albumName"),
        album_url: url.map(|u| u.split('?').next().unwrap_or_default().to_string()),
        artist_url: None,
        artist_artwork_url: artist_artwork,
        preview_url: attrs["previews"]
            .as_array()
            .and_then(|p| p.first())
            .and_then(|p| text(p, "url")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_apple_music_links() {
        let parse = |s: &str| parse_ref(&Url::parse(s).unwrap());
        assert_eq!(
            parse("https://music.apple.com/vn/album/hello/1544494392?i=1544494393"),
            Some(AppleRef {
                kind: AppleKind::Song,
                storefront: Some("vn".into()),
                id: "1544494393".into()
            })
        );
        assert_eq!(
            parse("https://music.apple.com/us/album/25/1544494392").unwrap().kind,
            AppleKind::Album
        );
        assert_eq!(
            parse("https://music.apple.com/us/playlist/todays-hits/pl.f4d106fed2bd41149aaacabb233eb5eb")
                .unwrap()
                .id,
            "pl.f4d106fed2bd41149aaacabb233eb5eb"
        );
        assert_eq!(
            parse("https://music.apple.com/us/artist/adele/262836961").unwrap().kind,
            AppleKind::Artist
        );
        assert_eq!(parse("https://music.apple.com/us/browse"), None);
        assert_eq!(parse("https://example.com/us/album/x/1"), None);
    }

    #[test]
    fn maps_songs() {
        let v = serde_json::json!({"id": "1544494393", "type": "songs", "attributes": {
            "name": "Hello", "artistName": "Adele", "albumName": "25", "durationInMillis": 295502,
            "isrc": "GBBKS1500214", "url": "https://music.apple.com/us/album/hello/1544494392?i=1544494393",
            "artwork": {"url": "https://is1-ssl.mzstatic.com/image/{w}x{h}bb.jpg"},
            "previews": [{"url": "https://audio-ssl.itunes.apple.com/preview.m4a"}]}});
        let song = parse_song(&v, None).unwrap();
        assert_eq!(song.info.source_name, "applemusic");
        assert_eq!(song.info.length, 295_502);
        assert_eq!(
            song.info.artwork_url.as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/600x600bb.jpg")
        );
        assert_eq!(
            song.album_url.as_deref(),
            Some("https://music.apple.com/us/album/hello/1544494392")
        );
        assert!(song.preview_url.is_some());
    }
}
