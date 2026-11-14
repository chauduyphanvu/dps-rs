//! Leveler, ahead of the rest of the chain: a slow gain that steers the
//! input's loudness towards a target, within a set boost and cut and at set
//! rates up and down, so quiet and loud sources reach the compressors at
//! about the same level. One gain for all channels, so the stereo image does
//! not shift.
//!
//! It measures the input, K-weighted (BS.1770), in 100 ms blocks, and its
//! momentary loudness over the last four. The programme loudness is the
//! power mean of the last 3 s (BS.1770's short-term window) of blocks whose
//! momentary loudness passed the gate. A silent block in a window that
//! passed counts, so the gaps between words and the ends of pauses weigh
//! about as they do among BS.1770's gated 400 ms blocks. The gain wants
//! `target - programme` and moves towards it by a fixed step each frame, so
//! it ramps in dB without steps, and only after an audible block that
//! passed:
//!
//! - It starts at 0 dB and stays there until it has measured 1 s.
//! - A block under -50 LUFS is silence (a paused player, the gap between
//!   tracks, room tone), and the gain holds through it however long it
//!   lasts, so the next sound starts where the last one left off. Momentary
//!   loudness under -50 LUFS is not measured and empties the momentary
//!   window, so the first sound after a pause is measured on its own rather
//!   than diluted by the silence before it.
//! - Momentary loudness 10 LU under the programme, BS.1770's relative gate,
//!   is a sudden drop: a fade or a soft bridge, which the rise would
//!   otherwise pump up. It is not measured and holds the gain until it has
//!   lasted 5 s, longer than a fade or a break, and is then taken for the
//!   new programme (another app, a quieter track).
//! - A loud onset after quiet material finds the gain up to boost + cut
//!   (18 dB by default) above what it wants. While the momentary loudness
//!   after the gain is 8 LU or more over the target, the gain falls at
//!   24 dB/s, or the set fall rate if faster: at the default 4 dB/s that
//!   excess would take 4.5 s to come off, all the while driving the
//!   compressors and the limiter far past their design point. Programme
//!   held at the target seldom swings 8 LU over it, so ordinary material
//!   falls at the set rate.

use crate::loudness::{Blocks, lufs_of};
use crate::profile::LevelerParams;

const PROGRAMME_BLOCKS: usize = 30;
const MOMENTARY_BLOCKS: usize = 4;
const START_BLOCKS: usize = 10;
const SILENCE_LUFS: f64 = -50.0;
const DROP_LU: f64 = 10.0;
const DROP_HOLD_BLOCKS: u32 = 50;
const LOUD_LU: f64 = 8.0;
const FAST_FALL_DB_PER_S: f32 = 24.0;

/// The last few block powers, in a ring.
#[derive(Clone, Copy)]
struct Recent<const N: usize> {
    powers: [f64; N],
    next: usize,
    heard: usize,
}

impl<const N: usize> Recent<N> {
    const EMPTY: Self = Self {
        powers: [0.0; N],
        next: 0,
        heard: 0,
    };

    fn push(&mut self, power: f64) {
        self.powers[self.next] = power;
        self.next = (self.next + 1) % N;
        self.heard = (self.heard + 1).min(N);
    }

    /// The power mean of what has been heard, in LUFS; -inf before anything.
    fn lufs(&self) -> f64 {
        lufs_of(self.powers.iter().sum::<f64>() / self.heard.max(1) as f64)
    }
}

pub struct Leveler {
    target_lufs: f64,
    max_boost_db: f64,
    max_cut_db: f64,
    /// Steps per frame, in dB.
    rise: f64,
    fall: f64,
    fast_fall: f64,
    blocks: Blocks,
    momentary: Recent<MOMENTARY_BLOCKS>,
    programme: Recent<PROGRAMME_BLOCKS>,
    /// Blocks whose momentary loudness was a drop, since the last one that
    /// was not.
    dropped: u32,
    /// Set at each block for the frames up to the next: whether the gain
    /// moves, where to and how fast down.
    moving: bool,
    wanted_db: f64,
    down: f64,
    gain_db: f64,
    gain: f32,
}

