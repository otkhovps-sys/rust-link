//! Local audio cache: SQLite index, on-disk files, LRU eviction and cold-storage pulls.

pub mod index;
pub mod lru;
pub mod manager;
pub mod progressive;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use tokio::sync::Notify;

pub use index::{CacheEntry, CacheIndex, CacheSummary};
pub use manager::CacheManager;

use crate::{api::models::TrackInfo, source::ByteStream, util::sha256_hex};

/// Cache key: the YouTube video ID, or SHA-256 of the source URL for anything else.
pub fn cache_key_for(info: &TrackInfo) -> String {
    if info.source_name == "youtube" && crate::source::is_youtube_video_id(&info.identifier) {
        return info.identifier.clone();
    }
    sha256_hex(info.uri.as_deref().unwrap_or(&info.identifier))
}

/// `{id_prefix}/{video_id}.{ext}`, relative to the cache directory.
pub fn relative_path_for(key: &str, extension: &str) -> String {
    let prefix: String = key.chars().take(2).collect();
    format!("{prefix}/{key}.{extension}")
}

#[derive(Debug, Clone, Default)]
pub struct ProgressState {
    pub written: u64,
    pub total: Option<u64>,
    pub done: bool,
    pub error: Option<String>,
}

/// Shared progress of an in-flight download, observable from async tasks and from the
/// blocking decoder thread.
#[derive(Debug)]
pub struct DownloadProgress {
    pub part_path: PathBuf,
    pub final_path: PathBuf,
    state: Mutex<ProgressState>,
    cond: Condvar,
    notify: Notify,
}

impl DownloadProgress {
    pub fn new(part_path: PathBuf, final_path: PathBuf) -> Self {
        Self {
            part_path,
            final_path,
            state: Mutex::new(ProgressState::default()),
            cond: Condvar::new(),
            notify: Notify::new(),
        }
    }

    fn update(&self, f: impl FnOnce(&mut ProgressState)) {
        {
            let mut state = self.state.lock().unwrap();
            f(&mut state);
        }
        self.cond.notify_all();
        self.notify.notify_waiters();
    }

    pub fn snapshot(&self) -> ProgressState {
        self.state.lock().unwrap().clone()
    }

    pub fn set_total(&self, total: Option<u64>) {
        self.update(|s| s.total = total);
    }

    pub fn reset(&self) {
        self.update(|s| *s = ProgressState::default());
    }

    pub fn set_written(&self, written: u64) {
        self.update(|s| s.written = written);
    }

    pub fn complete(&self) {
        self.update(|s| {
            s.total = Some(s.written);
            s.done = true;
        });
    }

    pub fn fail(&self, error: impl Into<String>) {
        let error = error.into();
        self.update(|s| s.error = Some(error));
    }

    /// Block the current (non-async) thread until `ready` holds or `timeout` elapses.
    pub fn wait_blocking(&self, ready: impl Fn(&ProgressState) -> bool, timeout: Duration) -> ProgressState {
        let guard = self.state.lock().unwrap();
        let (guard, _) = self.cond.wait_timeout_while(guard, timeout, |s| !ready(s)).unwrap();
        guard.clone()
    }

    /// Wait asynchronously until `ready` holds.
    pub async fn wait_until(&self, ready: impl Fn(&ProgressState) -> bool) -> ProgressState {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = self.snapshot();
            if ready(&state) {
                return state;
            }
            notified.await;
        }
    }

    pub async fn wait_done(&self) -> Result<(), String> {
        let state = self.wait_until(|s| s.done || s.error.is_some()).await;
        match state.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Reference counted set of cache keys that must not be evicted (playing or uploading).
#[derive(Debug, Default)]
pub struct PinTable {
    pins: Mutex<HashMap<String, usize>>,
}

impl PinTable {
    pub fn pin(self: &Arc<Self>, key: &str) -> CacheLease {
        *self.pins.lock().unwrap().entry(key.to_string()).or_default() += 1;
        CacheLease {
            key: key.to_string(),
            table: Arc::clone(self),
        }
    }

    pub fn is_pinned(&self, key: &str) -> bool {
        self.pins.lock().unwrap().contains_key(key)
    }
}

/// While alive, the LRU worker skips the pinned key.
#[derive(Debug)]
pub struct CacheLease {
    key: String,
    table: Arc<PinTable>,
}

impl CacheLease {
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        let mut pins = self.table.pins.lock().unwrap();
        if let Some(count) = pins.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                pins.remove(&self.key);
            }
        }
    }
}

/// Where the playback pipeline reads audio from.
pub enum PlayableSource {
    /// Complete file in the local cache.
    File { path: PathBuf, container: Option<String> },
    /// File still being written (source download or OneDrive pull).
    Progressive { progress: Arc<DownloadProgress> },
    /// Live stream that is never cached.
    Stream(ByteStream),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// Served from the local disk.
    Hit,
    /// Pulled back from OneDrive after an LRU eviction.
    ColdStorage,
    /// Downloaded from the original source.
    Miss,
    /// Attached to a download already in progress.
    Joined,
    /// Not cacheable (live stream or cache disabled).
    Bypass,
}

pub struct AcquiredSource {
    pub source: PlayableSource,
    pub lease: Option<CacheLease>,
    pub outcome: CacheOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_paths() {
        let mut info = TrackInfo {
            identifier: "dQw4w9WgXcQ".into(),
            is_seekable: true,
            author: String::new(),
            length: 0,
            is_stream: false,
            position: 0,
            title: String::new(),
            uri: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            artwork_url: None,
            isrc: None,
            source_name: "youtube".into(),
        };
        assert_eq!(cache_key_for(&info), "dQw4w9WgXcQ");
        assert_eq!(relative_path_for("dQw4w9WgXcQ", "opus"), "dQ/dQw4w9WgXcQ.opus");
        info.source_name = "http".into();
        let key = cache_key_for(&info);
        assert_eq!(key.len(), 64);
        assert_eq!(key, sha256_hex("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
    }

    #[test]
    fn leases_are_reference_counted() {
        let table = Arc::new(PinTable::default());
        let a = table.pin("x");
        let b = table.pin("x");
        drop(a);
        assert!(table.is_pinned("x"));
        drop(b);
        assert!(!table.is_pinned("x"));
    }
}
