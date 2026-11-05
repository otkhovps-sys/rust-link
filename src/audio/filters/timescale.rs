//! `timescale`: tempo change without pitch change (WSOLA), followed by resampling.
//!
//! As in SoundTouch, `speed`, `pitch` and `rate` reduce to two operations:
//! a time-stretch by `speed / pitch` and a resample by `rate * pitch`.

use super::{effects::hermite, Timescale};
use crate::audio::encoder::SAMPLE_RATE;

const MIN_FACTOR: f64 = 0.1;
const MAX_FACTOR: f64 = 10.0;
const OVERLAP_MS: f64 = 8.0;
const FIR_TAPS: usize = 33;

fn ms_to_frames(ms: f64) -> usize {
    (SAMPLE_RATE as f64 * ms / 1000.0).round() as usize
}

pub(super) struct TimescaleStage {
    wsola: Wsola,
    resampler: Resampler,
    stretched: Vec<f32>,
}

impl TimescaleStage {
    pub fn new(config: Timescale) -> Self {
        let mut stage = Self {
            wsola: Wsola::new(),
            resampler: Resampler::new(),
            stretched: Vec::new(),
        };
        stage.set(config);
        stage
    }

    pub fn set(&mut self, config: Timescale) {
        let (speed, pitch, rate) = (config.speed as f64, config.pitch as f64, config.rate as f64);
        self.wsola.set_tempo((speed / pitch).clamp(MIN_FACTOR, MAX_FACTOR));
        self.resampler.set_ratio((rate * pitch).clamp(MIN_FACTOR, MAX_FACTOR));
    }

    /// Source milliseconds consumed per output millisecond.
    pub fn time_factor(&self) -> f64 {
        self.wsola.tempo * self.resampler.ratio
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.stretched.clear();
        self.wsola.process(input, &mut self.stretched);
        self.resampler.process(&self.stretched, out);
    }

    pub fn flush(&mut self, out: &mut Vec<f32>) {
        self.stretched.clear();
        self.wsola.flush(&mut self.stretched);
        self.resampler.process(&self.stretched, out);
        self.resampler.flush(out);
    }

    pub fn reset(&mut self) {
        self.wsola.reset();
        self.resampler.reset();
    }
}

/// Waveform-similarity overlap-add time-stretcher (interleaved stereo).
struct Wsola {
    tempo: f64,
    overlap: usize,
    sequence: usize,
    seek: usize,
    input: Vec<f32>,
    mid: Vec<f32>,
    mid_mono: Vec<f32>,
    scan: Vec<f32>,
    skip_fract: f64,
    beginning: bool,
    /// Output frames owed for the input received so far (`input / tempo`).
    expected: f64,
    emitted: usize,
}

impl Wsola {
    fn new() -> Self {
        let overlap = ms_to_frames(OVERLAP_MS);
        let mut wsola = Self {
            tempo: 1.0,
            overlap,
            sequence: 0,
            seek: 0,
            input: Vec::new(),
            mid: vec![0.0; overlap * 2],
            mid_mono: vec![0.0; overlap],
            scan: Vec::new(),
            skip_fract: 0.0,
            beginning: true,
            expected: 0.0,
            emitted: 0,
        };
        wsola.set_tempo(1.0);
        wsola
    }

    fn set_tempo(&mut self, tempo: f64) {
        self.tempo = tempo;
        // SoundTouch's automatic sequence / seek window lengths.
        let sequence_ms = (90.0 - (tempo - 0.5) * (50.0 / 1.5)).clamp(40.0, 90.0);
        let seek_ms = (20.0 - (tempo - 0.5) * (5.0 / 1.5)).clamp(15.0, 20.0);
        self.sequence = ms_to_frames(sequence_ms);
        self.seek = ms_to_frames(seek_ms);
    }

    fn bypass(&self) -> bool {
        (self.tempo - 1.0).abs() < 1e-3
    }

    fn is_idle(&self) -> bool {
        self.beginning && self.input.is_empty() && self.emitted == 0
    }

    fn nominal_skip(&self) -> f64 {
        self.tempo * (self.sequence - self.overlap) as f64
    }

