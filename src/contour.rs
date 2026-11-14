//! Loudness contour, after bass harmonics: more bass and treble as the system
//! volume goes down, for the ear's loss of both at low levels. Live only:
//! without a known volume it adds nothing.
//!
//! A low shelf at `BASS_HZ` and a high shelf at `TREBLE_HZ` on every channel.
//! Each gain is `*_per_db` for every dB the volume sits below `reference_db`,
//! up to `max_*_db`. The gains glide to their targets at `GLIDE_DB_PER_S`,
//! re-aimed every `UPDATE_FRAMES` frames, so a volume step is no click and
//! the coefficients' trigonometry stays off the per-sample path. A gain of
//! exactly 0 runs an identity filter, which passes audio bit for bit: a 0 dB
//! shelf is identity only to within rounding.
//!
//! The treble shelf stays identity at any gain when its corner is at or above
//! 0.45 x the sample rate: too close to Nyquist to be meaningful.

use crate::biquad::{Biquad, Coefs};
use crate::profile::ContourParams;
use std::f64::consts::FRAC_1_SQRT_2;

const BASS_HZ: f64 = 120.0;
const TREBLE_HZ: f64 = 8000.0;
/// The steepest shelf slope without a bump.
const Q: f64 = FRAC_1_SQRT_2;
const GLIDE_DB_PER_S: f64 = 20.0;
const UPDATE_FRAMES: u32 = 64;

pub struct Contour {
    params: ContourParams,
    sample_rate: f64,
    low: Vec<Biquad>,
    high: Vec<Biquad>,
    target_bass_db: f64,
    target_treble_db: f64,
    bass_db: f64,
    treble_db: f64,
    /// As last set, for a chain that takes over with other settings.
    volume_db: Option<f32>,
    /// Frames since the last update, from 0 to `UPDATE_FRAMES` - 1.
    count: u32,
}

impl Contour {
    pub fn new(params: &ContourParams, sample_rate: f64, channels: usize) -> Self {
        let identity = || {
            (0..channels)
                .map(|_| Biquad::new(Coefs::IDENTITY))
                .collect()
        };
        Self {
            params: *params,
            sample_rate,
            low: identity(),
            high: identity(),
            target_bass_db: 0.0,
            target_treble_db: 0.0,
            bass_db: 0.0,
            treble_db: 0.0,
            volume_db: None,
            count: 0,
        }
    }

    /// One interleaved frame, all channels, in place. No allocation.
    pub fn frame(&mut self, frame: &mut [f32]) {
        if self.count == 0 {
            self.update();
        }
        self.count = (self.count + 1) % UPDATE_FRAMES;
        for (x, (low, high)) in frame
            .iter_mut()
            .zip(self.low.iter_mut().zip(&mut self.high))
        {
            *x = high.tick(low.tick(*x));
        }
    }

    /// The system volume in dB below maximum; `None` when unknown (files,
    /// devices without a volume control), which adds nothing. No allocation.
    pub fn set_volume_db(&mut self, volume_db: Option<f32>) {
        self.volume_db = volume_db;
        let deficit = match volume_db {
            Some(v) if v.is_finite() => {
                (f64::from(self.params.reference_db) - f64::from(v)).max(0.0)
            }
            _ => 0.0,
        };
        let p = &self.params;
        self.target_bass_db = (f64::from(p.bass_per_db) * deficit).min(f64::from(p.max_bass_db));
        self.target_treble_db =
            (f64::from(p.treble_per_db) * deficit).min(f64::from(p.max_treble_db));
    }

    /// Starts where `from` is: its volume, gains and filter state, so the
    /// tone carries on without the volume being sent again, and glides to
    /// what this stage's own settings make of that volume. No allocation.
    pub fn adopt(&mut self, from: &Self) {
        self.set_volume_db(from.volume_db);
        self.bass_db = from.bass_db;
        self.treble_db = from.treble_db;
        self.count = from.count;
        self.set_bass();
        self.set_treble();
        if self.low.len() == from.low.len() {
            for (to, from) in self.low.iter_mut().zip(&from.low) {
                to.adopt(from);
            }
            for (to, from) in self.high.iter_mut().zip(&from.high) {
                to.adopt(from);
            }
        }
    }

