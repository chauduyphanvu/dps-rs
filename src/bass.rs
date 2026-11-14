//! Bass harmonics, after the per-channel chain: adds harmonics of the
//! fundamentals below a cutoff, which a small speaker cannot play, so the
//! ear still hears their pitch (the missing fundamental); optionally
//! high-passes what the speaker cannot play.
//!
//! In each channel the band below the cutoff (6th-order Butterworth
//! low-pass) feeds two allpass chains whose outputs p and q are 90 degrees
//! apart, the analytic signal p + i q = r e^(i phi), and r cos(n phi) is the
//! nth harmonic of a sine at the sine's own level. So the harmonics keep
//! their ratio to the fundamental at any input level, with no envelope
//! follower to lag or ripple, where a waveshaper's grow with the level and a
//! rectifier's are even only (heard an octave up). Harmonics 2 to 6 are made,
//! at 2/n like a sawtooth's (the series of a plucked or bowed string), pass a
//! band from the cutoff (4th-order Butterworth high-pass) to 4 x cutoff
//! (2nd order) and are added to the input, which is otherwise untouched and
//! not delayed.
//!
//! `harmonics_db` = 0: a steady sine an octave below the cutoff gets
//! harmonics whose combined RMS equals its own.
//!
//! Per channel rather than from a mono sum: bass panned or out of phase
//! between channels keeps its harmonics instead of cancelling in the sum,
//! and mono bass gets the same harmonics in every channel either way.
//!
//! The low-pass has a tone two octaves above the cutoff 72 dB down before it
//! can make harmonics. Nothing is made above the 6th harmonic, unlike a
//! rectifier's or clipper's endless series, so at 8 kHz with a 250 Hz cutoff
//! the harmonics of fundamentals up to 2.6 x cutoff stay under Nyquist. At
//! rates where a harmonic of a fundamental at the cutoff would pass fs / 4
//! (below 6 kHz for 250 Hz), that harmonic is left out.
//!
//! `remove_lows` high-passes the input at the cutoff, 4th-order Butterworth:
//! a sine an octave below it comes out 24 dB down, where 2nd order would
//! give 12 dB, and steeper would ring more for little over the speaker's own
//! roll-off. The high-pass runs with `remove_lows` off too, so a chain that
//! takes over with it on starts from a running state.

use crate::biquad::{Biquad, Coefs};
use crate::profile::BassParams;
use std::f64::consts::{FRAC_1_SQRT_2, PI};

const MAX_HARMONIC: usize = 6;

/// Phase of each harmonic, by its number, in radians: a minimax design for
/// the lowest peak of one sine's harmonics after the band filter, 5.3 dB
/// above their RMS for fundamentals from cutoff / 4 to the cutoff, against
/// 8.8 dB with all in cosine phase.
const PHASES: [f64; MAX_HARMONIC + 1] = [0.0, 0.0, 0.0, 2.5018, 0.6183, 6.0649, 0.7755];

/// Poles of the two allpass chains, as multiples of the cutoff on the
/// prewarped axis tan(pi f / fs): p leads q by 90 degrees, within 0.4
/// degrees from cutoff / 8 to 2 x cutoff (a minimax design).
const CHAIN_P: [f64; 3] = [0.1783, 0.9292, 7.136];
const CHAIN_Q: [f64; 3] = [0.04906, 0.4207, 2.091];

/// Far above any audio, and low enough that no filter here can take a
/// sample past f32::MAX (`Biquad` hands f32 from section to section), which
/// would poison its state.
const BOUND: f32 = 1e20;

/// Q of each biquad of a Butterworth filter of order 2 N.
fn butterworth<const N: usize>() -> [f64; N] {
    std::array::from_fn(|k| 1.0 / (2.0 * ((2 * k + 1) as f64 * PI / (4 * N) as f64).cos()))
}

fn cascade(filters: &mut [Biquad], x: f32) -> f32 {
    filters.iter_mut().fold(x, |x, f| f.tick(x))
}

fn gain_at(filters: &[Coefs], freq_hz: f64, sample_rate: f64) -> f64 {
    let db: f64 = filters
        .iter()
        .map(|c| c.magnitude_db(freq_hz, sample_rate))
        .sum();
    10f64.powf(db / 20.0)
}

