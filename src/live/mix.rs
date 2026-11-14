//! What the audio callback plays: one of the tuner's two slots, A or B,
//! each a chain crossfaded from its old one whenever a new one arrives, or
//! in bypass the unprocessed signal crossfaded the same way.
//!
//! A slot without a chain of its own plays the other slot's, so while the
//! two slots hold the same profile only one chain runs. Once they differ,
//! both run on every block, the one not heard included, so switching
//! between them is a 30 ms crossfade between two warm chains. A chain
//! posted during a fade, in either slot, waits for it to end, so a slot
//! runs at most two chains.
//!
//! While both run, each one's K-weighted loudness is measured, and the
//! louder slot is turned down by one smoothed gain to the quieter's
//! loudness, never up, so a comparison is not won by being louder. The
//! measurements are running means of the frames that are not silent, as
//! bypass keeps them (see below), started over whenever a fade ends, so the
//! match settles within about a second of a change and holds through
//! silence. K-weighting rather than plain power, because two profiles
//! mostly differ in tone: plain power would count a bass boost as far
//! louder than it sounds.
//!
//! Bypass plays the unprocessed signal through one gain, the same on both
//! sides, chosen so its total (left plus right) power matches what the
//! heard slot plays, matching gain included. One gain keeps the unprocessed
//! signal's own stereo balance, so the comparison hears what the chain does
//! to the balance as well; with the two sides at very different levels,
//! each side on its own can be further off than the total.
//!
//! The chains keep running in bypass, so their filters stay warm and the
//! loudness bypass is matched to stays measured.
//!
//! The unprocessed signal is delayed as far as the heard chain delays its
//! output, so the bypass crossfade mixes the same moment of the audio and
//! the levels compare the same stretch of it. Through a fade between chains
//! or slots with other latencies, the delay moves with the same weights as
//! the chains' outputs, which comb-filters the unprocessed signal for that
//! long.
//!
//! Once a block, the mixer publishes meters for the screen: the heard
//! chain's gain reductions and leveler gain, and the short-term loudness of
//! the unprocessed input and of what plays.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};

use dps::Meters;
use dps::limiter::Limiter;
use dps::loudness::{KWeighting, Meter};

use super::{Chain, Handoff, Offer};

/// Length of every crossfade: between chains, between slots, and into and
/// out of bypass.
pub const FADE_SECONDS: f64 = 0.03;
/// Time constant of the loudness measurements bypass and level matching
/// use. They start over once a fade has ended, averaging every frame
/// equally until they span this long.
const LEVEL_SECONDS: f64 = 3.0;
/// Time constant of the smoothing on the unprocessed signal's gain and on
/// the slots' matching gains.
const GAIN_SECONDS: f64 = 0.2;
/// The most bypass raises or lowers the unprocessed signal, and the most
/// matching turns a slot down: 12 dB.
const MAX_GAIN_DB: f64 = 12.0;
/// Mean square below which an unprocessed frame counts as silent and is left
/// out of the measurements, so the gains hold through silence (-120 dBFS).
const SILENT: f64 = 1e-12;
/// Window of the loudness meters: BS.1770's short-term loudness.
const METER_SECONDS: f64 = 3.0;
/// How long level matching waits after a fade before measuring, for its
/// filters to let go of the fade: twelve time constants of the K-weighting
/// high-pass.
const SETTLE_SECONDS: f64 = 0.05;

/// Shared by the main thread and the audio callback for the whole run, so a
/// new session after a device change starts where the last one left off.
pub struct Controls {
    /// Main thread: play the unprocessed signal instead of the processed one.
    pub bypass: AtomicBool,
    /// Main thread: the slot to hear, 0 for A or 1 for B.
    slot: AtomicUsize,
    /// Main thread: turn the louder slot down to the quieter one's loudness.
    matching: AtomicBool,
    /// Callback: the gain bypass puts on the unprocessed signal, as f32 bits.
    dry_gain: AtomicU32,
    /// Callback: the measurements the gain comes from, as f64 bits.
    levels: [AtomicU64; 3],
    /// Main thread: the system volume in dB below maximum, as f32 bits;
    /// NaN when unknown.
    volume_db: AtomicU32,
    /// Callback: the gain matching puts on each slot, in dB (never above 0)
    /// as f32 bits.
    trim: [AtomicU32; 2],
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
            slot: AtomicUsize::new(0),
            matching: AtomicBool::new(true),
            dry_gain: AtomicU32::new(1f32.to_bits()),
            levels: Default::default(),
            volume_db: AtomicU32::new(f32::NAN.to_bits()),
            trim: Default::default(),
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

    pub fn set_volume_db(&self, volume_db: Option<f32>) {
        let bits = volume_db.unwrap_or(f32::NAN).to_bits();
        self.volume_db.store(bits, Relaxed);
    }

    pub fn volume_db(&self) -> Option<f32> {
        Some(f32::from_bits(self.volume_db.load(Relaxed))).filter(|v| !v.is_nan())
    }

    pub fn slot(&self) -> usize {
        self.slot.load(Relaxed)
    }

    pub fn set_slot(&self, slot: usize) {
        self.slot.store(slot.min(1), Relaxed);
    }

    pub fn matching(&self) -> bool {
        self.matching.load(Relaxed)
    }

    pub fn set_matching(&self, on: bool) {
        self.matching.store(on, Relaxed);
    }

