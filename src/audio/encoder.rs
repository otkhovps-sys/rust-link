//! 20 ms Opus framing for Discord (48 kHz, stereo, 960 samples per channel).

use audiopus::{coder::Encoder, Application, Bitrate, Channels, SampleRate};

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_MS: u64 = 20;
pub const SAMPLES_PER_FRAME: usize = 960;
/// Interleaved stereo samples per frame.
pub const FRAME_LEN: usize = SAMPLES_PER_FRAME * 2;
/// Opus silence frame recommended by Discord after audio stops.
pub const SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];
const MAX_PACKET: usize = 4000;

pub struct OpusEncoder {
    inner: Encoder,
    out: Vec<u8>,
}

impl OpusEncoder {
    pub fn new(bitrate: i32, complexity: u8) -> anyhow::Result<Self> {
        let mut inner = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
            .map_err(|e| anyhow::anyhow!("opus encoder init: {e}"))?;
        inner
            .set_bitrate(Bitrate::BitsPerSecond(bitrate.clamp(8_000, 512_000)))
            .map_err(|e| anyhow::anyhow!("opus bitrate: {e}"))?;
        inner
            .set_complexity(complexity.min(10))
            .map_err(|e| anyhow::anyhow!("opus complexity: {e}"))?;
        inner
            .set_inband_fec(true)
            .map_err(|e| anyhow::anyhow!("opus fec: {e}"))?;
        inner
            .set_packet_loss_perc(5)
            .map_err(|e| anyhow::anyhow!("opus packet loss: {e}"))?;
        Ok(Self {
            inner,
            out: vec![0; MAX_PACKET],
        })
    }

    /// Encode exactly one 20 ms frame of interleaved stereo f32 PCM.
    pub fn encode(&mut self, pcm: &[f32]) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(pcm.len() == FRAME_LEN, "opus frame must contain {FRAME_LEN} samples");
        let n = self
            .inner
            .encode_float(pcm, &mut self.out)
            .map_err(|e| anyhow::anyhow!("opus encode: {e}"))?;
        Ok(self.out[..n].to_vec())
    }
}

/// Apply a linear gain with hard clipping; a neutral gain is a no-op.
pub fn apply_gain(pcm: &mut [f32], gain: f32) {
    if (gain - 1.0).abs() < 1e-4 {
        return;
    }
    for sample in pcm {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

/// Collects interleaved PCM and hands out fixed 20 ms frames.
#[derive(Default)]
pub struct FramePacker {
    buffer: Vec<f32>,
}

impl FramePacker {
    pub fn push(&mut self, pcm: &[f32]) {
        self.buffer.extend_from_slice(pcm);
    }

    pub fn take_frame(&mut self) -> Option<Vec<f32>> {
        if self.buffer.len() < FRAME_LEN {
            return None;
        }
        let rest = self.buffer.split_off(FRAME_LEN);
        Some(std::mem::replace(&mut self.buffer, rest))
    }

    /// Final partial frame padded with silence.
    pub fn take_padded(&mut self) -> Option<Vec<f32>> {
        if self.buffer.is_empty() {
            return None;
        }
        let mut frame = std::mem::take(&mut self.buffer);
        frame.resize(FRAME_LEN, 0.0);
        Some(frame)
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packer_produces_fixed_frames() {
        let mut packer = FramePacker::default();
        packer.push(&vec![0.1; FRAME_LEN + 10]);
        assert_eq!(packer.take_frame().unwrap().len(), FRAME_LEN);
        assert!(packer.take_frame().is_none());
        let last = packer.take_padded().unwrap();
        assert_eq!(last.len(), FRAME_LEN);
        assert_eq!(last[9], 0.1);
        assert_eq!(last[10], 0.0);
        assert!(packer.is_empty());
    }

    #[test]
    fn encodes_a_sine_frame() {
        let mut enc = OpusEncoder::new(128_000, 10).unwrap();
        let pcm: Vec<f32> = (0..FRAME_LEN)
            .map(|i| ((i / 2) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
            .collect();
        let packet = enc.encode(&pcm).unwrap();
        assert!(!packet.is_empty() && packet.len() < 1500);
        let samples = audiopus::packet::nb_samples(
            audiopus::packet::Packet::try_from(&packet[..]).unwrap(),
            SampleRate::Hz48000,
        )
        .unwrap();
        assert_eq!(samples, SAMPLES_PER_FRAME);
    }

    #[test]
    fn gain_clips() {
        let mut pcm = vec![0.6, -0.6];
        apply_gain(&mut pcm, 2.0);
        assert_eq!(pcm, vec![1.0, -1.0]);
    }
}
