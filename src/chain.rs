use crate::bass::Bass;
use crate::bell::Bell;
use crate::compressor::Compressor;
use crate::contour::Contour;
use crate::correction::Correction;
use crate::crossfeed::Crossfeed;
use crate::leveler::Leveler;
use crate::limiter::Limiter;
use crate::profile::{EQ_BANDS, Profile};
use crate::shelf::Shelf;
use crate::stereo::StereoEnhancer;

fn db_to_gain(db: f32) -> f32 {
    10f64.powf(f64::from(db) / 20.0) as f32
}

/// Frames the per-channel stages take at a time, each channel in turn.
const CHUNK_FRAMES: usize = 256;
/// EQ bands run together in one pass over a chunk.
const BELLS_PER_PASS: usize = 4;

/// The nine stages for one channel, each with its own state.
struct Channel {
    input_gain: f32,
    low1: Shelf,
    high1: Shelf,
    comp1: Compressor,
    low2: Shelf,
    high2: Shelf,
    /// The EQ bands that fit the rate; the others are left out, which passes
    /// audio through them unchanged.
    bells: Vec<Bell>,
    comp2: Compressor,
    output_gain: f32,
    /// The smallest gain each compressor applied since the last `meters`.
    comp1_min: f32,
    comp2_min: f32,
}

impl Channel {
    fn new(p: &Profile, sample_rate: u32) -> Self {
        let fs = f64::from(sample_rate);
        let corner = f64::from(p.shelf_corner_hz);
        let low = f64::from(p.low_shelf_db);
        let high = f64::from(p.high_shelf_db);
        let skipped = p.skipped_eq_bands(sample_rate);
        let eq = &p.eq;
        Self {
            input_gain: db_to_gain(p.input_gain_db),
            low1: Shelf::low(low, corner, fs),
            high1: Shelf::high(high, corner, fs),
            comp1: Compressor::new(&p.compressor1, fs),
            low2: Shelf::low(-low, corner, fs),
            high2: Shelf::high(-high, corner, fs),
            bells: (0..EQ_BANDS)
                .filter(|i| !skipped.contains(i))
                .map(|i| {
                    Bell::new(
                        f64::from(eq.freqs_hz[i]),
                        f64::from(eq.q[i]),
                        f64::from(eq.gains_db[i]),
                        fs,
                    )
                })
                .collect(),
            comp2: Compressor::new(&p.compressor2, fs),
            output_gain: db_to_gain(p.output_gain_db),
            comp1_min: 1.0,
            comp2_min: 1.0,
        }
    }

    /// Runs a block of this channel's samples through its stages, a few
    /// stages at a time over the whole block rather than one sample at a
    /// time through all of them. A sample's path through all 17 filters is
    /// too long for the CPU to start on the next sample before it ends; over
    /// a few stages it can, and each filter's state stays in registers.
    /// Every sample meets the same arithmetic in the same order.
    fn run(&mut self, block: &mut [f32]) {
        for x in block.iter_mut() {
            let y = self.high1.tick(self.low1.tick(*x * self.input_gain));
            // `Compressor::tick` split open, to see the gain; the same products.
            let gain1 = self.comp1.gain(y);
            self.comp1_min = self.comp1_min.min(gain1);
            *x = self.high2.tick(self.low2.tick(y * gain1));
        }
        for bells in self.bells.chunks_mut(BELLS_PER_PASS) {
            match bells {
                [a, b, c, d] => {
                    for x in block.iter_mut() {
                        *x = d.tick(c.tick(b.tick(a.tick(*x))));
                    }
                }
                [a, b, c] => {
                    for x in block.iter_mut() {
                        *x = c.tick(b.tick(a.tick(*x)));
                    }
                }
                [a, b] => {
                    for x in block.iter_mut() {
                        *x = b.tick(a.tick(*x));
                    }
                }
                [a] => {
                    for x in block.iter_mut() {
                        *x = a.tick(*x);
                    }
                }
                _ => unreachable!("chunks of 1 to {BELLS_PER_PASS} bells"),
            }
        }
        for x in block.iter_mut() {
            let gain2 = self.comp2.gain(*x);
            self.comp2_min = self.comp2_min.min(gain2);
            *x = *x * gain2 * self.output_gain;
        }
    }
}

/// Gain reduction and levelling since the last `Processor::meters` call.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Meters {
    /// The leveler's gain now, in dB; 0 without one.
    pub leveler_db: f32,
    /// The largest gain reduction since the last `meters` call, in dB
    /// (>= 0), over all channels.
    pub compressor1_db: f32,
    pub compressor2_db: f32,
    pub limiter_db: f32,
}

/// A gain as a reduction in dB, 0 for unity or more.
fn reduction_db(gain: f32) -> f32 {
    if gain < 1.0 {
        -20.0 * gain.log10()
    } else {
        0.0
    }
}

/// The full chain for one stream. State carries across `process` calls, so
/// the output does not depend on how the stream is split into blocks.
///
/// Each optional stage is absent when disabled, which passes audio through
/// it unchanged.
pub struct Processor {
    leveler: Option<Leveler>,
    channels: Vec<Channel>,
    bass: Option<Bass>,
    contour: Option<Contour>,
    stereo: Option<StereoEnhancer>,
    crossfeed: Option<Crossfeed>,
    correction: Option<Correction>,
    limiter: Option<Limiter>,
    /// The limiter's smallest gain since the last `meters`.
    limiter_min: f32,
    /// One channel of a chunk, `CHUNK_FRAMES` long so that `process` does
    /// not allocate.
    scratch: Vec<f32>,
}

