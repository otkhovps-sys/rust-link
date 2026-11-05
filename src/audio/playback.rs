//! Playback of one track: a decoder thread produces 20 ms Opus frames into a bounded
//! channel; an async sender paces them onto the voice UDP socket every 20 ms.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use symphonia::core::io::{MediaSource, ReadOnlySource};
use tokio::{
    sync::mpsc::{self, error::TryRecvError},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::{
    decoder::TrackDecoder,
    encoder::{apply_gain, FramePacker, OpusEncoder, FRAME_MS, SAMPLES_PER_FRAME, SILENCE_FRAME},
    filters::{FilterChain, Filters},
    voice_gateway::{VoiceHandle, VoiceTransport},
    FrameCounters,
};
use crate::{
    api::models::TrackInfo,
    cache::{progressive::ProgressiveReader, CacheManager, PlayableSource},
};

const SILENCE_FRAMES_AFTER_AUDIO: u8 = 5;
/// Interleaved stereo samples per millisecond at 48 kHz.
const SAMPLES_PER_MS: f64 = 96.0;
const NO_END_TIME: u64 = u64::MAX;

#[derive(Debug, Clone)]
pub struct PlaybackSettings {
    pub opus_bitrate: i32,
    pub opus_complexity: u8,
    pub opus_passthrough: bool,
    pub buffer_frames: usize,
    pub stuck_threshold: Duration,
}

/// Shared control block between the player, the decoder thread and the sender.
pub struct PlaybackControls {
    paused: AtomicBool,
    gain_bits: AtomicU32,
    position_ms: AtomicU64,
    end_time_ms: AtomicU64,
    generation: AtomicU64,
    seek_request: Mutex<Option<(u64, u64)>>,
    /// Filter settings and a version bumped on every change.
    filters: Mutex<(u64, Arc<Filters>)>,
    cancel: CancellationToken,
    cancelled: Arc<AtomicBool>,
}

impl PlaybackControls {
    pub fn new(start_ms: u64, paused: bool, gain: f32, end_time: Option<u64>, filters: Arc<Filters>) -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(paused),
            gain_bits: AtomicU32::new(gain.to_bits()),
            position_ms: AtomicU64::new(start_ms),
            end_time_ms: AtomicU64::new(end_time.unwrap_or(NO_END_TIME)),
            generation: AtomicU64::new(0),
            seek_request: Mutex::new((start_ms > 0).then_some((start_ms, 0))),
            filters: Mutex::new((0, filters)),
            cancel: CancellationToken::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain_bits.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    pub fn position(&self) -> u64 {
        self.position_ms.load(Ordering::Relaxed)
    }

    pub fn set_filters(&self, filters: Arc<Filters>) {
        let mut current = self.filters.lock().unwrap();
        current.0 += 1;
        current.1 = filters;
    }

    /// The filters, when they changed since version `seen`.
    fn filters_since(&self, seen: Option<u64>) -> Option<(u64, Arc<Filters>)> {
        let current = self.filters.lock().unwrap();
        (seen != Some(current.0)).then(|| (current.0, Arc::clone(&current.1)))
    }

    pub fn set_end_time(&self, end_time: Option<u64>) {
        self.end_time_ms
            .store(end_time.unwrap_or(NO_END_TIME), Ordering::Relaxed);
    }

    /// Frames tagged with an older generation are dropped by the sender.
    pub fn seek(&self, position_ms: u64) {
        let mut request = self.seek_request.lock().unwrap();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *request = Some((position_ms, generation));
        self.position_ms.store(position_ms, Ordering::Relaxed);
    }

    fn take_seek(&self) -> Option<(u64, u64)> {
        self.seek_request.lock().unwrap().take()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Invalidate every queued frame (the sender drops older generations).
    fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn stop(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.cancel.cancel();
    }

    pub fn is_stopped(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackOutcome {
    Finished,
    Failed(String),
    Stopped,
}

enum FrameMsg {
    Frame {
        opus: Vec<u8>,
        /// Track position once this frame has been played.
        end_ms: u64,
        generation: u64,
    },
    End,
    Error(String),
}

enum SourceSpec {
    File {
        path: std::path::PathBuf,
        container: Option<String>,
    },
    Progressive(Arc<crate::cache::DownloadProgress>),
    Live(Box<dyn MediaSource>),
    Remote {
        reader: crate::source::remote::RangedReader,
        container: Option<String>,
    },
}

impl SourceSpec {
    fn open(self, cancelled: Arc<AtomicBool>) -> anyhow::Result<TrackDecoder> {
        match self {
            SourceSpec::File { path, container } => {
                let hint = container.or_else(|| super::decoder::detect_container(&path).map(str::to_string));
                TrackDecoder::open_file(&path, hint.as_deref())
            }
            SourceSpec::Progressive(progress) => {
                let reader = ProgressiveReader::open(Arc::clone(&progress), Arc::clone(&cancelled))?;
                match TrackDecoder::open(Box::new(reader), None) {
                    Ok(decoder) => Ok(decoder),
                    Err(error) => {
                        // Some containers need the full file (e.g. MP4 with `moov` at the end).
                        debug!(error = %error, "progressive open failed, waiting for full download");
                        loop {
                            if cancelled.load(Ordering::Relaxed) {
                                anyhow::bail!("playback cancelled");
                            }
                            let state =
                                progress.wait_blocking(|s| s.done || s.error.is_some(), Duration::from_millis(250));
                            if let Some(e) = state.error {
                                anyhow::bail!(e);
                            }
                            if state.done {
                                break;
                            }
                        }
                        let hint = super::decoder::detect_container(&progress.final_path);
                        TrackDecoder::open_file(&progress.final_path, hint)
                    }
                }
            }
            SourceSpec::Live(source) => TrackDecoder::open(source, None),
            SourceSpec::Remote { reader, container } => TrackDecoder::open(Box::new(reader), container.as_deref()),
        }
    }
}

/// Acquire the audio (cache hit / OneDrive pull / download), decode and send it.
pub async fn run_track(
    cache: Arc<CacheManager>,
    info: TrackInfo,
    controls: Arc<PlaybackControls>,
    voice: Arc<VoiceHandle>,
    settings: PlaybackSettings,
    counters: Arc<FrameCounters>,
    on_stuck: impl Fn() + Send + 'static,
) -> PlaybackOutcome {
    let acquired = tokio::select! {
        _ = controls.cancel.cancelled() => return PlaybackOutcome::Stopped,
        result = cache.acquire(&info) => match result {
            Ok(acquired) => acquired,
            Err(e) => return PlaybackOutcome::Failed(format!("{e:#}")),
        },
    };
    debug!(title = %info.title, outcome = ?acquired.outcome, "audio source acquired");
    let _lease = acquired.lease;
    let spec = match acquired.source {
        PlayableSource::File { path, container } => SourceSpec::File { path, container },
        PlayableSource::Progressive { progress } => SourceSpec::Progressive(progress),
        PlayableSource::Remote { mut reader, container } => {
            reader.attach_cancel(controls.cancel.clone());
            SourceSpec::Remote { reader, container }
        }
        PlayableSource::Stream(stream) => {
            let reader = stream.into_blocking_reader(&tokio::runtime::Handle::current(), controls.cancel.clone());
            SourceSpec::Live(Box::new(ReadOnlySource::new(reader)))
        }
    };

    let (tx, rx) = mpsc::channel(settings.buffer_frames.max(2));
    let thread_controls = Arc::clone(&controls);
    let thread_settings = settings.clone();
    let spawned = std::thread::Builder::new()
        .name("rustlink-decoder".into())
        .spawn(move || decode_thread(spec, tx, thread_controls, thread_settings));
    if let Err(e) = spawned {
        return PlaybackOutcome::Failed(format!("cannot spawn decoder thread: {e}"));
    }

    send_loop(rx, &controls, &voice, &settings, &counters, on_stuck).await
}

fn decode_thread(
    spec: SourceSpec,
    tx: mpsc::Sender<FrameMsg>,
    controls: Arc<PlaybackControls>,
    settings: PlaybackSettings,
) {
    let result = decode_frames(spec, &tx, &controls, &settings);
    if let Err(error) = result {
        if !controls.is_stopped() {
            let _ = tx.blocking_send(FrameMsg::Error(format!("{error:#}")));
        }
    }
}

/// Recent decoded PCM before filters and volume, so a settings change can be re-rendered
/// from the playback position instead of waiting for the queued frames to drain.
struct PcmHistory {
    samples: VecDeque<f32>,
    start_ms: f64,
    capacity: usize,
}

impl PcmHistory {
    fn new(buffer_frames: usize) -> Self {
        // Queued frames can cover more source time with timescale: keep a wide margin.
        let ms = (buffer_frames as f64 * FRAME_MS as f64 * 4.0 + 1_000.0).min(30_000.0);
        Self {
            samples: VecDeque::new(),
            start_ms: 0.0,
            capacity: (ms * SAMPLES_PER_MS) as usize,
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
    }

    fn end_ms(&self) -> f64 {
        self.start_ms + self.samples.len() as f64 / SAMPLES_PER_MS
    }

    fn push(&mut self, timestamp_ms: u64, pcm: &[f32]) {
        let timestamp_ms = timestamp_ms as f64;
        if self.samples.is_empty() || (timestamp_ms - self.end_ms()).abs() > 100.0 {
            self.samples.clear();
            self.start_ms = timestamp_ms;
        }
        self.samples.extend(pcm.iter().copied());
        let excess = self.samples.len().saturating_sub(self.capacity) & !1;
        if excess > 0 {
            self.samples.drain(..excess);
            self.start_ms += excess as f64 / SAMPLES_PER_MS;
        }
    }

    /// PCM from about `position_ms` to the end, starting on a 20 ms boundary counted back
    /// from the end (so Opus passthrough can resume right after). Returns its start time.
    fn replay_from(&self, position_ms: f64) -> Option<(f64, Vec<f32>)> {
        let frames = self.samples.len() / 2;
        let offset = ((position_ms - self.start_ms) * SAMPLES_PER_MS / 2.0).round();
        if offset < 0.0 || offset as usize > frames {
            return None;
        }
        let remaining = frames - offset as usize;
        let aligned = remaining.div_ceil(SAMPLES_PER_FRAME) * SAMPLES_PER_FRAME;
        let first = frames.saturating_sub(aligned);
        let start_ms = self.start_ms + (first * 2) as f64 / SAMPLES_PER_MS;
        Some((start_ms, self.samples.range(first * 2..).copied().collect()))
    }
}

fn decode_frames(
    spec: SourceSpec,
    tx: &mpsc::Sender<FrameMsg>,
    controls: &PlaybackControls,
    settings: &PlaybackSettings,
) -> anyhow::Result<()> {
    let mut decoder = spec.open(Arc::clone(&controls.cancelled))?;
    let mut encoder: Option<OpusEncoder> = None;
    let mut packer = FramePacker::default();
    let mut chain = FilterChain::new();
    let mut filters_version = None;
    let mut pcm = Vec::with_capacity(8192);
    let mut filtered = Vec::with_capacity(8192);
    let mut history = PcmHistory::new(settings.buffer_frames);
    let mut generation = 0u64;
    let mut last_gain = controls.gain();
    // Track position (source time) at the start of the next frame. With timescale
    // filters each 20 ms frame covers `20 * speed * rate` ms of the source.
    let mut clock_ms = 0f64;

    let emit = |opus: Vec<u8>, end_ms: f64, generation: u64| -> bool {
        tx.blocking_send(FrameMsg::Frame {
            opus,
            end_ms: end_ms.round() as u64,
            generation,
        })
        .is_ok()
    };
    let mut encode = |frame: &[f32]| -> anyhow::Result<Vec<u8>> {
        let enc = match &mut encoder {
            Some(enc) => enc,
            slot => slot.insert(OpusEncoder::new(settings.opus_bitrate, settings.opus_complexity)?),
        };
        enc.encode(frame)
    };

    loop {
        if controls.is_stopped() {
            return Ok(());
        }
        if let Some((target, seek_generation)) = controls.take_seek() {
            match decoder.seek(target) {
                Ok(actual) => clock_ms = actual as f64,
                Err(e) => warn!(target, error = %e, "seek failed"),
            }
            packer.clear();
            chain.reset();
            history.clear();
            generation = seek_generation;
        }
        let mut changed = false;
        if let Some((version, filters)) = controls.filters_since(filters_version) {
            chain.configure(&filters);
            changed = filters_version.is_some();
            filters_version = Some(version);
        }
        let gain = controls.gain();
        if (gain - last_gain).abs() > 1e-4 {
            last_gain = gain;
            changed = true;
        }
        if changed {
            // Drop the queued frames and redo them with the new settings from the current
            // playback position: filter and volume changes are heard within ~20 ms.
            let new_generation = controls.bump_generation();
            if let Some((start_ms, replay)) = history.replay_from(controls.position() as f64) {
                generation = new_generation;
                packer.clear();
                chain.reset();
                clock_ms = start_ms;
                filtered.clear();
                chain.process(&replay, &mut filtered);
                packer.push(&filtered);
                let step = FRAME_MS as f64 * chain.time_factor();
                while let Some(mut frame) = packer.take_frame() {
                    apply_gain(&mut frame, gain);
                    clock_ms += step;
                    if !emit(encode(&frame)?, clock_ms, generation) {
                        return Ok(());
                    }
                }
            } else {
                // Nothing to replay (e.g. right after a seek): keep the queued audio.
                generation = new_generation;
                packer.clear();
                chain.reset();
                clock_ms = controls.position() as f64;
            }
        }

        pcm.clear();
        filtered.clear();
        let packet = decoder.next_into(&mut pcm)?;
        let Some(packet) = packet else {
            chain.process(&pcm, &mut filtered);
            chain.flush(&mut filtered);
            packer.push(&filtered);
            let mut frames = Vec::new();
            while let Some(frame) = packer.take_frame() {
                frames.push(frame);
            }
            frames.extend(packer.take_padded());
            let step = FRAME_MS as f64 * chain.time_factor();
            for mut frame in frames {
                apply_gain(&mut frame, gain);
                clock_ms += step;
                if !emit(encode(&frame)?, clock_ms, generation) {
                    return Ok(());
                }
            }
            let _ = tx.blocking_send(FrameMsg::End);
            return Ok(());
        };
        history.push(packet.timestamp_ms, &pcm);

        let neutral = (gain - 1.0).abs() < 1e-4 && chain.is_neutral();
        if settings.opus_passthrough && neutral && packer.is_empty() {
            if let Some(opus) = packet.opus {
                clock_ms = (packet.timestamp_ms + FRAME_MS) as f64;
                if !emit(opus, clock_ms, generation) {
                    return Ok(());
                }
                continue;
            }
        }

        if packer.is_empty() && !chain.has_latency() {
            clock_ms = packet.timestamp_ms as f64;
        }
        chain.process(&pcm, &mut filtered);
        packer.push(&filtered);
        let step = FRAME_MS as f64 * chain.time_factor();
        while let Some(mut frame) = packer.take_frame() {
            apply_gain(&mut frame, gain);
            clock_ms += step;
            if !emit(encode(&frame)?, clock_ms, generation) {
                return Ok(());
            }
        }
    }
}

async fn send_silence(transport: &Option<Arc<VoiceTransport>>) {
    if let Some(transport) = transport {
        for _ in 0..SILENCE_FRAMES_AFTER_AUDIO {
            let _ = transport.send_opus(&SILENCE_FRAME).await;
        }
        transport.set_speaking(false);
    }
}

async fn send_loop(
    mut rx: mpsc::Receiver<FrameMsg>,
    controls: &PlaybackControls,
    voice: &VoiceHandle,
    settings: &PlaybackSettings,
    counters: &FrameCounters,
    on_stuck: impl Fn(),
) -> PlaybackOutcome {
    let frame = Duration::from_millis(FRAME_MS);
    let mut deadline = Instant::now();
    let mut silence_left = 0u8;
    let mut starving_since: Option<Instant> = None;
    let mut stuck_reported = false;

    loop {
        deadline += frame;
        let now = Instant::now();
        if deadline + Duration::from_millis(200) < now {
            // Fell far behind (e.g. runtime stall): resync instead of bursting.
            deadline = now;
        }
        tokio::select! {
            _ = controls.cancel.cancelled() => {
                send_silence(&voice.transport()).await;
                return PlaybackOutcome::Stopped;
            }
            _ = tokio::time::sleep_until(deadline) => {}
        }

        let transport = voice.transport();
        if controls.is_paused() {
            starving_since = None;
            if let Some(t) = &transport {
                if silence_left > 0 {
                    let _ = t.send_opus(&SILENCE_FRAME).await;
                    silence_left -= 1;
                    if silence_left == 0 {
                        t.set_speaking(false);
                    }
                }
            }
            continue;
        }
        // Like Lavalink, the track does not advance without a voice connection.
        let Some(transport) = transport else {
            starving_since = None;
            continue;
        };
        if controls.position() >= controls.end_time_ms.load(Ordering::Relaxed) {
            send_silence(&Some(transport)).await;
            return PlaybackOutcome::Finished;
        }

        let message = loop {
            match rx.try_recv() {
                Ok(FrameMsg::Frame { generation, .. }) if generation != controls.generation() => continue,
                other => break other,
            }
        };
        match message {
            Ok(FrameMsg::Frame { opus, end_ms, .. }) => {
                starving_since = None;
                stuck_reported = false;
                transport.set_speaking(true);
                if let Err(e) = transport.send_opus(&opus).await {
                    debug!(error = %e, "udp send failed");
                }
                controls.position_ms.store(end_ms, Ordering::Relaxed);
                counters.sent.fetch_add(1, Ordering::Relaxed);
                silence_left = SILENCE_FRAMES_AFTER_AUDIO;
            }
            Ok(FrameMsg::End) | Err(TryRecvError::Disconnected) => {
                send_silence(&Some(transport)).await;
                return PlaybackOutcome::Finished;
            }
            Ok(FrameMsg::Error(error)) => {
                send_silence(&Some(transport)).await;
                return PlaybackOutcome::Failed(error);
            }
            Err(TryRecvError::Empty) => {
                counters.nulled.fetch_add(1, Ordering::Relaxed);
                let since = *starving_since.get_or_insert(now);
                if !stuck_reported && since.elapsed() >= settings.stuck_threshold {
                    stuck_reported = true;
                    on_stuck();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_replays_from_a_frame_boundary() {
        let mut history = PcmHistory::new(20);
        // 100 ms of audio starting at 1 s, sample value = sample index.
        let pcm: Vec<f32> = (0..9_600).map(|i| i as f32).collect();
        history.push(1_000, &pcm);
        assert!((history.end_ms() - 1_100.0).abs() < 1e-6);

        let (start, replay) = history.replay_from(1_050.0).unwrap();
        assert_eq!(replay.len() % (SAMPLES_PER_FRAME * 2), 0, "whole 20 ms frames");
        assert!(
            (1_040.0..=1_050.0).contains(&start),
            "starts at or just before the position: {start}"
        );
        assert_eq!(*replay.last().unwrap(), 9_599.0, "ends where decoding stopped");
        assert_eq!(replay[0], ((start - 1_000.0) * SAMPLES_PER_MS) as f32);

        assert!(history.replay_from(900.0).is_none(), "before the history");
        assert!(history.replay_from(1_200.0).is_none(), "after the history");

        // A gap restarts the history.
        history.push(5_000, &pcm[..960]);
        assert!((history.start_ms - 5_000.0).abs() < 1e-6);
    }

    #[test]
    fn history_is_bounded() {
        let mut history = PcmHistory::new(1);
        let second = vec![0.0f32; 96_000];
        for i in 0..5 {
            history.push(i * 1_000, &second);
        }
        assert!(history.samples.len() <= history.capacity);
        assert!((history.end_ms() - 5_000.0).abs() < 1e-6);
    }
}
