//! Direct HTTP(S) audio files and Icecast/SHOUTcast style radio streams.

use std::{io, time::Duration};

use futures_util::TryStreamExt;
use reqwest::header;
use url::Url;

use super::{hls, ByteStream, SourceError};
use crate::api::models::{TrackInfo, STREAM_LENGTH};

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "oga", "opus", "m4a", "mp4", "aac", "webm", "mka", "mkv",
];

pub struct HttpSource {
    client: reqwest::Client,
}

impl HttpSource {
    pub fn new(timeout_secs: u64) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("rust-link/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(timeout_secs.max(1)))
            .read_timeout(Duration::from_secs(timeout_secs.max(1) * 3))
            .build()?;
        Ok(Self { client })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Size of the resource, when the server reports it.
    pub async fn content_length(&self, url: &str) -> Option<u64> {
        let response = self.client.head(url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
    }

    async fn probe_hls(&self, url: &Url) -> Result<TrackInfo, SourceError> {
        let playlist = hls::probe(&self.client, url)
            .await
            .map_err(|e| SourceError::Unavailable(format!("{e:#}")))?;
        let is_stream = !playlist.ended;
        Ok(TrackInfo {
            identifier: url.to_string(),
            is_seekable: false,
            author: "Unknown artist".into(),
            length: if is_stream {
                STREAM_LENGTH
            } else {
                playlist.duration_ms()
            },
            is_stream,
            position: 0,
            title: title_from_url(url),
            uri: Some(url.to_string()),
            artwork_url: None,
            isrc: None,
            source_name: "http".into(),
        })
    }

    /// Returns track info when the URL serves audio, `None` when it does not look like audio.
    pub async fn probe(&self, url: &Url) -> Result<Option<TrackInfo>, SourceError> {
        if hls::is_hls_url(url) {
            return self.probe_hls(url).await.map(Some);
        }
        let response = match self.client.get(url.clone()).send().await {
            Ok(r) => r,
            Err(e) if e.is_connect() || e.is_timeout() => return Err(e.into()),
            Err(_) => return Ok(None),
        };
        if !response.status().is_success() {
            return Ok(None);
        }
        let headers = response.headers();
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.contains("mpegurl") {
            drop(response);
            return self.probe_hls(url).await.map(Some);
        }
        let extension = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .and_then(|name| name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()));
        let audio_extension = extension.as_deref().is_some_and(|e| AUDIO_EXTENSIONS.contains(&e));
        let is_audio = content_type.starts_with("audio/")
            || content_type.starts_with("application/ogg")
            || ((content_type.is_empty() || content_type.starts_with("application/octet-stream")) && audio_extension);
        if !is_audio {
            return Ok(None);
        }
        let icy_name = headers
            .get("icy-name")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let is_stream = icy_name.is_some() || headers.contains_key("icy-br") || response.content_length().is_none();
        drop(response);

        let title = icy_name.unwrap_or_else(|| title_from_url(url));
        Ok(Some(TrackInfo {
            identifier: url.to_string(),
            is_seekable: !is_stream,
            author: "Unknown artist".into(),
            length: if is_stream { STREAM_LENGTH } else { 0 },
            is_stream,
            position: 0,
            title,
            uri: Some(url.to_string()),
            artwork_url: None,
            isrc: None,
            source_name: "http".into(),
        }))
    }

    /// Audio bytes of `url`; HLS playlists are followed segment by segment.
    pub async fn open(&self, url: &str) -> Result<ByteStream, SourceError> {
        self.open_with(url, header::HeaderMap::new()).await
    }

    /// [`Self::open`] with extra request headers (plugin URLs).
    pub async fn open_with(&self, url: &str, headers: header::HeaderMap) -> Result<ByteStream, SourceError> {
        if let Ok(parsed) = Url::parse(url) {
            if hls::is_hls_url(&parsed) {
                return Ok(hls::open(self.client.clone(), parsed, headers));
            }
        }
        let response = self
            .client
            .get(url)
            .headers(headers.clone())
            .send()
            .await?
            .error_for_status()?;
        let is_hls = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("mpegurl"));
        if is_hls {
            let final_url = response.url().clone();
            drop(response);
            return Ok(hls::open(self.client.clone(), final_url, headers));
        }
        let length = response.content_length();
        let stream = response.bytes_stream().map_err(io::Error::other);
        Ok(ByteStream::new(Box::pin(stream), length))
    }
}

fn title_from_url(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned())
        .unwrap_or_else(|| url.to_string())
}
