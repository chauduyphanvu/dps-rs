//! Output peak limiter, the chain's last stage. US 9,350,309 claims a gain
//! reduction "if the gain of the second compressed signal is above a
//! predetermined threshold" and gives no values (docs/patent-spec-later.md
//! section 6).
//!
//! One gain for the whole frame, from its largest sample, so a loud channel
//! pulls the others down with it and the stereo image does not shift. The
//! gain drops at once to what the frame needs, so no sample ever passes the
//! ceiling, not even the first of an abrupt start; there is no lookahead,
//! which would delay the output. It holds for `HOLD_SECONDS` after the last
//! frame that pulled it down, then recovers towards unity with the release
//! time constant.

use crate::profile::LimiterParams;

/// |x| of a 50 Hz tone peaks every 10 ms, so a hold this long keeps bass from
/// 50 Hz up at one gain through each cycle instead of modulating it at twice
/// its frequency, which a release starting at every peak would.
const HOLD_SECONDS: f64 = 0.01;

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
}

impl Limiter {
    pub fn new(params: &LimiterParams, sample_rate: f64) -> Self {
        Self {
            ceiling: 10f64.powf(f64::from(params.ceiling_db) / 20.0) as f32,
            release: (-1.0 / (sample_rate * f64::from(params.release_ms) / 1000.0)).exp() as f32,
            hold: (HOLD_SECONDS * sample_rate).round() as u32,
            reduction: 0.0,
            hold_left: 0,
        }
    }

    /// Limits one frame, all its channels, in place. A NaN or infinite
    /// sample is left out of the peak and passes as it is (times the gain),
    /// for the caller to deal with; it does not move the gain.
    pub fn frame(&mut self, frame: &mut [f32]) {
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
            // The rounded quotient can put peak * gain a step over the ceiling.
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
    }

    /// Starts where `from` is: its gain and what is left of its hold.
    pub fn adopt(&mut self, from: &Self) {
        self.reduction = from.reduction;
        self.hold_left = from.hold_left;
    }
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
}
