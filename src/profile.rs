//! A profile is every tunable value of the chain. The patents publish no
//! profile values, so the defaults here are conventional starting points, not
//! Bongiovi tunings.

use serde::{Deserialize, Serialize};

pub const EQ_BANDS: usize = 11;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub input_gain_db: f32,
    pub output_gain_db: f32,
    /// Corner of all four first-order shelves (1000 Hz in the patent).
    pub shelf_corner_hz: f32,
    /// Gain of the first low shelf (a cut, so <= 0). The second low shelf
    /// applies the exact inverse after compressor 1.
    pub low_shelf_db: f32,
    /// Gain of the first high shelf (a boost, so >= 0). The second high shelf
    /// applies the exact inverse after compressor 1.
    pub high_shelf_db: f32,
    pub compressor1: CompressorParams,
    pub compressor2: CompressorParams,
    pub eq: EqParams,
    pub stereo: StereoParams,
    pub limiter: LimiterParams,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressorParams {
    pub threshold_db: f32,
    /// r in r:1, >= 1. 1 disables the compressor.
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EqParams {
    pub freqs_hz: [f32; EQ_BANDS],
    pub q: [f32; EQ_BANDS],
    pub gains_db: [f32; EQ_BANDS],
}

/// Stereo field enhancement (US 9,398,394). Ignored unless the input has
/// exactly two channels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StereoParams {
    pub enabled: bool,
    /// Corner of the band split; the side boost applies above it (patent
    /// range 20-1000 Hz). The split is first-order, so content below it is
    /// not untouched: the boost fades out at 6 dB per octave below it.
    pub crossover_hz: f32,
    /// Makeup gain on the side signal and also the cap on its gain
    /// reduction (patent range 0-12 dB, example 8 dB).
    pub side_boost_db: f32,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

/// Peak limiter on the output, the last stage (US 9,350,309's output gain
/// reduction above a threshold, which the patent gives no values for).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimiterParams {
    pub enabled: bool,
    /// No output sample goes above this level.
    pub ceiling_db: f32,
    /// Time constant of the gain's recovery, which starts 10 ms after the
    /// last sample that needed the gain down.
    pub release_ms: f32,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            input_gain_db: 0.0,
            // Makeup for the two 4:1 compressors, with the limiter catching
            // the peaks it raises: measured (BS.1770) on a synthetic band mix
            // at -14 LUFS, the output is within 0.3 LU of the input.
            output_gain_db: 7.0,
            shelf_corner_hz: 1000.0,
            low_shelf_db: -10.0,
            high_shelf_db: 8.0,
            compressor1: CompressorParams::default(),
            compressor2: CompressorParams::default(),
            eq: EqParams::default(),
            stereo: StereoParams::default(),
            limiter: LimiterParams::default(),
        }
    }
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 10.0,
            release_ms: 100.0,
        }
    }
}

impl Default for EqParams {
    fn default() -> Self {
        // The patent fixes only the 30 Hz and 16 kHz end bands and says the
        // rest are about an octave apart; these are log-uniform between them.
        Self {
            freqs_hz: [
                30.0, 56.4, 106.0, 199.0, 375.0, 705.0, 1327.0, 2497.0, 4698.0, 8839.0, 16000.0,
            ],
            q: [1.5; EQ_BANDS],
            gains_db: [0.0; EQ_BANDS],
        }
    }
}

impl Default for StereoParams {
    fn default() -> Self {
        Self {
            enabled: false,
            crossover_hz: 200.0,
            side_boost_db: 8.0,
            threshold_db: -30.0,
            ratio: 4.0,
            attack_ms: 10.0,
            release_ms: 200.0,
        }
    }
}

impl Default for LimiterParams {
    fn default() -> Self {
        Self {
            enabled: true,
            ceiling_db: -1.0,
            release_ms: 100.0,
        }
    }
}

