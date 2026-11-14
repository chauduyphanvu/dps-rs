//! Second-order filters from Robert Bristow-Johnson's Audio EQ Cookbook, run
//! in transposed direct form II.
//!
//! Coefficients and state are f64, like the shelves' and bells': a low corner
//! at a high sample rate puts the poles within (2 pi fc / fs)^2 of the unit
//! circle, closer than f32 resolves.

use std::f64::consts::PI;

/// One biquad's coefficients, with a0 normalised to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

/// sin(w0) and cos(w0) of `freq_hz`, and the cookbook's alpha for `q`.
fn angle(freq_hz: f64, q: f64, sample_rate: f64) -> (f64, f64) {
    let (sin, cos) = (2.0 * PI * freq_hz / sample_rate).sin_cos();
    (sin / (2.0 * q), cos)
}

impl Coefs {
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    fn normalised([b0, b1, b2]: [f64; 3], [a0, a1, a2]: [f64; 3]) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// `gain_db` at `freq_hz`, 0 dB at DC and Nyquist; `freq_hz / q` wide at
    /// the half-gain points (in dB).
    pub fn peaking(freq_hz: f64, q: f64, gain_db: f64, sample_rate: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (alpha, cos) = angle(freq_hz, q, sample_rate);
        Self::normalised(
            [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a],
            [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a],
        )
    }

