//! Track resolution (`/v4/loadtracks`) and raw audio byte streams for the cache.

pub mod http;
pub mod ytdlp;

use std::{
    future::Future,
    io::{self, Read},
    sync::Mutex,
};

use bytes::{Buf, Bytes};
use futures_util::{future::BoxFuture, stream::BoxStream, StreamExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

use crate::{
    api::models::{LoadResult, PlaylistData, PlaylistInfo, Severity, Track, TrackInfo},
    cache::CacheIndex,
    config::SourcesConfig,
};

use self::{
    http::HttpSource,
    ytdlp::{Resolved, YtDlp},
};

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
}

impl SourceManager {
    pub fn new(cfg: &SourcesConfig, index: Option<CacheIndex>) -> anyhow::Result<Self> {
        Ok(Self {
            ytdlp: YtDlp::new(cfg.ytdlp.clone()),
            http: HttpSource::new(cfg.http_timeout_secs)?,
            cfg: cfg.clone(),
            index,
        })
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
        if self.index.is_some() {
            names.push("cache".to_string());
        }
        names
    }

    pub async fn load_tracks(&self, identifier: &str) -> LoadResult {
        let identifier = identifier.trim();
        if identifier.is_empty() {
            return LoadResult::empty();
        }
        match self.load_inner(identifier).await {
            Ok(result) => result,
            Err(e) => {
                warn!(identifier, error = %e, "track loading failed");
                LoadResult::error(e.to_string(), e.severity())
            }
        }
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

    /// Raw bytes of the audio file behind a track.
    pub async fn open_stream(&self, info: &TrackInfo) -> anyhow::Result<ByteStream> {
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
    fn recognizes_hosts_and_prefixes() {
        assert!(is_youtube_host(&Url::parse("https://m.youtube.com/watch?v=x").unwrap()));
        assert!(!is_youtube_host(&Url::parse("https://notyoutube.com/").unwrap()));
        assert_eq!(strip_prefix_ci("YTSEARCH:abc", "ytsearch:"), Some("abc"));
        assert!(is_youtube_video_id("dQw4w9WgXcQ"));
        assert!(!is_youtube_video_id("dQw4w9WgXc!"));
    }
}
