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
    /// Plugin processes, plus `plugins.<name>` settings handed to each plugin (the same
    /// layout as Lavalink's `plugins:` block).
    pub plugins: PluginsConfig,
}

/// External plugin processes (any language) speaking JSON-RPC over stdin/stdout.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PluginsConfig {
    pub enabled: bool,
    /// Every sub-directory holding a `plugin.yml` is started.
    pub directory: PathBuf,
    /// Plugins defined here instead of (or in addition to) a `plugin.yml`.
    pub entries: Vec<PluginEntry>,
    /// Limit for one call to a plugin (track loading, resolving, routes).
    pub request_timeout_secs: u64,
    /// Limit for a plugin to answer `initialize` after it was started.
    pub startup_timeout_secs: u64,
    /// `plugins.<name>`: settings handed to the plugin of that name.
    #[serde(flatten)]
    pub settings: std::collections::BTreeMap<String, serde_json::Value>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: PathBuf::from("plugins"),
            entries: Vec::new(),
            request_timeout_secs: 10,
            startup_timeout_secs: 15,
            settings: Default::default(),
        }
    }
}

/// One plugin: a command line, run in `workingDir`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PluginEntry {
    pub name: String,
    /// Program and arguments, as a list or as one string split on whitespace.
    #[serde(deserialize_with = "command_line")]
    pub command: Vec<String>,
    pub working_dir: Option<PathBuf>,
    pub env: std::collections::BTreeMap<String, String>,
    pub enabled: bool,
    /// Settings for the plugin; replaces `plugins.<name>` when present.
    pub config: Option<serde_json::Value>,
}

impl Default for PluginEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: Vec::new(),
            working_dir: None,
            env: Default::default(),
            enabled: true,
            config: None,
        }
    }
}

fn command_line<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Line(String),
        Parts(Vec<String>),
    }
    Ok(match Raw::deserialize(deserializer)? {
        Raw::Line(line) => line.split_whitespace().map(str::to_string).collect(),
        Raw::Parts(parts) => parts,
    })
}

/// Keys of the `plugins:` block that configure the host rather than a plugin.
pub const PLUGIN_HOST_KEYS: [&str; 5] = [
    "enabled",
    "directory",
    "entries",
    "requestTimeoutSecs",
    "startupTimeoutSecs",
];

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
    /// Filters clients may use (Lavalink: `lavalink.server.filters`).
    pub filters: FilterToggles,
}

/// Per-filter switches. A disabled filter in a player update is rejected with 400.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FilterToggles {
    pub volume: bool,
    pub equalizer: bool,
    pub karaoke: bool,
    pub timescale: bool,
    pub tremolo: bool,
    pub vibrato: bool,
    pub distortion: bool,
    pub rotation: bool,
    pub channel_mix: bool,
    pub low_pass: bool,
}

impl Default for FilterToggles {
    fn default() -> Self {
        Self {
            volume: true,
            equalizer: true,
            karaoke: true,
            timescale: true,
            tremolo: true,
            vibrato: true,
            distortion: true,
            rotation: true,
            channel_mix: true,
            low_pass: true,
        }
    }
}

impl FilterToggles {
    /// `name` as used by Lavalink (`channelMix`, `lowPass`, ...).
    pub fn is_enabled(&self, name: &str) -> bool {
        match name {
            "volume" => self.volume,
            "equalizer" => self.equalizer,
            "karaoke" => self.karaoke,
            "timescale" => self.timescale,
            "tremolo" => self.tremolo,
            "vibrato" => self.vibrato,
            "distortion" => self.distortion,
            "rotation" => self.rotation,
            "channelMix" => self.channel_mix,
            "lowPass" => self.low_pass,
            _ => false,
        }
    }

    /// Enabled filters, in the order Lavalink lists them in `/v4/info`.
    pub fn enabled_names(&self) -> Vec<String> {
        crate::audio::filters::FILTER_NAMES
            .iter()
            .filter(|name| self.is_enabled(name))
            .map(|name| name.to_string())
            .collect()
    }
}

