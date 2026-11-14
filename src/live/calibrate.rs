//! `dps calibrate`: plays the test tones of `dps::calibrate` on the default
//! output device and records them with the default input device, one IOProc
//! on each. The devices may run at different rates on different clocks: the
//! analysis needs only to know how far the recording had got when the
//! playback started, and finds the tones after that by itself.
//!
//! The callbacks only copy. The whole playback is rendered and the whole
//! recording allocated before IO starts, and positions reach the main thread
//! through atomics.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::io::{self, IsTerminal, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::thread;
use std::time::{Duration, Instant};

use dps::calibrate::{self, Calibration, Plan};
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};

use super::hal::{self, ObjectId};
use super::render::{self, Buffers, BuffersMut};
use super::{
    HalBuffers, NoSuspend, POLL, STOP, Target, check_float32, install_signal_handlers,
    notify_if_slow, tui,
};
use crate::config::{Dirs, Source};
use crate::service::{LiveLock, Paths};
use crate::{import, write_stdout};

/// How long the microphone records before the playback starts.
const PREROLL: Duration = Duration::from_millis(100);
/// How long a device may take to start.
const START_LIMIT: Duration = Duration::from_secs(5);
/// A device that delivers nothing for this long has stopped.
const STALL_LIMIT: Duration = Duration::from_secs(2);
/// `Status::anchor` before the player's first callback.
const NOT_YET: usize = usize::MAX;
const WAITING_FOR_PERMISSION: &str = "waiting for permission to use the microphone; answer the \
     macOS prompt, or press Ctrl-C twice to quit";
const NO_INPUT: &str = "there is no default input device, and dps calibrate records with a \
     microphone: connect one (many desktop Macs have none built in) and choose it in System \
     Settings > Sound > Input";

/// Measures the default output device with the default input device and,
/// unless `dry_run`, writes the correction into `profile`, or else the
/// settings file. The test tones play at `level_db` dBFS RMS.
pub fn run(
    profile: Option<PathBuf>,
    level_db: f64,
    dry_run: bool,
    dirs: &Dirs,
) -> Result<(), String> {
    let _lock = lock(dirs)?;
    // Loaded now, so that a profile that cannot be read fails before the
    // tones play.
    let source = if dry_run {
        None
    } else {
        let source = Source::new(profile, None, dirs)?;
        source.load()?;
        Some(source)
    };
    let output = Target::current()?;
    let input = Input::current()?;
    check_streams(&output.name, output.device, Scope::Output)?;
    check_streams(&input.name, input.device, Scope::Input)?;
    let plan = Plan::new(
        level_db,
        usable_rate(&output.name, output.sample_rate)?,
        usable_rate(&input.name, input.sample_rate)?,
    );
    say(&format!(
        "playing test tones at {level_db} dBFS on {} and recording them with {}, for about \
         {} s; put the microphone where you listen, keep the room quiet, and press Ctrl-C to stop",
        output.name,
        input.name,
        plan.recording_seconds().ceil()
    ));
    install_signal_handlers()?;
    let _no_suspend = NoSuspend::new()?;
    let (recording, anchor) = measure(&plan, &output, &input)?;
    let calibration = calibrate::analyse(&plan, &recording, anchor).map_err(|e| e.to_string())?;
    report(&calibration, source.as_ref())
}

/// The live lock, so that no live run starts while the tones play.
fn lock(dirs: &Dirs) -> Result<Option<LiveLock>, String> {
    // Without a home there can be no background run to clash with.
    let Ok(paths) = Paths::from_dirs(dirs) else {
        return Ok(None);
    };
    match LiveLock::take(&paths.lock, false)? {
        Ok(lock) => Ok(Some(lock)),
        Err(holder) => {
            let fix = if holder.background {
                "run `dps stop` first"
            } else {
                "quit it first"
            };
            Err(format!(
                "{}, and its system audio tap would mute or process the test tones; {fix}",
                holder.describe()
            ))
        }
    }
}

