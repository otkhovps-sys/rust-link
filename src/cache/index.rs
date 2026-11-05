//! SQLite index of cached audio files and their OneDrive backup state.

use std::{path::Path, str::FromStr, time::Duration};

use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    FromRow, SqlitePool,
};

use crate::{api::models::TrackInfo, util::now_millis};

#[derive(Debug, Clone, FromRow)]
pub struct CacheEntry {
    pub video_id: String,
    pub source_name: String,
    pub identifier: String,
    pub uri: Option<String>,
    pub title: String,
    pub author: String,
    pub artwork_url: Option<String>,
    pub isrc: Option<String>,
    pub file_path: String,
    pub file_size: i64,
    pub duration_ms: i64,
    pub sha256: Option<String>,
    pub container: Option<String>,
    pub is_local: bool,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: i64,
    pub onedrive_synced: bool,
    pub onedrive_item_id: Option<String>,
    pub onedrive_path: Option<String>,
    pub synced_at: Option<i64>,
    pub sync_attempts: i64,
    pub next_sync_at: i64,
    pub last_sync_error: Option<String>,
}

impl CacheEntry {
    pub fn to_track_info(&self) -> TrackInfo {
        TrackInfo {
            identifier: self.identifier.clone(),
            is_seekable: true,
            author: self.author.clone(),
            length: self.duration_ms.max(0) as u64,
            is_stream: false,
            position: 0,
            title: self.title.clone(),
            uri: self.uri.clone(),
            artwork_url: self.artwork_url.clone(),
            isrc: self.isrc.clone(),
            source_name: self.source_name.clone(),
        }
    }
}

