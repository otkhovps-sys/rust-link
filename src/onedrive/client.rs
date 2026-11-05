//! Microsoft Graph client for OneDrive: path addressing, simple PUT uploads, resumable
//! chunked upload sessions and streaming downloads.

use std::{future::Future, io, path::Path, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::TryStreamExt;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::{header, Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{debug, info, warn};

use super::{auth::TokenManager, quickxor, OneDriveError};
use crate::{config::OneDriveConfig, source::ByteStream, util::backoff};

/// Characters escaped inside a single path segment of a Graph item path.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b':')
    .add(b'/')
    .add(b'\\')
    .add(b'|')
    .add(b'&')
    .add(b'+');

const MAX_RETRIES: u32 = 6;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    pub quick_xor_hash: Option<String>,
    pub sha1_hash: Option<String>,
    pub sha256_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileFacet {
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DriveItem {
    pub id: String,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub file: Option<FileFacet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSession {
    pub upload_url: String,
    #[serde(default)]
    pub expiration_date_time: Option<String>,
    #[serde(default)]
    pub next_expected_ranges: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadStatus {
    #[serde(default)]
    next_expected_ranges: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DriveInfo {
    #[serde(rename = "driveType")]
    pub drive_type: Option<String>,
    pub owner: Option<serde_json::Value>,
    pub quota: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GraphErrorEnvelope {
    error: GraphErrorBody,
}

#[derive(Debug, Deserialize)]
struct GraphErrorBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

pub struct OneDriveClient {
    http: reqwest::Client,
    transfer: reqwest::Client,
    tokens: Arc<TokenManager>,
    base_url: String,
    remote_folder: String,
    chunk_size: u64,
    simple_upload_max: u64,
}

impl OneDriveClient {
    pub fn new(cfg: &OneDriveConfig, tokens: Arc<TokenManager>) -> anyhow::Result<Self> {
        let timeout = Duration::from_secs(cfg.request_timeout_secs.max(10));
        let http = reqwest::Client::builder()
            .user_agent(concat!("rust-link/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(timeout)
            .build()?;
        // Upload/download URLs are pre-authenticated: never attach the bearer token there.
        let transfer = reqwest::Client::builder()
            .user_agent(concat!("rust-link/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(timeout)
            .build()?;
        Ok(Self {
            http,
            transfer,
            tokens,
            base_url: cfg.graph_base_url.trim_end_matches('/').to_string(),
            remote_folder: cfg.remote_folder.trim_matches('/').to_string(),
            chunk_size: cfg.normalized_chunk_size(),
            simple_upload_max: cfg.simple_upload_max.as_u64().min(4 * 1024 * 1024),
        })
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// `AudioCache/{relative}`.
    pub fn remote_path(&self, relative: &str) -> String {
        format!("{}/{}", self.remote_folder, relative.trim_start_matches('/'))
    }

    fn item_url(&self, remote_path: &str, suffix: &str) -> String {
        let encoded = remote_path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
            .collect::<Vec<_>>()
            .join("/");
        if suffix.is_empty() {
            format!("{}/me/drive/root:/{encoded}", self.base_url)
        } else {
            format!("{}/me/drive/root:/{encoded}:{suffix}", self.base_url)
        }
    }

    /// Authenticated Graph request with token refresh on 401 and retries on throttling.
    async fn send_graph<F>(&self, build: F) -> Result<Response, OneDriveError>
    where
        F: Fn(&reqwest::Client) -> RequestBuilder,
    {
        let mut attempt = 0u32;
        let mut refreshed = false;
        loop {
            let token = self.tokens.access_token().await?;
            let result = build(&self.http).bearer_auth(token).send().await;
            match result {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    }
                    if status == StatusCode::UNAUTHORIZED && !refreshed {
                        refreshed = true;
                        self.tokens.invalidate().await;
                        continue;
                    }
                    if is_retryable(status) && attempt < MAX_RETRIES {
                        let wait = retry_after(&response).unwrap_or_else(|| default_backoff(attempt));
                        warn!(%status, ?wait, "graph request throttled, retrying");
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(api_error(response).await);
                }
                Err(e) if is_transient(&e) && attempt < MAX_RETRIES => {
                    let wait = default_backoff(attempt);
                    warn!(error = %e, ?wait, "graph request failed, retrying");
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub async fn drive_info(&self) -> Result<DriveInfo, OneDriveError> {
        let url = format!("{}/me/drive", self.base_url);
        Ok(self.send_graph(|c| c.get(&url)).await?.json().await?)
    }

    pub async fn get_item(&self, remote_path: &str) -> Result<Option<DriveItem>, OneDriveError> {
        let url = self.item_url(remote_path, "");
        match self.send_graph(|c| c.get(&url)).await {
            Ok(response) => Ok(Some(response.json().await?)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Upload a local file to `remote_path`, replacing any existing item.
    ///
    /// Small files use a single PUT; larger files go through an upload session whose URL is
    /// reported via `on_session` (so it can be persisted and resumed after a restart) and
    /// `on_session(None)` once the session is finished or abandoned.
    pub async fn upload_file<F, Fut>(
        &self,
        local: &Path,
        remote_path: &str,
        resume: Option<UploadSession>,
        mut on_session: F,
    ) -> Result<DriveItem, OneDriveError>
    where
        F: FnMut(Option<UploadSession>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let size = tokio::fs::metadata(local).await?.len();
        let local_hash = quickxor::hash_file(local).await?;
        let item = if size <= self.simple_upload_max {
            if resume.is_some() {
                on_session(None).await;
            }
            self.upload_small(local, remote_path).await?
        } else {
            let item = self
                .upload_large(local, remote_path, size, resume, &mut on_session)
                .await?;
            on_session(None).await;
            item
        };
        verify_upload(&item, size, &local_hash)?;
        Ok(item)
    }

    async fn upload_small(&self, local: &Path, remote_path: &str) -> Result<DriveItem, OneDriveError> {
        let body = Bytes::from(tokio::fs::read(local).await?);
        let url = self.item_url(remote_path, "/content");
        let response = self
            .send_graph(|c| {
                c.put(&url)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(body.clone())
            })
            .await?;
        Ok(response.json().await?)
    }

    pub async fn create_upload_session(&self, remote_path: &str) -> Result<UploadSession, OneDriveError> {
        let url = self.item_url(remote_path, "/createUploadSession");
        let body = serde_json::json!({
            "item": { "@microsoft.graph.conflictBehavior": "replace" }
        });
        let response = self.send_graph(|c| c.post(&url).json(&body)).await?;
        let session: UploadSession = response.json().await?;
        debug!(remote_path, "created upload session");
        Ok(session)
    }

    /// Ask the upload session which byte range it expects next.
    async fn session_offset(&self, upload_url: &str) -> Result<Option<u64>, OneDriveError> {
        let response = self.transfer.get(upload_url).send().await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Err(OneDriveError::SessionExpired);
        }
        if !status.is_success() {
            return Err(api_error(response).await);
        }
        let st: UploadStatus = response.json().await?;
        Ok(first_expected_offset(&st.next_expected_ranges))
    }

    async fn upload_large<F, Fut>(
        &self,
        local: &Path,
        remote_path: &str,
        size: u64,
        resume: Option<UploadSession>,
        on_session: &mut F,
    ) -> Result<DriveItem, OneDriveError>
    where
        F: FnMut(Option<UploadSession>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let mut offset = 0u64;
        let mut session = None;
        if let Some(previous) = resume {
            match self.session_offset(&previous.upload_url).await {
                Ok(next) => {
                    offset = next.unwrap_or(size);
                    info!(remote_path, offset, size, "resuming OneDrive upload session");
                    session = Some(previous);
                }
                Err(e) => {
                    debug!(remote_path, error = %e, "stored upload session unusable, starting over");
                    on_session(None).await;
                }
            }
        }
        let mut session = match session {
            Some(s) => s,
            None => {
                let s = self.create_upload_session(remote_path).await?;
                on_session(Some(s.clone())).await;
                s
            }
        };

        let mut file = tokio::fs::File::open(local).await?;
        let mut failures = 0u32;
        let mut buf = Vec::with_capacity(self.chunk_size as usize);
        loop {
            if offset >= size {
                // Every byte was accepted without a final response: check where we stand.
                match self.session_offset(&session.upload_url).await {
                    Ok(Some(next)) if next < size => {
                        offset = next;
                        continue;
                    }
                    Ok(_) | Err(OneDriveError::SessionExpired) => {
                        return self
                            .get_item(remote_path)
                            .await?
                            .filter(|item| item.size == Some(size))
                            .ok_or_else(|| OneDriveError::Integrity("upload finished without a drive item".into()));
                    }
                    Err(e) => return Err(e),
                }
            }

            let end = (offset + self.chunk_size).min(size);
            buf.resize((end - offset) as usize, 0);
            file.seek(io::SeekFrom::Start(offset)).await?;
            file.read_exact(&mut buf).await?;

            let result = self
                .transfer
                .put(&session.upload_url)
                .header(header::CONTENT_LENGTH, buf.len())
                .header(header::CONTENT_RANGE, format!("bytes {}-{}/{}", offset, end - 1, size))
                .body(buf.clone())
                .send()
                .await;

            match result {
                Ok(response) => match response.status() {
                    StatusCode::OK | StatusCode::CREATED => {
                        return Ok(response.json::<DriveItem>().await?);
                    }
                    StatusCode::ACCEPTED => {
                        failures = 0;
                        let st: UploadStatus = response.json().await.unwrap_or(UploadStatus {
                            next_expected_ranges: Vec::new(),
                        });
                        offset = first_expected_offset(&st.next_expected_ranges).unwrap_or(end);
                        debug!(remote_path, offset, size, "chunk accepted");
                    }
                    StatusCode::NOT_FOUND => {
                        warn!(remote_path, "upload session expired, creating a new one");
                        on_session(None).await;
                        session = self.create_upload_session(remote_path).await?;
                        on_session(Some(session.clone())).await;
                        offset = 0;
                    }
                    StatusCode::RANGE_NOT_SATISFIABLE => {
                        offset = self.session_offset(&session.upload_url).await?.unwrap_or(size);
                    }
                    status if is_retryable(status) => {
                        failures += 1;
                        if failures > MAX_RETRIES {
                            return Err(api_error(response).await);
                        }
                        let wait = retry_after(&response).unwrap_or_else(|| default_backoff(failures));
                        warn!(remote_path, %status, ?wait, "chunk upload failed, retrying");
                        tokio::time::sleep(wait).await;
                        if let Ok(Some(next)) = self.session_offset(&session.upload_url).await {
                            offset = next;
                        }
                    }
                    _ => return Err(api_error(response).await),
                },
                Err(e) if is_transient(&e) || e.is_body() => {
                    failures += 1;
                    if failures > MAX_RETRIES {
                        return Err(e.into());
                    }
                    let wait = default_backoff(failures);
                    warn!(remote_path, error = %e, ?wait, "chunk upload interrupted, resuming");
                    tokio::time::sleep(wait).await;
                    if let Ok(Some(next)) = self.session_offset(&session.upload_url).await {
                        offset = next;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub async fn cancel_upload_session(&self, upload_url: &str) {
        let _ = self.transfer.delete(upload_url).send().await;
    }

    /// Stream a file back from OneDrive. Graph answers `/content` with a redirect to a
    /// pre-authenticated URL; reqwest drops the Authorization header on cross-host redirects.
    pub async fn download_stream(&self, remote_path: &str) -> Result<ByteStream, OneDriveError> {
        let url = self.item_url(remote_path, "/content");
        let response = self.send_graph(|c| c.request(Method::GET, &url)).await?;
        let length = response.content_length();
        let stream = response.bytes_stream().map_err(io::Error::other);
        Ok(ByteStream::new(Box::pin(stream), length))
    }

    /// Download into `target` through a temporary file.
    pub async fn download_to_file(&self, remote_path: &str, target: &Path) -> Result<u64, OneDriveError> {
        use tokio::io::AsyncWriteExt;
        let mut stream = self.download_stream(remote_path).await?;
        let tmp = target.with_extension("download");
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut written = 0u64;
        while let Some(chunk) = stream.next_chunk().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, target).await?;
        Ok(written)
    }
}

fn verify_upload(item: &DriveItem, size: u64, local_hash: &str) -> Result<(), OneDriveError> {
    if let Some(remote_size) = item.size {
        if remote_size != size {
            return Err(OneDriveError::Integrity(format!(
                "size mismatch: local {size}, remote {remote_size}"
            )));
        }
    }
    let remote_hash = item
        .file
        .as_ref()
        .and_then(|f| f.hashes.as_ref())
        .and_then(|h| h.quick_xor_hash.as_deref());
    if let Some(remote_hash) = remote_hash {
        if remote_hash != local_hash {
            return Err(OneDriveError::Integrity(format!(
                "quickXorHash mismatch: local {local_hash}, remote {remote_hash}"
            )));
        }
    }
    Ok(())
}

/// Smallest start offset among ranges such as `["26214400-", "0-1023"]`.
pub fn first_expected_offset(ranges: &[String]) -> Option<u64> {
    ranges
        .iter()
        .filter_map(|r| r.split('-').next()?.trim().parse::<u64>().ok())
        .min()
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn is_transient(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

fn default_backoff(attempt: u32) -> Duration {
    backoff(attempt, Duration::from_secs(1), Duration::from_secs(60))
}

fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|secs| Duration::from_secs(secs.min(300)))
}

async fn api_error(response: Response) -> OneDriveError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let (code, message) = serde_json::from_str::<GraphErrorEnvelope>(&body)
        .map(|e| (e.error.code, e.error.message))
        .unwrap_or_else(|_| (String::new(), crate::util::truncate_utf8(&body, 300).to_string()));
    OneDriveError::Api { status, code, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_next_expected_ranges() {
        assert_eq!(first_expected_offset(&["26214400-".into()]), Some(26_214_400));
        assert_eq!(first_expected_offset(&["500-999".into(), "12-55".into()]), Some(12));
        assert_eq!(first_expected_offset(&[]), None);
    }
}
