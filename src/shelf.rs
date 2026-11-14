//! First-order shelving filters (spec section 3), magnitude-complementary:
//! a boost is the exact inverse of the cut with the same corner and |gain|.
//!
//! Coefficients and state are f64: for a low corner at a high sample rate
//! alpha is within 2 pi fc / fs of 1, and in f32 the rounding of alpha and
//! alpha^2 - 1 moved the DC gain by 0.24 dB (1 Hz, 24 dB, 192 kHz).

/// The patent's first-order allpass coefficient. Equal to (1 - t)/(1 + t)
/// with t = tan(pi fc / fs), so the allpass is at 90 degrees exactly at fc.
pub fn allpass_coef(corner_hz: f64, sample_rate: f64) -> f64 {
    let theta = 2.0 * std::f64::consts::PI * corner_hz / sample_rate;
    (1.0 - theta.sin()) / theta.cos()
}

pub struct Shelf {
    boost: bool,
    a: f64,
    a2m1: f64,
    dry: f64,
    wet: f64,
    w1: f64,
    s: f64,
}

impl Shelf {
    /// Unity above the corner, `gain_db` below it.
    pub fn low(gain_db: f64, corner_hz: f64, sample_rate: f64) -> Self {
        Self::new(1.0, gain_db, corner_hz, sample_rate)
    }

    /// Unity below the corner, `gain_db` above it.
    pub fn high(gain_db: f64, corner_hz: f64, sample_rate: f64) -> Self {
        Self::new(-1.0, gain_db, corner_hz, sample_rate)
    }

    /// `sign` is +1 for a low shelf, -1 for a high shelf. The cut-mode filter
    /// is H = (1+G)/2 + sign (1-G)/2 A(z) with G = 10^(-|gain_db|/20); a
    /// positive `gain_db` runs the patent's delay-free inverse of that cut.
    fn new(sign: f64, gain_db: f64, corner_hz: f64, sample_rate: f64) -> Self {
        let a = allpass_coef(corner_hz, sample_rate);
        let g = 10f64.powf(-gain_db.abs() / 20.0);
        // Spec 3.4 items 3-4 / gap table 4-5: the printed boost equation has
        // the wrong sign on the l term and defines wpt[1] only for the low
        // shelf; the high shelf needs -alpha in it to be the exact inverse.
        let w1 = 2.0 / ((1.0 + g) + sign * a * (1.0 - g));
        Self {
            boost: gain_db > 0.0,
            a,
            a2m1: a * a - 1.0,
            dry: 0.5 * (1.0 + g),
            wet: sign * 0.5 * (1.0 - g),
            w1,
            s: 0.0,
        }
    }

    pub fn tick(&mut self, x: f32) -> f32 {
        let x = f64::from(x);
        let out = if self.boost {
            let out = self.w1 * (x - self.wet * self.a2m1 * self.s);
            self.s = self.a * self.s + out;
            out
        } else {
            let ap = self.a * x + self.a2m1 * self.s;
            self.s = self.a * self.s + x;
            self.dry * x + self.wet * ap
        };
        out as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48_000.0;

    fn impulse_response(filter: &mut Shelf, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| f64::from(filter.tick(if i == 0 { 1.0 } else { 0.0 })))
            .collect()
    }

    /// Impulse response of y = b0 x + b1 x[k-1] - a1 y[k-1], in f64.
    fn direct_form(b0: f64, b1: f64, a1: f64, n: usize) -> Vec<f64> {
        let mut h = vec![b0];
        h.push(b1 - a1 * b0);
        while h.len() < n {
            let prev = h[h.len() - 1];
            h.push(-a1 * prev);
        }
        h
    }

