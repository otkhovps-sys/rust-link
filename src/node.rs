//! Node wiring: shared context, background workers and periodic Lavalink messages.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    api::models::{FrameStats, OutgoingMessage, Stats},
    audio::{playback::PlaybackSettings, voice_gateway::VoiceSettings, FrameCounters},
    cache::{lru, CacheIndex, CacheManager},
    config::AppConfig,
    onedrive::{auth::TokenManager, sync_worker, OneDriveClient, SyncWorker},
    player::SessionManager,
    source::SourceManager,
    stats::SystemSampler,
};

pub struct NodeContext {
    pub config: Arc<AppConfig>,
    pub cache: Arc<CacheManager>,
    pub sources: Arc<SourceManager>,
    pub sessions: SessionManager,
    pub frames: Arc<FrameCounters>,
    pub system: SystemSampler,
    pub started_at: Instant,
    pub shutdown: CancellationToken,
}

impl NodeContext {
    pub fn playback_settings(&self) -> PlaybackSettings {
        let audio = &self.config.audio;
        PlaybackSettings {
            opus_bitrate: audio.opus_bitrate,
            opus_complexity: audio.opus_complexity,
            opus_passthrough: audio.opus_passthrough,
            buffer_frames: (audio.frame_buffer_ms / crate::audio::encoder::FRAME_MS).max(2) as usize,
            stuck_threshold: Duration::from_millis(self.config.lavalink.track_stuck_threshold_ms.max(100)),
        }
    }

    pub fn voice_settings(&self) -> VoiceSettings {
        let audio = &self.config.audio;
        VoiceSettings {
            encryption_mode: audio.encryption_mode.clone(),
            dave: audio.dave,
            udp_keepalive: Duration::from_secs(audio.udp_keepalive_secs.max(1)),
        }
    }

    pub fn uptime_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Lavalink stats; frame stats are averaged per playing player and per minute.
    pub fn stats(&self, frames: Option<(u64, u64, Duration)>) -> Stats {
        let (players, playing) = self.sessions.player_counts();
        let frame_stats = frames.and_then(|(sent, nulled, window)| {
            if playing == 0 || window.is_zero() {
                return None;
            }
            let per_minute = 60_000.0 / window.as_millis() as f64 / playing as f64;
            let sent = (sent as f64 * per_minute).round() as i64;
            let nulled = (nulled as f64 * per_minute).round() as i64;
            Some(FrameStats {
                sent,
                nulled,
                deficit: 3_000 - sent - nulled,
            })
        });
        Stats {
            players,
            playing_players: playing,
            uptime: self.uptime_ms(),
            memory: self.system.memory(),
            cpu: self.system.cpu(),
            frame_stats,
        }
    }
}

/// Build every component from the configuration and start background workers.
pub async fn build(config: AppConfig, shutdown: CancellationToken) -> anyhow::Result<Arc<NodeContext>> {
    let config = Arc::new(config);
    let cache_cfg = &config.cache;
    tokio::fs::create_dir_all(&cache_cfg.directory)
        .await
        .with_context(|| format!("cannot create cache directory {}", cache_cfg.directory.display()))?;

    let onedrive = if config.onedrive.enabled {
        match init_onedrive(&config).await {
            Ok(client) => Some(client),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "OneDrive backup disabled");
                None
            }
        }
    } else {
        None
    };

    if let (Some(client), true) = (&onedrive, config.onedrive.restore_index_on_startup) {
        if let Err(e) = sync_worker::restore_index_if_missing(client, &cache_cfg.database).await {
            warn!(error = %format!("{e:#}"), "index restore from OneDrive failed");
        }
    }

    let index = CacheIndex::open(&cache_cfg.database)
        .await
        .with_context(|| format!("cannot open cache index {}", cache_cfg.database.display()))?;
    let sources = Arc::new(SourceManager::new(&config.sources, Some(index.clone()))?);
    let cache = CacheManager::new(cache_cfg.clone(), index, Arc::clone(&sources), onedrive.clone());
    cache.reconcile().await?;

    let ctx = Arc::new(NodeContext {
        config: Arc::clone(&config),
        cache: Arc::clone(&cache),
        sources,
        sessions: SessionManager::default(),
        frames: Arc::new(FrameCounters::default()),
        system: SystemSampler::default(),
        started_at: Instant::now(),
        shutdown: shutdown.clone(),
    });

    tokio::spawn(lru::run_lru_worker(Arc::clone(&cache), shutdown.clone()));
    if let Some(client) = onedrive {
        let worker = SyncWorker::new(
            Arc::clone(&cache),
            client,
            config.onedrive.clone(),
            cache_cfg.database.clone(),
        );
        tokio::spawn(worker.run(shutdown.clone()));
    }
    tokio::spawn(player_update_task(Arc::clone(&ctx)));
    tokio::spawn(stats_task(Arc::clone(&ctx)));
    Ok(ctx)
}

async fn init_onedrive(config: &AppConfig) -> anyhow::Result<Arc<OneDriveClient>> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let tokens = TokenManager::load(&config.onedrive.credentials_file, &config.onedrive.authority, http).await?;
    let client = Arc::new(OneDriveClient::new(&config.onedrive, tokens)?);
    info!(
        folder = %config.onedrive.remote_folder,
        chunk = client.chunk_size(),
        "OneDrive backup enabled"
    );
    Ok(client)
}

async fn player_update_task(ctx: Arc<NodeContext>) {
    let period = Duration::from_secs(ctx.config.lavalink.player_update_interval.max(1));
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ctx.shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        for session in ctx.sessions.all() {
            if !session.is_attached() {
                continue;
            }
            for player in session.players() {
                session.send(&OutgoingMessage::PlayerUpdate {
                    guild_id: player.guild_id.to_string(),
                    state: player.state(),
                });
            }
        }
    }
}

async fn stats_task(ctx: Arc<NodeContext>) {
    let period = Duration::from_secs(ctx.config.lavalink.stats_interval.max(5));
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await;
    let mut window_start = Instant::now();
    loop {
        tokio::select! {
            _ = ctx.shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        let (sent, nulled) = ctx.frames.take();
        let window = window_start.elapsed();
        window_start = Instant::now();
        let stats = ctx.stats(Some((sent, nulled, window)));
        let message = OutgoingMessage::Stats(stats);
        for session in ctx.sessions.all() {
            if session.is_attached() {
                session.send(&message);
            }
        }
    }
}
