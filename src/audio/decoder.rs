//! Demux + decode with symphonia, resample to 48 kHz stereo with rubato.
//!
//! symphonia ships no Opus decoder, so a libopus backed [`OpusDecoder`] is registered in
//! the codec registry. Opus packets that are already 20 ms / 48 kHz / stereo are also
//! exposed untouched so the playback loop can forward them without re-encoding.

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
    sync::{Mutex, OnceLock},
};

use anyhow::{anyhow, Context};
use audiopus::coder::GenericCtl;
use rubato::{FftFixedIn, Resampler};
use symphonia::core::{
    audio::{AsAudioBufferRef, AudioBuffer, AudioBufferRef, Channels, SampleBuffer, Signal, SignalSpec},
    codecs::{
        CodecDescriptor, CodecParameters, CodecRegistry, Decoder, DecoderOptions, FinalizeResult, CODEC_TYPE_NULL,
        CODEC_TYPE_OPUS,
    },
    errors::{Error as SymphoniaError, Result as SymphoniaResult},
    formats::{FormatOptions, FormatReader, Packet, SeekMode, SeekTo},
    io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions},
    meta::MetadataOptions,
    probe::Hint,
    units::{Time, TimeBase},
};
use tracing::debug;

use super::encoder::{SAMPLES_PER_FRAME, SAMPLE_RATE};

/// Longest Opus frame (120 ms at 48 kHz).
const MAX_OPUS_FRAME: usize = 5_760;
const RESAMPLER_CHUNK: usize = 1_024;

pub fn codec_registry() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_all::<OpusDecoder>();
        registry
    })
}

/// libopus decoder exposed through symphonia's `Decoder` trait. Always outputs stereo.
pub struct OpusDecoder {
    params: CodecParameters,
    // libopus state is `Send` but not `Sync`; symphonia's `Decoder` requires both.
    decoder: Mutex<audiopus::coder::Decoder>,
    buf: AudioBuffer<f32>,
    pcm: Vec<f32>,
}

impl Decoder for OpusDecoder {
    fn try_new(params: &CodecParameters, _options: &DecoderOptions) -> SymphoniaResult<Self> {
        let decoder = audiopus::coder::Decoder::new(audiopus::SampleRate::Hz48000, audiopus::Channels::Stereo)
            .map_err(|_| SymphoniaError::DecodeError("opus: cannot create decoder"))?;
        let channels = Channels::FRONT_LEFT | Channels::FRONT_RIGHT;
        let mut params = params.clone();
        params.with_sample_rate(SAMPLE_RATE).with_channels(channels);
        Ok(Self {
            params,
            decoder: Mutex::new(decoder),
            buf: AudioBuffer::new(MAX_OPUS_FRAME as u64, SignalSpec::new(SAMPLE_RATE, channels)),
            pcm: vec![0.0; MAX_OPUS_FRAME * 2],
        })
    }

    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[CodecDescriptor {
            codec: CODEC_TYPE_OPUS,
            short_name: "opus",
            long_name: "Opus (libopus)",
            inst_func: |params, options| Ok(Box::new(OpusDecoder::try_new(params, options)?)),
        }]
    }

    fn reset(&mut self) {
        let _ = self.decoder.get_mut().unwrap().reset_state();
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> SymphoniaResult<AudioBufferRef<'_>> {
        let input = if packet.data.is_empty() {
            None
        } else {
            Some(
                audiopus::packet::Packet::try_from(&packet.data[..])
                    .map_err(|_| SymphoniaError::DecodeError("opus: invalid packet"))?,
            )
        };
        let output = audiopus::MutSignals::try_from(&mut self.pcm[..])
            .map_err(|_| SymphoniaError::DecodeError("opus: invalid output buffer"))?;
        let frames = self
            .decoder
            .get_mut()
            .unwrap()
            .decode_float(input, output, false)
            .map_err(|_| SymphoniaError::DecodeError("opus: corrupt packet"))?;
        self.buf.clear();
        self.buf.render_reserved(Some(frames));
        {
            let (left, right) = self.buf.chan_pair_mut(0, 1);
            for i in 0..frames {
                left[i] = self.pcm[2 * i];
                right[i] = self.pcm[2 * i + 1];
            }
        }
        self.buf.trim(packet.trim_start as usize, packet.trim_end as usize);
        Ok(self.buf.as_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        self.buf.as_audio_buffer_ref()
    }
}

