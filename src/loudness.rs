//! ITU-R BS.1770-4 loudness: the K-weighting filter, a meter over a sliding
//! window (0.4 s momentary, 3 s short-term) and gated integrated loudness.
//!
//! Every channel weighs 1.0: the surround weights (1.41 on the rear
//! channels, none for LFE) are not applied.
//!
//! The two K-weighting biquads come from their analog prototypes under the
//! bilinear transform prewarped at f0, so they fit any rate: (Vh s^2 + Vb s
//! / Q + 1) / (s^2 + s / Q + 1) for the shelf and G s^2 / (s^2 + s / Q + 1)
//! for the high-pass, s normalised to f0. The parameters, the shelf's Vb =
//! Vh^0.49966677 and the high-pass's passband gain G (+0.043 dB, from the
//! table's unnormalised numerator 1, -2, 1) are fitted to the standard's
//! 48 kHz table, which they reproduce to its last digit. The cookbook shelf
//! has Vb = sqrt(Vh) and is 2.6e-5 off it. libebur128 and ffmpeg keep the
//! numerator 1, -2, 1 at every rate instead of G, which reads 0.004 LU
//! louder at 44.1 kHz and 0.2 LU at 8 kHz.

use crate::biquad::Coefs;
use std::f64::consts::PI;

/// BS.1770 gates 400 ms blocks every 100 ms; the meter's window moves in the
/// same 100 ms steps.
const BLOCK_SECONDS: f64 = 0.1;
const GATING_BLOCKS: usize = 4;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = 10.0;

const SHELF_HZ: f64 = 1681.974450955533;
const SHELF_DB: f64 = 3.999843853973347;
const SHELF_Q: f64 = 0.7071752369554196;
const SHELF_VB_EXPONENT: f64 = 0.4996667741545416;
const HIGH_PASS_HZ: f64 = 38.13547087602444;
const HIGH_PASS_Q: f64 = 0.5003270373238773;

/// Loudness of a K-weighted power, the sum over channels of their mean
/// squares; -inf for 0.
pub(crate) fn lufs_of(power: f64) -> f64 {
    -0.691 + 10.0 * power.log10()
}

fn shelf(sample_rate: f64) -> Coefs {
    let k = (PI * SHELF_HZ / sample_rate).tan();
    let vh = 10f64.powf(SHELF_DB / 20.0);
    let vb = vh.powf(SHELF_VB_EXPONENT);
    let a0 = 1.0 + k / SHELF_Q + k * k;
    Coefs {
        b0: (vh + vb * k / SHELF_Q + k * k) / a0,
        b1: 2.0 * (k * k - vh) / a0,
        b2: (vh - vb * k / SHELF_Q + k * k) / a0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / SHELF_Q + k * k) / a0,
    }
}

fn high_pass(sample_rate: f64) -> Coefs {
    let k_and_a0 = |rate: f64| {
        let k = (PI * HIGH_PASS_HZ / rate).tan();
        (k, 1.0 + k / HIGH_PASS_Q + k * k)
    };
    let (k, a0) = k_and_a0(sample_rate);
    let gain = k_and_a0(48_000.0).1 / a0;
    Coefs {
        b0: gain,
        b1: -2.0 * gain,
        b2: gain,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / HIGH_PASS_Q + k * k) / a0,
    }
}

/// One biquad in transposed direct form II, f64 in and out: a sample near
/// f32::MAX through the shelf would overflow an f32 between the two stages.
struct Stage {
    c: Coefs,
    s1: f64,
    s2: f64,
}

impl Stage {
    fn new(c: Coefs) -> Self {
        Self {
            c,
            s1: 0.0,
            s2: 0.0,
        }
    }

    fn tick(&mut self, x: f64) -> f64 {
        let c = &self.c;
        let y = c.b0 * x + self.s1;
        self.s1 = c.b1 * x - c.a1 * y + self.s2;
        self.s2 = c.b2 * x - c.a2 * y;
        y
    }

    fn adopt(&mut self, from: &Self) {
        self.s1 = from.s1;
        self.s2 = from.s2;
    }
}

/// The K-weighting pre-filter for one channel (BS.1770: a high shelf, then a
/// high-pass), for any sample rate.
pub struct KWeighting {
    shelf: Stage,
    high_pass: Stage,
}