/// First-order allpass, -90 degrees at `t` on the prewarped axis. State in
/// f64: the lowest pole, at cutoff / 20, is within 2e-5 of z = 1 at 768 kHz.
struct Allpass {
    a: f64,
    s: f64,
}

impl Allpass {
    fn new(t: f64) -> Self {
        Self {
            a: (t - 1.0) / (t + 1.0),
            s: 0.0,
        }
    }

    fn tick(&mut self, x: f64) -> f64 {
        let y = self.a * x + self.s;
        self.s = x - self.a * y;
        y
    }
}

/// The level of each harmonic r cos(n phi + phase) of the analytic signal
/// p + i q = r e^(i phi), split into its cos(n phi) and sin(n phi) parts.
struct Weights {
    cos: [f64; MAX_HARMONIC + 1],
    sin: [f64; MAX_HARMONIC + 1],
}

impl Weights {
    /// r cos(n phi) = r T_n(cos phi) and r sin(n phi) = q U_(n-1)(cos phi),
    /// with T and U the Chebyshev polynomials.
    fn generate(&self, p: f64, q: f64) -> f64 {
        let r = (p * p + q * q).sqrt();
        if r == 0.0 {
            return 0.0;
        }
        let c = p / r;
        let (mut t0, mut t1) = (1.0, c);
        let (mut u0, mut u1) = (0.0, 1.0);
        let (mut cos, mut sin) = (0.0, 0.0);
        for n in 2..=MAX_HARMONIC {
            (t0, t1) = (t1, 2.0 * c * t1 - t0);
            (u0, u1) = (u1, 2.0 * c * u1 - u0);
            cos += self.cos[n] * t1;
            sin += self.sin[n] * u1;
        }
        r * cos + q * sin
    }
}

struct Channel {
    /// The band harmonics are made from.
    low: [Biquad; 3],
    chain_p: [Allpass; 3],
    chain_q: [Allpass; 3],
    /// The band harmonics are kept in.
    band: [Biquad; 3],
    direct_high: [Biquad; 2],
}

impl Channel {
    /// The high-passed input and the harmonics.
    fn tick(&mut self, x: f32, weights: &Weights) -> (f32, f32) {
        let low = f64::from(cascade(&mut self.low, x));
        let p = self.chain_p.iter_mut().fold(low, |x, f| f.tick(x));
        let q = self.chain_q.iter_mut().fold(low, |x, f| f.tick(x));
        let harmonics = cascade(&mut self.band, weights.generate(p, q) as f32);
        (cascade(&mut self.direct_high, x), harmonics)
    }

    fn adopt(&mut self, from: &Self) {
        let biquads = self
            .low
            .iter_mut()
            .chain(&mut self.band)
            .chain(&mut self.direct_high);
        let from_biquads = from.low.iter().chain(&from.band).chain(&from.direct_high);
        for (to, from) in biquads.zip(from_biquads) {
            to.adopt(from);
        }
        let allpasses = self.chain_p.iter_mut().chain(&mut self.chain_q);
        for (to, from) in allpasses.zip(from.chain_p.iter().chain(&from.chain_q)) {
            to.s = from.s;
        }
    }
}

pub struct Bass {
    weights: Weights,
    remove_lows: bool,
    channels: Vec<Channel>,
}

impl Bass {
    pub fn new(params: &BassParams, sample_rate: f64, channels: usize) -> Self {
        let fs = sample_rate;
        let cutoff = f64::from(params.cutoff_hz);
        let low = butterworth::<3>().map(|q| Coefs::low_pass(cutoff, q, fs));
        let [high1, high2] = butterworth::<2>().map(|q| Coefs::high_pass(cutoff, q, fs));
        let top = (4.0 * cutoff).min(fs / 4.0);
        let band = [high1, high2, Coefs::low_pass(top, FRAC_1_SQRT_2, fs)];

        let f0 = cutoff / 2.0;
        let mut levels = [0.0; MAX_HARMONIC + 1];
        let mut power = 0.0;
        for (n, level) in levels.iter_mut().enumerate().skip(2) {
            if n as f64 * cutoff > fs / 4.0 {
                break;
            }
            *level = 2.0 / n as f64;
            power += (*level * gain_at(&band, n as f64 * f0, fs)).powi(2);
        }
        let scale = if power > 0.0 {
            10f64.powf(f64::from(params.harmonics_db) / 20.0)
                / (gain_at(&low, f0, fs) * power.sqrt())
        } else {
            0.0
        };
        let weights = Weights {
            cos: std::array::from_fn(|n| scale * levels[n] * PHASES[n].cos()),
            sin: std::array::from_fn(|n| -scale * levels[n] * PHASES[n].sin()),
        };

        let k = (PI * cutoff / fs).tan();
        Self {
            weights,
            remove_lows: params.remove_lows,
            channels: (0..channels)
                .map(|_| Channel {
                    low: low.map(Biquad::new),
                    chain_p: CHAIN_P.map(|m| Allpass::new(k * m)),
                    chain_q: CHAIN_Q.map(|m| Allpass::new(k * m)),
                    band: band.map(Biquad::new),
                    direct_high: [high1, high2].map(Biquad::new),
                })
                .collect(),
        }
    }

