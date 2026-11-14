//! Leveler, ahead of the rest of the chain: a slow gain that steers the
//! input's loudness towards a target, within a set boost and cut and at set
//! rates up and down, so quiet and loud sources reach the chain at about the
//! same level. One gain for all channels, so the stereo image does not shift.
//!
//! Not implemented yet: passes audio through unchanged.

use crate::profile::LevelerParams;

pub struct Leveler;

impl Leveler {
    pub fn new(_params: &LevelerParams, _sample_rate: f64, _channels: usize) -> Self {
        Self
    }

    /// One interleaved frame, all channels, in place. No allocation.
    pub fn frame(&mut self, _frame: &mut [f32]) {}

    /// The gain being applied now, in dB.
    pub fn gain_db(&self) -> f32 {
        0.0
    }

    /// Starts where `from` is, so a chain that takes over does not start
    /// levelling from scratch. No allocation.
    pub fn adopt(&mut self, _from: &Self) {}
}