impl Default for LavalinkConfig {
    fn default() -> Self {
        Self {
            player_update_interval: 5,
            stats_interval: 60,
            track_stuck_threshold_ms: 10_000,
            resume_backlog_size: 1_000,
            filters: FilterToggles::default(),
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
    /// Players on the same live stream with the same filters and volume share one decode.
    pub share_live_streams: bool,
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
            share_live_streams: true,
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
    pub spotify: SpotifyConfig,
    pub apple_music: AppleMusicConfig,
    pub deezer: DeezerConfig,
    /// Searches tried in order to find a playable YouTube copy of a Spotify, Apple Music or
    /// Deezer track (LavaSrc `providers`). `%ISRC%` and `%QUERY%` (title + artist) are
    /// substituted; templates with `%ISRC%` are skipped for tracks without one.
    pub mirror_providers: Vec<String>,
    /// Reuse `/v4/loadtracks` results of searches and music-service links for this long
    /// (0 = off). Skips yt-dlp and the remote APIs for repeated requests.
    pub load_cache_ttl_secs: u64,
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
            spotify: SpotifyConfig::default(),
            apple_music: AppleMusicConfig::default(),
            deezer: DeezerConfig::default(),
            mirror_providers: vec!["ytsearch:\"%ISRC%\"".into(), "ytsearch:%QUERY%".into()],
            load_cache_ttl_secs: 6 * 3600,
        }
    }
}

/// Spotify Web API (client credentials from https://developer.spotify.com/dashboard).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SpotifyConfig {
    pub enabled: bool,
    pub client_id: String,
    pub client_secret: String,
    pub country_code: String,
    /// Pages of 100 tracks loaded from a playlist.
    pub playlist_load_limit: usize,
    /// Pages of 50 tracks loaded from an album.
    pub album_load_limit: usize,
    pub search_limit: usize,
    pub api_url: String,
    pub accounts_url: String,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            client_id: String::new(),
            client_secret: String::new(),
            country_code: "US".into(),
            playlist_load_limit: 6,
            album_load_limit: 6,
            search_limit: 10,
            api_url: "https://api.spotify.com/v1".into(),
            accounts_url: "https://accounts.spotify.com".into(),
        }
    }
}

/// Apple Music catalog API, with a MusicKit developer token from an Apple Developer account.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppleMusicConfig {
    pub enabled: bool,
    pub country_code: String,
    #[serde(alias = "mediaAPIToken")]
    pub media_api_token: String,
    /// Pages of 100 tracks loaded from a playlist.
    pub playlist_load_limit: usize,
    /// Pages of 100 tracks loaded from an album.
    pub album_load_limit: usize,
    pub search_limit: usize,
    pub api_url: String,
}

impl Default for AppleMusicConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            country_code: "us".into(),
            media_api_token: String::new(),
            playlist_load_limit: 6,
            album_load_limit: 6,
            search_limit: 10,
            api_url: "https://api.music.apple.com".into(),
        }
    }
}

/// Deezer public API (no account needed for metadata).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DeezerConfig {
    pub enabled: bool,
    /// Pages of 100 tracks loaded from a playlist.
    pub playlist_load_limit: usize,
    pub search_limit: usize,
    pub api_url: String,
}

impl Default for DeezerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            playlist_load_limit: 6,
            search_limit: 10,
            api_url: "https://api.deezer.com".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct YtDlpConfig {
    /// Executable, or `managed` to let rust-link download and update its own copy.
    pub path: String,
    pub format: String,
    pub search_limit: usize,
    pub metadata_timeout_secs: u64,
    pub extra_args: Vec<String>,
    /// Update yt-dlp on a schedule (`yt-dlp -U`, or a fresh managed download).
    pub auto_update: bool,
    pub update_interval_hours: u64,
    /// Where the managed yt-dlp lives (`path: managed`).
    pub managed_dir: PathBuf,
    /// Release download base used for the managed yt-dlp.
    pub update_url: String,
}

