//! Feed-forward hard-knee compressor (spec section 4): a rectified one-pole
//! level detector and the gain law G_R = (L / K_T)^((1 - r) / r).

use crate::profile::CompressorParams;

/// One-pole smoothing coefficient for time constant `ms`. Spec 4.5 / gap
/// table 13: the printed exp(+1/(Fs T)) is missing its minus sign; the
/// printed factor 5 on the release is dropped.
fn smoothing_coef(ms: f32, sample_rate: f64) -> f32 {
    (-1.0 / (sample_rate * f64::from(ms) / 1000.0)).exp() as f32
}

pub struct Compressor {
    attack: f32,
    release: f32,
    inv_threshold: f32,
    exponent: f32,
    level: f32,
}

impl Compressor {
    pub fn new(params: &CompressorParams, sample_rate: f64) -> Self {
        let ratio = f64::from(params.ratio);
        Self {
            attack: smoothing_coef(params.attack_ms, sample_rate),
            release: smoothing_coef(params.release_ms, sample_rate),
            inv_threshold: 10f64.powf(-f64::from(params.threshold_db) / 20.0) as f32,
            exponent: ((1.0 - ratio) / ratio) as f32,
            level: 0.0,
        }
    }

    /// Feeds `detector` to the level detector and returns the gain to apply.
    /// With ratio 1 the exponent is 0 and the gain is exactly 1.
    pub fn gain(&mut self, detector: f32) -> f32 {
        let x = detector.abs();
        let coef = if x >= self.level {
            self.attack
        } else {
            self.release
        };
        self.level = coef * (self.level - x) + x;
        let excess = self.level * self.inv_threshold;
        if excess > 1.0 {
            excess.powf(self.exponent)
        } else {
            1.0
        }
    }

    pub fn tick(&mut self, x: f32) -> f32 {
        x * self.gain(x)
    }

    /// Starts the level detector where `from`'s is, so a compressor that
    /// takes over from a running one does not start from silence.
    pub fn adopt_level(&mut self, from: &Self) {
        self.level = from.level;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f64::consts::PI;

    const FS: f64 = 48_000.0;

    fn params(threshold_db: f32, ratio: f32, attack_ms: f32, release_ms: f32) -> CompressorParams {
        CompressorParams {
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
        }
    }

    /// An f64 model of spec 4.2-4.3, written from the text rather than the
    /// code: the level follows |x| through a one-pole smoother with time
    /// constant T (coefficient e^(-1/(fs T))), the attack while |x| is above
    /// the level and the release otherwise, and each sample's gain comes from
    /// the level that already includes it.
    pub(crate) struct Reference {
        attack: f64,
        release: f64,
        threshold: f64,
        ratio: f64,
        level: f64,
    }

    impl Reference {
        pub(crate) fn new(p: &CompressorParams, sample_rate: f64) -> Self {
            let coef = |ms: f32| (-1000.0 / (f64::from(ms) * sample_rate)).exp();
            Self {
                attack: coef(p.attack_ms),
                release: coef(p.release_ms),
                threshold: 10f64.powf(f64::from(p.threshold_db) / 20.0),
                ratio: f64::from(p.ratio),
                level: 0.0,
            }
        }

        pub(crate) fn gain(&mut self, x: f64) -> f64 {
            let x = x.abs();
            let c = if x > self.level {
                self.attack
            } else {
                self.release
            };
            self.level = c * self.level + (1.0 - c) * x;
            if self.level > self.threshold {
                (self.level / self.threshold).powf(1.0 / self.ratio - 1.0)
            } else {
                1.0
            }
        }
    }

    #[test]
    fn matches_reference_model_on_a_stepped_sine() {
        // A 1 kHz sine below the -20 dB threshold, then 18 dB above it
        // (attack), then far below it again (release). Every sample is
        // compared, so a one-sample detector lag or swapped time constants
        // show up.
        let p = params(-20.0, 4.0, 2.0, 30.0);
        let mut c = Compressor::new(&p, FS);
        let mut r = Reference::new(&p, FS);
        for n in 0..14_400 {
            let amp = match n {
                0..2400 => 0.05,
                2400..7200 => 0.8,
                _ => 0.02,
            };
            let x = (amp * (2.0 * PI * 1000.0 * n as f64 / FS).sin()) as f32;
            let want = f64::from(x) * r.gain(f64::from(x));
            let got = f64::from(c.tick(x));
            assert!(
                (got - want).abs() < 1e-6,
                "sample {n}: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn smoothing_constant_matches_spec() {
        assert!((f64::from(smoothing_coef(10.0, FS)) - 0.99792).abs() < 1e-5);
    }

    #[test]
    fn detector_time_constants() {
        // A unit step reaches 1 - 1/e after the attack time, and falls to 1/e
        // of its level after the release time (no factor 5).
        let mut c = Compressor::new(&params(0.0, 4.0, 10.0, 100.0), FS);
        for _ in 0..480 {
            c.gain(1.0);
        }
        assert!(
            (f64::from(c.level) - (1.0 - (-1f64).exp())).abs() < 1e-4,
            "{}",
            c.level
        );
        for _ in 0..48_000 {
            c.gain(1.0);
        }
        for _ in 0..4800 {
            c.gain(0.0);
        }
        assert!(
            (f64::from(c.level) - (-1f64).exp()).abs() < 1e-4,
            "{}",
            c.level
        );
    }

    #[test]
    fn static_curve_matches_spec() {
        // K_T = 0.1 (-20 dB), r = 4. A constant input settles the level at |x|.
        for (x, gr) in [
            (0.05f32, 1.0),
            (0.10, 1.0),
            (0.20, 0.59460),
            (0.40, 0.35355),
            (0.80, 0.21022),
        ] {
            let mut c = Compressor::new(&params(-20.0, 4.0, 10.0, 100.0), FS);
            let mut out = 0.0;
            for _ in 0..48_000 {
                out = c.tick(-x);
            }
            let want = -f64::from(x) * gr;
            assert!(
                (f64::from(out) - want).abs() < 2e-5,
                "x = {x}: out {out}, want {want}"
            );
        }
    }

    #[test]
    fn ratio_one_is_exact_passthrough() {
        let mut c = Compressor::new(&params(-60.0, 1.0, 0.01, 0.01), FS);
        for i in 0..10_000 {
            let x = ((i as f32) * 0.37).sin() * 0.9;
            assert_eq!(c.tick(x), x);
        }
    }
}
