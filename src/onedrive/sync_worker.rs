//! Background backup worker.
//!
//! Periodically uploads cache entries with `onedrive_synced = 0`, flips the flag after a
//! verified upload and backs up a consistent snapshot of the SQLite index. On startup the
//! index can be restored from that snapshot when the local database is missing.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures_util::{stream, StreamExt};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::{client::UploadSession, OneDriveClient};
use crate::{
    cache::{index::StoredUploadSession, CacheEntry, CacheManager},
    config::OneDriveConfig,
    util::{backoff, now_millis},
};

/// Remote location of the index snapshot, relative to `remoteFolder`.
pub const SNAPSHOT_RELATIVE_PATH: &str = "_meta/cache_index.sqlite";
const LAST_SNAPSHOT_KEY: &str = "onedrive.last_snapshot_at";
const NOTIFY_DEBOUNCE: Duration = Duration::from_secs(10);
const MIN_SNAPSHOT_GAP_MS: i64 = 60_000;

#[derive(Debug, Default, Clone, Copy)]
pub struct SyncReport {
    pub uploaded: usize,
    pub failed: usize,
}

pub struct SyncWorker {
    cache: Arc<CacheManager>,
    client: Arc<OneDriveClient>,
    cfg: OneDriveConfig,
    db_path: PathBuf,
}

impl SyncWorker {
    pub fn new(cache: Arc<CacheManager>, client: Arc<OneDriveClient>, cfg: OneDriveConfig, db_path: PathBuf) -> Self {
        Self {
            cache,
            client,
            cfg,
            db_path,
        }
    }

