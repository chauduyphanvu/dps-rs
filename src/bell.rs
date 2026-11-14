//! Mitra-Regalia second-order bell (spec section 5): a fixed allpass whose
//! output is mixed with the input, so the gain is the only run-time value.
//!
//! Coefficients and state are f64. The allpass poles sit about w0^2 inside
//! the stability limit (w0 = 2 pi fc / fs), which f32 cannot resolve for low
//! bands at high sample rates: a 5 Hz band at 192 kHz goes unstable.

use crate::biquad::Coefs;

pub struct Bell {
    b0: f64,
    b1: f64,
    dry: f64,
    wet: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Bell {
    /// `gain_db` at `freq_hz`, 0 dB at DC and Nyquist. `freq_hz / q` (the
    /// bandwidth) must be below `sample_rate / 2` for the allpass to be stable.
    pub fn new(freq_hz: f64, q: f64, gain_db: f64, sample_rate: f64) -> Self {
        use std::f64::consts::PI;
        // Spec 5.3(a) / gap table 7: the printed k1 and k2 definitions are
        // swapped; these are the ones that give a resonant allpass.
        let k1 = -(2.0 * PI * freq_hz / sample_rate).cos();
        let t = (PI * freq_hz / (q * sample_rate)).tan();
        let k2 = (1.0 - t) / (1.0 + t);
        let g = 10f64.powf(gain_db / 20.0);
        Self {
            b0: k2,
            b1: k1 * (1.0 + k2),
            // Spec 5.3(b) / gap table 8: (1+G) on the direct path as in the
            // patent's code, not on the allpass as in its printed H(z), so
            // the phase at fc is 0 rather than 180 degrees.
            dry: 0.5 * (1.0 + g),
            wet: 0.5 * (1.0 - g),
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    pub fn tick(&mut self, x: f32) -> f32 {
        let x = f64::from(x);
        let ap = self.b0 * (x - self.y2) + self.b1 * (self.x1 - self.y1) + self.x2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        // Spec 5.4 / gap table 9: the state holds the allpass output; the
        // listing's `yv[1] = *output` (the bell output) is unstable.
        self.y1 = ap;
        (self.wet * ap + self.dry * x) as f32
    }

    /// |H| in dB at `freq_hz`: the gain a steady sine there gets.
    pub fn magnitude_db(&self, freq_hz: f64, sample_rate: f64) -> f64 {
        // dry + wet A(z), with the allpass A(z) = (b0 + b1 z^-1 + z^-2) /
        // (1 + b1 z^-1 + b0 z^-2), over A's denominator.
        Coefs {
            b0: self.dry + self.wet * self.b0,
            b1: (self.dry + self.wet) * self.b1,
            b2: self.dry * self.b0 + self.wet,
            a1: self.b1,
            a2: self.b0,
        }
        .magnitude_db(freq_hz, sample_rate)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const FS: f64 = 48_000.0;

    /// Gain in dB and phase in degrees of an impulse response at `freq_hz`.
    pub(crate) fn response_at(h: &[f32], freq_hz: f64, sample_rate: f64) -> (f64, f64) {
        let w = 2.0 * std::f64::consts::PI * freq_hz / sample_rate;
        let (mut re, mut im) = (0.0, 0.0);
        for (n, v) in h.iter().enumerate() {
            let phase = w * n as f64;
            re += f64::from(*v) * phase.cos();
            im -= f64::from(*v) * phase.sin();
        }
        (
            10.0 * (re * re + im * im).log10(),
            im.atan2(re).to_degrees(),
        )
    }

    fn impulse_response(bell: &mut Bell, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| bell.tick(if i == 0 { 1.0 } else { 0.0 }))
            .collect()
    }

    #[test]
    fn coefficients_match_spec() {
        for (fc, q, b0, b1) in [
            (1000.0, 2.0, 0.936602, -1.920034),
            (30.0, 1.0, 0.996081, -1.996065),
            (16000.0, 2.0, 0.267949, 0.633975),
        ] {
            let bell = Bell::new(fc, q, 0.0, FS);
            assert!((bell.b0 - b0).abs() < 1e-6, "{fc} Hz b0 = {}", bell.b0);
            assert!((bell.b1 - b1).abs() < 1e-6, "{fc} Hz b1 = {}", bell.b1);
        }
    }

    #[test]
    fn boost_response_matches_spec() {
        let h = impulse_response(&mut Bell::new(1000.0, 2.0, 12.0, FS), 8192);
        let (at_fc, phase) = response_at(&h, 1000.0, FS);
        assert!((at_fc - 12.0).abs() < 1e-3, "{at_fc} dB at fc");
        assert!(phase.abs() < 0.01, "{phase} degrees at fc");
        for f in [0.0, 24_000.0] {
            let (db, _) = response_at(&h, f, FS);
            assert!(db.abs() < 1e-3, "{db} dB at {f} Hz");
        }
        // Spec 10 says "about 4 dB"; the closed form gives 3.960 and 3.935.
        for (f, want) in [(500.0, 3.960), (2000.0, 3.935)] {
            let (db, _) = response_at(&h, f, FS);
            assert!((db - want).abs() < 0.01, "{db} dB at {f} Hz");
        }
    }

    #[test]
    fn cut_is_mirror_of_boost() {
        let h = impulse_response(&mut Bell::new(1000.0, 2.0, -12.0, FS), 8192);
        let (at_fc, _) = response_at(&h, 1000.0, FS);
        assert!((at_fc + 12.0).abs() < 1e-3, "{at_fc} dB at fc");
    }

    #[test]
    fn notch_bandwidth_matches_q() {
        // G -> 0 is a notch whose -3 dB edges are fc/Q apart (spec 5.3).
        let h = impulse_response(&mut Bell::new(1000.0, 2.0, -200.0, FS), 8192);
        for f in [781.9, 1279.0] {
            let (db, _) = response_at(&h, f, FS);
            assert!((db + 3.01).abs() < 0.05, "{db} dB at {f} Hz");
        }
    }

    #[test]
    fn low_bands_at_192_khz_are_stable_and_exact() {
        // With f32 coefficients and state the 30 Hz band measured -18.77 dB
        // here, and the 5 Hz band diverged to NaN.
        let fs = 192_000.0;
        for (fc, q, gain_db, n) in [
            (30.0, 1.5, -24.0, 1 << 17),
            (5.0, 2.0, -24.0, 1 << 19),
            (5.0, 2.0, 12.0, 1 << 19),
        ] {
            let h = impulse_response(&mut Bell::new(fc, q, gain_db, fs), n);
            assert!(h.iter().all(|v| v.is_finite()), "{fc} Hz diverged");
            let (db, _) = response_at(&h, fc, fs);
            assert!((db - gain_db).abs() < 1e-3, "{fc} Hz: {db} dB at fc");
        }
    }

    #[test]
    fn magnitude_is_what_a_steady_sine_gets() {
        use crate::stereo::tests::amplitude;
        use std::f64::consts::PI;
        // A boost and a cut, a low band at a high rate and a wide one, each
        // measured on whole-cycle sines over the second after the first.
        for (fc, q, gain_db, fs) in [
            (1000.0, 2.0, 9.0, FS),
            (200.0, 0.7, -12.0, FS),
            (40.0, 1.5, 6.0, 96_000.0),
            (8000.0, 0.8, 4.5, 44_100.0),
        ] {
            let bell = Bell::new(fc, q, gain_db, fs);
            for f in [
                25.0, 40.0, 100.0, 200.0, 997.0, 1000.0, 3000.0, 8000.0, 15_000.0,
            ] {
                let mut ticking = Bell::new(fc, q, gain_db, fs);
                let n = fs as usize;
                let out: Vec<f32> = (0..2 * n)
                    .map(|i| ticking.tick((0.5 * (2.0 * PI * f * i as f64 / fs).sin()) as f32))
                    .collect();
                let measured = 20.0 * (amplitude(&out[n..], f, fs) / 0.5).log10();
                let want = bell.magnitude_db(f, fs);
                assert!(
                    (measured - want).abs() < 0.005,
                    "{fc} Hz bell at {f} Hz: measured {measured} dB, magnitude {want} dB"
                );
            }
            assert!((bell.magnitude_db(fc, fs) - gain_db).abs() < 1e-9);
        }
    }

    #[test]
    fn zero_gain_is_exact_passthrough() {
        let mut bell = Bell::new(30.0, 1.5, 0.0, FS);
        for x in [0.3f32, -0.7, 0.0, 0.999] {
            assert_eq!(bell.tick(x), x);
        }
    }
}
