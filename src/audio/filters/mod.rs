//! Lavalink v4 audio filters: request model, validation and the DSP chain.
//!
//! Every filter runs on 48 kHz interleaved stereo `f32`. The processing order follows
//! Lavalink's `FilterChain`: lowPass -> channelMix -> rotation -> distortion -> vibrato ->
//! tremolo -> timescale -> karaoke -> equalizer -> volume.

mod effects;
mod timescale;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use self::{
    effects::{
        ChannelMixStage, DistortionStage, EqualizerStage, KaraokeStage, LowPassStage, RotationStage, TremoloStage,
        VibratoStage,
    },
    timescale::TimescaleStage,
};
use super::encoder::SAMPLE_RATE;

/// Number of equalizer bands (Lavaplayer layout).
pub const EQ_BANDS: usize = 15;
/// Centre frequencies of the 15 equalizer bands, in Hz.
pub const EQ_FREQUENCIES: [f32; EQ_BANDS] = [
    25.0, 40.0, 63.0, 100.0, 160.0, 250.0, 400.0, 630.0, 1000.0, 1600.0, 2500.0, 4000.0, 6300.0, 10000.0, 16000.0,
];
pub const EQ_MIN_GAIN: f32 = -0.25;
pub const EQ_MAX_GAIN: f32 = 1.0;
pub const VIBRATO_MAX_FREQUENCY: f32 = 14.0;

/// Names used by Lavalink in `/v4/info` and in the `lavalink.server.filters` config.
pub const FILTER_NAMES: [&str; 10] = [
    "volume",
    "equalizer",
    "karaoke",
    "timescale",
    "tremolo",
    "vibrato",
    "distortion",
    "rotation",
    "channelMix",
    "lowPass",
];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub band: i32,
    pub gain: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Karaoke {
    pub level: f32,
    pub mono_level: f32,
    pub filter_band: f32,
    pub filter_width: f32,
}

