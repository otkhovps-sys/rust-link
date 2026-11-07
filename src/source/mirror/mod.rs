//! Spotify, Apple Music and Deezer, LavaSrc style: metadata comes from each service's API,
//! audio from a YouTube copy found at play time (ISRC first, then title and artist, with a
//! duration check) and remembered in the cache index.

pub mod applemusic;
pub mod deezer;
pub mod spotify;
pub mod spotify_login;

use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::api::models::{LoadResult, PlaylistData, PlaylistInfo, Track, TrackInfo};

/// A track of a mirrored service plus LavaSrc's `pluginInfo` fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MirrorTrack {
    pub info: TrackInfo,
    pub album_name: Option<String>,
    pub album_url: Option<String>,
    pub artist_url: Option<String>,
    pub artist_artwork_url: Option<String>,
    pub preview_url: Option<String>,
}

impl MirrorTrack {
    pub fn into_track(self) -> Track {
        let mut plugin = Map::new();
        plugin.insert("albumName".into(), json!(self.album_name));
        plugin.insert("albumUrl".into(), json!(self.album_url));
        plugin.insert("artistUrl".into(), json!(self.artist_url));
        plugin.insert("artistArtworkUrl".into(), json!(self.artist_artwork_url));
        plugin.insert("previewUrl".into(), json!(self.preview_url));
        plugin.insert("isPreview".into(), json!(false));
        Track::with_plugin_info(self.info, plugin)
    }
}

/// Album, playlist or artist top tracks.
#[derive(Debug, Clone, Default)]
pub struct MirrorCollection {
    /// `album`, `playlist` or `artist` (LavaSrc `pluginInfo.type`).
    pub kind: &'static str,
    pub name: String,
    pub url: Option<String>,
    pub artwork_url: Option<String>,
    pub author: Option<String>,
    pub total_tracks: Option<u64>,
    pub tracks: Vec<MirrorTrack>,
}

impl MirrorCollection {
    pub fn into_load_result(self) -> LoadResult {
        if self.tracks.is_empty() {
            return LoadResult::empty();
        }
        let total = self.total_tracks.unwrap_or(self.tracks.len() as u64);
        let mut plugin = Map::new();
        plugin.insert("type".into(), json!(self.kind));
        plugin.insert("url".into(), json!(self.url));
        plugin.insert("artworkUrl".into(), json!(self.artwork_url));
        plugin.insert("author".into(), json!(self.author));
        plugin.insert("totalTracks".into(), json!(total));
        LoadResult::Playlist(PlaylistData {
            info: PlaylistInfo {
                name: self.name,
                selected_track: -1,
            },
            plugin_info: plugin,
            tracks: self.tracks.into_iter().map(MirrorTrack::into_track).collect(),
        })
    }
}

pub fn single(track: Option<MirrorTrack>) -> LoadResult {
    match track {
        Some(track) => LoadResult::Track(track.into_track()),
        None => LoadResult::empty(),
    }
}