    fn required(&self) -> usize {
        (self.nominal_skip().ceil() as usize + self.overlap).max(self.sequence) + self.seek
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.bypass() {
            if !self.is_idle() {
                self.flush(out);
            }
            out.extend_from_slice(input);
            return;
        }
        self.expected += (input.len() / 2) as f64 / self.tempo;
        self.input.extend_from_slice(input);
        self.run(out);
    }

    /// Emit as many sequences as the buffered input allows.
    fn run(&mut self, out: &mut Vec<f32>) {
        let ov = self.overlap;
        while self.input.len() / 2 >= self.required() {
            let offset = if self.beginning {
                self.beginning = false;
                out.extend_from_slice(&self.input[..ov * 2]);
                ov
            } else {
                let best = self.best_offset();
                for i in 0..ov {
                    let w = i as f32 / ov as f32;
                    for ch in 0..2 {
                        let a = self.mid[i * 2 + ch];
                        let b = self.input[(best + i) * 2 + ch];
                        out.push(a + (b - a) * w);
                    }
                }
                best + ov
            };
            let body = self.sequence - 2 * ov;
            out.extend_from_slice(&self.input[offset * 2..(offset + body) * 2]);
            let mid_start = offset + body;
            self.mid
                .copy_from_slice(&self.input[mid_start * 2..(mid_start + ov) * 2]);
            for (mono, frame) in self.mid_mono.iter_mut().zip(self.mid.as_chunks::<2>().0) {
                *mono = frame[0] + frame[1];
            }
            self.emitted += self.sequence - ov;

            self.skip_fract += self.nominal_skip();
            let skip = self.skip_fract as usize;
            self.skip_fract -= skip as f64;
            self.input.drain(..skip * 2);
        }
    }

    /// Offset in `[0, seek)` whose window best continues the previous sequence.
    fn best_offset(&mut self) -> usize {
        let ov = self.overlap;
        let span = self.seek + ov;
        self.scan.clear();
        self.scan.extend(
            self.input[..span * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|frame| frame[0] + frame[1]),
        );
        let score = |offset: usize| {
            let window = &self.scan[offset..offset + ov];
            let (mut corr, mut energy) = (0.0f32, 1e-9f32);
            for (m, x) in self.mid_mono.iter().zip(window) {
                corr += m * x;
                energy += x * x;
            }
            corr / energy.sqrt()
        };
        let mut best = 0;
        let mut best_score = f32::MIN;
        for offset in (0..self.seek).step_by(2) {
            let s = score(offset);
            if s > best_score {
                best = offset;
                best_score = s;
            }
        }
        for offset in [best.saturating_sub(1), best + 1] {
            if offset < self.seek && score(offset) > best_score {
                best = offset;
                best_score = score(offset);
            }
        }
        best
    }

    /// Emit the remaining audio, padding with silence and trimming to the owed length.
    fn flush(&mut self, out: &mut Vec<f32>) {
        if !self.is_idle() {
            let target = self.expected.round() as usize;
            let start = out.len();
            while self.emitted < target {
                let padded = self.input.len() + self.required() * 2;
                self.input.resize(padded, 0.0);
                self.run(out);
            }
            let excess = (self.emitted - target.min(self.emitted)) * 2;
            out.truncate(out.len() - excess.min(out.len() - start));
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.input.clear();
        self.skip_fract = 0.0;
        self.beginning = true;
        self.expected = 0.0;
        self.emitted = 0;
    }
}

/// Windowed-sinc low-pass applied before downsampling (`ratio > 1`).
struct AntiAlias {
    taps: [f32; FIR_TAPS],
    history: Vec<f32>,
}

impl AntiAlias {
    fn new(ratio: f64) -> Self {
        let mut filter = Self {
            taps: [0.0; FIR_TAPS],
            history: vec![0.0; (FIR_TAPS - 1) * 2],
        };
        filter.design(ratio);
        filter
    }

