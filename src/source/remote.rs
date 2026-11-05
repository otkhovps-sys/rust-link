//! Seekable HTTP media read in `Range` chunks, for tracks streamed without caching
//! (longer than `cache.maxTrackDurationSecs`, or larger than `cache.maxFileSize`).
//!
//! The decoder thread reads synchronously; each chunk is fetched on the tokio runtime and
//! the next chunk is prefetched in the background, so playback does not wait on the
//! network. Expired signed URLs (YouTube) are refreshed once on 403/410.

use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail};
use bytes::Bytes;
use futures_util::future::BoxFuture;
use reqwest::{
    header::{HeaderMap, CONTENT_RANGE, RANGE},
    StatusCode,
};
use symphonia::core::io::MediaSource;
use tokio::{runtime::Handle, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::debug;

pub const CHUNK_SIZE: u64 = 1024 * 1024;
const ATTEMPTS: u32 = 4;

/// Produces a fresh media URL when the current one expired.
pub type UrlRefresher = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync>;

type RangeResult = anyhow::Result<(Bytes, Option<u64>)>;

pub struct RemoteMedia {
    pub url: String,
    pub headers: HeaderMap,
    pub refresh: Option<UrlRefresher>,
}

struct Fetcher {
    client: reqwest::Client,
    url: Mutex<String>,
    headers: HeaderMap,
    refresh: Option<UrlRefresher>,
}

impl Fetcher {
    /// Bytes `start..=end` plus the total length from `Content-Range`.
    async fn range(&self, start: u64, end: u64) -> RangeResult {
        let mut refreshed = false;
        let mut last_error = anyhow!("no attempt made");
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
            }
            let url = self.url.lock().unwrap().clone();
            let response = self
                .client
                .get(&url)
                .headers(self.headers.clone())
                .header(RANGE, format!("bytes={start}-{end}"))
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(e) => {
                    last_error = e.into();
                    continue;
                }
            };
            match response.status() {
                StatusCode::PARTIAL_CONTENT => {
                    let total = response
                        .headers()
                        .get(CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.rsplit('/').next())
                        .and_then(|v| v.parse().ok());
                    match response.bytes().await {
                        Ok(bytes) => return Ok((bytes, total)),
                        Err(e) => last_error = e.into(),
                    }
                }
                StatusCode::FORBIDDEN | StatusCode::GONE if !refreshed && self.refresh.is_some() => {
                    refreshed = true;
                    debug!("media URL expired, resolving a new one");
                    let refresh = self.refresh.as_ref().expect("checked");
                    *self.url.lock().unwrap() = refresh().await?;
                }
                StatusCode::OK => bail!("server ignores Range requests"),
                status if status.is_client_error() => bail!("media request failed with {status}"),
                status => last_error = anyhow!("media request failed with {status}"),
            }
        }
        Err(last_error)
    }
}

pub struct RangedReader {
    handle: Handle,
    fetcher: Arc<Fetcher>,
    length: u64,
    position: u64,
    chunk_start: u64,
    chunk: Bytes,
    prefetch: Option<(u64, JoinHandle<RangeResult>)>,
    cancel: CancellationToken,
}

impl RangedReader {
    /// Fetch the first chunk. `Ok(None)`: the server does not support ranges.
    pub async fn open(client: reqwest::Client, media: RemoteMedia) -> anyhow::Result<Option<Self>> {
        let fetcher = Arc::new(Fetcher {
            client,
            url: Mutex::new(media.url),
            headers: media.headers,
            refresh: media.refresh,
        });
        let (chunk, total) = match fetcher.range(0, CHUNK_SIZE - 1).await {
            Ok(result) => result,
            Err(e) if e.to_string().contains("ignores Range") => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(length) = total else {
            return Ok(None);
        };
        let mut reader = Self {
            handle: Handle::current(),
            fetcher,
            length,
            position: 0,
            chunk_start: 0,
            chunk,
            prefetch: None,
            cancel: CancellationToken::new(),
        };
        reader.schedule_prefetch();
        Ok(Some(reader))
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    /// Stop blocking reads when playback stops.
    pub fn attach_cancel(&mut self, cancel: CancellationToken) {
        self.cancel = cancel;
    }

    fn schedule_prefetch(&mut self) {
        let next = self.chunk_start + CHUNK_SIZE;
        if next >= self.length || self.prefetch.as_ref().is_some_and(|(start, _)| *start == next) {
            return;
        }
        if let Some((_, task)) = self.prefetch.take() {
            task.abort();
        }
        let fetcher = Arc::clone(&self.fetcher);
        let end = (next + CHUNK_SIZE).min(self.length) - 1;
        let task = self.handle.spawn(async move { fetcher.range(next, end).await });
        self.prefetch = Some((next, task));
    }

    fn load_chunk(&mut self, start: u64) -> io::Result<()> {
        let end = (start + CHUNK_SIZE).min(self.length) - 1;
        let task = match self.prefetch.take() {
            Some((prefetched, task)) if prefetched == start => task,
            other => {
                if let Some((_, task)) = other {
                    task.abort();
                }
                let fetcher = Arc::clone(&self.fetcher);
                self.handle.spawn(async move { fetcher.range(start, end).await })
            }
        };
        let cancel = self.cancel.clone();
        let result = self.handle.block_on(async move {
            tokio::select! {
                _ = cancel.cancelled() => Err(anyhow!("playback stopped")),
                joined = task => joined.map_err(|e| anyhow!(e)).and_then(|r| r),
            }
        });
        let (bytes, _) = result.map_err(|e| io::Error::other(format!("{e:#}")))?;
        if bytes.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty range response"));
        }
        self.chunk_start = start;
        self.chunk = bytes;
        self.schedule_prefetch();
        Ok(())
    }
}

impl Drop for RangedReader {
    fn drop(&mut self) {
        if let Some((_, task)) = self.prefetch.take() {
            task.abort();
        }
    }
}

impl Read for RangedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.length || buf.is_empty() {
            return Ok(0);
        }
        let chunk_end = self.chunk_start + self.chunk.len() as u64;
        if self.position < self.chunk_start || self.position >= chunk_end {
            self.load_chunk(self.position / CHUNK_SIZE * CHUNK_SIZE)?;
        }
        let offset = (self.position - self.chunk_start) as usize;
        let available = &self.chunk[offset.min(self.chunk.len())..];
        let n = buf.len().min(available.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for RangedReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(delta) => self.length as i128 + delta as i128,
            SeekFrom::Current(delta) => self.position as i128 + delta as i128,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.position = (target as u64).min(self.length);
        Ok(self.position)
    }
}

impl MediaSource for RangedReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}