fn report(calibration: &Calibration, source: Option<&Source>) -> Result<(), String> {
    write_stdout(&format!(
        "found the test tones {:.0} ms after they were played\n\n{}\n\
         this corrects the speakers as this microphone hears them where it is now\n",
        calibration.latency_s * 1000.0,
        calibration.table()
    ))?;
    let Some(source) = source else {
        return write_stdout("dry run: nothing was written\n");
    };
    let (Source::File(path) | Source::Settings(path)) = source else {
        unreachable!("calibrate takes no preset");
    };
    let mut target = source.load()?;
    target.correction = calibration.correction.clone();
    import::rewrite(path, &target)?;
    write_stdout(&format!(
        "wrote it into [correction] of {}, turned on; the file was rewritten, so comments in it \
         are not kept; the next dps live or dps start uses it, and `enabled = false` under \
         [correction], or the tuner, turns it off\n",
        source.name()
    ))
}

/// The input device to record with.
struct Input {
    device: ObjectId,
    name: String,
    sample_rate: f64,
    /// It has output channels too, which its callback must fill.
    outputs: bool,
}

impl Input {
    fn current() -> Result<Self, String> {
        let device = hal::default_input_device()?.ok_or(NO_INPUT)?;
        let name = hal::name(device)?;
        // A loopback driver records exact silence, which would pass for a
        // missing microphone permission, and a virtual noise suppressor
        // would undo the measurement.
        if hal::is_virtual(device)? {
            return Err(format!(
                "the default input device, {name}, is a virtual device rather than a \
                 microphone; choose the microphone in System Settings > Sound > Input"
            ));
        }
        let outputs = hal::streams(device, hal::kAudioObjectPropertyScopeOutput)?;
        Ok(Self {
            device,
            name,
            sample_rate: hal::nominal_sample_rate(device)?,
            outputs: outputs.iter().any(|s| s.format.mChannelsPerFrame > 0),
        })
    }
}

#[derive(Clone, Copy)]
enum Scope {
    Input,
    Output,
}