    fn assert_close(got: &[f64], want: &[f64], tol: f64) {
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            assert!((g - w).abs() < tol, "sample {i}: got {g}, want {w}");
        }
    }

    #[test]
    fn allpass_coefficient_matches_spec() {
        assert!((allpass_coef(1000.0, 48_000.0) - 0.87697646).abs() < 1e-8);
        assert!((allpass_coef(1000.0, 44_100.0) - 0.86678844).abs() < 1e-8);
    }

    #[test]
    fn cut_shelves_match_spec_direct_form() {
        let low = impulse_response(&mut Shelf::low(-10.0, 1000.0, FS), 64);
        assert_close(
            &low,
            &direct_form(0.95793996, -0.91903650, -0.87697646, 64),
            1e-6,
        );
        let high = impulse_response(&mut Shelf::high(-10.0, 1000.0, FS), 64);
        assert_close(
            &high,
            &direct_form(0.35828781, -0.23526427, -0.87697646, 64),
            1e-6,
        );
    }

    #[test]
    fn boost_gain_factor_matches_spec() {
        // The first boost output sample is w1 * x because the state starts at 0.
        assert!((f64::from(Shelf::low(10.0, 1000.0, FS).tick(1.0)) - 1.04390676).abs() < 1e-6);
        assert!((f64::from(Shelf::high(10.0, 1000.0, FS).tick(1.0)) - 2.79105229).abs() < 1e-6);
    }

    #[test]
    fn cut_then_boost_is_identity() {
        for (cut, boost) in [
            (Shelf::low(-10.0, 1000.0, FS), Shelf::low(10.0, 1000.0, FS)),
            (
                Shelf::high(-12.0, 1000.0, FS),
                Shelf::high(12.0, 1000.0, FS),
            ),
            (
                Shelf::low(-24.0, 300.0, 44_100.0),
                Shelf::low(24.0, 300.0, 44_100.0),
            ),
        ] {
            let (mut cut, mut boost) = (cut, boost);
            let h: Vec<f64> = (0..2000)
                .map(|i| f64::from(boost.tick(cut.tick(if i == 0 { 1.0 } else { 0.0 }))))
                .collect();
            let mut want = vec![0.0; 2000];
            want[0] = 1.0;
            // f32 processing; the f64 reference in the spec reaches 1e-12.
            assert_close(&h, &want, 1e-6);
        }
    }

    #[test]
    fn shelf_gains_at_dc_and_nyquist() {
        let g = 10f64.powf(-10.0 / 20.0);
        let sum = |h: &[f64]| h.iter().sum::<f64>();
        let alt = |h: &[f64]| {
            h.iter()
                .enumerate()
                .map(|(i, v)| if i % 2 == 0 { *v } else { -v })
                .sum::<f64>()
        };
        let low = impulse_response(&mut Shelf::low(-10.0, 1000.0, FS), 4000);
        assert!((sum(&low) - g).abs() < 1e-5 && (alt(&low) - 1.0).abs() < 1e-5);
        let high = impulse_response(&mut Shelf::high(10.0, 1000.0, FS), 4000);
        assert!((sum(&high) - 1.0).abs() < 1e-5 && (alt(&high) - 1.0 / g).abs() < 1e-4);
    }

    #[test]
    fn lowest_corner_at_192_khz_keeps_its_dc_gain() {
        // With f32 coefficients and state these were 0.12 dB (cut) and
        // 0.24 dB (boost) off. The boost's pole is 1/G times slower than the
        // cut's, hence the longer run.
        let fs = 192_000.0;
        for (gain_db, n) in [(-24.0, 600_000), (24.0, 10_000_000)] {
            let mut shelf = Shelf::low(gain_db, 1.0, fs);
            let mut y = 0.0;
            for _ in 0..n {
                y = shelf.tick(1.0);
            }
            let db = 20.0 * f64::from(y).log10();
            assert!(
                (db - gain_db).abs() < 1e-3,
                "{gain_db} dB shelf: {db} dB at DC"
            );
        }
    }

    #[test]
    fn zero_gain_is_exact_passthrough() {
        let mut shelves = [Shelf::low(0.0, 1000.0, FS), Shelf::high(0.0, 1000.0, FS)];
        for x in [0.3f32, -0.7, 1e-20, 0.999] {
            for s in &mut shelves {
                assert_eq!(s.tick(x), x);
            }
        }
    }
}