impl Default for YtDlpConfig {
    fn default() -> Self {
        Self {
            path: "yt-dlp".into(),
            format: "bestaudio[acodec=opus]/bestaudio/best".into(),
            search_limit: 10,
            metadata_timeout_secs: 30,
            extra_args: Vec::new(),
            auto_update: true,
            update_interval_hours: 24,
            managed_dir: PathBuf::from("bin"),
            update_url: "https://github.com/yt-dlp/yt-dlp/releases/latest/download".into(),
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
    /// Tracks longer than this are streamed from the origin instead of cached (0 = no limit).
    pub max_track_duration_secs: u64,
    /// Tracks cached ahead after a playlist load and after each playlist track starts (0 = off).
    pub prefetch_tracks: usize,
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
            max_track_duration_secs: 7_200,
            prefetch_tracks: 3,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LoggingConfig {
    /// `info`, or Lavalink's map form `{ root: INFO, lavalink: INFO }`.
    pub level: LogLevel,
    /// Lavalink compatible `logging.file.path`: also write daily rotated log files there.
    pub file: LogFileConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LogFileConfig {
    pub path: Option<PathBuf>,
}

/// Log level given either as a string or as Lavalink's `{root, lavalink}` map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLevel(pub String);

impl Default for LogLevel {
    fn default() -> Self {
        Self("info".into())
    }
}

impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Plain(String),
            Map {
                root: Option<String>,
                lavalink: Option<String>,
            },
        }
        let level = match Raw::deserialize(deserializer)? {
            Raw::Plain(level) => level,
            Raw::Map { root, lavalink } => lavalink.or(root).unwrap_or_else(|| "info".into()),
        };
        Ok(LogLevel(level.trim().to_ascii_lowercase()))
    }
}

/// Default file names searched in the working directory, like Lavalink.
pub const CONFIG_CANDIDATES: [&str; 3] = ["application.yml", "application.yaml", "config/application.yml"];

fn yaml_path<'a>(root: &'a serde_norway::Value, path: &[&str]) -> Option<&'a serde_norway::Value> {
    path.iter().try_fold(root, |node, key| node.get(*key))
}

