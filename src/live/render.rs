//! The audio callback's sample handling, kept free of Core Audio types so it
//! can be tested without hardware.
//!
//! Buffers follow Core Audio's `AudioBufferList`: a list of buffers, each
//! holding its own number of interleaved channels. Channels are numbered
//! across the list in order, so an interleaved stereo stream (one buffer of
//! two channels) and a non-interleaved one (two buffers of one channel) both
//! put the left channel at 0 and the right at 1.

/// A list of buffers of f32 samples.
pub trait Buffers {
    fn count(&self) -> usize;
    /// Interleaved channels in buffer `i`.
    fn channels(&self, i: usize) -> usize;
    /// The samples of buffer `i`, or `None` if its stream is disabled.
    fn samples(&self, i: usize) -> Option<&[f32]>;
}

pub trait BuffersMut: Buffers {
    fn samples_mut(&mut self, i: usize) -> Option<&mut [f32]>;
}

/// Where the tap's channels sit in the input list: `count` channels starting
/// at channel `first`. Only the first two are used; a single channel is
/// treated as mono and fed to both sides of the chain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TapChannels {
    pub first: usize,
    pub count: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Outcome {
    /// Output samples that left the chain outside [-1, 1] and were clamped.
    pub clamped: usize,
    /// The input held fewer frames than the output needed; the gap was fed to
    /// the chain as silence.
    pub short_input: bool,
    /// The chain produced NaN or infinity, so the whole output is silence.
    pub non_finite: bool,
    /// The input held at least one non-zero sample.
    pub heard: bool,
}

/// Where one channel sits in a buffer list.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Slot {
    buffer: usize,
    offset: usize,
    stride: usize,
}

fn locate(list: &impl Buffers, channel: usize) -> Option<Slot> {
    let mut first = 0;
    for buffer in 0..list.count() {
        let stride = list.channels(buffer);
        if channel < first + stride {
            return Some(Slot {
                buffer,
                offset: channel - first,
                stride,
            });
        }
        first += stride;
    }
    None
}

/// Frames in buffer `i`; 0 if it is disabled or has no channels.
fn frames(list: &impl Buffers, i: usize) -> usize {
    match (list.samples(i), list.channels(i)) {
        (Some(samples), channels) if channels > 0 => samples.len() / channels,
        _ => 0,
    }
}

/// Fills `output` with the tap's audio from `input`, run through `process`
/// as interleaved stereo blocks of at most `scratch.len() / 2` frames.
///
/// Every sample of every enabled output buffer is written and within
/// [-1, 1]. Non-finite input samples are fed to `process` as 0. If `process`
/// produces a non-finite sample anywhere, the whole output is silence
/// instead. Output channels after the first two are silenced, and a
/// single-channel output gets the mean of left and right. Only as many
/// frames as the shortest enabled output buffer holds are played; the rest
/// of any longer buffer is silenced.
pub fn render(
    input: &impl Buffers,
    tap: TapChannels,
    output: &mut impl BuffersMut,
    scratch: &mut [f32],
    process: &mut impl FnMut(&mut [f32]),
) -> Outcome {
    // A buffer too short for even one frame counts too, as 0 frames: every
    // frame played is written to every enabled buffer that has channels.
    let out_frames = (0..output.count())
        .filter(|&i| output.channels(i) > 0 && output.samples(i).is_some())
        .map(|i| frames(output, i))
        .min()
        .unwrap_or(0);
    silence(output);
    let mut outcome = Outcome::default();
    let (left_in, right_in) = match tap.count {
        0 => (None, None),
        1 => {
            let slot = locate(input, tap.first);
            (slot, slot)
        }
        _ => (locate(input, tap.first), locate(input, tap.first + 1)),
    };
    let in_frames = match (left_in, right_in) {
        (Some(l), Some(r)) => frames(input, l.buffer).min(frames(input, r.buffer)),
        _ => 0,
    };
    outcome.short_input = in_frames < out_frames;

    let out_channels: usize = (0..output.count()).map(|i| output.channels(i)).sum();
    let mono_out = out_channels == 1;
    let left_out = locate(output, 0);
    let right_out = if mono_out { None } else { locate(output, 1) };

    let block = scratch.len() / 2;
    let mut start = 0;
    while start < out_frames && block > 0 {
        let n = block.min(out_frames - start);
        let chunk = &mut scratch[..2 * n];

        chunk.fill(0.0);
        let available = in_frames.saturating_sub(start).min(n);
        if let (Some(l), Some(r)) = (left_in, right_in)
            && available > 0
        {
            let (ls, rs) = (input.samples(l.buffer), input.samples(r.buffer));
            if let (Some(ls), Some(rs)) = (ls, rs) {
                for (f, frame) in chunk.chunks_exact_mut(2).take(available).enumerate() {
                    let i = start + f;
                    frame[0] = finite_or_zero(ls[i * l.stride + l.offset]);
                    frame[1] = finite_or_zero(rs[i * r.stride + r.offset]);
                }
            }
        }
        outcome.heard |= chunk.iter().any(|&x| x != 0.0);

        process(chunk);
        if chunk.iter().any(|x| !x.is_finite()) {
            outcome.non_finite = true;
            break;
        }

        for (side, slot) in [(0, left_out), (1, right_out)] {
            let Some(slot) = slot else { continue };
            let Some(samples) = output.samples_mut(slot.buffer) else {
                continue;
            };
            for (f, frame) in chunk.chunks_exact(2).enumerate() {
                let x = if mono_out {
                    0.5 * (frame[0] + frame[1])
                } else {
                    frame[side]
                };
                if x.abs() > 1.0 {
                    outcome.clamped += 1;
                }
                samples[(start + f) * slot.stride + slot.offset] = x.clamp(-1.0, 1.0);
            }
        }
        start += n;
    }

    if outcome.non_finite {
        silence(output);
    }
    outcome
}