impl KWeighting {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            shelf: Stage::new(shelf(sample_rate)),
            high_pass: Stage::new(high_pass(sample_rate)),
        }
    }

    pub fn tick(&mut self, x: f32) -> f32 {
        self.weigh(f64::from(x)) as f32
    }

    fn weigh(&mut self, x: f64) -> f64 {
        self.high_pass.tick(self.shelf.tick(x))
    }

    /// Copies `from`'s filter state. No allocation.
    pub fn adopt(&mut self, from: &Self) {
        self.shelf.adopt(&from.shelf);
        self.high_pass.adopt(&from.high_pass);
    }
}

/// K-weighted power of a stream in consecutive 100 ms blocks: the sum over
/// channels of each one's mean square over the block.
pub(crate) struct Blocks {
    sample_rate: f64,
    filters: Vec<KWeighting>,
    /// Frames per block.
    len: usize,
    /// Frames into the current block, and the sum of their squared
    /// K-weighted samples.
    frames: usize,
    sum: f64,
}

impl Blocks {
    pub(crate) fn new(sample_rate: f64, channels: usize) -> Self {
        Self {
            sample_rate,
            filters: (0..channels)
                .map(|_| KWeighting::new(sample_rate))
                .collect(),
            len: ((BLOCK_SECONDS * sample_rate).round() as usize).max(1),
            frames: 0,
            sum: 0.0,
        }
    }

    /// One interleaved frame; the power of the block it completes, if it
    /// does. A non-finite sample counts as 0. No allocation.
    pub(crate) fn frame(&mut self, frame: &[f32]) -> Option<f64> {
        for (x, filter) in frame.iter().zip(&mut self.filters) {
            let x = if x.is_finite() { f64::from(*x) } else { 0.0 };
            let y = filter.weigh(x);
            self.sum += y * y;
        }
        self.frames += 1;
        if self.frames < self.len {
            return None;
        }
        let power = self.sum / self.len as f64;
        self.frames = 0;
        self.sum = 0.0;
        Some(power)
    }

    fn matches(&self, other: &Self) -> bool {
        self.sample_rate == other.sample_rate && self.filters.len() == other.filters.len()
    }

    /// Copies `from`'s filter states and current block, for `from` at the
    /// same rate and channel count. No allocation.
    pub(crate) fn adopt(&mut self, from: &Self) {
        for (to, from) in self.filters.iter_mut().zip(&from.filters) {
            to.adopt(from);
        }
        self.frames = from.frames;
        self.sum = from.sum;
    }
}

/// Loudness over a sliding window, in LUFS.
///
/// The window is a whole number of 100 ms blocks and moves a block at a
/// time, so the reading changes ten times a second.
pub struct Meter {
    blocks: Blocks,
    /// The last blocks' powers; the oldest is at `next` once all are heard.
    window: Vec<f64>,
    next: usize,
    /// Blocks heard, up to the window's length.
    heard: usize,
    /// Sum of `window`, added up afresh at each block rather than kept
    /// running, so silence after loud audio sums to exactly 0.
    sum: f64,
}

impl Meter {
    /// Allocates here only. `window_seconds` is e.g. 0.4 (momentary) or 3.0
    /// (short-term), rounded to whole 100 ms blocks.
    pub fn new(sample_rate: f64, channels: usize, window_seconds: f64) -> Self {
        let blocks = ((window_seconds / BLOCK_SECONDS).round() as usize).max(1);
        Self {
            blocks: Blocks::new(sample_rate, channels),
            window: vec![0.0; blocks],
            next: 0,
            heard: 0,
            sum: 0.0,
        }
    }

    /// One interleaved frame. No allocation. Non-finite samples count as 0.
    pub fn frame(&mut self, frame: &[f32]) {
        if let Some(power) = self.blocks.frame(frame) {
            self.window[self.next] = power;
            self.next = (self.next + 1) % self.window.len();
            self.heard = (self.heard + 1).min(self.window.len());
            self.sum = self.window.iter().sum();
        }
    }