/// Values written after a successful download from the original source.
#[derive(Debug, Clone)]
pub struct NewCacheEntry {
    pub video_id: String,
    pub info: TrackInfo,
    pub file_path: String,
    pub file_size: i64,
    pub duration_ms: i64,
    pub sha256: String,
    pub container: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct StoredUploadSession {
    pub video_id: String,
    pub remote_path: String,
    pub upload_url: String,
    pub file_size: i64,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheSummary {
    pub entries: i64,
    pub local_entries: i64,
    pub local_bytes: i64,
    pub synced_entries: i64,
    pub pending_sync: i64,
    pub cold_entries: i64,
}

#[derive(Clone)]
pub struct CacheIndex {
    pool: SqlitePool,
}

const SELECT_ENTRY: &str = "SELECT video_id, source_name, identifier, uri, title, author, \
    artwork_url, isrc, file_path, file_size, duration_ms, sha256, container, is_local, \
    created_at, last_accessed_at, access_count, onedrive_synced, onedrive_item_id, \
    onedrive_path, synced_at, sync_attempts, next_sync_at, last_sync_error FROM audio_cache";

impl CacheIndex {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn get(&self, video_id: &str) -> sqlx::Result<Option<CacheEntry>> {
        sqlx::query_as::<_, CacheEntry>(&format!("{SELECT_ENTRY} WHERE video_id = ?"))
            .bind(video_id)
            .fetch_optional(&self.pool)
            .await
    }

    /// Insert or refresh an entry after the file was fully written to disk.
    pub async fn upsert_downloaded(&self, e: &NewCacheEntry) -> sqlx::Result<()> {
        let now = now_millis();
        sqlx::query(
            "INSERT INTO audio_cache (video_id, source_name, identifier, uri, title, author, \
                artwork_url, isrc, file_path, file_size, duration_ms, sha256, container, \
                is_local, created_at, last_accessed_at, access_count, onedrive_synced, \
                sync_attempts, next_sync_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, 1, 0, 0, 0) \
             ON CONFLICT(video_id) DO UPDATE SET \
                source_name = excluded.source_name, identifier = excluded.identifier, \
                uri = excluded.uri, title = excluded.title, author = excluded.author, \
                artwork_url = COALESCE(excluded.artwork_url, audio_cache.artwork_url), \
                isrc = COALESCE(excluded.isrc, audio_cache.isrc), \
                file_path = excluded.file_path, file_size = excluded.file_size, \
                duration_ms = CASE WHEN excluded.duration_ms > 0 THEN excluded.duration_ms \
                                   ELSE audio_cache.duration_ms END, \
                sha256 = excluded.sha256, container = excluded.container, is_local = 1, \
                last_accessed_at = excluded.last_accessed_at, \
                access_count = audio_cache.access_count + 1, \
                onedrive_synced = 0, sync_attempts = 0, next_sync_at = 0, last_sync_error = NULL",
        )
        .bind(&e.video_id)
        .bind(&e.info.source_name)
        .bind(&e.info.identifier)
        .bind(&e.info.uri)
        .bind(&e.info.title)
        .bind(&e.info.author)
        .bind(&e.info.artwork_url)
        .bind(&e.info.isrc)
        .bind(&e.file_path)
        .bind(e.file_size)
        .bind(e.duration_ms)
        .bind(&e.sha256)
        .bind(&e.container)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark an entry as present on disk again after a OneDrive cold-storage pull.
    pub async fn mark_restored(&self, video_id: &str, file_size: i64) -> sqlx::Result<()> {
        sqlx::query(
            "UPDATE audio_cache SET is_local = 1, file_size = ?, last_accessed_at = ?, \
             access_count = access_count + 1 WHERE video_id = ?",
        )
        .bind(file_size)
        .bind(now_millis())
        .bind(video_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn touch(&self, video_id: &str) -> sqlx::Result<()> {
        sqlx::query(
            "UPDATE audio_cache SET last_accessed_at = ?, access_count = access_count + 1 \
             WHERE video_id = ?",
        )
        .bind(now_millis())
        .bind(video_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// LRU eviction of a backed-up file: keep the row, flag it as cold.
    pub async fn mark_evicted(&self, video_id: &str) -> sqlx::Result<()> {
        sqlx::query("UPDATE audio_cache SET is_local = 0 WHERE video_id = ?")
            .bind(video_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete(&self, video_id: &str) -> sqlx::Result<()> {
        sqlx::query("DELETE FROM audio_cache WHERE video_id = ?")
            .bind(video_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Total bytes of files currently on local disk.
    pub async fn local_bytes(&self) -> sqlx::Result<u64> {
        let bytes: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(file_size), 0) FROM audio_cache WHERE is_local = 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(bytes.max(0) as u64)
    }

    /// Least recently used local files first.
    pub async fn lru_candidates(&self, limit: i64, offset: i64) -> sqlx::Result<Vec<CacheEntry>> {
        sqlx::query_as::<_, CacheEntry>(&format!(
            "{SELECT_ENTRY} WHERE is_local = 1 ORDER BY last_accessed_at ASC, video_id ASC \
             LIMIT ? OFFSET ?"
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn local_entries(&self) -> sqlx::Result<Vec<CacheEntry>> {
        sqlx::query_as::<_, CacheEntry>(&format!("{SELECT_ENTRY} WHERE is_local = 1"))
            .fetch_all(&self.pool)
            .await
    }

    /// Remove rows that are neither on disk nor in OneDrive.
    pub async fn purge_orphans(&self) -> sqlx::Result<u64> {
        Ok(
            sqlx::query("DELETE FROM audio_cache WHERE is_local = 0 AND onedrive_synced = 0")
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    pub async fn pending_sync(&self, limit: u32, max_attempts: u32) -> sqlx::Result<Vec<CacheEntry>> {
        sqlx::query_as::<_, CacheEntry>(&format!(
            "{SELECT_ENTRY} WHERE onedrive_synced = 0 AND is_local = 1 AND next_sync_at <= ? \
             AND sync_attempts < ? ORDER BY created_at ASC LIMIT ?"
        ))
        .bind(now_millis())
        .bind(max_attempts as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn mark_synced(&self, video_id: &str, item_id: &str, remote_path: &str) -> sqlx::Result<()> {
        sqlx::query(
            "UPDATE audio_cache SET onedrive_synced = 1, onedrive_item_id = ?, onedrive_path = ?, \
             synced_at = ?, last_sync_error = NULL, sync_attempts = 0, next_sync_at = 0 \
             WHERE video_id = ?",
        )
        .bind(item_id)
        .bind(remote_path)
        .bind(now_millis())
        .bind(video_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_sync_failed(&self, video_id: &str, error: &str, retry_in: Duration) -> sqlx::Result<()> {
        sqlx::query(
            "UPDATE audio_cache SET sync_attempts = sync_attempts + 1, last_sync_error = ?, \
             next_sync_at = ? WHERE video_id = ?",
        )
        .bind(crate::util::truncate_utf8(error, 1000))
        .bind(now_millis() + retry_in.as_millis() as i64)
        .bind(video_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upload_session(&self, video_id: &str) -> sqlx::Result<Option<StoredUploadSession>> {
        sqlx::query_as::<_, StoredUploadSession>(
            "SELECT video_id, remote_path, upload_url, file_size, expires_at \
             FROM onedrive_upload_sessions WHERE video_id = ?",
        )
        .bind(video_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn save_upload_session(&self, s: &StoredUploadSession) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO onedrive_upload_sessions \
                (video_id, remote_path, upload_url, file_size, expires_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(video_id) DO UPDATE SET remote_path = excluded.remote_path, \
                upload_url = excluded.upload_url, file_size = excluded.file_size, \
                expires_at = excluded.expires_at, created_at = excluded.created_at",
        )
        .bind(&s.video_id)
        .bind(&s.remote_path)
        .bind(&s.upload_url)
        .bind(s.file_size)
        .bind(&s.expires_at)
        .bind(now_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_upload_session(&self, video_id: &str) -> sqlx::Result<()> {
        sqlx::query("DELETE FROM onedrive_upload_sessions WHERE video_id = ?")
            .bind(video_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Title / author search over cached metadata (`cachesearch:` prefix).
    pub async fn search(&self, query: &str, limit: i64) -> sqlx::Result<Vec<CacheEntry>> {
        let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        sqlx::query_as::<_, CacheEntry>(&format!(
            "{SELECT_ENTRY} WHERE title LIKE ?1 ESCAPE '\\' OR author LIKE ?1 ESCAPE '\\' \
             ORDER BY access_count DESC, last_accessed_at DESC LIMIT ?2"
        ))
        .bind(pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn summary(&self) -> sqlx::Result<CacheSummary> {
        let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), \
                COALESCE(SUM(is_local), 0), \
                COALESCE(SUM(CASE WHEN is_local = 1 THEN file_size ELSE 0 END), 0), \
                COALESCE(SUM(onedrive_synced), 0), \
                COALESCE(SUM(CASE WHEN onedrive_synced = 0 AND is_local = 1 THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN is_local = 0 THEN 1 ELSE 0 END), 0) \
             FROM audio_cache",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(CacheSummary {
            entries: row.0,
            local_entries: row.1,
            local_bytes: row.2,
            synced_entries: row.3,
            pending_sync: row.4,
            cold_entries: row.5,
        })
    }

    /// Consistent online copy of the database (`VACUUM INTO`).
    pub async fn snapshot_to(&self, target: &Path) -> sqlx::Result<()> {
        let _ = tokio::fs::remove_file(target).await;
        sqlx::query("VACUUM INTO ?")
            .bind(target.to_string_lossy().to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// YouTube video previously matched to a mirrored track: `(video_id, length_ms)`.
    pub async fn get_mirror(&self, source: &str, identifier: &str) -> sqlx::Result<Option<(String, i64)>> {
        sqlx::query_as("SELECT video_id, length_ms FROM mirror_map WHERE source = ? AND identifier = ?")
            .bind(source)
            .bind(identifier)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn put_mirror(&self, source: &str, identifier: &str, video_id: &str, length_ms: i64) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO mirror_map (source, identifier, video_id, length_ms, resolved_at) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(source, identifier) DO UPDATE SET video_id = excluded.video_id, \
             length_ms = excluded.length_ms, resolved_at = excluded.resolved_at",
        )
        .bind(source)
        .bind(identifier)
        .bind(video_id)
        .bind(length_ms)
        .bind(crate::util::now_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn state_get(&self, key: &str) -> sqlx::Result<Option<String>> {
        sqlx::query_scalar("SELECT value FROM node_state WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn state_set(&self, key: &str, value: &str) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO node_state (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Force a timestamp (used by tests and maintenance tooling).
    pub async fn set_last_accessed(&self, video_id: &str, at: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE audio_cache SET last_accessed_at = ? WHERE video_id = ?")
            .bind(at)
            .bind(video_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}