impl Profile {
    /// Checks every value against the range the DSP is defined for at the
    /// given sample rate. EQ bands too high for the rate are not errors; the
    /// chain skips them (`skipped_eq_bands`).
    pub fn validate(&self, sample_rate: u32) -> Result<(), String> {
        let nyquist = sample_rate as f32 / 2.0;
        let in_range = |name: &str, v: f32, lo: f32, hi: f32| {
            if v >= lo && v <= hi {
                Ok(())
            } else {
                Err(format!("{name} = {v} is outside {lo}..={hi}"))
            }
        };
        let below_nyquist = |name: &str, v: f32| {
            if v > 0.0 && v < nyquist {
                Ok(())
            } else {
                Err(format!(
                    "{name} = {v} Hz must be above 0 and below {nyquist} Hz at {sample_rate} Hz sample rate"
                ))
            }
        };
        let compressor = |name: &str, threshold_db: f32, ratio: f32, attack: f32, release: f32| {
            in_range(&format!("{name}.threshold_db"), threshold_db, -120.0, 0.0)?;
            in_range(&format!("{name}.ratio"), ratio, 1.0, 1000.0)?;
            in_range(&format!("{name}.attack_ms"), attack, 0.01, 10_000.0)?;
            in_range(&format!("{name}.release_ms"), release, 0.01, 10_000.0)
        };

        in_range("input_gain_db", self.input_gain_db, -120.0, 120.0)?;
        in_range("output_gain_db", self.output_gain_db, -120.0, 120.0)?;
        // A 1 kHz-class first-order shelf needs its corner below fs/4 for the
        // patent's alpha = (1 - sin)/cos to stay in (0, 1).
        in_range(
            "shelf_corner_hz",
            self.shelf_corner_hz,
            1.0,
            nyquist / 2.0 - 1.0,
        )?;
        in_range("low_shelf_db", self.low_shelf_db, -24.0, 0.0)?;
        in_range("high_shelf_db", self.high_shelf_db, 0.0, 24.0)?;
        for (name, c) in [
            ("compressor1", self.compressor1),
            ("compressor2", self.compressor2),
        ] {
            compressor(name, c.threshold_db, c.ratio, c.attack_ms, c.release_ms)?;
        }
        // The bell's poles sit about (2 pi fc / fs)^2 inside the unit circle,
        // so very low bands run out of f64 precision: measured 0.04 dB off at
        // fs / 1e7 and 0.36 dB at fs / 1e8, against 0.001 dB at fs / 1e6.
        let lowest_band = f64::from(sample_rate) / 1e6;
        for i in 0..EQ_BANDS {
            let freq = self.eq.freqs_hz[i];
            if !(f64::from(freq) >= lowest_band && freq.is_finite()) {
                return Err(format!(
                    "eq.freqs_hz[{i}] = {freq} Hz must be at least {lowest_band} Hz at {sample_rate} Hz sample rate"
                ));
            }
            in_range(&format!("eq.q[{i}]"), self.eq.q[i], 0.1, 20.0)?;
            in_range(
                &format!("eq.gains_db[{i}]"),
                self.eq.gains_db[i],
                -24.0,
                24.0,
            )?;
        }
        let s = self.stereo;
        in_range("stereo.crossover_hz", s.crossover_hz, 20.0, 1000.0)?;
        below_nyquist("stereo.crossover_hz", s.crossover_hz)?;
        in_range("stereo.side_boost_db", s.side_boost_db, 0.0, 12.0)?;
        compressor("stereo", s.threshold_db, s.ratio, s.attack_ms, s.release_ms)?;
        in_range("limiter.ceiling_db", self.limiter.ceiling_db, -24.0, 0.0)?;
        in_range("limiter.release_ms", self.limiter.release_ms, 1.0, 2000.0)
    }

    /// The EQ bands the chain skips at `sample_rate`, passing audio through
    /// them unchanged: those whose centre or bandwidth (freqs_hz / q) is not
    /// below Nyquist, where the bell's allpass would be unstable or
    /// meaningless.
    pub fn skipped_eq_bands(&self, sample_rate: u32) -> Vec<usize> {
        let nyquist = sample_rate as f32 / 2.0;
        (0..EQ_BANDS)
            .filter(|&i| {
                let (freq, q) = (self.eq.freqs_hz[i], self.eq.q[i]);
                // Written so that NaN counts as out of range.
                !(freq < nyquist && freq / q < nyquist)
            })
            .collect()
    }