impl Default for Karaoke {
    fn default() -> Self {
        Self {
            level: 1.0,
            mono_level: 1.0,
            filter_band: 220.0,
            filter_width: 100.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Timescale {
    pub speed: f32,
    pub pitch: f32,
    pub rate: f32,
}

impl Default for Timescale {
    fn default() -> Self {
        Self {
            speed: 1.0,
            pitch: 1.0,
            rate: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Tremolo {
    pub frequency: f32,
    pub depth: f32,
}

impl Default for Tremolo {
    fn default() -> Self {
        Self {
            frequency: 2.0,
            depth: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Vibrato {
    pub frequency: f32,
    pub depth: f32,
}

impl Default for Vibrato {
    fn default() -> Self {
        Self {
            frequency: 2.0,
            depth: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Rotation {
    pub rotation_hz: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Distortion {
    pub sin_offset: f32,
    pub sin_scale: f32,
    pub cos_offset: f32,
    pub cos_scale: f32,
    pub tan_offset: f32,
    pub tan_scale: f32,
    pub offset: f32,
    pub scale: f32,
}

impl Default for Distortion {
    fn default() -> Self {
        Self {
            sin_offset: 0.0,
            sin_scale: 1.0,
            cos_offset: 0.0,
            cos_scale: 1.0,
            tan_offset: 0.0,
            tan_scale: 1.0,
            offset: 0.0,
            scale: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ChannelMix {
    pub left_to_left: f32,
    pub left_to_right: f32,
    pub right_to_left: f32,
    pub right_to_right: f32,
}

impl Default for ChannelMix {
    fn default() -> Self {
        Self {
            left_to_left: 1.0,
            left_to_right: 0.0,
            right_to_left: 0.0,
            right_to_right: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LowPass {
    pub smoothing: f32,
}

impl Default for LowPass {
    fn default() -> Self {
        Self { smoothing: 20.0 }
    }
}

/// The `filters` object of a Lavalink player. A `PATCH` replaces every filter at once.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Filters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equalizer: Option<Vec<EqBand>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub karaoke: Option<Karaoke>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timescale: Option<Timescale>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tremolo: Option<Tremolo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vibrato: Option<Vibrato>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<Rotation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distortion: Option<Distortion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_mix: Option<ChannelMix>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_pass: Option<LowPass>,
    /// Filters handled by Lavalink plugins: stored and echoed back, never applied.
    #[serde(deserialize_with = "null_as_empty", skip_serializing_if = "Map::is_empty")]
    pub plugin_filters: Map<String, Value>,
}

fn null_as_empty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Map<String, Value>, D::Error> {
    Ok(Option::<Map<String, Value>>::deserialize(deserializer)?.unwrap_or_default())
}

fn ensure(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

impl Filters {
    /// Parse and validate the `filters` field of an update-player request.
    pub fn parse(value: &Value) -> Result<Self, String> {
        if !value.is_object() {
            return Err("filters must be an object".into());
        }
        let mut filters: Filters =
            serde_json::from_value(value.clone()).map_err(|e| format!("Invalid filters: {e}"))?;
        filters.validate()?;
        filters.normalize();
        Ok(filters)
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(volume) = self.volume {
            ensure(
                (0.0..=5.0).contains(&volume),
                "filters.volume must be between 0.0 and 5.0",
            )?;
        }
        for band in self.equalizer.iter().flatten() {
            ensure(
                (0..EQ_BANDS as i32).contains(&band.band),
                "filters.equalizer[].band must be between 0 and 14",
            )?;
        }
        if let Some(k) = &self.karaoke {
            ensure(
                k.filter_band > 0.0 && k.filter_band < SAMPLE_RATE as f32 / 2.0,
                "filters.karaoke.filterBand must be between 0 and 24000 Hz",
            )?;
            ensure(
                k.filter_width > 0.0,
                "filters.karaoke.filterWidth must be greater than 0",
            )?;
        }
        if let Some(t) = &self.timescale {
            ensure(t.speed > 0.0, "filters.timescale.speed must be greater than 0")?;
            ensure(t.pitch > 0.0, "filters.timescale.pitch must be greater than 0")?;
            ensure(t.rate > 0.0, "filters.timescale.rate must be greater than 0")?;
        }
        if let Some(t) = &self.tremolo {
            ensure(t.frequency > 0.0, "filters.tremolo.frequency must be greater than 0")?;
            ensure(
                (0.0..=1.0).contains(&t.depth),
                "filters.tremolo.depth must be between 0.0 and 1.0",
            )?;
        }
        if let Some(v) = &self.vibrato {
            ensure(
                v.frequency > 0.0 && v.frequency <= VIBRATO_MAX_FREQUENCY,
                "filters.vibrato.frequency must be greater than 0 and at most 14",
            )?;
            ensure(
                (0.0..=1.0).contains(&v.depth),
                "filters.vibrato.depth must be between 0.0 and 1.0",
            )?;
        }
        if let Some(m) = &self.channel_mix {
            let in_range = [m.left_to_left, m.left_to_right, m.right_to_left, m.right_to_right]
                .iter()
                .all(|v| (0.0..=1.0).contains(v));
            ensure(in_range, "filters.channelMix values must be between 0.0 and 1.0")?;
        }
        Ok(())
    }

    /// Equalizer bands: last value per band wins, gains clamped like Lavaplayer, sorted.
    fn normalize(&mut self) {
        if let Some(bands) = self.equalizer.take() {
            let mut gains: [Option<f32>; EQ_BANDS] = [None; EQ_BANDS];
            for band in bands {
                gains[band.band as usize] = Some(band.gain.clamp(EQ_MIN_GAIN, EQ_MAX_GAIN));
            }
            let bands: Vec<EqBand> = gains
                .iter()
                .enumerate()
                .filter_map(|(band, gain)| {
                    gain.map(|gain| EqBand {
                        band: band as i32,
                        gain,
                    })
                })
                .collect();
            self.equalizer = (!bands.is_empty()).then_some(bands);
        }
    }

    /// Names (as in [`FILTER_NAMES`]) of the filters present in this object.
    pub fn present(&self) -> Vec<&'static str> {
        let flags = [
            self.volume.is_some(),
            self.equalizer.is_some(),
            self.karaoke.is_some(),
            self.timescale.is_some(),
            self.tremolo.is_some(),
            self.vibrato.is_some(),
            self.distortion.is_some(),
            self.rotation.is_some(),
            self.channel_mix.is_some(),
            self.low_pass.is_some(),
        ];
        FILTER_NAMES
            .iter()
            .zip(flags)
            .filter_map(|(name, present)| present.then_some(*name))
            .collect()
    }
}

impl Karaoke {
    fn is_active(&self) -> bool {
        self.level != 0.0 || self.mono_level != 0.0
    }
}

impl Timescale {
    fn is_active(&self) -> bool {
        self.speed != 1.0 || self.pitch != 1.0 || self.rate != 1.0
    }
}

impl Tremolo {
    fn is_active(&self) -> bool {
        self.depth > 0.0 && self.frequency > 0.0
    }
}

impl Vibrato {
    fn is_active(&self) -> bool {
        self.depth > 0.0 && self.frequency > 0.0
    }
}

impl Rotation {
    fn is_active(&self) -> bool {
        self.rotation_hz != 0.0
    }
}

impl Distortion {
    fn is_active(&self) -> bool {
        *self != Distortion::default()
    }
}

impl ChannelMix {
    fn is_active(&self) -> bool {
        *self != ChannelMix::default()
    }
}

impl LowPass {
    fn is_active(&self) -> bool {
        self.smoothing > 1.0
    }
}

/// Create, update or drop a stage so it matches its (optional) active configuration.
fn sync_stage<C: Copy, S>(
    slot: &mut Option<S>,
    config: Option<C>,
    create: impl FnOnce(C) -> S,
    update: impl FnOnce(&mut S, C),
) {
    match (slot.as_mut(), config) {
        (Some(stage), Some(config)) => update(stage, config),
        (None, Some(config)) => *slot = Some(create(config)),
        (_, None) => *slot = None,
    }
}

/// Stateful DSP chain for one playback. Reconfigured in place when filters change.
pub struct FilterChain {
    low_pass: Option<LowPassStage>,
    channel_mix: Option<ChannelMixStage>,
    rotation: Option<RotationStage>,
    distortion: Option<DistortionStage>,
    vibrato: Option<VibratoStage>,
    tremolo: Option<TremoloStage>,
    timescale: Option<TimescaleStage>,
    karaoke: Option<KaraokeStage>,
    equalizer: Option<EqualizerStage>,
    volume: f32,
    /// Audio still held by a timescale stage that was just switched off.
    pending: Vec<f32>,
    work: Vec<f32>,
}

impl Default for FilterChain {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterChain {
    pub fn new() -> Self {
        Self {
            low_pass: None,
            channel_mix: None,
            rotation: None,
            distortion: None,
            vibrato: None,
            tremolo: None,
            timescale: None,
            karaoke: None,
            equalizer: None,
            volume: 1.0,
            pending: Vec::new(),
            work: Vec::new(),
        }
    }

    pub fn from_filters(filters: &Filters) -> Self {
        let mut chain = Self::new();
        chain.configure(filters);
        chain
    }

    /// Apply new settings, keeping filter state (phases, delay lines) where possible.
    pub fn configure(&mut self, f: &Filters) {
        let rate = SAMPLE_RATE as f32;
        self.volume = f.volume.unwrap_or(1.0);
        sync_stage(
            &mut self.low_pass,
            f.low_pass.filter(LowPass::is_active),
            LowPassStage::new,
            LowPassStage::set,
        );
        sync_stage(
            &mut self.channel_mix,
            f.channel_mix.filter(ChannelMix::is_active),
            ChannelMixStage::new,
            ChannelMixStage::set,
        );
        sync_stage(
            &mut self.rotation,
            f.rotation.filter(Rotation::is_active),
            |c| RotationStage::new(c, rate),
            |s, c| s.set(c, rate),
        );
        sync_stage(
            &mut self.distortion,
            f.distortion.filter(Distortion::is_active),
            DistortionStage::new,
            DistortionStage::set,
        );
        sync_stage(
            &mut self.vibrato,
            f.vibrato.filter(Vibrato::is_active),
            |c| VibratoStage::new(c, rate),
            |s, c| s.set(c, rate),
        );
        sync_stage(
            &mut self.tremolo,
            f.tremolo.filter(Tremolo::is_active),
            |c| TremoloStage::new(c, rate),
            |s, c| s.set(c, rate),
        );
        let timescale = f.timescale.filter(Timescale::is_active);
        if timescale.is_none() {
            if let Some(mut stage) = self.timescale.take() {
                stage.flush(&mut self.pending);
            }
        }
        sync_stage(&mut self.timescale, timescale, TimescaleStage::new, TimescaleStage::set);
        sync_stage(
            &mut self.karaoke,
            f.karaoke.filter(Karaoke::is_active),
            |c| KaraokeStage::new(c, rate),
            |s, c| s.set(c, rate),
        );
        let gains = f.equalizer.as_ref().map(|bands| {
            let mut gains = [0.0f32; EQ_BANDS];
            for band in bands {
                gains[band.band as usize] = band.gain;
            }
            gains
        });
        sync_stage(
            &mut self.equalizer,
            gains.filter(|g| g.iter().any(|v| *v != 0.0)),
            |g| EqualizerStage::new(g, rate),
            EqualizerStage::set,
        );
    }

    fn has_stages(&self) -> bool {
        self.low_pass.is_some()
            || self.channel_mix.is_some()
            || self.rotation.is_some()
            || self.distortion.is_some()
            || self.vibrato.is_some()
            || self.tremolo.is_some()
            || self.timescale.is_some()
            || self.karaoke.is_some()
            || self.equalizer.is_some()
            || (self.volume - 1.0).abs() > 1e-4
    }

    /// Nothing to do: the audio passes through bit-exact (Opus passthrough stays possible).
    pub fn is_neutral(&self) -> bool {
        !self.has_stages() && self.pending.is_empty()
    }

    /// The chain holds audio internally, so source timestamps lead the output.
    pub fn has_latency(&self) -> bool {
        self.timescale.is_some() || !self.pending.is_empty()
    }

    /// Milliseconds of source audio per millisecond of output (`speed * rate`).
    pub fn time_factor(&self) -> f64 {
        self.timescale.as_ref().map_or(1.0, TimescaleStage::time_factor)
    }

    /// Process interleaved stereo `input`, appending the result to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_neutral() {
            out.extend_from_slice(input);
            return;
        }
        let mut work = std::mem::take(&mut self.work);
        work.clear();
        work.extend_from_slice(input);
        if let Some(s) = &mut self.low_pass {
            s.process(&mut work);
        }
        if let Some(s) = &mut self.channel_mix {
            s.process(&mut work);
        }
        if let Some(s) = &mut self.rotation {
            s.process(&mut work);
        }
        if let Some(s) = &mut self.distortion {
            s.process(&mut work);
        }
        if let Some(s) = &mut self.vibrato {
            s.process(&mut work);
        }
        if let Some(s) = &mut self.tremolo {
            s.process(&mut work);
        }
        let start = out.len();
        out.append(&mut self.pending);
        match &mut self.timescale {
            Some(stage) => stage.process(&work, out),
            None => out.extend_from_slice(&work),
        }
        self.work = work;
        self.post_process(&mut out[start..]);
    }

    /// End of stream: emit whatever the chain still holds.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        let start = out.len();
        out.append(&mut self.pending);
        if let Some(stage) = &mut self.timescale {
            stage.flush(out);
        }
        self.post_process(&mut out[start..]);
    }

    fn post_process(&mut self, out: &mut [f32]) {
        if let Some(s) = &mut self.karaoke {
            s.process(out);
        }
        if let Some(s) = &mut self.equalizer {
            s.process(out);
        }
        let volume = self.volume;
        for sample in out.iter_mut() {
            *sample = (*sample * volume).clamp(-1.0, 1.0);
        }
    }

    /// Drop buffered audio and filter history (after a seek).
    pub fn reset(&mut self) {
        self.pending.clear();
        if let Some(s) = &mut self.low_pass {
            s.reset();
        }
        if let Some(s) = &mut self.distortion {
            s.reset();
        }
        if let Some(s) = &mut self.vibrato {
            s.reset();
        }
        if let Some(s) = &mut self.timescale {
            s.reset();
        }
        if let Some(s) = &mut self.karaoke {
            s.reset();
        }
        if let Some(s) = &mut self.equalizer {
            s.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn sine(freq: f32, seconds: f32, amplitude: f32) -> Vec<f32> {
        let frames = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * freq * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * amplitude;
                [v, v]
            })
            .collect()
    }

    pub(super) fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
    }

    /// Dominant frequency of the left channel, from rising zero crossings.
    pub(super) fn frequency(samples: &[f32]) -> f32 {
        let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
        let crossings = left.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        crossings as f32 * SAMPLE_RATE as f32 / left.len() as f32
    }

    fn run(filters: Value, input: &[f32]) -> Vec<f32> {
        let filters = Filters::parse(&filters).unwrap();
        let mut chain = FilterChain::from_filters(&filters);
        let mut out = Vec::new();
        for chunk in input.chunks(2048) {
            chain.process(chunk, &mut out);
        }
        chain.flush(&mut out);
        out
    }

    #[test]
    fn parses_lavalink_filters_with_defaults() {
        let filters = Filters::parse(&json!({
            "volume": 1,
            "equalizer": [{"band": 3, "gain": 2.0}, {"band": 0, "gain": 0.1}, {"band": 3, "gain": -0.5}],
            "karaoke": {},
            "timescale": {"speed": 1.25},
            "tremolo": null,
            "channelMix": {"leftToRight": 0.5, "rightToLeft": 0.5},
            "lowPass": {"smoothing": 30},
            "pluginFilters": {"echo": {"delay": 1}}
        }))
        .unwrap();
        assert_eq!(filters.volume, Some(1.0));
        assert_eq!(
            filters.equalizer.as_deref(),
            Some(&[EqBand { band: 0, gain: 0.1 }, EqBand { band: 3, gain: -0.25 }][..])
        );
        assert_eq!(filters.karaoke, Some(Karaoke::default()));
        assert_eq!(filters.timescale.unwrap().pitch, 1.0);
        assert!(filters.tremolo.is_none());
        assert_eq!(
            filters.present(),
            vec!["volume", "equalizer", "karaoke", "timescale", "channelMix", "lowPass"]
        );

        let echoed = serde_json::to_value(&filters).unwrap();
        assert_eq!(echoed["karaoke"]["filterBand"], 220.0);
        assert_eq!(echoed["timescale"], json!({"speed": 1.25, "pitch": 1.0, "rate": 1.0}));
        assert_eq!(echoed["channelMix"]["leftToLeft"], 1.0);
        assert_eq!(echoed["pluginFilters"]["echo"]["delay"], 1);
        assert!(echoed.get("tremolo").is_none());
        assert_eq!(serde_json::to_value(Filters::default()).unwrap(), json!({}));
        assert_eq!(
            Filters::parse(&json!({"pluginFilters": null, "equalizer": []})).unwrap(),
            Filters::default()
        );
    }

    #[test]
    fn rejects_out_of_range_values() {
        let cases = [
            (json!([]), "filters must be an object"),
            (json!({"volume": 6}), "filters.volume"),
            (json!({"volume": "loud"}), "Invalid filters"),
            (json!({"equalizer": [{"band": 15, "gain": 0.1}]}), "band"),
            (json!({"timescale": {"speed": 0}}), "speed"),
            (json!({"timescale": {"rate": -1}}), "rate"),
            (json!({"tremolo": {"depth": 1.5}}), "tremolo.depth"),
            (json!({"vibrato": {"frequency": 15}}), "vibrato.frequency"),
            (json!({"channelMix": {"leftToLeft": 2}}), "channelMix"),
            (json!({"karaoke": {"filterWidth": 0}}), "filterWidth"),
        ];
        for (value, expected) in cases {
            let error = Filters::parse(&value).unwrap_err();
            assert!(error.contains(expected), "{value}: {error}");
        }
    }

    #[test]
    fn neutral_filters_keep_audio_bit_exact() {
        let input = sine(440.0, 0.2, 0.5);
        let filters = json!({
            "volume": 1.0, "karaoke": {"level": 0, "monoLevel": 0}, "timescale": {}, "tremolo": {"depth": 0},
            "rotation": {}, "distortion": {}, "channelMix": {}, "lowPass": {"smoothing": 1}, "equalizer": [{"band": 1, "gain": 0}]
        });
        let chain = FilterChain::from_filters(&Filters::parse(&filters).unwrap());
        assert!(chain.is_neutral());
        assert_eq!(run(filters, &input), input);
    }

    #[test]
    fn volume_scales_and_clips() {
        let out = run(json!({"volume": 0.5}), &sine(440.0, 0.1, 0.8));
        assert!((rms(&out) - 0.8 / 2f32.sqrt() * 0.5).abs() < 0.01);
        let loud = run(json!({"volume": 5.0}), &sine(440.0, 0.1, 0.8));
        assert!(loud.iter().all(|s| s.abs() <= 1.0));
    }

    #[test]
    fn equalizer_cuts_and_boosts_a_band() {
        let input = sine(1000.0, 0.5, 0.2);
        let settle = 4800;
        let base = rms(&input[settle..]);
        let cut = run(json!({"equalizer": [{"band": 8, "gain": -0.25}]}), &input);
        assert!(
            rms(&cut[settle..]) < base * 0.2,
            "1 kHz band muted: {}",
            rms(&cut[settle..])
        );
        let doubled = run(json!({"equalizer": [{"band": 8, "gain": 0.25}]}), &input);
        let ratio = rms(&doubled[settle..]) / base;
        assert!((1.8..2.2).contains(&ratio), "gain 0.25 doubles the band: {ratio}");
        let other = run(json!({"equalizer": [{"band": 0, "gain": -0.25}]}), &input);
        assert!(
            (rms(&other[settle..]) / base - 1.0).abs() < 0.1,
            "25 Hz band leaves 1 kHz alone"
        );
    }

    #[test]
    fn timescale_changes_duration_and_pitch() {
        let input = sine(440.0, 2.0, 0.5);
        let frames = |v: &[f32]| v.len() as f32 / 2.0;

        let fast = run(json!({"timescale": {"speed": 1.5}}), &input);
        assert!(
            (frames(&fast) / frames(&input) - 1.0 / 1.5).abs() < 0.03,
            "speed shortens"
        );
        assert!(
            (frequency(&fast) - 440.0).abs() < 15.0,
            "speed keeps pitch: {}",
            frequency(&fast)
        );

        let slow = run(json!({"timescale": {"speed": 0.8}}), &input);
        assert!((frames(&slow) / frames(&input) - 1.25).abs() < 0.03, "slower lengthens");
        assert!(
            (frequency(&slow) - 440.0).abs() < 15.0,
            "slow keeps pitch: {}",
            frequency(&slow)
        );

        let rate = run(json!({"timescale": {"rate": 2.0}}), &input);
        assert!((frames(&rate) / frames(&input) - 0.5).abs() < 0.02, "rate shortens");
        assert!(
            (frequency(&rate) - 880.0).abs() < 20.0,
            "rate raises pitch: {}",
            frequency(&rate)
        );

        let pitch = run(json!({"timescale": {"pitch": 1.5}}), &input);
        assert!(
            (frames(&pitch) / frames(&input) - 1.0).abs() < 0.03,
            "pitch keeps duration"
        );
        assert!(
            (frequency(&pitch) - 660.0).abs() < 20.0,
            "pitch shifts: {}",
            frequency(&pitch)
        );

        let nightcore = Filters::parse(&json!({"timescale": {"speed": 1.2, "pitch": 1.1, "rate": 1.05}})).unwrap();
        assert!((FilterChain::from_filters(&nightcore).time_factor() - 1.26).abs() < 1e-6);
    }

    #[test]
    fn timescale_output_is_click_free() {
        // A clean 440 Hz sine moves at most ~0.029 per sample at this amplitude.
        let input = sine(440.0, 1.0, 0.5);
        for filters in [
            json!({"timescale": {"speed": 1.5}}),
            json!({"timescale": {"speed": 0.75}}),
            json!({"timescale": {"rate": 1.3}}),
            json!({"timescale": {"speed": 1.2, "pitch": 1.2, "rate": 1.0}}),
        ] {
            let out = run(filters.clone(), &input);
            let left: Vec<f32> = out.iter().step_by(2).copied().collect();
            let jump = left[100..left.len() - 100]
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0, f32::max);
            assert!(jump < 0.06, "{filters}: max step {jump}");
        }
    }

    #[test]
    fn modulation_filters_stay_bounded() {
        let input = sine(440.0, 1.0, 0.9);
        for filters in [
            json!({"tremolo": {"frequency": 4, "depth": 1}}),
            json!({"vibrato": {"frequency": 14, "depth": 1}}),
            json!({"rotation": {"rotationHz": 0.2}}),
            json!({"distortion": {"sinScale": 2, "tanOffset": 0.5, "tanScale": 3, "scale": 1.5, "offset": 0.1}}),
            json!({"karaoke": {"level": 1, "monoLevel": 1}}),
            json!({"lowPass": {"smoothing": 20}}),
        ] {
            let out = run(filters.clone(), &input);
            assert_eq!(out.len(), input.len(), "{filters}");
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "{filters}");
            assert!(rms(&out) > 0.01, "{filters} keeps some signal");
        }
    }

    #[test]
    fn tremolo_modulates_amplitude() {
        let out = run(json!({"tremolo": {"frequency": 2, "depth": 1}}), &sine(440.0, 1.0, 0.5));
        let window = |i: usize| rms(&out[i * 2400..(i + 1) * 2400]);
        let levels: Vec<f32> = (0..20).map(window).collect();
        let max = levels.iter().cloned().fold(0.0, f32::max);
        let min = levels.iter().cloned().fold(1.0, f32::min);
        assert!(max > 0.3 && min < 0.05, "levels {levels:?}");
    }

    #[test]
    fn channel_mix_and_rotation_move_audio_between_channels() {
        let mut input = sine(440.0, 0.5, 0.5);
        for frame in input.chunks_exact_mut(2) {
            frame[1] = 0.0;
        }
        let swapped = run(
            json!({"channelMix": {"leftToLeft": 0, "leftToRight": 1, "rightToLeft": 1, "rightToRight": 0}}),
            &input,
        );
        assert!(swapped.chunks_exact(2).all(|f| f[0] == 0.0));
        assert!((rms(&swapped) - rms(&input)).abs() < 1e-4);

        let rotated = run(json!({"rotation": {"rotationHz": 1}}), &sine(440.0, 1.0, 0.5));
        let left = |range: std::ops::Range<usize>| rms(&rotated[range].iter().step_by(2).copied().collect::<Vec<_>>());
        // A quarter turn (sin = 1) pans fully left, three quarters pans fully right.
        assert!(left(23_000..25_000) > 0.3);
        assert!(left(71_000..73_000) < 0.05);
    }

    #[test]
    fn karaoke_removes_centre_and_low_pass_smooths() {
        let input = sine(2000.0, 0.5, 0.5);
        let karaoke = run(json!({"karaoke": {"level": 1, "monoLevel": 0}}), &input);
        assert!(rms(&karaoke) < 1e-4, "centre-panned audio removed");

        let smoothed = run(json!({"lowPass": {"smoothing": 20}}), &sine(8000.0, 0.5, 0.5));
        assert!(rms(&smoothed) < rms(&sine(8000.0, 0.5, 0.5)) * 0.2);
        let bass = run(json!({"lowPass": {"smoothing": 20}}), &sine(50.0, 0.5, 0.5));
        assert!(rms(&bass[4800..]) > 0.3);
    }

    #[test]
    fn switching_timescale_off_keeps_buffered_audio() {
        let input = sine(440.0, 1.0, 0.5);
        let mut chain = FilterChain::from_filters(&Filters::parse(&json!({"timescale": {"speed": 1.5}})).unwrap());
        let mut out = Vec::new();
        chain.process(&input[..48_000], &mut out);
        assert!(chain.has_latency());
        chain.configure(&Filters::default());
        assert!(!chain.is_neutral(), "buffered tail still pending");
        chain.process(&input[48_000..], &mut out);
        assert!(chain.is_neutral());
        assert!(!chain.has_latency());
        let frames = out.len() / 2;
        let expected = 24_000.0 / 1.5 + 24_000.0;
        assert!((frames as f32 - expected).abs() < 3_000.0, "{frames} vs {expected}");
    }
}
