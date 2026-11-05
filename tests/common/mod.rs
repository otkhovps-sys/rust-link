#![allow(dead_code)]

pub mod media_server;
pub mod voice_mock;

use std::{net::SocketAddr, path::Path, sync::Arc};

use rust_link::{
    api::{self, models::TrackInfo, track_codec},
    cache::{cache_key_for, index::NewCacheEntry, relative_path_for},
    config::AppConfig,
    node::{self, NodeContext},
};
use tokio_util::sync::CancellationToken;

pub const PASSWORD: &str = "test-password";

pub struct TestNode {
    pub ctx: Arc<NodeContext>,
    pub addr: SocketAddr,
    pub http: reqwest::Client,
    pub shutdown: CancellationToken,
    pub dir: tempfile::TempDir,
}

impl TestNode {
    pub async fn start(customize: impl FnOnce(&mut AppConfig)) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.server.password = PASSWORD.into();
        config.cache.directory = dir.path().join("cache");
        config.cache.database = dir.path().join("cache/index.db");
        config.lavalink.player_update_interval = 1;
        config.sources.ytdlp.auto_update = false;
        customize(&mut config);
        config.validate().unwrap();

        let shutdown = CancellationToken::new();
        let ctx = node::build(config, shutdown.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = api::router(Arc::clone(&ctx));
        let token = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { token.cancelled().await })
                .await
                .unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        Self {
            ctx,
            addr,
            http,
            shutdown,
            dir,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(self.url(path)).header("Authorization", PASSWORD)
    }

    pub fn patch(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.patch(self.url(path)).header("Authorization", PASSWORD)
    }

    pub fn delete(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.delete(self.url(path)).header("Authorization", PASSWORD)
    }

    /// Put a WAV file into the cache as if it had been downloaded, return its encoded track.
    pub async fn seed_cached_wav(&self, title: &str, seconds: f32) -> (TrackInfo, String) {
        let info = TrackInfo {
            identifier: format!("https://example.invalid/{title}.wav"),
            is_seekable: true,
            author: "rust-link tests".into(),
            length: (seconds * 1000.0) as u64,
            is_stream: false,
            position: 0,
            title: title.into(),
            uri: Some(format!("https://example.invalid/{title}.wav")),
            artwork_url: None,
            isrc: None,
            source_name: "http".into(),
        };
        let key = cache_key_for(&info);
        let relative = relative_path_for(&key, "opus");
        let path = self.ctx.cache.absolute_path(&relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_wav(&path, 48_000, 2, seconds);
        let size = std::fs::metadata(&path).unwrap().len();
        self.ctx
            .cache
            .index()
            .upsert_downloaded(&NewCacheEntry {
                video_id: key,
                info: info.clone(),
                file_path: relative,
                file_size: size as i64,
                duration_ms: info.length as i64,
                sha256: rust_link::util::sha256_hex(std::fs::read(&path).unwrap()),
                container: Some("wav".into()),
            })
            .await
            .unwrap();
        let encoded = track_codec::encode_track(&info);
        (info, encoded)
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

pub fn write_wav(path: &Path, rate: u32, channels: u16, seconds: f32) {
    use std::io::Write;
    let frames = (rate as f32 * seconds) as u32;
    let data_len = frames * channels as u32 * 2;
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * channels as u32 * 2).to_le_bytes()).unwrap();
    f.write_all(&(channels * 2).to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    let mut buf = Vec::with_capacity(data_len as usize);
    for i in 0..frames {
        let v = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 12_000.0) as i16;
        for _ in 0..channels {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    f.write_all(&buf).unwrap();
}