    /// One phrase naming the bands `skipped_eq_bands` returns, e.g.
    /// "eq.freqs_hz[9] (8839 Hz) and eq.freqs_hz[10] (16000 Hz)", or `None`.
    pub fn describe_skipped_eq_bands(&self, sample_rate: u32) -> Option<String> {
        let names: Vec<String> = self
            .skipped_eq_bands(sample_rate)
            .into_iter()
            .map(|i| format!("eq.freqs_hz[{i}] ({} Hz)", self.eq.freqs_hz[i]))
            .collect();
        match names.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            [rest @ .., last] => Some(format!("{} and {last}", rest.join(", "))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(profile: &Profile, sample_rate: u32, needle: &str) {
        match profile.validate(sample_rate) {
            Ok(()) => panic!("accepted at {sample_rate} Hz, expected an error about {needle}"),
            Err(e) => assert!(e.contains(needle), "{needle:?} not in {e:?}"),
        }
    }

    fn accepted(profile: &Profile, sample_rate: u32) {
        if let Err(e) = profile.validate(sample_rate) {
            panic!("rejected at {sample_rate} Hz: {e}");
        }
    }

    #[test]
    fn default_profile_is_valid_at_common_rates() {
        for fs in [44_100, 48_000, 96_000, 192_000, 768_000] {
            accepted(&Profile::default(), fs);
        }
    }

    #[test]
    fn input_and_output_gains_are_limited_to_120_db() {
        for v in [-120.0, 0.0, 120.0] {
            accepted(
                &Profile {
                    input_gain_db: v,
                    ..Profile::default()
                },
                48_000,
            );
            accepted(
                &Profile {
                    output_gain_db: v,
                    ..Profile::default()
                },
                48_000,
            );
        }
        for v in [
            -120.5,
            120.5,
            800.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            let p = Profile {
                input_gain_db: v,
                ..Profile::default()
            };
            rejected(&p, 48_000, "input_gain_db");
            let p = Profile {
                output_gain_db: v,
                ..Profile::default()
            };
            rejected(&p, 48_000, "output_gain_db");
        }
    }

    #[test]
    fn shelf_corner_stays_between_1_hz_and_a_quarter_of_the_rate() {
        // At 48 kHz the upper limit is 48000 / 4 - 1 = 11999 Hz.
        for v in [1.0, 11_999.0] {
            accepted(
                &Profile {
                    shelf_corner_hz: v,
                    ..Profile::default()
                },
                48_000,
            );
        }
        for v in [0.9, 12_000.0, 30_000.0] {
            let p = Profile {
                shelf_corner_hz: v,
                ..Profile::default()
            };
            rejected(&p, 48_000, "shelf_corner_hz");
        }
    }

    #[test]
    fn eq_bands_at_or_above_nyquist_are_skipped_not_rejected() {
        // freqs_hz / q is the bandwidth; 16000 / 0.7 = 22857 Hz is under
        // 24 kHz, 16000 / 0.66 = 24242 Hz is not.
        let mut p = Profile::default();
        p.eq.q[10] = 0.7;
        accepted(&p, 48_000);
        assert_eq!(p.skipped_eq_bands(48_000), []);
        p.eq.q[10] = 0.66;
        accepted(&p, 48_000);
        assert_eq!(p.skipped_eq_bands(48_000), [10]);

        // A centre at Nyquist is skipped too: 16 kHz at 32 kHz.
        let p = Profile::default();
        for (fs, skipped) in [
            (44_100, &[][..]),
            (32_002, &[][..]),
            (32_000, &[10][..]),
            (22_050, &[10][..]),
            (16_000, &[9, 10][..]),
            (8_000, &[8, 9, 10][..]),
        ] {
            accepted(&p, fs);
            assert_eq!(p.skipped_eq_bands(fs), skipped, "{fs} Hz");
        }
        assert_eq!(
            p.describe_skipped_eq_bands(16_000).unwrap(),
            "eq.freqs_hz[9] (8839 Hz) and eq.freqs_hz[10] (16000 Hz)"
        );
        assert_eq!(
            p.describe_skipped_eq_bands(8_000).unwrap(),
            "eq.freqs_hz[8] (4698 Hz), eq.freqs_hz[9] (8839 Hz) and eq.freqs_hz[10] (16000 Hz)"
        );
        assert_eq!(p.describe_skipped_eq_bands(48_000), None);
    }

    #[test]
    fn eq_band_must_not_be_too_low_for_the_rate() {
        // The limit is fs / 1e6: 0.048 Hz at 48 kHz, 0.192 Hz at 192 kHz.
        for (fs, ok, low) in [(48_000, 0.05, 0.047), (192_000, 0.2, 0.19)] {
            let mut p = Profile::default();
            p.eq.freqs_hz[0] = ok;
            accepted(&p, fs);
            p.eq.freqs_hz[0] = low;
            rejected(&p, fs, "eq.freqs_hz[0]");
        }
        for bad in [0.0, -30.0, f32::NAN, f32::INFINITY] {
            let mut p = Profile::default();
            p.eq.freqs_hz[4] = bad;
            rejected(&p, 48_000, "eq.freqs_hz[4]");
        }
    }

    #[test]
    fn eq_gains_are_limited_to_24_db() {
        for v in [-24.0, 24.0] {
            let mut p = Profile::default();
            p.eq.gains_db[3] = v;
            accepted(&p, 48_000);
        }
        for v in [-24.1, 24.1, f32::NAN] {
            let mut p = Profile::default();
            p.eq.gains_db[3] = v;
            rejected(&p, 48_000, "eq.gains_db[3]");
        }
    }

    #[test]
    fn side_boost_is_limited_to_0_to_12_db() {
        for v in [0.0, 12.0] {
            let mut p = Profile::default();
            p.stereo.side_boost_db = v;
            accepted(&p, 48_000);
        }
        for v in [-0.1, -12.0, 12.1] {
            let mut p = Profile::default();
            p.stereo.side_boost_db = v;
            rejected(&p, 48_000, "stereo.side_boost_db");
        }
    }

    #[test]
    fn limiter_ceiling_and_release_have_limits() {
        for (ceiling, release) in [(-24.0, 1.0), (0.0, 2000.0), (-1.0, 100.0)] {
            let mut p = Profile::default();
            p.limiter.ceiling_db = ceiling;
            p.limiter.release_ms = release;
            accepted(&p, 48_000);
        }
        for v in [-24.1, 0.1, 6.0, f32::NAN, f32::INFINITY] {
            let mut p = Profile::default();
            p.limiter.ceiling_db = v;
            rejected(&p, 48_000, "limiter.ceiling_db");
        }
        for v in [0.9, 0.0, -5.0, 2000.5, f32::NAN, f32::INFINITY] {
            let mut p = Profile::default();
            p.limiter.release_ms = v;
            rejected(&p, 48_000, "limiter.release_ms");
        }
        // The ranges hold with the limiter off too, so turning it on never
        // makes a saved profile invalid.
        let mut p = Profile::default();
        p.limiter.enabled = false;
        p.limiter.ceiling_db = 1.0;
        rejected(&p, 48_000, "limiter.ceiling_db");
    }

    #[test]
    fn a_profile_without_a_limiter_table_gets_the_default_limiter() {
        let old: Profile =
            toml::from_str("output_gain_db = -3.5\n[stereo]\nenabled = true\n").unwrap();
        assert_eq!(old.limiter, LimiterParams::default());
        assert_eq!(
            LimiterParams::default(),
            LimiterParams {
                enabled: true,
                ceiling_db: -1.0,
                release_ms: 100.0,
            }
        );
        let partial: Profile = toml::from_str("[limiter]\nenabled = false\n").unwrap();
        assert_eq!(
            partial.limiter,
            LimiterParams {
                enabled: false,
                ..LimiterParams::default()
            }
        );
        let typo = toml::from_str::<Profile>("[limiter]\nceiling = -2.0\n").unwrap_err();
        assert!(typo.to_string().contains("ceiling"), "{typo}");
    }

    #[test]
    fn crossover_must_be_below_nyquist() {
        // A 1 kHz rate: every other value is moved inside its limits so only
        // the crossover can fail.
        let mut p = Profile {
            shelf_corner_hz: 100.0,
            ..Profile::default()
        };
        p.eq.freqs_hz = [
            10.0, 20.0, 30.0, 40.0, 60.0, 80.0, 100.0, 150.0, 200.0, 300.0, 400.0,
        ];
        for v in [20.0, 499.0] {
            p.stereo.crossover_hz = v;
            accepted(&p, 1000);
        }
        for v in [500.0, 800.0] {
            p.stereo.crossover_hz = v;
            rejected(&p, 1000, "stereo.crossover_hz");
        }
    }
}
