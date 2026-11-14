//! Output device correction, after crossfeed and before the limiter: a
//! preamp and up to `MAX_CORRECTION_BANDS` peaking and shelving filters
//! measured for the output device, such as an imported headphone profile.
//! Bands at or above Nyquist are skipped, like EQ bands.

use crate::biquad::{Biquad, Coefs};
use crate::profile::{BandKind, CorrectionBand, CorrectionParams};

pub struct Correction {
    preamp: f32,
    /// The same filters, one cascade per channel.
    channels: Vec<Vec<Biquad>>,
}

impl Correction {
    pub fn new(params: &CorrectionParams, sample_rate: f64, channels: usize) -> Self {
        let cascade = || {
            params
                .bands
                .iter()
                .filter(|band| !band.is_skipped(sample_rate))
                .map(|band| Biquad::new(coefs(band, sample_rate)))
                .collect()
        };
        Self {
            preamp: 10f64.powf(f64::from(params.preamp_db) / 20.0) as f32,
            channels: (0..channels).map(|_| cascade()).collect(),
        }
    }

    /// One interleaved frame, all channels, in place. No allocation.
    pub fn frame(&mut self, frame: &mut [f32]) {
        for (x, cascade) in frame.iter_mut().zip(&mut self.channels) {
            *x = cascade
                .iter_mut()
                .fold(*x * self.preamp, |y, filter| filter.tick(y));
        }
    }

    /// Starts where `from` is, when it has the same filters. No allocation.
    pub fn adopt(&mut self, from: &Self) {
        let alike = self.channels.len() == from.channels.len()
            && self
                .channels
                .iter()
                .zip(&from.channels)
                .all(|(to, from)| to.len() == from.len());
        if !alike {
            return;
        }
        for (to, from) in self.channels.iter_mut().zip(&from.channels) {
            for (to, from) in to.iter_mut().zip(from) {
                to.adopt(from);
            }
        }
    }
}

