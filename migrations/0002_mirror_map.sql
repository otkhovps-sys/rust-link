-- YouTube video chosen for a Spotify / Apple Music / Deezer track (keyed by source id and
-- by ISRC), so the search runs once per song.
CREATE TABLE IF NOT EXISTS mirror_map (
    source TEXT NOT NULL,
    identifier TEXT NOT NULL,
    video_id TEXT NOT NULL,
    length_ms INTEGER NOT NULL DEFAULT 0,
    resolved_at INTEGER NOT NULL,
    PRIMARY KEY (source, identifier)
);
