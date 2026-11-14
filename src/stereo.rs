//! Stereo field enhancement (US 9,398,394; docs/patent-spec-later.md section
//! 7): above a crossover, the side signal gets a makeup boost that an AGC,
//! driven by a tilted copy of the mid signal, pulls back towards 0 dB.

use crate::compressor::Compressor;
use crate::profile::{CompressorParams, StereoParams};
use crate::shelf::{Shelf, allpass_coef};

/// First-order low-pass 0.5 (1 - A(z)) on the shelves' allpass, so the
/// complementary high band x - low is the matching first-order high-pass.
struct Lowpass {
    a: f32,
    a2m1: f32,
    s: f32,
}

impl Lowpass {
    fn new(corner_hz: f64, sample_rate: f64) -> Self {
        let a = allpass_coef(corner_hz, sample_rate);
        Self {
            a: a as f32,
            a2m1: (a * a - 1.0) as f32,
            s: 0.0,
        }
    }

    fn tick(&mut self, x: f32) -> f32 {
        let ap = self.a * x + self.a2m1 * self.s;
        self.s = self.a * self.s + x;
        0.5 * (x - ap)
    }
}

pub struct StereoEnhancer {
    low_l: Lowpass,
    low_r: Lowpass,
    detector_low: Shelf,
    detector_high: Shelf,
    agc: Compressor,
    makeup: f32,
    min_gain: f32,
}

impl StereoEnhancer {
    pub fn new(params: &StereoParams, shelf_corner_hz: f64, sample_rate: f64) -> Self {
        let crossover = f64::from(params.crossover_hz);
        let boost = f64::from(params.side_boost_db);
        let agc = CompressorParams {
            threshold_db: params.threshold_db,
            ratio: params.ratio,
            attack_ms: params.attack_ms,
            release_ms: params.release_ms,
        };
        Self {
            low_l: Lowpass::new(crossover, sample_rate),
            low_r: Lowpass::new(crossover, sample_rate),
            // FIG. 5 of the patent: low shelf -12 dB, high shelf +12 dB.
            detector_low: Shelf::low(-12.0, shelf_corner_hz, sample_rate),
            detector_high: Shelf::high(12.0, shelf_corner_hz, sample_rate),
            agc: Compressor::new(&agc, sample_rate),
            makeup: 10f64.powf(boost / 20.0) as f32,
            min_gain: 10f64.powf(-boost / 20.0) as f32,
        }
    }

