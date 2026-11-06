//! Track resolution (`/v4/loadtracks`) and raw audio byte streams for the cache.

pub mod hls;
pub mod http;
pub mod mirror;
pub mod mpegts;
pub mod remote;
pub mod ytdlp;
pub mod ytdlp_update;

use std::{
    collections::HashMap,
    future::Future,
    io::{self, Read},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use bytes::{Buf, Bytes};
use futures_util::{future::BoxFuture, stream::BoxStream, StreamExt, TryStreamExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    api::{
        models::{LoadResult, PlaylistData, PlaylistInfo, Severity, Track, TrackInfo},
        track_codec::MIRROR_SOURCES,
    },
    cache::CacheIndex,
    config::SourcesConfig,
    plugin::{PlayTarget, PluginHost},
};

use self::{
    http::HttpSource,
    mirror::{applemusic::AppleMusicClient, deezer::DeezerClient, spotify::SpotifyClient},
    remote::{RangedReader, RemoteMedia, UrlRefresher},
    ytdlp::{Resolved, YtDlp},
};

/// yt-dlp format for live streams: audio-only HLS when offered, else the smallest video.
const LIVE_FORMAT: &str = "bestaudio/worst";

/// Audio played straight from the origin, never written to the cache.
pub enum DirectSource {
    /// Seekable HTTP resource read in Range chunks.
    Seekable {
        reader: RangedReader,
        container: Option<String>,
    },
    /// Forward-only stream (HLS, radio, yt-dlp pipe).
    Stream(ByteStream),
    /// Local file named by a plugin.
    File {
        path: std::path::PathBuf,
        container: Option<String>,
    },
}

/// How deep plugins may hand a track over to other tracks before playback gives up.
const MAX_DELEGATION: u8 = 4;

fn header_map(headers: &HashMap<String, String>) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("{0}")]
    Unavailable(String),
    #[error("failed to start {program}: {source}")]
    Spawn { program: String, source: io::Error },
    #[error("timed out after {0} seconds")]
    Timeout(u64),
    #[error("unexpected output: {0}")]
    Parse(String),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

impl SourceError {
    fn severity(&self) -> Severity {
        match self {
            SourceError::Unavailable(_) | SourceError::Http(_) => Severity::Common,
            SourceError::Timeout(_) => Severity::Suspicious,
            _ => Severity::Fault,
        }
    }
}

/// Async byte stream of an audio file plus an optional completion check (process exit
/// status for yt-dlp).
pub struct ByteStream {
    stream: BoxStream<'static, io::Result<Bytes>>,
    pub content_length: Option<u64>,
    finisher: Option<BoxFuture<'static, anyhow::Result<()>>>,
}

impl ByteStream {
    pub fn new(stream: BoxStream<'static, io::Result<Bytes>>, content_length: Option<u64>) -> Self {
        Self {
            stream,
            content_length,
            finisher: None,
        }
    }

    pub fn with_finisher(mut self, finisher: impl Future<Output = anyhow::Result<()>> + Send + 'static) -> Self {
        self.finisher = Some(Box::pin(finisher));
        self
    }

    pub async fn next_chunk(&mut self) -> Option<io::Result<Bytes>> {
        self.stream.next().await
    }

    pub async fn finish(self) -> anyhow::Result<()> {
        match self.finisher {
            Some(finisher) => finisher.await,
            None => Ok(()),
        }
    }

    /// Bridge to a blocking `Read` for the decoder thread (live streams).
    pub fn into_blocking_reader(
        mut self,
        handle: &tokio::runtime::Handle,
        cancel: CancellationToken,
    ) -> BlockingStreamReader {
        let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(64);
        handle.spawn(async move {
            loop {
                let chunk = tokio::select! {
                    _ = cancel.cancelled() => return,
                    chunk = self.next_chunk() => chunk,
                };
                match chunk {
                    Some(Ok(bytes)) => {
                        if tx.send(Ok(bytes)).await.is_err() {
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                    None => break,
                }
            }
            if let Err(e) = self.finish().await {
                let _ = tx.send(Err(io::Error::other(e.to_string()))).await;
            }
        });
        BlockingStreamReader {
            rx: Mutex::new(rx),
            current: Bytes::new(),
            eof: false,
        }
    }
}

pub struct BlockingStreamReader {
    rx: Mutex<tokio::sync::mpsc::Receiver<io::Result<Bytes>>>,
    current: Bytes,
    eof: bool,
}

impl Read for BlockingStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.current.is_empty() {
            if self.eof {
                return Ok(0);
            }
            match self.rx.get_mut().unwrap().blocking_recv() {
                Some(Ok(bytes)) => self.current = bytes,
                Some(Err(e)) => return Err(e),
                None => self.eof = true,
            }
        }
        let n = buf.len().min(self.current.len());
        buf[..n].copy_from_slice(&self.current[..n]);
        self.current.advance(n);
        Ok(n)
    }
}

