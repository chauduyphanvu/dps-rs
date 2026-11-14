//! What the audio callback plays: the chain's output, crossfaded from the
//! old chain whenever a new one arrives, or in bypass the unprocessed signal
//! crossfaded the same way.
//!
//! Bypass plays the unprocessed signal through one gain, the same on both
//! sides, chosen so its total (left plus right) power matches the processed
//! signal's. One gain keeps the unprocessed signal's own stereo balance, so
//! an A/B comparison hears what the chain does to the balance as well; with
//! the two sides at very different levels, each side on its own can be
//! further off than the total.
//!
//! The chain keeps running in bypass, so its filters stay warm and the
//! loudness it is matched to stays measured.
//!
//! The unprocessed signal is delayed as far as the chain delays its output,
//! so the bypass crossfade mixes the same moment of the audio and the levels
//! compare the same stretch of it. A new chain with another latency moves the
//! delay over the same 30 ms fade, which comb-filters the unprocessed signal
//! for that long.
//!
//! Once a block, the mixer publishes meters for the screen: the chain's gain
//! reductions and leveler gain, and the short-term loudness of the
//! unprocessed input and of what plays.

use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

use dps::Meters;
use dps::limiter::Limiter;
use dps::loudness::Meter;

use super::{Chain, Handoff};

/// Length of every crossfade: between chains, and into and out of bypass.
pub const FADE_SECONDS: f64 = 0.03;
/// Time constant of the loudness measurements bypass is matched with. They
/// start over once a new chain has faded in, averaging every frame equally
/// until they span this long.
const LEVEL_SECONDS: f64 = 3.0;
/// Time constant of the smoothing on the unprocessed signal's gain.
const GAIN_SECONDS: f64 = 0.2;
/// The most bypass raises or lowers the unprocessed signal: 12 dB.
const MAX_GAIN_DB: f64 = 12.0;
/// Mean square below which an unprocessed frame counts as silent and is left
/// out of the measurements, so the gain holds through silence (-120 dBFS).
const SILENT: f64 = 1e-12;
/// Window of the loudness meters: BS.1770's short-term loudness.
const METER_SECONDS: f64 = 3.0;

/// Shared by the main thread and the audio callback for the whole run, so a
/// new session after a device change starts where the last one left off.
pub struct Controls {
    /// Main thread: play the unprocessed signal instead of the processed one.
    pub bypass: AtomicBool,
    /// Callback: the gain bypass puts on the unprocessed signal, as f32 bits.
    dry_gain: AtomicU32,
    /// Callback: the measurements the gain comes from, as f64 bits.
    levels: [AtomicU64; 3],
    /// Callback: the largest gain reductions of compressor 1, compressor 2
    /// and the limiter since the main thread last read them, in dB as f32
    /// bits. They are never negative, so their bits order as they do.
    reductions: [AtomicU32; 3],
    /// Callback: the leveler's gain in dB, as f32 bits.
    leveler_db: AtomicU32,
    /// Callback: the short-term loudness of the unprocessed input and of
    /// what plays, in LUFS as f32 bits.
    loudness: [AtomicU32; 2],
}

impl Default for Controls {
    fn default() -> Self {
        let silent = || AtomicU32::new(f32::NEG_INFINITY.to_bits());
        Self {
            bypass: AtomicBool::new(false),
            dry_gain: AtomicU32::new(1f32.to_bits()),
            levels: Default::default(),
            reductions: Default::default(),
            leveler_db: AtomicU32::new(0f32.to_bits()),
            loudness: [silent(), silent()],
        }
    }
}

/// What the meters read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Readings {
    /// Short-term loudness of the unprocessed input and of what plays, in
    /// LUFS; -inf in digital silence.
    pub input_lufs: f32,
    pub output_lufs: f32,
    /// The heard chain's leveler gain, and its largest gain reductions
    /// since the last reading.
    pub chain: Meters,
}

impl Default for Readings {
    fn default() -> Self {
        Self {
            input_lufs: f32::NEG_INFINITY,
            output_lufs: f32::NEG_INFINITY,
            chain: Meters::default(),
        }
    }
}

impl Controls {
    pub fn dry_gain(&self) -> f32 {
        f32::from_bits(self.dry_gain.load(Relaxed))
    }

    /// Main thread: the meters, starting the gain reductions over.
    pub fn readings(&self) -> Readings {
        let [c1, c2, lim] = self
            .reductions
            .each_ref()
            .map(|r| f32::from_bits(r.swap(0, Relaxed)));
        let [input_lufs, output_lufs] = self
            .loudness
            .each_ref()
            .map(|l| f32::from_bits(l.load(Relaxed)));
        Readings {
            input_lufs,
            output_lufs,
            chain: Meters {
                leveler_db: f32::from_bits(self.leveler_db.load(Relaxed)),
                compressor1_db: c1,
                compressor2_db: c2,
                limiter_db: lim,
            },
        }
    }

    /// Callback: one block's meters.
    fn publish(&self, chain: Meters, input_lufs: f32, output_lufs: f32) {
        let reductions = [chain.compressor1_db, chain.compressor2_db, chain.limiter_db];
        for (slot, db) in self.reductions.iter().zip(reductions) {
            // NaN and -0.0 would break the bits' order.
            let db = if db > 0.0 { db } else { 0.0 };
            slot.fetch_max(db.to_bits(), Relaxed);
        }
        self.leveler_db.store(chain.leveler_db.to_bits(), Relaxed);
        self.loudness[0].store(input_lufs.to_bits(), Relaxed);
        self.loudness[1].store(output_lufs.to_bits(), Relaxed);
    }
}

/// Running mean squares of the unprocessed and processed signals over the
/// non-silent frames, each new frame weighted 1 / `frames`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Levels {
    dry: f64,
    wet: f64,
    /// How many frames the means stand for, up to `LEVEL_SECONDS` worth:
    /// until then every frame counts equally, after that the means are
    /// one-pole averages with that time constant.
    frames: f64,
}

impl Levels {
    fn load(controls: &Controls) -> Self {
        let [dry, wet, frames] = controls
            .levels
            .each_ref()
            .map(|l| f64::from_bits(l.load(Relaxed)));
        Self { dry, wet, frames }
    }

    fn store(&self, controls: &Controls) {
        for (slot, value) in controls
            .levels
            .iter()
            .zip([self.dry, self.wet, self.frames])
        {
            slot.store(value.to_bits(), Relaxed);
        }
    }

    /// Adds one non-silent frame's mean squares.
    fn add(&mut self, dry: f64, wet: f64, max_frames: f64) {
        self.frames = (self.frames + 1.0).min(max_frames);
        let weight = 1.0 / self.frames;
        self.dry += weight * (dry - self.dry);
        self.wet += weight * (wet - self.wet);
    }