    /// One interleaved frame, all channels, in place. No allocation.
    pub fn frame(&mut self, frame: &mut [f32]) {
        for (x, channel) in frame.iter_mut().zip(&mut self.channels) {
            // Left in place for the caller to act on; the filters get silence.
            if !x.is_finite() {
                channel.tick(0.0, &self.weights);
                continue;
            }
            let (high, harmonics) = channel.tick(x.clamp(-BOUND, BOUND), &self.weights);
            *x = if self.remove_lows { high } else { *x } + harmonics;
        }
    }

    /// Starts where `from` is. No allocation.
    pub fn adopt(&mut self, from: &Self) {
        for (to, from) in self.channels.iter_mut().zip(&from.channels) {
            to.adopt(from);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::Processor;
    use crate::profile::Profile;
    use crate::stereo::tests::amplitude;

    const FS: f64 = 48_000.0;

    /// Levels of harmonics 2 to 6 of a sine an octave below the cutoff, in dB
    /// relative to the sine, at `harmonics_db` = 0.
    const PARTIALS_DB: [f64; 5] = [-4.40, -5.17, -7.69, -9.95, -12.13];

    fn params(cutoff_hz: f64, harmonics_db: f32, remove_lows: bool) -> BassParams {
        BassParams {
            enabled: true,
            cutoff_hz: cutoff_hz as f32,
            harmonics_db,
            remove_lows,
        }
    }

    fn sine(freq_hz: f64, amp: f64, seconds: f64, fs: f64) -> Vec<f32> {
        (0..(seconds * fs) as usize)
            .map(|i| (amp * (2.0 * PI * freq_hz * i as f64 / fs).sin()) as f32)
            .collect()
    }

    fn sum(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(a, b)| a + b).collect()
    }

    fn diff(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(a, b)| a - b).collect()
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

    fn run(bass: &mut Bass, input: &[f32], channels: usize) -> Vec<f32> {
        let mut out = input.to_vec();
        for frame in out.chunks_exact_mut(channels) {
            bass.frame(frame);
        }
        out
    }

    fn mono(p: &BassParams, fs: f64, input: &[f32]) -> Vec<f32> {
        run(&mut Bass::new(p, fs, 1), input, 1)
    }

    /// Output minus input of a mono stage with `remove_lows` off: the
    /// harmonics.
    fn added(cutoff_hz: f64, harmonics_db: f32, fs: f64, input: &[f32]) -> Vec<f32> {
        diff(
            &mono(&params(cutoff_hz, harmonics_db, false), fs, input),
            input,
        )
    }

    /// The last second, after the filters have settled.
    fn last_second(x: &[f32], fs: f64) -> &[f32] {
        &x[x.len() - fs as usize..]
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn peak(x: &[f32]) -> f64 {
        x.iter().fold(0.0, |m, v| m.max(f64::from(v.abs())))
    }

    fn assert_db(got: f64, want: f64, tol: f64, what: &str) {
        assert!((got - want).abs() < tol, "{what}: {got} dB, want {want} dB");
    }

    /// The amplitude and phase (radians) of the `freq_hz` component
    /// A cos(2 pi f n / fs + phase) of `x`, which must hold a whole number of
    /// its cycles.
    fn phasor(x: &[f64], freq_hz: f64, sample_rate: f64) -> (f64, f64) {
        let w = 2.0 * PI * freq_hz / sample_rate;
        let (mut re, mut im) = (0.0, 0.0);
        for (n, v) in x.iter().enumerate() {
            re += v * (w * n as f64).cos();
            im -= v * (w * n as f64).sin();
        }
        let scale = 2.0 / x.len() as f64;
        ((re * re + im * im).sqrt() * scale, im.atan2(re))
    }

    #[test]
    fn the_allpass_chains_are_90_degrees_apart_from_an_eighth_of_the_cutoff_to_twice_it() {
        for (fs, cutoff) in [(48_000.0, 100.0), (8000.0, 250.0), (768_000.0, 40.0)] {
            let k = (PI * cutoff / fs).tan();
            for i in 0..=8 {
                // Whole hertz, so one second holds whole cycles.
                let f = (cutoff / 8.0 * 2f64.powf(f64::from(i) / 2.0)).round();
                let mut p = CHAIN_P.map(|m| Allpass::new(k * m));
                let mut q = CHAIN_Q.map(|m| Allpass::new(k * m));
                let (mut ps, mut qs) = (Vec::new(), Vec::new());
                for n in 0..2 * fs as usize {
                    let x = (2.0 * PI * f * n as f64 / fs).sin();
                    ps.push(p.iter_mut().fold(x, |x, a| a.tick(x)));
                    qs.push(q.iter_mut().fold(x, |x, a| a.tick(x)));
                }
                let tail = fs as usize..;
                let (amp_p, phase_p) = phasor(&ps[tail.clone()], f, fs);
                let (amp_q, phase_q) = phasor(&qs[tail], f, fs);
                let lead = (phase_p - phase_q).to_degrees().rem_euclid(360.0);
                assert!(
                    (lead - 90.0).abs() < 0.45,
                    "{fs} Hz, {f} Hz: {lead} degrees"
                );
                assert!((amp_p - 1.0).abs() < 1e-6 && (amp_q - 1.0).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn a_sine_an_octave_below_the_cutoff_gets_harmonics_at_its_own_level() {
        for cutoff in [40.0, 100.0, 250.0] {
            let f0 = cutoff / 2.0;
            let x = sine(f0, 0.5, 3.0, FS);
            let y = mono(&params(cutoff, 0.0, false), FS, &x);
            let h = diff(&y, &x);
            let (y, h) = (last_second(&y, FS), last_second(&h, FS));
            let partials: Vec<f64> = (2..=6)
                .map(|n| amplitude(h, f64::from(n) * f0, FS) / 0.5)
                .collect();
            for (n, (got, want)) in (2..).zip(partials.iter().zip(PARTIALS_DB)) {
                assert_db(db(*got), want, 0.02, &format!("{cutoff} Hz, harmonic {n}"));
            }
            let combined = partials.iter().map(|a| a * a).sum::<f64>().sqrt();
            assert_db(db(combined), 0.0, 0.01, "combined");
            // Nothing else is added: all of it is in the partials.
            assert_db(db(rms(h) * 2f64.sqrt() / 0.5), 0.0, 0.01, "all added");
            // The generator's own fundamental is -78 dB.
            assert_db(db(amplitude(y, f0, FS) / 0.5), 0.0, 0.002, "fundamental");
            assert!(db(amplitude(h, f0, FS) / 0.5) < -75.0);
            assert!(db(amplitude(h, 7.0 * f0, FS) / 0.5) < -55.0);
        }
    }

    #[test]
    fn harmonics_db_sets_their_level() {
        let x = sine(50.0, 0.5, 2.0, FS);
        let level = |harmonics_db| rms(last_second(&added(100.0, harmonics_db, FS, &x), FS));
        let reference = level(0.0);
        for harmonics_db in [-24.0, -6.0, 12.0] {
            assert_db(
                db(level(harmonics_db) / reference),
                f64::from(harmonics_db),
                1e-3,
                &format!("{harmonics_db} dB"),
            );
        }
    }

    #[test]
    fn harmonics_keep_their_ratio_to_the_input_at_every_level() {
        // Over 40 dB: each partial of a sine, and all that a sawtooth gets.
        let saw = |amp: f64| -> Vec<f32> {
            (0..2 * FS as usize)
                .map(|i| {
                    let t = 40.0 * i as f64 / FS;
                    (amp * 2.0 * (t - (t + 0.5).floor())) as f32
                })
                .collect()
        };
        let mut reference = None;
        for amp in [1.0, 0.1, 0.01] {
            let h = added(100.0, 0.0, FS, &sine(50.0, amp, 2.0, FS));
            let h = last_second(&h, FS);
            let mut ratios: Vec<f64> = (2..=6)
                .map(|n| db(amplitude(h, f64::from(n) * 50.0, FS) / amp))
                .collect();
            let x = saw(amp);
            let h = added(100.0, 0.0, FS, &x);
            ratios.push(db(rms(last_second(&h, FS)) / rms(last_second(&x, FS))));
            let reference = reference.get_or_insert_with(|| ratios.clone());
            for (got, want) in ratios.iter().zip(reference.iter()) {
                assert_db(*got, *want, 1e-3, &format!("amplitude {amp}"));
            }
        }
    }

    #[test]
    fn adds_no_dc() {
        // The band's high-pass has a zero at DC.
        let two_tones = sum(&sine(40.0, 0.3, 2.0, FS), &sine(70.0, 0.3, 2.0, FS));
        let pulses: Vec<f32> = (0..2 * FS as usize)
            .map(|i| if i % 1200 < 100 { 0.8 } else { 0.0 })
            .collect();
        for x in [sine(50.0, 0.5, 2.0, FS), two_tones, pulses] {
            let h = added(100.0, 12.0, FS, &x);
            let h = last_second(&h, FS);
            let mean = h.iter().map(|v| f64::from(*v)).sum::<f64>() / h.len() as f64;
            assert!(mean.abs() < 1e-6 * rms(h), "mean {mean}, RMS {}", rms(h));
        }
        // A constant in: nothing added once settled, and nothing out at all
        // with the lows removed.
        let dc = vec![0.5f32; 2 * FS as usize];
        assert!(peak(last_second(&added(100.0, 12.0, FS, &dc), FS)) < 1e-9);
        let out = mono(&params(100.0, 12.0, true), FS, &dc);
        assert!(peak(last_second(&out, FS)) < 1e-9);
    }

    #[test]
    fn a_tone_above_the_cutoff_gets_nothing() {
        // 1 kHz is 3.3 octaves above the default cutoff (measured -144 dB,
        // the rounding of the output) and two above the highest (-86 dB, and
        // -74 dB with harmonics_db at its +12 dB maximum).
        for (cutoff, harmonics_db, limit) in [
            (100.0, 0.0, -130.0),
            (100.0, 12.0, -120.0),
            (250.0, 0.0, -84.0),
            (250.0, 12.0, -72.0),
        ] {
            let x = sine(1000.0, 0.5, 2.0, FS);
            let h = added(cutoff, harmonics_db, FS, &x);
            let got = db(rms(last_second(&h, FS)) / rms(last_second(&x, FS)));
            assert!(got < limit, "{cutoff} Hz, {harmonics_db} dB: {got} dB");
        }
    }

    #[test]
    fn two_tones_intermodulate_inside_the_harmonics_band() {
        // Tones on a 10 Hz grid put every product on it too, so one second
        // holds whole cycles of all of them. Measured, as a share of the
        // power added: 99.1% between the cutoff and 4 x cutoff for 40 + 70 Hz
        // (-20.5 dB below, mostly the 2nd harmonic of 40 Hz; -45 dB above),
        // 99.8% for 50 + 80 Hz (-38 dB below; -27 dB above, the 6th harmonic
        // of 80 Hz), 99.95% for 40 + 110 Hz at a 150 Hz cutoff.
        for (cutoff, f1, f2, inside_db, below_db, above_db) in [
            (100.0, 40.0, 70.0, -0.05, -20.0, -44.0),
            (100.0, 50.0, 80.0, -0.02, -37.0, -26.0),
            (150.0, 40.0, 110.0, -0.01, -32.0, -43.0),
        ] {
            let x = sum(&sine(f1, 0.3, 2.0, FS), &sine(f2, 0.3, 2.0, FS));
            let h = added(cutoff, 0.0, FS, &x);
            let h = last_second(&h, FS);
            let total = rms(h).powi(2);
            let power = |range: std::ops::RangeInclusive<usize>| -> f64 {
                range
                    .map(|k| amplitude(h, 10.0 * k as f64, FS).powi(2) / 2.0)
                    .sum()
            };
            let grid = |f: f64| (f / 10.0) as usize;
            let below = power(1..=grid(cutoff) - 1);
            let inside = power(grid(cutoff)..=grid(4.0 * cutoff));
            let share = |p: f64| 10.0 * (p / total).log10();
            let what = format!("{f1} + {f2} Hz at {cutoff} Hz");
            assert!(share(inside) > inside_db, "{what}: {}", share(inside));
            assert!(share(below) < below_db, "{what}: {}", share(below));
            let above = total - below - inside;
            assert!(share(above) < above_db, "{what}: {}", share(above));
            // The difference tone, below the band, is not there.
            assert!(db(amplitude(h, f2 - f1, FS) / 0.3) < -50.0, "{what}");
        }
    }

    #[test]
    fn remove_lows_cuts_the_band_below_the_cutoff_and_keeps_the_harmonics() {
        let x = sine(50.0, 0.5, 2.0, FS);
        let on = mono(&params(100.0, 0.0, true), FS, &x);
        let off = mono(&params(100.0, 0.0, false), FS, &x);
        let (on, off) = (last_second(&on, FS), last_second(&off, FS));
        assert_db(
            db(amplitude(on, 50.0, FS) / 0.5),
            -24.1,
            0.05,
            "fundamental",
        );
        for n in 2..=6 {
            let f = f64::from(n) * 50.0;
            let change = db(amplitude(on, f, FS) / amplitude(off, f, FS));
            assert_db(change, 0.0, 1e-4, &format!("harmonic {n}"));
        }
        let x = sine(1000.0, 0.5, 2.0, FS);
        let on = mono(&params(100.0, 0.0, true), FS, &x);
        let got = db(amplitude(last_second(&on, FS), 1000.0, FS) / 0.5);
        assert_db(got, 0.0, 1e-3, "1 kHz");
    }

    #[test]
    fn the_input_passes_undelayed_and_untouched_plus_the_harmonics() {
        let p = params(100.0, 6.0, false);
        let x: Vec<f32> = noise(FS as usize, 3)
            .iter()
            .zip(sine(60.0, 0.5, 1.0, FS))
            .map(|(n, s)| 0.3 * n + s)
            .collect();
        let y = mono(&p, FS, &x);
        let mut twin = Bass::new(&p, FS, 1);
        for (i, (x, y)) in x.iter().zip(&y).enumerate() {
            let (_, harmonics) = twin.channels[0].tick(*x, &twin.weights);
            assert_eq!(*y, x + harmonics, "sample {i}");
        }
    }

    /// Two channels of bass and a little noise.
    fn stereo_bass(seconds: f64) -> Vec<f32> {
        let l = sum(
            &sine(45.0, 0.5, seconds, FS),
            &noise((seconds * FS) as usize, 4),
        );
        let r = sine(70.0, 0.4, seconds, FS);
        l.iter().zip(&r).flat_map(|(l, r)| [*l, *r]).collect()
    }

    #[test]
    fn adopt_carries_on_where_the_old_stage_was() {
        let x = stereo_bass(1.0);
        let (first, rest) = x.split_at(x.len() / 2);
        let (off, on) = (params(100.0, 0.0, false), params(100.0, 0.0, true));

        let mut old = Bass::new(&off, FS, 2);
        run(&mut old, first, 2);
        let mut new = Bass::new(&off, FS, 2);
        new.adopt(&old);
        let want = run(&mut old, rest, 2);
        assert_eq!(run(&mut new, rest, 2), want);
        assert_ne!(run(&mut Bass::new(&off, FS, 2), rest, 2), want);

        // The high-pass ran with remove_lows off, so turning it on carries on
        // as if it had been on from the start.
        let mut old = Bass::new(&off, FS, 2);
        run(&mut old, first, 2);
        let mut toggled = Bass::new(&on, FS, 2);
        toggled.adopt(&old);
        let mut always = Bass::new(&on, FS, 2);
        run(&mut always, first, 2);
        assert_eq!(run(&mut toggled, rest, 2), run(&mut always, rest, 2));
    }

    #[test]
    fn channels_are_processed_independently() {
        let p = params(120.0, 3.0, true);
        let n = FS as usize / 2;
        for channels in [1, 2, 6] {
            // Channel 1 is silent; the others carry different bass.
            let inputs: Vec<Vec<f32>> = (0..channels)
                .map(|c| match c {
                    1 => vec![0.0; n],
                    _ => sum(
                        &sine(30.0 + 10.0 * c as f64, 0.5, 0.5, FS),
                        &noise(n, c as u64),
                    ),
                })
                .collect();
            let interleaved: Vec<f32> = (0..n)
                .flat_map(|i| inputs.iter().map(move |x| x[i]))
                .collect();
            let out = run(&mut Bass::new(&p, FS, channels), &interleaved, channels);
            for (c, input) in inputs.iter().enumerate() {
                let got: Vec<f32> = out.iter().skip(c).step_by(channels).copied().collect();
                assert_eq!(got, mono(&p, FS, input), "{channels} channels, channel {c}");
            }
        }
    }

    #[test]
    fn the_chain_output_does_not_depend_on_block_sizes() {
        let mut profile = Profile {
            bass: params(120.0, 3.0, true),
            ..Profile::default()
        };
        let x = stereo_bass(1.0);
        let mut whole = x.clone();
        Processor::new(&profile, FS as u32, 2).process(&mut whole);
        let mut split = x.clone();
        let mut chain = Processor::new(&profile, FS as u32, 2);
        let mut rest = &mut split[..];
        for frames in [1, 7, 480, 1031].iter().cycle() {
            let n = (2 * frames).min(rest.len());
            let (block, tail) = rest.split_at_mut(n);
            chain.process(block);
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        assert_eq!(whole, split);
        let mut without = x;
        profile.bass.enabled = false;
        Processor::new(&profile, FS as u32, 2).process(&mut without);
        assert_ne!(whole, without, "the chain runs the stage");
    }

    #[test]
    fn the_zero_db_reference_holds_at_every_rate() {
        for (fs, cutoff) in [
            (44_100.0, 100.0),
            (48_000.0, 100.0),
            (96_000.0, 100.0),
            (8000.0, 100.0),
            (8000.0, 250.0),
            (768_000.0, 40.0),
        ] {
            let f0 = cutoff / 2.0;
            let h = added(cutoff, 0.0, fs, &sine(f0, 0.5, 2.0, fs));
            let h = last_second(&h, fs);
            let mut combined = 0.0;
            for (n, want) in (2..).zip(PARTIALS_DB) {
                let got = amplitude(h, f64::from(n) * f0, fs) / 0.5;
                combined += got * got;
                let what = format!("{fs} Hz, {cutoff} Hz, harmonic {n}");
                assert_db(db(got), want, 0.1, &what);
            }
            assert_db(10.0 * combined.log10(), 0.0, 0.01, &format!("{fs} Hz"));
        }
    }

    #[test]
    fn nothing_aliases_at_8_khz() {
        // The highest cutoff and level, and fundamentals up to past where the
        // 6th harmonic reaches Nyquist. What is added and not a harmonic of
        // the input measured -98 dB or less relative to the input.
        let fs = 8000.0;
        for f in [125.0, 437.0, 700.0, 1000.0, 1500.0] {
            let h = added(250.0, 12.0, fs, &sine(f, 0.5, 2.0, fs));
            let h = last_second(&h, fs);
            let harmonic: f64 = (1..)
                .map(|k| f64::from(k) * f)
                .take_while(|fk| *fk < fs / 2.0)
                .map(|fk| amplitude(h, fk, fs).powi(2) / 2.0)
                .sum();
            let rest = (rms(h).powi(2) - harmonic).max(0.0);
            let got = 10.0 * (rest / 0.125).log10();
            assert!(got < -95.0, "{f} Hz: {got} dB");
        }
    }

    #[test]
    fn harmonics_that_would_not_fit_a_low_rate_are_left_out() {
        // At 2 kHz the 6th harmonic of a fundamental at a 100 Hz cutoff would
        // pass fs / 4; the rest still sum to the reference level.
        let fs = 2000.0;
        let h = added(100.0, 0.0, fs, &sine(50.0, 0.5, 3.0, fs));
        let h = last_second(&h, fs);
        assert!(db(amplitude(h, 300.0, fs) / 0.5) < -60.0);
        assert!(db(amplitude(h, 250.0, fs) / 0.5) > -15.0);
        assert_db(db(rms(h) * 2f64.sqrt() / 0.5), 0.0, 0.05, "combined");
    }

    #[test]
    fn stays_finite_and_recovers_from_any_finite_input() {
        let extremes = |n: usize| -> [Vec<f32>; 4] {
            [
                (0..n)
                    .map(|i| if i % 960 < 480 { f32::MAX } else { f32::MIN })
                    .collect(),
                (0..n)
                    .map(|i| if i % 2 == 0 { f32::MAX } else { f32::MIN })
                    .collect(),
                noise(n, 5).iter().map(|v| v * f32::MAX).collect(),
                noise(n, 6).iter().map(|v| v * f32::MIN_POSITIVE).collect(),
            ]
        };
        for (fs, cutoff, recover) in [
            (48_000.0, 100.0, true),
            (8000.0, 250.0, true),
            (768_000.0, 40.0, false),
            (401.0, 200.0, false),
        ] {
            for remove_lows in [false, true] {
                for x in extremes((fs / 4.0) as usize) {
                    let mut bass = Bass::new(&params(cutoff, 12.0, remove_lows), fs, 2);
                    let stereo: Vec<f32> = x.iter().flat_map(|v| [*v, -*v]).collect();
                    let out = run(&mut bass, &stereo, 2);
                    assert!(out.iter().all(|v| v.is_finite()), "{fs} Hz, {cutoff} Hz");
                    if recover {
                        let silence = run(&mut bass, &vec![0.0; 2 * 3 * fs as usize], 2);
                        let settled = &silence[silence.len() - fs as usize..];
                        assert!(peak(settled) < 1e-9, "{fs} Hz: {}", peak(settled));
                    }
                }
            }
        }
    }

    #[test]
    fn a_non_finite_sample_stays_put_and_out_of_the_state() {
        let mut x = sum(&sine(50.0, 0.5, 0.25, FS), &noise(FS as usize / 4, 8));
        let mut clean = x.clone();
        for (i, v) in [
            (1000, f32::NAN),
            (2000, f32::INFINITY),
            (2001, f32::NEG_INFINITY),
        ] {
            x[i] = v;
            clean[i] = 0.0;
        }
        for remove_lows in [false, true] {
            let p = params(100.0, 0.0, remove_lows);
            let (y, want) = (mono(&p, FS, &x), mono(&p, FS, &clean));
            for (i, ((y, want), x)) in y.iter().zip(&want).zip(&x).enumerate() {
                if x.is_finite() {
                    assert_eq!(y, want, "sample {i}");
                } else {
                    assert_eq!(y.to_bits(), x.to_bits(), "sample {i}");
                }
            }
        }
    }

    #[test]
    fn headroom_taken_on_a_bass_heavy_mix() {
        // Kick, sawtooth bass line and a quiet chord, peaking at -1 dBFS:
        // how far harmonics at 0 dB raise the peak.
        let notes = [55.0, 41.2, 73.4, 61.7];
        let hiss = noise(8 * FS as usize, 7);
        let mix: Vec<f64> = (0..8 * FS as usize)
            .map(|i| {
                let t = i as f64 / FS;
                let beat = t % 0.5;
                // 110 Hz falling to 45 Hz with a 40 ms time constant.
                let phase = 2.0 * PI * (45.0 * beat + 2.6 * (1.0 - (-beat / 0.04).exp()));
                let kick = (-beat / 0.15).exp() * phase.sin();
                let f = notes[t as usize % notes.len()];
                let saw = 2.0 * (f * t - (f * t + 0.5).floor());
                let bass = saw * (0.4 + 0.6 * (-(t % 0.25) / 0.1).exp());
                let chord: f64 = [220.0, 277.2, 329.6]
                    .iter()
                    .map(|f| (2.0 * PI * f * t).sin())
                    .sum();
                kick + 0.5 * bass + 0.05 * chord + 0.02 * f64::from(hiss[i])
            })
            .collect();
        let max = mix.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let x: Vec<f32> = mix.iter().map(|v| (0.89 * v / max) as f32).collect();
        for (cutoff, remove_lows, want) in [
            (100.0, false, 4.17),
            (100.0, true, 2.78),
            (150.0, false, 5.41),
            (150.0, true, 2.22),
        ] {
            let y = mono(&params(cutoff, 0.0, remove_lows), FS, &x);
            let what = format!("{cutoff} Hz, remove_lows {remove_lows}");
            assert_db(db(peak(&y) / peak(&x)), want, 0.05, &what);
        }
    }
}