pub fn is_youtube_video_id(s: &str) -> bool {
    s.len() == 11 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn youtube_watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

fn is_youtube_host(url: &Url) -> bool {
    matches!(
        url.host_str().map(|h| h.trim_start_matches("www.")),
        Some("youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be" | "youtube-nocookie.com")
    )
}

fn is_soundcloud_host(url: &Url) -> bool {
    matches!(
        url.host_str().map(|h| h.trim_start_matches("www.")),
        Some("soundcloud.com" | "m.soundcloud.com" | "on.soundcloud.com")
    )
}

/// `(video_id, playlist_id)` of a YouTube URL.
pub fn youtube_ids(url: &Url) -> (Option<String>, Option<String>) {
    let query = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    let list = query("list").filter(|l| !l.is_empty());
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let video = if url.host_str() == Some("youtu.be") {
        segments.first().map(|s| s.to_string())
    } else {
        match segments.as_slice() {
            ["watch", ..] => query("v"),
            ["shorts" | "live" | "embed" | "v", id, ..] => Some(id.to_string()),
            _ => None,
        }
    };
    (video.filter(|v| is_youtube_video_id(v)), list)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    (s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)).then(|| &s[prefix.len()..])
}

pub struct SourceManager {
    cfg: SourcesConfig,
    ytdlp: YtDlp,
    http: HttpSource,
    index: Option<CacheIndex>,
    spotify: Option<SpotifyClient>,
    apple_music: Option<AppleMusicClient>,
    deezer: Option<DeezerClient>,
    load_cache_writes: AtomicU64,
    plugins: Arc<PluginHost>,
}

/// Search prefixes whose results are worth caching.
const CACHED_SEARCH_PREFIXES: [&str; 7] = [
    "ytsearch:",
    "ytmsearch:",
    "scsearch:",
    "spsearch:",
    "amsearch:",
    "dzsearch:",
    "dzisrc:",
];

/// Load-cache key: searches are case and whitespace insensitive, links are taken as-is.
/// `None` for identifiers that should always be resolved fresh (cache search, plain HTTP
/// files and radio streams).
fn load_cache_key(identifier: &str) -> Option<String> {
    for prefix in CACHED_SEARCH_PREFIXES {
        if let Some(query) = strip_prefix_ci(identifier, prefix) {
            let query = query.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
            return (!query.is_empty()).then(|| format!("{prefix}{query}"));
        }
    }
    if identifier.starts_with("spotify:") {
        return Some(identifier.to_string());
    }
    let url = Url::parse(identifier).ok()?;
    let host = url.host_str()?.trim_start_matches("www.").to_ascii_lowercase();
    let music_service = [
        "youtube.com",
        "m.youtube.com",
        "music.youtube.com",
        "youtu.be",
        "soundcloud.com",
        "open.spotify.com",
        "music.apple.com",
        "deezer.com",
    ]
    .contains(&host.as_str());
    music_service.then(|| identifier.to_string())
}

fn cacheable_result(result: &LoadResult) -> bool {
    let tracks: &[Track] = match result {
        LoadResult::Track(track) => std::slice::from_ref(track),
        LoadResult::Playlist(playlist) => &playlist.tracks,
        LoadResult::Search(tracks) => tracks,
        LoadResult::Empty(_) | LoadResult::Error(_) => return false,
    };
    // Live streams end; their metadata goes stale.
    !tracks.is_empty() && tracks.iter().all(|t| !t.info.is_stream)
}