    /// The gain that matches the unprocessed level to the processed one;
    /// `None` until a non-silent frame has been measured.
    fn gain(&self, max_gain: f64) -> Option<f64> {
        (self.frames > 0.0).then(|| (self.wet / self.dry).sqrt().clamp(1.0 / max_gain, max_gain))
    }
}

/// The callback's processing: no allocation, locks or frees.
pub struct Mixer<C> {
    chain: Box<C>,
    /// The chain being faded out, and how many frames of the fade have played.
    fading: Option<(Box<C>, usize)>,
    handoff: Arc<Handoff<C>>,
    controls: Arc<Controls>,
    fade_frames: usize,
    /// The unprocessed block, and the old chain's output while fading.
    dry: Vec<f32>,
    old: Vec<f32>,
    /// A ring of the last unprocessed frames, the next to be written at
    /// `history_at`, long enough for any chain's latency.
    history: Vec<f32>,
    history_at: usize,
    /// How many frames back the unprocessed signal is played and measured:
    /// the chain's latency, and while a new chain fades in, the old one's.
    delay: usize,
    old_delay: usize,
    /// How far bypass has faded in, in frames: 0 to `fade_frames`.
    dry_frames: usize,
    levels: Levels,
    /// The most `levels.frames` reaches.
    level_frames: f64,
    max_gain: f64,
    /// The gain on the unprocessed signal, and where it is heading.
    gain: f64,
    target: f64,
    gain_coef: f64,
    /// Loudness of the unprocessed input, and of what plays.
    input_meter: Meter,
    output_meter: Meter,
}

impl<C: Chain> Mixer<C> {
    /// A mixer for blocks of up to `block_frames` interleaved stereo frames,
    /// carrying on with the bypass state, gain and measurements in `controls`.
    pub fn new(
        chain: Box<C>,
        rate: u32,
        block_frames: usize,
        handoff: Arc<Handoff<C>>,
        controls: Arc<Controls>,
    ) -> Self {
        let fs = f64::from(rate);
        let gain = f64::from(controls.dry_gain());
        let levels = Levels::load(&controls);
        let max_gain = 10f64.powf(MAX_GAIN_DB / 20.0);
        let fade_frames = ((FADE_SECONDS * fs).round() as usize).max(1);
        let delay = chain.latency_frames();
        Self {
            chain,
            fading: None,
            fade_frames,
            dry: vec![0.0; 2 * block_frames],
            old: vec![0.0; 2 * block_frames],
            history: vec![0.0; 2 * (Limiter::max_latency_frames(fs) + 1)],
            history_at: 0,
            delay,
            old_delay: delay,
            dry_frames: if controls.bypass.load(Relaxed) {
                fade_frames
            } else {
                0
            },
            levels,
            level_frames: (LEVEL_SECONDS * fs).max(1.0),
            max_gain,
            gain,
            target: levels.gain(max_gain).unwrap_or(gain),
            gain_coef: 1.0 - (-1.0 / (GAIN_SECONDS * fs)).exp(),
            input_meter: Meter::new(fs, 2, METER_SECONDS),
            output_meter: Meter::new(fs, 2, METER_SECONDS),
            handoff,
            controls,
        }
    }

    /// Replaces the unprocessed interleaved stereo frames in `block` with
    /// what to play. A non-finite sample from a chain is left in `block`,
    /// even in bypass, so the caller silences the output.
    pub fn process(&mut self, block: &mut [f32]) {
        // A chain posted during a fade waits for it to end, so at most two
        // chains ever run and the output never jumps.
        if self.fading.is_none()
            && let Some(mut new) = self.handoff.take()
        {
            new.adopt(&self.chain);
            self.fading = Some((mem::replace(&mut self.chain, new), 0));
            self.old_delay = self.delay;
            self.delay = self.chain.latency_frames();
        }
        let dry = &mut self.dry[..block.len()];
        dry.copy_from_slice(block);
        for frame in block.chunks_exact(2) {
            self.input_meter.frame(frame);
        }
        self.chain.process(block);

        let mut finished = false;
        let mut faded = None;
        if let Some((old_chain, done)) = &mut self.fading {
            let old = &mut self.old[..block.len()];
            old.copy_from_slice(dry);
            old_chain.process(old);
            let length = self.fade_frames as f32;
            for (f, (new, old)) in block
                .chunks_exact_mut(2)
                .zip(old.chunks_exact(2))
                .enumerate()
            {
                let t = ((*done + f + 1) as f32 / length).min(1.0);
                for (x, o) in new.iter_mut().zip(old) {
                    *x = (1.0 - t) * o + t * *x;
                }
            }
            faded = Some(*done);
            *done += block.len() / 2;
            finished = *done >= self.fade_frames;
        }
        self.delay_dry(block.len() / 2, faded);
        if finished && let Some((old_chain, _)) = self.fading.take() {
            self.handoff.retire(old_chain);
            // The new chain's level is measured from scratch, so the match
            // follows it within about a second rather than `LEVEL_SECONDS`;
            // the gain's smoothing keeps that from jumping.
            self.levels.frames = 0.0;
        }

        if block.iter().all(|x| x.is_finite()) {
            self.mix_dry(block);
        }
        for frame in block.chunks_exact(2) {
            self.output_meter.frame(frame);
        }
        self.controls.publish(
            self.chain.meters(),
            self.input_meter.lufs(),
            self.output_meter.lufs(),
        );
    }

    /// Replaces the first `frames` unprocessed frames in `dry` with those
    /// `delay` frames earlier. `faded` is how far a fade between chains had
    /// got at the block's start; if their latencies differ, the frames are
    /// faded from `old_delay` frames earlier as the chains are.
    fn delay_dry(&mut self, frames: usize, faded: Option<usize>) {
        let size = self.history.len() / 2;
        let back = |at: usize, by: usize| 2 * ((at + size - by) % size);
        let faded = faded.filter(|_| self.old_delay != self.delay);
        for (f, dry) in self.dry[..2 * frames].chunks_exact_mut(2).enumerate() {
            let at = self.history_at;
            self.history[2 * at..2 * at + 2].copy_from_slice(dry);
            let new = &self.history[back(at, self.delay)..][..2];
            match faded {
                Some(done) => {
                    let t = ((done + f + 1) as f32 / self.fade_frames as f32).min(1.0);
                    let old = &self.history[back(at, self.old_delay)..][..2];
                    for ((x, o), n) in dry.iter_mut().zip(old).zip(new) {
                        *x = (1.0 - t) * o + t * n;
                    }
                }
                None => dry.copy_from_slice(new),
            }
            self.history_at = if at + 1 == size { 0 } else { at + 1 };
        }
    }