pub fn search_result(tracks: Vec<MirrorTrack>) -> LoadResult {
    if tracks.is_empty() {
        LoadResult::empty()
    } else {
        LoadResult::Search(tracks.into_iter().map(MirrorTrack::into_track).collect())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn track_info(
    source: &str,
    identifier: String,
    title: String,
    author: String,
    length_ms: u64,
    uri: Option<String>,
    artwork_url: Option<String>,
    isrc: Option<String>,
) -> TrackInfo {
    TrackInfo {
        identifier,
        is_seekable: true,
        author,
        length: length_ms,
        is_stream: false,
        position: 0,
        title,
        uri,
        artwork_url,
        isrc: isrc.filter(|i| !i.is_empty()),
        source_name: source.into(),
    }
}

pub(crate) fn http_client(timeout_secs: u64) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("rust-link/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(timeout_secs.max(1)))
        .timeout(Duration::from_secs(timeout_secs.max(1) * 3))
        .build()?)
}

pub(crate) fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `(provider, query)` pairs from LavaSrc-style templates such as `ytsearch:"%ISRC%"`.
pub fn provider_queries(templates: &[String], info: &TrackInfo) -> Vec<(String, String)> {
    let query = format!("{} {}", info.title, info.author);
    templates
        .iter()
        .filter_map(|template| {
            let (provider, rest) = template.split_once(':')?;
            let provider = match provider.trim().to_ascii_lowercase().as_str() {
                "ytsearch" | "ytmsearch" => "ytsearch",
                "scsearch" => "scsearch",
                _ => return None,
            };
            if rest.contains("%ISRC%") && info.isrc.is_none() {
                return None;
            }
            let rest = rest
                .replace("%ISRC%", info.isrc.as_deref().unwrap_or_default())
                .replace("%QUERY%", &query);
            Some((provider.to_string(), rest.trim().to_string()))
        })
        .collect()
}

/// First of the top 5 candidates whose duration matches the original track.
pub fn pick_candidate(candidates: &[TrackInfo], expected_ms: u64) -> Option<usize> {
    let usable = |c: &TrackInfo| !c.is_stream && c.length > 0;
    if expected_ms == 0 {
        return candidates.iter().take(5).position(usable);
    }
    let tolerance = (expected_ms * 15 / 100).max(10_000);
    candidates
        .iter()
        .take(5)
        .position(|c| usable(c) && c.length.abs_diff(expected_ms) <= tolerance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, length: u64) -> TrackInfo {
        track_info("youtube", id.into(), id.into(), "a".into(), length, None, None, None)
    }

    #[test]
    fn builds_provider_queries() {
        let mut info = candidate("x", 1000);
        info.title = "Hello".into();
        info.author = "Adele".into();
        let templates: Vec<String> = ["ytsearch:\"%ISRC%\"", "ytmsearch:%QUERY%", "dzisrc:%ISRC%", "nope"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            provider_queries(&templates, &info),
            vec![("ytsearch".into(), "Hello Adele".into())]
        );
        info.isrc = Some("GBBKS1500214".into());
        assert_eq!(
            provider_queries(&templates, &info),
            vec![
                ("ytsearch".into(), "\"GBBKS1500214\"".into()),
                ("ytsearch".into(), "Hello Adele".into())
            ]
        );
    }

    #[test]
    fn picks_candidates_by_duration() {
        let results = vec![
            candidate("loop", 3_600_000),
            candidate("live", 0),
            candidate("song", 296_000),
        ];
        assert_eq!(pick_candidate(&results, 295_000), Some(2));
        assert_eq!(pick_candidate(&results, 30_000), None);
        assert_eq!(
            pick_candidate(&results, 0),
            Some(0),
            "unknown length: first usable result"
        );
        assert_eq!(
            pick_candidate(&[candidate("long", 3_000_000)], 3_400_000),
            Some(0),
            "15% tolerance"
        );
    }

    #[test]
    fn collections_carry_lavasrc_plugin_info() {
        let track = MirrorTrack {
            info: track_info(
                "deezer",
                "3135556".into(),
                "Harder".into(),
                "Daft Punk".into(),
                224_000,
                None,
                None,
                None,
            ),
            album_name: Some("Discovery".into()),
            ..Default::default()
        };
        let LoadResult::Playlist(p) = MirrorCollection {
            kind: "album",
            name: "Discovery".into(),
            tracks: vec![track],
            ..Default::default()
        }
        .into_load_result() else {
            panic!("playlist expected")
        };
        assert_eq!(p.plugin_info["type"], "album");
        assert_eq!(p.plugin_info["totalTracks"], 1);
        assert_eq!(p.tracks[0].plugin_info["albumName"], "Discovery");
        assert_eq!(p.tracks[0].plugin_info["previewUrl"], Value::Null);
        assert!(matches!(
            MirrorCollection::default().into_load_result(),
            LoadResult::Empty(_)
        ));
    }
}