impl SourceManager {
    pub fn new(cfg: &SourcesConfig, index: Option<CacheIndex>) -> anyhow::Result<Self> {
        let api = mirror::http_client(cfg.http_timeout_secs)?;
        Ok(Self {
            ytdlp: YtDlp::new(cfg.ytdlp.clone()),
            http: HttpSource::new(cfg.http_timeout_secs)?,
            spotify: cfg
                .spotify
                .enabled
                .then(|| SpotifyClient::new(cfg.spotify.clone(), api.clone())),
            apple_music: cfg
                .apple_music
                .enabled
                .then(|| AppleMusicClient::new(cfg.apple_music.clone(), api.clone())),
            deezer: cfg.deezer.enabled.then(|| DeezerClient::new(cfg.deezer.clone(), api)),
            cfg: cfg.clone(),
            index,
            load_cache_writes: AtomicU64::new(0),
            plugins: PluginHost::disabled(),
        })
    }

    /// Route plugin search prefixes, URL patterns and tracks to `plugins`.
    pub fn with_plugins(mut self, plugins: Arc<PluginHost>) -> Self {
        self.plugins = plugins;
        self
    }

    /// Tracks of `cache: false` plugins skip the audio cache.
    pub fn bypasses_cache(&self, info: &TrackInfo) -> bool {
        self.plugins.bypasses_cache(info)
    }

    fn spotify(&self) -> Result<Option<&SpotifyClient>, SourceError> {
        match &self.spotify {
            Some(client) if client.is_configured() => Ok(Some(client)),
            Some(_) => Err(SourceError::Unavailable(
                "Spotify needs sources.spotify.clientId and clientSecret (https://developer.spotify.com/dashboard)"
                    .into(),
            )),
            None => Ok(None),
        }
    }