/// Another app's NaN or infinity would otherwise stay in the chain's filter
/// state and silence everything after it.
fn finite_or_zero(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// Zeroes every sample of every buffer.
pub fn silence(output: &mut impl BuffersMut) {
    for b in 0..output.count() {
        if let Some(samples) = output.samples_mut(b) {
            samples.fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonewell::{Processor, Profile};

    /// Buffers as (channels, samples); `None` samples is a disabled stream.
    struct List(Vec<(usize, Option<Vec<f32>>)>);

    impl Buffers for List {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn channels(&self, i: usize) -> usize {
            self.0[i].0
        }
        fn samples(&self, i: usize) -> Option<&[f32]> {
            self.0[i].1.as_deref()
        }
    }

    impl BuffersMut for List {
        fn samples_mut(&mut self, i: usize) -> Option<&mut [f32]> {
            self.0[i].1.as_deref_mut()
        }
    }

    /// One buffer of `channels` interleaved channels, sample (c, f) = gen(c, f).
    fn interleaved(channels: usize, frames: usize, generate: impl Fn(usize, usize) -> f32) -> List {
        let samples = (0..frames * channels)
            .map(|i| generate(i % channels, i / channels))
            .collect();
        List(vec![(channels, Some(samples))])
    }

    /// One mono buffer per channel.
    fn planar(channels: usize, frames: usize, generate: impl Fn(usize, usize) -> f32) -> List {
        List(
            (0..channels)
                .map(|c| (1, Some((0..frames).map(|f| generate(c, f)).collect())))
                .collect(),
        )
    }

    /// Output buffers prefilled with garbage, so silence must be written.
    fn garbage(mut list: List) -> List {
        for (_, samples) in &mut list.0 {
            if let Some(s) = samples {
                s.fill(0.75);
            }
        }
        list
    }

    fn channel(list: &List, ch: usize) -> Vec<f32> {
        let slot = locate(list, ch).unwrap();
        let samples = list.samples(slot.buffer).unwrap();
        samples
            .iter()
            .skip(slot.offset)
            .step_by(slot.stride)
            .copied()
            .collect()
    }

    fn ramp(c: usize, f: usize) -> f32 {
        (f as f32 * 0.001 + c as f32 * 0.25) * if c.is_multiple_of(2) { 1.0 } else { -1.0 }
    }

    const STEREO_TAP: TapChannels = TapChannels { first: 0, count: 2 };

    fn identity(_: &mut [f32]) {}

    fn run(input: &List, tap: TapChannels, output: &mut List, scratch_frames: usize) -> Outcome {
        let mut scratch = vec![0.0; 2 * scratch_frames];
        render(input, tap, output, &mut scratch, &mut identity)
    }

    #[test]
    fn interleaved_and_planar_layouts_carry_left_and_right() {
        let make_in = [interleaved(2, 300, ramp), planar(2, 300, ramp)];
        for input in &make_in {
            for mut output in [
                garbage(interleaved(2, 300, ramp)),
                garbage(planar(2, 300, ramp)),
            ] {
                let outcome = run(input, STEREO_TAP, &mut output, 128);
                assert_eq!(channel(&output, 0), channel(input, 0));
                assert_eq!(channel(&output, 1), channel(input, 1));
                assert_eq!(
                    outcome,
                    Outcome {
                        heard: true,
                        ..Outcome::default()
                    }
                );
            }
        }
    }

    #[test]
    fn tap_channels_are_found_after_the_devices_own_inputs() {
        // A device with two input channels of its own, then the stereo tap,
        // as interleaved buffers and as planar ones.
        let device_and_tap = |c: usize, f: usize| if c < 2 { 0.9 } else { ramp(c - 2, f) };
        for input in [
            List(vec![
                (2, Some(vec![0.9; 2 * 64])),
                interleaved(2, 64, ramp).0.remove(0),
            ]),
            planar(4, 64, device_and_tap),
        ] {
            let mut output = garbage(interleaved(2, 64, ramp));
            run(&input, TapChannels { first: 2, count: 2 }, &mut output, 64);
            let tap = interleaved(2, 64, ramp);
            assert_eq!(channel(&output, 0), channel(&tap, 0));
            assert_eq!(channel(&output, 1), channel(&tap, 1));
        }
    }

    #[test]
    fn mono_tap_feeds_both_sides() {
        let input = interleaved(1, 100, ramp);
        let mut output = garbage(planar(2, 100, ramp));
        run(&input, TapChannels { first: 0, count: 1 }, &mut output, 32);
        assert_eq!(channel(&output, 0), channel(&input, 0));
        assert_eq!(channel(&output, 1), channel(&input, 0));
    }

    #[test]
    fn mono_output_gets_the_mean_of_left_and_right() {
        let input = interleaved(2, 100, |c, f| if c == 0 { 0.5 } else { f as f32 * 0.001 });
        let mut output = garbage(interleaved(1, 100, ramp));
        run(&input, STEREO_TAP, &mut output, 32);
        let want: Vec<f32> = (0..100).map(|f| 0.5 * (0.5 + f as f32 * 0.001)).collect();
        assert_eq!(channel(&output, 0), want);
    }

    #[test]
    fn output_channels_after_the_first_two_are_silent() {
        let input = interleaved(2, 200, ramp);
        for mut output in [
            garbage(interleaved(6, 200, ramp)),
            garbage(planar(6, 200, ramp)),
        ] {
            run(&input, STEREO_TAP, &mut output, 64);
            assert_eq!(channel(&output, 0), channel(&input, 0));
            assert_eq!(channel(&output, 1), channel(&input, 1));
            for ch in 2..6 {
                assert!(
                    channel(&output, ch).iter().all(|&x| x == 0.0),
                    "channel {ch}"
                );
            }
        }
    }

    #[test]
    fn output_is_clamped_to_unit_range_and_counted() {
        let input = interleaved(2, 50, |c, f| {
            if f % 2 == 0 {
                0.2
            } else {
                0.4 * c as f32 - 0.2
            }
        });
        let mut output = garbage(interleaved(4, 50, ramp));
        let mut scratch = vec![0.0; 2 * 16];
        let outcome = render(
            &input,
            STEREO_TAP,
            &mut output,
            &mut scratch,
            &mut |x: &mut [f32]| x.iter_mut().for_each(|s| *s *= 8.0),
        );
        // 0.2 * 8 = 1.6 on both channels of even frames; odd frames are -1.6
        // on the left and +1.6 on the right. All 100 output samples clip.
        assert_eq!(outcome.clamped, 100);
        let all: Vec<f32> = (0..4).flat_map(|ch| channel(&output, ch)).collect();
        assert!(all.iter().all(|x| (-1.0..=1.0).contains(x)));
        assert!(channel(&output, 0).iter().all(|&x| x.abs() == 1.0));
        assert!(channel(&output, 3).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn non_finite_anywhere_silences_the_whole_output() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let input = interleaved(2, 300, ramp);
            let mut output = garbage(planar(3, 300, ramp));
            let mut scratch = vec![0.0; 2 * 64];
            let mut calls = 0;
            // Only the third block goes bad, after two were already written.
            let outcome = render(
                &input,
                STEREO_TAP,
                &mut output,
                &mut scratch,
                &mut |x: &mut [f32]| {
                    calls += 1;
                    if calls == 3 {
                        x[5] = bad;
                    }
                },
            );
            assert!(outcome.non_finite, "{bad}");
            for ch in 0..3 {
                assert!(
                    channel(&output, ch).iter().all(|&x| x == 0.0),
                    "{bad} channel {ch}"
                );
            }
        }
    }

    #[test]
    fn non_finite_check_covers_the_whole_block() {
        // The last sample of a block is the right channel of its last frame.
        for bad in [f32::NAN, f32::INFINITY] {
            let input = interleaved(2, 100, ramp);
            let mut output = garbage(planar(2, 100, ramp));
            let outcome = run_with(&input, &mut output, 50, &mut |x: &mut [f32]| {
                let last = x.len() - 1;
                x[last] = bad;
            });
            assert!(outcome.non_finite, "{bad}");
            assert!(channel(&output, 0).iter().all(|&x| x == 0.0), "{bad}");
            assert!(channel(&output, 1).iter().all(|&x| x == 0.0), "{bad}");
        }
    }

    #[test]
    fn non_finite_input_is_fed_as_silence() {
        let input = interleaved(2, 64, |c, f| match (c, f) {
            (0, 3) => f32::NAN,
            (1, 10) => f32::INFINITY,
            (1, 63) => f32::NEG_INFINITY,
            _ => ramp(c, f),
        });
        let mut fed = Vec::new();
        let mut output = garbage(interleaved(2, 64, ramp));
        let outcome = run_with(&input, &mut output, 64, &mut |x: &mut [f32]| {
            fed.extend_from_slice(x)
        });
        assert!(fed.iter().all(|x| x.is_finite()));
        assert_eq!((fed[6], fed[21], fed[127]), (0.0, 0.0, 0.0));
        assert_eq!(fed[5], ramp(1, 2));
        assert!(!outcome.non_finite);
    }

    #[test]
    fn the_shortest_buffer_sets_the_frames_played() {
        // Planar output, left longer than right: 60 frames play on both and
        // the rest of the left is silenced.
        let input = interleaved(2, 100, ramp);
        let mut output = List(vec![(1, Some(vec![0.75; 100])), (1, Some(vec![0.75; 60]))]);
        let outcome = run(&input, STEREO_TAP, &mut output, 32);
        let left = channel(&output, 0);
        assert_eq!(left[..60], channel(&input, 0)[..60]);
        assert!(left[60..].iter().all(|&x| x == 0.0));
        assert_eq!(channel(&output, 1), channel(&input, 1)[..60]);
        assert!(!outcome.short_input);

        // Planar input, right shorter than left: both sides stop after 50
        // frames and the rest is fed as silence.
        let input = List(vec![
            (1, Some((0..100).map(|f| ramp(0, f)).collect())),
            (1, Some((0..50).map(|f| ramp(1, f)).collect())),
        ]);
        let mut output = garbage(interleaved(2, 80, ramp));
        let outcome = run(&input, STEREO_TAP, &mut output, 32);
        assert!(outcome.short_input);
        let (left, right) = (channel(&output, 0), channel(&output, 1));
        assert_eq!(left[..50], channel(&input, 0)[..50]);
        assert_eq!(right[..50], channel(&input, 1)[..]);
        assert!(left[50..].iter().chain(&right[50..]).all(|&x| x == 0.0));

        // Input longer than the output: only what fits is played.
        let input = interleaved(2, 100, ramp);
        let mut output = garbage(interleaved(2, 30, ramp));
        let outcome = run(&input, STEREO_TAP, &mut output, 64);
        assert_eq!(channel(&output, 0), channel(&input, 0)[..30]);
        assert!(!outcome.short_input);
    }

    #[test]
    fn an_output_buffer_too_short_for_a_frame_plays_nothing() {
        let input = interleaved(2, 100, ramp);
        let mut output = List(vec![(2, Some(vec![0.75; 200])), (3, Some(vec![0.75; 2]))]);
        run(&input, STEREO_TAP, &mut output, 32);
        assert!(
            output
                .0
                .iter()
                .all(|(_, s)| s.as_ref().unwrap().iter().all(|&x| x == 0.0))
        );
    }

    /// splitmix64, so every run tries the same layouts.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// 0..=max
        fn upto(&mut self, max: usize) -> usize {
            (self.next() % (max as u64 + 1)) as usize
        }

        fn sample(&mut self) -> f32 {
            match self.upto(9) {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                3 => f32::MAX,
                4 => -f32::MAX,
                5 => 0.0,
                _ => self.upto(4000) as f32 / 1000.0 - 2.0,
            }
        }

        /// 0 to 3 buffers of 0 to 6 channels, any length (odd ones too),
        /// some of them disabled.
        fn list(&mut self, frames: usize, fill: &mut impl FnMut(&mut Self) -> f32) -> List {
            let count = self.upto(3);
            List(
                (0..count)
                    .map(|_| {
                        let channels = self.upto(6);
                        let enabled = self.upto(4) > 0;
                        let len = (frames * channels.max(1) + self.upto(5)).saturating_sub(2);
                        let len = if self.upto(5) == 0 {
                            self.upto(len)
                        } else {
                            len
                        };
                        let samples = enabled.then(|| (0..len).map(|_| fill(self)).collect());
                        (channels, samples)
                    })
                    .collect(),
            )
        }
    }

    #[test]
    fn random_layouts_never_panic_and_write_only_unit_range_samples() {
        let mut rng = Rng(0x5eed);
        for run in 0..100_000 {
            let frames = rng.upto(40);
            let input = rng.list(frames, &mut Rng::sample);
            let mut output = rng.list(frames, &mut |_| f32::NAN);
            let tap = TapChannels {
                first: rng.upto(6),
                count: rng.upto(3),
            };
            let mut scratch = vec![0.0; rng.upto(41)];
            let gain = [1.0, 8.0, f32::MAX][rng.upto(2)];
            let poison = rng.upto(9) == 0;
            let outcome = render(&input, tap, &mut output, &mut scratch, &mut |x| {
                x.iter_mut().for_each(|s| *s *= gain);
                if poison && let Some(s) = x.last_mut() {
                    *s = f32::NAN;
                }
            });
            for (b, (_, samples)) in output.0.iter().enumerate() {
                for &x in samples.iter().flatten() {
                    assert!(
                        (-1.0..=1.0).contains(&x),
                        "run {run}: buffer {b} holds {x}; outcome {outcome:?}"
                    );
                    assert!(!outcome.non_finite || x == 0.0, "run {run}");
                }
            }
        }
    }

    fn run_with(
        input: &List,
        output: &mut List,
        scratch_frames: usize,
        process: &mut impl FnMut(&mut [f32]),
    ) -> Outcome {
        let mut scratch = vec![0.0; 2 * scratch_frames];
        render(input, STEREO_TAP, output, &mut scratch, process)
    }

    #[test]
    fn short_or_missing_input_is_fed_as_silence() {
        let short = interleaved(2, 40, |_, _| 0.5);
        let mut output = garbage(interleaved(2, 100, ramp));
        let outcome = run(&short, STEREO_TAP, &mut output, 32);
        assert!(outcome.short_input && outcome.heard);
        let left = channel(&output, 0);
        assert!(left[..40].iter().all(|&x| x == 0.5));
        assert!(left[40..].iter().all(|&x| x == 0.0));

        for (missing, tap) in [
            (List(vec![(2, None)]), STEREO_TAP),
            (List(vec![]), STEREO_TAP),
            // Tap channels the input does not have.
            (
                interleaved(2, 100, ramp),
                TapChannels { first: 2, count: 2 },
            ),
        ] {
            let mut output = garbage(interleaved(2, 100, ramp));
            let outcome = run(&missing, tap, &mut output, 32);
            assert!(outcome.short_input && !outcome.heard);
            assert!(channel(&output, 0).iter().all(|&x| x == 0.0));
        }
    }

    #[test]
    fn silence_in_is_not_heard() {
        let input = interleaved(2, 64, |_, _| 0.0);
        let mut output = garbage(interleaved(2, 64, ramp));
        let outcome = run(&input, STEREO_TAP, &mut output, 64);
        assert_eq!(outcome, Outcome::default());
    }

    #[test]
    fn disabled_or_empty_outputs_are_left_alone() {
        let input = interleaved(2, 64, ramp);
        let mut output = List(vec![(2, None), (2, Some(vec![0.75; 128]))]);
        run(&input, STEREO_TAP, &mut output, 16);
        // Channels 0 and 1 are in the disabled stream; the rest is silenced.
        assert!(output.samples(1).unwrap().iter().all(|&x| x == 0.0));

        let mut nothing = List(vec![]);
        assert_eq!(
            run(&input, STEREO_TAP, &mut nothing, 16),
            Outcome::default()
        );
    }

    #[test]
    fn block_size_does_not_change_the_processed_stream() {
        let mut profile = Profile::default();
        profile.stereo.enabled = true;
        let input = interleaved(2, 5000, |c, f| ((f * 7 + c * 3) % 101) as f32 / 101.0 - 0.5);
        let mut outputs = Vec::new();
        for scratch_frames in [5000, 512, 97, 1] {
            let mut processor = Processor::new(&profile, 48_000, 2);
            let mut output = garbage(planar(2, 5000, ramp));
            let mut scratch = vec![0.0; 2 * scratch_frames];
            render(
                &input,
                STEREO_TAP,
                &mut output,
                &mut scratch,
                &mut |x: &mut [f32]| processor.process(x),
            );
            outputs.push((channel(&output, 0), channel(&output, 1)));
        }
        assert!(outputs.windows(2).all(|w| w[0] == w[1]));
        // The chain really changed the signal.
        assert_ne!(outputs[0].0, channel(&input, 0));
    }
}