impl AppConfig {
    /// Config file to use: the explicit path, else `application.yml` in the working
    /// directory (Lavalink behaviour), `config/application.yml`, then next to the binary.
    pub fn locate(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            return Some(path.to_path_buf());
        }
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        CONFIG_CANDIDATES
            .iter()
            .map(PathBuf::from)
            .chain(exe_dir.map(|d| d.join("application.yml")))
            .find(|p| p.is_file())
    }

    /// Load the YAML file (defaults when missing) and apply environment overrides.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut config = match path {
            Some(path) if path.exists() => {
                let raw = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
                Self::from_yaml(&raw).with_context(|| format!("invalid configuration in {}", path.display()))?
            }
            Some(path) => bail!("config file {} not found", path.display()),
            None => {
                eprintln!("no application.yml found, using built-in defaults");
                AppConfig::default()
            }
        };
        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    /// Parse rust-link YAML. A Lavalink v4 `application.yml` is also accepted: its
    /// `lavalink.server.*` keys are used wherever the rust-link key is absent.
    pub fn from_yaml(raw: &str) -> anyhow::Result<Self> {
        let value: serde_norway::Value = serde_norway::from_str(raw)?;
        if value.is_null() {
            return Ok(AppConfig::default());
        }
        let mut config: AppConfig = serde_norway::from_value(value.clone())?;
        config.apply_lavalink_compat(&value);
        Ok(config)
    }

    fn apply_lavalink_compat(&mut self, root: &serde_norway::Value) {
        let has = |path: &[&str]| yaml_path(root, path).is_some();
        let server = |key: &str| yaml_path(root, &["lavalink", "server", key]);
        let as_u64 = |v: &serde_norway::Value| v.as_u64();

        if !has(&["server", "password"]) {
            if let Some(password) = server("password").and_then(|v| v.as_str()) {
                self.server.password = password.to_string();
            }
        }
        if !has(&["lavalink", "playerUpdateInterval"]) {
            if let Some(v) = server("playerUpdateInterval").and_then(as_u64) {
                self.lavalink.player_update_interval = v;
            }
        }
        if !has(&["lavalink", "trackStuckThresholdMs"]) {
            if let Some(v) = server("trackStuckThresholdMs").and_then(as_u64) {
                self.lavalink.track_stuck_threshold_ms = v;
            }
        }
        if !has(&["lavalink", "filters"]) {
            if let Some(toggles) = server("filters").and_then(|v| serde_norway::from_value(v.clone()).ok()) {
                self.lavalink.filters = toggles;
            }
        }
        if !has(&["audio", "opusComplexity"]) {
            if let Some(v) = server("opusEncodingQuality").and_then(as_u64) {
                self.audio.opus_complexity = v.min(10) as u8;
            }
        }
        if !has(&["audio", "frameBufferMs"]) {
            if let Some(v) = server("bufferDurationMs").and_then(as_u64) {
                self.audio.frame_buffer_ms = v.max(40);
            }
        }
        let source = |name: &str| yaml_path(root, &["lavalink", "server", "sources", name]).and_then(|v| v.as_bool());
        if !has(&["sources", "youtube"]) {
            // Lavalink v4 disables the built-in YouTube source in favour of youtube-plugin.
            let plugin = yaml_path(root, &["plugins", "youtube", "enabled"]).and_then(|v| v.as_bool());
            if let Some(enabled) = match (source("youtube"), plugin) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(false) || b.unwrap_or(false)),
            } {
                self.sources.youtube = enabled;
            }
        }
        if !has(&["sources", "soundcloud"]) {
            if let Some(enabled) = source("soundcloud") {
                self.sources.soundcloud = enabled;
            }
        }
        if !has(&["sources", "http"]) {
            if let Some(enabled) = source("http") {
                self.sources.http = enabled;
            }
        }
        self.apply_lavasrc_compat(root);
    }

    /// LavaSrc settings (`plugins.lavasrc`) of a Lavalink application.yml.
    fn apply_lavasrc_compat(&mut self, root: &serde_norway::Value) {
        let has = |path: &[&str]| yaml_path(root, path).is_some();
        let lavasrc = |path: &[&str]| {
            let mut full = vec!["plugins", "lavasrc"];
            full.extend_from_slice(path);
            yaml_path(root, &full).cloned()
        };
        let text = |path: &[&str]| lavasrc(path).and_then(|v| v.as_str().map(str::to_string));
        let number = |path: &[&str]| lavasrc(path).and_then(|v| v.as_u64()).map(|v| v as usize);
        let flag = |path: &[&str]| lavasrc(path).and_then(|v| v.as_bool());

        if !has(&["sources", "mirrorProviders"]) {
            if let Some(providers) =
                lavasrc(&["providers"]).and_then(|v| serde_norway::from_value::<Vec<String>>(v).ok())
            {
                self.sources.mirror_providers = providers;
            }
        }
        if !has(&["sources", "spotify"]) {
            let spotify = &mut self.sources.spotify;
            if let Some(v) = flag(&["sources", "spotify"]) {
                spotify.enabled = v;
            }
            if let Some(v) = text(&["spotify", "clientId"]) {
                spotify.client_id = v;
            }
            if let Some(v) = text(&["spotify", "clientSecret"]) {
                spotify.client_secret = v;
            }
            if let Some(v) = text(&["spotify", "countryCode"]) {
                spotify.country_code = v;
            }
            if let Some(v) = number(&["spotify", "playlistLoadLimit"]) {
                spotify.playlist_load_limit = v;
            }
            if let Some(v) = number(&["spotify", "albumLoadLimit"]) {
                spotify.album_load_limit = v;
            }
        }
        if !has(&["sources", "appleMusic"]) {
            let apple = &mut self.sources.apple_music;
            if let Some(v) = flag(&["sources", "applemusic"]) {
                apple.enabled = v;
            }
            if let Some(v) = text(&["applemusic", "countryCode"]) {
                apple.country_code = v;
            }
            if let Some(v) = text(&["applemusic", "mediaAPIToken"]).filter(|t| !t.contains("your apple music")) {
                apple.media_api_token = v;
            }
            if let Some(v) = number(&["applemusic", "playlistLoadLimit"]) {
                apple.playlist_load_limit = v;
            }
            if let Some(v) = number(&["applemusic", "albumLoadLimit"]) {
                apple.album_load_limit = v;
            }
        }
        if !has(&["sources", "deezer"]) {
            if let Some(v) = flag(&["sources", "deezer"]) {
                self.sources.deezer.enabled = v;
            }
        }
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
        if let Ok(address) = std::env::var("RUSTLINK_SERVER_ADDRESS").or_else(|_| std::env::var("SERVER_ADDRESS")) {
            self.server.address = address;
        }
        if let Ok(dir) = std::env::var("RUSTLINK_CACHE_DIR") {
            let dir = PathBuf::from(dir);
            if self.cache.database == self.cache.directory.join("index.db") {
                self.cache.database = dir.join("index.db");
            }
            self.cache.directory = dir;
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
        let cfg = AppConfig::from_yaml(raw).expect("sample config must parse");
        cfg.validate().unwrap();
        assert_eq!(cfg.server.port, 2333);
        assert_eq!(cfg.cache.max_size.0, 20 * 1024 * 1024 * 1024);
    }

    /// Default `application.yml` shipped with Lavalink v4 (trimmed comments).
    const LAVALINK_APPLICATION_YML: &str = r#"
server:
  port: 2444
  address: 0.0.0.0
  http2:
    enabled: false
plugins:
  youtube:
    enabled: true
    allowSearch: true
    clients: ["MUSIC", "ANDROID_VR", "WEB", "WEBEMBEDDED"]
  lavasrc:
    providers:
      - "ytsearch:\"%ISRC%\""
      - "ytsearch:%QUERY%"
    sources:
      spotify: true
      applemusic: false
      deezer: true
    spotify:
      clientId: "spotify-id"
      clientSecret: "spotify-secret"
      countryCode: "VN"
      playlistLoadLimit: 3
    applemusic:
      countryCode: "US"
      mediaAPIToken: "your apple music api token"
lavalink:
  plugins:
    - dependency: "dev.lavalink.youtube:youtube-plugin:1.13.0"
      snapshot: false
  server:
    password: "secret-from-lavalink"
    sources:
      youtube: false
      bandcamp: true
      soundcloud: false
      twitch: true
      vimeo: true
      nico: true
      http: true
      local: false
    filters:
      volume: true
      equalizer: true
      karaoke: false
      channelMix: true
    bufferDurationMs: 400
    frameBufferDurationMs: 5000
    opusEncodingQuality: 8
    resamplingQuality: LOW
    trackStuckThresholdMs: 8000
    useSeekGhosting: true
    youtubePlaylistLoadLimit: 6
    playerUpdateInterval: 3
    youtubeSearchEnabled: true
    soundcloudSearchEnabled: true
    gc-warnings: true
metrics:
  prometheus:
    enabled: false
    endpoint: /metrics
sentry:
  dsn: ""
  environment: ""
logging:
  file:
    path: ./logs/
  level:
    root: INFO
    lavalink: DEBUG
  request:
    enabled: true
    includeClientInfo: true
  logback:
    rollingpolicy:
      max-file-size: 1GB
      max-history: 30
"#;

    #[test]
    fn lavalink_application_yml_is_accepted() {
        let cfg = AppConfig::from_yaml(LAVALINK_APPLICATION_YML).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.server.port, 2444);
        assert_eq!(cfg.server.password, "secret-from-lavalink");
        assert_eq!(cfg.lavalink.player_update_interval, 3);
        assert_eq!(cfg.lavalink.track_stuck_threshold_ms, 8000);
        assert_eq!(cfg.audio.opus_complexity, 8);
        assert_eq!(cfg.audio.frame_buffer_ms, 400);
        assert!(!cfg.lavalink.filters.karaoke);
        assert_eq!(cfg.sources.spotify.client_id, "spotify-id");
        assert_eq!(cfg.sources.spotify.country_code, "VN");
        assert_eq!(cfg.sources.spotify.playlist_load_limit, 3);
        assert_eq!(cfg.sources.spotify.album_load_limit, 6);
        assert!(!cfg.sources.apple_music.enabled);
        assert!(
            cfg.sources.apple_music.media_api_token.is_empty(),
            "placeholder token ignored"
        );
        assert_eq!(
            cfg.sources.mirror_providers,
            vec!["ytsearch:\"%ISRC%\"", "ytsearch:%QUERY%"]
        );
        assert!(
            cfg.lavalink.filters.timescale,
            "filters missing from the list stay enabled"
        );
        assert!(!cfg.lavalink.filters.enabled_names().contains(&"karaoke".to_string()));
        assert!(cfg.sources.youtube, "youtube-plugin enabled means YouTube stays on");
        assert!(!cfg.sources.soundcloud);
        assert!(cfg.sources.http);
        assert_eq!(cfg.logging.level, LogLevel("debug".into()));
        assert_eq!(cfg.logging.file.path.as_deref(), Some(Path::new("./logs/")));
    }

    #[test]
    fn native_keys_win_over_lavalink_keys() {
        let raw = "server:\n  password: native\nlavalink:\n  server:\n    password: legacy\n";
        assert_eq!(AppConfig::from_yaml(raw).unwrap().server.password, "native");
        assert_eq!(AppConfig::from_yaml("").unwrap().server.port, 2333);
        assert_eq!(
            AppConfig::from_yaml("logging:\n  level: WARN\n")
                .unwrap()
                .logging
                .level
                .0,
            "warn"
        );
    }
}
