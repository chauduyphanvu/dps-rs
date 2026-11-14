//! Headphone crossfeed, after the stereo block, for two channels only: feeds
//! each ear the other channel's lows at a set level, as each ear would hear
//! both speakers, so hard-panned sound is less tiring on headphones.
//!
//! Each output is its own channel minus `k` of its own lows plus `k` of the
//! other channel's lows, with `k = c / (1 + c)` for the level `c`. A
//! low-frequency sound panned hard to one side thus turns up in the other ear
//! at `c` times its own level. The two corrections are equal and opposite, so
//! the mid signal (L + R) is untouched and mono content (L = R) passes
//! exactly, as the difference of the lows is then exactly 0. The one-pole
//! low-pass's phase lag supplies a small interaural delay.

use crate::profile::CrossfeedParams;
use std::f64::consts::PI;

pub struct Crossfeed {
    k: f64,
    a: f64,
    lp_l: f64,
    lp_r: f64,
}

impl Crossfeed {
    pub fn new(params: &CrossfeedParams, sample_rate: f64) -> Self {
        let c = 10f64.powf(f64::from(params.level_db) / 20.0);
        Self {
            k: c / (1.0 + c),
            a: 1.0 - (-2.0 * PI * f64::from(params.cutoff_hz) / sample_rate).exp(),
            lp_l: 0.0,
            lp_r: 0.0,
        }
    }

    /// One frame: left and right in, left and right out.
    pub fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        let (l, r) = (f64::from(l), f64::from(r));
        self.lp_l += self.a * (l - self.lp_l);
        self.lp_r += self.a * (r - self.lp_r);
        let d = self.k * (self.lp_r - self.lp_l);
        ((l + d) as f32, (r - d) as f32)
    }

    /// Starts where `from` is. No allocation.
    pub fn adopt(&mut self, from: &Self) {
        self.lp_l = from.lp_l;
        self.lp_r = from.lp_r;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stereo::tests::amplitude;

    const FS: f64 = 48_000.0;

    /// Deterministic values in [-1, 1).
    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn defaults() -> CrossfeedParams {
        CrossfeedParams {
            enabled: true,
            ..CrossfeedParams::default()
        }
    }

    fn level(level_db: f32) -> f64 {
        10f64.powf(f64::from(level_db) / 20.0)
    }

    fn db(ratio: f64) -> f64 {
        20.0 * ratio.log10()
    }

    /// Right over left amplitude for a sine of `freq_hz` in the left channel
    /// only, once the low-passes have settled. Needs `freq_hz` a multiple of
    /// 10 Hz, so the 0.1 s measured holds whole cycles.
    fn right_over_left(params: &CrossfeedParams, fs: f64, freq_hz: f64) -> f64 {
        let mut cf = Crossfeed::new(params, fs);
        let n = (fs / 10.0) as usize;
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for i in 0..2 * n {
            let x = (0.5 * (2.0 * PI * freq_hz * i as f64 / fs).sin()) as f32;
            let (l, r) = cf.tick(x, 0.0);
            left.push(l);
            right.push(r);
        }
        amplitude(&right[n..], freq_hz, fs) / amplitude(&left[n..], freq_hz, fs)
    }

    #[test]
    fn mono_passes_bit_for_bit() {
        let x = noise(20_000, 1);
        let mut cf = Crossfeed::new(&defaults(), FS);
        for v in x {
            assert_eq!(cf.tick(v, v), (v, v));
        }
    }

    #[test]
    fn a_hard_panned_bass_note_reaches_the_other_ear_at_the_set_level() {
        let got = db(right_over_left(&defaults(), FS, 50.0));
        let want = db(level(-4.5));
        assert!((got - want).abs() < 0.2, "{got} dB, want {want} dB");
    }

    #[test]
    fn highs_stay_in_their_own_ear() {
        let got = db(right_over_left(&defaults(), FS, 10_000.0));
        assert!(got < -20.0, "{got} dB");
    }

    #[test]
    fn the_mid_signal_is_untouched() {
        let (l, r) = (noise(20_000, 2), noise(20_000, 3));
        let mut cf = Crossfeed::new(&defaults(), FS);
        for (l, r) in l.into_iter().zip(r) {
            let (out_l, out_r) = cf.tick(l, r);
            let err = (f64::from(out_l) + f64::from(out_r)) - (f64::from(l) + f64::from(r));
            assert!(err.abs() < 1e-6, "{err}");
        }
    }

    #[test]
    fn a_lower_level_crossfeeds_less() {
        let mut ratios = Vec::new();
        for level_db in [-1.0, -24.0] {
            let params = CrossfeedParams {
                level_db,
                ..defaults()
            };
            let got = db(right_over_left(&params, FS, 50.0));
            let want = db(level(level_db));
            assert!(
                (got - want).abs() < 0.2,
                "{level_db} dB: {got} dB, want {want} dB"
            );
            ratios.push(got);
        }
        assert!(ratios[1] < ratios[0], "{ratios:?}");
    }

    #[test]
    fn adopt_continues_where_the_other_left_off() {
        let (l, r) = (noise(8_000, 4), noise(8_000, 5));
        let mut a = Crossfeed::new(&defaults(), FS);
        for i in 0..4_000 {
            a.tick(l[i], r[i]);
        }
        let mut b = Crossfeed::new(&defaults(), FS);
        b.adopt(&a);
        for i in 4_000..8_000 {
            assert_eq!(a.tick(l[i], r[i]), b.tick(l[i], r[i]), "frame {i}");
        }
    }

    #[test]
    fn works_at_other_sample_rates() {
        for fs in [44_100.0, 192_000.0] {
            let got = db(right_over_left(&defaults(), fs, 50.0));
            let want = db(level(-4.5));
            assert!(
                (got - want).abs() < 0.3,
                "{fs} Hz: {got} dB, want {want} dB"
            );
            let (l, r) = (noise(4_000, 6), noise(4_000, 7));
            let mut cf = Crossfeed::new(&defaults(), fs);
            for (l, r) in l.into_iter().zip(r) {
                let (out_l, out_r) = cf.tick(l, r);
                assert!(out_l.is_finite() && out_r.is_finite(), "{fs} Hz");
            }
        }
    }
}
