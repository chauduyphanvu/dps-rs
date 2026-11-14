//! Headphone crossfeed, after the stereo block, for two channels only: feeds
//! each ear the other channel's lows at a set level, as each ear would hear
//! both speakers, so hard-panned sound is less tiring on headphones.
//!
//! Not implemented yet: passes audio through unchanged.

use crate::profile::CrossfeedParams;

pub struct Crossfeed;

impl Crossfeed {
    pub fn new(_params: &CrossfeedParams, _sample_rate: f64) -> Self {
        Self
    }

    /// One frame: left and right in, left and right out.
    pub fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        (l, r)
    }

    /// Starts where `from` is. No allocation.
    pub fn adopt(&mut self, _from: &Self) {}
}
