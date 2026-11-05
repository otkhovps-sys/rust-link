//! Sample-by-sample filter stages (interleaved stereo, length preserving).

use std::f64::consts::TAU;

use super::{ChannelMix, Distortion, Karaoke, LowPass, Rotation, Tremolo, Vibrato, EQ_BANDS, EQ_FREQUENCIES};

/// Catmull-Rom interpolation between `y1` and `y2`.
pub(super) fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

/// Sine LFO with a phase kept in `[0, 2π)`.
struct Lfo {
    phase: f64,
    step: f64,
}

impl Lfo {
    fn new(frequency: f32, sample_rate: f32) -> Self {
        let mut lfo = Self { phase: 0.0, step: 0.0 };
        lfo.set(frequency, sample_rate);
        lfo
    }

    fn set(&mut self, frequency: f32, sample_rate: f32) {
        self.step = TAU * frequency as f64 / sample_rate as f64;
    }

    fn next(&mut self) -> f32 {
        let value = self.phase.sin() as f32;
        self.phase = (self.phase + self.step).rem_euclid(TAU);
        value
    }
}

/// `lowPass`: exponential smoothing, `y += (x - y) / smoothing`.
pub(super) struct LowPassStage {
    factor: f32,
    state: [f32; 2],
}

impl LowPassStage {
    pub fn new(config: LowPass) -> Self {
        let mut stage = Self {
            factor: 1.0,
            state: [0.0; 2],
        };
        stage.set(config);
        stage
    }

    pub fn set(&mut self, config: LowPass) {
        self.factor = 1.0 / config.smoothing;
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(2) {
            for (sample, state) in frame.iter_mut().zip(self.state.iter_mut()) {
                *state += (*sample - *state) * self.factor;
                *sample = *state;
            }
        }
    }

    pub fn reset(&mut self) {
        self.state = [0.0; 2];
    }
}

/// `channelMix`: 2x2 matrix between the left and right channels.
pub(super) struct ChannelMixStage(ChannelMix);

impl ChannelMixStage {
    pub fn new(config: ChannelMix) -> Self {
        Self(config)
    }

    pub fn set(&mut self, config: ChannelMix) {
        self.0 = config;
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        let m = self.0;
        for frame in buf.chunks_exact_mut(2) {
            let (l, r) = (frame[0], frame[1]);
            frame[0] = l * m.left_to_left + r * m.right_to_left;
            frame[1] = l * m.left_to_right + r * m.right_to_right;
        }
    }
}

/// `rotation` ("8D audio"): constant-power panning driven by a sine LFO.
pub(super) struct RotationStage {
    lfo: Lfo,
}

impl RotationStage {
    pub fn new(config: Rotation, sample_rate: f32) -> Self {
        Self {
            lfo: Lfo::new(config.rotation_hz, sample_rate),
        }
    }

    pub fn set(&mut self, config: Rotation, sample_rate: f32) {
        self.lfo.set(config.rotation_hz, sample_rate);
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(2) {
            // +1 pans fully left, -1 fully right.
            let angle = (1.0 + self.lfo.next()) * std::f32::consts::FRAC_PI_4;
            frame[0] *= angle.sin();
            frame[1] *= angle.cos();
        }
    }
}

/// One-pole DC blocker, keeps offsets created by waveshaping off the speakers.
#[derive(Default, Clone, Copy)]
struct DcBlocker {
    x1: f32,
    y1: f32,
}

