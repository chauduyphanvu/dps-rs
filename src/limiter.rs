//! Output peak limiter, the chain's last stage. US 9,350,309 claims a gain
//! reduction "if the gain of the second compressed signal is above a
//! predetermined threshold" and gives no values (docs/patent-spec-later.md
//! section 6).
//!
//! One gain for the whole frame, from its largest sample, so a loud channel
//! pulls the others down with it and the stereo image does not shift. No
//! output sample ever passes the ceiling. Without lookahead the gain drops at
//! once to what a frame needs, all in one step at an abrupt start. With L
//! frames of lookahead the output is delayed by L and the gain comes down in
//! a straight line over the W = L + 1 frames up to the peak, at most
//! (1 - gain) / W a frame. It holds for `HOLD_SECONDS` after the last frame
//! that pulled it down, then recovers towards unity with the release time
//! constant.
//!
//! Why lookahead keeps the ceiling: let m[n] be the gain the loudest peak of
//! frames n - L ..= n needs, and the target at n the mean of m[n - L ..= n].
//! Each of those W windows holds frame n - L, the one leaving the delay, so
//! every m in the mean is a gain that keeps that frame under the ceiling, and
//! the mean is at most the largest of them. The mean is summed exactly, in
//! fixed point rounded down (gains under 2^-32 count as 0), and rounding it
//! to the nearest f32 cannot carry it past an f32 it is under. The gain
//! applied is the lower of the target and what the hold and release left.
//!
//! True peak also counts what the signal reaches between samples: a 4-phase
//! polyphase interpolator (a Kaiser-windowed sinc, beta 4, 16 taps a phase,
//! 64 in all, phase 0 being the sample itself) estimates the points a
//! quarter, half and three quarters of the way to the next sample, and the
//! largest on either side of a sample counts toward its frame's peak. It
//! reads 8 samples ahead, which delays the audio as much. A tone up to
//! 0.417 fs (20 kHz at 48 kHz) reads at most 0.35 dB under its peak and
//! 0.09 dB over. From 176.4 kHz only the midpoint is estimated (2x), which
//! up to 20 kHz reads at most 0.12 dB under.

use std::f64::consts::PI;

use crate::profile::LimiterParams;

/// |x| of a 50 Hz tone peaks every 10 ms, so a hold this long keeps bass from
/// 50 Hz up at one gain through each cycle instead of modulating it at twice
/// its frequency, which a release starting at every peak would.
const HOLD_SECONDS: f64 = 0.01;
/// The longest lookahead `Profile::validate` accepts.
const MAX_LOOKAHEAD_MS: f64 = 10.0;
/// Samples the true-peak interpolator sees for each point, half of them
/// after it: its delay is `TAPS / 2`.
const TAPS: usize = 16;
const KAISER_BETA: f64 = 4.0;
/// From this rate up, true peak estimates only the midpoint between samples.
const MIDPOINT_ONLY_HZ: f64 = 176_400.0;
/// Gain 1 in the fixed point the lookahead averages in.
const UNITY: u64 = 1 << 32;
/// `new` sizes the buffers for live mode's stereo. Other channel counts,
/// which only files have, resize them on the first frame.
const PREALLOCATED_CHANNELS: usize = 2;

pub struct Limiter {
    ceiling: f32,
    release: f32,
    hold: u32,
    /// 1 minus the gain. The release decays this rather than moving the gain
    /// towards 1: in f32 a gain within a few thousand steps of 1 would stop
    /// short of it, and the limiter would never pass audio unchanged again.
    reduction: f32,
    /// Frames still to hold before the release starts.
    hold_left: u32,
    /// The gain applied to the last frame.
    gain: f32,
    /// The detector's delay plus the lookahead.
    latency: usize,
    channels: usize,
    /// The last `latency` input frames, interleaved, the oldest at
    /// `delay_at`.
    delay: Vec<f32>,
    delay_at: usize,
    true_peak: Option<TruePeak>,
    lookahead: Option<Lookahead>,
}

impl Limiter {
    pub fn new(params: &LimiterParams, sample_rate: f64) -> Self {
        let lookahead = lookahead_frames(f64::from(params.lookahead_ms), sample_rate);
        let detector_delay = if params.true_peak { TAPS / 2 } else { 0 };
        let latency = lookahead + detector_delay;
        Self {
            ceiling: 10f64.powf(f64::from(params.ceiling_db) / 20.0) as f32,
            release: (-1.0 / (sample_rate * f64::from(params.release_ms) / 1000.0)).exp() as f32,
            hold: (HOLD_SECONDS * sample_rate).round() as u32,
            reduction: 0.0,
            hold_left: 0,
            gain: 1.0,
            latency,
            channels: PREALLOCATED_CHANNELS,
            delay: vec![0.0; latency * PREALLOCATED_CHANNELS],
            delay_at: 0,
            true_peak: params.true_peak.then(|| TruePeak::new(sample_rate)),
            lookahead: (lookahead > 0).then(|| Lookahead::new(lookahead + 1)),
        }
    }

    /// The most `latency_frames` can be at this rate for any valid
    /// parameters.
    pub fn max_latency_frames(sample_rate: f64) -> usize {
        lookahead_frames(MAX_LOOKAHEAD_MS, sample_rate) + TAPS / 2
    }

    /// Limits one frame, all its channels, in place, and returns in it the
    /// frame from `latency_frames` frames ago. A NaN or infinite sample is
    /// left out of the peak and passes as it is (times the gain), for the
    /// caller to deal with; it does not move the gain.
    pub fn frame(&mut self, frame: &mut [f32]) {
        if frame.len() != self.channels {
            self.fit(frame.len());
        }
        let peak = match &mut self.true_peak {
            Some(detector) => detector.push(frame),
            None => frame
                .iter()
                .filter(|x| x.is_finite())
                .fold(0.0f32, |peak, x| peak.max(x.abs())),
        };
        if self.latency > 0 {
            let at = self.delay_at * frame.len();
            for (x, delayed) in frame.iter_mut().zip(&mut self.delay[at..]) {
                std::mem::swap(x, delayed);
            }
            self.delay_at = if self.delay_at + 1 == self.latency {
                0
            } else {
                self.delay_at + 1
            };
        }

        if self.hold_left > 0 {
            self.hold_left -= 1;
        } else {
            self.reduction *= self.release;
        }
        let mut gain = 1.0 - self.reduction;
        if gain == 1.0 {
            self.reduction = 0.0;
        }
        let target = match &mut self.lookahead {
            Some(lookahead) => lookahead.push(peak, self.ceiling),
            None if peak * gain > self.ceiling => required(peak, self.ceiling),
            None => gain,
        };
        if target < gain {
            gain = target;
            self.reduction = 1.0 - gain;
            self.hold_left = self.hold;
        }
        if gain < 1.0 {
            for x in frame {
                *x *= gain;
            }
        }
        self.gain = gain;
    }

