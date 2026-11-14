//! Bass harmonics, after the per-channel chain: adds harmonics of the
//! fundamentals below a cutoff, which a small speaker cannot play, so the
//! ear still hears their pitch; optionally high-passes what the speaker
//! cannot play.
//!
//! Not implemented yet: passes audio through unchanged.

use crate::profile::BassParams;

pub struct Bass;

impl Bass {
    pub fn new(_params: &BassParams, _sample_rate: f64, _channels: usize) -> Self {
        Self
    }

    /// One interleaved frame, all channels, in place. No allocation.
    pub fn frame(&mut self, _frame: &mut [f32]) {}

    /// Starts where `from` is. No allocation.
    pub fn adopt(&mut self, _from: &Self) {}
}