impl Leveler {
    pub fn new(params: &LevelerParams, sample_rate: f64, channels: usize) -> Self {
        let per_frame = |db_per_s: f32| f64::from(db_per_s) / sample_rate;
        Self {
            target_lufs: f64::from(params.target_lufs),
            max_boost_db: f64::from(params.max_boost_db),
            max_cut_db: f64::from(params.max_cut_db),
            rise: per_frame(params.rise_db_per_s),
            fall: per_frame(params.fall_db_per_s),
            fast_fall: per_frame(params.fall_db_per_s.max(FAST_FALL_DB_PER_S)),
            blocks: Blocks::new(sample_rate, channels),
            momentary: Recent::EMPTY,
            programme: Recent::EMPTY,
            dropped: 0,
            moving: false,
            wanted_db: 0.0,
            down: 0.0,
            gain_db: 0.0,
            gain: 1.0,
        }
    }

    /// One interleaved frame, all channels, in place. A non-finite sample
    /// passes times the gain and is measured as 0. No allocation.
    pub fn frame(&mut self, frame: &mut [f32]) {
        if let Some(power) = self.blocks.frame(frame) {
            self.block(power);
        }
        if self.moving && self.gain_db != self.wanted_db {
            let gap = self.wanted_db - self.gain_db;
            self.gain_db = if gap > self.rise {
                self.gain_db + self.rise
            } else if gap < -self.down {
                self.gain_db - self.down
            } else {
                self.wanted_db
            };
            self.gain = 10f64.powf(self.gain_db / 20.0) as f32;
        }
        for x in frame {
            *x *= self.gain;
        }
    }

    fn block(&mut self, power: f64) {
        self.moving = false;
        self.momentary.push(power);
        let momentary = self.momentary.lufs();
        if momentary < SILENCE_LUFS {
            self.momentary = Recent::EMPTY;
            return;
        }
        if self.programme.heard >= START_BLOCKS && momentary < self.programme.lufs() - DROP_LU {
            self.dropped = self.dropped.saturating_add(1);
            if self.dropped <= DROP_HOLD_BLOCKS {
                return;
            }
        } else {
            self.dropped = 0;
        }
        self.programme.push(power);
        if self.programme.heard < START_BLOCKS || lufs_of(power) < SILENCE_LUFS {
            return;
        }
        self.wanted_db = (self.target_lufs - self.programme.lufs())
            .max(-self.max_cut_db)
            .min(self.max_boost_db);
        self.down = if momentary + self.gain_db >= self.target_lufs + LOUD_LU {
            self.fast_fall
        } else {
            self.fall
        };
        self.moving = true;
    }

    /// The gain being applied now, in dB.
    pub fn gain_db(&self) -> f32 {
        self.gain_db as f32
    }

