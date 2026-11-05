//! Thin async wrapper around the `yt-dlp` executable.
//!
//! User supplied identifiers are always passed after `--` so they can never be parsed as
//! yt-dlp options.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::TryStreamExt;
use serde::Deserialize;
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::io::ReaderStream;
use tracing::debug;

use super::{is_youtube_video_id, youtube_watch_url, ByteStream, SourceError};
use crate::{
    api::models::{TrackInfo, STREAM_LENGTH},
    config::YtDlpConfig,
};

const STDERR_TAIL: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
struct Thumbnail {
    url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct YtDlpInfo {
    #[serde(rename = "_type")]
    kind: Option<String>,
    id: Option<String>,
    title: Option<String>,
    url: Option<String>,
    webpage_url: Option<String>,
    original_url: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    artist: Option<String>,
    creator: Option<String>,
    duration: Option<f64>,
    is_live: Option<bool>,
    live_status: Option<String>,
    thumbnail: Option<String>,
    thumbnails: Option<Vec<Thumbnail>>,
    extractor_key: Option<String>,
    ie_key: Option<String>,
    isrc: Option<String>,
    entries: Option<Vec<Option<YtDlpInfo>>>,
    protocol: Option<String>,
    http_headers: Option<HashMap<String, String>>,
    ext: Option<String>,
}

/// Direct URL of the selected format, for playback without yt-dlp in the middle.
#[derive(Debug, Clone)]
pub struct MediaUrl {
    pub url: String,
    /// `https`, `m3u8_native`, `http_dash_segments`, ...
    pub protocol: String,
    pub headers: HashMap<String, String>,
    pub ext: Option<String>,
}

impl MediaUrl {
    pub fn is_hls(&self) -> bool {
        self.protocol.starts_with("m3u8")
    }

    pub fn is_plain_http(&self) -> bool {
        matches!(self.protocol.as_str(), "http" | "https")
    }
}

pub enum Resolved {
    Track(TrackInfo),
    Playlist { name: String, tracks: Vec<TrackInfo> },
}

pub struct YtDlp {
    cfg: YtDlpConfig,
    program: PathBuf,
}

impl YtDlp {
    pub fn new(cfg: YtDlpConfig) -> Self {
        let program = super::ytdlp_update::program_path(&cfg);
        Self { cfg, program }
    }

    fn program_name(&self) -> String {
        self.program.display().to_string()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .args(["--no-warnings", "--no-progress", "--ignore-config"])
            .args(&self.cfg.extra_args);
        cmd
    }

    async fn run_json(&self, args: &[&str], target: &str) -> Result<YtDlpInfo, SourceError> {
        let mut cmd = self.command();
        cmd.args(["--dump-single-json", "--skip-download"])
            .args(args)
            .arg("--")
            .arg(target);
        let child = cmd.spawn().map_err(|source| SourceError::Spawn {
            program: self.program_name(),
            source,
        })?;
        let timeout = self.cfg.metadata_timeout_secs.max(5);
        let output = tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output())
            .await
            .map_err(|_| SourceError::Timeout(timeout))??;
        if !output.status.success() {
            return Err(SourceError::Unavailable(clean_error(&output.stderr)));
        }
        serde_json::from_slice(&output.stdout).map_err(|e| SourceError::Parse(e.to_string()))
    }

    /// `ytsearch` / `scsearch` with the configured result limit.
    pub async fn search(&self, provider: &str, query: &str, flat: bool) -> Result<Vec<TrackInfo>, SourceError> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let target = format!("{provider}{}:{query}", self.cfg.search_limit.clamp(1, 50));
        let args: &[&str] = if flat { &["--flat-playlist"] } else { &[] };
        let info = self.run_json(args, &target).await?;
        let default_source = if provider == "scsearch" {
            "soundcloud"
        } else {
            "youtube"
        };
        Ok(info
            .entries
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter_map(|e| to_track_info(&e, default_source))
            .collect())
    }

    pub async fn resolve(&self, url: &str) -> Result<Resolved, SourceError> {
        let info = self.run_json(&["--flat-playlist", "--yes-playlist"], url).await?;
        if info.kind.as_deref() == Some("playlist") || info.entries.is_some() {
            let name = info.title.clone().unwrap_or_else(|| "Unknown playlist".into());
            let fallback = source_name_of(&info, "ytdlp");
            let tracks = info
                .entries
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .filter_map(|e| to_track_info(&e, &fallback))
                .collect();
            return Ok(Resolved::Playlist { name, tracks });
        }
        to_track_info(&info, "ytdlp")
            .map(Resolved::Track)
            .ok_or_else(|| SourceError::Parse("yt-dlp returned no usable track".into()))
    }

    /// Resolve the media URL of `format` (defaults to `sources.ytdlp.format`).
    pub async fn media_url(&self, url: &str, format: Option<&str>) -> Result<MediaUrl, SourceError> {
        let format = format.unwrap_or(&self.cfg.format);
        let info = self.run_json(&["-f", format, "--no-playlist"], url).await?;
        let media_url = info
            .url
            .ok_or_else(|| SourceError::Parse("yt-dlp selected a format without a direct URL".into()))?;
        Ok(MediaUrl {
            url: media_url,
            protocol: info.protocol.unwrap_or_else(|| "https".into()),
            headers: info.http_headers.unwrap_or_default(),
            ext: info.ext,
        })
    }

    /// Stream the selected audio format to stdout (`-o -`).
    pub fn download(&self, url: &str) -> Result<ByteStream, SourceError> {
        let mut cmd = self.command();
        cmd.args([
            "-f",
            &self.cfg.format,
            "--no-playlist",
            "--no-part",
            "--quiet",
            "-o",
            "-",
        ])
        .arg("--")
        .arg(url);
        let mut child = cmd.spawn().map_err(|source| SourceError::Spawn {
            program: self.program_name(),
            source,
        })?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");

        let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
        let tail_writer = Arc::clone(&tail);
        let stderr_task = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut tail = tail_writer.lock().unwrap();
                tail.extend_from_slice(&buf[..n]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                tail.drain(..excess);
            }
        });

        debug!(url, "spawned yt-dlp download");
        let stream = ReaderStream::with_capacity(stdout, 64 * 1024).map_ok(|b| b);
        Ok(ByteStream::new(Box::pin(stream), None).with_finisher(async move {
            let status = child.wait().await?;
            let _ = stderr_task.await;
            if status.success() {
                Ok(())
            } else {
                let stderr = tail.lock().unwrap().clone();
                anyhow::bail!("yt-dlp exited with {status}: {}", clean_error(&stderr))
            }
        }))
    }
}