    /// -0.691 + 10 log10(sum over channels of the K-weighted mean square
    /// over the window); f32::NEG_INFINITY for digital silence, which after
    /// sound takes the high-pass's tail about 3.5 s to reach. Before the
    /// window has filled, over what has been heard so far, the current
    /// block included.
    pub fn lufs(&self) -> f32 {
        let blocks = self.window.len();
        let power = if self.heard == blocks {
            self.sum / blocks as f64
        } else {
            let frames = self.heard * self.blocks.len + self.blocks.frames;
            if frames == 0 {
                return f32::NEG_INFINITY;
            }
            (self.sum * self.blocks.len as f64 + self.blocks.sum) / frames as f64
        };
        lufs_of(power) as f32
    }

    /// Copies `from`'s filter state and window when rates, channel counts
    /// and window lengths match. No allocation.
    pub fn adopt(&mut self, from: &Self) {
        if !self.blocks.matches(&from.blocks) || self.window.len() != from.window.len() {
            return;
        }
        self.blocks.adopt(&from.blocks);
        self.window.copy_from_slice(&from.window);
        self.next = from.next;
        self.heard = from.heard;
        self.sum = from.sum;
    }
}

/// Gated integrated loudness of interleaved samples (BS.1770-4: 400 ms
/// blocks, 75 % overlap, -70 LUFS absolute and -10 LU relative gates), in
/// LUFS; -inf when no block passes the gates or there is less than one
/// block. For tests and offline measurement; may allocate.
pub fn integrated_lufs(samples: &[f32], channels: usize, sample_rate: u32) -> f64 {
    let mut blocks = Blocks::new(f64::from(sample_rate), channels);
    let steps: Vec<f64> = samples
        .chunks_exact(channels)
        .filter_map(|frame| blocks.frame(frame))
        .collect();
    let audible: Vec<f64> = steps
        .windows(GATING_BLOCKS)
        .map(|w| w.iter().sum::<f64>() / GATING_BLOCKS as f64)
        .filter(|&p| lufs_of(p) > ABSOLUTE_GATE_LUFS)
        .collect();
    if audible.is_empty() {
        return f64::NEG_INFINITY;
    }
    let mean = |p: &[f64]| p.iter().sum::<f64>() / p.len() as f64;
    let gate = lufs_of(mean(&audible)) - RELATIVE_GATE_LU;
    let gated: Vec<f64> = audible.into_iter().filter(|&p| lufs_of(p) > gate).collect();
    lufs_of(mean(&gated))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const FS: u32 = 48_000;

    /// Deterministic values in [-1, 1).
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        }
    }

    /// `x` scaled to an integrated loudness of `lufs`.
    pub(crate) fn at_lufs(x: &[f32], channels: usize, rate: u32, lufs: f64) -> Vec<f32> {
        let gain = 10f64.powf((lufs - integrated_lufs(x, channels, rate)) / 20.0);
        x.iter().map(|v| (f64::from(*v) * gain) as f32).collect()
    }

    fn sine(freq_hz: f64, amp: f64, rate: u32, seconds: f64) -> Vec<f32> {
        let fs = f64::from(rate);
        (0..(seconds * fs) as usize)
            .map(|n| (amp * (2.0 * PI * freq_hz * n as f64 / fs).sin()) as f32)
            .collect()
    }

    /// Stereo with uncorrelated channels: white noise, then the same 6 dB
    /// down, 25 dB down (under the relative gate) and silent (under the
    /// absolute one), with a 440 Hz tone throughout on the left.
    fn sections(rate: u32) -> Vec<f32> {
        let fs = f64::from(rate);
        let mut rng = Rng(5);
        let mut x = Vec::new();
        for (seconds, level) in [(4.0, 0.3), (3.0, 0.15), (2.0, 0.017), (1.5, 0.0)] {
            for n in 0..(seconds * fs) as usize {
                let tone = 0.2 * level * (2.0 * PI * 440.0 * n as f64 / fs).sin();
                x.push((level * rng.next() + tone) as f32);
                x.push((level * rng.next()) as f32);
            }
        }
        x
    }

    fn assert_close(got: f64, want: f64, tol: f64, what: &str) {
        assert!(
            (got - want).abs() <= tol,
            "{what}: {got}, want {want} ± {tol}"
        );
    }

    #[test]
    fn coefficients_reproduce_the_48_khz_table() {
        // ITU-R BS.1770-4 table 1 (shelf) and table 2 (high-pass), given to
        // 14 decimals.
        let tol = 1e-13;
        let s = shelf(48_000.0);
        for (got, want) in [
            (s.b0, 1.53512485958697),
            (s.b1, -2.69169618940638),
            (s.b2, 1.19839281085285),
            (s.a1, -1.69065929318241),
            (s.a2, 0.73248077421585),
        ] {
            assert_close(got, want, tol, "shelf");
        }
        let h = high_pass(48_000.0);
        assert_eq!((h.b0, h.b1, h.b2), (1.0, -2.0, 1.0));
        assert_close(h.a1, -1.99004745483398, tol, "high-pass a1");
        assert_close(h.a2, 0.99007225036621, tol, "high-pass a2");
    }

    #[test]
    fn the_response_is_the_same_at_other_rates() {
        // Bilinear warping, which differs with the rate, bends it by at most
        // 0.0075 dB (around 3 kHz).
        let response = |rate: f64, f: f64| {
            shelf(rate).magnitude_db(f, rate) + high_pass(rate).magnitude_db(f, rate)
        };
        for rate in [44_100.0, 96_000.0, 192_000.0] {
            for f in [20.0, 38.0, 100.0, 997.0, 1682.0, 3000.0, 5000.0, 10_000.0] {
                let what = format!("{f} Hz at {rate} Hz");
                assert_close(response(rate, f), response(48_000.0, f), 0.01, &what);
            }
        }
        // Passband: +4 dB from the shelf and +0.043 dB from the high-pass.
        assert_close(response(48_000.0, 997.0), 0.691, 0.01, "997 Hz");
        assert_close(response(96_000.0, 15_000.0), 4.04, 0.01, "15 kHz");
    }

    #[test]
    fn a_full_scale_997_hz_sine_in_one_channel_reads_minus_3_01() {
        for rate in [44_100, FS, 96_000] {
            let x = sine(997.0, 1.0, rate, 5.0);
            assert_close(integrated_lufs(&x, 1, rate), -3.01, 0.1, "integrated");
            let mut meter = Meter::new(f64::from(rate), 1, 0.4);
            for frame in x.chunks_exact(1) {
                meter.frame(frame);
            }
            assert_close(f64::from(meter.lufs()), -3.01, 0.1, "momentary");
            // Stereo, the same in both channels: twice the power.
            let both: Vec<f32> = x.iter().flat_map(|v| [*v, *v]).collect();
            assert_close(integrated_lufs(&both, 2, rate), 0.0, 0.1, "stereo");
        }
    }

    /// The BS.1770 reference in tests/cli.rs, which the presets were tuned
    /// with, for interleaved stereo: cookbook approximations of the two
    /// filters (a +4 dB shelf at 1.5 kHz and a 38 Hz high-pass with unity
    /// passband), within 0.1 LU of ffmpeg's ebur128 on band material.
    fn reference_lufs(x: &[f64], rate: u32) -> f64 {
        fn biquad(x: &[f64], b: [f64; 3], a: [f64; 3]) -> Vec<f64> {
            let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
            x.iter()
                .map(|&v| {
                    let y = b[0] * v + b[1] * x1 + b[2] * x2 - a[1] * y1 - a[2] * y2;
                    (x2, x1, y2, y1) = (x1, v, y1, y);
                    y
                })
                .collect()
        }
        let fs = f64::from(rate);
        let k_weight = |x: Vec<f64>| {
            let a = 10f64.powf(4.0 / 40.0);
            let w = 2.0 * PI * 1500.0 / fs;
            let (alpha, c) = (w.sin() / 2f64.sqrt(), w.cos());
            let sa = 2.0 * a.sqrt() * alpha;
            let a0 = (a + 1.0) - (a - 1.0) * c + sa;
            let shelf = biquad(
                &x,
                [
                    a * ((a + 1.0) + (a - 1.0) * c + sa) / a0,
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * c) / a0,
                    a * ((a + 1.0) + (a - 1.0) * c - sa) / a0,
                ],
                [
                    1.0,
                    2.0 * ((a - 1.0) - (a + 1.0) * c) / a0,
                    ((a + 1.0) - (a - 1.0) * c - sa) / a0,
                ],
            );
            let w = 2.0 * PI * 38.0 / fs;
            let (alpha, c) = (w.sin(), w.cos());
            let a0 = 1.0 + alpha;
            biquad(
                &shelf,
                [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0],
                [1.0, -2.0 * c / a0, (1.0 - alpha) / a0],
            )
        };
        let channel = |c: usize| k_weight(x.iter().skip(c).step_by(2).copied().collect());
        let (l, r) = (channel(0), channel(1));
        let block = (0.4 * fs) as usize;
        let powers: Vec<f64> = (0..=(l.len() - block) / (block / 4))
            .map(|k| {
                let span = k * block / 4..k * block / 4 + block;
                l[span.clone()]
                    .iter()
                    .chain(&r[span])
                    .map(|v| v * v)
                    .sum::<f64>()
                    / block as f64
            })
            .collect();
        let lufs = |p: f64| -0.691 + 10.0 * p.log10();
        let mean = |p: &[f64]| p.iter().sum::<f64>() / p.len() as f64;
        let audible: Vec<f64> = powers.into_iter().filter(|&p| lufs(p) > -70.0).collect();
        let gate = lufs(mean(&audible)) - 10.0;
        let gated: Vec<f64> = audible.into_iter().filter(|&p| lufs(p) > gate).collect();
        lufs(mean(&gated))
    }

    #[test]
    fn integrated_loudness_agrees_with_the_reference() {
        // The reference's unity-gain high-pass reads 0.043 LU under the
        // standard's on anything above a few hundred Hz.
        for rate in [44_100, FS] {
            let x = sections(rate);
            let wide: Vec<f64> = x.iter().map(|v| f64::from(*v)).collect();
            let want = reference_lufs(&wide, rate);
            let got = integrated_lufs(&x, 2, rate);
            assert_close(got, want, 0.05, &format!("{rate} Hz"));
            // The gates are doing something: the plain mean is quieter.
            let mut meter = Meter::new(f64::from(rate), 2, 100.0);
            for frame in x.chunks_exact(2) {
                meter.frame(frame);
            }
            assert!(
                f64::from(meter.lufs()) < got - 0.5,
                "{} {got}",
                meter.lufs()
            );
        }
    }

    #[test]
    fn the_meter_agrees_with_integrated_loudness_on_a_steady_signal() {
        let mut rng = Rng(9);
        let x: Vec<f32> = (0..2 * 20 * FS as usize)
            .map(|_| (0.2 * rng.next()) as f32)
            .collect();
        let want = integrated_lufs(&x, 2, FS);
        for window in [0.4, 3.0] {
            let mut meter = Meter::new(f64::from(FS), 2, window);
            for (n, frame) in x.chunks_exact(2).enumerate() {
                meter.frame(frame);
                if n > FS as usize && n % 4801 == 0 {
                    let what = format!("{window} s at frame {n}");
                    assert_close(f64::from(meter.lufs()), want, 0.1, &what);
                }
            }
        }
    }

    #[test]
    fn before_the_window_fills_it_reads_what_has_been_heard() {
        let mut rng = Rng(10);
        let x: Vec<f32> = (0..FS as usize)
            .map(|_| (0.2 * rng.next()) as f32)
            .collect();
        let want = integrated_lufs(&x, 1, FS);
        let mut meter = Meter::new(f64::from(FS), 1, 3.0);
        assert_eq!(meter.lufs(), f32::NEG_INFINITY);
        for (n, frame) in x.chunks_exact(1).enumerate() {
            meter.frame(frame);
            // From 50 ms on, part of the first block included.
            if n >= 2400 && n % 1000 == 0 {
                assert_close(f64::from(meter.lufs()), want, 0.3, &format!("frame {n}"));
            }
        }
    }

    #[test]
    fn silence_reads_minus_infinity() {
        let zeros = vec![0.0f32; 2 * FS as usize];
        assert_eq!(integrated_lufs(&zeros, 2, FS), f64::NEG_INFINITY);
        assert_eq!(integrated_lufs(&[], 2, FS), f64::NEG_INFINITY);
        // Shorter than a gating block.
        let short = sine(997.0, 1.0, FS, 0.39);
        assert_eq!(integrated_lufs(&short, 1, FS), f64::NEG_INFINITY);
        let mut meter = Meter::new(f64::from(FS), 2, 0.4);
        for frame in zeros.chunks_exact(2) {
            meter.frame(frame);
        }
        assert_eq!(meter.lufs(), f32::NEG_INFINITY);
        // Silence after sound: the high-pass's tail decays about 0.02 dB a
        // sample, its square underflows to 0 within about 3.5 s, and then
        // the window is empty.
        let mut meter = Meter::new(f64::from(FS), 1, 0.4);
        for frame in sine(997.0, 1.0, FS, 1.0).chunks_exact(1) {
            meter.frame(frame);
        }
        assert!(meter.lufs() > -4.0);
        for _ in 0..FS {
            meter.frame(&[0.0]);
        }
        assert!(meter.lufs() < -1000.0, "{}", meter.lufs());
        for _ in 0..4 * FS {
            meter.frame(&[0.0]);
        }
        assert_eq!(meter.lufs(), f32::NEG_INFINITY);
    }

    #[test]
    fn non_finite_samples_count_as_zero_and_do_not_poison() {
        let mut rng = Rng(11);
        let x: Vec<f32> = (0..2 * 3 * FS as usize)
            .map(|_| (0.3 * rng.next()) as f32)
            .collect();
        let (mut with, mut zeroed) = (x.clone(), x);
        for (i, v) in [
            (101, f32::NAN),
            (20_000, f32::INFINITY),
            (20_001, f32::NEG_INFINITY),
            (90_000, f32::MAX),
        ] {
            with[i] = v;
            zeroed[i] = if v.is_finite() { v } else { 0.0 };
        }
        let mut a = Meter::new(f64::from(FS), 2, 0.4);
        let mut b = Meter::new(f64::from(FS), 2, 0.4);
        for (fa, fb) in with.chunks_exact(2).zip(zeroed.chunks_exact(2)) {
            a.frame(fa);
            b.frame(fb);
            assert_eq!(a.lufs().to_bits(), b.lufs().to_bits());
        }
        // f32::MAX is finite: it is measured, and leaves the filters
        // finite, so the reading comes back.
        assert!(a.lufs().is_finite() && a.lufs() < 0.0, "{}", a.lufs());
        assert_eq!(
            integrated_lufs(&with, 2, FS).to_bits(),
            integrated_lufs(&zeroed, 2, FS).to_bits()
        );
        let mut k = KWeighting::new(f64::from(FS));
        k.tick(f32::MAX);
        for _ in 0..10 * FS {
            k.tick(0.0);
        }
        assert!(k.tick(0.5).abs() < 1.0);
    }

    #[test]
    fn adopting_carries_the_reading() {
        let x = sections(FS);
        let split = 2 * (2 * FS as usize + 1234);
        let mut a = Meter::new(f64::from(FS), 2, 3.0);
        for frame in x[..split].chunks_exact(2) {
            a.frame(frame);
        }
        let mut b = Meter::new(f64::from(FS), 2, 3.0);
        b.adopt(&a);
        assert_eq!(b.lufs(), a.lufs());
        let mut cold = Meter::new(f64::from(FS), 2, 3.0);
        let mut differs = false;
        for frame in x[split..].chunks_exact(2) {
            a.frame(frame);
            b.frame(frame);
            cold.frame(frame);
            assert_eq!(b.lufs().to_bits(), a.lufs().to_bits());
            differs |= cold.lufs() != a.lufs();
        }
        assert!(differs);
        // Another window length, rate or channel count is left alone.
        for mut other in [
            Meter::new(f64::from(FS), 2, 0.4),
            Meter::new(44_100.0, 2, 3.0),
            Meter::new(f64::from(FS), 1, 3.0),
        ] {
            other.adopt(&a);
            assert_eq!(other.lufs(), f32::NEG_INFINITY);
        }
    }

    #[test]
    fn k_weighting_adopt_copies_the_state() {
        let mut rng = Rng(12);
        let x: Vec<f32> = (0..4000).map(|_| rng.next() as f32).collect();
        let mut a = KWeighting::new(f64::from(FS));
        for v in &x[..1000] {
            a.tick(*v);
        }
        let mut b = KWeighting::new(f64::from(FS));
        b.adopt(&a);
        for v in &x[1000..] {
            assert_eq!(a.tick(*v), b.tick(*v));
        }
    }
}