    /// The linear gain applied to the last frame.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Frames by which the output lags the input.
    pub fn latency_frames(&self) -> usize {
        self.latency
    }

    /// Starts where `from` is: its gain and what is left of its hold, and,
    /// when both have the same lookahead and detector, what is in its delay
    /// and detector, so the audio carries on without a gap or a repeat and
    /// a peak `from` saw coming is still turned down, to this limiter's own
    /// ceiling. Neither allocates nor frees.
    pub fn adopt(&mut self, from: &Self) {
        self.reduction = from.reduction;
        self.hold_left = from.hold_left;
        let same_shape = self.latency == from.latency
            && self.delay.capacity() >= from.delay.len()
            && match (&self.true_peak, &from.true_peak) {
                (Some(to), Some(from)) => to.can_adopt(from),
                (None, None) => true,
                _ => false,
            };
        if !same_shape {
            return;
        }
        self.channels = from.channels;
        self.delay.clear();
        self.delay.extend_from_slice(&from.delay);
        self.delay_at = from.delay_at;
        if let (Some(to), Some(from)) = (&mut self.true_peak, &from.true_peak) {
            to.adopt(from);
        }
        if let (Some(to), Some(from)) = (&mut self.lookahead, &from.lookahead) {
            to.adopt(from, self.ceiling);
        }
    }

    /// Empties the buffers and sizes them for frames of `channels`.
    fn fit(&mut self, channels: usize) {
        self.channels = channels;
        self.delay.clear();
        self.delay.resize(self.latency * channels, 0.0);
        self.delay_at = 0;
        if let Some(detector) = &mut self.true_peak {
            detector.fit(channels);
        }
    }
}

fn lookahead_frames(ms: f64, sample_rate: f64) -> usize {
    if ms > 0.0 {
        ((ms * sample_rate / 1000.0).round() as usize).max(1)
    } else {
        0
    }
}

/// The gain that brings `peak` down to `ceiling`, rounded down so it does
/// not pass it; 1 for a peak already there.
fn required(peak: f32, ceiling: f32) -> f32 {
    if peak <= ceiling {
        return 1.0;
    }
    let mut gain = ceiling / peak;
    // The rounded quotient can put peak * gain a step over the ceiling.
    while peak * gain > ceiling {
        gain = gain.next_down();
    }
    gain
}

/// Estimates each frame's peak including what the signal reaches between
/// samples, `TAPS / 2` frames after the frame arrives.
struct TruePeak {
    /// The interpolator's taps for each point estimated between samples.
    phases: Vec<[f32; TAPS]>,
    /// For each channel the last `TAPS` samples, twice over so that they
    /// read as one slice; a sample that is not finite is held as 0.
    history: Vec<f32>,
    at: usize,
    /// The largest estimate between the frame last reported and the next.
    after_last: f32,
}

impl TruePeak {
    fn new(sample_rate: f64) -> Self {
        let points: &[f64] = if sample_rate >= MIDPOINT_ONLY_HZ {
            &[0.5]
        } else {
            &[0.25, 0.5, 0.75]
        };
        let half = (TAPS / 2) as f64;
        let phases = points
            .iter()
            .map(|offset| {
                std::array::from_fn(|i| {
                    // From the point to tap i's sample, which is never 0.
                    let t = half - 1.0 - i as f64 + offset;
                    let window = bessel_i0(KAISER_BETA * (1.0 - (t / half).powi(2)).sqrt())
                        / bessel_i0(KAISER_BETA);
                    ((PI * t).sin() / (PI * t) * window) as f32
                })
            })
            .collect();
        Self {
            phases,
            history: vec![0.0; 2 * TAPS * PREALLOCATED_CHANNELS],
            at: 0,
            after_last: 0.0,
        }
    }

    /// Takes the next frame and returns the peak of the frame `TAPS / 2`
    /// before it: its largest sample, or the largest estimate between it and
    /// a neighbour if higher.
    fn push(&mut self, frame: &[f32]) -> f32 {
        let at = self.at;
        let (mut sample, mut between) = (0.0f32, 0.0f32);
        for (x, history) in frame.iter().zip(self.history.chunks_exact_mut(2 * TAPS)) {
            let x = if x.is_finite() { *x } else { 0.0 };
            history[at] = x;
            history[at + TAPS] = x;
            let recent = &history[at + 1..=at + TAPS];
            sample = sample.max(recent[TAPS / 2 - 1].abs());
            for taps in &self.phases {
                let y: f32 = recent.iter().zip(taps).map(|(x, c)| x * c).sum();
                between = between.max(y.abs());
            }
        }
        self.at = if at + 1 == TAPS { 0 } else { at + 1 };
        let peak = sample.max(self.after_last).max(between);
        self.after_last = between;
        peak
    }

    fn fit(&mut self, channels: usize) {
        self.history.clear();
        self.history.resize(2 * TAPS * channels, 0.0);
        self.at = 0;
        self.after_last = 0.0;
    }

    fn can_adopt(&self, from: &Self) -> bool {
        self.phases == from.phases && self.history.capacity() >= from.history.len()
    }

    fn adopt(&mut self, from: &Self) {
        self.history.clear();
        self.history.extend_from_slice(&from.history);
        self.at = from.at;
        self.after_last = from.after_last;
    }
}

/// The modified Bessel function of the first kind, order 0, from its series.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > sum * 1e-17 {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

/// The lookahead's target gain over the last `len` frames' peaks. Each is
/// kept in a ring of `len` slots, the next to be replaced at `at`.
struct Lookahead {
    len: usize,
    at: usize,
    peaks: Vec<f32>,
    /// The loudest peak of the `len` frames up to each frame.
    maxima: Vec<f32>,
    /// In fixed point, rounded down, the gain each of `maxima` needs.
    shares: Vec<u64>,
    sum: u64,
    /// A ring of the slots of the frames that are louder than every frame
    /// after them, oldest first, so the first is the loudest of the window:
    /// `count` of them from `head`.
    order: Vec<usize>,
    head: usize,
    count: usize,
}

