-- Results of /v4/loadtracks for searches and music-service links, reused until they expire
-- (sources.loadCacheTtlSecs) so repeated searches skip yt-dlp and the remote APIs.
CREATE TABLE IF NOT EXISTS load_cache (
    identifier TEXT PRIMARY KEY,
    result TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
