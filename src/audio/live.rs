//! Shared live streams. Players that play the same live stream with the same filters and
//! volume (a 24/7 radio bot in many servers) share one fetch, one decode and one Opus
//! encode: a channel task paces the frames every 20 ms and fans the packets out to every
//! subscribed player, so CPU and bandwidth stay flat as listeners are added.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use tokio::{
    sync::{broadcast, mpsc::error::TryRecvError},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::{
    encoder::{FRAME_MS, SILENCE_FRAME},
    filters::Filters,
    playback::{
        decode_thread, send_silence, source_spec, FrameMsg, PlaybackControls, PlaybackOutcome, PlaybackSettings,
        SILENCE_FRAMES_AFTER_AUDIO,
    },
    voice_gateway::VoiceHandle,
    FrameCounters,
};
use crate::{api::models::TrackInfo, cache::CacheManager};

/// How long a channel without listeners keeps running (quick replays and skips).
const IDLE_GRACE: Duration = Duration::from_secs(5);
const FAN_OUT_CAPACITY: usize = 64;

#[derive(Clone)]
enum LiveFrame {
    Opus(Arc<Vec<u8>>),
    End,
    Error(Arc<String>),
}

struct LiveChannel {
    key: String,
    tx: broadcast::Sender<LiveFrame>,
    subscribers: AtomicUsize,
    closed: AtomicBool,
    cancel: CancellationToken,
}

/// Decrements the listener count when a player leaves the channel.
struct Subscription {
    channel: Arc<LiveChannel>,
    rx: broadcast::Receiver<LiveFrame>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.channel.subscribers.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
pub struct LiveHub {
    channels: Mutex<HashMap<String, Arc<LiveChannel>>>,
}

/// Players share a channel only when they would produce identical packets.
fn channel_key(info: &TrackInfo, filters: &Filters, gain: f32) -> String {
    format!(
        "{}|{}|{}|{:08x}",
        info.source_name,
        info.uri.as_deref().unwrap_or(&info.identifier),
        serde_json::to_string(filters).unwrap_or_default(),
        gain.to_bits()
    )
}

impl LiveHub {
    pub fn channel_count(&self) -> usize {
        self.channels.lock().unwrap().len()
    }

    fn join(
        self: &Arc<Self>,
        info: &TrackInfo,
        filters: Arc<Filters>,
        gain: f32,
        cache: &Arc<CacheManager>,
        settings: &PlaybackSettings,
    ) -> Subscription {
        let key = channel_key(info, &filters, gain);
        let mut channels = self.channels.lock().unwrap();
        if let Some(channel) = channels.get(&key).filter(|c| !c.closed.load(Ordering::SeqCst)) {
            channel.subscribers.fetch_add(1, Ordering::SeqCst);
            debug!(
                listeners = channel.subscribers.load(Ordering::SeqCst),
                "joined shared live stream"
            );
            return Subscription {
                rx: channel.tx.subscribe(),
                channel: Arc::clone(channel),
            };
        }
        let (tx, rx) = broadcast::channel(FAN_OUT_CAPACITY);
        let channel = Arc::new(LiveChannel {
            key: key.clone(),
            tx,
            subscribers: AtomicUsize::new(1),
            closed: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        });
        channels.insert(key, Arc::clone(&channel));
        info!(title = %info.title, "starting shared live stream");
        tokio::spawn(run_channel(
            Arc::downgrade(self),
            Arc::clone(&channel),
            Arc::clone(cache),
            info.clone(),
            filters,
            gain,
            settings.clone(),
        ));
        Subscription { channel, rx }
    }