    pub async fn run(self, shutdown: CancellationToken) {
        let mut ticker = tokio::time::interval(Duration::from_secs(self.cfg.sync_interval_secs.max(10)));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut uploaded_since_snapshot = 0usize;
        info!(folder = %self.cfg.remote_folder, "OneDrive backup worker started");
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
                _ = self.cache.sync_notify().notified() => {
                    // Batch bursts of newly cached tracks into one cycle.
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(NOTIFY_DEBOUNCE) => {}
                    }
                }
            }

            match self.sync_pending(&shutdown).await {
                Ok(report) => {
                    uploaded_since_snapshot += report.uploaded;
                    if report.uploaded + report.failed > 0 {
                        info!(
                            uploaded = report.uploaded,
                            failed = report.failed,
                            "OneDrive sync cycle finished"
                        );
                    }
                }
                Err(e) => error!(%e, "OneDrive sync cycle failed"),
            }

            if self.snapshot_due(uploaded_since_snapshot).await {
                match self.upload_snapshot().await {
                    Ok(()) => uploaded_since_snapshot = 0,
                    Err(e) => warn!(%e, "index snapshot backup failed"),
                }
            }
        }
        info!("OneDrive backup worker stopped");
    }

    async fn snapshot_due(&self, uploaded_since_snapshot: usize) -> bool {
        let last: i64 = self
            .cache
            .index()
            .state_get(LAST_SNAPSHOT_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let elapsed = now_millis() - last;
        let interval_ms = self.cfg.snapshot_interval_secs.max(60) as i64 * 1000;
        elapsed >= interval_ms || (uploaded_since_snapshot > 0 && elapsed >= MIN_SNAPSHOT_GAP_MS)
    }

    /// Upload every pending entry (bounded concurrency).
    pub async fn sync_pending(&self, shutdown: &CancellationToken) -> anyhow::Result<SyncReport> {
        let mut report = SyncReport::default();
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let batch = self
                .cache
                .index()
                .pending_sync(self.cfg.sync_batch_size.max(1), self.cfg.max_sync_attempts)
                .await?;
            if batch.is_empty() {
                break;
            }
            let results: Vec<bool> = stream::iter(batch)
                .map(|entry| self.upload_entry(entry))
                .buffer_unordered(self.cfg.max_concurrent_uploads.max(1))
                .collect()
                .await;
            let ok = results.iter().filter(|r| **r).count();
            report.uploaded += ok;
            report.failed += results.len() - ok;
            // Failed rows get a future `next_sync_at`, so the loop always terminates.
        }
        Ok(report)
    }

    async fn upload_entry(&self, entry: CacheEntry) -> bool {
        let key = entry.video_id.clone();
        let index = self.cache.index().clone();
        let _lease = self.cache.pin(&key);
        let local = self.cache.absolute_path(&entry.file_path);
        let remote = self.client.remote_path(&entry.file_path);

        if !tokio::fs::try_exists(&local).await.unwrap_or(false) {
            let _ = index
                .mark_sync_failed(&key, "local file missing", Duration::from_secs(3600))
                .await;
            return false;
        }

        let resume = match index.upload_session(&key).await {
            Ok(Some(s)) if s.remote_path == remote && s.file_size == entry.file_size => Some(UploadSession {
                upload_url: s.upload_url,
                expiration_date_time: s.expires_at,
                next_expected_ranges: Vec::new(),
            }),
            _ => None,
        };

        let session_index = index.clone();
        let session_key = key.clone();
        let session_remote = remote.clone();
        let file_size = entry.file_size;
        let result = self
            .client
            .upload_file(&local, &remote, resume, move |session| {
                let index = session_index.clone();
                let key = session_key.clone();
                let remote = session_remote.clone();
                async move {
                    let outcome = match session {
                        Some(s) => {
                            index
                                .save_upload_session(&StoredUploadSession {
                                    video_id: key,
                                    remote_path: remote,
                                    upload_url: s.upload_url,
                                    file_size,
                                    expires_at: s.expiration_date_time,
                                })
                                .await
                        }
                        None => index.delete_upload_session(&key).await,
                    };
                    if let Err(e) = outcome {
                        warn!(%e, "cannot persist upload session state");
                    }
                }
            })
            .await;

        match result {
            Ok(item) => {
                if let Err(e) = index.mark_synced(&key, &item.id, &remote).await {
                    error!(%key, %e, "uploaded but cannot update index");
                    return false;
                }
                let _ = index.delete_upload_session(&key).await;
                info!(%key, %remote, size = entry.file_size, "backed up to OneDrive");
                true
            }
            Err(e) => {
                let retry_in = backoff(
                    entry.sync_attempts as u32,
                    Duration::from_secs(30),
                    Duration::from_secs(6 * 3600),
                );
                warn!(%key, error = %e, ?retry_in, "OneDrive upload failed");
                let _ = index.mark_sync_failed(&key, &e.to_string(), retry_in).await;
                false
            }
        }
    }

    /// Back up a consistent copy of the SQLite index (`VACUUM INTO`).
    pub async fn upload_snapshot(&self) -> anyhow::Result<()> {
        let snapshot = self.db_path.with_extension("snapshot.sqlite");
        self.cache.index().snapshot_to(&snapshot).await?;
        let remote = self.client.remote_path(SNAPSHOT_RELATIVE_PATH);
        let result = self.client.upload_file(&snapshot, &remote, None, |_| async {}).await;
        let _ = tokio::fs::remove_file(&snapshot).await;
        result?;
        self.cache
            .index()
            .state_set(LAST_SNAPSHOT_KEY, &now_millis().to_string())
            .await?;
        info!(%remote, "index snapshot backed up to OneDrive");
        Ok(())
    }
}

/// Download the index snapshot when the local database does not exist yet.
/// Returns `true` when a snapshot was restored.
pub async fn restore_index_if_missing(client: &OneDriveClient, db_path: &Path) -> anyhow::Result<bool> {
    if tokio::fs::try_exists(db_path).await.unwrap_or(false) {
        return Ok(false);
    }
    let remote = client.remote_path(SNAPSHOT_RELATIVE_PATH);
    match client.download_to_file(&remote, db_path).await {
        Ok(bytes) => {
            info!(%remote, bytes, "restored cache index snapshot from OneDrive");
            Ok(true)
        }
        Err(e) if e.is_not_found() => {
            info!("no index snapshot on OneDrive yet, starting with an empty index");
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}