fn clean_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find(|l| l.contains("ERROR"))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp failed without output");
    line.trim().trim_start_matches("ERROR:").trim().to_string()
}

fn source_name_of(info: &YtDlpInfo, fallback: &str) -> String {
    let key = info
        .ie_key
        .as_deref()
        .or(info.extractor_key.as_deref())
        .unwrap_or(fallback)
        .to_ascii_lowercase();
    match key.as_str() {
        k if k.starts_with("youtube") => "youtube".into(),
        k if k.starts_with("soundcloud") => "soundcloud".into(),
        // Never claim the "http"/"local" source names: they carry extra encoded data.
        "generic" | "http" | "local" => "ytdlp".into(),
        k => k.chars().filter(|c| c.is_ascii_alphanumeric()).collect(),
    }
}

fn to_track_info(info: &YtDlpInfo, default_source: &str) -> Option<TrackInfo> {
    let source_name = source_name_of(info, default_source);
    let is_stream = info.is_live == Some(true) || info.live_status.as_deref() == Some("is_live");
    let (identifier, uri) = if source_name == "youtube" {
        let id = info.id.clone().filter(|id| is_youtube_video_id(id))?;
        let uri = youtube_watch_url(&id);
        (id, uri)
    } else {
        let uri = info
            .webpage_url
            .clone()
            .or_else(|| info.original_url.clone())
            .or_else(|| info.url.clone())?;
        (uri.clone(), uri)
    };
    let artwork_url = info
        .thumbnails
        .as_ref()
        .and_then(|t| t.iter().rev().find_map(|t| t.url.clone()))
        .or_else(|| info.thumbnail.clone())
        .or_else(|| (source_name == "youtube").then(|| format!("https://i.ytimg.com/vi/{identifier}/hqdefault.jpg")));
    let length = if is_stream {
        STREAM_LENGTH
    } else {
        info.duration.map(|d| (d * 1000.0).round().max(0.0) as u64).unwrap_or(0)
    };
    Some(TrackInfo {
        identifier,
        is_seekable: !is_stream,
        author: info
            .artist
            .clone()
            .or_else(|| info.uploader.clone())
            .or_else(|| info.channel.clone())
            .or_else(|| info.creator.clone())
            .unwrap_or_else(|| "Unknown artist".into()),
        length,
        is_stream,
        position: 0,
        title: info.title.clone().unwrap_or_else(|| "Unknown title".into()),
        uri: Some(uri),
        artwork_url,
        isrc: info.isrc.clone(),
        source_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_flat_youtube_search_entry() {
        let raw = r#"{"_type":"playlist","entries":[{"_type":"url","ie_key":"Youtube","id":"dQw4w9WgXcQ",
            "url":"https://www.youtube.com/watch?v=dQw4w9WgXcQ","title":"Never Gonna Give You Up",
            "duration":212.0,"channel":"Rick Astley","live_status":null,
            "thumbnails":[{"url":"https://i.ytimg.com/a.jpg"},{"url":"https://i.ytimg.com/b.jpg"}]}]}"#;
        let info: YtDlpInfo = serde_json::from_str(raw).unwrap();
        let entry = info.entries.unwrap().into_iter().flatten().next().unwrap();
        let track = to_track_info(&entry, "youtube").unwrap();
        assert_eq!(track.identifier, "dQw4w9WgXcQ");
        assert_eq!(track.source_name, "youtube");
        assert_eq!(track.length, 212_000);
        assert_eq!(track.author, "Rick Astley");
        assert_eq!(track.artwork_url.as_deref(), Some("https://i.ytimg.com/b.jpg"));
    }

    #[test]
    fn generic_extractor_never_claims_http() {
        let info = YtDlpInfo {
            extractor_key: Some("Generic".into()),
            webpage_url: Some("https://example.com/page".into()),
            title: Some("x".into()),
            is_live: Some(true),
            ..Default::default()
        };
        let track = to_track_info(&info, "ytdlp").unwrap();
        assert_eq!(track.source_name, "ytdlp");
        assert!(track.is_stream);
        assert_eq!(track.length, STREAM_LENGTH);
    }

    #[test]
    fn extracts_error_line() {
        let stderr = b"[youtube] abc: Downloading\nERROR: [youtube] abc: Video unavailable\n";
        assert_eq!(clean_error(stderr), "[youtube] abc: Video unavailable");
    }
}