    pub fn source_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.cfg.youtube {
            names.push("youtube".to_string());
        }
        if self.cfg.soundcloud {
            names.push("soundcloud".to_string());
        }
        if self.cfg.http {
            names.push("http".to_string());
        }
        if self.cfg.ytdlp_generic {
            names.push("ytdlp".to_string());
        }
        if self.spotify.as_ref().is_some_and(SpotifyClient::is_configured) {
            names.push("spotify".to_string());
        }
        if self.apple_music.as_ref().is_some_and(AppleMusicClient::is_configured) {
            names.push("applemusic".to_string());
        }
        if self.deezer.is_some() {
            names.push("deezer".to_string());
        }
        if self.index.is_some() {
            names.push("cache".to_string());
        }
        for name in self.plugins.source_names() {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    pub async fn load_tracks(&self, identifier: &str) -> LoadResult {
        let identifier = identifier.trim();
        if identifier.is_empty() {
            return LoadResult::empty();
        }
        // Plugins first: they may claim prefixes and URLs the built-in sources know too.
        if let Some(result) = self.plugins.load_item(identifier).await {
            return result;
        }
        let ttl_ms = self.cfg.load_cache_ttl_secs.saturating_mul(1000).min(i64::MAX as u64) as i64;
        let cache = match (&self.index, load_cache_key(identifier)) {
            (Some(index), Some(key)) if ttl_ms > 0 => Some((index, key)),
            _ => None,
        };
        if let Some((index, key)) = &cache {
            if let Ok(Some(json)) = index.get_load(key, ttl_ms).await {
                if let Ok(result) = serde_json::from_str::<LoadResult>(&json) {
                    debug!(identifier, "loadtracks answered from the load cache");
                    return result;
                }
            }
        }
        let result = match self.load_inner(identifier).await {
            Ok(result) => result,
            Err(e) => {
                warn!(identifier, error = %e, "track loading failed");
                return LoadResult::error(e.to_string(), e.severity());
            }
        };
        if let (Some((index, key)), true) = (&cache, cacheable_result(&result)) {
            if let Ok(json) = serde_json::to_string(&result) {
                if let Err(e) = index.put_load(key, &json).await {
                    debug!(error = %e, "cannot store load result");
                }
                if self
                    .load_cache_writes
                    .fetch_add(1, Ordering::Relaxed)
                    .is_multiple_of(200)
                {
                    let _ = index.purge_loads(ttl_ms).await;
                }
            }
        }
        result
    }

    /// First playable track for `identifier` (player update with `track.identifier`).
    pub async fn load_first_track(&self, identifier: &str) -> Result<Track, String> {
        match self.load_tracks(identifier).await {
            LoadResult::Track(track) => Ok(track),
            LoadResult::Search(tracks) => tracks.into_iter().next().ok_or_else(|| "no matches".into()),
            LoadResult::Playlist(p) => {
                let index = usize::try_from(p.info.selected_track).unwrap_or(0);
                let mut tracks = p.tracks;
                if index < tracks.len() {
                    Ok(tracks.swap_remove(index))
                } else {
                    tracks.into_iter().next().ok_or_else(|| "empty playlist".into())
                }
            }
            LoadResult::Empty(_) => Err(format!("no track found for {identifier}")),
            LoadResult::Error(e) => Err(e.message.unwrap_or(e.cause)),
        }
    }

    async fn load_inner(&self, id: &str) -> Result<LoadResult, SourceError> {
        if let Some(q) = strip_prefix_ci(id, "ytsearch:").or_else(|| strip_prefix_ci(id, "ytmsearch:")) {
            if !self.cfg.youtube {
                return Ok(LoadResult::empty());
            }
            return Ok(search_result(self.ytdlp.search("ytsearch", q.trim(), true).await?));
        }
        if let Some(q) = strip_prefix_ci(id, "scsearch:") {
            if !self.cfg.soundcloud {
                return Ok(LoadResult::empty());
            }
            return Ok(search_result(self.ytdlp.search("scsearch", q.trim(), false).await?));
        }
        if let Some(q) = strip_prefix_ci(id, "spsearch:") {
            return match self.spotify()? {
                Some(spotify) => spotify.search(q.trim()).await,
                None => Ok(LoadResult::empty()),
            };
        }
        if let Some(q) = strip_prefix_ci(id, "amsearch:") {
            return match &self.apple_music {
                Some(apple) => apple.search(q.trim()).await,
                None => Ok(LoadResult::empty()),
            };
        }
        if let Some(q) = strip_prefix_ci(id, "dzsearch:") {
            return match &self.deezer {
                Some(deezer) => deezer.search(q.trim()).await,
                None => Ok(LoadResult::empty()),
            };
        }
        if let Some(isrc) = strip_prefix_ci(id, "dzisrc:") {
            return match &self.deezer {
                Some(deezer) => deezer.by_isrc(isrc).await,
                None => Ok(LoadResult::empty()),
            };
        }
        if let Some(reference) = mirror::spotify::parse_ref(id) {
            return match self.spotify()? {
                Some(spotify) => spotify.load(reference).await,
                None => Ok(LoadResult::empty()),
            };
        }
        if let Some(q) = strip_prefix_ci(id, "cachesearch:") {
            let Some(index) = &self.index else {
                return Ok(LoadResult::empty());
            };
            let entries = index
                .search(q.trim(), self.cfg.ytdlp.search_limit as i64)
                .await
                .map_err(|e| SourceError::Unavailable(e.to_string()))?;
            return Ok(search_result(entries.iter().map(|e| e.to_track_info()).collect()));
        }

        if let Ok(url) = Url::parse(id) {
            if !matches!(url.scheme(), "http" | "https") {
                return Ok(LoadResult::empty());
            }
            if is_youtube_host(&url) {
                return if self.cfg.youtube {
                    self.load_youtube_url(&url).await
                } else {
                    Ok(LoadResult::empty())
                };
            }
            if let Some(reference) = mirror::applemusic::parse_ref(&url) {
                return match &self.apple_music {
                    Some(apple) => apple.load(reference).await,
                    None => Ok(LoadResult::empty()),
                };
            }
            if mirror::deezer::is_deezer_host(&url) {
                let Some(deezer) = &self.deezer else {
                    return Ok(LoadResult::empty());
                };
                let reference = if mirror::deezer::is_short_link(&url) {
                    deezer.expand(&url).await?
                } else {
                    mirror::deezer::parse_ref(&url)
                };
                return match reference {
                    Some(reference) => deezer.load(reference).await,
                    None => Ok(LoadResult::empty()),
                };
            }
            if is_soundcloud_host(&url) {
                return if self.cfg.soundcloud {
                    self.resolve_with_ytdlp(url.as_str(), -1).await
                } else {
                    Ok(LoadResult::empty())
                };
            }
            if self.cfg.http {
                if let Some(info) = self.http.probe(&url).await? {
                    return Ok(LoadResult::Track(Track::new(info)));
                }
            }
            if self.cfg.ytdlp_generic {
                return self.resolve_with_ytdlp(url.as_str(), -1).await;
            }
            return Ok(LoadResult::empty());
        }

        if self.cfg.youtube && is_youtube_video_id(id) {
            return self.load_youtube_video(id).await;
        }
        Ok(LoadResult::empty())
    }

    async fn load_youtube_url(&self, url: &Url) -> Result<LoadResult, SourceError> {
        let (video, list) = youtube_ids(url);
        match (video, list) {
            // Radio/mix playlists ("RD...") are endless: play the requested video only.
            (Some(video), Some(list)) if list.starts_with("RD") => self.load_youtube_video(&video).await,
            (video, Some(list)) => {
                let playlist_url = format!("https://www.youtube.com/playlist?list={list}");
                let result = self.resolve_with_ytdlp(&playlist_url, -1).await?;
                Ok(match (result, video) {
                    (LoadResult::Playlist(mut p), Some(video)) => {
                        p.info.selected_track = p
                            .tracks
                            .iter()
                            .position(|t| t.info.identifier == video)
                            .map(|i| i as i32)
                            .unwrap_or(-1);
                        LoadResult::Playlist(p)
                    }
                    (other, _) => other,
                })
            }
            (Some(video), None) => self.load_youtube_video(&video).await,
            (None, None) => self.resolve_with_ytdlp(url.as_str(), -1).await,
        }
    }

    async fn load_youtube_video(&self, video_id: &str) -> Result<LoadResult, SourceError> {
        if let Some(index) = &self.index {
            if let Ok(Some(entry)) = index.get(video_id).await {
                debug!(video_id, "loadtracks answered from the local cache index");
                return Ok(LoadResult::Track(Track::new(entry.to_track_info())));
            }
        }
        self.resolve_with_ytdlp(&youtube_watch_url(video_id), -1).await
    }

    async fn resolve_with_ytdlp(&self, url: &str, selected: i32) -> Result<LoadResult, SourceError> {
        Ok(match self.ytdlp.resolve(url).await? {
            Resolved::Track(info) => LoadResult::Track(Track::new(info)),
            Resolved::Playlist { name, tracks } if tracks.is_empty() => {
                debug!(%name, "empty playlist");
                LoadResult::empty()
            }
            Resolved::Playlist { name, tracks } => LoadResult::Playlist(PlaylistData {
                info: PlaylistInfo {
                    name,
                    selected_track: selected,
                },
                plugin_info: Default::default(),
                tracks: tracks.into_iter().map(Track::new).collect(),
            }),
        })
    }

    /// Playable stand-in for a Spotify / Apple Music / Deezer track: the YouTube video
    /// matched by ISRC or title and artist (remembered in the cache index). Other tracks
    /// are returned unchanged.
    pub async fn resolve_mirror(&self, info: &TrackInfo) -> anyhow::Result<TrackInfo> {
        if !MIRROR_SOURCES.contains(&info.source_name.as_str()) {
            return Ok(info.clone());
        }
        let mut lookup = info.clone();
        if lookup.isrc.is_none() && info.source_name == "deezer" {
            if let Some(deezer) = &self.deezer {
                lookup.isrc = deezer.isrc_of(&info.identifier).await.ok().flatten();
            }
        }
        if let Some(index) = &self.index {
            let mut known = index.get_mirror(&info.source_name, &info.identifier).await?;
            if known.is_none() {
                if let Some(isrc) = &lookup.isrc {
                    known = index.get_mirror("isrc", isrc).await?;
                }
            }
            if let Some((video_id, length)) = known {
                debug!(source = %info.source_name, id = %info.identifier, %video_id, "mirror known");
                return Ok(mirrored(info, &video_id, length.max(0) as u64));
            }
        }

        let mut chosen = None;
        let mut fallback = None;
        for (provider, query) in mirror::provider_queries(&self.cfg.mirror_providers, &lookup) {
            let results = match self.ytdlp.search_n(&provider, &query, true, 5).await {
                Ok(results) => results,
                Err(e) => {
                    debug!(%provider, %query, error = %e, "mirror search failed");
                    continue;
                }
            };
            if let Some(i) = mirror::pick_candidate(&results, info.length) {
                chosen = Some(results[i].clone());
                break;
            }
            if fallback.is_none() {
                fallback = results.into_iter().find(|c| !c.is_stream);
            }
        }
        let candidate = chosen
            .or(fallback)
            .ok_or_else(|| anyhow::anyhow!("no playable match found for {} - {}", info.title, info.author))?;
        info!(
            source = %info.source_name,
            title = %info.title,
            matched = %candidate.identifier,
            "mirrored track resolved"
        );
        if candidate.source_name != "youtube" {
            let mut resolved = candidate;
            resolved.title = info.title.clone();
            resolved.author = info.author.clone();
            return Ok(resolved);
        }
        if let Some(index) = &self.index {
            let length = candidate.length as i64;
            index
                .put_mirror(&info.source_name, &info.identifier, &candidate.identifier, length)
                .await?;
            if let Some(isrc) = &lookup.isrc {
                index.put_mirror("isrc", isrc, &candidate.identifier, length).await?;
            }
        }
        Ok(mirrored(info, &candidate.identifier, candidate.length))
    }

    /// Page URL handed to yt-dlp for a track.
    fn page_url(info: &TrackInfo) -> String {
        if info.source_name == "youtube" && is_youtube_video_id(&info.identifier) {
            youtube_watch_url(&info.identifier)
        } else {
            info.uri.clone().unwrap_or_else(|| info.identifier.clone())
        }
    }

    /// Size of a direct HTTP track, when known.
    pub async fn content_length(&self, info: &TrackInfo) -> Option<u64> {
        if info.source_name != "http" {
            return None;
        }
        self.http
            .content_length(info.uri.as_deref().unwrap_or(&info.identifier))
            .await
    }

    /// Where a plugin track plays from: `None` for other tracks, and for plugin tracks
    /// left to the node without an http(s) URI (yt-dlp is tried on the URI then).
    async fn plugin_target(&self, info: &TrackInfo, depth: u8) -> anyhow::Result<Option<PlayTarget>> {
        if !self.plugins.owns_source(&info.source_name) {
            return Ok(None);
        }
        if depth >= MAX_DELEGATION {
            anyhow::bail!("plugin tracks hand over to each other more than {MAX_DELEGATION} times");
        }
        let target = match self.plugins.resolve(info).await? {
            Some(PlayTarget::Load(identifier)) => {
                let track = self
                    .load_first_track(&identifier)
                    .await
                    .map_err(|e| anyhow::anyhow!("{identifier}: {e}"))?;
                Some(PlayTarget::Track(self.resolve_mirror(&track.info).await?))
            }
            Some(PlayTarget::Track(track)) => Some(PlayTarget::Track(self.resolve_mirror(&track).await?)),
            Some(other) => Some(other),
            None => info
                .uri
                .as_deref()
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                .map(|uri| PlayTarget::Url {
                    url: uri.to_string(),
                    headers: HashMap::new(),
                    format: None,
                }),
        };
        if let Some(target) = &target {
            debug!(source = %info.source_name, id = %info.identifier, ?target, "plugin playback target");
        }
        Ok(target)
    }

    /// Play without caching: live streams (HLS when available) and tracks too large to
    /// cache (seekable HTTP ranges). Falls back to piping through yt-dlp.
    pub async fn open_direct(self: &Arc<Self>, info: &TrackInfo) -> anyhow::Result<DirectSource> {
        self.open_direct_at(info, 0).await
    }

    fn open_direct_at<'a>(
        self: &'a Arc<Self>,
        info: &'a TrackInfo,
        depth: u8,
    ) -> BoxFuture<'a, anyhow::Result<DirectSource>> {
        Box::pin(async move {
            match self.plugin_target(info, depth).await? {
                Some(PlayTarget::Track(next)) => self.open_direct_at(&next, depth + 1).await,
                Some(PlayTarget::File { path }) => Ok(DirectSource::File {
                    container: file_container(&path),
                    path,
                }),
                Some(PlayTarget::Url { url, headers, format }) => {
                    self.open_plugin_url(info, url, headers, format).await
                }
                Some(PlayTarget::Load(_)) => unreachable!("loaded by plugin_target"),
                None => self.open_direct_builtin(info).await,
            }
        })
    }

    /// Seekable ranges for files (the plugin is asked again when the URL expires), HLS and
    /// plain streams otherwise.
    async fn open_plugin_url(
        self: &Arc<Self>,
        info: &TrackInfo,
        url: String,
        headers: HashMap<String, String>,
        format: Option<String>,
    ) -> anyhow::Result<DirectSource> {
        let client = self.http.client().clone();
        let headers = header_map(&headers);
        if let Ok(parsed) = Url::parse(&url) {
            if hls::is_hls_url(&parsed) {
                return Ok(DirectSource::Stream(hls::open(client, parsed, headers)));
            }
        }
        if !info.is_stream {
            let plugins = Arc::clone(&self.plugins);
            let track = info.clone();
            let refresh: UrlRefresher = Arc::new(move || {
                let plugins = Arc::clone(&plugins);
                let track = track.clone();
                Box::pin(async move {
                    match plugins.resolve(&track).await? {
                        Some(PlayTarget::Url { url, .. }) => Ok(url),
                        _ => anyhow::bail!("plugin no longer resolves {} to a URL", track.identifier),
                    }
                })
            });
            let media = RemoteMedia {
                url: url.clone(),
                headers: headers.clone(),
                refresh: Some(refresh),
            };
            if let Some(reader) = RangedReader::open(client, media).await? {
                return Ok(DirectSource::Seekable {
                    reader,
                    container: format,
                });
            }
        }
        Ok(DirectSource::Stream(self.http.open_with(&url, headers).await?))
    }

    async fn open_direct_builtin(self: &Arc<Self>, info: &TrackInfo) -> anyhow::Result<DirectSource> {
        let client = self.http.client().clone();
        if info.source_name == "http" {
            let target = info.uri.clone().unwrap_or_else(|| info.identifier.clone());
            if !info.is_stream {
                let media = RemoteMedia {
                    url: target.clone(),
                    headers: Default::default(),
                    refresh: None,
                };
                if let Some(reader) = RangedReader::open(client, media).await? {
                    return Ok(DirectSource::Seekable {
                        reader,
                        container: None,
                    });
                }
            }
            return Ok(DirectSource::Stream(self.http.open(&target).await?));
        }

        let page = Self::page_url(info);
        let format = info.is_stream.then_some(LIVE_FORMAT);
        let media = match self.ytdlp.media_url(&page, format).await {
            Ok(media) => media,
            Err(e) => {
                debug!(error = %e, "no direct media URL, piping through yt-dlp");
                return Ok(DirectSource::Stream(self.ytdlp.download(&page)?));
            }
        };
        let headers = header_map(&media.headers);
        if media.is_hls() {
            let url = Url::parse(&media.url)?;
            debug!(%page, "playing HLS stream");
            return Ok(DirectSource::Stream(hls::open(client, url, headers)));
        }
        if media.is_plain_http() && !info.is_stream {
            let this = Arc::clone(self);
            let refresh_page = page.clone();
            let refresh: UrlRefresher = Arc::new(move || {
                let this = Arc::clone(&this);
                let page = refresh_page.clone();
                Box::pin(async move { Ok(this.ytdlp.media_url(&page, None).await?.url) })
            });
            let remote = RemoteMedia {
                url: media.url.clone(),
                headers,
                refresh: Some(refresh),
            };
            if let Some(reader) = RangedReader::open(client, remote).await? {
                debug!(%page, length = reader.length(), "streaming directly with HTTP ranges");
                return Ok(DirectSource::Seekable {
                    reader,
                    container: media.ext.clone(),
                });
            }
        }
        Ok(DirectSource::Stream(self.ytdlp.download(&page)?))
    }

    /// Raw bytes of the audio file behind a track.
    pub async fn open_stream(&self, info: &TrackInfo) -> anyhow::Result<ByteStream> {
        self.open_stream_at(info, 0).await
    }

    fn open_stream_at<'a>(&'a self, info: &'a TrackInfo, depth: u8) -> BoxFuture<'a, anyhow::Result<ByteStream>> {
        Box::pin(async move {
            match self.plugin_target(info, depth).await? {
                Some(PlayTarget::Track(next)) => self.open_stream_at(&next, depth + 1).await,
                Some(PlayTarget::File { path }) => {
                    let file = tokio::fs::File::open(&path)
                        .await
                        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
                    let length = file.metadata().await.ok().map(|m| m.len());
                    let stream = tokio_util::io::ReaderStream::new(file).map_err(io::Error::other);
                    Ok(ByteStream::new(Box::pin(stream), length))
                }
                Some(PlayTarget::Url { url, headers, .. }) => {
                    Ok(self.http.open_with(&url, header_map(&headers)).await?)
                }
                Some(PlayTarget::Load(_)) => unreachable!("loaded by plugin_target"),
                None => self.open_stream_builtin(info).await,
            }
        })
    }

    async fn open_stream_builtin(&self, info: &TrackInfo) -> anyhow::Result<ByteStream> {
        let target = info.uri.clone().unwrap_or_else(|| info.identifier.clone());
        match info.source_name.as_str() {
            "http" => Ok(self.http.open(&target).await?),
            "youtube" => {
                let url = if is_youtube_video_id(&info.identifier) {
                    youtube_watch_url(&info.identifier)
                } else {
                    target
                };
                Ok(self.ytdlp.download(&url)?)
            }
            _ => Ok(self.ytdlp.download(&target)?),
        }
    }
}