    pub fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        let low_l = self.low_l.tick(l);
        let low_r = self.low_r.tick(r);
        let (high_l, high_r) = (l - low_l, r - low_r);
        let mid = high_l + high_r;
        let side = high_r - high_l;
        let detect = self.detector_high.tick(self.detector_low.tick(mid));
        let gain = self.agc.gain(detect).max(self.min_gain);
        let side = side * (self.makeup * gain);
        (low_l + 0.5 * (mid - side), low_r + 0.5 * (mid + side))
    }

    /// Starts the AGC's level detector where `from`'s is.
    pub fn adopt_level(&mut self, from: &Self) {
        self.agc.adopt_level(&from.agc);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::compressor::tests::Reference as ReferenceAgc;
    use std::f64::consts::PI;

    const FS: f64 = 48_000.0;

    /// y = b0 x + b1 x[n-1] - a1 y[n-1] in f64: the analog first-order
    /// (n1 s + n0) / (d1 s + d0) through s = (1 - z^-1) / (1 + z^-1), with
    /// corners prewarped to W0 = tan(pi fc / fs) by the caller.
    struct Bilinear {
        b0: f64,
        b1: f64,
        a1: f64,
        x1: f64,
        y1: f64,
    }

    impl Bilinear {
        fn new(n1: f64, n0: f64, d1: f64, d0: f64) -> Self {
            let d = d1 + d0;
            Self {
                b0: (n1 + n0) / d,
                b1: (n0 - n1) / d,
                a1: (d0 - d1) / d,
                x1: 0.0,
                y1: 0.0,
            }
        }

        fn tick(&mut self, x: f64) -> f64 {
            let y = self.b0 * x + self.b1 * self.x1 - self.a1 * self.y1;
            self.x1 = x;
            self.y1 = y;
            y
        }
    }

    /// The stereo block built from docs/patent-spec-later.md 7.1-7.4 rather
    /// than the code: a first-order crossover, M = R + L and S = R - L on the
    /// upper band, a detector of -12 dB low shelf then +12 dB high shelf on
    /// M, an AGC on S whose reduction stops at the makeup gain, and the decode
    /// L = low + (M - S') / 2, R = low + (M + S') / 2.
    struct ReferenceStereo {
        low_l: Bilinear,
        low_r: Bilinear,
        detect_low: Bilinear,
        detect_high: Bilinear,
        agc: ReferenceAgc,
        makeup: f64,
    }

    impl ReferenceStereo {
        fn new(p: &StereoParams, shelf_corner_hz: f64) -> Self {
            let wx = (PI * f64::from(p.crossover_hz) / FS).tan();
            let wc = (PI * shelf_corner_hz / FS).tan();
            let g12 = 10f64.powf(-12.0 / 20.0);
            let agc = CompressorParams {
                threshold_db: p.threshold_db,
                ratio: p.ratio,
                attack_ms: p.attack_ms,
                release_ms: p.release_ms,
            };
            Self {
                low_l: Bilinear::new(0.0, wx, 1.0, wx),
                low_r: Bilinear::new(0.0, wx, 1.0, wx),
                // Low cut (s + G Wc) / (s + Wc); high boost is the inverse of
                // the high cut (G s + Wc) / (s + Wc).
                detect_low: Bilinear::new(1.0, g12 * wc, 1.0, wc),
                detect_high: Bilinear::new(1.0, wc, g12, wc),
                agc: ReferenceAgc::new(&agc, FS),
                makeup: 10f64.powf(f64::from(p.side_boost_db) / 20.0),
            }
        }

        fn tick(&mut self, l: f64, r: f64) -> (f64, f64) {
            let (low_l, low_r) = (self.low_l.tick(l), self.low_r.tick(r));
            let mid = (r - low_r) + (l - low_l);
            let side = (r - low_r) - (l - low_l);
            let detect = self.detect_high.tick(self.detect_low.tick(mid));
            let gain = self.agc.gain(detect).max(1.0 / self.makeup);
            let side = side * self.makeup * gain;
            (low_l + (mid - side) / 2.0, low_r + (mid + side) / 2.0)
        }
    }

    #[test]
    fn matches_reference_model_on_stepped_input() {
        // A quiet mid, a loud 3 kHz mid (attack), quiet again for longer than
        // the 200 ms release, then a 300 Hz mid that sits between crossover
        // and shelf corner, where the detector's low shelf sets the level.
        // The side carries a steady 5 kHz tone throughout.
        let params = StereoParams {
            enabled: true,
            ..StereoParams::default()
        };
        let mut st = StereoEnhancer::new(&params, 1000.0, FS);
        let mut reference = ReferenceStereo::new(&params, 1000.0);
        for n in 0..72_000 {
            let t = n as f64 / FS;
            let m = match n {
                0..9_600 => 0.01 * (2.0 * PI * 3000.0 * t).sin(),
                9_600..19_200 => 0.3 * (2.0 * PI * 3000.0 * t).sin(),
                19_200..43_200 => 0.01 * (2.0 * PI * 3000.0 * t).sin(),
                43_200..62_400 => 0.2 * (2.0 * PI * 300.0 * t).sin(),
                _ => 0.0,
            };
            let s = 0.05 * (2.0 * PI * 5000.0 * t).sin();
            let (l, r) = ((m - s) as f32, (m + s) as f32);
            let (got_l, got_r) = st.tick(l, r);
            let (want_l, want_r) = reference.tick(f64::from(l), f64::from(r));
            let err = (f64::from(got_l) - want_l)
                .abs()
                .max((f64::from(got_r) - want_r).abs());
            // The code's f32 crossover and AGC level drift up to about 2e-5
            // from the f64 model on this input.
            assert!(
                err < 1e-4,
                "sample {n}: got ({got_l}, {got_r}), want ({want_l}, {want_r})"
            );
        }
    }

    /// Amplitude of the `freq_hz` component of `x`, which must hold a whole
    /// number of its cycles.
    pub(crate) fn amplitude(x: &[f32], freq_hz: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * PI * freq_hz / sample_rate;
        let (mut re, mut im) = (0.0, 0.0);
        for (n, v) in x.iter().enumerate() {
            re += f64::from(*v) * (w * n as f64).cos();
            im += f64::from(*v) * (w * n as f64).sin();
        }
        2.0 * (re * re + im * im).sqrt() / x.len() as f64
    }

    #[test]
    fn crossover_bands_are_first_order_and_reconstruct() {
        // Bilinear first-order low-pass with -3 dB at the corner.
        let corner = 200.0;
        let t = (PI * corner / FS).tan();
        for f in [50.0, 200.0, 1000.0, 6000.0] {
            let mut lp = Lowpass::new(corner, FS);
            let x: Vec<f32> = (0..96_000)
                .map(|n| (2.0 * PI * f * n as f64 / FS).sin() as f32)
                .collect();
            let low: Vec<f32> = x.iter().map(|v| lp.tick(*v)).collect();
            let high: Vec<f32> = x.iter().zip(&low).map(|(v, l)| v - l).collect();
            let tw = (PI * f / FS).tan();
            let want_low = t / (t * t + tw * tw).sqrt();
            let want_high = tw / (t * t + tw * tw).sqrt();
            assert!(
                (amplitude(&low[48_000..], f, FS) - want_low).abs() < 1e-4,
                "{f} Hz low"
            );
            assert!(
                (amplitude(&high[48_000..], f, FS) - want_high).abs() < 1e-4,
                "{f} Hz high"
            );
        }
    }

    #[test]
    fn loud_mid_pulls_side_gain_down_to_unity_and_no_further() {
        // A loud high-frequency mid drives the AGC far past the 8 dB cap, so
        // the made-up side returns to its original level.
        let params = StereoParams {
            enabled: true,
            ..StereoParams::default()
        };
        let mut st = StereoEnhancer::new(&params, 1000.0, FS);
        let (mid_f, side_f) = (3000.0, 7000.0);
        let mut side_out = Vec::new();
        for n in 0..96_000 {
            let ph = 2.0 * PI * n as f64 / FS;
            let m = 0.4 * (mid_f * ph).sin();
            let s = 0.01 * (side_f * ph).sin();
            let (l, r) = st.tick((m - s) as f32, (m + s) as f32);
            side_out.push(0.5 * (r - l));
        }
        let got = amplitude(&side_out[48_000..], side_f, FS);
        assert!(
            (20.0 * (got / 0.01).log10()).abs() < 0.01,
            "side gain {got}"
        );
    }
}
