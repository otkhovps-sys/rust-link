use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{bail, Context};
use serde::{de, Deserialize, Deserializer};

/// Root configuration, loaded from `config/application.yml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppConfig {
    pub server: ServerConfig,
    pub lavalink: LavalinkConfig,
    pub audio: AudioConfig,
    pub sources: SourcesConfig,
    pub cache: CacheConfig,
    pub onedrive: OneDriveConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerConfig {
    pub address: String,
    pub port: u16,
    pub password: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            address: "0.0.0.0".into(),
            port: 2333,
            password: "youshallnotpass".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LavalinkConfig {
    /// Seconds between `playerUpdate` messages.
    pub player_update_interval: u64,
    /// Seconds between `stats` messages.
    pub stats_interval: u64,
    /// Milliseconds without audio frames before a `TrackStuckEvent` is emitted.
    pub track_stuck_threshold_ms: u64,
    /// Maximum number of events kept for a detached (resumable) session.
    pub resume_backlog_size: usize,
}

impl Default for LavalinkConfig {
    fn default() -> Self {
        Self {
            player_update_interval: 5,
            stats_interval: 60,
            track_stuck_threshold_ms: 10_000,
            resume_backlog_size: 1_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AudioConfig {
    pub opus_bitrate: i32,
    pub opus_complexity: u8,
    /// Size of the decoded-frame buffer between the decoder thread and the UDP sender.
    pub frame_buffer_ms: u64,
    /// Forward Opus packets from the source untouched when volume and filters are neutral.
    pub opus_passthrough: bool,
    /// `auto`, `aead_aes256_gcm_rtpsize` or `aead_xchacha20_poly1305_rtpsize`.
    pub encryption_mode: String,
    /// Advertise DAVE (Discord E2EE) support during voice identify.
    pub dave: bool,
    pub udp_keepalive_secs: u64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            opus_bitrate: 128_000,
            opus_complexity: 10,
            frame_buffer_ms: 400,
            opus_passthrough: true,
            encryption_mode: "auto".into(),
            dave: true,
            udp_keepalive_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SourcesConfig {
    pub youtube: bool,
    pub soundcloud: bool,
    pub http: bool,
    /// Resolve any other URL through the yt-dlp generic extractors.
    pub ytdlp_generic: bool,
    pub http_timeout_secs: u64,
    pub ytdlp: YtDlpConfig,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            youtube: true,
            soundcloud: true,
            http: true,
            ytdlp_generic: true,
            http_timeout_secs: 20,
            ytdlp: YtDlpConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct YtDlpConfig {
    pub path: String,
    pub format: String,
    pub search_limit: usize,
    pub metadata_timeout_secs: u64,
    pub extra_args: Vec<String>,
}

impl Default for YtDlpConfig {
    fn default() -> Self {
        Self {
            path: "yt-dlp".into(),
            format: "bestaudio[acodec=opus]/bestaudio/best".into(),
            search_limit: 10,
            metadata_timeout_secs: 30,
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CacheConfig {
    pub enabled: bool,
    pub directory: PathBuf,
    pub database: PathBuf,
    pub max_size: ByteSize,
    /// Eviction starts when usage exceeds `max_size * high_watermark`.
    pub high_watermark: f64,
    /// Eviction stops once usage drops below `max_size * low_watermark`.
    pub low_watermark: f64,
    pub lru_interval_secs: u64,
    /// Allow evicting files that have not been backed up to OneDrive yet.
    pub evict_unsynced: bool,
    /// Start playback while the file is still being downloaded.
    pub progressive_playback: bool,
    pub max_file_size: ByteSize,
    pub file_extension: String,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: PathBuf::from("/data/cache"),
            database: PathBuf::from("/data/cache/index.db"),
            max_size: ByteSize::gib(20),
            high_watermark: 0.90,
            low_watermark: 0.80,
            lru_interval_secs: 60,
            evict_unsynced: false,
            progressive_playback: true,
            max_file_size: ByteSize::gib(1),
            file_extension: "opus".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OneDriveConfig {
    pub enabled: bool,
    pub credentials_file: PathBuf,
    pub remote_folder: String,
    pub sync_interval_secs: u64,
    pub sync_batch_size: u32,
    pub max_concurrent_uploads: usize,
    /// Upload session chunk size. Rounded down to a multiple of 320 KiB.
    pub chunk_size: ByteSize,
    /// Files up to this size use a single PUT instead of an upload session.
    pub simple_upload_max: ByteSize,
    pub snapshot_interval_secs: u64,
    pub restore_index_on_startup: bool,
    pub max_sync_attempts: u32,
    pub request_timeout_secs: u64,
    pub graph_base_url: String,
    pub authority: String,
}

impl Default for OneDriveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            credentials_file: PathBuf::from("config/onedrive.json"),
            remote_folder: "AudioCache".into(),
            sync_interval_secs: 300,
            sync_batch_size: 25,
            max_concurrent_uploads: 2,
            chunk_size: ByteSize::mib(10),
            simple_upload_max: ByteSize::mib(4),
            snapshot_interval_secs: 3600,
            restore_index_on_startup: true,
            max_sync_attempts: 12,
            request_timeout_secs: 120,
            graph_base_url: "https://graph.microsoft.com/v1.0".into(),
            authority: "https://login.microsoftonline.com".into(),
        }
    }
}

/// Microsoft Graph requires upload chunks to be multiples of 320 KiB.
pub const GRAPH_CHUNK_ALIGNMENT: u64 = 320 * 1024;
/// Graph rejects chunks above 60 MiB.
pub const GRAPH_MAX_CHUNK: u64 = 60 * 1024 * 1024;

impl OneDriveConfig {
    pub fn normalized_chunk_size(&self) -> u64 {
        let raw = self.chunk_size.0.clamp(GRAPH_CHUNK_ALIGNMENT, GRAPH_MAX_CHUNK);
        (raw / GRAPH_CHUNK_ALIGNMENT) * GRAPH_CHUNK_ALIGNMENT
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { level: "info".into() }
    }
}

impl AppConfig {
    /// Load the YAML file (defaults when missing) and apply environment overrides.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
            serde_norway::from_str::<AppConfig>(&raw)
                .with_context(|| format!("invalid configuration in {}", path.display()))?
        } else {
            eprintln!("config file {} not found, falling back to defaults", path.display());
            AppConfig::default()
        };
        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self) {
        let password = std::env::var("RUSTLINK_SERVER_PASSWORD").or_else(|_| std::env::var("LAVALINK_SERVER_PASSWORD"));
        if let Ok(password) = password {
            self.server.password = password;
        }
        if let Some(port) = std::env::var("RUSTLINK_SERVER_PORT")
            .ok()
            .or_else(|| std::env::var("SERVER_PORT").ok())
            .and_then(|p| p.parse().ok())
        {
            self.server.port = port;
        }
        if let Ok(address) = std::env::var("RUSTLINK_SERVER_ADDRESS") {
            self.server.address = address;
        }
        if let Ok(dir) = std::env::var("RUSTLINK_CACHE_DIR") {
            self.cache.directory = PathBuf::from(dir);
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let c = &self.cache;
        if !(0.0 < c.low_watermark && c.low_watermark < c.high_watermark && c.high_watermark <= 1.0) {
            bail!("cache watermarks must satisfy 0 < lowWatermark < highWatermark <= 1");
        }
        if c.max_size.0 == 0 {
            bail!("cache.maxSize must be greater than zero");
        }
        if self.server.password.is_empty() {
            bail!("server.password must not be empty");
        }
        if self.audio.frame_buffer_ms < 40 {
            bail!("audio.frameBufferMs must be at least 40");
        }
        if self.onedrive.enabled && self.onedrive.remote_folder.trim_matches('/').is_empty() {
            bail!("onedrive.remoteFolder must not be empty");
        }
        Ok(())
    }
}

/// Human friendly byte size: accepts `1048576`, `"512MB"`, `"10MiB"`, `"20 GB"`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub const fn mib(n: u64) -> Self {
        Self(n * 1024 * 1024)
    }
    pub const fn gib(n: u64) -> Self {
        Self(n * 1024 * 1024 * 1024)
    }
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = self.0 as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit < UNITS.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            write!(f, "{} B", self.0)
        } else {
            write!(f, "{value:.2} {}", UNITS[unit])
        }
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
        let (number, unit) = s.split_at(split);
        let number: f64 = number.parse().map_err(|_| format!("invalid size number in {s:?}"))?;
        let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "k" | "kb" => 1_000,
            "kib" => 1 << 10,
            "m" | "mb" => 1_000_000,
            "mib" => 1 << 20,
            "g" | "gb" => 1_000_000_000,
            "gib" => 1 << 30,
            "t" | "tb" => 1_000_000_000_000,
            "tib" => 1 << 40,
            other => return Err(format!("unknown size unit {other:?}")),
        };
        Ok(ByteSize((number * multiplier as f64).round() as u64))
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = ByteSize;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte count or a size string such as \"20GB\"")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ByteSize, E> {
                u64::try_from(v)
                    .map(ByteSize)
                    .map_err(|_| E::custom("size must be positive"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ByteSize, E> {
                v.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_byte_sizes() {
        assert_eq!("20GB".parse::<ByteSize>().unwrap().0, 20_000_000_000);
        assert_eq!("10MiB".parse::<ByteSize>().unwrap().0, 10 * 1024 * 1024);
        assert_eq!("1.5 GiB".parse::<ByteSize>().unwrap().0, 1_610_612_736);
        assert_eq!("4096".parse::<ByteSize>().unwrap().0, 4096);
        assert!("12 parsecs".parse::<ByteSize>().is_err());
    }

    #[test]
    fn chunk_size_is_aligned_to_320_kib() {
        let cfg = OneDriveConfig {
            chunk_size: ByteSize(10 * 1024 * 1024 + 5),
            ..Default::default()
        };
        assert_eq!(cfg.normalized_chunk_size() % GRAPH_CHUNK_ALIGNMENT, 0);
        assert_eq!(cfg.normalized_chunk_size(), 32 * GRAPH_CHUNK_ALIGNMENT);
        let tiny = OneDriveConfig {
            chunk_size: ByteSize(1),
            ..Default::default()
        };
        assert_eq!(tiny.normalized_chunk_size(), GRAPH_CHUNK_ALIGNMENT);
    }

    #[test]
    fn sample_config_parses() {
        let raw = include_str!("../config/application.yml");
        let cfg: AppConfig = serde_norway::from_str(raw).expect("sample config must parse");
        cfg.validate().unwrap();
        assert_eq!(cfg.server.port, 2333);
        assert_eq!(cfg.cache.max_size.0, 20 * 1024 * 1024 * 1024);
    }
}