    /// Starts where `from`, a leveler at the same rate, is: its gain and all
    /// it has measured, so fed the same input both go on alike. No
    /// allocation.
    pub fn adopt(&mut self, from: &Self) {
        self.blocks.adopt(&from.blocks);
        self.momentary = from.momentary;
        self.programme = from.programme;
        self.dropped = from.dropped;
        self.moving = from.moving;
        self.wanted_db = from.wanted_db;
        self.down = from.down;
        self.gain_db = from.gain_db;
        self.gain = from.gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loudness::integrated_lufs;
    use crate::loudness::tests::{Rng, at_lufs};

    const FS: u32 = 48_000;
    const FS_HZ: f64 = FS as f64;

    fn params() -> LevelerParams {
        LevelerParams {
            enabled: true,
            ..LevelerParams::default()
        }
    }

    fn frames(seconds: f64) -> usize {
        (seconds * FS_HZ).round() as usize
    }

    /// Interleaved white noise, uncorrelated between channels, at `lufs`.
    fn noise(channels: usize, seconds: f64, lufs: f64, seed: u64) -> Vec<f32> {
        let mut rng = Rng(seed);
        let x: Vec<f32> = (0..channels * frames(seconds))
            .map(|_| rng.next() as f32)
            .collect();
        at_lufs(&x, channels, FS, lufs)
    }

    /// Runs `x` through `leveler` in place; the gain in dB after each frame.
    fn run(leveler: &mut Leveler, x: &mut [f32], channels: usize) -> Vec<f32> {
        x.chunks_exact_mut(channels)
            .map(|frame| {
                leveler.frame(frame);
                leveler.gain_db()
            })
            .collect()
    }

    fn assert_db(got: f32, want: f64, tol: f64, what: &str) {
        let got = f64::from(got);
        assert!((got - want).abs() <= tol, "{what}: {got} dB, want {want}");
    }

    /// Fails if the gain anywhere moves more than the rates allow over a
    /// second, or more than one frame's step between two frames.
    fn assert_rates(gains: &[f32], up: f64, down: f64, what: &str) {
        let slack = 1e-3;
        for (i, w) in gains.windows(2).enumerate() {
            let step = f64::from(w[1] - w[0]);
            assert!(
                step <= up / FS_HZ + 1e-5 && step >= -down / FS_HZ - 1e-5,
                "{what}: frame {i} steps {step} dB"
            );
        }
        let second = frames(1.0);
        for (i, (a, b)) in gains.iter().zip(&gains[second..]).enumerate() {
            let change = f64::from(b - a);
            assert!(
                change <= up + slack && change >= -down - slack,
                "{what}: {change} dB in the second from frame {i}"
            );
        }
    }

    #[test]
    fn steady_noise_settles_at_the_target_within_the_caps_at_the_set_rates() {
        // 1 s (ten blocks) to start, then the rise or fall rate: 12 s up to
        // the +12 dB cap for -30 LUFS (which would want +16), 6 s up for
        // -20 LUFS and 1.5 s down for -8 LUFS.
        let p = params();
        for (lufs, want, settled) in [(-30.0, 12.0, 13.0), (-20.0, 6.0, 7.0), (-8.0, -6.0, 2.5)] {
            let mut x = noise(2, settled + 4.0, lufs, 1);
            let input = x.clone();
            let mut leveler = Leveler::new(&p, FS_HZ, 2);
            let gains = run(&mut leveler, &mut x, 2);
            let what = format!("{lufs} LUFS");
            assert!(gains[..frames(1.0) - 1].iter().all(|g| *g == 0.0), "{what}");
            assert_eq!(x[..2 * frames(0.99)], input[..2 * frames(0.99)], "{what}");
            // Not there a tenth of a second early, there by 20 ms late.
            assert!(
                (f64::from(gains[frames(settled - 0.1)]) - want).abs() > 0.05,
                "{what}"
            );
            for g in &gains[frames(settled + 0.02)..] {
                assert_db(*g, want, 0.05, &what);
            }
            assert_rates(&gains, 1.0, 4.0, &what);
        }
    }

    #[test]
    fn output_of_a_steady_signal_ends_at_the_target() {
        for (lufs, target) in [
            (-20.0, -14.0),
            (-24.0, -14.0),
            (-9.0, -14.0),
            (-30.0, -23.0),
        ] {
            let p = LevelerParams {
                target_lufs: target as f32,
                ..params()
            };
            let mut x = noise(2, 16.0, lufs, 2);
            run(&mut Leveler::new(&p, FS_HZ, 2), &mut x, 2);
            let settled = &x[2 * frames(12.0)..];
            let got = integrated_lufs(settled, 2, FS);
            assert!(
                (got - target).abs() < 0.5,
                "{lufs} LUFS in, {got} out, target {target}"
            );
        }
    }

    #[test]
    fn speech_with_silent_gaps_ends_at_the_target() {
        // 250 ms syllables of varying level with 150 ms of digital silence
        // between them, and a 1 s pause every 4 s. It comes out 0.2 LU
        // under the target; measured only over the blocks with sound in
        // them, leaving out the gaps an integrated measurement counts, it
        // would come out 1.7 LU under.
        let mut rng = Rng(21);
        let speech: Vec<f32> = (0..frames(30.0))
            .flat_map(|i| {
                let t = i as f64 / FS_HZ;
                let level = if t % 0.4 < 0.25 && t % 4.0 < 3.0 {
                    0.5 + 0.5 * ((t / 0.4).floor() * 1.7).sin().abs()
                } else {
                    0.0
                };
                let v = (level * rng.next()) as f32;
                [v, v]
            })
            .collect();
        for lufs in [-20.0, -8.0] {
            let mut x = at_lufs(&speech, 2, FS, lufs);
            run(&mut Leveler::new(&params(), FS_HZ, 2), &mut x, 2);
            let got = integrated_lufs(&x[2 * frames(18.0)..], 2, FS);
            assert!((got + 14.0).abs() < 0.5, "{lufs} LUFS in, {got} out");
        }
    }

    #[test]
    fn silence_holds_the_gain_and_a_loud_onset_falls_fast() {
        // 15 s at -30 LUFS takes the gain to the +12 dB cap; 5 s of
        // silence leaves it there to the bit. Then -8 LUFS: the first
        // block plays at +12 dB, then the gain falls at 24 dB/s until the
        // output's momentary loudness is under 8 LU over the target, and at
        // 4 dB/s from there to -6 dB.
        let mut leveler = Leveler::new(&params(), FS_HZ, 2);
        let mut quiet = noise(2, 15.0, -30.0, 3);
        run(&mut leveler, &mut quiet, 2);
        assert_eq!(leveler.gain_db(), 12.0);
        let mut silence = vec![0.0f32; 2 * frames(5.0)];
        let held = run(&mut leveler, &mut silence, 2);
        assert!(held.iter().all(|g| *g == 12.0));

        let mut loud = noise(2, 6.0, -8.0, 4);
        let gains = run(&mut leveler, &mut loud, 2);
        assert!(gains[..frames(0.1) - 1].iter().all(|g| *g == 12.0));
        assert_rates(&gains, 1.0, 24.0, "onset");
        // Fast at first: 24 dB/s for the next 0.3 s.
        let fell = f64::from(gains[frames(0.1)] - gains[frames(0.4)]);
        assert!((fell - 7.2).abs() < 0.1, "{fell} dB");
        // At -6 dB once the quiet blocks have left the 3 s programme.
        for g in &gains[frames(3.5)..] {
            assert_db(*g, -6.0, 0.05, "after the onset");
        }
    }

    #[test]
    fn a_sudden_drop_holds_the_gain_for_5_s_then_is_followed() {
        // -20 LUFS settles at +6 dB; a drop to -32 LUFS, 12 LU down, holds
        // it for 5 s, then the programme follows the new level and the gain
        // rises to the +12 dB cap. The momentary loudness still has louder
        // blocks in it for the first three blocks of the drop, which pass
        // the gate and lower the programme 0.4 LU, and each block's decision
        // holds for the next: the gain rises 0.3 dB until 0.4 s, holds
        // until 5.4 s, then rises at 1 dB/s.
        let mut leveler = Leveler::new(&params(), FS_HZ, 2);
        let mut before = noise(2, 10.0, -20.0, 5);
        run(&mut leveler, &mut before, 2);
        let start = leveler.gain_db();
        assert_db(start, 6.0, 0.05, "before the drop");
        let mut drop = noise(2, 16.0, -32.0, 6);
        let gains = run(&mut leveler, &mut drop, 2);
        let held = gains[frames(0.4)];
        assert!(held - start <= 0.31, "{start} to {held}");
        assert!(
            gains[frames(0.4)..frames(5.4) - 1]
                .iter()
                .all(|g| *g == held)
        );
        assert_db(gains[frames(6.4)], f64::from(held) + 1.0, 0.01, "1 s on");
        assert_eq!(*gains.last().unwrap(), 12.0);
        assert_rates(&gains, 1.0, 4.0, "drop");
    }

    #[test]
    fn gain_does_not_depend_on_how_the_stream_is_split() {
        let input: Vec<f32> = [
            noise(2, 4.0, -30.0, 7),
            vec![0.0; 2 * frames(1.0)],
            noise(2, 4.0, -6.0, 8),
        ]
        .concat();
        let mut whole = input.clone();
        let whole_gains = run(&mut Leveler::new(&params(), FS_HZ, 2), &mut whole, 2);
        let mut chunked = input;
        let mut chunked_gains = Vec::new();
        let mut leveler = Leveler::new(&params(), FS_HZ, 2);
        let mut rest = chunked.as_mut_slice();
        for frames in [1, 479, 2, 4801, 64, 12_345].iter().cycle() {
            let take = (frames * 2).min(rest.len());
            let (head, tail) = rest.split_at_mut(take);
            chunked_gains.extend(run(&mut leveler, head, 2));
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        assert_eq!(whole_gains, chunked_gains);
        assert_eq!(whole, chunked);
    }

    #[test]
    fn every_channel_gets_the_same_gain() {
        // Loud noise on the left, quiet on the right.
        let mut rng = Rng(9);
        let input: Vec<f32> = (0..frames(6.0))
            .flat_map(|_| [0.05 * rng.next() as f32, 0.002 * rng.next() as f32])
            .collect();
        let mut out = input.clone();
        run(&mut Leveler::new(&params(), FS_HZ, 2), &mut out, 2);
        let mut moved = false;
        for (i, (o, x)) in out.chunks_exact(2).zip(input.chunks_exact(2)).enumerate() {
            let (l, r) = (o[0] / x[0], o[1] / x[1]);
            assert!((l - r).abs() <= 1e-6 * l, "frame {i}: {l} and {r}");
            moved |= l > 1.1;
        }
        assert!(moved);
    }

    #[test]
    fn adopting_carries_the_gain_and_the_measurement() {
        // A is mid-ramp, mid-block and has a drop's gate count when B takes
        // over; fed the same input from then on (silence, a loud onset),
        // they play alike, while a leveler that adopted nothing differs.
        let input: Vec<f32> = [
            noise(2, 6.0, -26.0, 10),
            noise(2, 1.5, -40.0, 11),
            vec![0.0; 2 * frames(2.0)],
            noise(2, 6.0, -8.0, 12),
            noise(2, 10.0, -30.0, 13),
        ]
        .concat();
        let split = 2 * (frames(7.0) + 1234);
        let p = params();
        let mut a = Leveler::new(&p, FS_HZ, 2);
        let mut head = input[..split].to_vec();
        run(&mut a, &mut head, 2);
        assert!(a.gain_db() > 3.0 && a.dropped > 0, "{}", a.gain_db());
        let mut b = Leveler::new(&p, FS_HZ, 2);
        b.adopt(&a);
        assert_eq!(b.gain_db(), a.gain_db());
        let mut cold = Leveler::new(&p, FS_HZ, 2);
        let (mut ya, mut yb, mut yc) = (
            input[split..].to_vec(),
            input[split..].to_vec(),
            input[split..].to_vec(),
        );
        let ga = run(&mut a, &mut ya, 2);
        let gb = run(&mut b, &mut yb, 2);
        let gc = run(&mut cold, &mut yc, 2);
        assert_eq!(ga, gb);
        assert_eq!(ya, yb);
        assert_ne!(ga, gc);
    }

    #[test]
    fn the_gain_never_steps() {
        // Every change of the gain, the fast fall included, is at most one
        // frame's worth of the fastest rate: under 0.0005 dB at 48 kHz.
        let mut leveler = Leveler::new(&params(), FS_HZ, 2);
        let mut x: Vec<f32> = [
            noise(2, 15.0, -40.0, 14),
            vec![0.0; 2 * frames(1.0)],
            noise(2, 4.0, -5.0, 15),
            noise(2, 8.0, -25.0, 16),
        ]
        .concat();
        let gains = run(&mut leveler, &mut x, 2);
        let largest = gains
            .windows(2)
            .map(|w| f64::from((w[1] - w[0]).abs()))
            .fold(0.0, f64::max);
        assert!(largest <= 24.0 / FS_HZ + 1e-5, "{largest} dB");
        assert!(largest > 23.0 / FS_HZ, "{largest} dB: never fell fast");
    }

    #[test]
    fn works_on_any_channel_count() {
        // -20 LUFS over all channels, each at the same level.
        for channels in [1, 2, 6] {
            let mut x = noise(channels, 12.0, -20.0, 17);
            let mut leveler = Leveler::new(&params(), FS_HZ, channels);
            let gains = run(&mut leveler, &mut x, channels);
            assert_db(*gains.last().unwrap(), 6.0, 0.05, &format!("{channels}"));
            let out = integrated_lufs(&x[channels * frames(8.0)..], channels, FS);
            assert!((out + 14.0).abs() < 0.5, "{channels} channels: {out}");
        }
    }

    #[test]
    fn non_finite_samples_pass_times_the_gain_and_are_not_measured() {
        let input = noise(2, 10.0, -26.0, 18);
        let bad = [
            (2 * frames(2.0) + 1, f32::NAN),
            (2 * frames(3.0), f32::INFINITY),
            (2 * frames(9.0), f32::NEG_INFINITY),
        ];
        let (mut with, mut zeroed) = (input.clone(), input);
        for (i, v) in bad {
            with[i] = v;
            zeroed[i] = 0.0;
        }
        let ga = run(&mut Leveler::new(&params(), FS_HZ, 2), &mut with, 2);
        let gb = run(&mut Leveler::new(&params(), FS_HZ, 2), &mut zeroed, 2);
        assert_eq!(ga, gb);
        for (i, (w, z)) in with.iter().zip(&zeroed).enumerate() {
            match bad.iter().find(|(at, _)| *at == i) {
                Some((_, v)) if v.is_nan() => assert!(w.is_nan()),
                Some((_, v)) => assert_eq!(w, v),
                None => assert_eq!(w, z),
            }
        }
    }

    #[test]
    fn no_boost_and_no_cut_is_exact_passthrough() {
        let p = LevelerParams {
            max_boost_db: 0.0,
            max_cut_db: 0.0,
            ..params()
        };
        let input: Vec<f32> = [noise(2, 3.0, -40.0, 19), noise(2, 3.0, -5.0, 20)].concat();
        let mut out = input.clone();
        run(&mut Leveler::new(&p, FS_HZ, 2), &mut out, 2);
        assert_eq!(out, input);
    }
}
