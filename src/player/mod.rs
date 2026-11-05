//! Lavalink players (one per guild per session) and sessions.

pub mod session;

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Weak,
};

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info};

pub use session::{Session, SessionManager};

use crate::{
    api::{
        models::{
            self, Event, Exception, OutgoingMessage, PlayerState, Severity, Track, TrackEndReason, UpdatePlayer,
            VoiceState,
        },
        track_codec, ApiError,
    },
    audio::{
        filters::Filters,
        playback::{self, PlaybackControls, PlaybackOutcome},
        voice_gateway::{VoiceConnectInfo, VoiceConnection, VoiceEvent, VoiceHandle},
    },
    node::NodeContext,
    util::now_millis,
};

struct ActivePlayback {
    id: u64,
    controls: Arc<PlaybackControls>,
}

struct PlayerInner {
    track: Option<Track>,
    volume: u16,
    paused: bool,
    filters: Arc<Filters>,
    end_time: Option<u64>,
    voice: VoiceState,
    active: Option<ActivePlayback>,
}

impl PlayerInner {
    fn gain(&self) -> f32 {
        self.volume as f32 / 100.0
    }
}

enum TrackAction {
    Keep,
    Stop,
    Play(Box<Track>),
}

pub struct Player {
    pub guild_id: u64,
    session: Weak<Session>,
    ctx: Arc<NodeContext>,
    voice: Arc<VoiceHandle>,
    voice_events: mpsc::UnboundedSender<VoiceEvent>,
    inner: Mutex<PlayerInner>,
    playback_seq: AtomicU64,
    playing: AtomicBool,
    position: Arc<AtomicU64>,
    current_controls: std::sync::Mutex<Option<Arc<PlaybackControls>>>,
}