    /// Moves each gain a step towards its target and refits the filters
    /// whose gain moved.
    fn update(&mut self) {
        let step = GLIDE_DB_PER_S * f64::from(UPDATE_FRAMES) / self.sample_rate;
        if self.bass_db != self.target_bass_db {
            self.bass_db = glide(self.bass_db, self.target_bass_db, step);
            self.set_bass();
        }
        if self.treble_db != self.target_treble_db {
            self.treble_db = glide(self.treble_db, self.target_treble_db, step);
            self.set_treble();
        }
    }

    fn set_bass(&mut self) {
        let coefs = if self.bass_db == 0.0 {
            Coefs::IDENTITY
        } else {
            Coefs::low_shelf(BASS_HZ, Q, self.bass_db, self.sample_rate)
        };
        for low in &mut self.low {
            low.set_coefs(coefs);
        }
    }

    fn set_treble(&mut self) {
        let coefs = if self.treble_db == 0.0 || TREBLE_HZ >= 0.45 * self.sample_rate {
            Coefs::IDENTITY
        } else {
            Coefs::high_shelf(TREBLE_HZ, Q, self.treble_db, self.sample_rate)
        };
        for high in &mut self.high {
            high.set_coefs(coefs);
        }
    }
}

/// `current` moved towards `target` by at most `step`, landing on it when
/// within a step.
fn glide(current: f64, target: f64, step: f64) -> f64 {
    let gap = target - current;
    if gap.abs() <= step {
        target
    } else {
        current + step.copysign(gap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stereo::tests::amplitude;
    use std::f64::consts::PI;

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

    fn sine(freq_hz: f64, fs: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (0.5 * (2.0 * PI * freq_hz * i as f64 / fs).sin()) as f32)
            .collect()
    }

    fn defaults() -> ContourParams {
        ContourParams {
            enabled: true,
            ..ContourParams::default()
        }
    }

    fn process(contour: &mut Contour, samples: &mut [f32], channels: usize) {
        for frame in samples.chunks_exact_mut(channels) {
            contour.frame(frame);
        }
    }

    fn db(ratio: f64) -> f64 {
        20.0 * ratio.log10()
    }

    /// The gain, in dB, at which a mono contour at `volume_db` plays a sine of
    /// `freq_hz` (a whole number of Hz) once its gains have settled.
    fn gain_db(volume_db: Option<f32>, fs: f64, freq_hz: f64) -> f64 {
        let mut contour = Contour::new(&defaults(), fs, 1);
        contour.set_volume_db(volume_db);
        let second = fs as usize;
        let mut x = sine(freq_hz, fs, 3 * second);
        process(&mut contour, &mut x, 1);
        db(amplitude(&x[2 * second..], freq_hz, fs) / 0.5)
    }

    fn assert_db(got: f64, want: f64, tol: f64, what: &str) {
        assert!((got - want).abs() < tol, "{what}: {got} dB, want {want} dB");
    }

    #[test]
    fn unknown_volume_and_volume_at_or_above_reference_pass_audio_bit_for_bit() {
        let input = noise(20_000, 1);
        let mut fresh = input.clone();
        process(&mut Contour::new(&defaults(), FS, 1), &mut fresh, 1);
        assert_eq!(fresh, input, "volume never set");

        let reference = defaults().reference_db;
        for volume in [
            None,
            Some(reference),
            Some(reference + 6.0),
            Some(0.0),
            Some(f32::NAN),
            Some(f32::INFINITY),
            Some(f32::NEG_INFINITY),
        ] {
            let mut contour = Contour::new(&defaults(), FS, 1);
            contour.set_volume_db(volume);
            let mut out = input.clone();
            process(&mut contour, &mut out, 1);
            assert_eq!(out, input, "{volume:?}");
        }
    }

    #[test]
    fn bass_and_treble_come_up_below_the_reference() {
        // 20 dB below the reference: 7 dB of bass and 2 dB of treble.
        let volume = Some(-32.0);
        assert_db(gain_db(volume, FS, 30.0), 7.0, 0.3, "30 Hz");
        assert_db(gain_db(volume, FS, 16_000.0), 2.0, 0.3, "16 kHz");
        assert_db(gain_db(volume, FS, 1_000.0), 0.0, 0.5, "1 kHz");
    }

    #[test]
    fn the_gains_stop_at_their_caps() {
        let volume = Some(-60.0);
        assert_db(gain_db(volume, FS, 30.0), 9.0, 0.3, "30 Hz");
        assert_db(gain_db(volume, FS, 16_000.0), 4.0, 0.3, "16 kHz");
    }

    fn max_step(x: &[f32]) -> f32 {
        x.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn the_gain_glides_in_without_a_click_and_arrives_in_about_half_a_second() {
        let second = FS as usize;
        let step_at = second / 10;

        let mut contour = Contour::new(&defaults(), FS, 1);
        let mut x = sine(100.0, FS, 2 * second);
        process(&mut contour, &mut x[..step_at], 1);
        contour.set_volume_db(Some(-60.0));
        process(&mut contour, &mut x[step_at..], 1);

        let mut settled = Contour::new(&defaults(), FS, 1);
        settled.set_volume_db(Some(-60.0));
        let mut y = sine(100.0, FS, 2 * second);
        process(&mut settled, &mut y, 1);
        let (glide_step, final_step) = (max_step(&x), max_step(&y[second..]));
        assert!(
            glide_step <= 1.5 * final_step,
            "{glide_step} against {final_step}"
        );

        // One 30 Hz cycle is 1600 samples; slide that window along.
        let mut contour = Contour::new(&defaults(), FS, 1);
        let mut x = sine(30.0, FS, 2 * second);
        process(&mut contour, &mut x[..step_at], 1);
        contour.set_volume_db(Some(-60.0));
        process(&mut contour, &mut x[step_at..], 1);
        let final_db = gain_db(Some(-60.0), FS, 30.0);
        let window = 1600;
        let arrival = (step_at..2 * second - window)
            .step_by(160)
            .find(|&s| {
                let got = db(amplitude(&x[s..s + window], 30.0, FS) / 0.5);
                (got - final_db).abs() < 0.5
            })
            .map(|s| (s + window / 2 - step_at) as f64 / FS)
            .expect("the gain never arrives");
        assert!((0.35..0.6).contains(&arrival), "arrives after {arrival} s");
    }

    #[test]
    fn returning_to_unknown_glides_back_to_exact_identity() {
        let mut contour = Contour::new(&defaults(), FS, 1);
        contour.set_volume_db(Some(-60.0));
        let mut warm = sine(100.0, FS, 2 * FS as usize);
        process(&mut contour, &mut warm, 1);
        contour.set_volume_db(None);
        let mut x = noise(2 * FS as usize, 2);
        let input = x.clone();
        process(&mut contour, &mut x, 1);
        // 9 dB at 20 dB/s is done in half a second.
        let from = FS as usize;
        assert_eq!(x[from..], input[from..]);
    }

    #[test]
    fn adopt_continues_where_the_other_left_off() {
        let mut a = Contour::new(&defaults(), FS, 2);
        a.set_volume_db(Some(-32.0));
        let mut warm = noise(2 * FS as usize, 3);
        process(&mut a, &mut warm, 2);
        // A little more, so the counter is part way through an update.
        let mut more = noise(2 * 37, 4);
        process(&mut a, &mut more, 2);

        let mut b = Contour::new(&defaults(), FS, 2);
        b.adopt(&a);
        let input = noise(2 * 10_000, 5);
        let (mut out_a, mut out_b) = (input.clone(), input);
        process(&mut a, &mut out_a, 2);
        process(&mut b, &mut out_b, 2);
        assert_eq!(out_a, out_b);

        // Mid-glide too: the gains carry on from where A's are.
        a.set_volume_db(Some(-60.0));
        let mut warm = noise(2 * 5_000, 6);
        process(&mut a, &mut warm, 2);
        let mut c = Contour::new(&defaults(), FS, 2);
        c.adopt(&a);
        let input = noise(2 * 20_000, 7);
        let (mut out_a, mut out_c) = (input.clone(), input);
        process(&mut a, &mut out_a, 2);
        process(&mut c, &mut out_c, 2);
        assert_eq!(out_a, out_c);
    }

    #[test]
    fn adopt_aims_at_its_own_settings_for_the_volume_it_takes_over() {
        let mut a = Contour::new(&ContourParams::default(), FS, 1);
        a.set_volume_db(Some(-32.0));
        let capped = ContourParams {
            max_bass_db: 3.0,
            ..ContourParams::default()
        };
        let mut b = Contour::new(&capped, FS, 1);
        b.adopt(&a);
        assert_eq!(b.target_bass_db, 3.0);
        let mut alone = Contour::new(&capped, FS, 1);
        alone.set_volume_db(Some(-32.0));
        assert_eq!(b.target_treble_db, alone.target_treble_db);
    }

    #[test]
    fn adopt_across_channel_counts_takes_the_tone_but_not_the_state() {
        let mut a = Contour::new(&defaults(), FS, 2);
        a.set_volume_db(Some(-32.0));
        let mut warm = noise(2 * FS as usize, 8);
        process(&mut a, &mut warm, 2);
        let mut b = Contour::new(&defaults(), FS, 1);
        b.adopt(&a);
        let mut out = sine(30.0, FS, 3 * FS as usize);
        process(&mut b, &mut out, 1);
        let got = db(amplitude(&out[2 * FS as usize..], 30.0, FS) / 0.5);
        assert_db(got, 7.0, 0.3, "30 Hz");
    }

    #[test]
    fn works_for_any_number_of_channels() {
        for channels in [1, 2, 6] {
            let mut contour = Contour::new(&defaults(), FS, channels);
            contour.set_volume_db(Some(-32.0));
            let second = FS as usize;
            let mono = sine(30.0, FS, 3 * second);
            let mut x: Vec<f32> = mono.iter().flat_map(|v| vec![*v; channels]).collect();
            process(&mut contour, &mut x, channels);
            for ch in 0..channels {
                let out: Vec<f32> = x.chunks_exact(channels).map(|f| f[ch]).collect();
                let got = db(amplitude(&out[2 * second..], 30.0, FS) / 0.5);
                assert_db(got, 7.0, 0.3, &format!("{channels} channels, channel {ch}"));
            }
        }
    }

    #[test]
    fn the_treble_shelf_is_skipped_too_close_to_nyquist() {
        // At 16 kHz the 8 kHz corner is past 0.45 of the sample rate.
        let fs = 16_000.0;
        let bass = Coefs::low_shelf(BASS_HZ, Q, 7.0, fs).magnitude_db(30.0, fs);
        assert_db(gain_db(Some(-32.0), fs, 30.0), bass, 0.05, "30 Hz");
        assert_db(gain_db(Some(-32.0), fs, 7_000.0), 0.0, 0.05, "7 kHz");
        let mut contour = Contour::new(&defaults(), fs, 1);
        contour.set_volume_db(Some(-60.0));
        let mut x = noise(3 * fs as usize, 9);
        process(&mut contour, &mut x, 1);
        assert!(x.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn works_at_a_high_sample_rate() {
        let fs = 192_000.0;
        let mut contour = Contour::new(&defaults(), fs, 2);
        contour.set_volume_db(Some(-60.0));
        let mut x = noise(2 * 3 * fs as usize, 10);
        process(&mut contour, &mut x, 2);
        assert!(x.iter().all(|v| v.is_finite()));
        let bass = Coefs::low_shelf(BASS_HZ, Q, 9.0, fs).magnitude_db(30.0, fs);
        assert_db(gain_db(Some(-60.0), fs, 30.0), bass, 0.05, "30 Hz");
        let treble = Coefs::high_shelf(TREBLE_HZ, Q, 4.0, fs).magnitude_db(16_000.0, fs);
        assert_db(gain_db(Some(-60.0), fs, 16_000.0), treble, 0.05, "16 kHz");
    }

    #[test]
    fn output_does_not_depend_on_how_the_stream_is_split() {
        let channels = 2;
        let frames = 40_000;
        let input = noise(channels * frames, 11);
        // Volume changes at frame 1000 and 20 000.
        let changes = [(1_000, Some(-40.0)), (20_000, None)];

        let mut by_frame = Contour::new(&defaults(), FS, channels);
        let mut want = input.clone();
        for (i, frame) in want.chunks_exact_mut(channels).enumerate() {
            for (_, volume) in changes.iter().filter(|(at, _)| *at == i) {
                by_frame.set_volume_db(*volume);
            }
            by_frame.frame(frame);
        }

        // Blocks of uneven size, cut at the changes.
        let mut by_block = Contour::new(&defaults(), FS, channels);
        let mut got = input;
        let mut cuts = vec![0, 1_000, 20_000, frames];
        cuts.extend([7, 333, 4_099, 12_345, 33_333]);
        cuts.sort_unstable();
        for pair in cuts.windows(2) {
            for (_, volume) in changes.iter().filter(|(at, _)| *at == pair[0]) {
                by_block.set_volume_db(*volume);
            }
            process(
                &mut by_block,
                &mut got[channels * pair[0]..channels * pair[1]],
                channels,
            );
        }
        assert_eq!(got, want);
    }
}
