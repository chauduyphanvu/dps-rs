//! Loudness contour, after bass harmonics: more bass and treble as the system
//! volume goes down, for the ear's loss of both at low levels. Live only:
//! without a known volume it adds nothing.
//!
//! Not implemented yet: passes audio through unchanged.

use crate::profile::ContourParams;

pub struct Contour;

impl Contour {
    pub fn new(_params: &ContourParams, _sample_rate: f64, _channels: usize) -> Self {
        Self
    }

    /// One interleaved frame, all channels, in place. No allocation.
    pub fn frame(&mut self, _frame: &mut [f32]) {}

    /// The system volume in dB below maximum; `None` when unknown (files,
    /// devices without a volume control), which adds nothing. No allocation.
    pub fn set_volume_db(&mut self, _volume_db: Option<f32>) {}

    /// Starts where `from` is. No allocation.
    pub fn adopt(&mut self, _from: &Self) {}
}
