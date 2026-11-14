//! A profile's static tonal response: what its EQ and device correction do
//! to a steady sine, for drawing the curve while tuning.

use crate::bell::Bell;
use crate::biquad::Coefs;
use crate::profile::{BandKind, EQ_BANDS, Profile};

/// The gain in dB the chain built from `profile` at `sample_rate` gives a
/// steady sine at `freq_hz`, from its tonal stages alone: the EQ bells it
/// plays (not those it skips at the rate), and with correction on, its
/// preamp and the bands it plays.
///
/// Left out: the mirrored shelves, which cancel for a steady sine; the
/// compressors, leveler, limiter, stereo enhancer and loudness contour,
/// whose gain depends on the level or the system volume; bass harmonics
/// and crossfeed, which depend on the material; and the input and output
/// gains, which move the whole curve rather than shape it.
pub fn response_db(profile: &Profile, freq_hz: f64, sample_rate: u32) -> f64 {
    let fs = f64::from(sample_rate);
    let skipped = profile.skipped_eq_bands(sample_rate);
    let eq = &profile.eq;
    let bells: f64 = (0..EQ_BANDS)
        .filter(|i| !skipped.contains(i))
        .map(|i| {
            let (f, q, gain) = (eq.freqs_hz[i], eq.q[i], eq.gains_db[i]);
            Bell::new(f64::from(f), f64::from(q), f64::from(gain), fs).magnitude_db(freq_hz, fs)
        })
        .sum();
    let correction = &profile.correction;
    if !correction.enabled {
        return bells;
    }
    let bands: f64 = correction
        .bands
        .iter()
        .filter(|band| !band.is_skipped(fs))
        .map(|band| {
            let (f, q, gain) = (
                f64::from(band.freq_hz),
                f64::from(band.q),
                f64::from(band.gain_db),
            );
            let coefs = match band.kind {
                BandKind::Peaking => Coefs::peaking(f, q, gain, fs),
                BandKind::LowShelf => Coefs::low_shelf(f, q, gain, fs),
                BandKind::HighShelf => Coefs::high_shelf(f, q, gain, fs),
            };
            coefs.magnitude_db(freq_hz, fs)
        })
        .sum();
    bells + f64::from(correction.preamp_db) + bands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Processor;
    use crate::profile::{CompressorParams, CorrectionBand, LimiterParams};
    use crate::stereo::tests::amplitude;
    use std::f64::consts::PI;

    /// Every stage at unity, the limiter off: only the EQ and correction
    /// shape what passes.
    fn flat() -> Profile {
        let unity = CompressorParams {
            ratio: 1.0,
            ..CompressorParams::default()
        };
        Profile {
            output_gain_db: 0.0,
            low_shelf_db: 0.0,
            high_shelf_db: 0.0,
            compressor1: unity,
            compressor2: unity,
            limiter: LimiterParams {
                enabled: false,
                ..LimiterParams::default()
            },
            ..Profile::default()
        }
    }

    /// The gain in dB the chain gives a whole-cycle sine at `freq_hz`,
    /// measured over the second after the first.
    fn measured(profile: &Profile, freq_hz: f64, sample_rate: u32) -> f64 {
        let fs = f64::from(sample_rate);
        let n = sample_rate as usize;
        let mut x: Vec<f32> = (0..2 * n)
            .map(|i| (0.25 * (2.0 * PI * freq_hz * i as f64 / fs).sin()) as f32)
            .collect();
        Processor::new(profile, sample_rate, 1).process(&mut x);
        20.0 * (amplitude(&x[n..], freq_hz, fs) / 0.25).log10()
    }

    fn band(kind: BandKind, freq_hz: f32, q: f32, gain_db: f32) -> CorrectionBand {
        CorrectionBand {
            kind,
            freq_hz,
            q,
            gain_db,
        }
    }

    #[test]
    fn the_response_is_what_the_chain_does_to_a_sine() {
        let mut p = flat();
        p.eq.gains_db = [4.0, 0.0, -3.0, 0.0, 2.5, 0.0, -6.0, 0.0, 5.0, 0.0, 12.0];
        p.eq.q[4] = 0.7;
        p.eq.q[8] = 3.0;
        // At 44.1 kHz, 16 kHz at Q 0.5 spans 32 kHz, past Nyquist, and the
        // correction's 23 kHz band is past it too: both are skipped, so
        // their boosts must not show.
        p.eq.q[10] = 0.5;
        p.correction.enabled = true;
        p.correction.preamp_db = -2.0;
        p.correction.bands = vec![
            band(BandKind::LowShelf, 120.0, 0.7, 3.0),
            band(BandKind::Peaking, 3000.0, 2.0, -4.0),
            band(BandKind::HighShelf, 9000.0, 0.7, 2.0),
            band(BandKind::Peaking, 23_000.0, 1.0, 9.0),
        ];
        let rate = 44_100;
        assert_eq!(p.validate(rate), Ok(()));
        assert_eq!(p.skipped_eq_bands(rate), [10]);
        assert_eq!(p.skipped_correction_bands(rate), [3]);
        let mut largest = 0.0f64;
        for f in [
            25.0, 60.0, 100.0, 220.0, 440.0, 1000.0, 1500.0, 3000.0, 5000.0, 8000.0, 12_000.0,
            18_000.0,
        ] {
            let want = response_db(&p, f, rate);
            let got = measured(&p, f, rate);
            assert!(
                (got - want).abs() < 0.1,
                "{f} Hz: the chain gives {got:.3} dB, the response says {want:.3} dB"
            );
            largest = largest.max(want.abs());
        }
        // The curve really moves, by more than the tolerance.
        assert!(largest > 4.0, "{largest}");
    }

    #[test]
    fn a_flat_profile_is_flat_and_correction_counts_only_when_on() {
        let mut p = flat();
        for f in [20.0, 1000.0, 20_000.0] {
            assert!(response_db(&p, f, 48_000).abs() < 1e-9);
        }
        p.correction.preamp_db = -3.0;
        p.correction.bands = vec![band(BandKind::Peaking, 1000.0, 1.0, 6.0)];
        assert!(response_db(&p, 1000.0, 48_000).abs() < 1e-9);
        p.correction.enabled = true;
        assert!((response_db(&p, 1000.0, 48_000) - 3.0).abs() < 1e-9);
        // A single bell is its own magnitude.
        let mut p = flat();
        p.eq.gains_db[6] = 12.0;
        let bell = Bell::new(1327.0, 1.5, 12.0, 48_000.0);
        for f in [100.0, 1327.0, 5000.0] {
            let want = bell.magnitude_db(f, 48_000.0);
            assert!((response_db(&p, f, 48_000) - want).abs() < 1e-9);
        }
    }
}
