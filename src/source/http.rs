//! Direct HTTP(S) audio files and Icecast/SHOUTcast style radio streams.

use std::{io, time::Duration};

use futures_util::TryStreamExt;
use reqwest::header;
use url::Url;

use super::{ByteStream, SourceError};
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

    /// Returns track info when the URL serves audio, `None` when it does not look like audio.
    pub async fn probe(&self, url: &Url) -> Result<Option<TrackInfo>, SourceError> {
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

        let title = icy_name.unwrap_or_else(|| {
            url.path_segments()
                .and_then(|mut s| s.next_back())
                .filter(|s| !s.is_empty())
                .map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned())
                .unwrap_or_else(|| url.to_string())
        });
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

    pub async fn open(&self, url: &str) -> Result<ByteStream, SourceError> {
        let response = self.client.get(url).send().await?.error_for_status()?;
        let length = response.content_length();
        let stream = response.bytes_stream().map_err(io::Error::other);
        Ok(ByteStream::new(Box::pin(stream), length))
    }
}