    /// Measures both signals and mixes in the level-matched unprocessed one
    /// as far as bypass has faded in.
    fn mix_dry(&mut self, block: &mut [f32]) {
        let bypass = self.controls.bypass.load(Relaxed);
        let length = self.fade_frames as f32;
        let dry = &self.dry[..block.len()];
        for (out, dry) in block.chunks_exact_mut(2).zip(dry.chunks_exact(2)) {
            let dry_power = power(dry);
            if dry_power > SILENT {
                self.levels.add(dry_power, power(out), self.level_frames);
                self.target = self.levels.gain(self.max_gain).unwrap_or(self.target);
            }
            self.gain += self.gain_coef * (self.target - self.gain);
            self.dry_frames = if bypass {
                (self.dry_frames + 1).min(self.fade_frames)
            } else {
                self.dry_frames.saturating_sub(1)
            };
            if self.dry_frames > 0 {
                let (m, g) = (self.dry_frames as f32 / length, self.gain as f32);
                for (x, d) in out.iter_mut().zip(dry) {
                    *x = (1.0 - m) * *x + m * g * d;
                }
            }
        }
        self.controls
            .dry_gain
            .store((self.gain as f32).to_bits(), Relaxed);
        self.levels.store(&self.controls);
    }
}

/// Mean square of one stereo frame.
fn power(frame: &[f32]) -> f64 {
    frame.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use dps::profile::{CompressorParams, LimiterParams, StereoParams};
    use dps::{Processor, Profile};
    use std::f64::consts::PI;

    const RATE: u32 = 48_000;
    /// 30 ms at 48 kHz.
    const FADE: usize = 1440;

    /// Multiplies every sample by its gain and delays it by a latency,
    /// counting how often it is dropped.
    struct Scale {
        gain: f32,
        drops: Arc<AtomicU64>,
        /// The delayed samples, interleaved, the oldest at `at`.
        line: Vec<f32>,
        at: usize,
    }

    impl Scale {
        fn new(gain: f32, latency: usize, drops: &Arc<AtomicU64>) -> Self {
            Self {
                gain,
                drops: Arc::clone(drops),
                line: vec![0.0; 2 * latency],
                at: 0,
            }
        }
    }

    impl Chain for Scale {
        fn process(&mut self, block: &mut [f32]) {
            for x in block {
                *x *= self.gain;
                if !self.line.is_empty() {
                    mem::swap(x, &mut self.line[self.at]);
                    self.at = (self.at + 1) % self.line.len();
                }
            }
        }

        fn latency_frames(&self) -> usize {
            self.line.len() / 2
        }
    }

    impl Drop for Scale {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Relaxed);
        }
    }

    struct Rig {
        mixer: Mixer<Scale>,
        handoff: Arc<Handoff<Scale>>,
        controls: Arc<Controls>,
        drops: Arc<AtomicU64>,
    }

    impl Rig {
        fn new(gain: f32) -> Self {
            Self::with(gain, Arc::default())
        }

        /// A rig whose mixer carries on from `controls`, as a new session does.
        fn with(gain: f32, controls: Arc<Controls>) -> Self {
            Self::delayed(gain, 0, controls)
        }

        /// A rig on a chain that delays its output by `latency` frames.
        fn delayed(gain: f32, latency: usize, controls: Arc<Controls>) -> Self {
            let drops = Arc::new(AtomicU64::new(0));
            let handoff = Arc::new(Handoff::new());
            let chain = Box::new(Scale::new(gain, latency, &drops));
            let mixer = Mixer::new(
                chain,
                RATE,
                256,
                Arc::clone(&handoff),
                Arc::clone(&controls),
            );
            Self {
                mixer,
                handoff,
                controls,
                drops,
            }
        }

        fn post(&self, gain: f32) {
            self.post_delayed(gain, 0);
        }

        fn post_delayed(&self, gain: f32, latency: usize) {
            self.handoff.post(Scale::new(gain, latency, &self.drops));
        }

        /// Runs `input` (one sample per frame, the same on both sides)
        /// through in blocks of up to 256 frames and returns the left side.
        fn run(&mut self, input: &[f32]) -> Vec<f32> {
            let mut out = Vec::with_capacity(input.len());
            for chunk in input.chunks(256) {
                let mut block: Vec<f32> = chunk.iter().flat_map(|&x| [x, x]).collect();
                self.mixer.process(&mut block);
                out.extend(block.iter().step_by(2));
            }
            out
        }
    }

    fn sine(freq: f64, amp: f64, start: usize, n: usize) -> Vec<f32> {
        (start..start + n)
            .map(|i| (amp * (2.0 * PI * freq * i as f64 / f64::from(RATE)).sin()) as f32)
            .collect()
    }

    /// The largest change between neighbouring samples.
    fn max_step(x: &[f32]) -> f32 {
        x.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    /// The largest step a sine of `amp` at `freq` takes on its own:
    /// |sin(a + w) - sin(a)| peaks at 2 sin(w / 2).
    fn sine_step(freq: f64, amp: f64) -> f32 {
        (2.0 * amp * (PI * freq / f64::from(RATE)).sin()) as f32
    }

    #[test]
    fn a_new_chain_fades_in_over_30_ms() {
        // A constant 1 through a unity chain, then a chain that mutes it:
        // the output falls linearly from 1 to 0 over 1440 frames.
        let mut rig = Rig::new(1.0);
        assert!(rig.run(&[1.0; 512]).iter().all(|&x| x == 1.0));
        rig.post(0.0);
        let out = rig.run(&[1.0; 2048]);
        assert!((out[0] - (1.0 - 1.0 / 1440.0)).abs() < 1e-6, "{}", out[0]);
        assert!((out[719] - 0.5).abs() < 1e-6, "{}", out[719]);
        assert!(out[FADE - 2] > 0.0);
        assert!(out[FADE - 1..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_chain_change_is_continuous_where_a_swap_would_jump() {
        // A 1 kHz sine at 0.5, then a chain that inverts it: the worst case,
        // since the two outputs are opposite. The swap lands at frame 4864,
        // where the sine is at 0.866 of its peak.
        let (freq, amp) = (1000.0, 0.5);
        let input = sine(freq, amp, 0, 4864 + 4000);
        let mut rig = Rig::new(1.0);
        let mut out = rig.run(&input[..4864]);
        rig.post(-1.0);
        out.extend(rig.run(&input[4864..]));

        // Swapping without a fade would play +x then -x.
        let abrupt: Vec<f32> = input[..4864]
            .iter()
            .copied()
            .chain(input[4864..].iter().map(|x| -x))
            .collect();
        assert!(max_step(&abrupt) > 0.8, "{}", max_step(&abrupt));

        // Faded, each step is the sine's own step plus the fade's slope
        // across the 2 x amp gap: (2 amp) / 1440 frames.
        let bound = sine_step(freq, amp) + (2.0 * amp / 1440.0) as f32 + 1e-6;
        assert!(max_step(&out) <= bound, "{} > {bound}", max_step(&out));
        let tail = 4864 + FADE..out.len();
        assert!(
            out[tail.clone()]
                .iter()
                .zip(&input[tail])
                .all(|(o, i)| *o == -i)
        );
    }

    #[test]
    fn the_old_chain_is_handed_back_once_when_the_fade_ends() {
        let mut rig = Rig::new(1.0);
        rig.post(0.5);
        rig.run(&[0.1; FADE - 1]);
        assert!(rig.handoff.retired.load(Relaxed).is_null(), "retired early");
        rig.run(&[0.1; 1]);
        assert!(!rig.handoff.retired.load(Relaxed).is_null());
        assert_eq!(rig.drops.load(Relaxed), 0, "freed on the callback");
        rig.handoff.collect();
        assert_eq!(rig.drops.load(Relaxed), 1);
        rig.run(&[0.1; 4 * FADE]);
        rig.handoff.collect();
        assert_eq!(rig.drops.load(Relaxed), 1);
        assert!(rig.run(&[0.1; 4]).iter().all(|&x| x == 0.05));
        let drops = Arc::clone(&rig.drops);
        drop(rig);
        assert_eq!(drops.load(Relaxed), 2);
    }

    #[test]
    fn a_chain_posted_mid_fade_follows_once_the_fade_ends() {
        // 1 -> -1 is under way when 0.25 arrives; then 0.25 is replaced by
        // 0.5 before the callback takes it, which frees 0.25 at once.
        let (freq, amp) = (440.0, 0.5);
        let input = sine(freq, amp, 0, 20_000);
        let mut rig = Rig::new(1.0);
        let mut out = rig.run(&input[..1024]);
        rig.post(-1.0);
        out.extend(rig.run(&input[1024..1024 + FADE / 2]));
        rig.post(0.25);
        rig.post(0.5);
        assert_eq!(rig.drops.load(Relaxed), 1, "the replaced chain");
        let end_of_first = 1024 + FADE;
        out.extend(rig.run(&input[out.len()..end_of_first + 512]));
        // The first fade ran to its end: -x, while 0.5 waits for the
        // handed-back chain to be collected.
        assert!(
            out[end_of_first..]
                .iter()
                .zip(&input[end_of_first..])
                .all(|(o, i)| *o == -i)
        );
        rig.handoff.collect();
        assert_eq!(rig.drops.load(Relaxed), 2);
        let start_of_second = out.len();
        out.extend(rig.run(&input[start_of_second..]));
        let settled = start_of_second + FADE;
        assert!(
            out[settled..]
                .iter()
                .zip(&input[settled..])
                .all(|(o, i)| *o == 0.5 * i)
        );
        let bound = sine_step(freq, amp) + (2.0 * amp / 1440.0) as f32 + 1e-6;
        assert!(max_step(&out) <= bound, "{} > {bound}", max_step(&out));
        rig.handoff.collect();
        assert_eq!(rig.drops.load(Relaxed), 3);
    }

    /// Mean square of `x`.
    fn mean_square(x: &[f32]) -> f64 {
        x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64
    }

    #[test]
    fn bypass_plays_the_dry_signal_at_the_processed_level() {
        // The chain halves the signal, so the dry signal is matched at 0.5.
        let (freq, amp) = (440.0, 0.4);
        let mut rig = Rig::new(0.5);
        rig.run(&sine(freq, amp, 0, 4 * 48_000));
        assert!((rig.controls.dry_gain() - 0.5).abs() < 1e-6);
        rig.controls.bypass.store(true, Relaxed);
        let input = sine(freq, amp, 4 * 48_000, 48_000);
        let out = rig.run(&input);
        let g = rig.controls.dry_gain();
        for (o, i) in out[FADE..].iter().zip(&input[FADE..]) {
            assert!((o - g * i).abs() <= 1e-6, "{o} vs {}", g * i);
        }
        let processed = mean_square(&input[FADE..]) * 0.25;
        let bypassed = mean_square(&out[FADE..]);
        let error_db = 10.0 * (bypassed / processed).log10();
        assert!(error_db.abs() < 0.001, "{error_db} dB");
    }

    #[test]
    fn bypass_plays_the_dry_signal_as_late_as_the_chain_plays_its_output() {
        // The chain halves the signal 300 frames late; bypass plays the
        // input 300 frames late too, at the matched gain.
        let (freq, amp, latency) = (440.0, 0.4, 300);
        let mut rig = Rig::delayed(0.5, latency, Arc::default());
        let start = 4 * 48_000;
        let processed = rig.run(&sine(freq, amp, 0, start));
        let late = sine(freq, amp, start - 48_000 - latency, 48_000);
        assert!(
            processed[start - 48_000..]
                .iter()
                .zip(&late)
                .all(|(o, i)| *o == 0.5 * i)
        );
        assert!((rig.controls.dry_gain() - 0.5).abs() < 1e-6);
        rig.controls.bypass.store(true, Relaxed);
        let out = rig.run(&sine(freq, amp, start, 48_000));
        let late = sine(freq, amp, start - latency, 48_000);
        let g = rig.controls.dry_gain();
        for (o, i) in out[FADE..].iter().zip(&late[FADE..]) {
            assert!((o - g * i).abs() <= 1e-6, "{o} vs {}", g * i);
        }
    }

    #[test]
    fn bypass_matches_bursts_through_a_chain_that_delays_them() {
        // 10 ms tone bursts every 40 ms through a chain that halves them
        // 10 ms late. Each processed burst starts as its unprocessed one
        // ends, so unaligned the measurements would set unprocessed sound
        // against processed silence and hold the gain at its -12 dB floor.
        let latency = 480;
        let bursts = |start: usize, n: usize| -> Vec<f32> {
            sine(1000.0, 0.4, start, n)
                .into_iter()
                .enumerate()
                .map(|(i, x)| if (start + i) % 1920 < 480 { x } else { 0.0 })
                .collect()
        };
        let mut rig = Rig::delayed(0.5, latency, Arc::default());
        rig.run(&bursts(0, 6 * 48_000));
        let gain = rig.controls.dry_gain();
        assert!((gain - 0.5).abs() < 0.5 * 1e-4, "{gain}");
    }

    #[test]
    fn in_bypass_a_chain_with_another_latency_takes_over_without_a_jump() {
        // Bypassed at unity, a chain 300 frames late replaces one with none:
        // the dry signal moves to its new delay over the 30 ms fade, rather
        // than jumping back 300 frames, and then plays that late.
        let (freq, amp, latency) = (440.0, 0.5, 300);
        let input = sine(freq, amp, 0, 6 * 48_000);
        let mut rig = Rig::new(1.0);
        rig.controls.bypass.store(true, Relaxed);
        let at = 4 * 48_000;
        let mut out = rig.run(&input[..at]);
        rig.post_delayed(1.0, latency);
        out.extend(rig.run(&input[at..]));

        let bound = sine_step(freq, amp) + (2.0 * amp / 1440.0) as f32 + 1e-6;
        assert!(max_step(&out) <= bound, "{} > {bound}", max_step(&out));
        let jump = (input[at] - input[at - latency]).abs();
        assert!(jump > 10.0 * bound, "{jump}");
        // The gain still moves by a few parts in a million after the new
        // chain's empty delay read quieter during the fade.
        for (o, i) in out[at + FADE..].iter().zip(&input[at + FADE - latency..]) {
            assert!((o - i).abs() <= 1e-4, "{o} vs {i}");
        }
    }

    #[test]
    fn bypass_matches_the_loudness_of_the_default_chain() {
        // Two tones the shelves and compressors treat differently. The
        // reference level comes from running the chain on its own.
        let n = 12 * 48_000;
        let tone = |i: usize| {
            let t = i as f64 / f64::from(RATE);
            (0.3 * (2.0 * PI * 100.0 * t).sin() + 0.1 * (2.0 * PI * 3000.0 * t).sin()) as f32
        };
        let input: Vec<f32> = (0..n).flat_map(|i| [tone(i), tone(i)]).collect();
        let mut reference = input.clone();
        Processor::new(&Profile::default(), RATE, 2).process(&mut reference);

        let controls = Arc::new(Controls::default());
        let chain = Box::new(Processor::new(&Profile::default(), RATE, 2));
        let mut mixer = Mixer::new(
            chain,
            RATE,
            512,
            Arc::new(Handoff::new()),
            Arc::clone(&controls),
        );
        let mut out = input.clone();
        let bypass_at = 2 * 10 * 48_000;
        for (start, block) in (0..).step_by(1024).zip(out.chunks_mut(1024)) {
            controls.bypass.store(start >= bypass_at, Relaxed);
            mixer.process(block);
        }
        let last = 2 * (n - 48_000)..;
        let error_db =
            10.0 * (mean_square(&out[last.clone()]) / mean_square(&reference[last])).log10();
        assert!(error_db.abs() < 0.05, "{error_db} dB");
        // The chain really changes the level, so matching did something.
        let change_db = 10.0 * (mean_square(&reference) / mean_square(&input)).log10();
        assert!(change_db.abs() > 1.0, "{change_db} dB");
    }

    fn db(gain: f32) -> f64 {
        20.0 * f64::from(gain).log10()
    }

    #[test]
    fn bypass_rematches_within_a_second_of_a_chain_change_without_jumping() {
        // Matched at 0.5 and bypassed; then the chain goes to 1.0, so the
        // processed level rises 6 dB. The new chain's level is measured from
        // scratch once it has faded in, and the gain glides there with its
        // 0.2 s smoothing.
        let (freq, amp) = (440.0, 0.4);
        let mut rig = Rig::new(0.5);
        rig.run(&sine(freq, amp, 0, 6 * 48_000));
        rig.controls.bypass.store(true, Relaxed);
        rig.run(&sine(freq, amp, 0, 48_000));
        assert!((rig.controls.dry_gain() - 0.5).abs() < 1e-6);
        rig.post(1.0);
        let mut gains = Vec::new();
        for i in 0..6 * 48_000 / 256 {
            rig.run(&sine(freq, amp, i * 256, 256));
            gains.push(rig.controls.dry_gain());
        }
        let error_at = |seconds: f64| db(gains[(seconds * 48_000.0 / 256.0) as usize - 1]);
        let report: Vec<String> = [0.1, 0.25, 0.5, 1.0, 2.0, 6.0]
            .iter()
            .map(|&t| format!("{t} s: {:+.3} dB", error_at(t)))
            .collect();
        eprintln!("bypass level error after +6 dB: {}", report.join(", "));
        // 0.03 s of fade, then the gain closes 1 - 1/e of the gap every
        // 0.2 s: 1 - 1/2 e^(-(t - 0.03) / 0.2) is -0.425 dB at 0.5 s and
        // -0.034 dB at 1 s.
        assert!((-0.46..-0.39).contains(&error_at(0.5)), "{report:?}");
        assert!((-0.05..-0.02).contains(&error_at(1.0)), "{report:?}");
        assert!(error_at(2.0).abs() < 0.005, "{report:?}");
        // Never a jump: each 5 ms block moves the gain by at most what
        // 0.2 s smoothing allows across the whole 6 dB.
        let steps = gains.windows(2).map(|w| w[1] - w[0]);
        assert!(
            steps.clone().all(|d| (0.0..0.0134).contains(&d)),
            "{gains:?}"
        );
    }

    /// A chain whose gain the test turns while it runs, so the processed
    /// level changes without a new chain.
    struct Dial(Arc<AtomicU32>);

    impl Chain for Dial {
        fn process(&mut self, block: &mut [f32]) {
            let gain = f32::from_bits(self.0.load(Relaxed));
            block.iter_mut().for_each(|x| *x *= gain);
        }
    }

    #[test]
    fn without_a_chain_change_the_match_follows_over_3_s() {
        // The same chain drops from 0.5 to 0.25 (as a compressor's gain
        // might): the measured processed power moves 1 - 1/e of the way in
        // 3 s, as a one-pole average with a 3 s time constant does.
        let dial = Arc::new(AtomicU32::new(0.5f32.to_bits()));
        let controls = Arc::new(Controls::default());
        let mut mixer = Mixer::new(
            Box::new(Dial(Arc::clone(&dial))),
            RATE,
            256,
            Arc::new(Handoff::new()),
            Arc::clone(&controls),
        );
        let mut at = 0;
        let mut run = |seconds: usize| {
            for chunk in sine(440.0, 0.4, at, seconds * 48_000).chunks(256) {
                let mut block: Vec<f32> = chunk.iter().flat_map(|&x| [x, x]).collect();
                mixer.process(&mut block);
            }
            at += seconds * 48_000;
            controls.dry_gain()
        };
        assert!((run(6) - 0.5).abs() < 1e-6);
        dial.store(0.25f32.to_bits(), Relaxed);
        let expected = |t: f64| (0.0625 + 0.1875 * (-t / 3.0).exp()).sqrt() as f32;
        for (t, seconds) in [(1.0, 1), (3.0, 2), (10.0, 7)] {
            let gain = run(seconds);
            // The gain trails the measurement by its 0.2 s smoothing.
            let want = expected(t - 0.2);
            assert!(
                (gain - want).abs() < want * 0.005,
                "{t} s: {gain} vs {want}"
            );
        }
    }

    #[test]
    fn a_new_session_carries_on_from_the_last_measurements() {
        // A session matched at 0.5 ends; the next one starts with a chain
        // that plays louder for its first moments, as a cold chain does.
        // Those moments are a small share of 3 s of measurements, so the
        // gain hardly moves, instead of heading for their level at once.
        let controls = Arc::new(Controls::default());
        let mut rig = Rig::with(0.5, Arc::clone(&controls));
        rig.run(&sine(440.0, 0.4, 0, 5 * 48_000));
        controls.bypass.store(true, Relaxed);
        rig.run(&sine(440.0, 0.4, 0, 48_000));
        drop(rig);
        let mut rig = Rig::with(1.0, Arc::clone(&controls));
        rig.run(&sine(440.0, 0.4, 0, 48_000 / 20));
        let gain = rig.controls.dry_gain();
        assert!((gain - 0.5).abs() < 0.5 * 0.005, "{gain}");
    }

    #[test]
    fn a_new_mixer_starts_in_bypass_at_the_carried_gain() {
        // Bypass is on and matched at 0.5 when a session ends; the next one
        // plays the dry signal at 0.5 from its first frame, with no fade in
        // from the processed signal (here 0.25 of the input).
        let controls = Arc::new(Controls::default());
        controls.bypass.store(true, Relaxed);
        controls.dry_gain.store(0.5f32.to_bits(), Relaxed);
        let carried = Levels {
            dry: 0.08,
            wet: 0.02,
            frames: 144_000.0,
        };
        carried.store(&controls);
        let mut rig = Rig::with(0.25, controls);
        assert_eq!(rig.mixer.levels, carried);
        let input = sine(440.0, 0.4, 1, 256);
        let out = rig.run(&input);
        for (o, i) in out.iter().zip(&input) {
            assert!((o - 0.5 * i).abs() < 1e-5, "{o} vs {}", 0.5 * i);
        }
    }

    #[test]
    fn signals_below_minus_120_dbfs_leave_the_match_alone() {
        // A chain that halves anything audible and passes the noise floor
        // through: minutes of noise at -130 dBFS would match bypass to the
        // noise floor's 1.0 if they counted.
        struct Floor;
        impl Chain for Floor {
            fn process(&mut self, block: &mut [f32]) {
                for x in block {
                    *x *= if x.abs() < 1e-5 { 1.0 } else { 0.5 };
                }
            }
        }
        let controls = Arc::new(Controls::default());
        let mut mixer = Mixer::new(
            Box::new(Floor),
            RATE,
            256,
            Arc::new(Handoff::new()),
            Arc::clone(&controls),
        );
        let mut run = |samples: Vec<f32>| {
            for chunk in samples.chunks(512) {
                mixer.process(&mut chunk.to_vec());
            }
        };
        run(sine(440.0, 0.4, 0, 3 * 48_000)
            .iter()
            .flat_map(|&x| [x, x])
            .collect());
        assert!((controls.dry_gain() - 0.5).abs() < 1e-6);
        // RMS 3e-7 on each side: -130 dBFS.
        let floor: Vec<f32> = (0..2 * 48_000)
            .map(|i| if i % 4 < 2 { 3e-7 } else { -3e-7 })
            .collect();
        for _ in 0..120 {
            run(floor.clone());
        }
        assert!(
            (controls.dry_gain() - 0.5).abs() < 1e-6,
            "{}",
            controls.dry_gain()
        );
    }

    #[test]
    fn quiet_material_still_counts() {
        // After a chain change the next material is at -70 dBFS: quiet, not
        // silent, so bypass re-matches on it.
        let mut rig = Rig::new(0.5);
        rig.run(&sine(440.0, 0.4, 0, 3 * 48_000));
        rig.post(0.25);
        let amp = 10f64.powf(-70.0 / 20.0) * 2f64.sqrt();
        rig.run(&sine(440.0, amp, 0, 3 * 48_000));
        let gain = rig.controls.dry_gain();
        assert!((gain - 0.25).abs() < 0.25 * 0.01, "{gain}");
    }

    /// White noise in [-amp, amp), different on each side, interleaved.
    fn noise(frames: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..2 * frames)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((state >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// `input` through a mixer on a chain for `first`, posting one for
    /// `then.1` once `then.0` frames have played.
    fn through(input: &[f32], first: &Profile, then: Option<(usize, &Profile)>) -> Vec<f32> {
        let handoff = Arc::new(Handoff::new());
        let chain = Box::new(Processor::new(first, RATE, 2));
        let mut mixer = Mixer::new(chain, RATE, 256, Arc::clone(&handoff), Arc::default());
        let mut out = input.to_vec();
        for (i, block) in out.chunks_mut(512).enumerate() {
            if let Some((at, next)) = then
                && i * 256 == at
            {
                handoff.post(Processor::new(next, RATE, 2));
            }
            mixer.process(block);
            handoff.collect();
        }
        out
    }

    /// Level in dB of each 5 ms window of interleaved stereo.
    fn window_db(x: &[f32]) -> Vec<f64> {
        x.chunks(2 * 240)
            .map(|w| 10.0 * mean_square(w).log10())
            .collect()
    }

    #[test]
    fn a_new_identical_chain_keeps_the_level_of_loud_material() {
        // Loud noise, pulled down by both compressors, by compressor 2 alone,
        // or with its side boost pulled back by the stereo AGC. A new chain
        // whose detectors started from zero would play it uncompressed until
        // they caught up: about 2 dB louder for 40 ms.
        let off = CompressorParams {
            ratio: 1.0,
            ..CompressorParams::default()
        };
        let second_only = Profile {
            compressor1: off,
            compressor2: CompressorParams {
                threshold_db: -30.0,
                ..CompressorParams::default()
            },
            ..Profile::default()
        };
        // The 12 dB side boost keeps this noise at the limiter's ceiling,
        // which would hide what the AGC does, so the limiter is off; its own
        // hand-over has its own test.
        let agc_only = Profile {
            compressor1: off,
            compressor2: off,
            stereo: StereoParams {
                enabled: true,
                side_boost_db: 12.0,
                attack_ms: 100.0,
                ..StereoParams::default()
            },
            limiter: LimiterParams {
                enabled: false,
                ..LimiterParams::default()
            },
            ..Profile::default()
        };
        let input = noise(3 * 48_000, 0.6, 1);
        let at = 2 * 48_000;
        for (name, profile) in [
            ("default", Profile::default()),
            ("compressor 2", second_only),
            ("stereo AGC", agc_only),
        ] {
            let kept = window_db(&through(&input, &profile, None)[2 * at..]);
            let swapped = window_db(&through(&input, &profile, Some((at, &profile)))[2 * at..]);
            let worst = swapped
                .iter()
                .zip(&kept)
                .map(|(a, b)| a - b)
                .fold(0.0f64, |w, d| if d.abs() > w.abs() { d } else { w });
            eprintln!("identical {name} chain swap: worst 5 ms level difference {worst:+.3} dB");
            assert!(worst.abs() < 0.2, "{name}: {worst} dB");
            // A cold chain really would be louder here.
            let mut cold = input[2 * at..].to_vec();
            Processor::new(&profile, RATE, 2).process(&mut cold);
            let louder = window_db(&cold)
                .iter()
                .zip(&kept)
                .map(|(a, b)| a - b)
                .fold(0.0f64, f64::max);
            assert!(
                louder > 1.0,
                "{name}: a cold chain is only {louder} dB louder"
            );
        }
    }

    #[test]
    fn a_new_identical_chain_keeps_the_limiters_gain() {
        // A second of noise 9 dB over the ceiling, then noise under it: the
        // new chain arrives 5 ms later, while the limiter still holds its
        // gain down and before it has released. Started at unity, it would
        // play the quieter noise at once at full level, about 9 dB louder.
        let off = CompressorParams {
            ratio: 1.0,
            ..CompressorParams::default()
        };
        let limiter_only = Profile {
            output_gain_db: 0.0,
            low_shelf_db: 0.0,
            high_shelf_db: 0.0,
            compressor1: off,
            compressor2: off,
            ..Profile::default()
        };
        let at = 188 * 256;
        let loud = at - 256;
        let mut input = noise(3 * 48_000, 2.5, 5);
        input[2 * loud..].iter_mut().for_each(|x| *x *= 0.3 / 2.5);
        let kept = through(&input, &limiter_only, None);
        let swapped = through(&input, &limiter_only, Some((at, &limiter_only)));
        let (kept, swapped) = (window_db(&kept[2 * at..]), window_db(&swapped[2 * at..]));
        let worst = swapped
            .iter()
            .zip(&kept)
            .map(|(a, b)| a - b)
            .fold(0.0f64, |w, d| if d.abs() > w.abs() { d } else { w });
        eprintln!("identical limiting chain swap: worst 5 ms level difference {worst:+.3} dB");
        assert!(worst.abs() < 0.01, "{worst} dB");
        let mut cold = input[2 * at..].to_vec();
        Processor::new(&limiter_only, RATE, 2).process(&mut cold);
        let louder = window_db(&cold)
            .iter()
            .zip(&kept)
            .map(|(a, b)| a - b)
            .fold(0.0f64, f64::max);
        assert!(louder > 6.0, "a cold chain is only {louder} dB louder");
    }

    #[test]
    fn a_new_identical_chain_hardly_moves_the_bypass_gain() {
        // In bypass the new chain's level is measured from scratch, starting
        // from a few frames that say little; the gain's smoothing keeps that
        // from showing. The swap comes once the measurements no longer hold
        // the first chain's cold start.
        let input = noise(14 * 48_000, 0.2, 4);
        let swap = 12 * 48_000 / 256;
        let default = Profile::default();
        let controls = Arc::new(Controls::default());
        let handoff = Arc::new(Handoff::new());
        let chain = Box::new(Processor::new(&default, RATE, 2));
        let mut mixer = Mixer::new(
            chain,
            RATE,
            256,
            Arc::clone(&handoff),
            Arc::clone(&controls),
        );
        let mut gains = Vec::new();
        for (i, block) in input.clone().chunks_mut(512).enumerate() {
            if i == swap {
                controls.bypass.store(true, Relaxed);
                handoff.post(Processor::new(&default, RATE, 2));
            }
            mixer.process(block);
            handoff.collect();
            gains.push(controls.dry_gain());
        }
        let settled = gains[swap - 1];
        let worst = gains[swap..]
            .iter()
            .map(|&g| db(g / settled))
            .fold(0.0f64, |w, d| if d.abs() > w.abs() { d } else { w });
        eprintln!("bypassed identical chain swap: gain moves at most {worst:+.3} dB");
        assert!(worst.abs() < 0.05, "{worst} dB");
    }

    #[test]
    fn a_small_change_moves_the_level_without_overshoot() {
        // +0.5 dB on the output, and +0.5 dB into compressor 1: through the
        // swap each 5 ms window stays between the old chain's level and the
        // new one's, give or take 0.2 dB.
        let input = noise(3 * 48_000, 0.6, 2);
        let at = 2 * 48_000;
        let default = Profile::default();
        for louder in [
            Profile {
                output_gain_db: default.output_gain_db + 0.5,
                ..Profile::default()
            },
            Profile {
                input_gain_db: 0.5,
                ..Profile::default()
            },
        ] {
            let old = window_db(&through(&input, &default, None)[2 * at..]);
            let new = window_db(&through(&input, &louder, None)[2 * at..]);
            let swapped = window_db(&through(&input, &default, Some((at, &louder)))[2 * at..]);
            let mut worst = 0.0f64;
            for ((s, o), n) in swapped.iter().zip(&old).zip(&new) {
                let outside = (s - o.max(*n)).max(o.min(*n) - s).max(0.0);
                worst = worst.max(outside);
            }
            eprintln!(
                "+0.5 dB swap ({} in, {} out): worst {worst:.3} dB outside old..new",
                louder.input_gain_db, louder.output_gain_db
            );
            assert!(worst < 0.2, "{worst} dB");
        }
    }

    #[test]
    fn bypass_keeps_the_dry_balance_and_matches_the_total_level() {
        // The right side 20 dB below the left: the per-side compressors pull
        // the left down more than the right. One gain matches the total
        // loudness and keeps the dry left/right balance; neither side alone
        // is matched (the right ends up about 7 dB quiet).
        let frames = 12 * 48_000;
        let mut input = noise(frames, 0.2, 3);
        input.iter_mut().skip(1).step_by(2).for_each(|x| *x *= 0.1);
        let profile = Profile::default();
        let mut reference = input.clone();
        Processor::new(&profile, RATE, 2).process(&mut reference);
        let controls = Arc::new(Controls::default());
        let chain = Box::new(Processor::new(&profile, RATE, 2));
        let mut mixer = Mixer::new(
            chain,
            RATE,
            512,
            Arc::new(Handoff::new()),
            Arc::clone(&controls),
        );
        let mut out = input.clone();
        for (start, block) in (0..).step_by(512).zip(out.chunks_mut(1024)) {
            controls.bypass.store(start >= 10 * 48_000, Relaxed);
            mixer.process(block);
        }
        let last = 2 * (frames - 48_000)..;
        let side =
            |x: &[f32], c: usize| -> Vec<f32> { x.iter().skip(c).step_by(2).copied().collect() };
        let level = |x: &[f32]| 10.0 * mean_square(x).log10();
        let (out, reference, input) = (&out[last.clone()], &reference[last.clone()], &input[last]);
        let total = level(out) - level(reference);
        let left = level(&side(out, 0)) - level(&side(reference, 0));
        let right = level(&side(out, 1)) - level(&side(reference, 1));
        let balance = (level(&side(out, 0)) - level(&side(out, 1)))
            - (level(&side(input, 0)) - level(&side(input, 1)));
        eprintln!(
            "bypass with R 20 dB below L: total {total:+.3} dB, left {left:+.2} dB, right {right:+.2} dB, \
             balance change {balance:+.4} dB"
        );
        assert!(total.abs() < 0.05, "{total} dB");
        assert!(balance.abs() < 1e-3, "{balance} dB");
    }

    #[test]
    fn matched_gain_is_limited_to_12_db() {
        for (chain, want) in [(10.0, 3.981_072), (0.01, 0.251_189)] {
            let mut rig = Rig::new(chain);
            rig.run(&sine(440.0, 0.05, 0, 5 * 48_000));
            let gain = rig.controls.dry_gain();
            assert!((gain - want).abs() < want * 1e-4, "{chain}: {gain}");
        }
    }

    #[test]
    fn matched_gain_holds_through_silence() {
        let mut rig = Rig::new(0.5);
        rig.run(&sine(440.0, 0.4, 0, 3 * 48_000));
        rig.run(&vec![0.0; 120 * 48_000]);
        assert!((rig.controls.dry_gain() - 0.5).abs() < 1e-3);
    }

    #[test]
    fn toggling_bypass_is_click_free() {
        // The chain inverts the signal at the same level, so the dry signal
        // plays at unity and each toggle crosses from -x to +x and back.
        let (freq, amp) = (1000.0, 0.5);
        let input = sine(freq, amp, 0, 48_000);
        let mut rig = Rig::new(-1.0);
        let mut out = rig.run(&input[..8192]);
        let toggles = [8192, 8192 + 700, 20_000, 30_000];
        for (i, at) in toggles.iter().enumerate() {
            rig.controls.bypass.store(i % 2 == 0, Relaxed);
            let next = toggles.get(i + 1).copied().unwrap_or(input.len());
            out.extend(rig.run(&input[*at..next]));
        }
        let bound = sine_step(freq, amp) + (2.0 * amp / 1440.0) as f32 + 1e-4;
        assert!(max_step(&out) <= bound, "{} > {bound}", max_step(&out));
        // Fully bypassed between 20 000 + 30 ms and 30 000.
        for (o, i) in out[20_000 + FADE..30_000]
            .iter()
            .zip(&input[20_000 + FADE..])
        {
            assert!((o - i).abs() < 1e-4, "{o} vs {i}");
        }
        assert!(
            out[30_000 + FADE..]
                .iter()
                .zip(&input[30_000 + FADE..])
                .all(|(o, i)| *o == -i)
        );
    }

    /// Passes audio through and meters it: its reductions are 10, 20 and 30
    /// times the largest sample since the last reading, and its leveler
    /// gain the negated last sample, so a test can steer them.
    #[derive(Default)]
    struct Metered {
        peak: f32,
        last: f32,
    }

    impl Chain for Metered {
        fn process(&mut self, block: &mut [f32]) {
            for &x in block.iter() {
                self.peak = self.peak.max(x.abs());
                self.last = x;
            }
        }

        fn meters(&mut self) -> Meters {
            let peak = mem::take(&mut self.peak);
            Meters {
                leveler_db: -self.last,
                compressor1_db: 10.0 * peak,
                compressor2_db: 20.0 * peak,
                limiter_db: 30.0 * peak,
            }
        }
    }

    #[test]
    fn meters_show_the_largest_reduction_since_the_last_reading() {
        let controls = Arc::new(Controls::default());
        let mut mixer = Mixer::new(
            Box::new(Metered::default()),
            RATE,
            256,
            Arc::new(Handoff::new()),
            Arc::clone(&controls),
        );
        let mut play = |levels: &[f32]| {
            for &level in levels {
                mixer.process(&mut [level; 512]);
            }
        };
        let close = |got: f32, want: f32| (got - want).abs() < 1e-5;
        play(&[0.1, 0.5, 0.2]);
        let r = controls.readings();
        assert!(close(r.chain.compressor1_db, 5.0), "{r:?}");
        assert!(close(r.chain.compressor2_db, 10.0), "{r:?}");
        assert!(close(r.chain.limiter_db, 15.0), "{r:?}");
        assert!(close(r.chain.leveler_db, -0.2), "{r:?}");
        // Reading started them over: the next shows only what came since.
        play(&[0.05, 0.03]);
        let r = controls.readings();
        assert!(close(r.chain.compressor1_db, 0.5), "{r:?}");
        assert!(close(r.chain.leveler_db, -0.03), "{r:?}");
        // Nothing since: no reduction, and the leveler still where it was.
        let r = controls.readings();
        assert_eq!(r.chain.limiter_db, 0.0);
        assert!(close(r.chain.leveler_db, -0.03), "{r:?}");
    }

    #[test]
    fn loudness_meters_read_the_input_and_what_plays() {
        // A 997 Hz sine at 0.5 on both sides reads -6.02 LUFS; the chain
        // halves it to -12.04, and bypass, matched, plays it at that too.
        let mut rig = Rig::new(0.5);
        assert_eq!(rig.controls.readings(), Readings::default());
        rig.run(&vec![0.0; 48_000]);
        let r = rig.controls.readings();
        assert_eq!(
            (r.input_lufs, r.output_lufs),
            (f32::NEG_INFINITY, f32::NEG_INFINITY)
        );
        rig.run(&sine(997.0, 0.5, 0, 4 * 48_000));
        let r = rig.controls.readings();
        assert!((r.input_lufs + 6.02).abs() < 0.05, "{r:?}");
        assert!((r.output_lufs + 12.04).abs() < 0.05, "{r:?}");
        rig.controls.bypass.store(true, Relaxed);
        rig.run(&sine(997.0, 0.5, 0, 4 * 48_000));
        let r = rig.controls.readings();
        assert!((r.input_lufs + 6.02).abs() < 0.05, "{r:?}");
        assert!((r.output_lufs + 12.04).abs() < 0.05, "{r:?}");
    }

    #[test]
    fn a_broken_chain_reaches_the_output_even_in_bypass() {
        let mut rig = Rig::new(0.5);
        rig.controls.bypass.store(true, Relaxed);
        rig.run(&[0.1; 4096]);
        rig.post(f32::NAN);
        let out = rig.run(&[0.1; 256]);
        assert!(out.iter().any(|x| x.is_nan()));
    }
}