    fn remove(&self, channel: &Arc<LiveChannel>) {
        let mut channels = self.channels.lock().unwrap();
        if channels.get(&channel.key).is_some_and(|c| Arc::ptr_eq(c, channel)) {
            channels.remove(&channel.key);
        }
    }
}

/// One fetch + decode + encode for every listener of the channel.
async fn run_channel(
    hub: Weak<LiveHub>,
    channel: Arc<LiveChannel>,
    cache: Arc<CacheManager>,
    info: TrackInfo,
    filters: Arc<Filters>,
    gain: f32,
    settings: PlaybackSettings,
) {
    let controls = PlaybackControls::new(0, false, gain, None, filters);
    let outcome = produce(&channel, &cache, &info, &controls, &settings).await;
    controls.stop();
    channel.closed.store(true, Ordering::SeqCst);
    let _ = channel.tx.send(match outcome {
        Ok(()) => LiveFrame::End,
        Err(message) => LiveFrame::Error(Arc::new(message)),
    });
    if let Some(hub) = hub.upgrade() {
        hub.remove(&channel);
    }
    debug!(title = %info.title, "shared live stream stopped");
}

async fn produce(
    channel: &LiveChannel,
    cache: &Arc<CacheManager>,
    info: &TrackInfo,
    controls: &Arc<PlaybackControls>,
    settings: &PlaybackSettings,
) -> Result<(), String> {
    let acquired = tokio::select! {
        _ = channel.cancel.cancelled() => return Ok(()),
        result = cache.acquire(info) => result.map_err(|e| format!("{e:#}"))?,
    };
    let _lease = acquired.lease;
    let spec = source_spec(acquired.source, controls);
    let (tx, mut rx) = tokio::sync::mpsc::channel(settings.buffer_frames.max(2));
    let thread_controls = Arc::clone(controls);
    let thread_settings = settings.clone();
    std::thread::Builder::new()
        .name("rustlink-live".into())
        .spawn(move || decode_thread(spec, tx, thread_controls, thread_settings))
        .map_err(|e| format!("cannot spawn decoder thread: {e}"))?;

    let frame = Duration::from_millis(FRAME_MS);
    let mut deadline = Instant::now();
    let mut idle_since: Option<Instant> = None;
    loop {
        deadline += frame;
        let now = Instant::now();
        if deadline + Duration::from_millis(200) < now {
            deadline = now;
        }
        tokio::select! {
            _ = channel.cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep_until(deadline) => {}
        }
        if channel.subscribers.load(Ordering::SeqCst) == 0 {
            let since = *idle_since.get_or_insert(now);
            if since.elapsed() >= IDLE_GRACE {
                return Ok(());
            }
        } else {
            idle_since = None;
        }
        // The stream keeps moving even without listeners, so a rejoin lands on the live edge.
        match rx.try_recv() {
            Ok(FrameMsg::Frame { opus, .. }) => {
                let _ = channel.tx.send(LiveFrame::Opus(Arc::new(opus)));
            }
            Ok(FrameMsg::End) | Err(TryRecvError::Disconnected) => return Ok(()),
            Ok(FrameMsg::Error(error)) => return Err(error),
            Err(TryRecvError::Empty) => {}
        }
    }
}

/// Playback of a live stream through a shared channel (instead of `send_loop`).
#[allow(clippy::too_many_arguments)]
pub async fn play_shared(
    hub: &Arc<LiveHub>,
    cache: &Arc<CacheManager>,
    info: &TrackInfo,
    controls: &PlaybackControls,
    voice: &VoiceHandle,
    settings: &PlaybackSettings,
    counters: &FrameCounters,
    on_stuck: impl Fn(),
) -> PlaybackOutcome {
    let mut silence_left = 0u8;
    loop {
        let (version, filters) = controls.current_filters();
        let gain = controls.gain();
        let mut subscription = hub.join(info, filters, gain, cache, settings);
        let mut last_frame = Instant::now();
        let mut stuck_reported = false;
        let rejoin = loop {
            let frame = tokio::select! {
                _ = controls.cancel.cancelled() => {
                    send_silence(&voice.transport()).await;
                    return PlaybackOutcome::Stopped;
                }
                _ = tokio::time::sleep_until(last_frame + settings.stuck_threshold), if !stuck_reported => {
                    stuck_reported = true;
                    on_stuck();
                    continue;
                }
                frame = subscription.rx.recv() => frame,
            };
            match frame {
                Ok(LiveFrame::Opus(opus)) => {
                    last_frame = Instant::now();
                    stuck_reported = false;
                    // Different filters or volume: move to the matching channel.
                    if controls.filters_version() != version || (controls.gain() - gain).abs() > 1e-4 {
                        break true;
                    }
                    let Some(transport) = voice.transport() else { continue };
                    if controls.is_paused() {
                        if silence_left > 0 {
                            let _ = transport.send_opus(&SILENCE_FRAME).await;
                            silence_left -= 1;
                            if silence_left == 0 {
                                transport.set_speaking(false);
                            }
                        }
                        continue;
                    }
                    transport.set_speaking(true);
                    if let Err(e) = transport.send_opus(&opus).await {
                        debug!(error = %e, "udp send failed");
                    }
                    controls.advance_position(FRAME_MS);
                    counters.sent.fetch_add(1, Ordering::Relaxed);
                    silence_left = SILENCE_FRAMES_AFTER_AUDIO;
                }
                Ok(LiveFrame::End) | Err(broadcast::error::RecvError::Closed) => break false,
                Ok(LiveFrame::Error(error)) => {
                    send_silence(&voice.transport()).await;
                    return PlaybackOutcome::Failed(error.to_string());
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    counters.nulled.fetch_add(skipped, Ordering::Relaxed);
                }
            }
        };
        if !rejoin {
            send_silence(&voice.transport()).await;
            return PlaybackOutcome::Finished;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_keys_separate_settings() {
        let info = TrackInfo {
            identifier: "https://radio.example/live.m3u8".into(),
            uri: Some("https://radio.example/live.m3u8".into()),
            source_name: "http".into(),
            is_stream: true,
            ..Default::default()
        };
        let plain = Filters::default();
        let nightcore = Filters::parse(&serde_json::json!({"timescale": {"speed": 1.2}})).unwrap();
        assert_eq!(channel_key(&info, &plain, 1.0), channel_key(&info, &plain, 1.0));
        assert_ne!(channel_key(&info, &plain, 1.0), channel_key(&info, &plain, 0.5));
        assert_ne!(channel_key(&info, &plain, 1.0), channel_key(&info, &nightcore, 1.0));
        let mut other = info.clone();
        other.uri = Some("https://radio.example/other.m3u8".into());
        assert_ne!(channel_key(&info, &plain, 1.0), channel_key(&other, &plain, 1.0));
    }
}