impl DcBlocker {
    fn process(&mut self, x: f32) -> f32 {
        let y = x - self.x1 + 0.995 * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// `distortion`: sin/cos/tan waveshaping. A function only takes part once one of its
/// parameters leaves the default (offset 0, scale 1), as in Lavalink.
pub(super) struct DistortionStage {
    config: Distortion,
    sin: bool,
    cos: bool,
    tan: bool,
    dc: [DcBlocker; 2],
}

impl DistortionStage {
    pub fn new(config: Distortion) -> Self {
        let mut stage = Self {
            config,
            sin: false,
            cos: false,
            tan: false,
            dc: [DcBlocker::default(); 2],
        };
        stage.set(config);
        stage
    }

    pub fn set(&mut self, config: Distortion) {
        self.config = config;
        self.sin = config.sin_offset != 0.0 || config.sin_scale != 1.0;
        self.cos = config.cos_offset != 0.0 || config.cos_scale != 1.0;
        self.tan = config.tan_offset != 0.0 || config.tan_scale != 1.0;
    }

    fn shape(&self, x: f32) -> f32 {
        let c = &self.config;
        if !(self.sin || self.cos || self.tan) {
            return c.offset + c.scale * x;
        }
        let mut y = 1.0;
        if self.sin {
            y *= c.sin_offset + (x * c.sin_scale).sin();
        }
        if self.cos {
            y *= c.cos_offset + (x * c.cos_scale).cos();
        }
        if self.tan {
            y *= c.tan_offset + (x * c.tan_scale).tan();
        }
        let y = c.offset + c.scale * y;
        if y.is_finite() {
            y.clamp(-1.0, 1.0)
        } else {
            0.0
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(2) {
            for (ch, sample) in frame.iter_mut().enumerate() {
                let shaped = self.shape(*sample);
                *sample = self.dc[ch].process(shaped);
            }
        }
    }

    pub fn reset(&mut self) {
        self.dc = [DcBlocker::default(); 2];
    }
}

/// `tremolo`: amplitude modulation, gain swings between `1 - depth` and 1.
pub(super) struct TremoloStage {
    lfo: Lfo,
    depth: f32,
}

impl TremoloStage {
    pub fn new(config: Tremolo, sample_rate: f32) -> Self {
        Self {
            lfo: Lfo::new(config.frequency, sample_rate),
            depth: config.depth,
        }
    }

    pub fn set(&mut self, config: Tremolo, sample_rate: f32) {
        self.lfo.set(config.frequency, sample_rate);
        self.depth = config.depth;
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(2) {
            let gain = 1.0 - self.depth * 0.5 * (1.0 + self.lfo.next());
            frame[0] *= gain;
            frame[1] *= gain;
        }
    }
}

const VIBRATO_RING: usize = 512;
const VIBRATO_BASE_DELAY_SECS: f32 = 0.002;
const VIBRATO_EXTRA_DELAY: f32 = 3.0;

/// `vibrato`: pitch modulation through an LFO-driven fractional delay line.
pub(super) struct VibratoStage {
    lfo: Lfo,
    depth: f32,
    max_delay: f32,
    ring: Box<[[f32; 2]; VIBRATO_RING]>,
    write: usize,
}

impl VibratoStage {
    pub fn new(config: Vibrato, sample_rate: f32) -> Self {
        Self {
            lfo: Lfo::new(config.frequency, sample_rate),
            depth: config.depth,
            max_delay: VIBRATO_BASE_DELAY_SECS * sample_rate,
            ring: Box::new([[0.0; 2]; VIBRATO_RING]),
            write: 0,
        }
    }

    pub fn set(&mut self, config: Vibrato, sample_rate: f32) {
        self.lfo.set(config.frequency, sample_rate);
        self.depth = config.depth;
        self.max_delay = VIBRATO_BASE_DELAY_SECS * sample_rate;
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        const MASK: usize = VIBRATO_RING - 1;
        for frame in buf.chunks_exact_mut(2) {
            self.ring[self.write] = [frame[0], frame[1]];
            let lfo = 0.5 * (1.0 + self.lfo.next());
            let delay = VIBRATO_EXTRA_DELAY + lfo * self.depth * self.max_delay;
            let position = self.write as f32 + VIBRATO_RING as f32 - delay;
            let index = position.floor() as usize;
            let t = position - position.floor();
            let at = |offset: usize| self.ring[(index + offset).wrapping_sub(1) & MASK];
            let (p0, p1, p2, p3) = (at(0), at(1), at(2), at(3));
            for ch in 0..2 {
                frame[ch] = hermite(p0[ch], p1[ch], p2[ch], p3[ch], t);
            }
            self.write = (self.write + 1) & MASK;
        }
    }

    pub fn reset(&mut self) {
        self.ring.fill([0.0; 2]);
    }
}

/// `karaoke`: removes centre-panned audio (usually vocals) and adds back a band of
/// the mono signal around `filterBand` (usually bass and kick drum).
pub(super) struct KaraokeStage {
    level: f32,
    mono_level: f32,
    a: f32,
    b: f32,
    c: f32,
    y1: f32,
    y2: f32,
}

impl KaraokeStage {
    pub fn new(config: Karaoke, sample_rate: f32) -> Self {
        let mut stage = Self {
            level: 0.0,
            mono_level: 0.0,
            a: 0.0,
            b: 0.0,
            c: 0.0,
            y1: 0.0,
            y2: 0.0,
        };
        stage.set(config, sample_rate);
        stage
    }

    pub fn set(&mut self, config: Karaoke, sample_rate: f32) {
        self.level = config.level;
        self.mono_level = config.mono_level;
        // Two-pole resonator centred on `filterBand` with a bandwidth of `filterWidth`.
        let c = (-std::f32::consts::TAU * config.filter_width / sample_rate).exp();
        let b = -4.0 * c / (1.0 + c) * (std::f32::consts::TAU * config.filter_band / sample_rate).cos();
        self.a = (1.0 - b * b / (4.0 * c)).max(0.0).sqrt() * (1.0 - c);
        self.b = b;
        self.c = c;
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(2) {
            let (l, r) = (frame[0], frame[1]);
            let y = self.a * (l + r) * 0.5 - self.b * self.y1 - self.c * self.y2;
            self.y2 = self.y1;
            self.y1 = y;
            let mono = y * self.mono_level;
            frame[0] = l - r * self.level + mono;
            frame[1] = r - l * self.level + mono;
        }
    }

    pub fn reset(&mut self) {
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

#[derive(Clone, Copy, Default)]
struct BandCoefficients {
    alpha: f32,
    beta: f32,
    gamma: f32,
}

#[derive(Clone, Copy, Default)]
struct ChannelHistory {
    x1: f32,
    x2: f32,
    y: [[f32; 2]; EQ_BANDS],
}

/// 15-band equalizer laid out like Lavaplayer's: `out = x + 4 * Σ gain[i] * bandpass_i(x)`,
/// so a gain of -0.25 mutes a band and 0.25 doubles it.
pub(super) struct EqualizerStage {
    gains: [f32; EQ_BANDS],
    coefficients: [BandCoefficients; EQ_BANDS],
    history: [ChannelHistory; 2],
}

impl EqualizerStage {
    pub fn new(gains: [f32; EQ_BANDS], sample_rate: f32) -> Self {
        // Constant 0 dB peak band-pass biquads, 2/3 octave wide (bands are 2/3 octave apart).
        let q = 1.0 / (2f64.powf(1.0 / 3.0) - 2f64.powf(-1.0 / 3.0));
        let mut coefficients = [BandCoefficients::default(); EQ_BANDS];
        for (c, frequency) in coefficients.iter_mut().zip(EQ_FREQUENCIES) {
            let w0 = TAU * frequency as f64 / sample_rate as f64;
            let alpha = w0.sin() / (2.0 * q);
            let a0 = 1.0 + alpha;
            *c = BandCoefficients {
                alpha: (alpha / a0) as f32,
                beta: ((1.0 - alpha) / a0) as f32,
                gamma: (2.0 * w0.cos() / a0) as f32,
            };
        }
        Self {
            gains,
            coefficients,
            history: [ChannelHistory::default(); 2],
        }
    }

    pub fn set(&mut self, gains: [f32; EQ_BANDS]) {
        for (band, gain) in gains.iter().enumerate() {
            if self.gains[band] == 0.0 && *gain != 0.0 {
                // The band was skipped while muted: restart it from silence.
                for history in &mut self.history {
                    history.y[band] = [0.0; 2];
                }
            }
        }
        self.gains = gains;
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(2) {
            for (sample, history) in frame.iter_mut().zip(self.history.iter_mut()) {
                let x = *sample;
                let mut result = x * 0.25;
                for band in 0..EQ_BANDS {
                    let gain = self.gains[band];
                    if gain == 0.0 {
                        continue;
                    }
                    let c = self.coefficients[band];
                    let [y1, y2] = history.y[band];
                    let y = c.alpha * (x - history.x2) + c.gamma * y1 - c.beta * y2;
                    history.y[band] = [y, y1];
                    result += y * gain;
                }
                history.x2 = history.x1;
                history.x1 = x;
                *sample = result * 4.0;
            }
        }
    }

    pub fn reset(&mut self) {
        self.history = [ChannelHistory::default(); 2];
    }
}
