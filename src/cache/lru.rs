//! LRU eviction worker.
//!
//! When local usage exceeds `maxSize * highWatermark`, files with the oldest
//! `last_accessed_at` are deleted from disk until usage drops below
//! `maxSize * lowWatermark`. Rows of files already backed up to OneDrive are kept
//! (`is_local = 0`) so they can be pulled back on demand.

use std::{io, sync::Arc, time::Duration};

use serde::Serialize;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::CacheManager;

const PAGE: i64 = 128;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvictionReport {
    pub usage_before: u64,
    pub usage_after: u64,
    pub high_watermark_bytes: u64,
    pub low_watermark_bytes: u64,
    pub evicted_files: u32,
    pub freed_bytes: u64,
    pub dropped_rows: u32,
    pub skipped_unsynced: u32,
    pub skipped_in_use: u32,
}

impl CacheManager {
    pub async fn enforce_capacity(&self) -> anyhow::Result<EvictionReport> {
        let cfg = self.config();
        let max = cfg.max_size.as_u64() as f64;
        let mut report = EvictionReport {
            high_watermark_bytes: (max * cfg.high_watermark) as u64,
            low_watermark_bytes: (max * cfg.low_watermark) as u64,
            ..Default::default()
        };
        let usage = self.index().local_bytes().await?;
        report.usage_before = usage;
        report.usage_after = usage;
        if usage <= report.high_watermark_bytes {
            return Ok(report);
        }

        let backup_enabled = self.onedrive().is_some();
        let mut current = usage;
        // Rows skipped stay in the `is_local = 1` set, so the page offset only advances by them.
        let mut offset = 0i64;
        'scan: loop {
            let batch = self.index().lru_candidates(PAGE, offset).await?;
            if batch.is_empty() {
                break;
            }
            for entry in batch {
                if current <= report.low_watermark_bytes {
                    break 'scan;
                }
                let key = entry.video_id.as_str();
                if self.is_pinned(key) || self.is_inflight(key) {
                    report.skipped_in_use += 1;
                    offset += 1;
                    continue;
                }
                if !entry.onedrive_synced && backup_enabled && !cfg.evict_unsynced {
                    report.skipped_unsynced += 1;
                    offset += 1;
                    continue;
                }
                let path = self.absolute_path(&entry.file_path);
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        warn!(key, path = %path.display(), %e, "cannot delete cached file");
                        offset += 1;
                        continue;
                    }
                }
                if entry.onedrive_synced {
                    self.index().mark_evicted(key).await?;
                } else {
                    // No backup exists: drop the row entirely.
                    self.index().delete(key).await?;
                    report.dropped_rows += 1;
                }
                let size = entry.file_size.max(0) as u64;
                current = current.saturating_sub(size);
                report.freed_bytes += size;
                report.evicted_files += 1;
                debug!(key, size, synced = entry.onedrive_synced, "evicted from local cache");
            }
        }
        report.usage_after = current;
        Ok(report)
    }
}

pub async fn run_lru_worker(cache: Arc<CacheManager>, shutdown: CancellationToken) {
    let period = Duration::from_secs(cache.config().lru_interval_secs.max(5));
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    info!(max_size = %cache.config().max_size, "LRU eviction worker started");
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
            _ = cache.lru_notify().notified() => {}
        }
        match cache.enforce_capacity().await {
            Ok(report) if report.evicted_files > 0 => info!(
                evicted = report.evicted_files,
                freed = report.freed_bytes,
                usage = report.usage_after,
                "LRU eviction pass finished"
            ),
            Ok(report) if report.usage_after > report.high_watermark_bytes => warn!(
                usage = report.usage_after,
                skipped_unsynced = report.skipped_unsynced,
                skipped_in_use = report.skipped_in_use,
                "cache above high watermark but nothing evictable (waiting for OneDrive sync?)"
            ),
            Ok(_) => {}
            Err(e) => error!(%e, "LRU eviction pass failed"),
        }
    }
    info!("LRU eviction worker stopped");
}