impl Processor {
    /// `profile` must already have passed `Profile::validate(sample_rate)`.
    /// The EQ bands `Profile::skipped_eq_bands` names pass audio unchanged.
    pub fn new(profile: &Profile, sample_rate: u32, channels: usize) -> Self {
        let fs = f64::from(sample_rate);
        let stereo = (channels == 2 && profile.stereo.enabled)
            .then(|| StereoEnhancer::new(&profile.stereo, f64::from(profile.shelf_corner_hz), fs));
        Self {
            leveler: profile
                .leveler
                .enabled
                .then(|| Leveler::new(&profile.leveler, fs, channels)),
            channels: (0..channels)
                .map(|_| Channel::new(profile, sample_rate))
                .collect(),
            bass: profile
                .bass
                .enabled
                .then(|| Bass::new(&profile.bass, fs, channels)),
            contour: profile
                .contour
                .enabled
                .then(|| Contour::new(&profile.contour, fs, channels)),
            stereo,
            crossfeed: (channels == 2 && profile.crossfeed.enabled)
                .then(|| Crossfeed::new(&profile.crossfeed, fs)),
            correction: profile
                .correction
                .enabled
                .then(|| Correction::new(&profile.correction, fs, channels)),
            limiter: profile
                .limiter
                .enabled
                .then(|| Limiter::new(&profile.limiter, fs)),
            limiter_min: 1.0,
            scratch: vec![0.0; CHUNK_FRAMES],
        }
    }

    /// Processes interleaved frames in place. `samples.len()` must be a
    /// multiple of the channel count.
    pub fn process(&mut self, samples: &mut [f32]) {
        let channels = self.channels.len();
        assert_eq!(samples.len() % channels, 0);
        for chunk in samples.chunks_mut(CHUNK_FRAMES * channels) {
            self.process_chunk(chunk);
        }
    }

    /// Each stage sees the same samples in the same order whichever way the
    /// stages take turns, since none reads another's state; so each takes
    /// the whole chunk before the next starts, which lets the CPU overlap
    /// one frame's work in a stage with the next frame's.
    fn process_chunk(&mut self, chunk: &mut [f32]) {
        let channels = self.channels.len();
        if let Some(leveler) = &mut self.leveler {
            for frame in chunk.chunks_exact_mut(channels) {
                leveler.frame(frame);
            }
        }
        let scratch = &mut self.scratch[..chunk.len() / channels];
        for (c, channel) in self.channels.iter_mut().enumerate() {
            for (x, frame) in scratch.iter_mut().zip(chunk.chunks_exact(channels)) {
                *x = frame[c];
            }
            channel.run(scratch);
            for (x, frame) in scratch.iter().zip(chunk.chunks_exact_mut(channels)) {
                frame[c] = *x;
            }
        }
        if let Some(bass) = &mut self.bass {
            for frame in chunk.chunks_exact_mut(channels) {
                bass.frame(frame);
            }
        }
        if let Some(contour) = &mut self.contour {
            for frame in chunk.chunks_exact_mut(channels) {
                contour.frame(frame);
            }
        }
        if let Some(stereo) = &mut self.stereo {
            for frame in chunk.chunks_exact_mut(channels) {
                (frame[0], frame[1]) = stereo.tick(frame[0], frame[1]);
            }
        }
        if let Some(crossfeed) = &mut self.crossfeed {
            for frame in chunk.chunks_exact_mut(channels) {
                (frame[0], frame[1]) = crossfeed.tick(frame[0], frame[1]);
            }
        }
        if let Some(correction) = &mut self.correction {
            for frame in chunk.chunks_exact_mut(channels) {
                correction.frame(frame);
            }
        }
        if let Some(limiter) = &mut self.limiter {
            for frame in chunk.chunks_exact_mut(channels) {
                limiter.frame(frame);
                self.limiter_min = self.limiter_min.min(limiter.gain());
            }
        }
    }

    /// Frames by which the output lags the input.
    pub fn latency_frames(&self) -> usize {
        self.limiter.as_ref().map_or(0, Limiter::latency_frames)
    }

    /// The system volume in dB below maximum, or `None` when unknown, for
    /// the loudness contour. No allocation.
    pub fn set_volume_db(&mut self, volume_db: Option<f32>) {
        if let Some(contour) = &mut self.contour {
            contour.set_volume_db(volume_db);
        }
    }

    /// The leveler's gain and the largest gain reductions since the last
    /// call, which starts them over. No allocation.
    pub fn meters(&mut self) -> Meters {
        let (mut comp1, mut comp2) = (1.0f32, 1.0f32);
        for channel in &mut self.channels {
            comp1 = comp1.min(channel.comp1_min);
            comp2 = comp2.min(channel.comp2_min);
            channel.comp1_min = 1.0;
            channel.comp2_min = 1.0;
        }
        let limiter = std::mem::replace(&mut self.limiter_min, 1.0);
        Meters {
            leveler_db: self.leveler.as_ref().map_or(0.0, Leveler::gain_db),
            compressor1_db: reduction_db(comp1),
            compressor2_db: reduction_db(comp2),
            limiter_db: reduction_db(limiter),
        }
    }