/// Container hint from a file extension (`song.FLAC` → `flac`).
fn file_container(path: &std::path::Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| !e.is_empty())
}

/// The original track's metadata on the matched YouTube video.
fn mirrored(original: &TrackInfo, video_id: &str, length: u64) -> TrackInfo {
    TrackInfo {
        identifier: video_id.to_string(),
        is_seekable: true,
        author: original.author.clone(),
        length: if length > 0 { length } else { original.length },
        is_stream: false,
        position: original.position,
        title: original.title.clone(),
        uri: Some(youtube_watch_url(video_id)),
        artwork_url: original.artwork_url.clone(),
        isrc: original.isrc.clone(),
        source_name: "youtube".into(),
    }
}

fn search_result(tracks: Vec<TrackInfo>) -> LoadResult {
    if tracks.is_empty() {
        LoadResult::empty()
    } else {
        LoadResult::Search(tracks.into_iter().map(Track::new).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_youtube_urls() {
        let ids = |s: &str| youtube_ids(&Url::parse(s).unwrap());
        assert_eq!(
            ids("https://www.youtube.com/watch?v=dQw4w9WgXcQ").0.as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            ids("https://youtu.be/dQw4w9WgXcQ?t=3").0.as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            ids("https://youtube.com/shorts/dQw4w9WgXcQ").0.as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            ids("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123")
                .1
                .as_deref(),
            Some("PL123")
        );
        assert_eq!(
            ids("https://www.youtube.com/playlist?list=PLabc"),
            (None, Some("PLabc".into()))
        );
        assert_eq!(ids("https://www.youtube.com/watch?v=short").0, None);
    }

    #[test]
    fn load_cache_keys() {
        assert_eq!(
            load_cache_key("YTSearch:  Hello   World ").as_deref(),
            Some("ytsearch:hello world")
        );
        assert_eq!(
            load_cache_key("spsearch:Sơn Tùng").as_deref(),
            Some("spsearch:sơn tùng")
        );
        assert_eq!(load_cache_key("ytsearch:   "), None);
        assert_eq!(load_cache_key("cachesearch:x"), None);
        assert!(load_cache_key("https://open.spotify.com/track/abc").is_some());
        assert!(load_cache_key("https://www.youtube.com/watch?v=dQw4w9WgXcQ").is_some());
        assert!(load_cache_key("spotify:album:abc").is_some());
        assert_eq!(
            load_cache_key("https://radio.example/live.mp3"),
            None,
            "plain HTTP is never cached"
        );
    }

    #[test]
    fn recognizes_hosts_and_prefixes() {
        assert!(is_youtube_host(&Url::parse("https://m.youtube.com/watch?v=x").unwrap()));
        assert!(!is_youtube_host(&Url::parse("https://notyoutube.com/").unwrap()));
        assert_eq!(strip_prefix_ci("YTSEARCH:abc", "ytsearch:"), Some("abc"));
        assert!(is_youtube_video_id("dQw4w9WgXcQ"));
        assert!(!is_youtube_video_id("dQw4w9WgXc!"));
    }
}