    /// `gain_db` at DC, 0 dB at Nyquist, half the gain (in dB) at `freq_hz`.
    /// `q` is the cookbook's shelf Q: 1/sqrt(2) is the steepest slope
    /// without a bump.
    pub fn low_shelf(freq_hz: f64, q: f64, gain_db: f64, sample_rate: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (alpha, cos) = angle(freq_hz, q, sample_rate);
        let k = 2.0 * a.sqrt() * alpha;
        Self::normalised(
            [
                a * ((a + 1.0) - (a - 1.0) * cos + k),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - k),
            ],
            [
                (a + 1.0) + (a - 1.0) * cos + k,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - k,
            ],
        )
    }

    /// 0 dB at DC, `gain_db` at Nyquist, half the gain (in dB) at `freq_hz`.
    pub fn high_shelf(freq_hz: f64, q: f64, gain_db: f64, sample_rate: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (alpha, cos) = angle(freq_hz, q, sample_rate);
        let k = 2.0 * a.sqrt() * alpha;
        Self::normalised(
            [
                a * ((a + 1.0) + (a - 1.0) * cos + k),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - k),
            ],
            [
                (a + 1.0) - (a - 1.0) * cos + k,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - k,
            ],
        )
    }

    /// |H| = `q` at `freq_hz`: -3.01 dB with Q = 1/sqrt(2) (Butterworth).
    pub fn low_pass(freq_hz: f64, q: f64, sample_rate: f64) -> Self {
        let (alpha, cos) = angle(freq_hz, q, sample_rate);
        let b = 1.0 - cos;
        Self::normalised(
            [0.5 * b, b, 0.5 * b],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    /// |H| = `q` at `freq_hz`: -3.01 dB with Q = 1/sqrt(2) (Butterworth).
    pub fn high_pass(freq_hz: f64, q: f64, sample_rate: f64) -> Self {
        let (alpha, cos) = angle(freq_hz, q, sample_rate);
        let b = 1.0 + cos;
        Self::normalised(
            [0.5 * b, -b, 0.5 * b],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    /// |H| in dB at `freq_hz`.
    pub fn magnitude_db(&self, freq_hz: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * PI * freq_hz / sample_rate;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let num = (
            self.b0 + self.b1 * c1 + self.b2 * c2,
            self.b1 * s1 + self.b2 * s2,
        );
        let den = (
            1.0 + self.a1 * c1 + self.a2 * c2,
            self.a1 * s1 + self.a2 * s2,
        );
        10.0 * ((num.0 * num.0 + num.1 * num.1) / (den.0 * den.0 + den.1 * den.1)).log10()
    }
}

pub struct Biquad {
    c: Coefs,
    s1: f64,
    s2: f64,
}

impl Biquad {
    pub fn new(coefs: Coefs) -> Self {
        Self {
            c: coefs,
            s1: 0.0,
            s2: 0.0,
        }
    }

    pub fn tick(&mut self, x: f32) -> f32 {
        let x = f64::from(x);
        let c = &self.c;
        let y = c.b0 * x + self.s1;
        self.s1 = c.b1 * x - c.a1 * y + self.s2;
        self.s2 = c.b2 * x - c.a2 * y;
        y as f32
    }

    /// Changes the response and keeps the state (for coefficients that move
    /// while running).
    pub fn set_coefs(&mut self, coefs: Coefs) {
        self.c = coefs;
    }

    /// Copies `from`'s state. No allocation.
    pub fn adopt(&mut self, from: &Self) {
        self.s1 = from.s1;
        self.s2 = from.s2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stereo::tests::amplitude;
    use std::f64::consts::FRAC_1_SQRT_2;

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

    fn assert_db(got: f64, want: f64, tol: f64, what: &str) {
        assert!((got - want).abs() < tol, "{what}: {got} dB, want {want} dB");
    }

    #[test]
    fn peaking_has_its_gain_at_the_centre_and_none_far_from_it() {
        for (freq, q, gain) in [(1000.0, 2.0, 9.0), (60.0, 0.7, -12.0), (12_000.0, 4.0, 3.5)] {
            let c = Coefs::peaking(freq, q, gain, FS);
            assert_db(c.magnitude_db(freq, FS), gain, 1e-9, "centre");
            assert_db(c.magnitude_db(0.0, FS), 0.0, 1e-9, "DC");
            assert_db(c.magnitude_db(FS / 2.0, FS), 0.0, 1e-9, "Nyquist");
        }
        // Six octaves off a Q 2 band, under a hundredth of its gain is left.
        let c = Coefs::peaking(1000.0, 2.0, 9.0, FS);
        for f in [15.6, 22_000.0] {
            assert!(c.magnitude_db(f, FS).abs() < 0.09, "{f} Hz");
        }
    }

    #[test]
    fn shelves_have_their_gain_at_one_end_and_none_at_the_other() {
        for (freq, q, gain) in [(200.0, FRAC_1_SQRT_2, -6.0), (3000.0, 1.2, 9.0)] {
            let low = Coefs::low_shelf(freq, q, gain, FS);
            assert_db(low.magnitude_db(0.0, FS), gain, 1e-9, "low shelf at DC");
            assert_db(
                low.magnitude_db(FS / 2.0, FS),
                0.0,
                1e-9,
                "low shelf at Nyquist",
            );
            assert_db(
                low.magnitude_db(freq, FS),
                gain / 2.0,
                1e-9,
                "low shelf at fc",
            );
            let high = Coefs::high_shelf(freq, q, gain, FS);
            assert_db(high.magnitude_db(0.0, FS), 0.0, 1e-9, "high shelf at DC");
            assert_db(
                high.magnitude_db(FS / 2.0, FS),
                gain,
                1e-9,
                "high shelf at Nyquist",
            );
            assert_db(
                high.magnitude_db(freq, FS),
                gain / 2.0,
                1e-9,
                "high shelf at fc",
            );
        }
        // Near the ends rather than at them.
        let low = Coefs::low_shelf(1000.0, FRAC_1_SQRT_2, 6.0, FS);
        assert_db(low.magnitude_db(10.0, FS), 6.0, 1e-3, "low shelf at 10 Hz");
        assert_db(
            low.magnitude_db(23_000.0, FS),
            0.0,
            1e-3,
            "low shelf at 23 kHz",
        );
    }

    #[test]
    fn butterworth_passes_are_3_db_down_at_the_cutoff() {
        let half_power = 20.0 * FRAC_1_SQRT_2.log10();
        assert_db(half_power, -3.0103, 1e-4, "half power");
        for freq in [80.0, 1000.0, 15_000.0] {
            let low = Coefs::low_pass(freq, FRAC_1_SQRT_2, FS);
            assert_db(
                low.magnitude_db(freq, FS),
                half_power,
                1e-9,
                "low-pass at fc",
            );
            assert_db(low.magnitude_db(0.0, FS), 0.0, 1e-9, "low-pass at DC");
            let high = Coefs::high_pass(freq, FRAC_1_SQRT_2, FS);
            assert_db(
                high.magnitude_db(freq, FS),
                half_power,
                1e-9,
                "high-pass at fc",
            );
            assert_db(
                high.magnitude_db(FS / 2.0, FS),
                0.0,
                1e-9,
                "high-pass at Nyquist",
            );
        }
        // Second order: 12 dB per octave well past the cutoff, and well under
        // Nyquist, where the bilinear transform's warping steepens it.
        let low = Coefs::low_pass(200.0, FRAC_1_SQRT_2, FS);
        let slope = low.magnitude_db(1600.0, FS) - low.magnitude_db(3200.0, FS);
        assert!((slope - 12.0).abs() < 0.5, "{slope} dB per octave");
    }

    #[test]
    fn a_steady_sine_comes_out_at_the_designed_magnitude() {
        let filters = [
            Coefs::peaking(1000.0, 2.0, 9.0, FS),
            Coefs::low_shelf(200.0, FRAC_1_SQRT_2, -6.0, FS),
            Coefs::high_shelf(6000.0, 0.9, 5.0, FS),
            Coefs::low_pass(2000.0, FRAC_1_SQRT_2, FS),
            Coefs::high_pass(150.0, FRAC_1_SQRT_2, FS),
        ];
        for (i, coefs) in filters.iter().enumerate() {
            for f in [50.0, 300.0, 1000.0, 4000.0, 12_000.0] {
                let mut biquad = Biquad::new(*coefs);
                let out: Vec<f32> = (0..96_000)
                    .map(|n| biquad.tick((0.5 * (2.0 * PI * f * n as f64 / FS).sin()) as f32))
                    .collect();
                let got = 20.0 * (amplitude(&out[48_000..], f, FS) / 0.5).log10();
                let want = coefs.magnitude_db(f, FS);
                assert_db(got, want, 0.01, &format!("filter {i} at {f} Hz"));
            }
        }
    }

    #[test]
    fn zero_db_peaking_and_shelves_are_exact_passthrough() {
        let input = noise(10_000, 1);
        for coefs in [
            Coefs::peaking(1000.0, 1.5, 0.0, FS),
            Coefs::low_shelf(100.0, FRAC_1_SQRT_2, 0.0, FS),
            Coefs::high_shelf(8000.0, 2.0, 0.0, FS),
            Coefs::IDENTITY,
        ] {
            let mut biquad = Biquad::new(coefs);
            let out: Vec<f32> = input.iter().map(|x| biquad.tick(*x)).collect();
            assert_eq!(out, input, "{coefs:?}");
        }
        assert_eq!(Coefs::IDENTITY.magnitude_db(1234.0, FS), 0.0);
    }

    #[test]
    fn set_coefs_keeps_the_state_and_adopt_copies_it() {
        // B takes over A's state through an identity filter's, then A's
        // coefficients: from then on they play alike.
        let coefs = Coefs::peaking(300.0, 0.8, -9.0, FS);
        let input = noise(4000, 2);
        let mut a = Biquad::new(coefs);
        for x in &input[..1000] {
            a.tick(*x);
        }
        let mut b = Biquad::new(Coefs::IDENTITY);
        b.adopt(&a);
        b.set_coefs(coefs);
        let mut cold = Biquad::new(coefs);
        let mut differs = false;
        for x in &input[1000..] {
            let (ya, yb, yc) = (a.tick(*x), b.tick(*x), cold.tick(*x));
            assert_eq!(ya, yb);
            differs |= ya != yc;
        }
        assert!(differs, "a filter without the state plays the same");
    }
}