fn coefs(band: &CorrectionBand, sample_rate: f64) -> Coefs {
    let (freq, q, gain) = (
        f64::from(band.freq_hz),
        f64::from(band.q),
        f64::from(band.gain_db),
    );
    match band.kind {
        BandKind::Peaking => Coefs::peaking(freq, q, gain, sample_rate),
        BandKind::LowShelf => Coefs::low_shelf(freq, q, gain, sample_rate),
        BandKind::HighShelf => Coefs::high_shelf(freq, q, gain, sample_rate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stereo::tests::amplitude;
    use std::f64::consts::PI;

    const FS: f64 = 48_000.0;

    fn band(kind: BandKind, freq_hz: f32, q: f32, gain_db: f32) -> CorrectionBand {
        CorrectionBand {
            kind,
            freq_hz,
            q,
            gain_db,
        }
    }

    fn params(preamp_db: f32, bands: Vec<CorrectionBand>) -> CorrectionParams {
        CorrectionParams {
            enabled: true,
            preamp_db,
            bands,
        }
    }

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

    fn run(correction: &mut Correction, channels: usize, samples: &[f32]) -> Vec<f32> {
        let mut out = samples.to_vec();
        for frame in out.chunks_exact_mut(channels) {
            correction.frame(frame);
        }
        out
    }

    /// The stage's gain in dB at `freq_hz`, from a sine of whole cycles
    /// measured over the second after the first has let it settle. `freq_hz`
    /// must be a whole number of Hz and `fs` too.
    fn gain_db(params: &CorrectionParams, fs: f64, freq_hz: f64) -> f64 {
        let mut correction = Correction::new(params, fs, 1);
        let n = fs as usize;
        let sine: Vec<f32> = (0..2 * n)
            .map(|i| (0.5 * (2.0 * PI * freq_hz * i as f64 / fs).sin()) as f32)
            .collect();
        let out = run(&mut correction, 1, &sine);
        20.0 * (amplitude(&out[n..], freq_hz, fs) / 0.5).log10()
    }

    fn assert_db(got: f64, want: f64, tol: f64, what: &str) {
        assert!((got - want).abs() < tol, "{what}: {got} dB, want {want} dB");
    }

    #[test]
    fn no_bands_and_no_preamp_is_bit_for_bit_identity() {
        for channels in [1, 2, 6] {
            let input = noise(channels * 5000, 7);
            let mut correction = Correction::new(&params(0.0, vec![]), FS, channels);
            assert_eq!(run(&mut correction, channels, &input), input, "{channels}");
        }
    }

    #[test]
    fn the_preamp_alone_scales_the_signal() {
        let input = noise(4000, 3);
        // 20 log10(0.5): exactly half, to f32 rounding.
        let mut half = Correction::new(&params(-6.020_6, vec![]), FS, 1);
        for (got, x) in run(&mut half, 1, &input).iter().zip(&input) {
            assert!((got - x * 0.5).abs() <= 1e-6 * x.abs(), "{got} for {x}");
        }
        // -6 dB is 0.5012.
        let mut six = Correction::new(&params(-6.0, vec![]), FS, 1);
        for (got, x) in run(&mut six, 1, &input).iter().zip(&input) {
            let want = x * 10f32.powf(-6.0 / 20.0);
            assert!((got - want).abs() <= 1e-6 * x.abs(), "{got} for {x}");
            assert!((got - x * 0.5).abs() <= 0.0013 * x.abs(), "{got} for {x}");
        }
        assert_db(
            gain_db(&params(6.0, vec![]), FS, 1000.0),
            6.0,
            1e-4,
            "+6 dB",
        );
    }

    #[test]
    fn a_peaking_band_raises_its_centre_and_leaves_far_frequencies() {
        let p = params(0.0, vec![band(BandKind::Peaking, 1000.0, 1.41, 6.0)]);
        assert_db(gain_db(&p, FS, 1000.0), 6.0, 0.05, "1 kHz");
        assert_db(gain_db(&p, FS, 100.0), 0.0, 0.3, "100 Hz");
        assert_db(gain_db(&p, FS, 10_000.0), 0.0, 0.3, "10 kHz");
    }

    #[test]
    fn shelves_match_their_design_response() {
        let cases = [
            (
                band(BandKind::LowShelf, 200.0, 0.8, 5.0),
                Coefs::low_shelf(200.0, 0.8, 5.0, FS),
                [50.0, 200.0, 2000.0],
            ),
            (
                band(BandKind::HighShelf, 5000.0, 0.7, -4.0),
                Coefs::high_shelf(5000.0, 0.7, -4.0, FS),
                [1000.0, 5000.0, 15_000.0],
            ),
        ];
        for (band, coefs, freqs) in cases {
            let p = params(0.0, vec![band]);
            for f in freqs {
                assert_db(
                    gain_db(&p, FS, f),
                    coefs.magnitude_db(f, FS),
                    0.05,
                    &format!("{:?} at {f} Hz", band.kind),
                );
            }
        }
    }

    #[test]
    fn bands_and_the_preamp_compose() {
        let bands = vec![
            band(BandKind::LowShelf, 90.0, 0.7, 4.0),
            band(BandKind::Peaking, 250.0, 1.2, -3.0),
            band(BandKind::Peaking, 1000.0, 2.0, 5.0),
            band(BandKind::Peaking, 3500.0, 3.0, -4.5),
            band(BandKind::HighShelf, 9000.0, 0.7, 3.0),
        ];
        let p = params(-3.0, bands.clone());
        for f in [60.0, 250.0, 800.0, 1000.0, 3500.0, 7000.0, 14_000.0] {
            let want = -3.0
                + bands
                    .iter()
                    .map(|b| coefs(b, FS).magnitude_db(f, FS))
                    .sum::<f64>();
            assert_db(gain_db(&p, FS, f), want, 0.1, &format!("{f} Hz"));
        }
    }

    #[test]
    fn a_band_at_or_above_nyquist_is_skipped_and_the_rest_still_act() {
        let fs = 22_050.0;
        let working = band(BandKind::Peaking, 1000.0, 1.41, 6.0);
        let skipped = [
            band(BandKind::Peaking, 16_000.0, 2.0, 9.0),
            band(BandKind::HighShelf, 11_025.0, 0.7, -9.0),
            band(BandKind::LowShelf, f32::NAN, 0.7, 9.0),
        ];
        let mut bands = skipped.to_vec();
        bands.insert(1, working);
        let p = params(0.0, bands);
        assert_eq!(Correction::new(&p, fs, 2).channels[1].len(), 1);

        assert_db(gain_db(&p, fs, 1000.0), 6.0, 0.05, "1 kHz");
        let input = noise(3000, 5);
        let only = params(0.0, vec![working]);
        assert_eq!(
            run(&mut Correction::new(&p, fs, 1), 1, &input),
            run(&mut Correction::new(&only, fs, 1), 1, &input)
        );
        // The same band is not skipped at a rate with room for it.
        let high = params(0.0, vec![skipped[0]]);
        assert_eq!(Correction::new(&high, fs, 1).channels[0].len(), 0);
        assert_eq!(Correction::new(&high, 44_100.0, 1).channels[0].len(), 1);
    }

    #[test]
    fn adopt_continues_from_the_other_stage() {
        let p = params(
            -2.0,
            vec![
                band(BandKind::LowShelf, 120.0, 0.7, 6.0),
                band(BandKind::Peaking, 700.0, 1.5, -8.0),
            ],
        );
        for channels in [1, 2, 6] {
            let input = noise(channels * 6000, 11);
            let (first, second) = input.split_at(channels * 2000);
            let mut a = Correction::new(&p, FS, channels);
            run(&mut a, channels, first);
            let mut b = Correction::new(&p, FS, channels);
            b.adopt(&a);
            let mut cold = Correction::new(&p, FS, channels);
            let (from_a, from_b) = (run(&mut a, channels, second), run(&mut b, channels, second));
            assert_eq!(from_a, from_b, "{channels} channels");
            assert_ne!(from_a, run(&mut cold, channels, second), "{channels}");
        }
    }

    #[test]
    fn adopt_does_nothing_unless_the_filters_match() {
        let p = params(0.0, vec![band(BandKind::Peaking, 700.0, 1.5, -8.0)]);
        let two_bands = params(
            0.0,
            vec![
                band(BandKind::Peaking, 700.0, 1.5, -8.0),
                band(BandKind::HighShelf, 6000.0, 0.7, 3.0),
            ],
        );
        let input = noise(2 * 3000, 13);
        for mut from in [
            Correction::new(&p, FS, 1),
            Correction::new(&two_bands, FS, 2),
            Correction::new(&p, FS, 3),
        ] {
            let channels = from.channels.len();
            let warm = noise(channels * 2000, 17);
            run(&mut from, channels, &warm);
            let mut to = Correction::new(&p, FS, 2);
            to.adopt(&from);
            let mut cold = Correction::new(&p, FS, 2);
            assert_eq!(
                run(&mut to, 2, &input),
                run(&mut cold, 2, &input),
                "{channels} channels"
            );
        }
    }

    #[test]
    fn every_channel_gets_the_same_response_on_its_own() {
        let p = params(-1.0, vec![band(BandKind::Peaking, 1000.0, 1.41, 6.0)]);
        let want = gain_db(&p, FS, 1000.0);
        assert_db(want, 5.0, 0.05, "mono");
        for channels in [1, 2, 6] {
            // Channel c carries a 1 kHz sine of amplitude 0.05 * (c + 1) ...
            let sine = |c: usize, i: usize| {
                (0.05 * (c + 1) as f64 * (2.0 * PI * 1000.0 * i as f64 / FS).sin()) as f32
            };
            let input: Vec<f32> = (0..2 * 48_000)
                .flat_map(|i| (0..channels).map(move |c| sine(c, i)))
                .collect();
            let out = run(&mut Correction::new(&p, FS, channels), channels, &input);
            for c in 0..channels {
                let own: Vec<f32> = out[48_000 * channels..]
                    .iter()
                    .skip(c)
                    .step_by(channels)
                    .copied()
                    .collect();
                let level = 0.05 * (c + 1) as f64;
                let got = 20.0 * (amplitude(&own, 1000.0, FS) / level).log10();
                assert_db(
                    got,
                    want,
                    0.001,
                    &format!("{channels} channels, channel {c}"),
                );
            }
        }
        // ... and a silent channel stays silent beside a loud one.
        let mut stereo = Correction::new(&p, FS, 2);
        for i in 0..1000 {
            let mut frame = [(0.5 * (i as f32 * 0.1).sin()), 0.0];
            stereo.frame(&mut frame);
            assert_eq!(frame[1], 0.0);
        }
    }
}