/// Fixed-ratio FFT resampler for interleaved stereo.
struct StereoResampler {
    inner: FftFixedIn<f32>,
    input: [Vec<f32>; 2],
    output: Vec<Vec<f32>>,
    delay_remaining: usize,
    ratio: f64,
    input_frames: u64,
    output_frames: u64,
}

impl StereoResampler {
    fn new(source_rate: u32) -> anyhow::Result<Self> {
        let inner = FftFixedIn::<f32>::new(source_rate as usize, SAMPLE_RATE as usize, RESAMPLER_CHUNK, 2, 2)?;
        let output = inner.output_buffer_allocate(true);
        let delay_remaining = inner.output_delay();
        Ok(Self {
            inner,
            input: [Vec::new(), Vec::new()],
            output,
            delay_remaining,
            ratio: SAMPLE_RATE as f64 / source_rate as f64,
            input_frames: 0,
            output_frames: 0,
        })
    }

    fn emit(&mut self, produced: usize, out: &mut Vec<f32>) {
        // Flushing pads the input with zeros: never emit more than the input length allows.
        let expected = (self.input_frames as f64 * self.ratio).round() as u64;
        for i in 0..produced {
            if self.delay_remaining > 0 {
                self.delay_remaining -= 1;
                continue;
            }
            if self.output_frames >= expected {
                break;
            }
            self.output_frames += 1;
            out.push(self.output[0][i]);
            out.push(self.output[1][i]);
        }
    }

    fn process(&mut self, interleaved: &[f32], out: &mut Vec<f32>) -> anyhow::Result<()> {
        for frame in interleaved.chunks_exact(2) {
            self.input[0].push(frame[0]);
            self.input[1].push(frame[1]);
        }
        self.input_frames += (interleaved.len() / 2) as u64;
        loop {
            let needed = self.inner.input_frames_next();
            if self.input[0].len() < needed {
                return Ok(());
            }
            let (used, produced) = self.inner.process_into_buffer(
                &[&self.input[0][..needed], &self.input[1][..needed]],
                &mut self.output[..],
                None,
            )?;
            self.input[0].drain(..used);
            self.input[1].drain(..used);
            self.emit(produced, out);
        }
    }

    fn flush(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        if !self.input[0].is_empty() {
            let (_, produced) = self.inner.process_partial_into_buffer(
                Some(&[&self.input[0][..], &self.input[1][..]]),
                &mut self.output[..],
                None,
            )?;
            self.input[0].clear();
            self.input[1].clear();
            self.emit(produced, out);
        }
        let (_, produced) = self
            .inner
            .process_partial_into_buffer(None::<&[&[f32]]>, &mut self.output[..], None)?;
        self.emit(produced, out);
        Ok(())
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.input[0].clear();
        self.input[1].clear();
        self.delay_remaining = self.inner.output_delay();
        self.input_frames = 0;
        self.output_frames = 0;
    }
}

/// Metadata of one decoded packet; PCM is appended to the caller's buffer.
#[derive(Debug)]
pub struct DecodedPacket {
    pub timestamp_ms: u64,
    /// Original Opus packet when it can be sent to Discord as-is.
    pub opus: Option<Vec<u8>>,
}

pub struct TrackDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    params: CodecParameters,
    track_id: u32,
    time_base: Option<TimeBase>,
    is_opus: bool,
    sample_buf: Option<SampleBuffer<f32>>,
    stereo: Vec<f32>,
    resampler: Option<StereoResampler>,
    resampler_rate: u32,
    skip_until_ts: Option<u64>,
    finished: bool,
}