    fn design(&mut self, ratio: f64) {
        let cutoff = 0.45 / ratio;
        let middle = (FIR_TAPS - 1) as f64 / 2.0;
        let mut sum = 0.0;
        let mut taps = [0.0f64; FIR_TAPS];
        for (k, tap) in taps.iter_mut().enumerate() {
            let x = k as f64 - middle;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (std::f64::consts::TAU * cutoff * x).sin() / (std::f64::consts::PI * x)
            };
            let phase = std::f64::consts::TAU * k as f64 / (FIR_TAPS - 1) as f64;
            let blackman = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            *tap = sinc * blackman;
            sum += *tap;
        }
        for (dst, tap) in self.taps.iter_mut().zip(taps) {
            *dst = (tap / sum) as f32;
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let mut extended = std::mem::take(&mut self.history);
        extended.extend_from_slice(input);
        let frames = input.len() / 2;
        out.reserve(frames * 2);
        for j in 0..frames {
            let mut acc = [0.0f32; 2];
            for (k, tap) in self.taps.iter().enumerate() {
                let base = (j + k) * 2;
                acc[0] += tap * extended[base];
                acc[1] += tap * extended[base + 1];
            }
            out.extend_from_slice(&acc);
        }
        let keep = (FIR_TAPS - 1) * 2;
        self.history = extended.split_off(extended.len() - keep);
    }

    fn reset(&mut self) {
        self.history.fill(0.0);
    }
}

/// Streaming resampler (Catmull-Rom interpolation), `ratio` input frames per output frame.
struct Resampler {
    ratio: f64,
    position: f64,
    buffer: Vec<f32>,
    anti_alias: Option<AntiAlias>,
    filtered: Vec<f32>,
    expected: f64,
    emitted: usize,
}

impl Resampler {
    fn new() -> Self {
        Self {
            ratio: 1.0,
            position: 1.0,
            buffer: vec![0.0; 2],
            anti_alias: None,
            filtered: Vec::new(),
            expected: 0.0,
            emitted: 0,
        }
    }

    fn set_ratio(&mut self, ratio: f64) {
        self.ratio = ratio;
        if ratio > 1.0 + 1e-6 {
            match &mut self.anti_alias {
                Some(filter) => filter.design(ratio),
                None => self.anti_alias = Some(AntiAlias::new(ratio)),
            }
        } else {
            self.anti_alias = None;
        }
    }

    fn bypass(&self) -> bool {
        (self.ratio - 1.0).abs() < 1e-4
    }

    fn is_idle(&self) -> bool {
        self.expected == 0.0 && self.emitted == 0
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.bypass() {
            if !self.is_idle() {
                self.flush(out);
            }
            out.extend_from_slice(input);
            return;
        }
        self.expected += (input.len() / 2) as f64 / self.ratio;
        self.run(input, out);
    }

    fn run(&mut self, input: &[f32], out: &mut Vec<f32>) {
        match &mut self.anti_alias {
            Some(filter) => {
                self.filtered.clear();
                filter.process(input, &mut self.filtered);
                self.buffer.extend_from_slice(&self.filtered);
            }
            None => self.buffer.extend_from_slice(input),
        }
        let frames = self.buffer.len() / 2;
        let b = &self.buffer;
        while (self.position as usize) + 2 < frames {
            let i = self.position as usize;
            let t = (self.position - i as f64) as f32;
            for ch in 0..2 {
                out.push(hermite(
                    b[(i - 1) * 2 + ch],
                    b[i * 2 + ch],
                    b[(i + 1) * 2 + ch],
                    b[(i + 2) * 2 + ch],
                    t,
                ));
            }
            self.position += self.ratio;
            self.emitted += 1;
        }
        let consumed = (self.position as usize).saturating_sub(1).min(frames);
        self.buffer.drain(..consumed * 2);
        self.position -= consumed as f64;
    }

    fn flush(&mut self, out: &mut Vec<f32>) {
        if !self.is_idle() {
            let target = self.expected.round() as usize;
            let start = out.len();
            let silence = vec![0.0; (FIR_TAPS + 4) * 2];
            while self.emitted < target {
                self.run(&silence, out);
            }
            let excess = (self.emitted - target.min(self.emitted)) * 2;
            out.truncate(out.len() - excess.min(out.len() - start));
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.position = 1.0;
        self.buffer.clear();
        self.buffer.extend_from_slice(&[0.0, 0.0]);
        if let Some(filter) = &mut self.anti_alias {
            filter.reset();
        }
        self.expected = 0.0;
        self.emitted = 0;
    }
}