impl Lookahead {
    /// Starts after `len` frames of silence.
    fn new(len: usize) -> Self {
        Self {
            len,
            at: 0,
            peaks: vec![0.0; len],
            maxima: vec![0.0; len],
            shares: vec![UNITY; len],
            sum: UNITY * len as u64,
            order: vec![0; len],
            head: 0,
            count: 0,
        }
    }

    /// Takes the next frame's peak and returns the target gain for the frame
    /// `len - 1` before it.
    fn push(&mut self, peak: f32, ceiling: f32) -> f32 {
        let at = self.at;
        // The frame in slot `at` leaves the window.
        if self.count > 0 && self.order[self.head] == at {
            self.head = self.wrap(self.head + 1);
            self.count -= 1;
        }
        while self.count > 0
            && self.peaks[self.order[self.wrap(self.head + self.count - 1)]] <= peak
        {
            self.count -= 1;
        }
        let back = self.wrap(self.head + self.count);
        self.order[back] = at;
        self.count += 1;
        self.peaks[at] = peak;
        let loudest = self.peaks[self.order[self.head]];
        let share = share(required(loudest, ceiling));
        self.sum = self.sum - self.shares[at] + share;
        self.shares[at] = share;
        self.maxima[at] = loudest;
        self.at = self.wrap(at + 1);
        (((self.sum / self.len as u64) as f64) / UNITY as f64) as f32
    }

    fn wrap(&self, slot: usize) -> usize {
        if slot >= self.len {
            slot - self.len
        } else {
            slot
        }
    }

    /// Takes over `from`'s window, of the same length, with the gains worked
    /// out again for `ceiling`.
    fn adopt(&mut self, from: &Self, ceiling: f32) {
        self.peaks.copy_from_slice(&from.peaks);
        self.maxima.copy_from_slice(&from.maxima);
        self.order.copy_from_slice(&from.order);
        (self.at, self.head, self.count) = (from.at, from.head, from.count);
        self.sum = 0;
        for (share_slot, loudest) in self.shares.iter_mut().zip(&self.maxima) {
            *share_slot = share(required(*loudest, ceiling));
            self.sum += *share_slot;
        }
    }
}