impl TrackDecoder {
    pub fn open(source: Box<dyn MediaSource>, container_hint: Option<&str>) -> anyhow::Result<Self> {
        let stream = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(container) = container_hint {
            hint.with_extension(container);
        }
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .context("unsupported or corrupt audio container")?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("no audio track found"))?;
        let params = track.codec_params.clone();
        let track_id = track.id;
        let decoder = codec_registry()
            .make(&params, &DecoderOptions::default())
            .context("unsupported audio codec")?;
        debug!(codec = ?params.codec, rate = ?params.sample_rate, channels = ?params.channels, "decoder opened");
        Ok(Self {
            is_opus: params.codec == CODEC_TYPE_OPUS,
            time_base: params.time_base,
            format,
            decoder,
            params,
            track_id,
            sample_buf: None,
            stereo: Vec::with_capacity(4096),
            resampler: None,
            resampler_rate: SAMPLE_RATE,
            skip_until_ts: None,
            finished: false,
        })
    }

    pub fn open_file(path: &Path, container_hint: Option<&str>) -> anyhow::Result<Self> {
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        Self::open(Box::new(file), container_hint)
    }

    /// Total duration when the container declares it.
    pub fn duration_ms(&self) -> Option<u64> {
        let frames = self.params.n_frames?;
        match self.time_base {
            Some(tb) => Some(time_to_ms(tb.calc_time(frames))),
            None => Some(frames * 1000 / self.params.sample_rate? as u64),
        }
    }

    fn ts_to_ms(&self, ts: u64) -> u64 {
        match self.time_base {
            Some(tb) => time_to_ms(tb.calc_time(ts)),
            None => ts * 1000 / self.params.sample_rate.unwrap_or(SAMPLE_RATE) as u64,
        }
    }

    pub fn seek(&mut self, position_ms: u64) -> anyhow::Result<u64> {
        let time = Time::new(position_ms / 1000, (position_ms % 1000) as f64 / 1000.0);
        let seeked = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        )?;
        self.decoder.reset();
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
        }
        self.skip_until_ts = Some(seeked.required_ts);
        self.finished = false;
        Ok(self.ts_to_ms(seeked.required_ts))
    }

    /// Decode the next packet, appending interleaved 48 kHz stereo PCM to `out`.
    /// Returns `None` at end of stream (after flushing the resampler into `out`).
    pub fn next_into(&mut self, out: &mut Vec<f32>) -> anyhow::Result<Option<DecodedPacket>> {
        if self.finished {
            return Ok(None);
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.finished = true;
                    if let Some(resampler) = &mut self.resampler {
                        resampler.flush(out)?;
                    }
                    return Ok(None);
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder = codec_registry().make(&self.params, &DecoderOptions::default())?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let timestamp_ms = self.ts_to_ms(packet.ts());
            let skip = self
                .skip_until_ts
                .is_some_and(|until| packet.ts() + packet.dur() <= until);
            if !skip {
                self.skip_until_ts = None;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(SymphoniaError::DecodeError(msg)) => {
                    debug!(msg, "skipping undecodable packet");
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if skip || decoded.frames() == 0 {
                continue;
            }

            let spec = *decoded.spec();
            let frames = decoded.frames();
            let sample_buf = match &mut self.sample_buf {
                Some(buf) if buf.capacity() >= frames * spec.channels.count() => buf,
                slot => slot.insert(SampleBuffer::new(frames.max(SAMPLES_PER_FRAME) as u64, spec)),
            };
            sample_buf.copy_interleaved_ref(decoded);
            let channels = spec.channels.count().max(1);
            self.stereo.clear();
            for frame in sample_buf.samples().chunks_exact(channels) {
                let left = frame[0];
                let right = if channels > 1 { frame[1] } else { left };
                self.stereo.push(left);
                self.stereo.push(right);
            }

            let opus = (self.is_opus && is_discord_ready_opus(&packet.data)).then(|| packet.data.to_vec());

            if spec.rate == SAMPLE_RATE {
                out.extend_from_slice(&self.stereo);
            } else {
                if self.resampler.is_none() || self.resampler_rate != spec.rate {
                    self.resampler = Some(StereoResampler::new(spec.rate)?);
                    self.resampler_rate = spec.rate;
                }
                self.resampler.as_mut().unwrap().process(&self.stereo, out)?;
            }
            return Ok(Some(DecodedPacket { timestamp_ms, opus }));
        }
    }
}

fn time_to_ms(time: Time) -> u64 {
    time.seconds * 1000 + (time.frac * 1000.0) as u64
}

/// 20 ms stereo Opus packet that Discord accepts without transcoding.
fn is_discord_ready_opus(data: &[u8]) -> bool {
    let samples = audiopus::packet::Packet::try_from(data)
        .ok()
        .and_then(|p| audiopus::packet::nb_samples(p, audiopus::SampleRate::Hz48000).ok());
    let channels = audiopus::packet::Packet::try_from(data)
        .ok()
        .and_then(|p| audiopus::packet::nb_channels(p).ok());
    samples == Some(SAMPLES_PER_FRAME) && matches!(channels, Some(audiopus::Channels::Stereo))
}

/// Container type from magic bytes.
pub fn detect_container(path: &Path) -> Option<&'static str> {
    let mut header = [0u8; 12];
    let mut file = File::open(path).ok()?;
    let n = file.read(&mut header).ok()?;
    sniff_container(&header[..n])
}

