//! Output device correction, after crossfeed and before the limiter: a
//! preamp and up to `MAX_CORRECTION_BANDS` peaking and shelving filters
//! measured for the output device, such as an imported headphone profile.
//! Bands at or above Nyquist are skipped, like EQ bands.
//!
//! Not implemented yet: passes audio through unchanged.

use crate::profile::CorrectionParams;

pub struct Correction;

impl Correction {
    pub fn new(_params: &CorrectionParams, _sample_rate: f64, _channels: usize) -> Self {
        Self
    }

    /// One interleaved frame, all channels, in place. No allocation.
    pub fn frame(&mut self, _frame: &mut [f32]) {}

    /// Starts where `from` is. No allocation.
    pub fn adopt(&mut self, _from: &Self) {}
}