/// `gain` (0 to 1) in fixed point, rounded down.
fn share(gain: f32) -> u64 {
    (f64::from(gain) * UNITY as f64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const FS: f64 = 48_000.0;

    fn limiter(ceiling_db: f32, release_ms: f32, sample_rate: f64) -> Limiter {
        let params = LimiterParams {
            enabled: true,
            ceiling_db,
            release_ms,
            lookahead_ms: 0.0,
            true_peak: false,
        };
        Limiter::new(&params, sample_rate)
    }

    /// The ceiling as an f32 level, as the limiter holds it.
    fn level(ceiling_db: f32) -> f32 {
        10f64.powf(f64::from(ceiling_db) / 20.0) as f32
    }

    /// Deterministic values in [-1, 1).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn run(limiter: &mut Limiter, samples: &mut [f32], channels: usize) {
        for frame in samples.chunks_exact_mut(channels) {
            limiter.frame(frame);
        }
    }

    fn bits(x: &[f32]) -> Vec<u32> {
        x.iter().map(|v| v.to_bits()).collect()
    }

    /// Interleaved inputs meant to slip past a limiter: (name, channels).
    fn adversarial(ceiling: f32) -> Vec<(&'static str, usize, Vec<f32>)> {
        let mut rng = Rng(1);
        let n = 20_000;
        let mut first: Vec<f32> = vec![1.0, -1.0];
        first.extend((0..2 * n).map(|_| rng.next()));
        let mut impulses = vec![0.0; 2 * n];
        for (k, v) in [1.0, 10.0, f32::MAX, ceiling.next_up(), -1.0, 2.0]
            .into_iter()
            .enumerate()
        {
            // Long enough apart for the gain to be back at unity.
            impulses[2 * (k * 3000 + 17) + k % 2] = v;
        }
        let mut random = Vec::with_capacity(6 * n);
        let mut scale = 1.0;
        for i in 0..6 * n {
            if i % 600 == 0 {
                // Each 100-frame stretch at a level from -60 to +60 dB.
                scale = 10f32.powf(3.0 * rng.next());
            }
            random.push(scale * rng.next());
        }
        vec![
            ("full scale on the first sample", 2, first),
            (
                "alternating full scale",
                1,
                (0..n)
                    .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
                    .collect(),
            ),
            ("DC above the ceiling", 2, vec![3.0; 2 * n]),
            ("impulses after silence", 2, impulses),
            ("random levels on 6 channels", 6, random),
            ("one step over the ceiling", 1, vec![ceiling.next_up(); n]),
        ]
    }

    #[test]
    fn no_sample_ever_passes_the_ceiling() {
        for ceiling_db in [-24.0, -6.0, -1.0, -0.1, 0.0] {
            let ceiling = level(ceiling_db);
            for release_ms in [1.0, 100.0, 2000.0] {
                for rate in [44_100.0, 192_000.0] {
                    for (name, channels, input) in adversarial(ceiling) {
                        let mut out = input.clone();
                        run(
                            &mut limiter(ceiling_db, release_ms, rate),
                            &mut out,
                            channels,
                        );
                        let what = format!("{name}, {ceiling_db} dB, {release_ms} ms, {rate} Hz");
                        if let Some(i) = out.iter().position(|x| x.abs() > ceiling) {
                            panic!("{what}: sample {i} is {} over {ceiling}", out[i]);
                        }
                        if input.iter().any(|x| x.abs() > ceiling) {
                            assert_ne!(bits(&out), bits(&input), "{what}: nothing was limited");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn audio_at_or_under_the_ceiling_passes_bit_for_bit() {
        for ceiling_db in [-24.0, -1.0, 0.0] {
            let ceiling = level(ceiling_db);
            let mut rng = Rng(2);
            let input: Vec<f32> = (0..2 * 48_000)
                .map(|i| match i {
                    0 => ceiling,
                    1 => -ceiling,
                    2 => -0.0,
                    _ => ceiling * rng.next(),
                })
                .collect();
            let mut out = input.clone();
            run(&mut limiter(ceiling_db, 100.0, FS), &mut out, 2);
            assert_eq!(bits(&out), bits(&input), "{ceiling_db} dB");
        }
    }

    #[test]
    fn one_gain_turns_down_every_channel_of_a_frame() {
        // Left 6 dB over the ceiling, the others far under it. A fourth
        // channel holds 2^-10, which the gain scales exactly, so it shows
        // the frame's gain.
        let mut lim = limiter(-1.0, 100.0, FS);
        let probe = 2f32.powi(-10);
        let mut turned_down = 0;
        for n in 0..9600 {
            let t = n as f64 / FS;
            let input = [
                (2.0 * (2.0 * PI * 100.0 * t).sin()) as f32,
                (0.01 * (2.0 * PI * 3000.0 * t).sin()) as f32,
                -0.003,
                probe,
            ];
            let mut out = input;
            lim.frame(&mut out);
            let gain = out[3] / probe;
            for c in 0..3 {
                assert_eq!(out[c], input[c] * gain, "frame {n} channel {c}");
            }
            if gain < 0.9 {
                turned_down += 1;
            }
        }
        assert!(turned_down > 9000, "{turned_down}");
    }

    #[test]
    fn gain_holds_10_ms_then_recovers_with_the_release_time_constant() {
        // An f64 model from the description, not the code: after a sample
        // of 2 the gain is ceiling / 2, it stays there for 10 ms, then the
        // reduction 1 - gain decays as e^(-t / release).
        for (rate, release_ms) in [(48_000.0, 100.0), (44_100.0, 250.0), (192_000.0, 5.0)] {
            let ceiling = f64::from(level(-1.0));
            let mut lim = limiter(-1.0, release_ms as f32, rate);
            let mut frame = [2.0f32];
            lim.frame(&mut frame);
            let start = f64::from(frame[0]) / 2.0;
            assert!((start - ceiling / 2.0).abs() < 1e-7, "{start}");

            let hold = (0.01 * rate).round() as usize;
            let tau = release_ms / 1000.0 * rate;
            let want = |n: usize| {
                if n <= hold {
                    start
                } else {
                    1.0 - (1.0 - start) * (-((n - hold) as f64) / tau).exp()
                }
            };
            let probe = 0.25f32;
            let frames = hold + (25.0 * tau) as usize;
            let one_tau = hold + tau.round() as usize;
            let mut at_one_tau = 0.0;
            for n in 1..=frames {
                let mut frame = [probe];
                lim.frame(&mut frame);
                let got = f64::from(frame[0]) / f64::from(probe);
                if n <= hold {
                    // Not a step of the release, which moves it about 1e-4.
                    assert!(
                        (got - start).abs() < 1e-7,
                        "{rate} Hz: frame {n} in the hold"
                    );
                }
                assert!(
                    (got - want(n)).abs() < 1e-4,
                    "{rate} Hz, {release_ms} ms: frame {n} gain {got}, want {}",
                    want(n)
                );
                if n == one_tau {
                    at_one_tau = got;
                }
            }
            // One time constant into the release, 1 - 1/e of the reduction
            // is gone; long after, the probe passes bit for bit.
            let reduction_left = (1.0 - at_one_tau) / (1.0 - start);
            assert!(
                (reduction_left - (-1f64).exp()).abs() < 1e-3,
                "{reduction_left}"
            );
            let mut frame = [probe];
            lim.frame(&mut frame);
            assert_eq!(frame[0], probe, "{rate} Hz");
        }
    }

    #[test]
    fn a_non_finite_sample_neither_moves_the_gain_nor_turns_finite() {
        // Loud stereo noise with NaN and infinities on single samples: every
        // other sample comes out as if they had been 0, and they stay
        // non-finite for the caller to see.
        let mut rng = Rng(3);
        let input: Vec<f32> = (0..2 * 20_000).map(|_| 1.5 * rng.next()).collect();
        let bad = [
            (2001, f32::NAN),
            (9000, f32::INFINITY),
            (15_333, f32::NEG_INFINITY),
        ];
        let (mut with, mut zeroed) = (input.clone(), input);
        for (i, v) in bad {
            with[i] = v;
            zeroed[i] = 0.0;
        }
        run(&mut limiter(-1.0, 100.0, FS), &mut with, 2);
        run(&mut limiter(-1.0, 100.0, FS), &mut zeroed, 2);
        for (i, (w, z)) in with.iter().zip(&zeroed).enumerate() {
            match bad.iter().find(|(at, _)| *at == i) {
                Some((_, v)) if v.is_nan() => assert!(w.is_nan(), "sample {i}"),
                Some((_, v)) => assert_eq!(w, v, "sample {i}"),
                None => assert_eq!(w.to_bits(), z.to_bits(), "sample {i}"),
            }
        }
        // From rest, an infinity beside a quiet sample leaves both alone,
        // and so the quiet audio after it.
        let mut lim = limiter(-1.0, 100.0, FS);
        let mut frame = [f32::INFINITY, 0.5];
        lim.frame(&mut frame);
        assert_eq!(frame, [f32::INFINITY, 0.5]);
        let mut frame = [0.3, -0.3];
        lim.frame(&mut frame);
        assert_eq!(frame, [0.3, -0.3]);
    }

    #[test]
    fn gain_is_what_the_last_frame_was_multiplied_by() {
        let mut lim = limiter(-1.0, 100.0, FS);
        assert_eq!(lim.gain(), 1.0);
        lim.frame(&mut [0.5, -0.25]);
        assert_eq!(lim.gain(), 1.0);
        let input = [2.0, -0.5];
        let mut frame = input;
        lim.frame(&mut frame);
        assert!(lim.gain() < level(-1.0) / 2.0 + 1e-6, "{}", lim.gain());
        assert_eq!(frame, input.map(|x| x * lim.gain()));
        // Through the hold and the release.
        for n in 0..2000 {
            let mut frame = [0.25f32];
            lim.frame(&mut frame);
            assert_eq!(frame[0], 0.25 * lim.gain(), "frame {n}");
        }
        assert!(lim.gain() > level(-1.0) / 2.0 && lim.gain() < 1.0);
    }

    #[test]
    fn adopting_carries_the_gain_and_the_rest_of_the_hold() {
        // A takes a loud sample and is 200 frames into its 480-frame hold
        // when B adopts it; from then on they play alike, while a limiter
        // that adopted nothing plays the same audio louder.
        let mut a = limiter(-1.0, 100.0, FS);
        a.frame(&mut [3.0, -3.0]);
        for _ in 0..200 {
            a.frame(&mut [0.1, 0.1]);
        }
        let mut b = limiter(-1.0, 100.0, FS);
        b.adopt(&a);
        let mut cold = limiter(-1.0, 100.0, FS);
        let mut louder = 0;
        for n in 0..48_000 {
            let x = (0.5 * (2.0 * PI * 440.0 * n as f64 / FS).sin()) as f32;
            let (mut fa, mut fb, mut fc) = ([x, -x], [x, -x], [x, -x]);
            a.frame(&mut fa);
            b.frame(&mut fb);
            cold.frame(&mut fc);
            assert_eq!(fb, fa, "frame {n}");
            if fc[0].abs() > fa[0].abs() {
                louder += 1;
            }
        }
        assert!(louder > 1000, "{louder}");
    }

    /// The limiter as it was before lookahead and true peak, to hold the
    /// zero-latency mode to.
    struct OldLimiter {
        ceiling: f32,
        release: f32,
        hold: u32,
        reduction: f32,
        hold_left: u32,
        gain: f32,
    }

    impl OldLimiter {
        fn new(params: &LimiterParams, sample_rate: f64) -> Self {
            Self {
                ceiling: 10f64.powf(f64::from(params.ceiling_db) / 20.0) as f32,
                release: (-1.0 / (sample_rate * f64::from(params.release_ms) / 1000.0)).exp()
                    as f32,
                hold: (HOLD_SECONDS * sample_rate).round() as u32,
                reduction: 0.0,
                hold_left: 0,
                gain: 1.0,
            }
        }

        fn frame(&mut self, frame: &mut [f32]) {
            if self.hold_left > 0 {
                self.hold_left -= 1;
            } else {
                self.reduction *= self.release;
            }
            let mut gain = 1.0 - self.reduction;
            if gain == 1.0 {
                self.reduction = 0.0;
            }
            let peak = frame
                .iter()
                .filter(|x| x.is_finite())
                .fold(0.0f32, |peak, x| peak.max(x.abs()));
            if peak * gain > self.ceiling {
                gain = self.ceiling / peak;
                while peak * gain > self.ceiling {
                    gain = gain.next_down();
                }
                self.reduction = 1.0 - gain;
                self.hold_left = self.hold;
            }
            if gain < 1.0 {
                for x in frame {
                    *x *= gain;
                }
            }
            self.gain = gain;
        }
    }

    fn ahead(lookahead_ms: f32, true_peak: bool, ceiling_db: f32, sample_rate: f64) -> Limiter {
        let params = LimiterParams {
            enabled: true,
            ceiling_db,
            release_ms: 100.0,
            lookahead_ms,
            true_peak,
        };
        Limiter::new(&params, sample_rate)
    }

    /// `input` through `limiter` and then as much silence as it delays by,
    /// with that delay taken off: the output lined up with `input`.
    fn through(limiter: &mut Limiter, input: &[f32], channels: usize) -> Vec<f32> {
        let delay = limiter.latency_frames() * channels;
        let mut out = input.to_vec();
        out.resize(input.len() + delay, 0.0);
        run(limiter, &mut out, channels);
        out.split_off(delay)
    }

    /// The largest change between neighbouring values.
    fn max_step(x: &[f32]) -> f32 {
        x.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    /// The largest change between neighbouring gains, in dB.
    fn max_step_db(gains: &[f32]) -> f64 {
        gains
            .windows(2)
            .map(|w| (20.0 * (f64::from(w[1]) / f64::from(w[0])).log10()).abs())
            .fold(0.0, f64::max)
    }

    /// The peak of `x` read at `factor` points per sample through a long
    /// Kaiser-windowed sinc in f64, away from its ends.
    fn oversampled_peak(x: &[f32], factor: usize) -> f64 {
        let (half, beta) = (64, 10.0);
        let kernel: Vec<Vec<f64>> = (0..factor)
            .map(|q| {
                (0..2 * half)
                    .map(|j| {
                        // From the point k + q / factor to x[k + 1 - half + j].
                        let t = half as f64 - 1.0 - j as f64 + q as f64 / factor as f64;
                        let sinc = if t == 0.0 {
                            1.0
                        } else {
                            (PI * t).sin() / (PI * t)
                        };
                        let r = t / half as f64;
                        sinc * bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta)
                    })
                    .collect()
            })
            .collect();
        let mut peak = 0.0f64;
        for k in half..x.len() - half {
            for taps in &kernel {
                let y: f64 = x[k + 1 - half..=k + half]
                    .iter()
                    .zip(taps)
                    .map(|(v, c)| f64::from(*v) * c)
                    .sum();
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn without_lookahead_or_true_peak_it_is_the_limiter_it_was() {
        // Noise whose level jumps between -60 and +60 dB every 300 frames,
        // on 1, 2 and 6 channels, against the algorithm before lookahead.
        for channels in [1, 2, 6] {
            for (ceiling_db, release_ms, rate) in [
                (-1.0, 100.0, 48_000.0),
                (-6.0, 5.0, 44_100.0),
                (0.0, 2000.0, 192_000.0),
            ] {
                let mut rng = Rng(channels as u64);
                let mut scale = 1.0;
                let input: Vec<f32> = (0..channels * 30_000)
                    .map(|i| {
                        if i % (channels * 300) == 0 {
                            scale = 10f32.powf(3.0 * rng.next());
                        }
                        scale * rng.next()
                    })
                    .collect();
                let params = LimiterParams {
                    enabled: true,
                    ceiling_db,
                    release_ms,
                    lookahead_ms: 0.0,
                    true_peak: false,
                };
                let (mut new, mut old) =
                    (Limiter::new(&params, rate), OldLimiter::new(&params, rate));
                assert_eq!(new.latency_frames(), 0);
                let (mut a, mut b) = (input.clone(), input);
                for (n, (fa, fb)) in a
                    .chunks_exact_mut(channels)
                    .zip(b.chunks_exact_mut(channels))
                    .enumerate()
                {
                    new.frame(fa);
                    old.frame(fb);
                    assert_eq!(new.gain().to_bits(), old.gain.to_bits(), "frame {n}");
                }
                assert_eq!(bits(&a), bits(&b), "{channels} channels, {ceiling_db} dB");
            }
        }
    }

    #[test]
    fn latency_is_the_lookahead_plus_8_frames_for_true_peak() {
        for (rate, lookahead_ms, true_peak, want) in [
            (48_000.0, 1.5, true, 80),
            (44_100.0, 1.5, true, 74),
            (96_000.0, 1.5, true, 152),
            (48_000.0, 1.5, false, 72),
            (48_000.0, 0.0, true, 8),
            (48_000.0, 0.0, false, 0),
            (48_000.0, 0.001, false, 1),
            (192_000.0, 10.0, true, 1928),
        ] {
            let lim = ahead(lookahead_ms, true_peak, -1.0, rate);
            assert_eq!(lim.latency_frames(), want, "{rate} Hz, {lookahead_ms} ms");
        }
        for rate in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 176_400.0, 192_000.0] {
            let max = Limiter::max_latency_frames(rate);
            assert_eq!(ahead(10.0, true, -1.0, rate).latency_frames(), max);
            for tenths in 0..=100 {
                for true_peak in [false, true] {
                    let lim = ahead(tenths as f32 / 10.0, true_peak, -1.0, rate);
                    assert!(lim.latency_frames() <= max, "{rate} Hz");
                }
            }
        }
    }

    #[test]
    fn audio_under_the_ceiling_comes_out_delayed_bit_for_bit() {
        // Samples up to the ceiling itself; with true peak at most 1 / 2.2 of
        // it, which no interpolated point can reach the ceiling from, since
        // each phase's taps add up to less than 2.2 in magnitude.
        for rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let detector = TruePeak::new(rate);
            assert!(
                detector
                    .phases
                    .iter()
                    .all(|taps| taps.iter().map(|c| c.abs()).sum::<f32>() < 2.2)
            );
            for (lookahead_ms, true_peak) in [(0.0, true), (0.1, false), (1.5, true), (10.0, true)]
            {
                for channels in [1, 2, 6] {
                    let ceiling = level(-1.0);
                    let scale = if true_peak { ceiling / 2.2 } else { ceiling };
                    let mut rng = Rng(4);
                    let mut input: Vec<f32> =
                        (0..channels * 6000).map(|_| scale * rng.next()).collect();
                    input[0] = scale;
                    input[channels * 3000] = -scale;
                    let mut lim = ahead(lookahead_ms, true_peak, -1.0, rate);
                    let delay = channels * lim.latency_frames();
                    assert!(delay > 0);
                    let mut out = input.clone();
                    run(&mut lim, &mut out, channels);
                    let what = format!("{rate} Hz, {lookahead_ms} ms, {channels} ch");
                    assert!(out[..delay].iter().all(|x| *x == 0.0), "{what}");
                    assert_eq!(
                        bits(&out[delay..]),
                        bits(&input[..input.len() - delay]),
                        "{what}"
                    );
                }
            }
        }
    }

    /// Abrupt loud starts: (name, channels, samples).
    fn onsets(rate: f64) -> Vec<(&'static str, usize, Vec<f32>)> {
        let n = (0.1 * rate) as usize;
        let mut rng = Rng(5);
        let square = (0..n)
            .map(|i| match i {
                _ if i < n / 4 => 0.0,
                _ if (i / 50) % 2 == 0 => 1.0,
                _ => -1.0,
            })
            .collect();
        // 2 ms bursts at +12 dB every 10 ms over noise 20 dB down.
        let (burst, every) = ((0.002 * rate) as usize, (0.01 * rate) as usize);
        let bursts = (0..2 * n)
            .map(|i| {
                let scale = if (i / 2) % every < burst { 4.0 } else { 0.1 };
                scale * rng.next()
            })
            .collect();
        let mut spike = vec![0.0; 6 * n];
        spike[6 * (n / 2) + 3] = 8.0;
        vec![
            ("full-scale square after silence", 1, square),
            ("+12 dB noise bursts", 2, bursts),
            ("a one-sample spike", 6, spike),
        ]
    }

    #[test]
    fn with_lookahead_or_true_peak_no_sample_ever_passes_the_ceiling() {
        for rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            for (lookahead_ms, true_peak) in [
                (0.0, true),
                (0.1, false),
                (0.1, true),
                (1.5, false),
                (1.5, true),
                (10.0, false),
                (10.0, true),
            ] {
                for ceiling_db in [-6.0, -1.0] {
                    let ceiling = level(ceiling_db);
                    let mut inputs = onsets(rate);
                    if ceiling_db == -1.0 && (rate == 44_100.0 || rate == 192_000.0) {
                        inputs.extend(adversarial(ceiling));
                    }
                    for (name, channels, input) in inputs {
                        let mut lim = ahead(lookahead_ms, true_peak, ceiling_db, rate);
                        let out = through(&mut lim, &input, channels);
                        let what = format!(
                            "{name}, {rate} Hz, {lookahead_ms} ms, true peak {true_peak}, {ceiling_db} dB"
                        );
                        if let Some(i) = out.iter().position(|x| x.abs() > ceiling) {
                            panic!("{what}: sample {i} is {} over {ceiling}", out[i]);
                        }
                        assert_ne!(bits(&out), bits(&input), "{what}: nothing was limited");
                    }
                }
            }
        }
    }

    #[test]
    fn at_an_abrupt_start_the_gain_comes_down_over_the_lookahead() {
        // A 1 kHz square at full scale and at +20 dB after silence, 1.5 ms
        // of lookahead at 48 kHz: the gain falls by at most
        // (1 - its lowest) / W a frame, W = 73, where the limiter without
        // lookahead takes all of it, 1 - ceiling / amplitude, in one step.
        let w = 73.0f32;
        for amp in [1.0f32, 10.0] {
            for true_peak in [false, true] {
                let mut lim = ahead(1.5, true_peak, -1.0, FS);
                assert_eq!(lim.lookahead.as_ref().map(|l| l.len), Some(73));
                let mut old = limiter(-1.0, 100.0, FS);
                let (mut gains, mut old_gains) = (Vec::new(), Vec::new());
                for i in 0..4800 {
                    let x = match i {
                        _ if i < 1000 => 0.0,
                        _ if (i / 24) % 2 == 0 => amp,
                        _ => -amp,
                    };
                    lim.frame(&mut [x]);
                    gains.push(lim.gain());
                    old.frame(&mut [x]);
                    old_gains.push(old.gain());
                }
                let lowest = gains.iter().fold(1.0f32, |a, &g| a.min(g));
                let (step, old_step) = (max_step(&gains), max_step(&old_gains));
                eprintln!(
                    "onset at {:+.0} dBFS, true peak {true_peak}: largest gain step {step:.5} \
                     ({:.3} dB), without lookahead {old_step:.5} ({:.3} dB)",
                    20.0 * amp.log10(),
                    max_step_db(&gains),
                    max_step_db(&old_gains),
                );
                assert!(lowest <= level(-1.0) / amp, "{lowest}");
                assert!(
                    step <= (1.0 - lowest) / w + 1e-6,
                    "{step} > {}",
                    (1.0 - lowest) / w
                );
                assert!(old_step >= 1.0 - level(-1.0) / amp - 1e-6, "{old_step}");
            }
        }
    }

    #[test]
    fn true_peak_turns_down_a_peak_between_samples() {
        // A sine at fs / 4 with 45 degrees of phase: its samples sit at 0.707
        // of its 0 dBFS peak, 3 dB down and under the -1 dB ceiling, while
        // the peak between them is over it. At 192 kHz only the midpoint is
        // estimated, which is where these peaks are.
        for rate in [44_100.0, 48_000.0, 192_000.0] {
            let n = (0.05 * rate) as usize;
            let input: Vec<f32> = (0..n)
                .map(|i| (PI / 2.0 * i as f64 + PI / 4.0).sin() as f32)
                .collect();
            assert!(input.iter().all(|x| x.abs() < level(-1.0)));
            assert!((oversampled_peak(&input[..1024], 16) - 1.0).abs() < 1e-3);

            let mut off = ahead(1.5, false, -1.0, rate);
            let delay = off.latency_frames();
            let mut out = input.clone();
            run(&mut off, &mut out, 1);
            assert_eq!(bits(&out[delay..]), bits(&input[..n - delay]), "{rate} Hz");

            let mut on = ahead(1.5, true, -1.0, rate);
            let mut out = input.clone();
            run(&mut on, &mut out, 1);
            let peak_db = 20.0 * oversampled_peak(&out[n / 2..n / 2 + 1024], 16).log10();
            eprintln!("fs/4 sine at {rate} Hz with true peak: {peak_db:.3} dBTP");
            assert!((peak_db + 1.0).abs() <= 0.3, "{rate} Hz: {peak_db} dB");
        }
    }

    #[test]
    fn true_peak_reads_a_tone_within_0_4_db_of_its_peak() {
        // A cosine peaking at frame 40 plus a fraction, for every fraction in
        // 32 steps and tones up to 0.417 fs (20 kHz at 48 kHz) or, at 192
        // kHz, 20 kHz: the peak reported for frames 40 and 41, between which
        // the tone peaks, against its amplitude of 1.
        for (rate, top) in [
            (48_000.0, 20_000.0),
            (44_100.0, 18_375.0),
            (192_000.0, 20_000.0),
        ] {
            let (mut under, mut over) = (1.0f32, 1.0f32);
            for step in 1..=100 {
                let f = top * f64::from(step) / 100.0;
                for fraction in 0..32 {
                    let at = 40.0 + f64::from(fraction) / 32.0;
                    let mut detector = TruePeak::new(rate);
                    let mut read = 0.0f32;
                    for i in 0..42 + TAPS / 2 {
                        let x = (2.0 * PI * f * (i as f64 - at) / rate).cos() as f32;
                        let peak = detector.push(&[x]);
                        if i >= 40 + TAPS / 2 {
                            read = read.max(peak);
                        }
                    }
                    under = under.min(read);
                    over = over.max(read);
                }
            }
            let (under_db, over_db) = (20.0 * under.log10(), 20.0 * over.log10());
            eprintln!(
                "true peak at {rate} Hz up to {top} Hz: reads {under_db:+.3} to {over_db:+.3} dB"
            );
            assert!(
                under_db > -0.4 && over_db < 0.1,
                "{rate} Hz: {under_db}, {over_db}"
            );
        }
    }

    #[test]
    fn a_limiter_that_adopts_another_mid_stream_plays_what_it_would_have() {
        // Loud stereo noise that A limits; B takes over 1000 frames in, with
        // audio and peaks in A's delay and window, and from then on plays
        // exactly what A does. A limiter starting cold drops the audio that
        // was in the delay.
        let mut rng = Rng(6);
        let input: Vec<f32> = (0..2 * 12_000).map(|_| 3.0 * rng.next()).collect();
        for (lookahead_ms, true_peak) in [(1.5, true), (1.5, false), (0.0, true), (10.0, true)] {
            let mut a = ahead(lookahead_ms, true_peak, -1.0, FS);
            let mut played = input.clone();
            let (head, tail) = played.split_at_mut(2000);
            run(&mut a, head, 2);
            let mut b = ahead(lookahead_ms, true_peak, -1.0, FS);
            b.adopt(&a);
            let mut adopted = tail.to_vec();
            run(&mut a, tail, 2);
            run(&mut b, &mut adopted, 2);
            assert_eq!(bits(&adopted), bits(tail), "{lookahead_ms} ms, {true_peak}");

            let mut cold = input[2000..].to_vec();
            let mut c = ahead(lookahead_ms, true_peak, -1.0, FS);
            run(&mut c, &mut cold, 2);
            let delay = 2 * c.latency_frames();
            assert!(cold[..delay].iter().all(|x| *x == 0.0));
            assert!(tail[..delay].iter().all(|x| *x != 0.0));
        }
    }

    #[test]
    fn adopting_with_a_lower_ceiling_keeps_the_audio_and_holds_the_new_ceiling() {
        // B's ceiling is 6 dB under A's: from its first frame B plays the
        // audio in A's delay, each frame at one gain, and never over its own
        // ceiling, the peaks A had already seen coming included.
        let mut rng = Rng(7);
        let input: Vec<f32> = (0..2 * 8000).map(|_| 1.5 * rng.next()).collect();
        let mut a = ahead(1.5, true, -1.0, FS);
        let mut head = input[..2000].to_vec();
        run(&mut a, &mut head, 2);
        let mut b = ahead(1.5, true, -7.0, FS);
        b.adopt(&a);
        let delay = b.latency_frames();
        for (n, frame) in input[2000..].chunks_exact(2).enumerate() {
            let mut out = [frame[0], frame[1]];
            b.frame(&mut out);
            let source = &input[2000 + 2 * n - 2 * delay..][..2];
            assert_eq!(
                out,
                [source[0] * b.gain(), source[1] * b.gain()],
                "frame {n}"
            );
            assert!(out.iter().all(|x| x.abs() <= level(-7.0)), "frame {n}");
        }
    }

    #[test]
    fn adopting_across_a_different_lookahead_carries_the_gain_only() {
        // A, with no lookahead, takes a loud sample; B, with lookahead and
        // true peak, adopts it in the hold. B's delay starts empty, and its
        // gain then follows A's through the hold and release.
        let mut a = ahead(0.0, false, -1.0, FS);
        a.frame(&mut [3.0, -3.0]);
        for _ in 0..100 {
            a.frame(&mut [0.1, 0.1]);
        }
        let mut b = ahead(1.5, true, -1.0, FS);
        b.adopt(&a);
        for n in 0..10_000 {
            let (mut fa, mut fb) = ([0.25, -0.25], [0.25, -0.25]);
            a.frame(&mut fa);
            b.frame(&mut fb);
            assert_eq!(b.gain(), a.gain(), "frame {n}");
            if n < b.latency_frames() {
                assert_eq!(fb, [0.0, 0.0]);
            } else {
                assert_eq!(fb, fa, "frame {n}");
            }
        }
        assert!(a.gain() < 1.0);
    }

    #[test]
    fn an_input_shorter_than_the_lookahead_comes_out_whole_and_limited() {
        // Ten frames at up to +12 dB against 72 frames of lookahead: they
        // come out once silence pushes them through, each frame at one gain
        // and under the ceiling.
        let mut rng = Rng(8);
        let input: Vec<f32> = (0..2 * 10).map(|_| 4.0 * rng.next()).collect();
        for true_peak in [false, true] {
            let mut lim = ahead(1.5, true_peak, -1.0, FS);
            let latency = lim.latency_frames();
            let mut played = Vec::new();
            for n in 0..10 + latency {
                let mut frame = match input.get(2 * n..2 * n + 2) {
                    Some(source) => [source[0], source[1]],
                    None => [0.0, 0.0],
                };
                lim.frame(&mut frame);
                if n < latency {
                    assert_eq!(frame, [0.0, 0.0], "frame {n}");
                } else {
                    played.push((frame, lim.gain()));
                }
            }
            for ((out, gain), source) in played.iter().zip(input.chunks_exact(2)) {
                assert!(*gain < 1.0, "{gain}");
                assert_eq!(*out, [source[0] * gain, source[1] * gain]);
                assert!(out.iter().all(|x| x.abs() <= level(-1.0)), "{out:?}");
            }
        }
    }

    #[test]
    fn with_lookahead_a_non_finite_sample_neither_moves_the_gain_nor_turns_finite() {
        // As without lookahead: loud stereo noise with NaN and infinities on
        // single samples comes out as if they had been 0, apart from them.
        let mut rng = Rng(3);
        let input: Vec<f32> = (0..2 * 20_000).map(|_| 1.5 * rng.next()).collect();
        let bad = [
            (2001, f32::NAN),
            (9000, f32::INFINITY),
            (15_333, f32::NEG_INFINITY),
        ];
        let (mut with, mut zeroed) = (input.clone(), input);
        for (i, v) in bad {
            with[i] = v;
            zeroed[i] = 0.0;
        }
        let with = through(&mut ahead(1.5, true, -1.0, FS), &with, 2);
        let zeroed = through(&mut ahead(1.5, true, -1.0, FS), &zeroed, 2);
        for (i, (w, z)) in with.iter().zip(&zeroed).enumerate() {
            match bad.iter().find(|(at, _)| *at == i) {
                Some((_, v)) if v.is_nan() => assert!(w.is_nan(), "sample {i}"),
                Some((_, v)) => assert_eq!(w, v, "sample {i}"),
                None => assert_eq!(w.to_bits(), z.to_bits(), "sample {i}"),
            }
        }
        // From rest, an infinity beside a quiet sample leaves both alone,
        // and so the quiet audio after it.
        let out = through(
            &mut ahead(1.5, true, -1.0, FS),
            &[f32::INFINITY, 0.5, 0.3, -0.3],
            2,
        );
        assert_eq!(out, [f32::INFINITY, 0.5, 0.3, -0.3]);
    }

    #[test]
    fn with_lookahead_gain_is_what_the_frame_out_was_multiplied_by() {
        // A probe channel at 2^-10, which any gain scales exactly, shows
        // what the frame leaving the delay was multiplied by.
        let mut lim = ahead(1.5, true, -1.0, FS);
        let probe = 2f32.powi(-10);
        let mut rng = Rng(9);
        let mut lowest = 1.0f32;
        for n in 0..20_000 {
            let mut frame = [3.0 * rng.next(), probe];
            lim.frame(&mut frame);
            if n >= lim.latency_frames() {
                assert_eq!(frame[1], probe * lim.gain(), "frame {n}");
            }
            lowest = lowest.min(lim.gain());
        }
        assert!(lowest < 0.5, "{lowest}");
    }

    #[test]
    #[ignore = "timing, run by hand with --release"]
    fn cost_per_frame() {
        use std::time::Instant;
        let mut rng = Rng(10);
        let input: Vec<f32> = (0..2 * 480_000).map(|_| 1.5 * rng.next()).collect();
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut old = OldLimiter::new(
                &LimiterParams {
                    lookahead_ms: 0.0,
                    true_peak: false,
                    ..LimiterParams::default()
                },
                FS,
            );
            let mut x = input.clone();
            let start = Instant::now();
            for frame in x.chunks_exact_mut(2) {
                old.frame(frame);
            }
            best = best.min(start.elapsed().as_secs_f64());
            std::hint::black_box(&x);
        }
        eprintln!(
            "before lookahead: {:.1} ns per stereo frame",
            best * 1e9 / 480_000.0
        );
        for (name, lookahead_ms, true_peak) in [
            ("no lookahead", 0.0, false),
            ("1.5 ms", 1.5, false),
            ("1.5 ms + true peak", 1.5, true),
        ] {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let mut lim = ahead(lookahead_ms, true_peak, -1.0, FS);
                let mut x = input.clone();
                let start = Instant::now();
                run(&mut lim, &mut x, 2);
                best = best.min(start.elapsed().as_secs_f64());
                std::hint::black_box(&x);
            }
            eprintln!("{name}: {:.1} ns per stereo frame", best * 1e9 / 480_000.0);
        }
    }
}