    /// Starts every level detector, the limiter's gain and the other
    /// stages' running state where `from`'s are, so a chain that takes over
    /// from a running one does not under-compress while its detectors rise
    /// from zero, nor jump up while `from` is limiting. Does nothing unless
    /// the channel counts match. Neither allocates nor frees.
    pub fn adopt_levels(&mut self, from: &Processor) {
        if self.channels.len() != from.channels.len() {
            return;
        }
        if let (Some(to), Some(from)) = (&mut self.leveler, &from.leveler) {
            to.adopt(from);
        }
        for (to, from) in self.channels.iter_mut().zip(&from.channels) {
            to.comp1.adopt_level(&from.comp1);
            to.comp2.adopt_level(&from.comp2);
        }
        if let (Some(to), Some(from)) = (&mut self.bass, &from.bass) {
            to.adopt(from);
        }
        if let (Some(to), Some(from)) = (&mut self.contour, &from.contour) {
            to.adopt(from);
        }
        if let (Some(to), Some(from)) = (&mut self.stereo, &from.stereo) {
            to.adopt_level(from);
        }
        if let (Some(to), Some(from)) = (&mut self.crossfeed, &from.crossfeed) {
            to.adopt(from);
        }
        if let (Some(to), Some(from)) = (&mut self.correction, &from.correction) {
            to.adopt(from);
        }
        if let (Some(to), Some(from)) = (&mut self.limiter, &from.limiter) {
            to.adopt(from);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bell::tests::response_at;
    use crate::profile::{CompressorParams, LimiterParams, StereoParams};
    use crate::stereo::tests::amplitude;
    use std::f64::consts::PI;

    /// |H| of a first-order shelf from its analog prototype under the
    /// bilinear transform (W = tan(pi f / fs)): a cut is (s + G W0) / (s + W0)
    /// for the low shelf and (G s + W0) / (s + W0) for the high one, and a
    /// boost is the reciprocal of the cut with the same |gain|.
    fn shelf_mag(low: bool, gain_db: f64, corner_hz: f64, f: f64) -> f64 {
        let g = 10f64.powf(-gain_db.abs() / 20.0);
        let (w, w0) = ((PI * f / FS_HZ).tan(), (PI * corner_hz / FS_HZ).tan());
        let num = if low {
            w.powi(2) + (g * w0).powi(2)
        } else {
            (g * w).powi(2) + w0.powi(2)
        };
        let cut = (num / (w.powi(2) + w0.powi(2))).sqrt();
        if gain_db > 0.0 { 1.0 / cut } else { cut }
    }

    /// Steady-state gain of a compressor with equal attack and release fed a
    /// sine of amplitude `amp`: its detector is then a linear smoother that
    /// settles at the mean of |x|, 2 amp / pi.
    fn static_gain(c: &CompressorParams, amp: f64) -> f64 {
        let level = 2.0 * amp / PI;
        let threshold = 10f64.powf(f64::from(c.threshold_db) / 20.0);
        let ratio = f64::from(c.ratio);
        if level > threshold {
            (level / threshold).powf((1.0 - ratio) / ratio)
        } else {
            1.0
        }
    }

    /// Output amplitude of a mono chain fed `seconds` of a sine, measured
    /// over the last second.
    fn sine_out(profile: &Profile, freq_hz: f64, amp: f64, seconds: usize) -> f64 {
        let n = seconds * FS as usize;
        let mut x: Vec<f32> = sine(freq_hz, amp, n).iter().map(|v| *v as f32).collect();
        Processor::new(profile, FS, 1).process(&mut x);
        amplitude(&x[n - FS as usize..], freq_hz, FS_HZ)
    }

    /// Asymmetric shelves around an active compressor 1 with equal attack
    /// and release; everything after the second shelf pair is at unity.
    fn tilted_compressor1() -> Profile {
        Profile {
            low_shelf_db: -12.0,
            high_shelf_db: 4.0,
            compressor1: CompressorParams {
                threshold_db: -36.0,
                ratio: 4.0,
                attack_ms: 300.0,
                release_ms: 300.0,
            },
            ..flat()
        }
    }

    const FS: u32 = 48_000;
    const FS_HZ: f64 = FS as f64;

    /// Deterministic white noise in [-0.5, 0.5).
    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
            })
            .collect()
    }

    fn sine(freq_hz: f64, amp: f64, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| amp * (2.0 * PI * freq_hz * i as f64 / FS_HZ).sin())
            .collect()
    }

    fn uncompressed() -> CompressorParams {
        CompressorParams {
            ratio: 1.0,
            ..CompressorParams::default()
        }
    }

    /// Every stage at unity: flat shelves and EQ, 0 dB gains, ratio 1, no
    /// limiter.
    fn flat() -> Profile {
        Profile {
            output_gain_db: 0.0,
            low_shelf_db: 0.0,
            high_shelf_db: 0.0,
            compressor1: uncompressed(),
            compressor2: uncompressed(),
            limiter: LimiterParams {
                enabled: false,
                ..LimiterParams::default()
            },
            ..Profile::default()
        }
    }

    fn interleave(l: &[f32], r: &[f32]) -> Vec<f32> {
        l.iter().zip(r).flat_map(|(a, b)| [*a, *b]).collect()
    }

    fn channel(x: &[f32], ch: usize) -> Vec<f32> {
        x.iter().skip(ch).step_by(2).copied().collect()
    }

    #[test]
    fn flat_chain_is_exact_identity() {
        let input = noise(48_000, 1);
        let mut out = input.clone();
        Processor::new(&flat(), FS, 1).process(&mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn eq_bands_too_high_for_the_rate_pass_audio_unchanged() {
        // 16 kHz at a 22.05 kHz rate, and at 48 kHz a band whose bandwidth
        // 8839 / 0.3 = 29463 Hz is past Nyquist: built as given, both
        // allpasses would be unstable. Boosted 24 dB, each must still leave
        // the flat chain an exact identity.
        for (fs, band, q) in [(22_050, 10, 1.5), (FS, 9, 0.3)] {
            let mut profile = flat();
            profile.eq.q[band] = q;
            profile.eq.gains_db[band] = 24.0;
            assert!(profile.validate(fs).is_ok());
            assert_eq!(profile.skipped_eq_bands(fs), [band]);
            let input = noise(fs as usize, 10);
            let mut out = input.clone();
            Processor::new(&profile, fs, 1).process(&mut out);
            assert_eq!(out, input, "{fs} Hz band {band}");

            // The bands that fit still act.
            profile.eq.gains_db[0] = 6.0;
            let mut out = input.clone();
            Processor::new(&profile, fs, 1).process(&mut out);
            assert!(out.iter().all(|x| x.is_finite()));
            assert_ne!(out, input);
        }
    }

    #[test]
    fn mirrored_shelves_cancel_around_unity_compressor() {
        // With compressor 1 at ratio 1 the second shelf pair undoes the
        // first, so only f32 rounding remains.
        let profile = Profile {
            low_shelf_db: -12.0,
            high_shelf_db: 12.0,
            ..flat()
        };
        let input = noise(48_000, 2);
        let mut out = input.clone();
        Processor::new(&profile, FS, 1).process(&mut out);
        let worst = input
            .iter()
            .zip(&out)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(worst < 1e-5, "worst error {worst}");
    }

    #[test]
    fn compressor1_sees_the_first_shelf_pairs_tilt() {
        // Low shelf 1 cuts and high shelf 1 boosts before compressor 1, and
        // the second pair undoes both after it, so a steady sine comes out
        // at its own amplitude times compressor 1's gain for the tilted level.
        let profile = tilted_compressor1();
        for f in [100.0, 10_000.0] {
            let tilt = shelf_mag(true, -12.0, 1000.0, f) * shelf_mag(false, 4.0, 1000.0, f);
            let want = 0.5 * static_gain(&profile.compressor1, 0.5 * tilt);
            let got = sine_out(&profile, f, 0.5, 5);
            let err_db = 20.0 * (got / want).log10();
            assert!(
                err_db.abs() < 0.02,
                "{f} Hz: got {got}, want {want} ({err_db} dB)"
            );
        }
    }

    #[test]
    fn input_gain_is_applied_before_compressor1() {
        let profile = Profile {
            input_gain_db: 6.0,
            output_gain_db: -3.0,
            ..tilted_compressor1()
        };
        let (gain_in, gain_out) = (10f64.powf(6.0 / 20.0), 10f64.powf(-3.0 / 20.0));
        let f = 10_000.0;
        let tilt = shelf_mag(true, -12.0, 1000.0, f) * shelf_mag(false, 4.0, 1000.0, f);
        let amp = 0.25 * gain_in;
        let want = amp * static_gain(&profile.compressor1, amp * tilt) * gain_out;
        let got = sine_out(&profile, f, 0.25, 5);
        let err_db = 20.0 * (got / want).log10();
        assert!(err_db.abs() < 0.02, "got {got}, want {want} ({err_db} dB)");
    }

    #[test]
    fn stereo_block_runs_after_both_channel_chains() {
        // The limiter, which runs after the stereo block, is off here; its
        // own place in the chain has its own test.
        let mut profile = Profile::default();
        profile.stereo.enabled = true;
        profile.limiter.enabled = false;
        let off = Profile {
            stereo: StereoParams {
                enabled: false,
                ..profile.stereo
            },
            ..profile.clone()
        };
        let stereo =
            || StereoEnhancer::new(&profile.stereo, f64::from(profile.shelf_corner_hz), FS_HZ);
        let stereo_pass = |x: &mut [f32]| {
            let mut st = stereo();
            for frame in x.chunks_exact_mut(2) {
                (frame[0], frame[1]) = st.tick(frame[0], frame[1]);
            }
        };
        // Loud enough for both compressors and the AGC to act.
        let input: Vec<f32> = interleave(&noise(24_000, 7), &noise(24_000, 8))
            .iter()
            .map(|x| x * 1.2)
            .collect();

        let mut got = input.clone();
        Processor::new(&profile, FS, 2).process(&mut got);
        let mut want = input.clone();
        Processor::new(&off, FS, 2).process(&mut want);
        stereo_pass(&mut want);
        assert_eq!(got, want);

        // The reverse order really is different on this input.
        let mut reversed = input;
        stereo_pass(&mut reversed);
        Processor::new(&off, FS, 2).process(&mut reversed);
        let gap = reversed
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(gap > 0.01, "orders differ by only {gap}");
    }

    /// `profile` with the limiter on or off.
    fn limited(profile: &Profile, enabled: bool) -> Profile {
        Profile {
            limiter: LimiterParams {
                enabled,
                ..profile.limiter
            },
            ..profile.clone()
        }
    }

    #[test]
    fn limiter_runs_last_after_the_output_gain_and_the_stereo_block() {
        // 12 dB of output gain and the side boost push this noise well past
        // the ceiling; the chain must equal the unlimited chain followed by
        // a limiter on its own.
        let mut profile = Profile {
            output_gain_db: 12.0,
            ..Profile::default()
        };
        profile.stereo.enabled = true;
        let input = interleave(&noise(24_000, 21), &noise(24_000, 22));
        let mut got = input.clone();
        Processor::new(&profile, FS, 2).process(&mut got);
        let mut want = input;
        Processor::new(&limited(&profile, false), FS, 2).process(&mut want);
        let ceiling = 10f64.powf(f64::from(profile.limiter.ceiling_db) / 20.0) as f32;
        assert!(want.iter().any(|x| x.abs() > 2.0 * ceiling));
        let mut limiter = Limiter::new(&profile.limiter, FS_HZ);
        for frame in want.chunks_exact_mut(2) {
            limiter.frame(frame);
        }
        assert_eq!(got, want);
        assert!(got.iter().all(|x| x.abs() <= ceiling));
    }

    #[test]
    fn limiter_off_or_unneeded_leaves_the_chain_exact() {
        // Off, it passes 12 dB over full scale untouched; on, audio that
        // never reaches the ceiling comes out delayed by the lookahead and
        // otherwise untouched. That is samples up to the ceiling itself when
        // it limits sample peaks; with true peak, as by default, noise up to
        // the ceiling has points between samples over it and is turned down,
        // and noise at 1 / 2.2 of the ceiling, which no point between samples
        // can reach it from, is not.
        let loud: Vec<f32> = noise(48_000, 23).iter().map(|x| 8.0 * x).collect();
        let mut out = loud.clone();
        Processor::new(&flat(), FS, 1).process(&mut out);
        assert_eq!(out, loud);

        let on = limited(&flat(), true);
        assert!(on.limiter.true_peak && on.limiter.lookahead_ms > 0.0);
        let sample_peak = Profile {
            limiter: LimiterParams {
                true_peak: false,
                ..on.limiter
            },
            ..on.clone()
        };
        let ceiling = 10f64.powf(f64::from(on.limiter.ceiling_db) / 20.0) as f32;
        let quiet = |peak: f32| {
            let mut x: Vec<f32> = noise(2 * 48_000, 24)
                .iter()
                .map(|x| 2.0 * peak * x)
                .collect();
            x[0] = peak;
            x[1] = -peak;
            x
        };
        for (profile, peak, limits) in [
            (&sample_peak, ceiling, false),
            (&on, ceiling, true),
            (&on, ceiling / 2.2, false),
        ] {
            let input = quiet(peak);
            let mut chain = Processor::new(profile, FS, 2);
            let delay = 2 * chain.latency_frames();
            assert!(delay > 0);
            let mut out = input.clone();
            chain.process(&mut out);
            assert!(out[..delay].iter().all(|x| *x == 0.0));
            let what = format!("true peak {}, peak {peak}", profile.limiter.true_peak);
            assert_eq!(
                out[delay..] != input[..input.len() - delay],
                limits,
                "{what}"
            );
        }
    }

    #[test]
    fn limited_output_does_not_depend_on_block_size() {
        // Swells from quiet to 10 dB over the ceiling and back, so blocks
        // end while the gain is held, falling and recovering.
        let on = limited(&flat(), true);
        let input: Vec<f32> = noise(2 * 40_000, 25)
            .iter()
            .enumerate()
            .map(|(i, x)| x * 6.0 * (i as f32 / 16_000.0).sin().abs())
            .collect();
        let mut whole = input.clone();
        Processor::new(&on, FS, 2).process(&mut whole);
        assert_ne!(whole, input, "the limiter did nothing");
        let mut chunked = input.clone();
        let mut processor = Processor::new(&on, FS, 2);
        let mut rest = chunked.as_mut_slice();
        for frames in [1, 479, 2, 4801, 64, 12_345].iter().cycle() {
            let take = (frames * 2).min(rest.len());
            let (head, tail) = rest.split_at_mut(take);
            processor.process(head);
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        assert_eq!(whole, chunked);
    }

    #[test]
    fn stereo_block_needs_exactly_two_channels() {
        let mut on = Profile::default();
        on.stereo.enabled = true;
        let off = Profile::default();
        for channels in [1, 3, 6] {
            let input = noise(channels * 10_000, 9);
            let (mut a, mut b) = (input.clone(), input);
            Processor::new(&on, FS, channels).process(&mut a);
            Processor::new(&off, FS, channels).process(&mut b);
            assert_eq!(a, b, "{channels} channels");
        }
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let mut profile = Profile::default();
        profile.eq.gains_db = [3.0, -2.0, 0.0, 4.0, -6.0, 1.0, 0.0, 2.0, -3.0, 5.0, -1.0];
        profile.stereo.enabled = true;
        // Loud enough that both compressors and the stereo AGC are active.
        let input: Vec<f32> = noise(2 * 30_000, 3).iter().map(|x| x * 1.6).collect();
        let mut whole = input.clone();
        Processor::new(&profile, FS, 2).process(&mut whole);
        let mut chunked = input.clone();
        let mut processor = Processor::new(&profile, FS, 2);
        let mut rest = chunked.as_mut_slice();
        for frames in [1, 7, 333, 1, 4099, 12_345].iter().cycle() {
            let take = (frames * 2).min(rest.len());
            let (head, tail) = rest.split_at_mut(take);
            processor.process(head);
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        assert_eq!(whole, chunked);
    }

    #[test]
    fn eq_band_response_matches_bell_design() {
        // Band 8 (2497 Hz) at +9 dB with the default mirrored shelves; the
        // chain is linear with both ratios at 1, so its impulse response
        // gives the frequency response.
        let mut profile = Profile {
            low_shelf_db: -10.0,
            high_shelf_db: 8.0,
            ..flat()
        };
        profile.eq.gains_db[7] = 9.0;
        let (fc, q, g) = (2497.0f64, 1.5f64, 10f64.powf(9.0 / 20.0));
        let mut h = vec![0.0f32; 1 << 16];
        h[0] = 1.0;
        Processor::new(&profile, FS, 1).process(&mut h);

        // Closed-form Regalia-Mitra bell, H = (1+G)/2 + (1-G)/2 A(e^jw).
        let expected_db = |f: f64| {
            let k1 = -(2.0 * PI * fc / FS_HZ).cos();
            let t = (PI * fc / (q * FS_HZ)).tan();
            let k2 = (1.0 - t) / (1.0 + t);
            let w = 2.0 * PI * f / FS_HZ;
            let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
            let num = (k2 + k1 * (1.0 + k2) * c1 + c2, k1 * (1.0 + k2) * s1 + s2);
            let den = (
                1.0 + k1 * (1.0 + k2) * c1 + k2 * c2,
                k1 * (1.0 + k2) * s1 + k2 * s2,
            );
            let d = den.0 * den.0 + den.1 * den.1;
            let a = (
                (num.0 * den.0 + num.1 * den.1) / d,
                (num.1 * den.0 - num.0 * den.1) / d,
            );
            let re = 0.5 * (1.0 + g) + 0.5 * (1.0 - g) * a.0;
            let im = 0.5 * (1.0 - g) * a.1;
            10.0 * (re * re + im * im).log10()
        };
        assert!((expected_db(fc) - 9.0).abs() < 1e-9);
        for f in [fc, 1000.0, 1800.0, 3500.0, 8000.0, 100.0] {
            let (got, _) = response_at(&h, f, FS_HZ);
            let want = expected_db(f);
            assert!(
                (got - want).abs() < 0.01,
                "{f} Hz: got {got} dB, want {want} dB"
            );
        }
    }

    #[test]
    fn compressor2_reduces_loud_sine_by_static_amount() {
        // Equal attack and release make the detector a linear smoother, so
        // its steady state is the mean of |x| = 2A/pi with negligible ripple
        // at 1 kHz; the gain is then (2A / (pi K_T))^((1-r)/r).
        let mut profile = flat();
        profile.compressor2 = CompressorParams {
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 50.0,
            release_ms: 50.0,
        };
        let amp = 0.5;
        let level: f64 = 2.0 * amp / PI;
        let want = amp * (level / 0.1).powf((1.0 - 4.0) / 4.0);
        let mut x: Vec<f32> = sine(1000.0, amp, 96_000)
            .iter()
            .map(|v| *v as f32)
            .collect();
        Processor::new(&profile, FS, 1).process(&mut x);
        let got = amplitude(&x[48_000..], 1000.0, FS_HZ);
        let err_db = 20.0 * (got / want).log10();
        assert!(err_db.abs() < 0.05, "got {got}, want {want} ({err_db} dB)");
        // Sanity: this is a reduction of about 7.5 dB, not a no-op.
        assert!(20.0 * (got / amp).log10() < -7.0);
    }

    #[test]
    fn stereo_with_zero_boost_is_passthrough() {
        let on = Profile {
            stereo: StereoParams {
                enabled: true,
                side_boost_db: 0.0,
                ..StereoParams::default()
            },
            ..Profile::default()
        };
        let off = Profile {
            stereo: StereoParams {
                enabled: false,
                ..on.stereo
            },
            ..on.clone()
        };
        let input = interleave(&noise(48_000, 4), &noise(48_000, 5));
        let (mut a, mut b) = (input.clone(), input);
        Processor::new(&on, FS, 2).process(&mut a);
        Processor::new(&off, FS, 2).process(&mut b);
        let worst = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max);
        assert!(worst < 1e-6, "worst difference {worst}");
    }

    #[test]
    fn stereo_keeps_mono_input_mono() {
        let mut profile = Profile::default();
        profile.stereo.enabled = true;
        let mono = noise(48_000, 6);
        let mut x = interleave(&mono, &mono);
        Processor::new(&profile, FS, 2).process(&mut x);
        assert_eq!(channel(&x, 0), channel(&x, 1));
    }

    #[test]
    fn stereo_widens_quiet_input_above_crossover_only() {
        // Mid and side both quiet (below the AGC threshold even after the
        // detector's +12 dB tilt), so the side gets the full makeup gain on
        // the high band. With a first-order split the side gain at f is
        // |1 + (g - 1) HP(f)|, HP the bilinear high-pass at the crossover.
        let mut profile = flat();
        profile.stereo = StereoParams {
            enabled: true,
            crossover_hz: 1000.0,
            side_boost_db: 8.0,
            ..StereoParams::default()
        };
        let g = 10f64.powf(8.0 / 20.0);
        let side_gain = |f: f64| {
            let t = (PI * 1000.0 / FS_HZ).tan();
            let tw = (PI * f / FS_HZ).tan();
            // HP(jW) = jW / (jW + t) with W = tan(w/2).
            let d = t * t + tw * tw;
            let (hp_re, hp_im) = (tw * tw / d, tw * t / d);
            ((1.0 + (g - 1.0) * hp_re).powi(2) + ((g - 1.0) * hp_im).powi(2)).sqrt()
        };

        let n = 2 * FS as usize;
        let (mid_f, low_side_f, high_side_f) = (3000.0, 20.0, 8000.0);
        let m = sine(mid_f, 0.002, n);
        let s_low = sine(low_side_f, 0.003, n);
        let s_high = sine(high_side_f, 0.002, n);
        let l: Vec<f32> = (0..n)
            .map(|i| (m[i] - s_low[i] - s_high[i]) as f32)
            .collect();
        let r: Vec<f32> = (0..n)
            .map(|i| (m[i] + s_low[i] + s_high[i]) as f32)
            .collect();
        let mut x = interleave(&l, &r);
        Processor::new(&profile, FS, 2).process(&mut x);
        let (lo, ro) = (channel(&x, 0), channel(&x, 1));
        let side: Vec<f32> = lo.iter().zip(&ro).map(|(a, b)| 0.5 * (b - a)).collect();
        let mid: Vec<f32> = lo.iter().zip(&ro).map(|(a, b)| 0.5 * (a + b)).collect();
        let tail = n / 2..;

        let high_db = 20.0 * (amplitude(&side[tail.clone()], high_side_f, FS_HZ) / 0.002).log10();
        let low_db = 20.0 * (amplitude(&side[tail.clone()], low_side_f, FS_HZ) / 0.003).log10();
        let mid_db = 20.0 * (amplitude(&mid[tail], mid_f, FS_HZ) / 0.002).log10();
        assert!(
            (high_db - 20.0 * side_gain(high_side_f).log10()).abs() < 0.01,
            "{high_db}"
        );
        assert!(
            (low_db - 20.0 * side_gain(low_side_f).log10()).abs() < 0.01,
            "{low_db}"
        );
        assert!((high_db - 8.0).abs() < 0.25, "high side {high_db} dB");
        assert!(low_db.abs() < 0.05, "low side {low_db} dB");
        assert!(mid_db.abs() < 0.01, "mid {mid_db} dB");
    }

    /// Every new stage's table written out with values other than the
    /// defaults, and each stage off.
    const NEW_STAGES_OFF: &str = "\
[leveler]
enabled = false
target_lufs = -23.0
max_boost_db = 20.0
max_cut_db = 3.0
rise_db_per_s = 0.05
fall_db_per_s = 40.0

[bass]
enabled = false
cutoff_hz = 180.0
harmonics_db = 9.0
remove_lows = true

[contour]
enabled = false
reference_db = 0.0
bass_per_db = 1.0
treble_per_db = 1.0
max_bass_db = 18.0
max_treble_db = 12.0

[crossfeed]
enabled = false
cutoff_hz = 300.0
level_db = -1.0

[correction]
enabled = false
preamp_db = 12.0

[[correction.bands]]
kind = \"low_shelf\"
freq_hz = 100.0
q = 0.7
gain_db = 24.0

[[correction.bands]]
freq_hz = 3000.0
gain_db = -24.0
";

    #[test]
    fn new_stages_switched_off_leave_the_chain_as_it_was() {
        let written: Profile = toml::from_str(NEW_STAGES_OFF).unwrap();
        assert_eq!(written.correction.bands.len(), 2);
        assert_ne!(written, Profile::default());
        assert!(written.validate(FS).is_ok());
        for stereo in [false, true] {
            let (mut default, mut off) = (Profile::default(), written.clone());
            default.stereo.enabled = stereo;
            off.stereo.enabled = stereo;
            for channels in [1, 2, 6] {
                // Loud enough for both compressors and the limiter to act.
                let input: Vec<f32> = noise(channels * 20_000, 30)
                    .iter()
                    .map(|x| x * 3.0)
                    .collect();
                let (mut a, mut b) = (input.clone(), input);
                Processor::new(&default, FS, channels).process(&mut a);
                Processor::new(&off, FS, channels).process(&mut b);
                assert_eq!(a, b, "{channels} channels, stereo {stereo}");
            }
        }
    }

    #[test]
    fn latency_is_the_limiters() {
        let profile = Profile::default();
        let limiter = Limiter::new(&profile.limiter, FS_HZ);
        let chain = Processor::new(&profile, FS, 2);
        assert_eq!(chain.latency_frames(), limiter.latency_frames());
        assert_eq!(Processor::new(&flat(), FS, 2).latency_frames(), 0);
    }

    /// Steady compression on the left channel only: a sine through one
    /// compressor with equal attack and release, the right channel under its
    /// threshold.
    fn metered(compressor1: bool) -> (Profile, Vec<f32>, f64) {
        let c = CompressorParams {
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 50.0,
            release_ms: 50.0,
        };
        let mut profile = flat();
        if compressor1 {
            profile.compressor1 = c;
        } else {
            profile.compressor2 = c;
        }
        let left = sine(1000.0, 0.5, FS as usize);
        let right = sine(1000.0, 0.05, FS as usize);
        let input = interleave(
            &left.iter().map(|v| *v as f32).collect::<Vec<_>>(),
            &right.iter().map(|v| *v as f32).collect::<Vec<_>>(),
        );
        let want_db = -20.0 * static_gain(&c, 0.5).log10();
        (profile, input, want_db)
    }

    #[test]
    fn meters_show_each_compressors_largest_reduction_and_start_over() {
        for compressor1 in [true, false] {
            let (profile, input, want_db) = metered(compressor1);
            let mut chain = Processor::new(&profile, FS, 2);
            let mut x = input.clone();
            // The attack, then a second of steady compression on its own.
            chain.process(&mut x);
            chain.meters();
            let mut x = input.clone();
            chain.process(&mut x);
            let m = chain.meters();
            let (active, idle) = if compressor1 {
                (m.compressor1_db, m.compressor2_db)
            } else {
                (m.compressor2_db, m.compressor1_db)
            };
            assert!(
                (f64::from(active) - want_db).abs() < 0.01,
                "compressor1 {compressor1}: {m:?}, want {want_db} dB"
            );
            assert_eq!((idle, m.limiter_db, m.leveler_db), (0.0, 0.0, 0.0));
            // About 7.5 dB, not a no-op.
            assert!(want_db > 7.0, "{want_db}");
            // Read again at once, nothing has been reduced since.
            assert_eq!(chain.meters(), Meters::default());

            // Silence: the detector takes a while to fall under the
            // threshold, then nothing more is reduced.
            let mut quiet = vec![0.0f32; FS as usize];
            chain.process(&mut quiet);
            assert!(chain.meters() != Meters::default());
            let mut quiet = vec![0.0f32; FS as usize];
            chain.process(&mut quiet);
            assert_eq!(chain.meters(), Meters::default());
        }
    }

    #[test]
    fn meters_show_the_limiters_largest_reduction() {
        // A frame at twice full scale under a -1 dB ceiling: the limiter's
        // gain is ceiling / 2, a 7.02 dB reduction.
        let profile = limited(&flat(), true);
        let mut chain = Processor::new(&profile, FS, 2);
        let mut x = vec![0.1f32; 2 * 1000];
        x[500] = 2.0;
        chain.process(&mut x);
        let m = chain.meters();
        let want = 20.0 * 2.0f64.log10() + 1.0;
        assert!((f64::from(m.limiter_db) - want).abs() < 1e-4, "{m:?}");
        assert_eq!((m.compressor1_db, m.compressor2_db), (0.0, 0.0));
        // Metering does not change the output.
        let mut plain = vec![0.1f32; 2 * 1000];
        plain[500] = 2.0;
        let mut unmetered = Processor::new(&profile, FS, 2);
        unmetered.process(&mut plain);
        assert_eq!(x, plain);
        assert_eq!(chain.meters(), Meters::default());
    }

    #[test]
    fn leveler_brings_loud_and_quiet_sources_out_at_about_the_same_loudness() {
        // 20 s of a stereo programme at 120 bpm (a decaying 50 Hz kick on
        // the beat, a 110 Hz bass, a chord detuned across the sides, hats of
        // high-passed noise on the sixteenths, a low-passed noise bed), set
        // to -8 and -30 LUFS. The leveler has settled on both by 15 s (the
        // quiet one after 1 s to start and 12 s of rise to the +12 dB cap);
        // the default chain alone turns the loud one down and the quiet one
        // up, but not nearly as far.
        use crate::biquad::{Biquad, Coefs};
        use crate::loudness::{integrated_lufs, tests::at_lufs};
        use std::f64::consts::FRAC_1_SQRT_2;
        let n = 20 * FS as usize;
        let (hiss, rumble) = (noise(2 * n, 40), noise(2 * n, 41));
        let mut hats = [0, 1].map(|_| Biquad::new(Coefs::high_pass(6000.0, FRAC_1_SQRT_2, FS_HZ)));
        let mut bed = [0, 1].map(|_| Biquad::new(Coefs::low_pass(400.0, FRAC_1_SQRT_2, FS_HZ)));
        let mut programme = Vec::with_capacity(2 * n);
        for i in 0..n {
            let t = i as f64 / FS_HZ;
            let kick = (2.0 * PI * 50.0 * t).sin() * (-(t % 0.5) / 0.12).exp();
            let bass = 0.35 * (2.0 * PI * 110.0 * t).sin();
            let hat_env = (-(t % 0.125) / 0.02).exp() as f32;
            for c in 0..2 {
                let detune = 1.0 + 0.003 * c as f64;
                let chord = [261.6, 329.6, 392.0]
                    .iter()
                    .map(|f| (2.0 * PI * f * detune * t).sin())
                    .sum::<f64>();
                let hat = hats[c].tick(hiss[2 * i + c]) * hat_env;
                let bed = bed[c].tick(rumble[2 * i + c]);
                programme.push((kick + bass + 0.12 * chord) as f32 + 0.8 * hat + 0.3 * bed);
            }
        }
        let settled = |lufs: f64, leveler: bool| {
            let mut profile = Profile::default();
            profile.leveler.enabled = leveler;
            let mut x = at_lufs(&programme, 2, FS, lufs);
            Processor::new(&profile, FS, 2).process(&mut x);
            integrated_lufs(&x[2 * 15 * FS as usize..], 2, FS)
        };
        let on = (settled(-8.0, true), settled(-30.0, true));
        let off = (settled(-8.0, false), settled(-30.0, false));
        assert!((on.0 - on.1).abs() < 2.0, "leveler on: {on:?} LUFS");
        assert!((off.0 - off.1).abs() > 8.0, "leveler off: {off:?} LUFS");
    }
}