pub fn sniff_container(h: &[u8]) -> Option<&'static str> {
    if h.len() < 4 {
        return None;
    }
    if h.starts_with(b"OggS") {
        Some("ogg")
    } else if h.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some("webm")
    } else if h.len() >= 8 && &h[4..8] == b"ftyp" {
        Some("mp4")
    } else if h.starts_with(b"fLaC") {
        Some("flac")
    } else if h.len() >= 12 && h.starts_with(b"RIFF") && &h[8..12] == b"WAVE" {
        Some("wav")
    } else if h.starts_with(b"ID3") {
        Some("mp3")
    } else if h[0] == 0xFF && h[1] & 0xE0 == 0xE0 {
        // MPEG audio frame sync: layer bits 00 mean ADTS AAC.
        Some(if h[1] & 0x06 == 0 { "aac" } else { "mp3" })
    } else {
        None
    }
}

/// Duration from container metadata (no full decode).
pub fn probe_duration_ms(path: &Path) -> Option<u64> {
    let hint = detect_container(path);
    TrackDecoder::open_file(path, hint).ok()?.duration_ms()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// 16-bit PCM WAV with a sine tone.
    pub fn write_wav(path: &Path, rate: u32, channels: u16, seconds: f32) {
        let frames = (rate as f32 * seconds) as u32;
        let data_len = frames * channels as u32 * 2;
        let mut f = File::create(path).unwrap();
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
        for i in 0..frames {
            let v = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 12_000.0) as i16;
            for _ in 0..channels {
                f.write_all(&v.to_le_bytes()).unwrap();
            }
        }
    }

    fn decode_all(decoder: &mut TrackDecoder) -> usize {
        let mut pcm = Vec::new();
        while decoder.next_into(&mut pcm).unwrap().is_some() {}
        pcm.len() / 2
    }

    #[test]
    fn resamples_mono_44100_wav_to_48k_stereo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        write_wav(&path, 44_100, 1, 2.0);
        assert_eq!(detect_container(&path), Some("wav"));
        assert_eq!(probe_duration_ms(&path), Some(2_000));
        let mut decoder = TrackDecoder::open_file(&path, Some("wav")).unwrap();
        let frames = decode_all(&mut decoder);
        // 2 s at 48 kHz, trimmed to the exact resampled length.
        assert!((95_900..=96_000).contains(&frames), "got {frames} frames");
    }

    #[test]
    fn seek_skips_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone48.wav");
        write_wav(&path, 48_000, 2, 3.0);
        let mut decoder = TrackDecoder::open_file(&path, None).unwrap();
        let landed = decoder.seek(2_000).unwrap();
        assert!((1_950..=2_000).contains(&landed), "landed at {landed}");
        let frames = decode_all(&mut decoder);
        assert!((47_000..=50_000).contains(&frames), "got {frames} frames after seek");
    }

    #[test]
    fn decodes_and_passes_through_webm_opus_when_ffmpeg_exists() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("in.wav");
        let webm = dir.path().join("out.webm");
        write_wav(&wav, 48_000, 2, 1.0);
        let status = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-i"])
            .arg(&wav)
            .args(["-c:a", "libopus", "-b:a", "96k", "-frame_duration", "20"])
            .arg(&webm)
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            eprintln!("ffmpeg with libopus not available, skipping");
            return;
        }
        assert_eq!(detect_container(&webm), Some("webm"));
        let mut decoder = TrackDecoder::open_file(&webm, Some("webm")).unwrap();
        let mut pcm = Vec::new();
        let mut passthrough = 0;
        let mut packets = 0;
        while let Some(packet) = decoder.next_into(&mut pcm).unwrap() {
            packets += 1;
            passthrough += packet.opus.is_some() as usize;
        }
        assert!(packets >= 45, "packets {packets}");
        assert_eq!(passthrough, packets);
        assert!(pcm.len() / 2 >= 45 * 960);
    }

    #[test]
    fn sniffs_containers() {
        assert_eq!(sniff_container(b"OggS\0\0"), Some("ogg"));
        assert_eq!(sniff_container(&[0x1A, 0x45, 0xDF, 0xA3, 1]), Some("webm"));
        assert_eq!(sniff_container(b"\0\0\0\x20ftypM4A "), Some("mp4"));
        assert_eq!(sniff_container(&[0xFF, 0xFB, 0x90, 0x00]), Some("mp3"));
        assert_eq!(sniff_container(&[0xFF, 0xF1, 0x50, 0x80]), Some("aac"));
        assert_eq!(sniff_container(b"<html>"), None);
    }
}