impl Player {
    pub fn new(guild_id: u64, session: &Arc<Session>, ctx: &Arc<NodeContext>) -> Arc<Self> {
        let (voice_events, mut events_rx) = mpsc::unbounded_channel::<VoiceEvent>();
        let player = Arc::new(Self {
            guild_id,
            session: Arc::downgrade(session),
            ctx: Arc::clone(ctx),
            voice: Arc::new(VoiceHandle::default()),
            voice_events,
            inner: Mutex::new(PlayerInner {
                track: None,
                volume: 100,
                paused: false,
                filters: Arc::default(),
                end_time: None,
                voice: VoiceState::default(),
                active: None,
            }),
            playback_seq: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            position: Arc::new(AtomicU64::new(0)),
            current_controls: std::sync::Mutex::new(None),
        });
        let weak = Arc::downgrade(&player);
        tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                let Some(player) = weak.upgrade() else { break };
                match event {
                    VoiceEvent::Closed {
                        code,
                        reason,
                        by_remote,
                    } => player.emit(Event::WebSocketClosedEvent {
                        guild_id: player.guild_id.to_string(),
                        code,
                        reason,
                        by_remote,
                    }),
                }
            }
        });
        player
    }

    fn emit(&self, event: Event) {
        if let Some(session) = self.session.upgrade() {
            session.send(&OutgoingMessage::Event(event));
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    fn position(&self) -> u64 {
        match &*self.current_controls.lock().unwrap() {
            Some(controls) => controls.position(),
            None => self.position.load(Ordering::Relaxed),
        }
    }

    pub fn state(&self) -> PlayerState {
        PlayerState {
            time: now_millis() as u64,
            position: self.position(),
            connected: self.voice.is_connected(),
            ping: self.voice.ping(),
        }
    }

    fn model(&self, inner: &PlayerInner) -> models::Player {
        let track = inner.track.clone().map(|mut track| {
            track.info.position = self.position();
            track
        });
        models::Player {
            guild_id: self.guild_id.to_string(),
            track,
            volume: inner.volume,
            paused: inner.paused,
            state: self.state(),
            voice: inner.voice.clone(),
            filters: (*inner.filters).clone(),
        }
    }

    pub async fn to_model(&self) -> models::Player {
        let inner = self.inner.lock().await;
        self.model(&inner)
    }

    async fn resolve_track(&self, req: &UpdatePlayer) -> Result<TrackAction, ApiError> {
        let (encoded, identifier, user_data) = match &req.track {
            Some(track) => (track.encoded.clone(), track.identifier.clone(), track.user_data.clone()),
            None => (req.encoded_track.clone(), req.identifier.clone(), None),
        };
        if encoded.is_some() && identifier.is_some() {
            return Err(ApiError::bad_request("Cannot specify both encoded and identifier"));
        }
        let mut track = match (encoded, identifier) {
            (Some(None), _) => return Ok(TrackAction::Stop),
            (Some(Some(encoded)), _) => {
                let info = track_codec::decode_track(&encoded)
                    .map_err(|e| ApiError::bad_request(format!("Invalid encoded track: {e}")))?;
                Track {
                    encoded,
                    info,
                    plugin_info: Default::default(),
                    user_data: Default::default(),
                }
            }
            (None, Some(identifier)) => self
                .ctx
                .sources
                .load_first_track(&identifier)
                .await
                .map_err(|e| ApiError::bad_request(format!("Failed to load track: {e}")))?,
            (None, None) => return Ok(TrackAction::Keep),
        };
        if let Some(user_data) = user_data {
            track.user_data = user_data;
        }
        Ok(TrackAction::Play(Box::new(track)))
    }

    /// `PATCH /v4/sessions/{sessionId}/players/{guildId}`.
    pub async fn update(self: &Arc<Self>, req: UpdatePlayer, no_replace: bool) -> Result<models::Player, ApiError> {
        if let Some(volume) = req.volume {
            if !(0..=1000).contains(&volume) {
                return Err(ApiError::bad_request("Volume must be between 0 and 1000"));
            }
        }
        let filters = match &req.filters {
            Some(filters) => Some(self.parse_filters(filters)?),
            None => None,
        };
        if let Some(voice) = &req.voice {
            if voice.token.is_empty() || voice.endpoint.is_empty() || voice.session_id.is_empty() {
                return Err(ApiError::bad_request(
                    "Partial Lavalink voice state: token, endpoint and sessionId are required",
                ));
            }
        }
        // Resolving an identifier may take seconds (yt-dlp): do it before locking the player.
        let action = self.resolve_track(&req).await?;

        let mut inner = self.inner.lock().await;
        if let Some(voice) = req.voice {
            self.apply_voice(&mut inner, voice);
        }
        if let Some(paused) = req.paused {
            inner.paused = paused;
        }
        if let Some(volume) = req.volume {
            inner.volume = volume as u16;
        }
        let filters_changed = filters.is_some();
        if let Some(filters) = filters {
            inner.filters = Arc::new(filters);
        }
        let new_track = matches!(action, TrackAction::Play(_));
        if let Some(end_time) = req.end_time {
            inner.end_time = end_time.filter(|t| *t > 0);
        } else if new_track && !(no_replace && inner.active.is_some()) {
            inner.end_time = None;
        }

        if let Some(active) = &inner.active {
            active.controls.set_paused(inner.paused);
            active.controls.set_gain(inner.gain());
            active.controls.set_end_time(inner.end_time);
            if filters_changed {
                active.controls.set_filters(Arc::clone(&inner.filters));
            }
        }

        match action {
            TrackAction::Play(track) if !(no_replace && inner.active.is_some()) => {
                let start = req.position.unwrap_or(0);
                self.start_track(&mut inner, *track, start);
            }
            TrackAction::Play(_) => debug!(guild = self.guild_id, "noReplace: keeping current track"),
            TrackAction::Stop => self.stop_current(&mut inner, TrackEndReason::Stopped),
            TrackAction::Keep => {
                if let (Some(position), Some(active)) = (req.position, &inner.active) {
                    active.controls.seek(position);
                }
            }
        }
        Ok(self.model(&inner))
    }

    /// Validate `filters` and reject the ones disabled in `lavalink.server.filters`.
    fn parse_filters(&self, value: &serde_json::Value) -> Result<Filters, ApiError> {
        let filters = Filters::parse(value).map_err(ApiError::bad_request)?;
        let toggles = &self.ctx.config.lavalink.filters;
        let disabled: Vec<&str> = filters
            .present()
            .into_iter()
            .filter(|name| !toggles.is_enabled(name))
            .collect();
        if !disabled.is_empty() {
            return Err(ApiError::bad_request(format!(
                "Following filters are disabled in the config: {}",
                disabled.join(", ")
            )));
        }
        Ok(filters)
    }

    fn apply_voice(&self, inner: &mut PlayerInner, voice: VoiceState) {
        let Some(session) = self.session.upgrade() else { return };
        let info = VoiceConnectInfo {
            guild_id: self.guild_id,
            user_id: session.user_id,
            channel_id: voice.channel_id.as_deref().and_then(|c| c.parse().ok()),
            session_id: voice.session_id.clone(),
            token: voice.token.clone(),
            endpoint: voice.endpoint.clone(),
        };
        inner.voice = voice;
        if self.voice.matches(&info) {
            return;
        }
        info!(guild = self.guild_id, endpoint = %info.endpoint, "connecting to Discord voice");
        let connection = VoiceConnection::connect(info, self.ctx.voice_settings(), self.voice_events.clone());
        self.voice.replace(Some(connection));
    }

    fn start_track(self: &Arc<Self>, inner: &mut PlayerInner, mut track: Track, start_ms: u64) {
        if let Some(previous) = inner.active.take() {
            previous.controls.stop();
            if let Some(old) = inner.track.take() {
                self.emit(Event::TrackEndEvent {
                    guild_id: self.guild_id.to_string(),
                    track: old,
                    reason: TrackEndReason::Replaced,
                });
            }
        }
        track.info.position = start_ms;
        let id = self.playback_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let controls = PlaybackControls::new(
            start_ms,
            inner.paused,
            inner.gain(),
            inner.end_time,
            Arc::clone(&inner.filters),
        );
        inner.track = Some(track.clone());
        inner.active = Some(ActivePlayback {
            id,
            controls: Arc::clone(&controls),
        });
        *self.current_controls.lock().unwrap() = Some(Arc::clone(&controls));
        self.playing.store(true, Ordering::Relaxed);
        if let Some(session) = self.session.upgrade() {
            session.send(&OutgoingMessage::PlayerUpdate {
                guild_id: self.guild_id.to_string(),
                state: self.state(),
            });
        }
        self.emit(Event::TrackStartEvent {
            guild_id: self.guild_id.to_string(),
            track: track.clone(),
        });
        self.ctx.cache.on_track_started(&track.info);

        let weak = Arc::downgrade(self);
        let stuck_player = Arc::downgrade(self);
        let stuck_track = track.clone();
        let threshold_ms = self.ctx.config.lavalink.track_stuck_threshold_ms;
        let on_stuck = move || {
            if let Some(player) = stuck_player.upgrade() {
                player.emit(Event::TrackStuckEvent {
                    guild_id: player.guild_id.to_string(),
                    track: stuck_track.clone(),
                    threshold_ms,
                });
            }
        };
        let cache = Arc::clone(&self.ctx.cache);
        let voice = Arc::clone(&self.voice);
        let settings = self.ctx.playback_settings();
        let counters = Arc::clone(&self.ctx.frames);
        let live = self.ctx.live.clone();
        tokio::spawn(async move {
            let outcome =
                playback::run_track(cache, track.info, controls, voice, settings, counters, live, on_stuck).await;
            if let Some(player) = weak.upgrade() {
                player.on_playback_finished(id, outcome).await;
            }
        });
    }

    async fn on_playback_finished(&self, id: u64, outcome: PlaybackOutcome) {
        let mut inner = self.inner.lock().await;
        if inner.active.as_ref().map(|a| a.id) != Some(id) {
            return;
        }
        let active = inner.active.take().expect("checked above");
        self.position.store(active.controls.position(), Ordering::Relaxed);
        *self.current_controls.lock().unwrap() = None;
        self.playing.store(false, Ordering::Relaxed);
        let Some(track) = inner.track.take() else { return };
        let guild_id = self.guild_id.to_string();
        match outcome {
            PlaybackOutcome::Finished => self.emit(Event::TrackEndEvent {
                guild_id,
                track,
                reason: TrackEndReason::Finished,
            }),
            PlaybackOutcome::Failed(message) => {
                info!(guild = self.guild_id, title = %track.info.title, %message, "track failed");
                self.emit(Event::TrackExceptionEvent {
                    guild_id: guild_id.clone(),
                    track: track.clone(),
                    exception: Exception::new(message, Severity::Common),
                });
                self.emit(Event::TrackEndEvent {
                    guild_id,
                    track,
                    reason: TrackEndReason::LoadFailed,
                });
            }
            PlaybackOutcome::Stopped => {}
        }
    }

    fn stop_current(&self, inner: &mut PlayerInner, reason: TrackEndReason) {
        if let Some(active) = inner.active.take() {
            self.position.store(active.controls.position(), Ordering::Relaxed);
            active.controls.stop();
        }
        *self.current_controls.lock().unwrap() = None;
        self.playing.store(false, Ordering::Relaxed);
        if let Some(track) = inner.track.take() {
            self.emit(Event::TrackEndEvent {
                guild_id: self.guild_id.to_string(),
                track,
                reason,
            });
        }
    }

    /// Stop playback and leave the voice channel.
    pub async fn destroy(&self) {
        let mut inner = self.inner.lock().await;
        self.stop_current(&mut inner, TrackEndReason::Cleanup);
        self.voice.replace(None);
    }
}
