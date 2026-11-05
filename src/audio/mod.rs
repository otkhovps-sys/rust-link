//! Audio pipeline: decode (symphonia) -> resample (rubato) -> filters -> Opus encode (libopus)
//! -> RTP + AEAD (+ DAVE E2EE) -> Discord voice UDP.

pub mod crypto;
pub mod dave;
pub mod decoder;
pub mod encoder;
pub mod filters;
pub mod playback;
pub mod voice_gateway;

use std::sync::atomic::{AtomicU64, Ordering};

/// Node-wide frame counters, reset at every stats interval.
#[derive(Debug, Default)]
pub struct FrameCounters {
    pub sent: AtomicU64,
    pub nulled: AtomicU64,
}

impl FrameCounters {
    pub fn take(&self) -> (u64, u64) {
        (
            self.sent.swap(0, Ordering::Relaxed),
            self.nulled.swap(0, Ordering::Relaxed),
        )
    }
}