/// The callbacks copy raw, packed 32-bit floats, so the streams in `scope`
/// must hold them, and at least one channel.
fn check_streams(name: &str, device: ObjectId, scope: Scope) -> Result<(), String> {
    let (selector, what) = match scope {
        Scope::Input => (hal::kAudioObjectPropertyScopeInput, "input"),
        Scope::Output => (hal::kAudioObjectPropertyScopeOutput, "output"),
    };
    let streams = hal::streams(device, selector)?;
    if streams.iter().all(|s| s.format.mChannelsPerFrame == 0) {
        return Err(format!("{name} has no {what} channels"));
    }
    for stream in &streams {
        check_float32(&stream.format).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

fn usable_rate(name: &str, rate: f64) -> Result<f64, String> {
    if (8_000.0..=768_000.0).contains(&rate) {
        Ok(rate)
    } else {
        Err(format!(
            "{name} reports an unusable sample rate of {rate} Hz"
        ))
    }
}

fn frames(duration: Duration, rate: f64) -> usize {
    (duration.as_secs_f64() * rate).round() as usize
}

/// Writes `message` as a line on stderr, which may have been closed.
fn say(message: &str) {
    let _ = writeln!(io::stderr(), "dps calibrate: {}", tui::printable(message));
}

/// Plays the tones and records them; the recording and its anchor (see
/// `calibrate::analyse`).
fn measure(plan: &Plan, output: &Target, input: &Input) -> Result<(Vec<f32>, usize), String> {
    let playback = plan.render(output.sample_rate);
    let playback_frames = playback.len();
    // The anchor is at most what records while the output device starts.
    let latest_anchor = frames(START_LIMIT, input.sample_rate);
    let tape = Arc::new(Tape::new(latest_anchor + plan.recording_frames()));
    let status = Arc::new(Status {
        played: AtomicUsize::new(0),
        anchor: AtomicUsize::new(NOT_YET),
        panicked: AtomicBool::new(false),
    });

    let recorder = Box::new(Recorder {
        tape: Arc::clone(&tape),
        status: Arc::clone(&status),
        outputs: input.outputs,
        failed: false,
    });
    let on_slow = || say(WAITING_FOR_PERMISSION);
    let _recording = notify_if_slow(on_slow, || {
        // SAFETY: `record` expects a `Recorder`.
        unsafe { hal::IoProc::start(input.device, Some(record), recorder) }
    })?;
    let started = Instant::now();
    while tape.len() < frames(PREROLL, input.sample_rate) {
        thread::sleep(POLL);
        if STOP.load(Relaxed) {
            return Err("stopped; nothing was written".into());
        }
        if started.elapsed() > START_LIMIT {
            return Err(format!(
                "{} delivered no audio within {} s",
                input.name,
                START_LIMIT.as_secs()
            ));
        }
    }

    let player = Box::new(Player {
        playback,
        position: 0,
        limit: plan.peak(),
        tape: Arc::clone(&tape),
        status: Arc::clone(&status),
        started: false,
        failed: false,
    });
    // SAFETY: `play` expects a `Player`.
    let _playing = unsafe { hal::IoProc::start(output.device, Some(play), player)? };

    let mut progress = Progress::new(io::stderr().is_terminal());
    let now = Instant::now();
    let (mut played_stall, mut recorded_stall) = (Stall::new(now), Stall::new(now));
    loop {
        thread::sleep(POLL);
        if STOP.load(Relaxed) {
            progress.finish();
            return Err("stopped; nothing was written".into());
        }
        if status.panicked.load(Relaxed) {
            progress.finish();
            return Err("internal error in an audio callback; nothing was written".into());
        }
        let anchor = status.anchor.load(Acquire);
        let played = status.played.load(Acquire);
        let recorded = tape.len();
        let now = Instant::now();
        progress.show(plan, played as f64 / output.sample_rate);
        let failure = if anchor == NOT_YET && recorded >= latest_anchor
            || anchor != NOT_YET && anchor > latest_anchor
        {
            Some(format!(
                "{} took more than {} s to start playing",
                output.name,
                START_LIMIT.as_secs()
            ))
        } else if anchor != NOT_YET && played < playback_frames && played_stall.check(played, now) {
            Some(format!("{} stopped playing", output.name))
        } else if recorded_stall.check(recorded, now) {
            Some(format!("{} stopped recording", input.name))
        } else {
            None
        };
        if let Some(failure) = failure {
            progress.finish();
            return Err(format!("{failure}; nothing was written"));
        }
        if anchor != NOT_YET
            && played >= playback_frames
            && recorded >= anchor + plan.recording_frames()
        {
            progress.finish();
            return Ok((tape.recorded().to_vec(), anchor));
        }
    }
}

/// Shows on stderr what is being measured: one line rewritten in place on
/// a terminal, else a line for each step.
struct Progress {
    terminal: bool,
    shown: String,
}

impl Progress {
    fn new(terminal: bool) -> Self {
        Self {
            terminal,
            shown: String::new(),
        }
    }

    fn show(&mut self, plan: &Plan, seconds: f64) {
        let step = match plan.playing(seconds) {
            Some((slot, centre)) => {
                format!("tone {} of {}, {centre} Hz", slot + 1, calibrate::BANDS)
            }
            None if seconds < plan.seconds() => "measuring the room's noise".into(),
            None => "finishing the recording".into(),
        };
        if step == self.shown {
            return;
        }
        let _ = if self.terminal {
            write!(io::stderr(), "\r\x1b[Kdps calibrate: {step}")
        } else {
            writeln!(io::stderr(), "dps calibrate: {step}")
        };
        self.shown = step;
    }

    /// Ends the line rewritten in place.
    fn finish(&mut self) {
        if self.terminal && !self.shown.is_empty() {
            let _ = writeln!(io::stderr());
            self.shown.clear();
        }
    }
}

/// Notices a count that stops growing.
struct Stall {
    last: usize,
    since: Instant,
}

impl Stall {
    fn new(now: Instant) -> Self {
        Self {
            last: 0,
            since: now,
        }
    }

    /// Whether `count` has stayed the same for `STALL_LIMIT`.
    fn check(&mut self, count: usize, now: Instant) -> bool {
        if count != self.last {
            self.last = count;
            self.since = now;
        }
        now.saturating_duration_since(self.since) >= STALL_LIMIT
    }
}

/// The recording: the recorder's callback appends to it and the main thread
/// reads it. The callback writes only past `len` and the main thread reads
/// only before it, so they never touch the same sample.
struct Tape {
    samples: Box<[UnsafeCell<f32>]>,
    len: AtomicUsize,
}

// SAFETY: see the type's documentation; only one thread calls `append`.
unsafe impl Sync for Tape {}

impl Tape {
    fn new(capacity: usize) -> Self {
        Self {
            samples: (0..capacity).map(|_| UnsafeCell::new(0.0)).collect(),
            len: AtomicUsize::new(0),
        }
    }

    fn len(&self) -> usize {
        self.len.load(Acquire)
    }

    /// Lets `write` fill the start of the space left and keeps as much as
    /// it says it wrote. Only one thread may call this. No allocation.
    fn append(&self, write: impl FnOnce(&mut [f32]) -> usize) {
        let len = self.len.load(Relaxed);
        let free = self.samples.len() - len;
        // SAFETY: `UnsafeCell<f32>` has the layout of `f32`, and the
        // samples from `len` on are this thread's alone.
        let space = unsafe {
            slice::from_raw_parts_mut(UnsafeCell::raw_get(self.samples.as_ptr().add(len)), free)
        };
        let written = write(space).min(free);
        self.len.store(len + written, Release);
    }

    fn recorded(&self) -> &[f32] {
        let len = self.len();
        // SAFETY: as in `append`; `append` no longer writes these.
        unsafe { slice::from_raw_parts(UnsafeCell::raw_get(self.samples.as_ptr()), len) }
    }
}

/// What the callbacks tell the main thread.
struct Status {
    /// Frames the output device has asked the player for.
    played: AtomicUsize,
    /// The frames recorded when the player's first callback came, or
    /// `NOT_YET`.
    anchor: AtomicUsize,
    panicked: AtomicBool,
}

/// The output callback's state.
struct Player {
    playback: Vec<f32>,
    position: usize,
    /// No sample leaves louder than this, whatever else goes wrong.
    limit: f32,
    tape: Arc<Tape>,
    status: Arc<Status>,
    started: bool,
    /// Set after a panic; the output stays silent.
    failed: bool,
}

impl Player {
    fn render(&mut self, output: &mut impl BuffersMut) {
        if !self.started {
            self.started = true;
            self.status.anchor.store(self.tape.len(), Release);
        }
        if self.failed {
            render::silence(output);
            return;
        }
        self.position += fill(output, &self.playback, self.position, self.limit);
        self.status.played.store(self.position, Release);
    }
}

/// The input callback's state.
struct Recorder {
    tape: Arc<Tape>,
    status: Arc<Status>,
    outputs: bool,
    /// Set after a panic; nothing more is recorded.
    failed: bool,
}

impl Recorder {
    fn render(&mut self, input: &impl Buffers, output: Option<&mut impl BuffersMut>) {
        if let Some(output) = output {
            render::silence(output);
        }
        if !self.failed {
            self.tape.append(|space| capture(input, space));
        }
    }
}

/// Writes `playback` from frame `from` on to every channel of `output`,
/// each sample held within ±`limit`, and silence once it runs out. Returns
/// the frames that took: as many as the shortest output buffer holds.
fn fill(output: &mut impl BuffersMut, playback: &[f32], from: usize, limit: f32) -> usize {
    let frames = (0..output.count())
        .filter(|&i| output.channels(i) > 0)
        .filter_map(|i| Some(output.samples(i)?.len() / output.channels(i)))
        .min()
        .unwrap_or(0);
    render::silence(output);
    let rest = playback.get(from..).unwrap_or_default();
    let source = &rest[..frames.min(rest.len())];
    for i in 0..output.count() {
        let channels = output.channels(i);
        if channels == 0 {
            continue;
        }
        let Some(samples) = output.samples_mut(i) else {
            continue;
        };
        for (frame, &x) in samples.chunks_exact_mut(channels).zip(source) {
            frame.fill(x.clamp(-limit, limit));
        }
    }
    frames
}

/// Copies the first input channel into the start of `space`, a non-finite
/// sample as 0, and returns the frames copied.
fn capture(input: &impl Buffers, space: &mut [f32]) -> usize {
    let Some(i) = (0..input.count()).find(|&i| input.channels(i) > 0) else {
        return 0;
    };
    let Some(samples) = input.samples(i) else {
        return 0;
    };
    let mut copied = 0;
    for (to, frame) in space
        .iter_mut()
        .zip(samples.chunks_exact(input.channels(i)))
    {
        *to = if frame[0].is_finite() { frame[0] } else { 0.0 };
        copied += 1;
    }
    copied
}

/// The output device's IOProc. Runs on the HAL's real-time thread: no
/// allocation, locks or I/O here or in anything it calls.
unsafe extern "C-unwind" fn play(
    _device: ObjectId,
    _now: NonNull<AudioTimeStamp>,
    _input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the run's `Player`, alive while IO runs and
    // touched by nothing else; the list is valid for this call.
    let (player, mut output) = unsafe { (&mut *client.cast::<Player>(), HalBuffers::new(output)) };
    if panic::catch_unwind(AssertUnwindSafe(|| player.render(&mut output))).is_err() {
        player.failed = true;
        player.status.panicked.store(true, Relaxed);
        render::silence(&mut output);
    }
    0
}

/// The input device's IOProc, under the same rules as `play`.
unsafe extern "C-unwind" fn record(
    _device: ObjectId,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the run's `Recorder`, alive while IO runs and
    // touched by nothing else; the lists are valid for this call. The
    // output list is left alone unless the device has outputs.
    let (recorder, input) = unsafe { (&mut *client.cast::<Recorder>(), HalBuffers::new(input)) };
    // SAFETY: as above.
    let mut output = recorder.outputs.then(|| unsafe { HalBuffers::new(output) });
    if panic::catch_unwind(AssertUnwindSafe(|| {
        recorder.render(&input, output.as_mut())
    }))
    .is_err()
    {
        recorder.failed = true;
        recorder.status.panicked.store(true, Relaxed);
        if let Some(output) = &mut output {
            render::silence(output);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn fill_plays_every_channel_and_holds_the_limit() {
        let playback = [0.1, -0.2, 0.9, -0.9, 0.3];
        // Interleaved stereo, and planar with a disabled stream.
        let mut stereo = List(vec![(2, Some(vec![7.0; 6]))]);
        assert_eq!(fill(&mut stereo, &playback, 1, 0.5), 3);
        assert_eq!(
            stereo.0[0].1.as_deref().unwrap(),
            [-0.2, -0.2, 0.5, 0.5, -0.5, -0.5]
        );
        let mut planar = List(vec![
            (1, Some(vec![7.0; 4])),
            (1, None),
            (1, Some(vec![7.0; 4])),
        ]);
        assert_eq!(fill(&mut planar, &playback, 0, 1.0), 4);
        for i in [0, 2] {
            assert_eq!(planar.0[i].1.as_deref().unwrap(), [0.1, -0.2, 0.9, -0.9]);
        }
    }

    #[test]
    fn fill_plays_silence_past_the_end() {
        let playback = [0.1, 0.2, 0.3];
        let mut out = List(vec![(2, Some(vec![7.0; 8]))]);
        assert_eq!(fill(&mut out, &playback, 2, 1.0), 4);
        assert_eq!(
            out.0[0].1.as_deref().unwrap(),
            [0.3, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
        assert_eq!(fill(&mut out, &playback, 50, 1.0), 4);
        assert!(out.0[0].1.as_deref().unwrap().iter().all(|&x| x == 0.0));
        // The shortest buffer sets the frames; the rest of a longer one is
        // silent.
        let mut uneven = List(vec![(1, Some(vec![7.0; 5])), (1, Some(vec![7.0; 2]))]);
        assert_eq!(fill(&mut uneven, &playback, 0, 1.0), 2);
        assert_eq!(uneven.0[0].1.as_deref().unwrap(), [0.1, 0.2, 0.0, 0.0, 0.0]);
        assert_eq!(fill(&mut List(vec![]), &playback, 0, 1.0), 0);
    }

    #[test]
    fn capture_takes_the_first_channel() {
        let input = List(vec![
            (0, Some(vec![])),
            (2, Some(vec![0.1, 9.0, f32::NAN, 9.0, 0.3, 9.0])),
        ]);
        let mut space = [5.0; 2];
        assert_eq!(capture(&input, &mut space), 2);
        assert_eq!(space, [0.1, 0.0]);
        let mut space = [5.0; 8];
        assert_eq!(capture(&input, &mut space), 3);
        assert_eq!(space[..4], [0.1, 0.0, 0.3, 5.0]);
        assert_eq!(capture(&List(vec![(1, None)]), &mut space), 0);
        assert_eq!(capture(&List(vec![]), &mut space), 0);
    }

    #[test]
    fn the_tape_keeps_what_was_appended_and_stops_when_full() {
        let tape = Tape::new(5);
        tape.append(|space| {
            space[..3].copy_from_slice(&[1.0, 2.0, 3.0]);
            3
        });
        assert_eq!(tape.recorded(), [1.0, 2.0, 3.0]);
        tape.append(|space| {
            assert_eq!(space.len(), 2);
            space.fill(4.0);
            10
        });
        assert_eq!(tape.recorded(), [1.0, 2.0, 3.0, 4.0, 4.0]);
        tape.append(|space| {
            assert!(space.is_empty());
            0
        });
        assert_eq!(tape.len(), 5);
    }

    #[test]
    fn the_player_sets_the_anchor_once_and_counts_frames() {
        let tape = Arc::new(Tape::new(10));
        tape.append(|space| {
            space[..4].fill(0.5);
            4
        });
        let status = Arc::new(Status {
            played: AtomicUsize::new(0),
            anchor: AtomicUsize::new(NOT_YET),
            panicked: AtomicBool::new(false),
        });
        let mut player = Player {
            playback: vec![0.25; 6],
            position: 0,
            limit: 0.2,
            tape: Arc::clone(&tape),
            status: Arc::clone(&status),
            started: false,
            failed: false,
        };
        let mut out = List(vec![(1, Some(vec![0.0; 4]))]);
        player.render(&mut out);
        assert_eq!(status.anchor.load(Relaxed), 4);
        assert_eq!(out.0[0].1.as_deref().unwrap(), [0.2; 4]);
        tape.append(|_| 0);
        tape.append(|space| {
            space[0] = 1.0;
            1
        });
        player.render(&mut out);
        assert_eq!(status.anchor.load(Relaxed), 4);
        assert_eq!(status.played.load(Relaxed), 8);
        assert_eq!(out.0[0].1.as_deref().unwrap(), [0.2, 0.2, 0.0, 0.0]);
    }

    #[test]
    fn a_stall_is_a_count_that_stays_put() {
        let start = Instant::now();
        let mut stall = Stall::new(start);
        assert!(!stall.check(0, start));
        assert!(!stall.check(5, start + STALL_LIMIT));
        assert!(!stall.check(5, start + STALL_LIMIT + Duration::from_secs(1)));
        assert!(stall.check(5, start + 2 * STALL_LIMIT));
        assert!(!stall.check(6, start + 3 * STALL_LIMIT));
    }

    #[test]
    fn only_rates_a_device_could_have_are_usable() {
        for rate in [8_000.0, 44_100.0, 48_000.0, 768_000.0] {
            assert_eq!(usable_rate("X", rate), Ok(rate));
        }
        for rate in [0.0, 7_999.0, 1e9, f64::NAN] {
            let e = usable_rate("X", rate).unwrap_err();
            assert!(e.starts_with("X reports an unusable sample rate"), "{e}");
        }
    }
}