    /// The gain matching puts on A and on B, in dB; at most one is below 0.
    pub fn trim_db(&self) -> [f32; 2] {
        self.trim
            .each_ref()
            .map(|t| f32::from_bits(t.load(Relaxed)))
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

/// Level matching between the slots: running means of each one's
/// K-weighted power, kept as `Levels` keeps them, and the gain on each. The
/// filters run all the while both slots do, and a measurement started over
/// waits until what they hold of the fade before it has died away.
struct Trim {
    weighting: [[KWeighting; 2]; 2],
    power: [f64; 2],
    frames: f64,
    /// Frames still to wait before measuring, and how many a restart waits.
    settling: usize,
    settle: usize,
    /// The gain on each slot, and where it is heading; never above 1.
    gains: [f64; 2],
    targets: [f64; 2],
}

impl Trim {
    fn new(fs: f64) -> Self {
        let pair = || [KWeighting::new(fs), KWeighting::new(fs)];
        Self {
            weighting: [pair(), pair()],
            power: [0.0; 2],
            frames: 0.0,
            settling: 0,
            settle: (SETTLE_SECONDS * fs).round() as usize,
            gains: [1.0; 2],
            targets: [1.0; 2],
        }
    }

    /// Measures from scratch, once the filters have settled.
    fn restart(&mut self) {
        self.power = [0.0; 2];
        self.frames = 0.0;
        self.settling = self.settle;
    }

    /// The K-weighted power of one frame of each slot's output.
    fn weigh(&mut self, frames: [&[f32]; 2]) -> [f64; 2] {
        let mut k = [0.0; 2];
        for ((k, weighting), frame) in k.iter_mut().zip(&mut self.weighting).zip(frames) {
            *k = frame
                .iter()
                .zip(weighting)
                .map(|(&x, w)| f64::from(w.tick(x)).powi(2))
                .sum();
        }
        k
    }

    /// Adds one frame's powers from `weigh`, if it is not silent.
    fn add(&mut self, k: [f64; 2], silent: bool, max_frames: f64) {
        if self.settling > 0 {
            self.settling -= 1;
            return;
        }
        if silent {
            return;
        }
        self.frames = (self.frames + 1.0).min(max_frames);
        let weight = 1.0 / self.frames;
        for (power, k) in self.power.iter_mut().zip(k) {
            *power += weight * (k - *power);
        }
    }

    /// Aims the gains: the louder slot down to the quieter one's loudness
    /// if `on`, else both at unity. Without a measurement they hold.
    fn aim(&mut self, on: bool, max_db: f64) {
        let [a, b] = self.power;
        if !on {
            self.targets = [1.0; 2];
        } else if self.frames > 0.0 && a > 0.0 && b > 0.0 {
            // Positive when A is the louder.
            let db = (10.0 * (a / b).log10()).clamp(-max_db, max_db);
            self.targets = [-db.max(0.0), db.min(0.0)].map(|d| 10f64.powf(d / 20.0));
        }
    }
}

/// One slot's chains, and what they made of the block.
struct Slot<C> {
    /// The chain the slot plays; `None` plays the other slot's.
    chain: Option<Box<C>>,
    /// What the slot is fading from, and how many frames of the fade have
    /// played: the chain it played before, or `None` for the other slot's.
    fading: Option<(Option<Box<C>>, usize)>,
    /// The slot's output, and the unprocessed signal as late as it.
    out: Vec<f32>,
    dry: Vec<f32>,
    /// The same of the chain it is fading from.
    old: Vec<f32>,
    old_dry: Vec<f32>,
}

impl<C: Chain> Slot<C> {
    fn new(chain: Option<Box<C>>, block_frames: usize) -> Self {
        Self {
            chain,
            fading: None,
            out: vec![0.0; 2 * block_frames],
            dry: vec![0.0; 2 * block_frames],
            old: vec![0.0; 2 * block_frames],
            old_dry: vec![0.0; 2 * block_frames],
        }
    }

    /// Whether what it plays comes from the other slot's chain, now or in
    /// its fade.
    fn leans(&self) -> bool {
        self.chain.is_none() || matches!(self.fading, Some((None, _)))
    }

    /// Runs its chains on `input`, a block of unprocessed frames that ends
    /// at `history`'s write position; true if one made a non-finite sample.
    fn run(&mut self, input: &[f32], history: &History) -> bool {
        let n = input.len();
        let mut broken = false;
        if let Some(chain) = &mut self.chain {
            broken |= run_chain(
                &mut **chain,
                input,
                &mut self.out[..n],
                &mut self.dry[..n],
                history,
            );
        }
        if let Some((Some(old), _)) = &mut self.fading {
            broken |= run_chain(
                &mut **old,
                input,
                &mut self.old[..n],
                &mut self.old_dry[..n],
                history,
            );
        }
        broken
    }

    /// Turns its `out` and `dry` for `n` samples into what it plays, which
    /// fades from the old chain, or from or to `other`'s, over `fade` frames.
    fn resolve(&mut self, other: &Self, n: usize, fade: usize) {
        // `to` faded in over `from`, `done` frames into the fade.
        let weigh = |done: usize, from: &[f32], to: &mut [f32]| {
            for (f, (to, from)) in to.chunks_exact_mut(2).zip(from.chunks_exact(2)).enumerate() {
                let t = ((done + f + 1) as f32 / fade as f32).min(1.0);
                for (x, o) in to.iter_mut().zip(from) {
                    *x = (1.0 - t) * o + t * *x;
                }
            }
        };
        let latency = |chain: &Option<Box<C>>| chain.as_ref().map(|c| c.latency_frames());
        let (own, done, from_own) = match &self.fading {
            None => (self.chain.is_some(), 0, None),
            Some((old, done)) => (self.chain.is_some(), *done, Some(latency(old))),
        };
        match (own, from_own) {
            (true, None) => {}
            (true, Some(Some(old_latency))) => {
                weigh(done, &self.old[..n], &mut self.out[..n]);
                if latency(&self.chain) != Some(old_latency) {
                    weigh(done, &self.old_dry[..n], &mut self.dry[..n]);
                }
            }
            (true, Some(None)) => {
                weigh(done, &other.out[..n], &mut self.out[..n]);
                weigh(done, &other.dry[..n], &mut self.dry[..n]);
            }
            (false, Some(Some(_))) => {
                self.out[..n].copy_from_slice(&other.out[..n]);
                self.dry[..n].copy_from_slice(&other.dry[..n]);
                weigh(done, &self.old[..n], &mut self.out[..n]);
                weigh(done, &self.old_dry[..n], &mut self.dry[..n]);
            }
            (false, _) => {
                self.out[..n].copy_from_slice(&other.out[..n]);
                self.dry[..n].copy_from_slice(&other.dry[..n]);
            }
        }
    }
}

/// `chain`'s output for `input` into `out`, and the input as late as it
/// into `dry`; true if a sample came out non-finite.
fn run_chain<C: Chain>(
    chain: &mut C,
    input: &[f32],
    out: &mut [f32],
    dry: &mut [f32],
    history: &History,
) -> bool {
    out.copy_from_slice(input);
    chain.process(out);
    history.read(chain.latency_frames(), dry);
    !out.iter().all(|x| x.is_finite())
}

/// A ring of the last unprocessed frames, the next to be written at `at`,
/// long enough for a block and any chain's latency.
struct History {
    frames: Vec<f32>,
    at: usize,
}

impl History {
    fn write(&mut self, block: &[f32]) {
        let size = self.frames.len() / 2;
        for frame in block.chunks_exact(2) {
            self.frames[2 * self.at..2 * self.at + 2].copy_from_slice(frame);
            self.at = if self.at + 1 == size { 0 } else { self.at + 1 };
        }
    }

    /// Into `dry`, the frames of the block just written, each `delay`
    /// frames earlier.
    fn read(&self, delay: usize, dry: &mut [f32]) {
        let size = self.frames.len() / 2;
        // The ring holds a block and the longest latency, so this is never
        // more than a lap behind.
        let mut at = (self.at + size - dry.len() / 2 - delay) % size;
        for frame in dry.chunks_exact_mut(2) {
            frame.copy_from_slice(&self.frames[2 * at..2 * at + 2]);
            at = if at + 1 == size { 0 } else { at + 1 };
        }
    }
}

/// The callback's processing: no allocation, locks or frees.
pub struct Mixer<C> {
    slots: [Slot<C>; 2],
    handoff: Arc<Handoff<C>>,
    controls: Arc<Controls>,
    fade_frames: usize,
    /// How far the heard slot has moved from A to B, in frames: 0 to
    /// `fade_frames`.
    heard: usize,
    /// The unprocessed block, delayed as far as what is heard.
    dry: Vec<f32>,
    history: History,
    /// How far bypass has faded in, in frames: 0 to `fade_frames`.
    dry_frames: usize,
    levels: Levels,
    /// The most `levels.frames` and `trim.frames` reach.
    level_frames: f64,
    max_gain: f64,
    /// The gain on the unprocessed signal, and where it is heading.
    gain: f64,
    target: f64,
    gain_coef: f64,
    trim: Trim,
    /// Loudness of the unprocessed input, and of what plays.
    input_meter: Meter,
    output_meter: Meter,
}

impl<C: Chain> Mixer<C> {
    /// A mixer for blocks of up to `block_frames` interleaved stereo frames,
    /// carrying on with the bypass state, gain and measurements in `controls`
    /// and playing `chain` in the slot it names.
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
        let heard = controls.slot();
        let mut chain = Some(chain);
        let slots =
            [0, 1].map(|s| Slot::new(if s == heard { chain.take() } else { None }, block_frames));
        let history = Limiter::max_latency_frames(fs) + block_frames + 1;
        Self {
            slots,
            fade_frames,
            heard: heard * fade_frames,
            dry: vec![0.0; 2 * block_frames],
            history: History {
                frames: vec![0.0; 2 * history],
                at: 0,
            },
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
            trim: Trim::new(fs),
            input_meter: Meter::new(fs, 2, METER_SECONDS),
            output_meter: Meter::new(fs, 2, METER_SECONDS),
            handoff,
            controls,
        }
    }

    /// Calls `f` on every chain running: each slot's, and any being faded
    /// out.
    fn for_each_chain(&mut self, mut f: impl FnMut(&mut C)) {
        for slot in &mut self.slots {
            if let Some(chain) = &mut slot.chain {
                f(chain);
            }
            if let Some((Some(old), _)) = &mut slot.fading {
                f(old);
            }
        }
    }

    /// Replaces the unprocessed interleaved stereo frames in `block` with
    /// what to play. A non-finite sample from any chain running, heard or
    /// not, leaves the block non-finite, even in bypass, so the caller
    /// silences the output.
    pub fn process(&mut self, block: &mut [f32]) {
        self.take_offer();
        // Every chain's meters start over with each block.
        let volume_db = self.controls.volume_db();
        self.for_each_chain(|chain| {
            chain.set_volume_db(volume_db);
            chain.meters();
        });
        let n = block.len();
        for frame in block.chunks_exact(2) {
            self.input_meter.frame(frame);
        }
        self.history.write(block);
        let mut broken = false;
        for slot in &mut self.slots {
            broken |= slot.run(block, &self.history);
        }
        // A slot that leans on the other is worked out after it; at most
        // one does, as `take_offer` sees to it.
        let first = usize::from(self.slots[0].leans());
        let [a, b] = &mut self.slots;
        let (first_slot, second_slot) = if first == 0 { (a, b) } else { (b, a) };
        first_slot.resolve(second_slot, n, self.fade_frames);
        second_slot.resolve(first_slot, n, self.fade_frames);

        let switched = self.mix_slots(block);
        let finished = self.end_fades(n / 2);
        if broken {
            block.fill(f32::NAN);
        } else if block.iter().all(|x| x.is_finite()) {
            self.mix_dry(block);
        }
        if switched || finished {
            // What plays changed, so its level is measured from scratch, from
            // the next block, which has none of the fade in it, and bypass
            // follows it within about a second rather than `LEVEL_SECONDS`;
            // the gain's smoothing keeps that from jumping.
            self.levels.frames = 0.0;
        }
        for frame in block.chunks_exact(2) {
            self.output_meter.frame(frame);
        }
        let [a, b] = &mut self.slots;
        let (heard, other) = if self.controls.slot() == 0 {
            (a, b)
        } else {
            (b, a)
        };
        let meters = match (&mut heard.chain, &mut other.chain) {
            (Some(chain), _) | (None, Some(chain)) => chain.meters(),
            (None, None) => Meters::default(),
        };
        self.controls
            .publish(meters, self.input_meter.lufs(), self.output_meter.lufs());
    }

    /// Takes a new chain, or word to play the other slot's, for at most one
    /// slot, and starts its fade.
    fn take_offer(&mut self) {
        // An offer made during a fade waits for it to end, so only one slot
        // fades at a time and the other always has what the first fades
        // from or to.
        if self.slots.iter().any(|s| s.fading.is_some()) {
            return;
        }
        // Both offers wait while either slot cannot take its own: taken
        // without the other's, an offer to play the other slot's chain
        // could find that slot still without one and be lost.
        if self.handoff.blocked() {
            return;
        }
        // A slot playing the other's chain goes first: the main thread only
        // asks a slot to play the other's chain once that one has its own,
        // and with the order of the lanes' offers that holds here too.
        let order = if self.slots[1].chain.is_none() {
            [1, 0]
        } else {
            [0, 1]
        };
        for s in order {
            let (this, other) = match &mut self.slots {
                [a, b] if s == 0 => (a, &*b),
                [a, b] => (b, &*a),
            };
            match self.handoff.take(s) {
                None => {}
                Some(Offer::Chain(mut new)) => {
                    if let Some(from) = this.chain.as_deref().or(other.chain.as_deref()) {
                        new.adopt(from);
                    }
                    this.fading = Some((this.chain.replace(new), 0));
                    return;
                }
                // Neither slot ever goes without a chain of its own while
                // the other has none.
                Some(Offer::Empty) if this.chain.is_none() || other.chain.is_none() => {}
                Some(Offer::Empty) => {
                    this.fading = Some((this.chain.take(), 0));
                    return;
                }
            }
        }
    }

    /// Mixes the slots' outputs into `block`, and their delayed unprocessed
    /// signals into `dry`, as far as the heard slot has moved from A to B,
    /// each output through its matching gain, measuring the slots' loudness
    /// on the way; true if a move between slots ended.
    fn mix_slots(&mut self, block: &mut [f32]) -> bool {
        let n = block.len();
        let target = self.controls.slot() * self.fade_frames;
        let comparing = self.slots.iter().all(|s| s.chain.is_some());
        // Not through a fade, which would count what neither slot plays
        // once it ends.
        let measuring = comparing && self.slots.iter().all(|s| s.fading.is_none());
        let fade = self.fade_frames as f32;
        let [a, b] = &self.slots;
        let mut switched = false;
        for (f, (out, dry)) in block
            .chunks_exact_mut(2)
            .zip(self.dry[..n].chunks_exact_mut(2))
            .enumerate()
        {
            let at = 2 * f..2 * f + 2;
            let (a_out, b_out) = (&a.out[at.clone()], &b.out[at.clone()]);
            let (a_dry, b_dry) = (&a.dry[at.clone()], &b.dry[at]);
            let moving = self.heard != target;
            self.heard = if self.heard < target {
                self.heard + 1
            } else {
                self.heard.saturating_sub(usize::from(self.heard > target))
            };
            switched |= moving && self.heard == target;
            if comparing {
                let k = self.trim.weigh([a_out, b_out]);
                if measuring {
                    let silent = power(a_dry).max(power(b_dry)) <= SILENT;
                    self.trim.add(k, silent, self.level_frames);
                }
            }
            for (g, t) in self.trim.gains.iter_mut().zip(self.trim.targets) {
                *g += self.gain_coef * (t - *g);
            }
            let [ga, gb] = self.trim.gains.map(|g| g as f32);
            let m = self.heard as f32 / fade;
            if self.heard == 0 {
                out.iter_mut().zip(a_out).for_each(|(x, a)| *x = ga * a);
                dry.copy_from_slice(a_dry);
            } else if self.heard == self.fade_frames {
                out.iter_mut().zip(b_out).for_each(|(x, b)| *x = gb * b);
                dry.copy_from_slice(b_dry);
            } else {
                for i in 0..2 {
                    out[i] = (1.0 - m) * ga * a_out[i] + m * gb * b_out[i];
                    dry[i] = (1.0 - m) * a_dry[i] + m * b_dry[i];
                }
            }
        }
        self.trim
            .aim(comparing && self.controls.matching(), MAX_GAIN_DB);
        for (slot, g) in self.controls.trim.iter().zip(self.trim.gains) {
            slot.store(((20.0 * g.log10()) as f32).to_bits(), Relaxed);
        }
        switched
    }

    /// Moves each fade on by `frames` and hands back the chains whose fades
    /// ended; true if one that what is heard comes from ended.
    fn end_fades(&mut self, frames: usize) -> bool {
        let mut heard_changed = false;
        for s in 0..2 {
            let Some((_, done)) = &mut self.slots[s].fading else {
                continue;
            };
            *done += frames;
            if *done < self.fade_frames {
                continue;
            }
            if let Some((Some(old), _)) = self.slots[s].fading.take() {
                self.handoff.retire(s, old);
            }
            self.trim.restart();
            heard_changed |= self.heard_from(s);
        }
        heard_changed
    }

    /// Whether what is heard comes from slot `s`'s chains.
    fn heard_from(&self, s: usize) -> bool {
        let weighs = |s: usize| {
            if s == 0 {
                self.heard < self.fade_frames
            } else {
                self.heard > 0
            }
        };
        weighs(s) || (weighs(1 - s) && self.slots[1 - s].chain.is_none())
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
    use dps::loudness::integrated_lufs;
    use dps::profile::{CompressorParams, LimiterParams, StereoParams};
    use dps::{Processor, Profile};
    use std::f64::consts::PI;
    use std::mem;

    const RATE: u32 = 48_000;
    /// 30 ms at 48 kHz.
    const FADE: usize = 1440;

    /// Multiplies every sample by its gain and delays it by a latency,
    /// counting the blocks it runs on and how often it is dropped.
    struct Scale {
        gain: f32,
        drops: Arc<AtomicU64>,
        calls: Arc<AtomicU64>,
        /// The delayed samples, interleaved, the oldest at `at`.
        line: Vec<f32>,
        at: usize,
    }

    impl Scale {
        fn new(gain: f32, latency: usize, drops: &Arc<AtomicU64>) -> Self {
            Self {
                gain,
                drops: Arc::clone(drops),
                calls: Arc::default(),
                line: vec![0.0; 2 * latency],
                at: 0,
            }
        }
    }

    impl Chain for Scale {
        fn process(&mut self, block: &mut [f32]) {
            self.calls.fetch_add(1, Relaxed);
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
        /// How many blocks the first chain has run on.
        calls: Arc<AtomicU64>,
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
            let calls = Arc::clone(&chain.calls);
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
                calls,
            }
        }

        /// Posts a chain for `slot`; returns its count of blocks run.
        fn post_to(&self, slot: usize, gain: f32, latency: usize) -> Arc<AtomicU64> {
            let chain = Scale::new(gain, latency, &self.drops);
            let calls = Arc::clone(&chain.calls);
            self.handoff.post(slot, chain);
            calls
        }

        fn post(&self, gain: f32) {
            self.post_delayed(gain, 0);
        }

        fn post_delayed(&self, gain: f32, latency: usize) {
            self.handoff.post(0, Scale::new(gain, latency, &self.drops));
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
        let handoff = Arc::clone(&rig.handoff);
        let retired = || handoff.lanes[0].retired.load(Relaxed);
        assert!(retired().is_null(), "retired early");
        rig.run(&[0.1; 1]);
        assert!(!retired().is_null());
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
                handoff.post(0, Processor::new(next, RATE, 2));
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
                handoff.post(0, Processor::new(&default, RATE, 2));
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

    fn level_db(x: &[f32]) -> f64 {
        10.0 * mean_square(x).log10()
    }

    #[test]
    fn both_slots_run_while_b_has_a_chain_and_one_runs_when_not() {
        let mut rig = Rig::new(1.0);
        // Unmatched, so what B plays below is A's chain's output as it is.
        rig.controls.set_matching(false);
        rig.run(&[0.1; 10 * 256]);
        assert_eq!(rig.calls.load(Relaxed), 10);
        // B's chain runs from the block that takes it, unheard.
        let b = rig.post_to(1, 0.5, 0);
        rig.run(&[0.1; 10 * 256]);
        assert_eq!((rig.calls.load(Relaxed), b.load(Relaxed)), (20, 10));
        // Asked to play A's chain, B fades to it and its own stops.
        rig.handoff.clear(1);
        rig.run(&[0.1; 10 * 256]);
        // Six blocks of 256 frames cover the 1440 of the fade.
        assert_eq!((rig.calls.load(Relaxed), b.load(Relaxed)), (30, 16));
        rig.run(&[0.1; 10 * 256]);
        assert_eq!((rig.calls.load(Relaxed), b.load(Relaxed)), (40, 16));
        // Without a chain of its own, B plays A's, the same samples.
        rig.controls.set_slot(1);
        let out = rig.run(&[0.1; 8 * 256]);
        assert!(out.iter().all(|&x| (x - 0.1).abs() < 1e-7), "{out:?}");
        assert!(out[FADE..].iter().all(|&x| x == 0.1), "{out:?}");
        assert_eq!(rig.calls.load(Relaxed), 48);
    }

    #[test]
    fn switching_slots_crossfades_without_a_click() {
        // A plays the sine, B inverts it: the worst case, at the same level
        // so matching leaves both at unity. The switch to B lands where the
        // sine is at 0.866 of its peak, then back to A mid-fade.
        let (freq, amp) = (1000.0, 0.5);
        let input = sine(freq, amp, 0, 3 * 48_000);
        let mut rig = Rig::new(1.0);
        rig.post_to(1, -1.0, 0);
        let at = 48_000 + 64;
        let mut out = rig.run(&input[..at]);
        rig.controls.set_slot(1);
        out.extend(rig.run(&input[at..at + 3 * FADE]));
        let back = at + 3 * FADE;
        rig.controls.set_slot(0);
        out.extend(rig.run(&input[back..back + 512]));
        rig.controls.set_slot(1);
        out.extend(rig.run(&input[back + 512..]));
        let bound = sine_step(freq, amp) + (2.0 * amp / 1440.0) as f32 + 1e-6;
        assert!(max_step(&out) <= bound, "{} > {bound}", max_step(&out));
        // Swapping at once would jump by twice the sample.
        assert!(2.0 * input[at].abs() > 10.0 * bound);
        // Matching weighs the two the same, to a few parts in a million.
        let near = |o: f32, i: f32| (o - i).abs() < 1e-6;
        assert!(out[..at].iter().zip(&input).all(|(o, i)| near(*o, *i)));
        let settled = back + 512 + FADE;
        assert!(
            out[settled..]
                .iter()
                .zip(&input[settled..])
                .all(|(o, i)| near(*o, -i))
        );
        assert!(rig.controls.trim_db().iter().all(|t| t.abs() < 1e-4));
    }

    #[test]
    fn matching_turns_the_louder_slot_down_to_the_quieter_one() {
        // B plays 6.02 dB under A: A is turned down to B's level, never B
        // up, settled within a second and holding through silence.
        let (freq, amp) = (440.0, 0.4);
        let mut rig = Rig::new(1.0);
        rig.post_to(1, 0.5, 0);
        let mut at = 0;
        let mut trims = Vec::new();
        // What plays and what went in.
        let mut play = |rig: &mut Rig, seconds: f64, amp: f64| {
            let (mut out, mut input) = (Vec::new(), Vec::new());
            for _ in 0..(seconds * 48_000.0 / 256.0) as usize {
                let block = sine(freq, amp, at, 256);
                out.extend(rig.run(&block));
                input.extend(block);
                at += 256;
                trims.push(rig.controls.trim_db());
            }
            (out, input)
        };
        let close = |got: f32, want: f64, tol: f64| (f64::from(got) - want).abs() < tol;
        play(&mut rig, 1.0, amp);
        let [a, b] = rig.controls.trim_db();
        assert!(close(a, -6.02, 0.1) && b == 0.0, "{a} {b}");
        play(&mut rig, 0.5, amp);
        let [a, b] = rig.controls.trim_db();
        assert!(close(a, -6.02, 0.01) && b == 0.0, "{a} {b}");
        // What plays from A is at B's level: half the input.
        let (out, input) = play(&mut rig, 0.5, amp);
        let error = level_db(&out) - level_db(&input) + 6.02;
        assert!(error.abs() < 0.05, "{error} dB");
        // B is heard at its own level.
        rig.controls.set_slot(1);
        let (out, input) = play(&mut rig, 0.5, amp);
        let error = level_db(&out[FADE..]) - level_db(&input[FADE..]) + 6.02;
        assert!(error.abs() < 0.05, "{error} dB");
        // A new A 12 dB down makes B the louder by 6 dB: within a second B
        // is the one turned down.
        rig.post_to(0, 0.25, 0);
        play(&mut rig, 1.0, amp);
        let [a, b] = rig.controls.trim_db();
        assert!(a.abs() < 0.1 && close(b, -6.02, 0.1), "{a} {b}");
        play(&mut rig, 1.0, amp);
        let [a, b] = rig.controls.trim_db();
        assert!(a.abs() < 0.01 && close(b, -6.02, 0.02), "{a} {b}");
        // A minute of silence leaves it where it was, and quiet material
        // still counts.
        play(&mut rig, 60.0, 0.0);
        let [a, b] = rig.controls.trim_db();
        assert!(a.abs() < 0.01 && close(b, -6.02, 0.02), "{a} {b}");
        play(&mut rig, 1.0, 1e-3);
        let [a, b] = rig.controls.trim_db();
        assert!(a.abs() < 0.01 && close(b, -6.02, 0.02), "{a} {b}");
        // Turned off, both glide back to unity.
        rig.controls.set_matching(false);
        play(&mut rig, 2.0, amp);
        let [a, b] = rig.controls.trim_db();
        assert!(a.abs() < 1e-3 && b.abs() < 1e-3, "{a} {b}");
        assert!(
            trims.iter().flatten().all(|&t| t <= 0.0),
            "a gain above unity"
        );
    }

    /// Every stage at unity and the limiter off.
    fn flat() -> Profile {
        let unity = CompressorParams {
            ratio: 1.0,
            ..CompressorParams::default()
        };
        Profile {
            output_gain_db: 0.0,
            low_shelf_db: 0.0,
            high_shelf_db: 0.0,
            compressor1: unity,
            compressor2: unity,
            limiter: LimiterParams {
                enabled: false,
                ..LimiterParams::default()
            },
            ..Profile::default()
        }
    }

    #[test]
    fn matching_weighs_loudness_as_bs_1770_does() {
        // A lifts 30 Hz by 12 dB. Over a 30 Hz and a 1 kHz tone, that puts
        // A's power far above B's, but BS.1770's K-weighting all but
        // ignores 30 Hz: A is turned down by what a loudness meter says it
        // is louder, not by its power.
        let mut boosted = flat();
        boosted.eq.gains_db[0] = 12.0;
        let handoff = Arc::new(Handoff::new());
        let controls = Arc::new(Controls::default());
        let chain = Box::new(Processor::new(&boosted, RATE, 2));
        let mut mixer = Mixer::new(
            chain,
            RATE,
            512,
            Arc::clone(&handoff),
            Arc::clone(&controls),
        );
        handoff.post(1, Processor::new(&flat(), RATE, 2));
        let tone = |i: usize| {
            let t = i as f64 / f64::from(RATE);
            (0.2 * (2.0 * PI * 30.0 * t).sin() + 0.2 * (2.0 * PI * 1000.0 * t).sin()) as f32
        };
        let input: Vec<f32> = (0..4 * 48_000).flat_map(|i| [tone(i), tone(i)]).collect();
        for block in input.clone().chunks_mut(1024) {
            mixer.process(block);
        }
        let through = |profile: &Profile| {
            let mut out = input[2 * 48_000..].to_vec();
            Processor::new(profile, RATE, 2).process(&mut out);
            out
        };
        let (a, b) = (through(&boosted), through(&flat()));
        let louder = integrated_lufs(&a, 2, RATE) - integrated_lufs(&b, 2, RATE);
        let stronger = level_db(&a) - level_db(&b);
        let [trim, other] = controls.trim_db();
        eprintln!("A louder by {louder:.2} LU, stronger by {stronger:.2} dB, trimmed {trim:.2} dB");
        assert!((f64::from(trim) + louder).abs() < 0.1, "{trim} vs {louder}");
        assert_eq!(other, 0.0);
        assert!(stronger > louder + 3.0, "{stronger} vs {louder}");
    }

    #[test]
    fn editing_one_slot_leaves_the_other_chain_running() {
        let mut rig = Rig::new(1.0);
        let b = rig.post_to(1, 0.5, 0);
        rig.run(&[0.1; 8 * 256]);
        let a = rig.post_to(0, 0.8, 0);
        rig.run(&[0.1; 20 * 256]);
        // The same B chain ran on every block, and only A's old chain, which
        // ran through the fade, was handed back.
        assert_eq!(b.load(Relaxed), 28);
        assert_eq!(a.load(Relaxed), 20);
        assert_eq!(rig.calls.load(Relaxed), 8 + 6);
        assert_eq!(rig.drops.load(Relaxed), 0);
        rig.handoff.collect();
        assert_eq!(rig.drops.load(Relaxed), 1);
        assert!(rig.handoff.lanes[1].retired.load(Relaxed).is_null());
        // Another B, posted while A fades, waits for the fade to end and for
        // A's old chain to be collected.
        let a2 = rig.post_to(0, 0.6, 0);
        let b2 = rig.post_to(1, 0.4, 0);
        rig.run(&[0.1; 256]);
        assert_eq!((a2.load(Relaxed), b2.load(Relaxed)), (1, 0));
        rig.run(&[0.1; 6 * 256]);
        assert_eq!(b2.load(Relaxed), 0);
        rig.handoff.collect();
        rig.run(&[0.1; 256]);
        assert_eq!(b2.load(Relaxed), 1);
    }

    #[test]
    fn an_offer_to_play_the_other_slot_waits_while_that_slot_cannot_take_its_chain() {
        let mut rig = Rig::new(1.0);
        rig.post_to(1, 0.5, 0);
        rig.run(&[0.1; 8 * 256]);
        rig.handoff.clear(1);
        rig.run(&[0.1; 8 * 256]);
        // B's old chain is handed back but not collected yet, so B cannot
        // take the chain posted now; A is told to play B's.
        assert!(!rig.handoff.lanes[1].retired.load(Relaxed).is_null());
        rig.post_to(1, 0.25, 0);
        rig.handoff.clear(0);
        rig.run(&[0.1; 256]);
        rig.handoff.collect();
        rig.run(&[0.1; 20 * 256]);
        rig.handoff.collect();
        // A gave its chain up for B's: it runs no more and was handed back.
        let calls = rig.calls.load(Relaxed);
        rig.run(&[0.1; 4 * 256]);
        assert_eq!(rig.calls.load(Relaxed), calls);
        assert_eq!(rig.drops.load(Relaxed), 2);
    }

    #[test]
    fn chains_leave_through_the_handoff_and_never_free_on_the_callback() {
        let mut rig = Rig::new(1.0);
        rig.post_to(1, 0.5, 0);
        rig.run(&[0.1; 8 * 256]);
        rig.handoff.clear(1);
        rig.run(&[0.1; 8 * 256]);
        assert_eq!(rig.drops.load(Relaxed), 0, "freed on the callback");
        assert!(!rig.handoff.lanes[1].retired.load(Relaxed).is_null());
        rig.handoff.collect();
        assert_eq!(rig.drops.load(Relaxed), 1);
        // A slot gets a chain again; the other is replaced; both go back
        // only through the handoff.
        rig.post_to(1, 0.25, 0);
        rig.run(&[0.1; 8 * 256]);
        rig.post_to(0, 0.75, 0);
        rig.run(&[0.1; 8 * 256]);
        assert_eq!(rig.drops.load(Relaxed), 1);
        rig.handoff.collect();
        assert_eq!(rig.drops.load(Relaxed), 2);
        let drops = Arc::clone(&rig.drops);
        drop(rig);
        assert_eq!(drops.load(Relaxed), 4);
    }

    #[test]
    fn bypass_on_slot_b_matches_slot_b() {
        // A plays at unity and B at half, heard through bypass: the dry
        // signal plays at B's level, with matching on or off.
        let (freq, amp) = (440.0, 0.4);
        for matching in [true, false] {
            let mut rig = Rig::new(1.0);
            rig.controls.set_matching(matching);
            rig.post_to(1, 0.5, 0);
            rig.controls.set_slot(1);
            rig.run(&sine(freq, amp, 0, 4 * 48_000));
            assert!((rig.controls.dry_gain() - 0.5).abs() < 1e-3, "{matching}");
            rig.controls.bypass.store(true, Relaxed);
            let input = sine(freq, amp, 4 * 48_000, 48_000);
            let out = rig.run(&input);
            let g = rig.controls.dry_gain();
            for (o, i) in out[FADE..].iter().zip(&input[FADE..]) {
                assert!((o - g * i).abs() <= 1e-6, "{o} vs {}", g * i);
            }
        }
    }

    #[test]
    fn the_unprocessed_signal_is_as_late_as_the_heard_slot() {
        // In bypass at unity, A plays with no latency and B 300 frames
        // late: switching to B moves the dry signal over the fade rather
        // than jumping back 300 frames, then plays it that late, and back.
        let (freq, amp, latency) = (440.0, 0.5, 300);
        let input = sine(freq, amp, 0, 6 * 48_000);
        let mut rig = Rig::new(1.0);
        // Unmatched, so the dry signal is matched to unity alone: a pure
        // tone measured 300 frames apart reads a few thousandths of a dB
        // different until its averages are long.
        rig.controls.set_matching(false);
        rig.post_to(1, 1.0, latency);
        rig.controls.bypass.store(true, Relaxed);
        let at = 2 * 48_000;
        let mut out = rig.run(&input[..at]);
        rig.controls.set_slot(1);
        out.extend(rig.run(&input[at..2 * at]));
        rig.controls.set_slot(0);
        out.extend(rig.run(&input[2 * at..]));
        let bound = sine_step(freq, amp) + (2.0 * amp / 1440.0) as f32 + 1e-4;
        assert!(max_step(&out) <= bound, "{} > {bound}", max_step(&out));
        assert!((input[at] - input[at - latency]).abs() > 10.0 * bound);
        for (o, i) in out[at - 512..at].iter().zip(&input[at - 512..]) {
            assert!((o - i).abs() <= 1e-4, "{o} vs {i}");
        }
        for (o, i) in out[at + FADE..2 * at]
            .iter()
            .zip(&input[at + FADE - latency..])
        {
            assert!((o - i).abs() <= 1e-4, "{o} vs {i}");
        }
        for (o, i) in out[2 * at + FADE..].iter().zip(&input[2 * at + FADE..]) {
            assert!((o - i).abs() <= 1e-4, "{o} vs {i}");
        }
    }

    #[test]
    fn a_broken_chain_not_heard_still_silences() {
        let mut rig = Rig::new(0.5);
        rig.run(&[0.1; 4096]);
        rig.post_to(1, f32::NAN, 0);
        let out = rig.run(&[0.1; 256]);
        assert!(out.iter().any(|x| x.is_nan()));
        assert_eq!(rig.controls.slot(), 0);
    }

    #[test]
    fn a_new_mixer_plays_its_chain_in_the_slot_heard() {
        // The session starts while B is heard: its chain plays B, at once,
        // and A, posted next, runs beside it.
        let controls = Arc::new(Controls::default());
        controls.set_slot(1);
        let mut rig = Rig::with(0.5, Arc::clone(&controls));
        assert!(rig.run(&[0.2; 512]).iter().all(|&x| x == 0.1));
        let a = rig.post_to(0, 1.0, 0);
        rig.controls.set_matching(false);
        assert!(rig.run(&[0.2; 512]).iter().all(|&x| x == 0.1));
        assert_eq!(a.load(Relaxed), 2);
    }

    #[test]
    fn a_heard_slot_told_to_play_the_other_ones_chain_fades_to_it() {
        // B inverts and is heard; asked to play A's chain, it fades from -x
        // to x over 30 ms, with no step larger than the fade's own.
        let (freq, amp) = (1000.0, 0.5);
        let input = sine(freq, amp, 0, 48_000);
        let mut rig = Rig::new(1.0);
        rig.post_to(1, -1.0, 0);
        rig.controls.set_slot(1);
        let at = 8192;
        let mut out = rig.run(&input[..at]);
        rig.handoff.clear(1);
        out.extend(rig.run(&input[at..]));
        let bound = sine_step(freq, amp) + (2.0 * amp / 1440.0) as f32 + 1e-6;
        assert!(max_step(&out) <= bound, "{} > {bound}", max_step(&out));
        assert!(
            out[at - 512..at]
                .iter()
                .zip(&input[at - 512..])
                .all(|(o, i)| *o == -i)
        );
        assert!(
            out[at + FADE..]
                .iter()
                .zip(&input[at + FADE..])
                .all(|(o, i)| o == i)
        );
    }

    #[test]
    fn the_chain_hears_the_volume_set_on_the_controls() {
        struct Heard(Arc<AtomicU32>);
        impl Chain for Heard {
            fn process(&mut self, _: &mut [f32]) {}
            fn set_volume_db(&mut self, volume_db: Option<f32>) {
                self.0
                    .store(volume_db.unwrap_or(f32::NAN).to_bits(), Relaxed);
            }
        }
        let heard = Arc::new(AtomicU32::new(0));
        let controls = Arc::new(Controls::default());
        assert_eq!(controls.volume_db(), None);
        let mut mixer = Mixer::new(
            Box::new(Heard(Arc::clone(&heard))),
            RATE,
            256,
            Arc::new(Handoff::new()),
            Arc::clone(&controls),
        );
        let seen = || Some(f32::from_bits(heard.load(Relaxed))).filter(|v| !v.is_nan());
        mixer.process(&mut [0.0; 64]);
        assert_eq!(seen(), None);
        controls.set_volume_db(Some(-18.5));
        mixer.process(&mut [0.0; 64]);
        assert_eq!(seen(), Some(-18.5));
        controls.set_volume_db(None);
        mixer.process(&mut [0.0; 64]);
        assert_eq!(seen(), None);
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
