//! Cache lookups, downloads into the cache and OneDrive cold-storage pulls.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, bail, Context};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::Notify};
use tracing::{debug, info, warn};

use super::{
    cache_key_for, index::NewCacheEntry, relative_path_for, AcquiredSource, CacheIndex, CacheLease, CacheOutcome,
    DownloadProgress, PinTable, PlayableSource,
};
use crate::{
    api::models::TrackInfo, audio::decoder, config::CacheConfig, onedrive::OneDriveClient, source::SourceManager,
};

#[derive(Debug, Clone)]
enum Origin {
    OneDrive {
        remote_path: String,
        expected_sha256: Option<String>,
    },
    Source,
}

pub struct CacheManager {
    cfg: CacheConfig,
    index: CacheIndex,
    sources: Arc<SourceManager>,
    onedrive: Option<Arc<OneDriveClient>>,
    inflight: Mutex<HashMap<String, Arc<DownloadProgress>>>,
    pins: Arc<PinTable>,
    lru_notify: Notify,
    sync_notify: Notify,
}

impl CacheManager {
    pub fn new(
        cfg: CacheConfig,
        index: CacheIndex,
        sources: Arc<SourceManager>,
        onedrive: Option<Arc<OneDriveClient>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            index,
            sources,
            onedrive,
            inflight: Mutex::new(HashMap::new()),
            pins: Arc::new(PinTable::default()),
            lru_notify: Notify::new(),
            sync_notify: Notify::new(),
        })
    }

    pub fn config(&self) -> &CacheConfig {
        &self.cfg
    }

    pub fn index(&self) -> &CacheIndex {
        &self.index
    }

    pub fn onedrive(&self) -> Option<&Arc<OneDriveClient>> {
        self.onedrive.as_ref()
    }

    pub fn absolute_path(&self, relative: &str) -> PathBuf {
        self.cfg.directory.join(relative)
    }

    pub fn pin(&self, key: &str) -> CacheLease {
        self.pins.pin(key)
    }

    pub fn is_pinned(&self, key: &str) -> bool {
        self.pins.is_pinned(key)
    }

    pub fn is_inflight(&self, key: &str) -> bool {
        self.inflight.lock().unwrap().contains_key(key)
    }

    pub fn lru_notify(&self) -> &Notify {
        &self.lru_notify
    }

    pub fn sync_notify(&self) -> &Notify {
        &self.sync_notify
    }

    /// Resolve a playable source for a track, downloading it into the cache when needed.
    ///
    /// 1. local hit -> serve the file and refresh `last_accessed_at`
    /// 2. evicted but backed up -> pull from OneDrive (cold storage), source as fallback
    /// 3. miss -> download from the original source
    pub async fn acquire(self: &Arc<Self>, info: &TrackInfo) -> anyhow::Result<AcquiredSource> {
        if info.is_stream || !self.cfg.enabled {
            let stream = self.sources.open_stream(info).await?;
            return Ok(AcquiredSource {
                source: PlayableSource::Stream(stream),
                lease: None,
                outcome: CacheOutcome::Bypass,
            });
        }

        let key = cache_key_for(info);
        let lease = self.pins.pin(&key);
        if let Some(progress) = self.inflight_get(&key) {
            return self.deliver(progress, lease, CacheOutcome::Joined).await;
        }

        let mut relative = relative_path_for(&key, &self.cfg.file_extension);
        let mut origins = vec![Origin::Source];
        let mut outcome = CacheOutcome::Miss;

        if let Some(entry) = self.index.get(&key).await? {
            let path = self.absolute_path(&entry.file_path);
            if entry.is_local {
                if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    self.index.touch(&key).await?;
                    debug!(%key, "cache hit");
                    return Ok(AcquiredSource {
                        source: PlayableSource::File {
                            path,
                            container: entry.container.clone(),
                        },
                        lease: Some(lease),
                        outcome: CacheOutcome::Hit,
                    });
                }
                warn!(%key, path = %path.display(), "indexed file is missing from disk");
                if entry.onedrive_synced {
                    self.index.mark_evicted(&key).await?;
                } else {
                    self.index.delete(&key).await?;
                }
            }
            if entry.onedrive_synced {
                if let Some(onedrive) = &self.onedrive {
                    let remote_path = entry
                        .onedrive_path
                        .clone()
                        .unwrap_or_else(|| onedrive.remote_path(&entry.file_path));
                    origins.insert(
                        0,
                        Origin::OneDrive {
                            remote_path,
                            expected_sha256: entry.sha256.clone(),
                        },
                    );
                    outcome = CacheOutcome::ColdStorage;
                }
            }
            relative = entry.file_path.clone();
        }

        let (progress, joined) = self.start_download(&key, info, relative, origins);
        let outcome = if joined { CacheOutcome::Joined } else { outcome };
        info!(%key, ?outcome, title = %info.title, "fetching track into cache");
        self.deliver(progress, lease, outcome).await
    }

    fn inflight_get(&self, key: &str) -> Option<Arc<DownloadProgress>> {
        self.inflight.lock().unwrap().get(key).cloned()
    }

    async fn deliver(
        &self,
        progress: Arc<DownloadProgress>,
        lease: CacheLease,
        outcome: CacheOutcome,
    ) -> anyhow::Result<AcquiredSource> {
        // Surface early failures (yt-dlp errors, 404, ...) as acquisition errors.
        let state = progress
            .wait_until(|s| s.written > 0 || s.done || s.error.is_some())
            .await;
        if let Some(error) = state.error {
            bail!(error);
        }
        let source = if self.cfg.progressive_playback {
            PlayableSource::Progressive { progress }
        } else {
            progress.wait_done().await.map_err(|e| anyhow!(e))?;
            PlayableSource::File {
                path: progress.final_path.clone(),
                container: None,
            }
        };
        Ok(AcquiredSource {
            source,
            lease: Some(lease),
            outcome,
        })
    }

    fn start_download(
        self: &Arc<Self>,
        key: &str,
        info: &TrackInfo,
        relative: String,
        origins: Vec<Origin>,
    ) -> (Arc<DownloadProgress>, bool) {
        let mut inflight = self.inflight.lock().unwrap();
        if let Some(existing) = inflight.get(key) {
            return (Arc::clone(existing), true);
        }
        let final_path = self.absolute_path(&relative);
        let part_path = PathBuf::from(format!("{}.part", final_path.display()));
        let progress = Arc::new(DownloadProgress::new(part_path, final_path));
        inflight.insert(key.to_string(), Arc::clone(&progress));
        drop(inflight);

        let this = Arc::clone(self);
        let task_progress = Arc::clone(&progress);
        let key = key.to_string();
        let info = info.clone();
        tokio::spawn(async move {
            this.run_download(key, info, relative, origins, task_progress).await;
        });
        (progress, false)
    }

    async fn run_download(
        self: Arc<Self>,
        key: String,
        info: TrackInfo,
        relative: String,
        origins: Vec<Origin>,
        progress: Arc<DownloadProgress>,
    ) {
        let _lease = self.pin(&key);
        let mut last_error = None;
        let mut fetched = None;
        for origin in origins {
            match self.fetch_into(&origin, &info, &progress).await {
                Ok(result) => {
                    fetched = Some((origin, result));
                    break;
                }
                Err(error) => {
                    warn!(%key, ?origin, %error, "download attempt failed");
                    last_error = Some(error);
                    if progress.snapshot().written > 0 {
                        // A reader may already hold partial bytes: no silent fallback.
                        break;
                    }
                    progress.reset();
                }
            }
        }

        let result = match fetched {
            Some((origin, (size, sha256))) => {
                self.finalize(&key, &info, &relative, origin, size, sha256, &progress)
                    .await
            }
            None => Err(last_error.unwrap_or_else(|| anyhow!("no download origin available"))),
        };

        match result {
            Ok(()) => progress.complete(),
            Err(error) => {
                let _ = tokio::fs::remove_file(&progress.part_path).await;
                warn!(%key, %error, "caching failed");
                progress.fail(format!("{error:#}"));
            }
        }
        self.inflight.lock().unwrap().remove(&key);
    }

    async fn fetch_into(
        &self,
        origin: &Origin,
        info: &TrackInfo,
        progress: &DownloadProgress,
    ) -> anyhow::Result<(u64, String)> {
        let part = &progress.part_path;
        if let Some(parent) = part.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut stream = match origin {
            Origin::OneDrive { remote_path, .. } => {
                let onedrive = self
                    .onedrive
                    .as_ref()
                    .ok_or_else(|| anyhow!("onedrive is not configured"))?;
                onedrive.download_stream(remote_path).await?
            }
            Origin::Source => self.sources.open_stream(info).await?,
        };
        progress.set_total(stream.content_length);

        let max = self.cfg.max_file_size.as_u64();
        let mut file = tokio::fs::File::create(part)
            .await
            .with_context(|| format!("cannot create {}", part.display()))?;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        while let Some(chunk) = stream.next_chunk().await {
            let chunk = chunk?;
            written += chunk.len() as u64;
            if written > max {
                bail!("file exceeds cache.maxFileSize ({})", self.cfg.max_file_size);
            }
            file.write_all(&chunk).await?;
            // Flush so the progressive reader (separate handle) sees the bytes.
            file.flush().await?;
            hasher.update(&chunk);
            progress.set_written(written);
        }
        stream.finish().await?;
        file.sync_all().await?;
        if written == 0 {
            bail!("source returned no audio data");
        }
        let sha256 = hex::encode(hasher.finalize());
        if let Origin::OneDrive {
            expected_sha256: Some(expected),
            ..
        } = origin
        {
            if !expected.eq_ignore_ascii_case(&sha256) {
                bail!("sha256 mismatch after OneDrive pull (expected {expected}, got {sha256})");
            }
        }
        Ok((written, sha256))
    }

    #[allow(clippy::too_many_arguments)]
    async fn finalize(
        &self,
        key: &str,
        info: &TrackInfo,
        relative: &str,
        origin: Origin,
        size: u64,
        sha256: String,
        progress: &DownloadProgress,
    ) -> anyhow::Result<()> {
        tokio::fs::rename(&progress.part_path, &progress.final_path).await?;
        match origin {
            Origin::OneDrive { .. } => {
                self.index.mark_restored(key, size as i64).await?;
                info!(%key, size, "restored from OneDrive cold storage");
            }
            Origin::Source => {
                let path = progress.final_path.clone();
                let container = decoder::detect_container(&path).map(str::to_string);
                let duration_ms = if info.length > 0 && info.length < crate::api::models::STREAM_LENGTH {
                    info.length as i64
                } else {
                    tokio::task::spawn_blocking(move || decoder::probe_duration_ms(&path))
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(0) as i64
                };
                self.index
                    .upsert_downloaded(&NewCacheEntry {
                        video_id: key.to_string(),
                        info: info.clone(),
                        file_path: relative.to_string(),
                        file_size: size as i64,
                        duration_ms,
                        sha256,
                        container,
                    })
                    .await?;
                info!(%key, size, "cached new track");
                self.sync_notify.notify_one();
            }
        }
        self.lru_notify.notify_one();
        Ok(())
    }

    /// Startup consistency pass between the index and the files on disk.
    pub async fn reconcile(&self) -> anyhow::Result<()> {
        let removed_parts = remove_part_files(&self.cfg.directory).await;
        let mut missing = 0;
        for entry in self.index.local_entries().await? {
            let path = self.absolute_path(&entry.file_path);
            if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                continue;
            }
            missing += 1;
            if entry.onedrive_synced {
                self.index.mark_evicted(&entry.video_id).await?;
            } else {
                self.index.delete(&entry.video_id).await?;
            }
        }
        let orphans = self.index.purge_orphans().await?;
        let summary = self.index.summary().await?;
        info!(
            removed_parts,
            missing,
            orphans,
            entries = summary.entries,
            local = summary.local_entries,
            local_bytes = summary.local_bytes,
            cold = summary.cold_entries,
            "cache index reconciled"
        );
        Ok(())
    }
}

async fn remove_part_files(root: &Path) -> usize {
    let mut removed = 0;
    let mut stack = vec![(root.to_path_buf(), 0u8)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(kind) = entry.file_type().await else {
                continue;
            };
            if kind.is_dir() && depth < 2 {
                stack.push((path, depth + 1));
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "part") {
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => warn!(path = %path.display(), %e, "cannot remove stale part file"),
                }
            }
        }
    }
    removed
}
