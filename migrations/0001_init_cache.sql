-- Chỉ mục cache audio cục bộ + trạng thái backup OneDrive.
-- video_id: YouTube video ID, hoặc SHA-256 (hex) của URL nguồn với các nguồn khác.
CREATE TABLE IF NOT EXISTS audio_cache (
    video_id          TEXT PRIMARY KEY NOT NULL,
    source_name       TEXT    NOT NULL,
    identifier        TEXT    NOT NULL,
    uri               TEXT,
    title             TEXT    NOT NULL DEFAULT 'Unknown title',
    author            TEXT    NOT NULL DEFAULT 'Unknown artist',
    artwork_url       TEXT,
    isrc              TEXT,
    -- Đường dẫn tương đối so với cache.directory, dạng "{id_prefix}/{video_id}.opus"
    file_path         TEXT    NOT NULL,
    file_size         INTEGER NOT NULL DEFAULT 0,
    duration_ms       INTEGER NOT NULL DEFAULT 0,
    sha256            TEXT,
    -- Container thực tế nhận diện từ magic bytes (webm, ogg, mp4, mp3, flac, wav...)
    container         TEXT,
    -- 1 = file đang nằm trên đĩa cục bộ, 0 = đã bị LRU dọn (chỉ còn trên OneDrive)
    is_local          INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL,
    last_accessed_at  INTEGER NOT NULL,
    access_count      INTEGER NOT NULL DEFAULT 0,
    onedrive_synced   INTEGER NOT NULL DEFAULT 0,
    onedrive_item_id  TEXT,
    onedrive_path     TEXT,
    synced_at         INTEGER,
    sync_attempts     INTEGER NOT NULL DEFAULT 0,
    next_sync_at      INTEGER NOT NULL DEFAULT 0,
    last_sync_error   TEXT
);

CREATE INDEX IF NOT EXISTS idx_audio_cache_lru
    ON audio_cache (is_local, last_accessed_at);

CREATE INDEX IF NOT EXISTS idx_audio_cache_sync
    ON audio_cache (onedrive_synced, is_local, next_sync_at);

-- Upload session đang dở của Microsoft Graph, cho phép resume sau khi node khởi động lại.
CREATE TABLE IF NOT EXISTS onedrive_upload_sessions (
    video_id     TEXT PRIMARY KEY NOT NULL
                 REFERENCES audio_cache (video_id) ON DELETE CASCADE,
    remote_path  TEXT    NOT NULL,
    upload_url   TEXT    NOT NULL,
    file_size    INTEGER NOT NULL,
    expires_at   TEXT,
    created_at   INTEGER NOT NULL
);

-- Key/value nội bộ (thời điểm snapshot gần nhất, ...).
CREATE TABLE IF NOT EXISTS node_state (
    key    TEXT PRIMARY KEY NOT NULL,
    value  TEXT NOT NULL
);
