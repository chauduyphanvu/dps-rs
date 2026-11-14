//! `dps live`: runs all system audio through the chain in real time.
//!
//! A private process tap mixes every other process to stereo and mutes them
//! while it is being read. A private aggregate device combines it with the
//! default output device, which is the clock; the tap is drift-compensated
//! against it. One IOProc on the aggregate reads the tap's input buffers, runs
//! the chain and writes the device's output buffers, so input and output share
//! one clock and one buffer and need no ring buffer between them.
//!
//! When stdin and stderr are both terminals and the run is in the
//! terminal's foreground, a keyboard tuner takes over the terminal once
//! audio runs; otherwise messages are plain lines on stderr. A background
//! run (`dps start`) also reports its progress for `dps status`. Every run
//! takes `dps bypass` and `dps use` on its control socket (`control`).

mod calibrate;
mod hal;
mod mix;
mod render;
mod tui;
mod tuner;

pub use calibrate::run as calibrate;

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_int, c_void};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::mem::{self, size_of};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed},
};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use dps::{Processor, Profile};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};

use crate::config::{Config, create_default};
use crate::control::{self, Request, Switch};
use crate::presets;
use hal::ObjectId;
use mix::{Controls, Mixer};
use render::{Buffers, BuffersMut, TapChannels};
use tui::Screen;
use tuner::{Effect, Key, Tuner};

/// Largest block the chain runs on; longer device buffers are split.
const BLOCK_FRAMES: usize = 4096;
const POLL: Duration = Duration::from_millis(50);
/// How often to ask the HAL whether other processes are running output.
const PLAYING_POLL: Duration = Duration::from_millis(250);
/// How long other processes may run output without a break, while our
/// callback runs and the tap delivers only exact zeros, before that is
/// reported as a likely missing permission. A denied tap delivers zeros, so
/// the system may be silent until then.
const SILENCE_LIMIT: Duration = Duration::from_secs(3);
/// Device changes arrive as bursts of notifications; wait this long after the
/// last one before rebuilding.
const SETTLE: Duration = Duration::from_millis(300);
/// More rebuilds than this within `REBUILD_WINDOW` end the run. A person
/// switching outputs causes a few; a device that keeps changing, or a
/// session that never matches its device, causes one every half second or
/// so.
const REBUILD_LIMIT: usize = 5;
const REBUILD_WINDOW: Duration = Duration::from_secs(10);
/// Starting IO normally takes milliseconds but blocks while macOS shows its
/// permission prompt.
const START_NOTICE: Duration = Duration::from_secs(1);
/// Apps start and quit with bursts of process changes; wait this long after
/// one before looking at which to leave alone.
const APPS_SETTLE: Duration = Duration::from_millis(500);
/// How often the output volume is read even without a notification, which
/// some devices never send.
const VOLUME_POLL: Duration = Duration::from_secs(2);
/// The least time between restarts made only because the tap could not be
/// told which apps to leave alone, so that apps starting and quitting can
/// restart it at most twice within `REBUILD_WINDOW`.
const APPS_REBUILD_GAP: Duration = Duration::from_secs(5);

pub const PERMISSION_HINT: &str = "allow your terminal app in System Settings > Privacy & Security > \
     Screen & System Audio Recording > System Audio Recording Only, then run dps live again";
/// launchd starts the background run, so macOS asks about dps itself rather
/// than a terminal app.
pub const BACKGROUND_PERMISSION_HINT: &str = "allow dps in System Settings > Privacy & Security > \
     Screen & System Audio Recording > System Audio Recording Only, then run dps start again";
const WAITING_FOR_PERMISSION: &str =
    "waiting for permission to record system audio; answer the macOS prompt";

/// Set by SIGINT, SIGTERM or SIGHUP.
static STOP: AtomicBool = AtomicBool::new(false);
/// Set by the first of those signals; the next one ends the process.
static SIGNALLED: AtomicBool = AtomicBool::new(false);
/// Set by the HAL when the default output device, its sample rate or its
/// presence may have changed.
static CHANGED: AtomicBool = AtomicBool::new(false);
/// Set by the HAL when the session must be rebuilt: IO on the aggregate
/// device stopped on its own, or its streams changed.
static REBUILD: AtomicBool = AtomicBool::new(false);
/// Set by the HAL when a process connects to or leaves the audio system.
static PROCESSES: AtomicBool = AtomicBool::new(false);
/// Set by the HAL when the output volume may have changed.
static VOLUME: AtomicBool = AtomicBool::new(false);

/// Counters the audio callback and HAL listeners update for the main thread.
#[derive(Default)]
struct Stats {
    /// Calls of the audio callback.
    callbacks: AtomicU64,
    clamped: AtomicU64,
    /// Callbacks where the tap delivered fewer frames than the output needed.
    short: AtomicU64,
    overloads: AtomicU64,
    /// The tap has delivered a non-zero sample.
    heard: AtomicBool,
    non_finite: AtomicBool,
    panicked: AtomicBool,
}

/// How a live run gets and keeps its settings.
pub struct Options {
    /// The settings to start with.
    pub profile: Profile,
    /// What messages call them: a file path, or e.g. "preset music".
    pub name: String,
    /// A file to reload the settings from whenever it is saved.
    pub watch: Option<PathBuf>,
    /// Where settings changed while running are saved.
    pub save_path: PathBuf,
    /// config.toml, watched for changes.
    pub config: Option<PathBuf>,
    /// The settings file, which `dps use --settings` goes back to.
    pub settings_file: Option<PathBuf>,
    /// Each output device plays with the profile config.toml assigns it, or
    /// else with `settings_file`.
    pub follow_devices: bool,
    /// Where to listen for `dps bypass` and `dps use`; the run holds the
    /// live lock, so whatever is there is left over and is replaced.
    pub control: Option<PathBuf>,
    /// Set for a background run, which has no terminal and reports here.
    pub background: Option<Arc<dyn Report>>,
}

/// What a background run tells `dps status` and `dps start` about itself.
pub trait Report: Send + Sync {
    /// Starting audio is blocked on the macOS permission prompt.
    fn waiting_for_permission(&self);
    /// Audio runs on `device` at `rate` with the profile `profile`, with
    /// `latency` as `Latency::describe` puts it if known; said again when
    /// the profile changes.
    fn running(&self, device: &str, rate: u32, profile: &str, latency: Option<&str>);
    /// The tap has delivered sound, so the permission is there.
    fn heard(&self);
    fn reload_failed(&self, message: &str);
    fn reloaded(&self);
    /// The `exclude` entries of config.toml that cover a running app, whose
    /// audio is left alone.
    fn excluded(&self, apps: &[String]);
}

pub fn run(options: Options) -> Result<(), String> {
    // A process in the background (`dps live &`) that changed the terminal's
    // mode would be stopped with its tap still muting every other app.
    let interactive = options.background.is_none()
        && io::stdin().is_terminal()
        && io::stderr().is_terminal()
        && in_foreground(STDIN);
    let mut live = Live::new(options);
    install_signal_handlers()?;
    let _no_suspend = NoSuspend::new()?;
    hal::notify_on_hal_thread()?;
    let own = hal::process_object(std::process::id() as i32)?
        .ok_or("cannot find this process's Core Audio object to exclude it from the tap")?;
    let stats = Arc::new(Stats::default());
    let controls = Arc::new(Controls::default());
    // SAFETY: `on_change` and `on_processes` only touch statics.
    let _system_watch = unsafe {
        [
            hal::Listener::add(
                hal::SYSTEM_OBJECT,
                hal::kAudioHardwarePropertyDefaultOutputDevice,
                hal::GLOBAL,
                Some(on_change),
                ptr::null_mut(),
                "watch the default output device",
            )?,
            hal::Listener::add(
                hal::SYSTEM_OBJECT,
                hal::kAudioHardwarePropertyProcessObjectList,
                hal::GLOBAL,
                Some(on_processes),
                ptr::null_mut(),
                "watch the audio processes",
            )?,
        ]
    };

    let started = Instant::now();
    let mut target = Target::current()?;
    let mut restarted = false;
    let mut announced = false;
    let mut rebuilds = Rebuilds::default();
    // Kept until a session announces it, past one that ends at once.
    let mut switched = None;
    loop {
        switched = live.follow(&target.uid, &target.name, None).or(switched);
        let rate = target.validated_rate(live.tuner.profile(), &live.name)?;
        let notice = live.permission_notice();
        let background = live.background.clone();
        let on_slow_start = move || {
            let _ = io::stderr().write_all(&notice);
            if let Some(report) = &background {
                report.waiting_for_permission();
            }
        };
        let exclude = live.session_exclusions(own)?;
        controls.set_volume_db(hal::output_volume_db(target.device).ok().flatten());
        let profile = live.tuner.profile();
        let hint = live.permission_hint();
        let session = Session::start(
            &target,
            profile,
            rate,
            &exclude,
            &stats,
            &controls,
            hint,
            on_slow_start,
        )
        .map_err(|e| {
            if restarted {
                format!(
                    "the output changed to {} and restarting on it failed: {e}",
                    target.name
                )
            } else {
                e
            }
        })?;
        // Notifications sent while the session started, or late ones about
        // the previous aggregate, are stale; `mismatch` reads what they would
        // have reported.
        CHANGED.store(false, Relaxed);
        REBUILD.store(false, Relaxed);
        let next = match session.mismatch(&target, rate)? {
            Some(reason) => match settle_and_read()? {
                Some(now) => Next::Rebuild(now, reason),
                None => Next::Stop,
            },
            None => {
                if interactive && live.screen.is_none() {
                    let screen = Screen::enter()
                        .map_err(|e| format!("cannot set up the terminal for the tuner: {e}"))?;
                    live.screen = Some(screen);
                }
                live.latency = session.latency;
                live.announce(&target.name, rate, announced, switched.take());
                announced = true;
                watch(&target, own, &stats, &controls, &session, rate, &mut live)?
            }
        };
        drop(session);
        let Next::Rebuild(new_target, reason) = next else {
            break;
        };
        if !rebuilds.record(Instant::now()) {
            return Err(format!(
                "dps live had to restart more than {REBUILD_LIMIT} times within {} s, most \
                 recently because {reason}, so it stopped",
                REBUILD_WINDOW.as_secs()
            ));
        }
        target = new_target;
        restarted = true;
    }

    // The terminal goes back to normal before the summary is printed.
    live.screen = None;
    let secs = started.elapsed().as_secs();
    // Not eprintln!, which panics when the terminal has been closed.
    let _ = writeln!(
        io::stderr(),
        "dps live: stopped after {}:{:02}:{:02}; {} samples clamped, {} callbacks short of input, {} overloads",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        stats.clamped.load(Relaxed),
        stats.short.load(Relaxed),
        stats.overloads.load(Relaxed),
    );
    Ok(())
}

/// The profile file, watched for saves while live.
struct ProfileFile {
    path: PathBuf,
    /// When the file was last modified and its length, as last seen.
    seen: Option<(SystemTime, u64)>,
}

impl ProfileFile {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            seen: Self::stamp(path),
        }
    }

    fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
        let metadata = fs::metadata(path).ok()?;
        Some((metadata.modified().ok()?, metadata.len()))
    }

    /// The profile if the file was saved since the last look, or why that
    /// save cannot be used at `rate`.
    fn changed(&mut self, rate: u32) -> Option<Result<Profile, String>> {
        // A missing file is an editor mid-save; the next look sees the result.
        let stamp = Self::stamp(&self.path)?;
        if self.seen == Some(stamp) {
            return None;
        }
        self.seen = Some(stamp);
        // So is an empty one, which would otherwise load as the defaults.
        if fs::read_to_string(&self.path).is_ok_and(|text| text.trim().is_empty()) {
            return None;
        }
        Some(super::load_profile(&self.path).and_then(|profile| {
            profile
                .validate(rate)
                .map_err(|e| format!("{}: {e}", self.path.display()))?;
            Ok(profile)
        }))
    }

    /// Takes the file as it is now as already seen.
    fn refresh(&mut self) {
        self.seen = Self::stamp(&self.path);
    }
}

/// config.toml, watched for saves like the profile file, and what it last
/// said that could be read.
struct ConfigFile {
    path: PathBuf,
    seen: Option<(SystemTime, u64)>,
    config: Config,
}

impl ConfigFile {
    /// The file as it is now; one that cannot be read counts as empty, and
    /// why is returned to be said.
    fn new(path: &Path) -> (Self, Option<String>) {
        let (config, problem) = match Config::load(path) {
            Ok(config) => (config, None),
            Err(e) => (Config::default(), Some(e)),
        };
        let file = Self {
            path: path.to_owned(),
            seen: ProfileFile::stamp(path),
            config,
        };
        (file, problem)
    }

    fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// Takes the file in if it was saved since the last look; why not, once
    /// per save, if it cannot be read.
    fn changed(&mut self) -> Option<Result<(), String>> {
        // A missing or empty file is an editor mid-save, as for the profile.
        let stamp = ProfileFile::stamp(&self.path)?;
        if self.seen == Some(stamp) {
            return None;
        }
        self.seen = Some(stamp);
        if fs::read_to_string(&self.path).is_ok_and(|text| text.trim().is_empty()) {
            return None;
        }
        Some(Config::load(&self.path).map(|config| self.config = config))
    }
}

/// The profile in the file at `path`, checked at `rate` if given.
fn load_checked(path: &Path, rate: Option<u32>) -> Result<Profile, String> {
    let profile = super::load_profile(path)?;
    if let Some(rate) = rate {
        profile
            .validate(rate)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(profile)
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// Which apps the tap leaves out, kept across sessions.
#[derive(Default)]
struct Apps {
    /// The processes the session's tap leaves out, as last set.
    applied: Vec<ObjectId>,
    /// The `exclude` entries that cover a running app.
    shown: Vec<String>,
    /// `shown` changed since it was last said.
    unsaid: bool,
    /// Bundle IDs by process, so each is read from the HAL once.
    bundles: HashMap<ObjectId, String>,
    /// When to look at the processes again.
    due: Option<Instant>,
    /// When the session last restarted to change them.
    rebuilt: Option<Instant>,
}

/// What a run keeps across sessions: the settings being tuned, where they
/// come from and go, and the tuner's screen once it is up.
struct Live {
    tuner: Tuner,
    name: String,
    save_path: PathBuf,
    profile_file: Option<ProfileFile>,
    config: Option<ConfigFile>,
    settings_file: Option<PathBuf>,
    /// Each output device plays with the profile config.toml assigns it,
    /// and every other device with the settings file.
    follow_devices: bool,
    /// Where `dps bypass` and `dps use` reach this run.
    server: Option<control::Server>,
    /// The output device of the session: its UID and name.
    device: Option<(String, String)>,
    /// What the HAL reported for the session.
    latency: Option<Latency>,
    apps: Apps,
    /// Set in interactive runs once audio first runs.
    screen: Option<Screen>,
    background: Option<Arc<dyn Report>>,
    /// The tap's first sound has been reported.
    heard: bool,
}

impl Live {
    fn new(options: Options) -> Self {
        let (config, problem) = options.config.as_deref().map(ConfigFile::new).unzip();
        let server = options.control.as_deref().map(control::Server::bind);
        let mut live = Self {
            tuner: Tuner::new(options.profile, options.save_path.display().to_string()),
            profile_file: options.watch.as_deref().map(ProfileFile::new),
            name: options.name,
            save_path: options.save_path,
            config,
            settings_file: options.settings_file,
            follow_devices: options.follow_devices,
            server: None,
            device: None,
            latency: None,
            apps: Apps::default(),
            screen: None,
            background: options.background,
            heard: false,
        };
        if let Some(problem) = problem.flatten() {
            live.warn(format!(
                "config.toml is ignored until it is fixed: {problem}"
            ));
        }
        // Audio does not need the socket, so a run goes on without it.
        match server {
            Some(Ok(server)) => live.server = Some(server),
            Some(Err(e)) => live.warn(format!(
                "{e}; `dps bypass` and `dps use` cannot reach this run"
            )),
            None => {}
        }
        live
    }

    /// Says a problem that does not end the run, and reports it to `dps
    /// status`.
    fn warn(&mut self, message: String) {
        if let Some(report) = &self.background {
            report.reload_failed(&message);
        }
        self.say(message);
    }

    /// Points the run at the profile the output device `uid`, called
    /// `device`, plays with, if that follows the device; the profile is
    /// checked at `rate` if given. Returns what to add to the announcement,
    /// " with <file>" and more, if the profile changed or the device's own
    /// cannot be used.
    fn follow(&mut self, uid: &str, device: &str, rate: Option<u32>) -> Option<String> {
        self.device = Some((uid.into(), device.into()));
        if !self.follow_devices || self.config.is_none() {
            return None;
        }
        let settings = self.settings_file.clone()?;
        let mut problem = None;
        let mut next = None;
        if let Some(own) = self.own_profile() {
            if same_file(&own, &self.save_path) {
                return None;
            }
            match load_checked(&own, rate) {
                Ok(profile) => next = Some((own, profile)),
                Err(e) => problem = Some(format!("its own profile cannot be used: {e}")),
            }
        }
        let switched = match next {
            Some((own, profile)) => Some(self.switch(own, profile)),
            None if same_file(&settings, &self.save_path) => None,
            None => match load_checked(&settings, rate) {
                Ok(profile) => Some(self.switch(settings, profile)),
                Err(e) => {
                    let mut message = format!("{device} keeps {}", self.name);
                    if let Some(problem) = &problem {
                        message += &format!(", since {problem},");
                    }
                    self.warn(format!(
                        "{message} as the settings file cannot be used: {e}"
                    ));
                    return None;
                }
            },
        };
        if switched.is_none() && problem.is_none() {
            return None;
        }
        let mut note = format!(" with {}", self.name);
        if let Some(problem) = &problem {
            note += &format!(", since {problem}");
            if let Some(report) = &self.background {
                report.reload_failed(&format!("{device} plays{note}"));
            }
        }
        Some(note + &switched.unwrap_or_default())
    }

    /// The profile file config.toml gives the session's output device, if
    /// it gives it one.
    fn own_profile(&self) -> Option<PathBuf> {
        let (uid, _) = self.device.as_ref()?;
        let config = self.config.as_ref()?;
        config.config.profile_for(uid, config.dir())
    }

    /// Plays, edits and saves `profile` from the file `path` from now on;
    /// returns what to add after "with <path>" in a message.
    fn switch(&mut self, path: PathBuf, profile: Profile) -> String {
        self.switch_to(profile, path.display().to_string(), path, true)
    }

    /// Plays and edits `profile`, called `name`, from now on, saving it to
    /// `save_path`, which is watched for saves if `watched`; returns what
    /// to add after "with <name>" in a message.
    fn switch_to(
        &mut self,
        profile: Profile,
        name: String,
        save_path: PathBuf,
        watched: bool,
    ) -> String {
        let asked = self.tuner.prompt().is_some();
        let dropped = self.tuner.reload(profile);
        let old = mem::replace(&mut self.name, name);
        self.save_path = save_path;
        self.tuner.set_save_to(self.save_path.display().to_string());
        self.profile_file = watched.then(|| ProfileFile::new(&self.save_path));
        if let Some(report) = &self.background {
            report.reloaded();
        }
        let mut note = String::new();
        if dropped {
            note += &format!("; unsaved changes to {old} were dropped");
        }
        if asked {
            note += ", so there is nothing to save; press q to quit";
        }
        note
    }

    /// The processes the next session's tap leaves out: this one, `own`,
    /// and those of the apps config.toml excludes.
    fn session_exclusions(&mut self, own: ObjectId) -> Result<Vec<ObjectId>, String> {
        let wanted = if self.excludes_nothing() {
            vec![own]
        } else {
            let processes = audio_processes(&mut self.apps.bundles)?;
            self.apps_now(own, &processes).0
        };
        self.apps.applied.clone_from(&wanted);
        Ok(wanted)
    }

    fn excludes_nothing(&self) -> bool {
        self.config
            .as_ref()
            .is_none_or(|c| c.config.exclude.is_empty())
    }

    /// Takes in `processes`, each with its bundle ID if it has one: returns
    /// what the tap should leave out, sorted, and whether that differs from
    /// what it does among the processes that still exist.
    fn apps_now(
        &mut self,
        own: ObjectId,
        processes: &[(ObjectId, Option<String>)],
    ) -> (Vec<ObjectId>, bool) {
        let mut wanted = vec![own];
        let mut shown = Vec::new();
        if let Some(config) = &self.config {
            for (process, bundle) in processes {
                if let Some(entry) = bundle.as_deref().and_then(|b| config.config.exclusion(b)) {
                    wanted.push(*process);
                    shown.push(entry.to_owned());
                }
            }
        }
        wanted.sort_unstable();
        wanted.dedup();
        shown.sort_unstable();
        shown.dedup();
        if shown != self.apps.shown {
            self.apps.shown = shown;
            self.apps.unsaid = true;
        }
        let exists = |p: &ObjectId| *p == own || processes.iter().any(|(q, _)| q == p);
        let still: Vec<ObjectId> = self.apps.applied.iter().copied().filter(exists).collect();
        let changed = still != wanted;
        (wanted, changed)
    }

    /// Which apps are left alone, as a message, if that changed since it
    /// was last said; reported to `dps status` too.
    fn apps_note(&mut self) -> Option<String> {
        if !mem::take(&mut self.apps.unsaid) {
            return None;
        }
        if let Some(report) = &self.background {
            report.excluded(&self.apps.shown);
        }
        let apps = match &self.apps.shown[..] {
            [] => "none running".into(),
            shown => shown.join(", "),
        };
        Some(format!("apps left alone: {apps}"))
    }

    /// Brings what `tap` leaves out up to date with the running processes
    /// and config.toml; why the session must restart instead, if the tap
    /// cannot be changed.
    fn check_apps(&mut self, own: ObjectId, tap: &hal::Tap) -> Result<Option<String>, String> {
        if self.excludes_nothing() && self.apps.applied == [own] && self.apps.shown.is_empty() {
            return Ok(None);
        }
        let processes = audio_processes(&mut self.apps.bundles)?;
        let (wanted, changed) = self.apps_now(own, &processes);
        if let Some(note) = self.apps_note() {
            self.say(note);
        }
        if !changed {
            return Ok(None);
        }
        let Err(e) = tap.exclude(&wanted) else {
            self.apps.applied = wanted;
            return Ok(None);
        };
        let now = Instant::now();
        if let Some(next) = next_apps_rebuild(self.apps.rebuilt, now) {
            self.apps.due = Some(next);
            return Ok(None);
        }
        self.apps.rebuilt = Some(now);
        self.say(format!(
            "{e}; restarting audio to leave the right apps alone"
        ));
        Ok(Some("the apps to leave alone changed".into()))
    }

    fn permission_hint(&self) -> &'static str {
        if self.background.is_some() {
            BACKGROUND_PERMISSION_HINT
        } else {
            PERMISSION_HINT
        }
    }

    /// Reports the tap's first sound to `dps status`.
    fn check_heard(&mut self, stats: &Stats) {
        if !self.heard && stats.heard.load(Relaxed) {
            self.heard = true;
            if let Some(report) = &self.background {
                report.heard();
            }
        }
    }

    /// Shows a message on the tuner's message line, or as a line on stderr
    /// while the tuner is not up.
    fn say(&mut self, message: String) {
        if self.screen.is_none() {
            print_line(&message);
        }
        self.tuner.set_message(message);
    }

    /// What `notify_if_slow` writes to stderr. The tuner reads no keys
    /// while audio starts, so its version does not offer Ctrl-C.
    fn permission_notice(&self) -> Vec<u8> {
        match &self.screen {
            Some(screen) => screen.notice(WAITING_FOR_PERMISSION),
            None if self.background.is_some() => {
                format!("dps live: {WAITING_FOR_PERMISSION}\n").into_bytes()
            }
            None => format!("dps live: {WAITING_FOR_PERMISSION}, or press Ctrl-C twice to quit\n")
                .into_bytes(),
        }
    }

    /// Says that audio runs on `device` at `rate`, `again` after a change,
    /// with `switched` from `follow` after it.
    fn announce(&mut self, device: &str, rate: u32, again: bool, switched: Option<String>) {
        if let Some(screen) = &mut self.screen {
            // A restart may have written over the screen.
            screen.invalidate();
        }
        let latency = self.latency.map(|l| l.describe());
        if let Some(report) = &self.background {
            report.running(device, rate, &self.name, latency.as_deref());
        }
        let mut details = switched.unwrap_or_default();
        if let Some(latency) = latency {
            details += &format!("; {latency}");
        }
        if let Some(apps) = self.apps_note() {
            details += &format!("; {apps}");
        }
        if again {
            self.say(format!("now on {device} at {rate} Hz{details}"));
        } else if self.screen.is_some() || self.background.is_some() {
            self.say(format!(
                "processing system audio on {device} at {rate} Hz{details}"
            ));
        } else {
            print_line(&format!(
                "processing system audio on {device} at {rate} Hz{details}; press Ctrl-C to stop"
            ));
        }
    }

    /// Waits up to `POLL` for a key; plain runs just wait.
    fn wait(&mut self) -> Result<Option<Key>, String> {
        let Some(screen) = &mut self.screen else {
            thread::sleep(POLL);
            return Ok(None);
        };
        match screen.next_key(POLL) {
            Ok(key) => Ok(key),
            // The terminal was closed: stop as on the hangup signal, which
            // may not have arrived yet.
            Err(e) if STOP.load(Relaxed) || e.kind() == io::ErrorKind::UnexpectedEof => {
                STOP.store(true, Relaxed);
                Ok(None)
            }
            Err(e) => Err(format!("cannot read the keyboard: {e}")),
        }
    }

    /// Acts on a key, if one was pressed, and on a save of the watched
    /// file; true if the run should end.
    fn step(
        &mut self,
        key: Option<Key>,
        rate: u32,
        handoff: &Handoff<Processor>,
        controls: &Controls,
    ) -> bool {
        // A new session's audio started with the heard slot's chain alone.
        if handoff.first_sync() {
            self.tuner.restarted();
        }
        if let Some(key) = key
            && self.key(key, rate)
        {
            return true;
        }
        if let Some(call) = self.server.as_ref().and_then(control::Server::next) {
            let answer = match &call.request {
                Ok(request) => self.control(request, rate),
                Err(e) => Err(e.clone()),
            };
            call.answer(answer);
        }
        match self
            .profile_file
            .as_mut()
            .and_then(|file| file.changed(rate))
        {
            Some(Ok(new)) => self.reloaded(new),
            Some(Err(e)) => self.warn(format!("profile not reloaded: {e}")),
            None => {}
        }
        self.reload_config(rate);
        self.sync(rate, handoff, controls);
        false
    }

    /// Takes in a save of config.toml, switching to the profile it now
    /// assigns the output device, if that changed.
    fn reload_config(&mut self, rate: u32) {
        match self.config.as_mut().and_then(ConfigFile::changed) {
            None => {}
            Some(Err(e)) => self.warn(format!("config.toml not reloaded: {e}")),
            Some(Ok(())) => {
                self.apps.due.get_or_insert_with(Instant::now);
                let before = self.save_path.clone();
                let followed = self
                    .device
                    .clone()
                    .and_then(|(uid, name)| Some((self.follow(&uid, &name, Some(rate))?, name)));
                if self.save_path != before {
                    self.report_profile(rate);
                }
                match followed {
                    Some((note, device)) => self.say(format!("{device} now plays{note}")),
                    None => {
                        if let Some(report) = &self.background {
                            report.reloaded();
                        }
                        let path = self.config.as_ref().map(|c| c.path.display().to_string());
                        self.say(format!("reloaded {}", path.unwrap_or_default()));
                    }
                }
            }
        }
    }

    /// Brings the latency, and what a background run reports, up to date
    /// with a change of profile.
    fn report_profile(&mut self, rate: u32) {
        let chain = Processor::new(self.tuner.profile(), rate, 2);
        if let Some(latency) = &mut self.latency {
            latency.chain = chain.latency_frames();
        }
        let latency = self.latency.map(|l| l.describe());
        if let (Some(report), Some((_, device))) = (&self.background, &self.device) {
            report.running(device, rate, &self.name, latency.as_deref());
        }
    }

    /// Acts on a request from `dps bypass` or `dps use`, and returns what to
    /// answer; done, it is said here too. What it changes reaches the audio
    /// through `sync`.
    fn control(&mut self, request: &Request, rate: u32) -> Result<String, String> {
        let said = match request {
            Request::Bypass(switch) => {
                let was = self.tuner.bypass();
                let on = match switch {
                    Switch::On => true,
                    Switch::Off => false,
                    Switch::Toggle => !was,
                };
                if !self.tuner.set_bypass(on) {
                    return Err(
                        "the tuner is running a blind test, which keeps bypass off; end it \
                         with x first"
                            .into(),
                    );
                }
                let state = if on { "on" } else { "off" };
                if on == was {
                    format!("bypass was already {state}")
                } else {
                    format!("bypass is {state}")
                }
            }
            Request::Preset(name) => {
                let preset =
                    presets::find(name).ok_or_else(|| format!("there is no preset {name}"))?;
                let name = format!("preset {}", preset.name);
                let profile = preset.profile();
                self.fits(&profile, &name, rate)?;
                let save_path = self.settings_file.clone().ok_or(
                    "cannot locate the settings file, where changes to a preset are saved",
                )?;
                let note = self.switch_to(profile, name, save_path, false);
                self.follow_devices = false;
                format!("now playing {} on every output device{note}", self.name)
            }
            Request::File(path) => {
                let profile = super::load_profile(path)?;
                let name = path.display().to_string();
                self.fits(&profile, &name, rate)?;
                let note = self.switch_to(profile, name, path.clone(), true);
                self.follow_devices = false;
                format!("now playing {} on every output device{note}", self.name)
            }
            Request::Settings => {
                let settings = self
                    .settings_file
                    .clone()
                    .ok_or("cannot locate the settings file")?;
                let created = create_default(&settings)?;
                let device = self
                    .device
                    .clone()
                    .map(|(_, name)| name)
                    .unwrap_or_default();
                let mut own = None;
                let mut problem = String::new();
                if let Some(path) = self.own_profile() {
                    match load_checked(&path, Some(rate)) {
                        Ok(profile) => own = Some((path, profile)),
                        Err(e) => problem = format!("; {device}'s own cannot be used: {e}"),
                    }
                }
                let (path, profile, whose) = match own {
                    Some((path, profile)) => (path, profile, format!("the profile of {device}")),
                    None => {
                        let profile = super::load_profile(&settings)?;
                        self.fits(&profile, &settings.display().to_string(), rate)?;
                        (settings.clone(), profile, "the settings file".into())
                    }
                };
                let note = self.switch(path, profile);
                self.follow_devices = true;
                let mut said = format!(
                    "now playing {}, {whose}, and following the output device",
                    self.name
                );
                if created {
                    said += &format!("; created {} with the default profile", settings.display());
                }
                said + &problem + &note
            }
        };
        if !matches!(request, Request::Bypass(_)) {
            self.report_profile(rate);
        }
        self.say(said.clone());
        Ok(said)
    }

    /// Whether `profile`, called `name`, can play on the session's output at
    /// `rate`.
    fn fits(&self, profile: &Profile, name: &str, rate: u32) -> Result<(), String> {
        let device = self.device.as_ref().map_or("the output", |(_, name)| name);
        profile.validate(rate).map_err(|e| {
            format!("{name} does not fit {device} at {rate} Hz: {e}; nothing was changed")
        })
    }

    /// Acts on a key; true if the run should end. What it changes reaches
    /// the audio through `sync`.
    fn key(&mut self, key: Key, rate: u32) -> bool {
        match self.tuner.key(key, rate) {
            Effect::None
            | Effect::Apply
            | Effect::Bypass(_)
            | Effect::Listen
            | Effect::Matching(_) => false,
            Effect::Save => {
                self.save();
                false
            }
            Effect::SaveAndQuit => self.save(),
            Effect::Quit => true,
        }
    }

    /// Brings the audio in line with the tuner: a chain for each slot that
    /// needs a new one, the slot heard, bypass and level matching.
    fn sync(&mut self, rate: u32, handoff: &Handoff<Processor>, controls: &Controls) {
        for (slot, profile) in self.tuner.chains(rate) {
            match profile {
                Some(profile) => handoff.post(slot, Processor::new(&profile, rate, 2)),
                None => handoff.clear(slot),
            }
        }
        controls.set_slot(self.tuner.slot());
        controls.set_matching(self.tuner.matching());
        controls.bypass.store(self.tuner.bypass(), Relaxed);
    }

    /// Saves the profile; false, with the reason on screen, if that failed.
    fn save(&mut self) -> bool {
        match tuner::save(&self.save_path, self.tuner.profile()) {
            Ok(()) => {
                self.tuner.saved();
                // The watcher would otherwise reload what was just saved.
                if let Some(file) = &mut self.profile_file
                    && same_file(&file.path, &self.save_path)
                {
                    file.refresh();
                }
                true
            }
            Err(e) => {
                self.tuner.set_message(e);
                false
            }
        }
    }

    fn reloaded(&mut self, profile: Profile) {
        if let Some(report) = &self.background {
            report.reloaded();
        }
        let asked = self.tuner.prompt().is_some();
        let edited = self.tuner.reload(profile);
        let mut message = format!("reloaded {}", self.name);
        if edited {
            message += "; unsaved changes were replaced";
        }
        if asked {
            message += ", so there is nothing to save; press q to quit";
        }
        self.say(message);
    }

    fn draw(
        &mut self,
        device: &str,
        rate: u32,
        stats: &Stats,
        controls: &Controls,
    ) -> Result<(), String> {
        let Some(screen) = &mut self.screen else {
            return Ok(());
        };
        let contour = contour_text(self.tuner.profile(), controls.volume_db());
        let view = tui::View {
            tuner: &self.tuner,
            name: &self.name,
            device,
            rate,
            clamped: stats.clamped.load(Relaxed),
            dry_gain: controls.dry_gain(),
            contour: contour.as_deref(),
            meters: controls.readings(),
            trim: controls.trim_db(),
        };
        match screen.draw(&view) {
            Ok(()) => Ok(()),
            Err(_) if STOP.load(Relaxed) => Ok(()),
            Err(e) => Err(format!("cannot draw the tuner: {e}")),
        }
    }
}

/// Passes new chains to the audio callback, which may not lock, allocate or
/// free, in one lane for each of the tuner's two slots: the callback takes a
/// slot's new chain, or word that the slot is to play the other slot's
/// chain, and later hands the chain it replaced back to be freed here.
struct Handoff<C> {
    lanes: [Lane<C>; 2],
    /// Main thread: set once it has brought this session in line with the
    /// tuner.
    synced: AtomicBool,
}

struct Lane<C> {
    pending: AtomicPtr<C>,
    retired: AtomicPtr<C>,
}

/// What the callback takes for a slot.
enum Offer<C> {
    Chain(Box<C>),
    /// Play the other slot's chain.
    Empty,
}

/// Its address stands for `Offer::Empty` in a lane; no chain is ever there.
static EMPTY: u8 = 0;

impl<C> Handoff<C> {
    fn new() -> Self {
        let lane = || Lane {
            pending: AtomicPtr::new(ptr::null_mut()),
            retired: AtomicPtr::new(ptr::null_mut()),
        };
        Self {
            lanes: [lane(), lane()],
            synced: AtomicBool::new(false),
        }
    }

    fn empty() -> *mut C {
        (&raw const EMPTY).cast::<C>().cast_mut()
    }

    fn free(chain: *mut C) {
        if !chain.is_null() && chain != Self::empty() {
            // SAFETY: every other pointer stored here came from
            // `Box::into_raw` in `post`, and the swap that produced `chain`
            // took it out.
            drop(unsafe { Box::from_raw(chain) });
        }
    }

    /// Main thread: offers `chain` for `slot`, replacing an offer for it the
    /// callback has not taken.
    fn post(&self, slot: usize, chain: C) {
        self.offer(slot, Box::into_raw(Box::new(chain)));
    }

    /// Main thread: asks `slot` to play the other slot's chain, replacing an
    /// offer for it the callback has not taken.
    fn clear(&self, slot: usize) {
        self.offer(slot, Self::empty());
    }

    fn offer(&self, slot: usize, offer: *mut C) {
        Self::free(self.lanes[slot].pending.swap(offer, AcqRel));
    }

    /// Main thread: frees the chains the callback handed back.
    fn collect(&self) {
        for lane in &self.lanes {
            Self::free(lane.retired.swap(ptr::null_mut(), AcqRel));
        }
    }

    /// Main thread: true the first time it is asked, as a new session's
    /// handoff is.
    fn first_sync(&self) -> bool {
        !self.synced.swap(true, Relaxed)
    }

    /// Callback: takes the offer for `slot`, if any. Each offer taken may be
    /// followed by one `retire` to that slot before its next `take`.
    fn take(&self, slot: usize) -> Option<Offer<C>> {
        let lane = &self.lanes[slot];
        // Wait while the last one handed back is uncollected, so the single
        // return slot is never overwritten.
        if !lane.retired.load(Acquire).is_null() {
            return None;
        }
        let offered = NonNull::new(lane.pending.swap(ptr::null_mut(), AcqRel))?;
        if offered.as_ptr() == Self::empty() {
            return Some(Offer::Empty);
        }
        // SAFETY: from `Box::into_raw` in `post`, and the swap made this
        // thread its only holder.
        Some(Offer::Chain(unsafe { Box::from_raw(offered.as_ptr()) }))
    }

    /// Callback: hands `slot`'s old chain back for `collect` to free.
    fn retire(&self, slot: usize, chain: Box<C>) {
        let previous = self.lanes[slot].retired.swap(Box::into_raw(chain), AcqRel);
        debug_assert!(previous.is_null(), "a chain handed back was overwritten");
    }
}

impl<C> Drop for Handoff<C> {
    fn drop(&mut self) {
        for lane in &mut self.lanes {
            Self::free(*lane.pending.get_mut());
            Self::free(*lane.retired.get_mut());
        }
    }
}

/// What to do once a session ends.
enum Next {
    Stop,
    /// Start a new session on this output, for this reason.
    Rebuild(Target, String),
}

/// Watches a running session until Ctrl-C, quitting the tuner, a failure,
/// or a change that needs a new session.
fn watch(
    target: &Target,
    own: ObjectId,
    stats: &Stats,
    controls: &Controls,
    session: &Session,
    rate: u32,
    live: &mut Live,
) -> Result<Next, String> {
    let mut silence = SilenceWatch::new(Instant::now(), stats.callbacks.load(Relaxed));
    let mut warned = false;
    // `run` read it just before the session started.
    let mut volume_read = Instant::now();
    loop {
        let key = live.wait()?;
        if STOP.load(Relaxed) {
            return Ok(Next::Stop);
        }
        if VOLUME.swap(false, Relaxed) || volume_read.elapsed() >= VOLUME_POLL {
            controls.set_volume_db(hal::output_volume_db(target.device).ok().flatten());
            volume_read = Instant::now();
        }
        if live.step(key, rate, &session.handoff, controls) {
            return Ok(Next::Stop);
        }
        live.check_heard(stats);
        session.handoff.collect();
        if stats.non_finite.load(Relaxed) {
            return Err(
                "the processed signal became NaN or infinite; the output was \
                 silenced and dps live stopped; check the profile"
                    .into(),
            );
        }
        if stats.panicked.load(Relaxed) {
            return Err(
                "internal error in the audio callback; the output was silenced \
                 and dps live stopped"
                    .into(),
            );
        }
        if PROCESSES.swap(false, Relaxed) {
            live.apps
                .due
                .get_or_insert_with(|| Instant::now() + APPS_SETTLE);
        }
        if live.apps.due.is_some_and(|due| Instant::now() >= due) {
            live.apps.due = None;
            if let Some(reason) = live.check_apps(own, &session.tap)? {
                return Ok(Next::Rebuild(Target::current()?, reason));
            }
        }
        let forced = REBUILD.swap(false, Relaxed);
        if forced || CHANGED.load(Relaxed) {
            let Some(now) = settle_and_read()? else {
                return Ok(Next::Stop);
            };
            if forced {
                let reason = "the audio device stopped or changed its streams".into();
                return Ok(Next::Rebuild(now, reason));
            }
            if !now.same_as(target) {
                return Ok(Next::Rebuild(now, "the output changed".into()));
            }
        }
        if !warned && !stats.heard.load(Relaxed) {
            let now = Instant::now();
            if silence.due(now)
                && silence.exceeded(
                    now,
                    stats.callbacks.load(Relaxed),
                    hal::others_playing(own)?,
                )
            {
                let seen = format!(
                    "only silence from the system audio tap for {} s while other apps have \
                     audio output running",
                    SILENCE_LIMIT.as_secs()
                );
                // Nobody watches a background run, and a missing permission
                // would leave every other app muted, so it stops instead.
                if live.background.is_some() {
                    return Err(format!(
                        "{seen}, so dps stopped to let them play normally; the System Audio \
                         Recording permission is probably missing: {BACKGROUND_PERMISSION_HINT} \
                         (if dps is already allowed there, an app was holding its output open \
                         while silent; run dps start again)"
                    ));
                }
                // Only a warning: an app can hold its output open while
                // sending zeros (a paused player), which looks the same.
                live.say(format!(
                    "warning: {seen}; if you hear nothing while something is playing, the \
                     Screen & System Audio Recording permission is probably missing: \
                     {PERMISSION_HINT}"
                ));
                warned = true;
            }
        }
        live.draw(&target.name, rate, stats, controls)?;
    }
}

/// Waits for a burst of changes to end, then reads the output to restart
/// on; `None` if Ctrl-C came meanwhile.
fn settle_and_read() -> Result<Option<Target>, String> {
    settle();
    if STOP.load(Relaxed) {
        return Ok(None);
    }
    Target::current().map(Some)
}

/// Waits until no change notification has arrived for `SETTLE`.
fn settle() {
    loop {
        CHANGED.store(false, Relaxed);
        thread::sleep(SETTLE);
        if !CHANGED.load(Relaxed) || STOP.load(Relaxed) {
            return;
        }
    }
}

/// Recent rebuilds, to stop a loop of them.
#[derive(Default)]
struct Rebuilds(VecDeque<Instant>);

impl Rebuilds {
    /// Records a rebuild at `now`; false if that makes more than
    /// `REBUILD_LIMIT` within `REBUILD_WINDOW`.
    fn record(&mut self, now: Instant) -> bool {
        while self
            .0
            .front()
            .is_some_and(|&t| now.duration_since(t) >= REBUILD_WINDOW)
        {
            self.0.pop_front();
        }
        self.0.push_back(now);
        self.0.len() <= REBUILD_LIMIT
    }
}

/// What the tuner's device line says of the system volume and whether the
/// loudness contour adds anything at it, when the profile has the contour
/// on.
fn contour_text(profile: &Profile, volume_db: Option<f32>) -> Option<String> {
    let contour = &profile.contour;
    if !contour.enabled {
        return None;
    }
    let Some(volume) = volume_db else {
        return Some("volume unknown, contour idle".into());
    };
    let below = (contour.reference_db - volume).max(0.0);
    let bass = (contour.bass_per_db * below).min(contour.max_bass_db);
    let treble = (contour.treble_per_db * below).min(contour.max_treble_db);
    let state = if bass > 0.0 || treble > 0.0 {
        "acting"
    } else {
        "idle"
    };
    // Adding 0 turns -0 into 0.
    let shown = volume.round() + 0.0;
    Some(format!("volume {shown} dB, contour {state}"))
}

/// When the session may next restart to change the apps it leaves alone,
/// if not yet at `now`: `APPS_REBUILD_GAP` after the last such restart.
fn next_apps_rebuild(last: Option<Instant>, now: Instant) -> Option<Instant> {
    last.map(|t| t + APPS_REBUILD_GAP)
        .filter(|&next| next > now)
}

/// Every process connected to the audio system, with its bundle ID if it
/// has one. IDs are read once per process and kept in `bundles`.
fn audio_processes(
    bundles: &mut HashMap<ObjectId, String>,
) -> Result<Vec<(ObjectId, Option<String>)>, String> {
    let processes = hal::process_objects()?;
    bundles.retain(|p, _| processes.contains(p));
    Ok(processes
        .into_iter()
        .map(|p| {
            let bundle = match bundles.get(&p) {
                Some(bundle) => Some(bundle.clone()),
                // One without an ID now is asked again next time.
                None => hal::bundle_id(p).ok().filter(|b| !b.is_empty()),
            };
            if let Some(bundle) = &bundle {
                bundles.insert(p, bundle.clone());
            }
            (p, bundle)
        })
        .collect())
}

/// Watches for the sign of a missing permission: other processes keep
/// running output, our callback keeps running, and the tap, which has never
/// delivered a non-zero sample, keeps delivering zeros.
///
/// Running output is not the same as making sound, so this only counts
/// unbroken stretches of it.
struct SilenceWatch {
    /// How long the sign has held without a break.
    held: Duration,
    last_poll: Instant,
    last_callbacks: u64,
}

impl SilenceWatch {
    fn new(now: Instant, callbacks: u64) -> Self {
        Self {
            held: Duration::ZERO,
            last_poll: now,
            last_callbacks: callbacks,
        }
    }

    fn due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_poll) >= PLAYING_POLL
    }

    /// Records a poll at `now`; true once the sign has held for
    /// `SILENCE_LIMIT`. A late poll counts as at most two poll intervals,
    /// since nothing is known about the time in between.
    fn exceeded(&mut self, now: Instant, callbacks: u64, others_playing: bool) -> bool {
        let since = now
            .saturating_duration_since(self.last_poll)
            .min(2 * PLAYING_POLL);
        let running = callbacks != self.last_callbacks;
        self.last_poll = now;
        self.last_callbacks = callbacks;
        if others_playing && running {
            self.held += since;
        } else {
            self.held = Duration::ZERO;
        }
        self.held >= SILENCE_LIMIT
    }
}

/// The output device to play on.
struct Target {
    device: ObjectId,
    name: String,
    uid: String,
    sample_rate: f64,
}

impl Target {
    fn current() -> Result<Self, String> {
        let device = hal::default_output_device()?.ok_or("there is no default output device")?;
        Ok(Self {
            device,
            name: hal::name(device)?,
            uid: hal::device_uid(device)?,
            sample_rate: hal::nominal_sample_rate(device)?,
        })
    }

    fn same_as(&self, other: &Self) -> bool {
        self.device == other.device && self.sample_rate == other.sample_rate
    }

    /// The device's rate as a whole number of Hz, checked against `profile`.
    fn validated_rate(&self, profile: &Profile, name: &str) -> Result<u32, String> {
        let hz = self.sample_rate.round();
        if !(1.0..=f64::from(u32::MAX)).contains(&hz) {
            return Err(format!(
                "{} reports an unusable sample rate of {} Hz",
                self.name, self.sample_rate
            ));
        }
        let hz = hz as u32;
        profile
            .validate(hz)
            .map_err(|e| format!("{name} does not fit {} at {hz} Hz: {e}", self.name))?;
        Ok(hz)
    }
}

/// The parts of a session's latency, in frames: what the HAL reports for
/// the aggregate device and the output device, and the chain's own. The
/// HAL's figures are what the devices declare, not a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Latency {
    rate: u32,
    /// The aggregate's IO buffer, which input and output each wait for.
    buffer: u32,
    /// The output device's latency, safety offset and stream latency.
    output: u32,
    output_safety: u32,
    output_stream: u32,
    /// The aggregate's input side, which carries the tap: its latency and
    /// safety offset, and the tap stream's latency.
    tap: u32,
    tap_safety: u32,
    tap_stream: u32,
    chain: usize,
}

impl Latency {
    /// What the HAL reports for a running session on `aggregate` and
    /// `target`, with a chain that lags `chain` frames.
    fn read(
        aggregate: &hal::Aggregate,
        target: &Target,
        rate: u32,
        chain: usize,
    ) -> Result<Self, hal::Error> {
        let (input, output) = (
            hal::kAudioObjectPropertyScopeInput,
            hal::kAudioObjectPropertyScopeOutput,
        );
        let stream_latency = |device, scope, last: bool| -> Result<u32, hal::Error> {
            let streams = hal::stream_ids(device, scope)?;
            let stream = if last {
                streams.last()
            } else {
                streams.first()
            };
            stream.map_or(Ok(0), |&s| hal::stream_latency(s))
        };
        Ok(Self {
            rate,
            buffer: hal::buffer_frames(aggregate.id)?,
            output: hal::device_latency(target.device, output)?,
            output_safety: hal::safety_offset(target.device, output)?,
            // The callback plays on the first output channels.
            output_stream: stream_latency(target.device, output, false)?,
            tap: hal::device_latency(aggregate.id, input)?,
            tap_safety: hal::safety_offset(aggregate.id, input)?,
            // The aggregate lists the tap's stream after its device's.
            tap_stream: stream_latency(aggregate.id, input, true)?,
            chain,
        })
    }

    /// From the tap to the output: a buffer in, a buffer out, and every
    /// other part once.
    fn frames(&self) -> u64 {
        2 * u64::from(self.buffer)
            + [
                self.output,
                self.output_safety,
                self.output_stream,
                self.tap,
                self.tap_safety,
                self.tap_stream,
            ]
            .into_iter()
            .map(u64::from)
            .sum::<u64>()
            + self.chain as u64
    }

    fn millis(&self) -> f64 {
        self.frames() as f64 * 1000.0 / f64::from(self.rate.max(1))
    }

    /// The total and its parts, leaving out those the HAL reports as 0.
    fn describe(&self) -> String {
        let mut parts = vec![format!("IO buffer 2 × {}", self.buffer)];
        for (name, frames) in [
            ("output latency", self.output as usize),
            ("output safety offset", self.output_safety as usize),
            ("output stream latency", self.output_stream as usize),
            ("tap latency", self.tap as usize),
            ("tap safety offset", self.tap_safety as usize),
            ("tap stream latency", self.tap_stream as usize),
            ("chain", self.chain),
        ] {
            if frames > 0 {
                parts.push(format!("{name} {frames}"));
            }
        }
        format!(
            "latency by the HAL's figures: {:.1} ms ({}; frames at {} Hz)",
            self.millis(),
            parts.join(", "),
            self.rate
        )
    }
}

/// Everything one run on one output device owns. Fields drop in order: IO
/// stops and the callback's state is freed before the listeners go, and the
/// aggregate goes before the tap it holds.
struct Session {
    _io: hal::IoProc<Engine>,
    _listeners: Vec<hal::Listener>,
    aggregate: hal::Aggregate,
    tap: hal::Tap,
    /// Where the callback reads the tap.
    tap_channels: TapChannels,
    handoff: Arc<Handoff<Processor>>,
    /// What the HAL reports, if it could be read.
    latency: Option<Latency>,
}

impl Session {
    /// Starts audio, calling `on_slow_start` if that takes long; `hint` says
    /// how to grant the permission.
    #[allow(clippy::too_many_arguments)]
    fn start(
        target: &Target,
        profile: &Profile,
        rate: u32,
        exclude: &[ObjectId],
        stats: &Arc<Stats>,
        controls: &Arc<Controls>,
        hint: &str,
        on_slow_start: impl FnOnce() + Send,
    ) -> Result<Self, String> {
        let tap = hal::Tap::global_stereo_excluding(exclude, "dps live", true)
            .map_err(|e| format!("{e}; if this is a permission problem, {hint}"))?;
        let aggregate = hal::Aggregate::new(&target.uid, &tap.uid()?)?;
        let tap_channels = Layout::read(&aggregate, &tap, target)?.tap;

        let handoff = Arc::new(Handoff::new());
        let chain = Processor::new(profile, rate, 2);
        let chain_latency = chain.latency_frames();
        let engine = Box::new(Engine::new(
            chain,
            rate,
            BLOCK_FRAMES,
            Arc::clone(&handoff),
            Arc::clone(controls),
            tap_channels,
            Arc::clone(stats),
        ));
        let stats_ptr = Arc::as_ptr(stats).cast_mut().cast::<c_void>();
        let wildcard = hal::kAudioObjectPropertyScopeWildcard;
        // SAFETY: `on_change` and `on_rebuild` only touch statics, and
        // `on_overload` gets `Stats`, which `run` keeps alive past every
        // session.
        let mut listeners = unsafe {
            vec![
                hal::Listener::add(
                    aggregate.id,
                    hal::kAudioDeviceProcessorOverload,
                    hal::GLOBAL,
                    Some(on_overload),
                    stats_ptr,
                    "watch for overloads",
                )?,
                hal::Listener::add(
                    aggregate.id,
                    hal::kAudioDevicePropertyIOStoppedAbnormally,
                    hal::GLOBAL,
                    Some(on_rebuild),
                    ptr::null_mut(),
                    "watch the aggregate device",
                )?,
                hal::Listener::add(
                    aggregate.id,
                    hal::kAudioDevicePropertyStreams,
                    wildcard,
                    Some(on_rebuild),
                    ptr::null_mut(),
                    "watch the aggregate device's streams",
                )?,
                hal::Listener::add(
                    aggregate.id,
                    hal::kAudioDevicePropertyStreamConfiguration,
                    wildcard,
                    Some(on_rebuild),
                    ptr::null_mut(),
                    "watch the aggregate device's stream layout",
                )?,
                hal::Listener::add(
                    target.device,
                    hal::kAudioDevicePropertyNominalSampleRate,
                    hal::GLOBAL,
                    Some(on_change),
                    ptr::null_mut(),
                    "watch the output sample rate",
                )?,
                hal::Listener::add(
                    target.device,
                    hal::kAudioDevicePropertyDeviceIsAlive,
                    hal::GLOBAL,
                    Some(on_change),
                    ptr::null_mut(),
                    "watch the output device",
                )?,
            ]
        };
        // A volume that cannot be watched is still read every `VOLUME_POLL`.
        listeners.extend(
            hal::volume_elements(target.device)
                .into_iter()
                .filter_map(|element| {
                    // SAFETY: `on_volume` only touches a static.
                    unsafe {
                        hal::Listener::add_on_element(
                            target.device,
                            hal::kAudioDevicePropertyVolumeDecibels,
                            hal::kAudioObjectPropertyScopeOutput,
                            element,
                            Some(on_volume),
                            ptr::null_mut(),
                            "watch the output volume",
                        )
                    }
                    .ok()
                }),
        );
        let io = notify_if_slow(on_slow_start, || {
            // SAFETY: `io_proc::<Processor>` expects an `Engine<Processor>`.
            unsafe { hal::IoProc::start(aggregate.id, Some(io_proc::<Processor>), engine) }
        })
        .map_err(|e| format!("{e}; if this is a permission problem, {hint}"))?;
        let latency = Latency::read(&aggregate, target, rate, chain_latency).ok();
        Ok(Self {
            _io: io,
            _listeners: listeners,
            aggregate,
            tap,
            tap_channels,
            handoff,
            latency,
        })
    }

    /// Why this running session does not fit `target` at `rate`, if it does
    /// not. Something can change between reading `target` and registering
    /// the listeners that would report it.
    fn mismatch(&self, target: &Target, rate: u32) -> Result<Option<String>, String> {
        if !Target::current()?.same_as(target) {
            return Ok(Some("the output changed while dps live started".into()));
        }
        if !hal::is_running(self.aggregate.id)? {
            return Ok(Some("audio IO stopped while dps live started".into()));
        }
        Ok(match Layout::read(&self.aggregate, &self.tap, target) {
            Err(e) => Some(e),
            Ok(layout) if layout.tap != self.tap_channels => {
                Some("the audio device's streams changed while dps live started".into())
            }
            Ok(layout) => layout
                .rates
                .iter()
                .find(|r| r.round() != f64::from(rate))
                .map(|r| format!("a stream runs at {r} Hz, not the {rate} Hz expected")),
        })
    }
}

/// The aggregate device's streams as the callback sees them.
struct Layout {
    tap: TapChannels,
    /// The sample rate of every input and output stream.
    rates: Vec<f64>,
}

impl Layout {
    fn read(aggregate: &hal::Aggregate, tap: &hal::Tap, target: &Target) -> Result<Self, String> {
        let tap_format = tap.format()?;
        let inputs = hal::streams(aggregate.id, hal::kAudioObjectPropertyScopeInput)?;
        let outputs = hal::streams(aggregate.id, hal::kAudioObjectPropertyScopeOutput)?;
        let device_inputs = hal::streams(target.device, hal::kAudioObjectPropertyScopeInput)?;
        for format in inputs.iter().chain(&outputs).map(|s| &s.format) {
            check_float32(format)?;
        }
        check_float32(&tap_format)?;
        if outputs.is_empty() {
            return Err(format!("{} has no output streams", target.name));
        }
        let tap = locate_tap(
            &inputs,
            &device_inputs,
            tap_format.mChannelsPerFrame as usize,
        )?;
        let rates = inputs
            .iter()
            .chain(&outputs)
            .map(|s| s.format.mSampleRate)
            .collect();
        Ok(Self { tap, rates })
    }
}

/// Where the tap's channels are among the aggregate's input channels.
///
/// A tap has one input stream (CATapDescription.h), and the aggregate lists
/// it after its sub-device's input streams. That order is observed, not
/// documented, and getting it wrong would play a microphone as system audio,
/// so the layout must be exactly that: the device's own input streams, with
/// the same channel counts and terminal types, then one stream with the
/// tap's channel count.
fn locate_tap(
    inputs: &[hal::Stream],
    device_inputs: &[hal::Stream],
    tap_channels: usize,
) -> Result<TapChannels, String> {
    let channels = |s: &hal::Stream| s.format.mChannelsPerFrame as usize;
    let (device_part, tap_part) = inputs.split_at(inputs.len().saturating_sub(1));
    let fits = tap_channels > 0
        && inputs.len() == device_inputs.len() + 1
        && device_part
            .iter()
            .zip(device_inputs)
            .all(|(a, d)| channels(a) == channels(d) && a.terminal == d.terminal)
        && tap_part.iter().map(channels).eq([tap_channels]);
    if !fits {
        let counts = |s: &[hal::Stream]| s.iter().map(channels).collect::<Vec<_>>();
        return Err(format!(
            "unexpected aggregate device layout: input streams of {:?} channels for a device \
             with input streams of {:?} channels and a {tap_channels}-channel tap",
            counts(inputs),
            counts(device_inputs),
        ));
    }
    Ok(TapChannels {
        first: device_part.iter().map(channels).sum(),
        count: tap_channels,
    })
}

/// Runs `start`, calling `on_slow` if it takes longer than `START_NOTICE`.
fn notify_if_slow<T>(on_slow: impl FnOnce() + Send, start: impl FnOnce() -> T) -> T {
    let (done, waiting) = mpsc::channel::<()>();
    thread::scope(|s| {
        s.spawn(move || {
            if waiting.recv_timeout(START_NOTICE) == Err(RecvTimeoutError::Timeout) {
                on_slow();
            }
        });
        let result = start();
        drop(done);
        result
    })
}

/// The callback reads and writes raw, packed 32-bit f32 samples, so anything
/// else would be played as noise.
fn check_float32(f: &AudioStreamBasicDescription) -> Result<(), String> {
    let channels_per_buffer = if f.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
        1
    } else {
        f.mChannelsPerFrame
    };
    // CoreAudioBaseTypes.h: packed is implied when the bytes per frame
    // match the bits per channel.
    let packed = f.mFormatFlags & kAudioFormatFlagIsPacked != 0
        || f.mBytesPerFrame == 4 * channels_per_buffer;
    let ok = f.mFormatID == kAudioFormatLinearPCM
        && f.mFormatFlags & kAudioFormatFlagIsFloat != 0
        && f.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
        && f.mBitsPerChannel == 32
        && packed;
    if ok {
        Ok(())
    } else {
        Err(format!(
            "unsupported stream format (format {:#x}, flags {:#x}, {} bits, {} bytes per frame); \
             only packed 32-bit float is supported",
            f.mFormatID, f.mFormatFlags, f.mBitsPerChannel, f.mBytesPerFrame
        ))
    }
}

/// The processing the callback runs on each block.
trait Chain {
    fn process(&mut self, block: &mut [f32]);

    /// Takes over whatever running state of `from`, the chain this one
    /// replaces, keeps the switch smooth. Called on the callback, so it may
    /// not allocate or free.
    fn adopt(&mut self, _from: &Self) {}

    /// Frames by which the output lags the input.
    fn latency_frames(&self) -> usize {
        0
    }

    /// The system volume in dB below maximum, `None` when unknown. Called
    /// on the callback, so it may not allocate or free.
    fn set_volume_db(&mut self, _volume_db: Option<f32>) {}
    /// The largest gain reductions since the last call, which starts them
    /// over, and the leveler's gain now. No allocation.
    fn meters(&mut self) -> dps::Meters {
        dps::Meters::default()
    }
}

impl Chain for Processor {
    fn process(&mut self, block: &mut [f32]) {
        Processor::process(self, block);
    }

    fn adopt(&mut self, from: &Self) {
        self.adopt_levels(from);
    }

    fn latency_frames(&self) -> usize {
        Processor::latency_frames(self)
    }

    fn set_volume_db(&mut self, volume_db: Option<f32>) {
        Processor::set_volume_db(self, volume_db);
    }

    fn meters(&mut self) -> dps::Meters {
        Processor::meters(self)
    }
}

/// The capture check's chain: it plays nothing, so the apps it taps without
/// muting are not heard twice.
struct Silent;

impl Chain for Silent {
    fn process(&mut self, block: &mut [f32]) {
        block.fill(0.0);
    }
}

/// What a brief, unmuted system audio tap showed.
pub enum Capture {
    /// Audio ran; whether the tap delivered sound, and whether other apps
    /// had audio output running meanwhile.
    Ran {
        heard: bool,
        others_playing: bool,
    },
    /// Starting audio did not finish in time: macOS is asking for
    /// permission.
    Waiting,
    Failed(String),
}

/// How long the capture check gives audio to start, then to run.
const CHECK_START: Duration = Duration::from_secs(3);
const CHECK_RUN: Duration = Duration::from_millis(500);

/// Taps all system audio on the default output for half a second without
/// muting or playing anything, to see whether capturing it works.
pub fn check_capture() -> Capture {
    let (done, result) = mpsc::channel();
    // On its own thread, so a start blocked on the permission prompt does
    // not block the caller; ending the process cleans up after it.
    thread::spawn(move || {
        let _ = done.send(run_capture_check());
    });
    match result.recv_timeout(CHECK_START + CHECK_RUN) {
        Ok(Ok(capture)) => capture,
        Ok(Err(e)) => Capture::Failed(e),
        Err(_) => Capture::Waiting,
    }
}

fn run_capture_check() -> Result<Capture, String> {
    hal::notify_on_hal_thread()?;
    let own = hal::process_object(std::process::id() as i32)?
        .ok_or("cannot find this process's Core Audio object to exclude it from the tap")?;
    let target = Target::current()?;
    let tap = hal::Tap::global_stereo_excluding(&[own], "dps doctor", false)?;
    let aggregate = hal::Aggregate::new(&target.uid, &tap.uid()?)?;
    let tap_channels = Layout::read(&aggregate, &tap, &target)?.tap;
    let stats = Arc::new(Stats::default());
    let engine = Box::new(Engine::new(
        Silent,
        target.sample_rate.round() as u32,
        BLOCK_FRAMES,
        Arc::new(Handoff::new()),
        Arc::default(),
        tap_channels,
        Arc::clone(&stats),
    ));
    // SAFETY: `io_proc::<Silent>` expects an `Engine<Silent>`.
    let io = unsafe { hal::IoProc::start(aggregate.id, Some(io_proc::<Silent>), engine)? };
    let end = Instant::now() + CHECK_RUN;
    let mut others_playing = false;
    while Instant::now() < end && !stats.heard.load(Relaxed) {
        others_playing |= hal::others_playing(own)?;
        thread::sleep(POLL);
    }
    drop(io);
    if stats.callbacks.load(Relaxed) == 0 {
        return Err("audio started but the tap delivered nothing".into());
    }
    Ok(Capture::Ran {
        heard: stats.heard.load(Relaxed),
        others_playing,
    })
}

/// The default output device's name and sample rate.
pub fn default_output() -> Result<(String, u32), String> {
    let target = Target::current()?;
    Ok((target.name, target.sample_rate.round() as u32))
}

/// A device with output streams.
pub struct OutputDevice {
    pub uid: String,
    pub name: String,
    /// The default output device.
    pub default: bool,
}

/// The devices with output streams, in the HAL's order. One that goes away
/// while they are read is left out.
pub fn output_devices() -> Result<Vec<OutputDevice>, String> {
    let default = hal::default_output_device()?;
    Ok(hal::devices()?
        .into_iter()
        .filter_map(|device| {
            let streams = hal::stream_ids(device, hal::kAudioObjectPropertyScopeOutput).ok()?;
            (!streams.is_empty()).then_some(())?;
            Some(OutputDevice {
                uid: hal::device_uid(device).ok()?,
                name: hal::name(device).ok()?,
                default: Some(device) == default,
            })
        })
        .collect())
}

/// The bundle ID of each process connected to the audio system that has
/// one, and whether it is running audio output.
pub fn audio_apps() -> Result<Vec<(String, bool)>, String> {
    Ok(hal::process_objects()?
        .into_iter()
        .filter_map(|p| {
            let bundle = hal::bundle_id(p).ok().filter(|b| !b.is_empty())?;
            Some((bundle, hal::runs_output(p)))
        })
        .collect())
}

/// Whether any process but `pid`, which may be a live run's, has audio
/// output running.
pub fn others_playing(pid: u32) -> Result<bool, String> {
    let own = hal::process_object(pid as i32)?.unwrap_or(hal::UNKNOWN);
    Ok(hal::others_playing(own)?)
}

/// The audio callback's state.
struct Engine<C = Processor> {
    mixer: Mixer<C>,
    scratch: Vec<f32>,
    tap: TapChannels,
    stats: Arc<Stats>,
    /// Set after a non-finite sample or a panic; the output stays silent.
    failed: bool,
}

impl<C: Chain> Engine<C> {
    /// An engine that runs the chain on blocks of at most `block_frames`.
    fn new(
        chain: C,
        rate: u32,
        block_frames: usize,
        handoff: Arc<Handoff<C>>,
        controls: Arc<Controls>,
        tap: TapChannels,
        stats: Arc<Stats>,
    ) -> Self {
        Self {
            mixer: Mixer::new(Box::new(chain), rate, block_frames, handoff, controls),
            scratch: vec![0.0; 2 * block_frames],
            tap,
            stats,
            failed: false,
        }
    }

    fn render(&mut self, input: &HalBuffers<'_>, output: &mut HalBuffers<'_>) {
        if self.failed {
            render::silence(output);
            return;
        }
        let mixer = &mut self.mixer;
        let outcome = render::render(input, self.tap, output, &mut self.scratch, &mut |block| {
            mixer.process(block)
        });
        let stats = &*self.stats;
        if outcome.clamped > 0 {
            stats.clamped.fetch_add(outcome.clamped as u64, Relaxed);
        }
        if outcome.short_input {
            stats.short.fetch_add(1, Relaxed);
        }
        if outcome.heard && !stats.heard.load(Relaxed) {
            stats.heard.store(true, Relaxed);
        }
        if outcome.non_finite {
            self.failed = true;
            stats.non_finite.store(true, Relaxed);
        }
    }
}

/// An `AudioBufferList` from the HAL viewed as `render::Buffers`.
struct HalBuffers<'a> {
    buffers: &'a [AudioBuffer],
}

impl HalBuffers<'_> {
    /// # Safety
    /// `list` must be a valid `AudioBufferList` for the lifetime chosen.
    unsafe fn new(list: NonNull<AudioBufferList>) -> Self {
        let list = list.as_ptr();
        // SAFETY: `mBuffers` is a C flexible array of `mNumberBuffers`.
        unsafe {
            let count = (*list).mNumberBuffers as usize;
            let first = ptr::addr_of!((*list).mBuffers).cast::<AudioBuffer>();
            Self {
                buffers: slice::from_raw_parts(first, count),
            }
        }
    }

    fn data(&self, i: usize) -> Option<(*mut f32, usize)> {
        let buffer = &self.buffers[i];
        let data = buffer.mData.cast::<f32>();
        (!data.is_null() && data.is_aligned())
            .then_some((data, buffer.mDataByteSize as usize / size_of::<f32>()))
    }
}

impl Buffers for HalBuffers<'_> {
    fn count(&self) -> usize {
        self.buffers.len()
    }

    fn channels(&self, i: usize) -> usize {
        self.buffers[i].mNumberChannels as usize
    }

    fn samples(&self, i: usize) -> Option<&[f32]> {
        // SAFETY: the HAL's buffer, valid for the callback.
        self.data(i)
            .map(|(data, len)| unsafe { slice::from_raw_parts(data, len) })
    }
}

impl BuffersMut for HalBuffers<'_> {
    fn samples_mut(&mut self, i: usize) -> Option<&mut [f32]> {
        // SAFETY: the HAL's output buffer, valid and ours for the callback.
        self.data(i)
            .map(|(data, len)| unsafe { slice::from_raw_parts_mut(data, len) })
    }
}

/// The IOProc. Runs on the HAL's real-time thread: no allocation, locks or
/// I/O here or in anything it calls.
unsafe extern "C-unwind" fn io_proc<C: Chain>(
    _device: ObjectId,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the session's `Engine<C>`, alive while IO runs and
    // touched by nothing else; the lists are valid for this call.
    let (engine, input, mut output) = unsafe {
        (
            &mut *client.cast::<Engine<C>>(),
            HalBuffers::new(input),
            HalBuffers::new(output),
        )
    };
    engine.stats.callbacks.fetch_add(1, Relaxed);
    if panic::catch_unwind(AssertUnwindSafe(|| engine.render(&input, &mut output))).is_err() {
        engine.failed = true;
        engine.stats.panicked.store(true, Relaxed);
        render::silence(&mut output);
    }
    0
}

unsafe extern "C-unwind" fn on_change(
    _object: ObjectId,
    _count: u32,
    _addresses: NonNull<hal::PropertyAddress>,
    _client: *mut c_void,
) -> i32 {
    CHANGED.store(true, Relaxed);
    0
}

unsafe extern "C-unwind" fn on_volume(
    _object: ObjectId,
    _count: u32,
    _addresses: NonNull<hal::PropertyAddress>,
    _client: *mut c_void,
) -> i32 {
    VOLUME.store(true, Relaxed);
    0
}

unsafe extern "C-unwind" fn on_processes(
    _object: ObjectId,
    _count: u32,
    _addresses: NonNull<hal::PropertyAddress>,
    _client: *mut c_void,
) -> i32 {
    PROCESSES.store(true, Relaxed);
    0
}

unsafe extern "C-unwind" fn on_rebuild(
    _object: ObjectId,
    _count: u32,
    _addresses: NonNull<hal::PropertyAddress>,
    _client: *mut c_void,
) -> i32 {
    REBUILD.store(true, Relaxed);
    0
}

/// Called on the IO thread when a cycle misses its deadline.
unsafe extern "C-unwind" fn on_overload(
    _object: ObjectId,
    _count: u32,
    _addresses: NonNull<hal::PropertyAddress>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: registered with a pointer to the run's `Stats`.
    let stats = unsafe { &*client.cast::<Stats>() };
    stats.overloads.fetch_add(1, Relaxed);
    0
}

unsafe extern "C" {
    fn signal(signum: c_int, handler: usize) -> usize;
    fn raise(signum: c_int) -> c_int;
    fn tcgetpgrp(fd: c_int) -> i32;
    fn getpgrp() -> i32;
}

const SIG_ERR: usize = usize::MAX;
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;
const STDIN: c_int = 0;

/// Writes `message` as a line on stderr; not eprintln!, which panics when
/// the terminal has been closed.
fn print_line(message: &str) {
    let _ = writeln!(io::stderr(), "dps live: {}", tui::printable(message));
}

/// Whether this process is in the foreground of the terminal on `fd`, so
/// it may change the terminal's mode and read keys without being stopped.
fn in_foreground(fd: c_int) -> bool {
    // SAFETY: plain calls; tcgetpgrp fails for anything but this process's
    // controlling terminal.
    let group = unsafe { tcgetpgrp(fd) };
    group != -1 && group == unsafe { getpgrp() }
}

extern "C" fn on_signal(signum: c_int) {
    if SIGNALLED.swap(true, Relaxed) {
        // A second signal ends the process at once in case shutdown hangs;
        // the OS then reclaims the tap and the aggregate device. The
        // terminal goes back first, as the tuner would have put it.
        tui::restore_in_signal_handler();
        // SAFETY: async-signal-safe calls; the signal is blocked until this
        // handler returns, and then takes its default action.
        unsafe {
            signal(signum, SIG_DFL);
            raise(signum);
        }
    } else {
        STOP.store(true, Relaxed);
    }
}

fn install_signal_handlers() -> Result<(), String> {
    const SIGHUP: c_int = 1;
    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    for signum in [SIGHUP, SIGINT, SIGTERM] {
        // SAFETY: the handler only touches atomics and makes
        // async-signal-safe calls.
        if unsafe { signal(signum, on_signal as extern "C" fn(c_int) as usize) } == SIG_ERR {
            return Err("cannot install the Ctrl-C handler".into());
        }
    }
    Ok(())
}

/// Ignores the signals that stop a process until dropped: Ctrl-Z
/// (SIGTSTP), and reading from or changing the terminal while in the
/// background (SIGTTIN, SIGTTOU). A stopped process keeps its tap, which
/// can leave every other app muted.
struct NoSuspend {
    previous: [usize; 3],
}

impl NoSuspend {
    const SIGNALS: [c_int; 3] = [18, 21, 22];

    fn new() -> Result<Self, String> {
        let mut previous = [SIG_DFL; 3];
        for (signum, previous) in Self::SIGNALS.into_iter().zip(&mut previous) {
            // SAFETY: SIG_IGN installs no code.
            *previous = unsafe { signal(signum, SIG_IGN) };
            if *previous == SIG_ERR {
                return Err("cannot ignore Ctrl-Z".into());
            }
        }
        Ok(Self { previous })
    }
}

impl Drop for NoSuspend {
    fn drop(&mut self) {
        for (signum, previous) in Self::SIGNALS.into_iter().zip(self.previous) {
            // SAFETY: restores the disposition `new` replaced.
            unsafe { signal(signum, previous) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dps::profile::{CompressorParams, LimiterParams};
    use objc2_core_audio_types::{
        kAudioFormatFlagIsAlignedHigh, kAudioFormatFlagIsSignedInteger, kAudioFormatMPEG4AAC,
    };
    use std::fs::File;
    use std::mem;
    use std::mem::{align_of, offset_of};
    use std::os::fd::AsRawFd;

    /// Every stage at unity and the limiter off, so the chain passes samples
    /// through exactly.
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

    /// An `AudioBufferList` of `N` buffers, laid out as the HAL passes it.
    #[repr(C)]
    struct FakeList<const N: usize> {
        count: u32,
        buffers: [AudioBuffer; N],
    }

    fn list<const N: usize>(buffers: [(u32, &mut [f32]); N]) -> FakeList<N> {
        FakeList {
            count: N as u32,
            buffers: buffers.map(|(channels, data)| AudioBuffer {
                mNumberChannels: channels,
                mDataByteSize: size_of_val(data) as u32,
                mData: data.as_mut_ptr().cast(),
            }),
        }
    }

    /// Lets a test script what the callback's chain does.
    impl<F: FnMut(&mut [f32])> Chain for F {
        fn process(&mut self, block: &mut [f32]) {
            self(block);
        }
    }

    fn wired<C: Chain>(
        chain: C,
        scratch_frames: usize,
        handoff: &Arc<Handoff<C>>,
        controls: &Arc<Controls>,
    ) -> Engine<C> {
        Engine::new(
            chain,
            48_000,
            scratch_frames,
            Arc::clone(handoff),
            Arc::clone(controls),
            TapChannels { first: 0, count: 2 },
            Arc::default(),
        )
    }

    fn scripted<C: Chain>(chain: C, scratch_frames: usize) -> Engine<C> {
        wired(
            chain,
            scratch_frames,
            &Arc::new(Handoff::new()),
            &Arc::default(),
        )
    }

    fn engine(profile: &Profile, scratch_frames: usize) -> Engine {
        scripted(Processor::new(profile, 48_000, 2), scratch_frames)
    }

    /// Calls the IOProc the way the HAL does.
    fn call<C: Chain, const I: usize, const O: usize>(
        engine: &mut Engine<C>,
        input: &mut FakeList<I>,
        output: &mut FakeList<O>,
    ) {
        // SAFETY: AudioTimeStamp is plain data, valid when zeroed.
        let mut time: AudioTimeStamp = unsafe { std::mem::zeroed() };
        let time = NonNull::from(&mut time);
        // SAFETY: valid lists and an Engine, as the HAL would pass them.
        unsafe {
            io_proc::<C>(
                0,
                time,
                NonNull::from(input).cast(),
                time,
                NonNull::from(output).cast(),
                time,
                ptr::from_mut(engine).cast(),
            )
        };
    }

    #[test]
    fn fake_list_matches_the_c_layout() {
        // C: struct AudioBuffer { UInt32; UInt32; void *; } and
        // struct AudioBufferList { UInt32; AudioBuffer mBuffers[1]; }.
        assert_eq!(
            (
                offset_of!(AudioBuffer, mNumberChannels),
                offset_of!(AudioBuffer, mDataByteSize),
                offset_of!(AudioBuffer, mData),
                size_of::<AudioBuffer>(),
            ),
            (0, 4, 8, 16)
        );
        assert_eq!(size_of::<FakeList<1>>(), size_of::<AudioBufferList>());
        assert_eq!(align_of::<FakeList<1>>(), align_of::<AudioBufferList>());
        assert_eq!(
            offset_of!(FakeList<1>, count),
            offset_of!(AudioBufferList, mNumberBuffers)
        );
        assert_eq!(
            offset_of!(FakeList<1>, buffers),
            offset_of!(AudioBufferList, mBuffers)
        );
        // Later buffers follow the first, as in C's flexible array.
        assert_eq!(
            offset_of!(FakeList<3>, buffers),
            offset_of!(AudioBufferList, mBuffers)
        );
        assert_eq!(
            size_of::<FakeList<3>>(),
            offset_of!(AudioBufferList, mBuffers) + 3 * size_of::<AudioBuffer>()
        );
    }

    #[test]
    fn callback_plays_the_tap_on_the_first_two_output_channels() {
        // Interleaved stereo in, three non-interleaved channels out, and a
        // scratch buffer smaller than the callback so it runs in blocks.
        let frames = 100;
        let mut tap: Vec<f32> = (0..2 * frames)
            .map(|i| (i as f32 * 0.37).sin() * 0.5)
            .collect();
        let left: Vec<f32> = tap.iter().step_by(2).copied().collect();
        let right: Vec<f32> = tap.iter().skip(1).step_by(2).copied().collect();
        let mut outs = [vec![0.25; frames], vec![0.25; frames], vec![0.25; frames]];
        let [o0, o1, o2] = &mut outs;
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(1, &mut o0[..]), (1, &mut o1[..]), (1, &mut o2[..])]);
        let mut engine = engine(&flat(), 32);
        call(&mut engine, &mut input, &mut output);

        assert_eq!(outs[0], left);
        assert_eq!(outs[1], right);
        assert!(outs[2].iter().all(|&x| x == 0.0));
        let stats = &engine.stats;
        assert!(stats.heard.load(Relaxed));
        assert_eq!(stats.clamped.load(Relaxed), 0);
        assert_eq!(stats.short.load(Relaxed), 0);
        assert!(!stats.non_finite.load(Relaxed));
    }

    #[test]
    fn callback_fades_in_a_posted_chain_and_hands_the_old_one_back() {
        // Each call plays 2048 frames, more than the 1440 of a 30 ms fade at
        // 48 kHz; it returns the first and last left samples.
        let run = |engine: &mut Engine| {
            let mut tap = [0.5f32; 2 * 2048];
            let mut out = [0.0f32; 2 * 2048];
            let mut input = list([(2, &mut tap[..])]);
            let mut output = list([(2, &mut out[..])]);
            call(engine, &mut input, &mut output);
            (out[0], out[2 * 2047])
        };
        let handoff = Arc::new(Handoff::new());
        let mut engine = wired(
            Processor::new(&flat(), 48_000, 2),
            4096,
            &handoff,
            &Arc::default(),
        );
        assert_eq!(run(&mut engine), (0.5, 0.5));

        let mut quieter = flat();
        quieter.output_gain_db = -20.0;
        handoff.post(0, Processor::new(&quieter, 48_000, 2));
        let (first, last) = run(&mut engine);
        // The first frame is almost all the old chain, the last all the new.
        assert!((first - (0.5 - 0.45 / 1440.0)).abs() < 1e-6, "{first}");
        assert!((last - 0.05).abs() < 1e-6, "{last}");
        assert!(handoff.lanes[0].pending.load(Relaxed).is_null());
        assert!(!handoff.lanes[0].retired.load(Relaxed).is_null());

        // Until the old chain is collected the next one waits.
        handoff.post(0, Processor::new(&flat(), 48_000, 2));
        let (first, last) = run(&mut engine);
        assert!((first - 0.05).abs() < 1e-6 && (last - 0.05).abs() < 1e-6);
        handoff.collect();
        assert!(handoff.lanes[0].retired.load(Relaxed).is_null());
        let (first, last) = run(&mut engine);
        assert!((first - (0.05 + 0.45 / 1440.0)).abs() < 1e-6, "{first}");
        assert_eq!(last, 0.5);
    }

    #[test]
    fn bypass_through_the_callback_is_clamped_and_counted() {
        // A chain 12 dB up makes the matched gain 12 dB too, so 0.5 dry
        // plays at about 2 and is clamped like the processed signal.
        let mut profile = flat();
        profile.output_gain_db = 12.0;
        let controls = Arc::new(Controls::default());
        let mut engine = wired(
            Processor::new(&profile, 48_000, 2),
            4096,
            &Arc::new(Handoff::new()),
            &controls,
        );
        let frames = 4096;
        let mut tap = vec![0.5; 2 * frames];
        let mut out = vec![0.0; 2 * frames];
        for _ in 0..24 {
            let mut input = list([(2, &mut tap[..])]);
            let mut output = list([(2, &mut out[..])]);
            call(&mut engine, &mut input, &mut output);
        }
        let gain = controls.dry_gain();
        assert!((gain - 3.981).abs() < 0.01, "{gain}");
        controls.bypass.store(true, Relaxed);
        let clamped = engine.stats.clamped.load(Relaxed);
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        call(&mut engine, &mut input, &mut output);
        assert!(out.iter().all(|&x| x == 1.0));
        assert_eq!(
            engine.stats.clamped.load(Relaxed) - clamped,
            2 * frames as u64
        );
    }

    #[test]
    fn handoff_frees_every_chain_exactly_once() {
        struct Counted(Arc<AtomicU64>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Relaxed);
            }
        }
        let drops = Arc::new(AtomicU64::new(0));
        let chain = || Counted(Arc::clone(&drops));
        let handoff = Handoff::new();
        let mut current = Box::new(chain());
        let mut swap_in = |handoff: &Handoff<Counted>| {
            let Some(Offer::Chain(taken)) = handoff.take(0) else {
                panic!("no chain offered");
            };
            handoff.retire(0, std::mem::replace(&mut current, taken));
        };

        handoff.post(0, chain());
        handoff.post(0, chain()); // replaces the untaken one
        assert_eq!(drops.load(Relaxed), 1);
        swap_in(&handoff);
        handoff.collect();
        assert_eq!(drops.load(Relaxed), 2);
        handoff.post(0, chain());
        swap_in(&handoff);
        handoff.post(0, chain());
        // Nothing more is taken until the one handed back is collected.
        assert!(handoff.take(0).is_none());
        // The other lane is its own: a chain posted there, then replaced by
        // word to play the other's chain, which frees it; and asking twice
        // frees nothing.
        handoff.post(1, chain());
        handoff.clear(1);
        assert_eq!(drops.load(Relaxed), 3);
        assert!(matches!(handoff.take(1), Some(Offer::Empty)));
        handoff.clear(1);
        handoff.clear(1);
        handoff.post(1, chain());
        drop(handoff); // one pending in each lane, one handed back
        assert_eq!(drops.load(Relaxed), 6);
        drop(current);
        assert_eq!(drops.load(Relaxed), 7);
    }

    #[test]
    fn profile_file_reports_each_save_once() {
        let path =
            std::env::temp_dir().join(format!("dps-live-profile-{}.toml", std::process::id()));
        let save = |text: &str, age_secs: u64| {
            std::fs::write(&path, text).unwrap();
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + age_secs);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        };
        save("output_gain_db = -3.0\n", 0);
        let mut file = ProfileFile::new(&path);
        assert!(file.changed(48_000).is_none());

        save("output_gain_db = -6.0\n", 1);
        let profile = file.changed(48_000).unwrap().unwrap();
        assert_eq!(profile.output_gain_db, -6.0);
        assert!(file.changed(48_000).is_none());

        save("output_gain_db = 500.0\n", 2);
        let error = file.changed(48_000).unwrap().unwrap_err();
        assert!(error.contains("output_gain_db"), "{error}");
        assert!(file.changed(48_000).is_none());

        std::fs::remove_file(&path).unwrap();
        assert!(file.changed(48_000).is_none());
    }

    #[test]
    fn an_empty_profile_file_waits_for_the_next_save() {
        // An editor that truncates before it writes leaves an empty or blank
        // file for a moment; that is not a profile of all defaults.
        let path = std::env::temp_dir().join(format!("dps-live-blank-{}.toml", std::process::id()));
        let save = |text: &str, secs: u64| {
            fs::write(&path, text).unwrap();
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs);
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        };
        save("output_gain_db = -3.0\n", 0);
        let mut file = ProfileFile::new(&path);
        save("", 1);
        assert!(file.changed(48_000).is_none());
        save("  \n\t\n", 2);
        assert!(file.changed(48_000).is_none());
        // The finished save, even within the same second as the blank one.
        save("output_gain_db = -6.0\n", 2);
        let profile = file.changed(48_000).unwrap().unwrap();
        assert_eq!(profile.output_gain_db, -6.0);
        fs::remove_file(&path).unwrap();
    }

    /// A directory for one test, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dps-live-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn same_file_compares_the_files_paths_lead_to() {
        let dir = Scratch::new("same");
        let real = dir.0.join("real.toml");
        fs::write(&real, "").unwrap();
        let link = dir.0.join("link.toml");
        std::os::unix::fs::symlink("real.toml", &link).unwrap();
        let other = dir.0.join("other.toml");
        fs::write(&other, "").unwrap();
        assert!(same_file(&link, &real));
        assert!(same_file(&dir.0.join(".").join("real.toml"), &real));
        assert!(!same_file(&other, &real));
        assert!(!same_file(&dir.0.join("missing.toml"), &real));
    }

    /// What a background run would report, in order.
    #[derive(Default)]
    struct Reports(std::sync::Mutex<Vec<String>>);

    impl Reports {
        fn take(&self) -> Vec<String> {
            mem::take(&mut *self.0.lock().unwrap())
        }

        fn push(&self, report: String) {
            self.0.lock().unwrap().push(report);
        }
    }

    impl Report for Reports {
        fn waiting_for_permission(&self) {
            self.push("waiting".into());
        }
        fn running(&self, device: &str, rate: u32, profile: &str, latency: Option<&str>) {
            let latency = latency.map(|l| format!("; {l}")).unwrap_or_default();
            self.push(format!(
                "running on {device} at {rate} with {profile}{latency}"
            ));
        }
        fn heard(&self) {
            self.push("heard".into());
        }
        fn reload_failed(&self, message: &str) {
            self.push(format!("reload failed: {message}"));
        }
        fn reloaded(&self) {
            self.push("reloaded".into());
        }
        fn excluded(&self, apps: &[String]) {
            self.push(format!("excluded [{}]", apps.join(", ")));
        }
    }

    /// A run watching and saving to `settings.toml` in `dir`, without audio:
    /// the chains it posts wait in the returned handoff.
    fn live_in(dir: &Scratch, text: &str, background: Option<Arc<Reports>>) -> (Live, PathBuf) {
        let path = dir.0.join("settings.toml");
        stamp(&path, text, 0);
        (Live::new(options_in(&path, None, background)), path)
    }

    /// Options for a run on `path`, as `live_in` makes it, listening on
    /// `control` if given.
    fn options_in(
        path: &Path,
        control: Option<PathBuf>,
        background: Option<Arc<Reports>>,
    ) -> Options {
        Options {
            profile: super::super::load_profile(path).unwrap(),
            name: "settings.toml".into(),
            watch: Some(path.to_owned()),
            save_path: path.to_owned(),
            config: None,
            settings_file: Some(path.to_owned()),
            follow_devices: false,
            control,
            background: background.map(|r| r as Arc<dyn Report>),
        }
    }

    /// Writes `text` to `path`, dated `secs` after a fixed time, so every
    /// write is a new save.
    fn stamp(path: &Path, text: &str, secs: u64) {
        fs::write(path, text).unwrap();
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    /// The chain in what the callback would take, if it is one.
    fn chain(offer: Option<Offer<Processor>>) -> Option<Processor> {
        match offer? {
            Offer::Chain(chain) => Some(*chain),
            Offer::Empty => panic!("asked to play the other slot's chain"),
        }
    }

    /// Whether `chain` processes like a new chain for `profile`.
    fn plays(mut chain: Processor, profile: &Profile) -> bool {
        let input: Vec<f32> = (0..4000).map(|i| (i as f32 * 0.013).sin() * 0.7).collect();
        let (mut got, mut want) = (input.clone(), input);
        chain.process(&mut got);
        Processor::new(profile, 48_000, 2).process(&mut want);
        got == want
    }

    #[test]
    fn keys_go_through_the_tuner_to_the_audio() {
        let dir = Scratch::new("keys");
        let (mut live, path) = live_in(&dir, "output_gain_db = -3.0\n", None);
        let (handoff, controls) = (Handoff::new(), Controls::default());
        // Down past the leveler's six settings to the input gain.
        for _ in 0..6 {
            assert!(!live.step(Some(Key::Down), 48_000, &handoff, &controls));
        }
        assert!(handoff.take(0).is_none() && handoff.take(1).is_none());
        assert!(!live.step(Some(Key::Right), 48_000, &handoff, &controls));
        let want = Profile {
            input_gain_db: 0.5,
            output_gain_db: -3.0,
            ..Profile::default()
        };
        assert_eq!(live.tuner.profile(), &want);
        assert!(plays(chain(handoff.take(0)).unwrap(), &want));
        // B, still as saved, now differs from A, so it gets its own chain
        // to be compared with.
        let saved = Profile {
            output_gain_db: -3.0,
            ..Profile::default()
        };
        assert!(plays(chain(handoff.take(1)).unwrap(), &saved));
        // Bypass goes to the callback's controls, with no new chain.
        assert!(!live.step(Some(Key::Char('b')), 48_000, &handoff, &controls));
        assert!(controls.bypass.load(Relaxed));
        assert!(handoff.take(0).is_none() && handoff.take(1).is_none());
        // Saving writes the file and takes that save as seen, so it is not
        // reloaded as if someone else had written it.
        assert!(!live.step(Some(Key::Char('s')), 48_000, &handoff, &controls));
        assert_eq!(super::super::load_profile(&path).unwrap(), want);
        let saved = live.tuner.message().to_owned();
        assert!(saved.starts_with("saved: rewrote "), "{saved}");
        assert!(!live.step(None, 48_000, &handoff, &controls));
        assert!(handoff.take(0).is_none() && handoff.take(1).is_none());
        assert_eq!(live.tuner.message(), saved);
        // Nothing unsaved: q quits.
        assert!(live.step(Some(Key::Char('q')), 48_000, &handoff, &controls));
    }

    #[test]
    fn a_bands_frequency_and_q_reach_the_audio_one_chain_per_change() {
        let dir = Scratch::new("eq");
        let boosted = "[eq]\ngains_db = [0.0, 0.0, 0.0, 9.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]\n";
        let (mut live, path) = live_in(&dir, boosted, None);
        let original = live.tuner.profile().clone();
        let (handoff, controls) = (Handoff::new(), Controls::default());
        let press = |live: &mut Live, key| {
            assert!(!live.step(Some(key), 48_000, &handoff, &controls));
            handoff.take(0)
        };
        // Down to the fourth band, then Tab to its frequency: no new chain.
        for _ in 0..22 {
            assert!(press(&mut live, Key::Down).is_none());
        }
        assert!(press(&mut live, Key::Tab).is_none());
        // 199 Hz a sixth of an octave up is 223 Hz.
        let new = chain(press(&mut live, Key::Right)).expect("a new chain");
        let mut want = original.clone();
        want.eq.freqs_hz[3] = 223.0;
        assert_eq!(live.tuner.profile(), &want);
        assert!(plays(new, &want));
        assert!(!plays(Processor::new(&original, 48_000, 2), &want));
        // B keeps the saved settings, on a chain of its own.
        assert!(plays(chain(handoff.take(1)).unwrap(), &original));
        // Nothing more is posted until the next change, and only for A.
        assert!(press(&mut live, Key::Tab).is_none());
        let new = chain(press(&mut live, Key::ShiftLeft)).expect("a new chain");
        want.eq.q[3] = 1.48;
        assert_eq!(live.tuner.profile(), &want);
        assert!(plays(new, &want));
        assert!(handoff.take(1).is_none());
        // A refused step posts nothing: Q cannot go above 20.
        for _ in 0..30 {
            press(&mut live, Key::Right);
        }
        assert_eq!(live.tuner.profile().eq.q[3], 18.0);
        assert!(press(&mut live, Key::Right).is_none());
        assert!(live.tuner.message().starts_with("refused: eq.q[3] = 20.2"));
        for _ in 0..50 {
            press(&mut live, Key::Left);
        }
        assert_eq!(live.tuner.profile().eq.q[3], 0.111);
        // Back to the saved settings, which B's chain has played all along:
        // A plays that one rather than a new one.
        let reverted = press(&mut live, Key::Char('r'));
        assert!(matches!(reverted, Some(Offer::Empty)));
        assert!(handoff.take(1).is_none());
        assert_eq!(live.tuner.profile(), &original);
        // Saved, the file reads back as the edited profile.
        assert!(press(&mut live, Key::BackTab).is_none());
        let new = chain(press(&mut live, Key::ShiftRight)).expect("a new chain");
        let mut want = original.clone();
        want.eq.freqs_hz[3] = 201.0;
        assert!(plays(new, &want));
        assert!(press(&mut live, Key::Char('s')).is_none());
        assert_eq!(super::super::load_profile(&path).unwrap(), want);
        assert!(fs::read_to_string(&path).unwrap().contains(
            "freqs_hz = [30.0, 56.4, 106.0, 201.0, 375.0, 705.0, 1327.0, 2497.0, 4698.0, 8839.0, 16000.0]"
        ));
    }

    #[test]
    fn a_b_keys_reach_the_slots_lanes_and_the_callbacks_controls() {
        let dir = Scratch::new("ab");
        let (mut live, _) = live_in(&dir, "output_gain_db = -3.0\n", None);
        let saved = live.tuner.profile().clone();
        let (handoff, controls) = (Handoff::new(), Controls::default());
        let press = |live: &mut Live, key, handoff: &Handoff<Processor>, controls: &Controls| {
            assert!(!live.step(Some(key), 48_000, handoff, controls));
        };
        let quiet =
            |handoff: &Handoff<Processor>| handoff.take(0).is_none() && handoff.take(1).is_none();
        // p loads a preset into A, the slot heard; B keeps the saved
        // settings, now on a chain of its own.
        press(&mut live, Key::Char('p'), &handoff, &controls);
        let preset = crate::presets::PRESETS[0].profile();
        assert_eq!(live.tuner.profile(), &preset);
        assert!(plays(chain(handoff.take(0)).unwrap(), &preset));
        assert!(plays(chain(handoff.take(1)).unwrap(), &saved));
        assert_eq!(controls.slot(), 0);
        // a switches the callback to B, with no new chain.
        press(&mut live, Key::Char('a'), &handoff, &controls);
        assert_eq!(controls.slot(), 1);
        assert!(quiet(&handoff));
        // An edit in B goes to B's lane alone.
        for _ in 0..6 {
            press(&mut live, Key::Down, &handoff, &controls);
        }
        press(&mut live, Key::Right, &handoff, &controls);
        let edited = Profile {
            input_gain_db: 0.5,
            ..saved.clone()
        };
        assert!(plays(chain(handoff.take(1)).unwrap(), &edited));
        assert!(handoff.take(0).is_none());
        // m turns matching off; a blind test turns it back on and bypass
        // off, and its draws pick the slot heard.
        assert!(controls.matching());
        press(&mut live, Key::Char('m'), &handoff, &controls);
        assert!(!controls.matching());
        press(&mut live, Key::Char('b'), &handoff, &controls);
        press(&mut live, Key::Char('x'), &handoff, &controls);
        assert!(controls.matching() && !controls.bypass.load(Relaxed));
        for vote in ['1', '2', '1', '2', '1', '2'] {
            press(&mut live, Key::Char(vote), &handoff, &controls);
            assert_eq!(controls.slot(), live.tuner.slot());
        }
        press(&mut live, Key::Char('x'), &handoff, &controls);
        assert!(!controls.matching());
        assert_eq!(controls.slot(), 1);
        assert!(quiet(&handoff));
        // A new session starts with the heard slot's chain alone: A's comes
        // through the new handoff.
        let (handoff, controls) = (Handoff::new(), Controls::default());
        live.step(None, 48_000, &handoff, &controls);
        assert!(plays(chain(handoff.take(0)).unwrap(), &preset));
        assert!(handoff.take(1).is_none());
        assert_eq!(controls.slot(), 1);
        assert!(!controls.matching());
        // c copies B to A, which then plays B's chain.
        press(&mut live, Key::Char('c'), &handoff, &controls);
        assert_eq!(live.tuner.message(), "copied B to A");
        assert!(matches!(handoff.take(0), Some(Offer::Empty)));
        assert!(handoff.take(1).is_none());
        press(&mut live, Key::Char('a'), &handoff, &controls);
        assert_eq!(controls.slot(), 0);
        assert!(quiet(&handoff));
    }

    #[test]
    fn a_save_of_the_watched_file_is_played_and_said() {
        let dir = Scratch::new("reload");
        let reports = Arc::new(Reports::default());
        let (mut live, path) = live_in(&dir, "output_gain_db = -3.0\n", Some(Arc::clone(&reports)));
        let (handoff, controls) = (Handoff::new(), Controls::default());
        stamp(&path, "output_gain_db = -6.0\n", 1);
        live.step(None, 48_000, &handoff, &controls);
        let six_down = Profile {
            output_gain_db: -6.0,
            ..Profile::default()
        };
        assert!(plays(chain(handoff.take(0)).unwrap(), &six_down));
        assert_eq!(live.tuner.message(), "reloaded settings.toml");
        assert_eq!(reports.take(), ["reloaded"]);
        // The same settings saved again: said, but the chain is left alone.
        stamp(&path, "output_gain_db = -6.0 # again\n", 2);
        live.tuner.set_message(String::new());
        live.step(None, 48_000, &handoff, &controls);
        assert!(handoff.take(0).is_none() && handoff.take(1).is_none());
        assert_eq!(live.tuner.message(), "reloaded settings.toml");
        // A save that does not load is said and changes nothing.
        stamp(&path, "output_gain_db = \"loud\"\n", 3);
        live.step(None, 48_000, &handoff, &controls);
        assert!(handoff.take(0).is_none() && handoff.take(1).is_none());
        let message = live.tuner.message().to_owned();
        assert!(message.starts_with("profile not reloaded: "), "{message}");
        assert_eq!(
            reports.take(),
            ["reloaded".to_owned(), format!("reload failed: {message}")]
        );
        assert_eq!(live.tuner.profile(), &six_down);
        // An empty file, as an editor leaves it mid-save, is not reloaded.
        stamp(&path, "", 4);
        live.step(None, 48_000, &handoff, &controls);
        assert!(handoff.take(0).is_none() && handoff.take(1).is_none());
        assert_eq!(live.tuner.profile(), &six_down);
        assert!(reports.take().is_empty());
    }

    #[test]
    fn a_reload_while_asking_to_save_drops_the_question() {
        let dir = Scratch::new("ask");
        let (mut live, path) = live_in(&dir, "", None);
        let (handoff, controls) = (Handoff::new(), Controls::default());
        live.step(Some(Key::Right), 48_000, &handoff, &controls);
        assert!(!live.step(Some(Key::Char('q')), 48_000, &handoff, &controls));
        assert!(live.tuner.prompt().is_some());
        // Other messages leave the question up.
        live.announce("External Headphones", 44_100, true, None);
        assert!(live.tuner.prompt().is_some());
        stamp(&path, "output_gain_db = -1.0\n", 1);
        live.step(None, 48_000, &handoff, &controls);
        assert_eq!(live.tuner.prompt(), None);
        assert_eq!(
            live.tuner.message(),
            "reloaded settings.toml; unsaved changes were replaced, so there is nothing to \
             save; press q to quit"
        );
        // y no longer saves over the reloaded file; q quits.
        assert!(!live.step(Some(Key::Char('y')), 48_000, &handoff, &controls));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "output_gain_db = -1.0\n"
        );
        assert!(live.step(Some(Key::Char('q')), 48_000, &handoff, &controls));
    }

    #[test]
    fn a_new_output_is_announced() {
        let dir = Scratch::new("announce");
        let reports = Arc::new(Reports::default());
        let (mut live, _) = live_in(&dir, "", Some(Arc::clone(&reports)));
        live.announce("MacBook Pro Speakers", 48_000, false, None);
        assert_eq!(
            live.tuner.message(),
            "processing system audio on MacBook Pro Speakers at 48000 Hz"
        );
        live.announce("External Headphones", 44_100, true, None);
        assert_eq!(
            live.tuner.message(),
            "now on External Headphones at 44100 Hz"
        );
        assert_eq!(
            reports.take(),
            [
                "running on MacBook Pro Speakers at 48000 with settings.toml",
                "running on External Headphones at 44100 with settings.toml"
            ]
        );
    }

    /// A run on `settings.toml` in `dir` that follows the output device,
    /// with config.toml there saying `config`.
    fn following(dir: &Scratch, config: &str, background: Option<Arc<Reports>>) -> (Live, PathBuf) {
        let path = dir.0.join("settings.toml");
        stamp(&path, "output_gain_db = -3.0\n", 0);
        let config_path = dir.0.join("config.toml");
        stamp(&config_path, config, 0);
        let options = Options {
            profile: super::super::load_profile(&path).unwrap(),
            name: path.display().to_string(),
            watch: Some(path.clone()),
            save_path: path.clone(),
            config: Some(config_path),
            settings_file: Some(path.clone()),
            follow_devices: true,
            control: None,
            background: background.map(|r| r as Arc<dyn Report>),
        };
        (Live::new(options), path)
    }

    fn gain(db: f32) -> Profile {
        Profile {
            output_gain_db: db,
            ..Profile::default()
        }
    }

    const PHONES: &str = "[devices.phones]\nname = \"Phones\"\nprofile = \"phones.toml\"\n";

    #[test]
    fn each_output_plays_with_its_own_profile_or_else_the_settings_file() {
        let dir = Scratch::new("follow");
        let phones = dir.0.join("phones.toml");
        stamp(&phones, "output_gain_db = -6.0\n", 0);
        let reports = Arc::new(Reports::default());
        let (mut live, settings) = following(&dir, PHONES, Some(Arc::clone(&reports)));
        let (handoff, controls) = (Handoff::new(), Controls::default());

        // A device without a profile of its own keeps the settings file.
        assert_eq!(live.follow("spk", "Speakers", None), None);
        assert_eq!(live.tuner.profile(), &gain(-3.0));

        assert_eq!(
            live.follow("phones", "Phones", None),
            Some(format!(" with {}", phones.display()))
        );
        assert_eq!(live.tuner.profile(), &gain(-6.0));
        assert_eq!(live.name, phones.display().to_string());
        assert_eq!(live.save_path, phones);
        assert_eq!(live.follow("phones", "Phones", None), None);
        // Edits are saved to the device's own file, and a save of it reloads.
        live.step(Some(Key::Right), 48_000, &handoff, &controls);
        live.step(Some(Key::Char('s')), 48_000, &handoff, &controls);
        let edited = super::super::load_profile(&phones).unwrap();
        assert_ne!(edited, gain(-6.0));
        assert_eq!(super::super::load_profile(&settings).unwrap(), gain(-3.0));
        stamp(&phones, "output_gain_db = -9.0\n", 5);
        handoff.take(0);
        handoff.take(1);
        live.step(None, 48_000, &handoff, &controls);
        assert!(plays(chain(handoff.take(0)).unwrap(), &gain(-9.0)));

        // Back on the speakers, with an unsaved edit to the phones' profile.
        live.step(Some(Key::Right), 48_000, &handoff, &controls);
        assert_eq!(
            live.follow("spk", "Speakers", None),
            Some(format!(
                " with {}; unsaved changes to {} were dropped",
                settings.display(),
                phones.display()
            ))
        );
        assert_eq!(live.tuner.profile(), &gain(-3.0));
        assert!(!live.tuner.dirty());
        live.announce("Speakers", 44_100, true, Some(" with x".into()));
        assert_eq!(live.tuner.message(), "now on Speakers at 44100 Hz with x");
        let reports = reports.take();
        assert_eq!(
            reports.last().unwrap(),
            &format!("running on Speakers at 44100 with {}", settings.display())
        );
    }

    #[test]
    fn a_devices_own_profile_that_cannot_be_used_gives_way_to_the_settings_file() {
        let dir = Scratch::new("follow-broken");
        let reports = Arc::new(Reports::default());
        let (mut live, settings) = following(&dir, PHONES, Some(Arc::clone(&reports)));
        let phones = dir.0.join("phones.toml");

        // Missing: the settings file it already plays stays, and why is said.
        let note = live.follow("phones", "Phones", None).unwrap();
        assert!(
            note.starts_with(&format!(
                " with {}, since its own profile cannot be used: cannot read profile {}",
                settings.display(),
                phones.display()
            )),
            "{note}"
        );
        assert_eq!(live.save_path, settings);
        assert_eq!(
            reports.take(),
            [format!("reload failed: Phones plays{note}")]
        );

        // From another device's own profile, it switches to the settings
        // file. One that does not fit the rate cannot be used either.
        stamp(
            &dir.0.join("config.toml"),
            &format!("{PHONES}[devices.dac]\nprofile = \"dac.toml\"\n"),
            1,
        );
        stamp(&dir.0.join("dac.toml"), "output_gain_db = -1.0\n", 0);
        assert!(live.config.as_mut().unwrap().changed().unwrap().is_ok());
        assert!(live.follow("dac", "DAC", None).is_some());
        stamp(&phones, "shelf_corner_hz = 3000.0\n", 0);
        let note = live.follow("phones", "Phones", Some(8_000)).unwrap();
        assert!(
            note.contains("does not fit") || note.contains("shelf_corner_hz"),
            "{note}"
        );
        assert!(
            note.starts_with(&format!(" with {}, since", settings.display())),
            "{note}"
        );
        assert_eq!(live.tuner.profile(), &gain(-3.0));
    }

    #[test]
    fn a_save_of_config_toml_switches_the_profile_in_place() {
        let dir = Scratch::new("follow-config");
        let reports = Arc::new(Reports::default());
        let (mut live, settings) = following(&dir, "", Some(Arc::clone(&reports)));
        let config = dir.0.join("config.toml");
        let phones = dir.0.join("phones.toml");
        stamp(&phones, "output_gain_db = -6.0\n", 0);
        let (handoff, controls) = (Handoff::new(), Controls::default());
        assert_eq!(live.follow("phones", "Phones", None), None);
        live.step(None, 48_000, &handoff, &controls);
        assert!(handoff.take(0).is_none());

        stamp(&config, PHONES, 1);
        live.step(None, 48_000, &handoff, &controls);
        assert!(plays(chain(handoff.take(0)).unwrap(), &gain(-6.0)));
        assert_eq!(
            live.tuner.message(),
            format!("Phones now plays with {}", phones.display())
        );
        assert_eq!(
            reports.take(),
            [
                "reloaded".to_owned(),
                format!("running on Phones at 48000 with {}", phones.display())
            ]
        );

        // A broken save is said once and changes nothing.
        stamp(&config, "[devices.phones]\nprofil = \"x.toml\"\n", 2);
        live.step(None, 48_000, &handoff, &controls);
        let message = live.tuner.message().to_owned();
        assert!(
            message.starts_with("config.toml not reloaded: "),
            "{message}"
        );
        assert!(message.contains("profil"), "{message}");
        assert!(handoff.take(0).is_none());
        live.tuner.set_message(String::new());
        live.step(None, 48_000, &handoff, &controls);
        assert_eq!(live.tuner.message(), "");
        assert_eq!(live.save_path, phones);

        // Unassigned again: back to the settings file. A change that leaves
        // the device alone is only said.
        stamp(&config, "exclude = []\n", 3);
        live.step(None, 48_000, &handoff, &controls);
        assert!(plays(chain(handoff.take(0)).unwrap(), &gain(-3.0)));
        assert_eq!(live.save_path, settings);
        stamp(&config, "exclude = [\"x\"]\n", 4);
        live.step(None, 48_000, &handoff, &controls);
        assert!(handoff.take(0).is_none());
        assert_eq!(
            live.tuner.message(),
            format!("reloaded {}", config.display())
        );
    }

    #[test]
    fn a_profile_file_or_preset_run_plays_it_on_every_device() {
        let dir = Scratch::new("follow-not");
        let (mut live, path) = live_in(&dir, "", None);
        stamp(&dir.0.join("phones.toml"), "", 0);
        let config = dir.0.join("config.toml");
        stamp(&config, PHONES, 0);
        live.config = Some(ConfigFile::new(&config).0);
        assert_eq!(live.follow("phones", "Phones", None), None);
        assert_eq!(live.save_path, path);
    }

    #[test]
    fn a_broken_config_toml_at_the_start_is_said_and_ignored() {
        let dir = Scratch::new("follow-start");
        let (live, settings) = following(&dir, "exclude = 3\n", None);
        assert!(
            live.tuner
                .message()
                .starts_with("config.toml is ignored until it is fixed: "),
            "{}",
            live.tuner.message()
        );
        assert_eq!(live.config.unwrap().config, Config::default());
        assert_eq!(live.save_path, settings);
    }

    fn bypass(live: &mut Live, switch: Switch) -> Result<String, String> {
        live.control(&Request::Bypass(switch), 48_000)
    }

    #[test]
    fn a_bypass_request_goes_through_the_tuner_to_the_audio_and_is_said() {
        let dir = Scratch::new("ctl-bypass");
        let (mut live, _) = live_in(&dir, "", None);
        let (handoff, controls) = (Handoff::new(), Controls::default());
        live.step(None, 48_000, &handoff, &controls);
        assert_eq!(bypass(&mut live, Switch::On), Ok("bypass is on".into()));
        assert!(live.tuner.bypass());
        assert_eq!(live.tuner.message(), "bypass is on");
        live.step(None, 48_000, &handoff, &controls);
        assert!(controls.bypass.load(Relaxed));
        assert!(handoff.take(0).is_none() && handoff.take(1).is_none());
        assert_eq!(
            bypass(&mut live, Switch::On),
            Ok("bypass was already on".into())
        );
        assert_eq!(
            bypass(&mut live, Switch::Toggle),
            Ok("bypass is off".into())
        );
        live.step(None, 48_000, &handoff, &controls);
        assert!(!controls.bypass.load(Relaxed));
        assert_eq!(
            bypass(&mut live, Switch::Off),
            Ok("bypass was already off".into())
        );
        assert_eq!(bypass(&mut live, Switch::Toggle), Ok("bypass is on".into()));
        // The tuner's b key and the request share one bypass.
        live.step(Some(Key::Char('b')), 48_000, &handoff, &controls);
        assert!(!controls.bypass.load(Relaxed));
        assert_eq!(bypass(&mut live, Switch::Toggle), Ok("bypass is on".into()));
    }

    #[test]
    fn a_bypass_request_is_refused_during_a_blind_test() {
        let dir = Scratch::new("ctl-blind");
        let (mut live, _) = live_in(&dir, "output_gain_db = -3.0\n", None);
        let (handoff, controls) = (Handoff::new(), Controls::default());
        live.step(Some(Key::Char('p')), 48_000, &handoff, &controls);
        live.step(Some(Key::Char('x')), 48_000, &handoff, &controls);
        assert!(live.tuner.trial().is_some());
        let message = live.tuner.message().to_owned();
        for switch in [Switch::On, Switch::Toggle] {
            let e = bypass(&mut live, switch).unwrap_err();
            assert_eq!(
                e,
                "the tuner is running a blind test, which keeps bypass off; end it with x first"
            );
        }
        live.step(None, 48_000, &handoff, &controls);
        assert!(!live.tuner.bypass() && !controls.bypass.load(Relaxed));
        assert_eq!(live.tuner.message(), message);
        live.step(Some(Key::Char('x')), 48_000, &handoff, &controls);
        assert_eq!(bypass(&mut live, Switch::On), Ok("bypass is on".into()));
    }

    #[test]
    fn use_plays_a_preset_on_every_device_and_saves_edits_to_the_settings_file() {
        let dir = Scratch::new("ctl-preset");
        stamp(&dir.0.join("phones.toml"), "output_gain_db = -6.0\n", 0);
        let reports = Arc::new(Reports::default());
        let (mut live, settings) = following(&dir, PHONES, Some(Arc::clone(&reports)));
        let (handoff, controls) = (Handoff::new(), Controls::default());
        assert_eq!(live.follow("spk", "Speakers", None), None);
        live.latency = Some(speakers_latency());
        live.step(None, 48_000, &handoff, &controls);

        let said = live
            .control(&Request::Preset("night".into()), 48_000)
            .unwrap();
        assert_eq!(said, "now playing preset night on every output device");
        assert_eq!(live.tuner.message(), said);
        let night = presets::find("night").unwrap().profile();
        assert_eq!(live.tuner.profile(), &night);
        assert_eq!(live.name, "preset night");
        assert_eq!(live.save_path, settings);
        assert!(live.profile_file.is_none());
        // `dps status` shows it, with the chain's latency for the preset.
        let latency = Latency {
            chain: Processor::new(&night, 48_000, 2).latency_frames(),
            ..speakers_latency()
        };
        assert_eq!(
            reports.take(),
            [
                "reloaded".to_owned(),
                format!(
                    "running on Speakers at 48000 with preset night; {}",
                    latency.describe()
                )
            ]
        );
        live.step(None, 48_000, &handoff, &controls);
        assert!(plays(chain(handoff.take(0)).unwrap(), &night));

        // It no longer follows the device, and the settings file is not
        // watched, as in a --preset run.
        assert_eq!(live.follow("phones", "Phones", None), None);
        assert_eq!(live.tuner.profile(), &night);
        stamp(&settings, "output_gain_db = -1.0\n", 3);
        live.step(None, 48_000, &handoff, &controls);
        assert_eq!(live.tuner.profile(), &night);
        // Edits are saved to the settings file.
        live.step(Some(Key::Right), 48_000, &handoff, &controls);
        live.step(Some(Key::Char('s')), 48_000, &handoff, &controls);
        assert_ne!(live.tuner.profile(), &night);
        assert_eq!(
            &super::super::load_profile(&settings).unwrap(),
            live.tuner.profile()
        );

        assert_eq!(
            live.control(&Request::Preset("loud".into()), 48_000),
            Err("there is no preset loud".into())
        );
    }

    #[test]
    fn use_plays_a_file_on_every_device_and_watches_it() {
        let dir = Scratch::new("ctl-file");
        let (mut live, settings) = following(&dir, "", None);
        let (handoff, controls) = (Handoff::new(), Controls::default());
        let mine = dir.0.join("mine.toml");
        stamp(&mine, "output_gain_db = -2.0\n", 0);
        let said = live.control(&Request::File(mine.clone()), 48_000).unwrap();
        assert_eq!(
            said,
            format!("now playing {} on every output device", mine.display())
        );
        assert_eq!(live.tuner.profile(), &gain(-2.0));
        assert_eq!(live.name, mine.display().to_string());
        assert_eq!(live.save_path, mine);
        assert_eq!(live.profile_file.as_ref().unwrap().path, mine);
        assert!(!live.follow_devices);
        stamp(&mine, "output_gain_db = -4.0\n", 5);
        live.step(None, 48_000, &handoff, &controls);
        assert_eq!(live.tuner.profile(), &gain(-4.0));

        // One that cannot be read changes nothing.
        for (path, start) in [
            (dir.0.join("gone.toml"), "cannot read profile "),
            (settings.with_file_name("bad.toml"), ""),
        ] {
            stamp(&dir.0.join("bad.toml"), "output_gain_db = \"loud\"\n", 0);
            let e = live
                .control(&Request::File(path.clone()), 48_000)
                .unwrap_err();
            assert!(e.starts_with(start) && e.contains("toml"), "{e}");
            assert_eq!(live.save_path, mine);
            assert_eq!(live.tuner.profile(), &gain(-4.0));
        }
    }

    #[test]
    fn use_settings_goes_back_to_following_the_output_device() {
        let dir = Scratch::new("ctl-settings");
        let phones = dir.0.join("phones.toml");
        stamp(&phones, "output_gain_db = -6.0\n", 0);
        let (mut live, settings) = following(&dir, PHONES, None);
        assert!(live.follow("phones", "Phones", None).is_some());
        live.control(&Request::Preset("music".into()), 48_000)
            .unwrap();
        assert!(!live.follow_devices);

        let said = live.control(&Request::Settings, 48_000).unwrap();
        assert_eq!(
            said,
            format!(
                "now playing {}, the profile of Phones, and following the output device",
                phones.display()
            )
        );
        assert_eq!(live.tuner.profile(), &gain(-6.0));
        assert_eq!(live.save_path, phones);
        assert_eq!(live.profile_file.as_ref().unwrap().path, phones);
        assert!(live.follow_devices);
        assert!(live.follow("spk", "Speakers", None).is_some());
        assert_eq!(live.save_path, settings);
        let said = live.control(&Request::Settings, 48_000).unwrap();
        assert_eq!(
            said,
            format!(
                "now playing {}, the settings file, and following the output device",
                settings.display()
            )
        );

        // A device's own profile that cannot be used gives way to the
        // settings file, which is created if it is missing.
        live.control(&Request::Preset("music".into()), 48_000)
            .unwrap();
        assert_eq!(live.follow("phones", "Phones", None), None);
        stamp(&phones, "output_gain_db = \"x\"\n", 1);
        fs::remove_file(&settings).unwrap();
        let said = live.control(&Request::Settings, 48_000).unwrap();
        assert!(
            said.starts_with(&format!(
                "now playing {}, the settings file, and following the output device; created {} \
                 with the default profile; Phones's own cannot be used: {}:",
                settings.display(),
                settings.display(),
                phones.display()
            )),
            "{said}"
        );
        assert_eq!(live.tuner.profile(), &Profile::default());
        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            crate::config::default_toml()
        );
    }

    #[test]
    fn use_drops_unsaved_changes_and_a_question_about_them_and_says_so() {
        let dir = Scratch::new("ctl-drop");
        let (mut live, settings) = live_in(&dir, "output_gain_db = -3.0\n", None);
        let (handoff, controls) = (Handoff::new(), Controls::default());
        live.step(Some(Key::Right), 48_000, &handoff, &controls);
        assert!(live.tuner.dirty());
        let said = live
            .control(&Request::Preset("night".into()), 48_000)
            .unwrap();
        assert_eq!(
            said,
            "now playing preset night on every output device; unsaved changes to settings.toml \
             were dropped"
        );
        assert!(!live.tuner.dirty());

        live.step(Some(Key::Right), 48_000, &handoff, &controls);
        live.step(Some(Key::Char('q')), 48_000, &handoff, &controls);
        assert!(live.tuner.prompt().is_some());
        let said = live.control(&Request::Settings, 48_000).unwrap();
        assert_eq!(
            said,
            format!(
                "now playing {}, the settings file, and following the output device; unsaved \
                 changes to preset night were dropped, so there is nothing to save; press q to \
                 quit",
                settings.display()
            )
        );
        assert_eq!(live.tuner.prompt(), None);
        assert_eq!(live.tuner.profile(), &gain(-3.0));
    }

    #[test]
    fn a_profile_that_does_not_fit_the_rate_is_refused_and_nothing_changes() {
        let dir = Scratch::new("ctl-rate");
        let reports = Arc::new(Reports::default());
        let (mut live, settings) =
            live_in(&dir, "output_gain_db = -3.0\n", Some(Arc::clone(&reports)));
        live.follow("spk", "Speakers", None);
        let high = dir.0.join("high.toml");
        stamp(&high, "shelf_corner_hz = 3000.0\n", 0);
        let message = live.tuner.message().to_owned();
        let e = live
            .control(&Request::File(high.clone()), 8_000)
            .unwrap_err();
        assert!(
            e.starts_with(&format!(
                "{} does not fit Speakers at 8000 Hz: shelf_corner_hz = 3000 is outside",
                high.display()
            )),
            "{e}"
        );
        assert!(e.ends_with("; nothing was changed"), "{e}");
        stamp(&settings, "shelf_corner_hz = 3000.0\n", 0);
        let e = live.control(&Request::Settings, 8_000).unwrap_err();
        assert!(e.contains("does not fit Speakers at 8000 Hz"), "{e}");

        assert_eq!(live.tuner.profile(), &gain(-3.0));
        assert_eq!(live.name, "settings.toml");
        assert_eq!(live.save_path, settings);
        assert_eq!(live.profile_file.as_ref().unwrap().path, settings);
        assert!(!live.follow_devices);
        assert_eq!(live.tuner.message(), message);
        assert!(reports.take().is_empty());
    }

    #[test]
    fn a_request_on_the_socket_is_answered_from_the_main_loop() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        let dir = Scratch::new("ctl-sock");
        let path = dir.0.join("settings.toml");
        stamp(&path, "", 0);
        let socket = dir.0.join("control.sock");
        let mut live = Live::new(options_in(&path, Some(socket.clone()), None));
        assert!(socket.exists());
        let (handoff, controls) = (Handoff::new(), Controls::default());
        let client = {
            let socket = socket.clone();
            thread::spawn(move || {
                let mut stream = UnixStream::connect(&socket).unwrap();
                stream.write_all(b"bypass on\n").unwrap();
                let mut reply = String::new();
                stream.read_to_string(&mut reply).unwrap();
                reply
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.is_finished() {
            assert!(Instant::now() < deadline, "no answer");
            assert!(!live.step(None, 48_000, &handoff, &controls));
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(client.join().unwrap(), "ok bypass is on\n");
        assert!(controls.bypass.load(Relaxed));
        drop(live);
        assert!(!socket.exists());

        // Audio goes on without a socket, and says why it has none.
        let nowhere = dir.0.join("missing/control.sock");
        let live = Live::new(options_in(&path, Some(nowhere.clone()), None));
        assert!(live.server.is_none());
        assert!(
            live.tuner
                .message()
                .starts_with(&format!("cannot listen on {}: ", nowhere.display())),
            "{}",
            live.tuner.message()
        );
        assert!(
            live.tuner
                .message()
                .ends_with("; `dps bypass` and `dps use` cannot reach this run")
        );
    }

    fn processes(list: &[(ObjectId, Option<&str>)]) -> Vec<(ObjectId, Option<String>)> {
        list.iter()
            .map(|&(p, bundle)| (p, bundle.map(String::from)))
            .collect()
    }

    #[test]
    fn the_tap_leaves_out_this_process_and_the_apps_config_toml_excludes() {
        let dir = Scratch::new("apps");
        let reports = Arc::new(Reports::default());
        let config = "exclude = [\"us.zoom.xos\", \"com.google.Chrome\"]\n";
        let (mut live, _) = following(&dir, config, Some(Arc::clone(&reports)));
        let own = 10;
        let mut running = processes(&[
            (11, Some("com.spotify.client")),
            (12, Some("us.zoom.xos")),
            (13, Some("com.google.Chrome.helper")),
            (14, None),
            (15, Some("com.google.ChromeCanary")),
        ]);
        assert!(!live.excludes_nothing());
        assert_eq!(live.apps_now(own, &running), (vec![10, 12, 13], true));
        live.apps.applied = vec![10, 12, 13];
        assert_eq!(
            live.apps_note().as_deref(),
            Some("apps left alone: com.google.Chrome, us.zoom.xos")
        );
        assert_eq!(
            reports.take(),
            ["excluded [com.google.Chrome, us.zoom.xos]"]
        );
        // Nothing new: nothing to change or say.
        assert_eq!(live.apps_now(own, &running), (vec![10, 12, 13], false));
        assert_eq!(live.apps_note(), None);

        // An app that quits needs no change to the tap, only a message.
        running.retain(|(p, _)| *p != 12);
        assert_eq!(live.apps_now(own, &running), (vec![10, 13], false));
        assert_eq!(
            live.apps_note().as_deref(),
            Some("apps left alone: com.google.Chrome")
        );
        // One that starts does change it.
        running.push((16, Some("com.google.Chrome.helper.GPU".into())));
        assert_eq!(live.apps_now(own, &running), (vec![10, 13, 16], true));
        assert_eq!(live.apps_note(), None);
        live.apps.applied = vec![10, 13, 16];

        // A config.toml that stops excluding them puts them back in the tap.
        stamp(&dir.0.join("config.toml"), "exclude = []\n", 1);
        let (handoff, controls) = (Handoff::new(), Controls::default());
        live.step(None, 48_000, &handoff, &controls);
        assert!(live.apps.due.is_some());
        assert!(live.excludes_nothing());
        assert_eq!(live.apps_now(own, &running), (vec![10], true));
        assert_eq!(
            live.apps_note().as_deref(),
            Some("apps left alone: none running")
        );
    }

    #[test]
    fn without_excludes_only_this_process_is_left_out() {
        let dir = Scratch::new("apps-none");
        let (mut live, _) = live_in(&dir, "", None);
        assert!(live.excludes_nothing());
        let running = processes(&[(11, Some("us.zoom.xos")), (12, None)]);
        assert_eq!(live.apps_now(10, &running), (vec![10], true));
        assert_eq!(live.apps_note(), None);
        // A preset or --profile run follows config.toml's excludes too.
        let config = dir.0.join("config.toml");
        stamp(&config, "exclude = [\"us.zoom.xos\"]\n", 0);
        live.config = Some(ConfigFile::new(&config).0);
        assert_eq!(live.apps_now(10, &running), (vec![10, 11], true));
    }

    #[test]
    fn the_first_announcement_names_the_apps_left_alone() {
        let dir = Scratch::new("apps-announce");
        let reports = Some(Arc::new(Reports::default()));
        let (mut live, _) = following(&dir, "exclude = [\"us.zoom.xos\"]\n", reports);
        live.apps_now(10, &processes(&[(12, Some("us.zoom.xos"))]));
        live.announce("Speakers", 48_000, false, Some(" with x.toml".into()));
        assert_eq!(
            live.tuner.message(),
            "processing system audio on Speakers at 48000 Hz with x.toml; apps left alone: \
             us.zoom.xos"
        );
        live.announce("Speakers", 48_000, true, None);
        assert_eq!(live.tuner.message(), "now on Speakers at 48000 Hz");
    }

    /// What the HAL reports for the built-in speakers of the machine this
    /// was written on, with an assumed tap side and chain.
    fn speakers_latency() -> Latency {
        Latency {
            rate: 48_000,
            buffer: 512,
            output: 60,
            output_safety: 48,
            output_stream: 690,
            tap: 0,
            tap_safety: 32,
            tap_stream: 0,
            chain: 72,
        }
    }

    #[test]
    fn latency_adds_a_buffer_each_way_and_every_other_part_once() {
        let latency = speakers_latency();
        assert_eq!(latency.frames(), 2 * 512 + 60 + 48 + 690 + 32 + 72);
        assert!((latency.millis() - 1926.0 / 48.0).abs() < 1e-9);
        assert_eq!(
            latency.describe(),
            "latency by the HAL's figures: 40.1 ms (IO buffer 2 × 512, output latency 60, output \
             safety offset 48, output stream latency 690, tap safety offset 32, chain 72; frames \
             at 48000 Hz)"
        );
        let bare = Latency {
            rate: 44_100,
            buffer: 441,
            ..Latency::default()
        };
        assert_eq!(
            bare.describe(),
            "latency by the HAL's figures: 20.0 ms (IO buffer 2 × 441; frames at 44100 Hz)"
        );
        assert_eq!(Latency::default().millis(), 0.0);
    }

    #[test]
    fn each_session_announces_its_latency() {
        let dir = Scratch::new("latency");
        let reports = Arc::new(Reports::default());
        let (mut live, _) = live_in(&dir, "", Some(Arc::clone(&reports)));
        live.latency = Some(speakers_latency());
        live.announce("Speakers", 48_000, false, None);
        let said = live.tuner.message().to_owned();
        assert!(
            said.starts_with(
                "processing system audio on Speakers at 48000 Hz; latency by the HAL's figures: \
                 40.1 ms (IO buffer 2 × 512,"
            ),
            "{said}"
        );
        assert_eq!(
            reports.take(),
            [format!(
                "running on Speakers at 48000 with settings.toml; {}",
                speakers_latency().describe()
            )]
        );
    }

    #[test]
    fn the_device_line_shows_the_volume_and_whether_the_contour_acts() {
        let mut profile = Profile::default();
        profile.contour.enabled = false;
        assert_eq!(contour_text(&profile, Some(-30.0)), None);
        profile.contour.enabled = true;
        profile.contour.reference_db = -10.0;
        profile.contour.bass_per_db = 0.5;
        profile.contour.treble_per_db = 0.25;
        let text = |v| contour_text(&profile, v).unwrap();
        assert_eq!(text(None), "volume unknown, contour idle");
        assert_eq!(text(Some(-18.4)), "volume -18 dB, contour acting");
        assert_eq!(text(Some(-10.0)), "volume -10 dB, contour idle");
        assert_eq!(text(Some(-0.2)), "volume 0 dB, contour idle");
        // Nothing to add at any volume: idle.
        profile.contour.max_bass_db = 0.0;
        profile.contour.max_treble_db = 0.0;
        assert_eq!(
            contour_text(&profile, Some(-40.0)).unwrap(),
            "volume -40 dB, contour idle"
        );
    }

    #[test]
    fn apps_starting_and_quitting_cannot_restart_audio_often_enough_to_stop_it() {
        let t0 = Instant::now();
        assert_eq!(next_apps_rebuild(None, t0), None);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(next_apps_rebuild(Some(t0), at(1_000)), Some(at(5_000)));
        assert_eq!(next_apps_rebuild(Some(t0), at(5_000)), None);
        // An update that fails every 100 ms for a minute restarts the
        // session only as often as allowed, and that never adds up to more
        // than the limit, with room left for device changes.
        let mut rebuilds = Rebuilds::default();
        let mut last = None;
        let mut count = 0;
        for ms in (0..60_000).step_by(100) {
            if next_apps_rebuild(last, at(ms)).is_none() {
                last = Some(at(ms));
                count += 1;
                assert!(rebuilds.record(at(ms)));
                assert!(rebuilds.0.len() <= REBUILD_LIMIT - 3, "{ms} ms");
            }
        }
        assert_eq!(count, 12);
    }

    unsafe extern "C" {
        fn setsid() -> i32;
        fn setpgid(pid: i32, group: i32) -> c_int;
        fn ioctl(fd: c_int, request: std::ffi::c_ulong, ...) -> c_int;
    }

    /// Run by the test below in a child process on a pseudo-terminal: whether
    /// this run would take over the terminal.
    #[test]
    #[ignore = "run by only_the_terminals_foreground_gets_the_tuner"]
    fn foreground_child() {
        let Some(want) = std::env::var_os("DPS_FOREGROUND_CHILD") else {
            return;
        };
        if want == "leader" {
            // The session leader runs the background case as its own child,
            // in a process group of its own.
            let me = std::env::current_exe().unwrap();
            let mut child = std::process::Command::new(me);
            child
                .args(["--exact", "live::tests::foreground_child", "--ignored"])
                .env("DPS_FOREGROUND_CHILD", "background");
            // SAFETY: setpgid() is async-signal-safe.
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut child, || {
                    setpgid(0, 0);
                    Ok(())
                });
            }
            assert!(child.status().unwrap().success());
        }
        let foreground = want != "background";
        assert_eq!(in_foreground(STDIN), foreground, "{want:?}");
        assert!(io::stdin().is_terminal() && io::stderr().is_terminal());
    }

    #[test]
    fn only_the_terminals_foreground_gets_the_tuner() {
        // `dps live &` from an interactive shell runs in a process group
        // that is not the terminal's foreground one.
        let (master, slave) = tui::pty();
        let me = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(me);
        child
            .args(["--exact", "live::tests::foreground_child", "--ignored"])
            .env("DPS_FOREGROUND_CHILD", "leader")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        // SAFETY: setsid() and ioctl() are async-signal-safe.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut child, || {
                const TIOCSCTTY: std::ffi::c_ulong = 0x2000_7461;
                if setsid() == -1 || ioctl(0, TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = child.spawn().unwrap();
        let output = drain_until_exit(&master, &mut child);
        let status = child.wait().unwrap();
        assert!(
            status.success(),
            "{status}: {}",
            String::from_utf8_lossy(&output)
        );
        // Not this process's terminal at all, and not a terminal.
        assert!(!in_foreground(slave.as_raw_fd()));
        assert!(!in_foreground(io::pipe().unwrap().0.as_raw_fd()));
    }

    /// Reads what a child writes to the pseudo-terminal `master` until it
    /// exits, so it never blocks on a full terminal.
    fn drain_until_exit(master: &std::os::fd::OwnedFd, child: &mut std::process::Child) -> Vec<u8> {
        let mut master = File::from(master.try_clone().unwrap());
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let exited = child.try_wait().unwrap().is_some();
            let mut fd = libc_pollfd(master.as_raw_fd());
            // SAFETY: one valid pollfd. Once the child has exited the
            // terminal may report a hangup rather than data, so a read that
            // returns nothing ends the round.
            while unsafe { poll(&mut fd, 1, if exited { 0 } else { 50 }) } > 0 {
                let mut buf = [0; 4096];
                match io::Read::read(&mut master, &mut buf) {
                    Ok(n) if n > 0 => output.extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
            if exited {
                return output;
            }
            assert!(
                Instant::now() < deadline,
                "the child did not finish: {:?}",
                String::from_utf8_lossy(&output)
            );
        }
    }

    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: i16,
        revents: i16,
    }

    fn libc_pollfd(fd: c_int) -> PollFd {
        PollFd {
            fd,
            events: 1,
            revents: 0,
        }
    }

    unsafe extern "C" {
        fn poll(fds: *mut PollFd, count: u32, timeout_ms: c_int) -> c_int;
    }

    #[test]
    fn stop_signals_are_ignored_while_live() {
        // SIGTSTP (Ctrl-Z), SIGTTIN and SIGTTOU would each stop the process
        // with its tap still muting other apps.
        let disposition = |signum| {
            // SAFETY: reads the disposition by setting it to itself.
            unsafe {
                let current = signal(signum, SIG_DFL);
                signal(signum, current);
                current
            }
        };
        let before = NoSuspend::SIGNALS.map(disposition);
        let guard = NoSuspend::new().unwrap();
        assert_eq!(NoSuspend::SIGNALS, [18, 21, 22]);
        assert_eq!(NoSuspend::SIGNALS.map(disposition), [SIG_IGN; 3]);
        drop(guard);
        assert_eq!(NoSuspend::SIGNALS.map(disposition), before);
    }

    /// Run by the test below in a child process on a pseudo-terminal.
    #[test]
    #[ignore = "run by a_second_signal_puts_the_terminal_back_before_the_process_ends"]
    fn second_signal_child() {
        if std::env::var_os("DPS_SECOND_SIGNAL_CHILD").is_none() {
            return;
        }
        install_signal_handlers().unwrap();
        let _screen = Screen::enter().unwrap();
        // SAFETY: raise() delivers to this thread before it returns.
        unsafe { raise(15) };
        assert!(STOP.load(Relaxed));
        // As if shutdown hung and SIGTERM came again: the process ends here.
        unsafe { raise(15) };
        unreachable!("still running after a second SIGTERM");
    }

    #[test]
    fn a_second_signal_puts_the_terminal_back_before_the_process_ends() {
        use std::os::unix::process::ExitStatusExt;
        let (master, slave) = tui::pty();
        let me = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(me)
            .args(["--exact", "live::tests::second_signal_child", "--ignored"])
            .env("DPS_SECOND_SIGNAL_CHILD", "1")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap())
            .spawn()
            .unwrap();
        let output = drain_until_exit(&master, &mut child);
        let status = child.wait().unwrap();
        let shown = String::from_utf8_lossy(&output);
        assert_eq!(status.signal(), Some(15), "{status}: {shown:?}");
        // The tuner's screen came up, and the last thing written put it away.
        assert!(shown.contains("\x1b[?1049h"), "{shown:?}");
        assert!(output.ends_with(b"\x1b[?25h\x1b[?1049l"), "{shown:?}");
        assert!(tui::canonical(slave.as_raw_fd()));
    }

    #[test]
    fn callback_clamps_and_counts_loud_output() {
        let frames = 64;
        let mut profile = flat();
        profile.output_gain_db = 12.0;
        let mut tap = vec![0.5; 2 * frames];
        let mut out = vec![0.0; 2 * frames];
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        let mut engine = engine(&profile, 4096);
        call(&mut engine, &mut input, &mut output);

        assert!(out.iter().all(|&x| x == 1.0));
        assert_eq!(engine.stats.clamped.load(Relaxed), 2 * frames as u64);
    }

    #[test]
    fn with_the_limiter_on_the_callback_has_nothing_to_clamp() {
        // The same 12 dB over full scale, from the first sample, comes out
        // after the limiter's lookahead under its ceiling, by what true peak
        // reads between the samples of so abrupt a start (a sampled step
        // reconstructs to about 1.13 times its height); the clamp stays for
        // a limiter turned off.
        let frames = 512;
        let mut profile = flat();
        profile.output_gain_db = 12.0;
        profile.limiter.enabled = true;
        let mut tap = vec![0.5; 2 * frames];
        let mut out = vec![0.0; 2 * frames];
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        let mut engine = engine(&profile, 4096);
        call(&mut engine, &mut input, &mut output);

        let ceiling = 10f64.powf(f64::from(profile.limiter.ceiling_db) / 20.0) as f32;
        let delay = 2 * Processor::new(&profile, 48_000, 2).latency_frames();
        assert!(out[..delay].iter().all(|&x| x == 0.0));
        assert!(
            out[delay..]
                .iter()
                .all(|&x| x <= ceiling && x > ceiling / 1.15)
        );
        assert_eq!(engine.stats.clamped.load(Relaxed), 0);
    }

    #[test]
    fn callback_stays_silent_after_a_non_finite_sample() {
        // The chain turns f32::MAX, raised 12 dB, into infinity.
        let frames = 64;
        let mut profile = flat();
        profile.output_gain_db = 12.0;
        let mut tap = vec![0.1; 2 * frames];
        tap[7] = f32::MAX;
        let mut out = vec![0.25; 2 * frames];
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        let mut engine = engine(&profile, 4096);
        call(&mut engine, &mut input, &mut output);
        assert!(out.iter().all(|&x| x == 0.0));
        assert!(engine.stats.non_finite.load(Relaxed));

        // Clean input afterwards does not bring the output back.
        tap.fill(0.1);
        out.fill(0.25);
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        call(&mut engine, &mut input, &mut output);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn callback_feeds_non_finite_input_to_the_chain_as_silence() {
        let frames = 64;
        let mut tap: Vec<f32> = (0..2 * frames)
            .map(|i| (i as f32 * 0.37).sin() * 0.5)
            .collect();
        let mut want = tap.clone();
        for (i, bad) in [
            (3, f32::NAN),
            (8, f32::INFINITY),
            (2 * frames - 1, f32::NEG_INFINITY),
        ] {
            tap[i] = bad;
            want[i] = 0.0;
        }
        let mut out = vec![0.25; 2 * frames];
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        let mut engine = engine(&flat(), 4096);
        call(&mut engine, &mut input, &mut output);

        assert_eq!(out, want);
        assert!(!engine.stats.non_finite.load(Relaxed));
        assert!(!engine.failed);
    }

    #[test]
    fn after_a_non_finite_output_a_healthy_chain_stays_silent() {
        // Bad once, then a pass-through with no state to poison, so only the
        // latch can keep the output silent.
        let mut calls = 0;
        let chain = move |block: &mut [f32]| {
            calls += 1;
            if calls == 1 {
                block[1] = f32::NAN;
            }
        };
        let mut engine = scripted(chain, 4096);
        let frames = 64;
        let mut tap = vec![0.1; 2 * frames];
        let mut out = vec![0.25; 2 * frames];
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        call(&mut engine, &mut input, &mut output);
        assert!(out.iter().all(|&x| x == 0.0));
        assert!(engine.stats.non_finite.load(Relaxed));

        out.fill(0.25);
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        call(&mut engine, &mut input, &mut output);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_panic_in_the_callback_is_silenced_and_stays_silent() {
        // The second block of the first callback panics, after the first
        // block was already written; every later call is healthy.
        let mut calls = 0;
        let chain = move |_: &mut [f32]| {
            calls += 1;
            if calls == 2 {
                panic!("scripted panic in the audio callback");
            }
        };
        let mut engine = scripted(chain, 32);
        let frames = 100;
        let mut tap = vec![0.1; 2 * frames];
        let mut out = vec![0.25; 2 * frames];
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        call(&mut engine, &mut input, &mut output);
        assert!(out.iter().all(|&x| x == 0.0));
        assert!(engine.stats.panicked.load(Relaxed));
        assert!(!engine.stats.non_finite.load(Relaxed));

        out.fill(0.25);
        let mut input = list([(2, &mut tap[..])]);
        let mut output = list([(2, &mut out[..])]);
        call(&mut engine, &mut input, &mut output);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(engine.stats.callbacks.load(Relaxed), 2);
    }

    #[test]
    fn hal_buffers_report_disabled_streams_as_missing() {
        let mut data = [0.5; 8];
        let mut empty = [0.0f32; 0];
        let mut fake = list([(2, &mut data[..]), (3, &mut empty[..])]);
        fake.buffers[1].mData = ptr::null_mut();
        // SAFETY: a valid list for the duration of the test.
        let buffers = unsafe { HalBuffers::new(NonNull::from(&mut fake).cast()) };
        assert_eq!(buffers.count(), 2);
        assert_eq!((buffers.channels(0), buffers.channels(1)), (2, 3));
        assert_eq!(buffers.samples(0), Some(&[0.5; 8][..]));
        assert_eq!(buffers.samples(1), None);
    }

    #[test]
    fn hal_buffers_report_misaligned_data_as_missing() {
        let mut data = [0.5f32; 8];
        let mut fake = list([(1, &mut data[..])]);
        fake.buffers[0].mData = fake.buffers[0].mData.cast::<u8>().wrapping_add(1).cast();
        fake.buffers[0].mDataByteSize -= 4;
        // SAFETY: a valid list for the duration of the test; the misaligned
        // pointer is never read.
        let buffers = unsafe { HalBuffers::new(NonNull::from(&mut fake).cast()) };
        assert_eq!(buffers.samples(0), None);
    }

    fn target(sample_rate: f64) -> Target {
        Target {
            device: 0,
            name: "Test Out".into(),
            uid: String::new(),
            sample_rate,
        }
    }

    #[test]
    fn device_rate_is_validated_against_the_profile() {
        let default = Profile::default();
        assert_eq!(
            target(48_000.0).validated_rate(&default, "default profile"),
            Ok(48_000)
        );
        assert_eq!(
            target(44_100.000_001).validated_rate(&default, "default profile"),
            Ok(44_100)
        );
        for bad in [0.0, -48_000.0, f64::NAN, 1e12] {
            let e = target(bad)
                .validated_rate(&default, "default profile")
                .unwrap_err();
            assert!(e.contains("unusable sample rate"), "{e}");
        }
        // The default profile's top EQ bands do not fit under 4 kHz; they
        // are skipped, not rejected.
        assert_eq!(
            target(8_000.0).validated_rate(&default, "default profile"),
            Ok(8_000)
        );
        let high_corner = Profile {
            shelf_corner_hz: 3_000.0,
            ..Profile::default()
        };
        let e = target(8_000.0)
            .validated_rate(&high_corner, "default profile")
            .unwrap_err();
        assert!(
            e.starts_with("default profile does not fit Test Out at 8000 Hz: shelf_corner_hz"),
            "{e}"
        );
    }

    #[test]
    fn same_as_compares_device_and_rate() {
        let here = target(48_000.0);
        assert!(here.same_as(&target(48_000.0)));
        assert!(!here.same_as(&target(44_100.0)));
        assert!(!here.same_as(&Target {
            device: 7,
            ..target(48_000.0)
        }));
        // A renamed device is the same output.
        assert!(here.same_as(&Target {
            name: "Renamed".into(),
            uid: "other".into(),
            ..target(48_000.0)
        }));
    }

    #[test]
    fn only_32_bit_native_float_streams_are_accepted() {
        let format = |id, flags, bits| AudioStreamBasicDescription {
            mSampleRate: 48_000.0,
            mFormatID: id,
            mFormatFlags: flags,
            mBytesPerPacket: 0,
            mFramesPerPacket: 1,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 2,
            mBitsPerChannel: bits,
            mReserved: 0,
        };
        let float = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
        assert!(check_float32(&format(kAudioFormatLinearPCM, float, 32)).is_ok());
        let planar = float | kAudioFormatFlagIsNonInterleaved;
        assert!(check_float32(&format(kAudioFormatLinearPCM, planar, 32)).is_ok());
        for (id, flags, bits) in [
            (kAudioFormatLinearPCM, float, 64),
            (
                kAudioFormatLinearPCM,
                float | kAudioFormatFlagIsBigEndian,
                32,
            ),
            (
                kAudioFormatLinearPCM,
                kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
                16,
            ),
            (kAudioFormatMPEG4AAC, 0, 0),
        ] {
            assert!(check_float32(&format(id, flags, bits)).is_err());
        }
    }

    #[test]
    fn float_check_rejects_integer_and_unpacked_samples() {
        let format = |flags, bytes_per_frame| AudioStreamBasicDescription {
            mSampleRate: 48_000.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: flags,
            mBytesPerPacket: bytes_per_frame,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let float = kAudioFormatFlagIsFloat;
        let planar = float | kAudioFormatFlagIsNonInterleaved;
        // Packed by flag, or by sizes that leave no room for padding.
        for (flags, bytes) in [
            (float | kAudioFormatFlagIsPacked, 8),
            (float, 8),
            (planar, 4),
        ] {
            assert!(check_float32(&format(flags, bytes)).is_ok(), "{flags:#x}");
        }
        for (flags, bytes) in [
            // 32-bit integers.
            (
                kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
                8,
            ),
            (kAudioFormatFlagIsSignedInteger, 8),
            // 32-bit floats in 64-bit slots, high- or low-aligned.
            (float | kAudioFormatFlagIsAlignedHigh, 16),
            (float, 16),
            (planar, 8),
        ] {
            assert!(check_float32(&format(flags, bytes)).is_err(), "{flags:#x}");
        }
    }

    fn stream(channels: u32, terminal: u32) -> hal::Stream {
        hal::Stream {
            format: AudioStreamBasicDescription {
                mSampleRate: 48_000.0,
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
                mBytesPerPacket: 4 * channels,
                mFramesPerPacket: 1,
                mBytesPerFrame: 4 * channels,
                mChannelsPerFrame: channels,
                mBitsPerChannel: 32,
                mReserved: 0,
            },
            terminal,
        }
    }

    const MIC: u32 = u32::from_be_bytes(*b"micr");
    const LINE: u32 = u32::from_be_bytes(*b"line");

    #[test]
    fn the_tap_is_the_stream_after_the_devices_own_inputs() {
        // Built-in speakers have no input streams of their own.
        assert_eq!(
            locate_tap(&[stream(2, 0)], &[], 2),
            Ok(TapChannels { first: 0, count: 2 })
        );
        // A headset with a mono microphone.
        assert_eq!(
            locate_tap(&[stream(1, MIC), stream(2, 0)], &[stream(1, MIC)], 2),
            Ok(TapChannels { first: 1, count: 2 })
        );
        // An interface with a line input and a microphone.
        let device = [stream(2, LINE), stream(1, MIC)];
        assert_eq!(
            locate_tap(&[stream(2, LINE), stream(1, MIC), stream(2, 0)], &device, 2),
            Ok(TapChannels { first: 3, count: 2 })
        );
    }

    #[test]
    fn unexpected_aggregate_layouts_are_refused() {
        let mic = [stream(2, MIC)];
        for (inputs, device, tap) in [
            // The tap first, where only the terminal types tell them apart.
            (vec![stream(2, 0), stream(2, MIC)], &mic[..], 2),
            // No tap stream, two of them, or the wrong channel count.
            (vec![stream(2, MIC)], &mic[..], 2),
            (
                vec![stream(2, MIC), stream(2, 0), stream(2, 0)],
                &mic[..],
                2,
            ),
            (vec![stream(2, MIC), stream(1, 0)], &mic[..], 2),
            // The device's streams changed shape on the way in.
            (vec![stream(1, MIC), stream(2, 0)], &mic[..], 2),
            (vec![], &[][..], 2),
            (vec![stream(0, 0)], &[][..], 0),
        ] {
            let e = locate_tap(&inputs, device, tap).unwrap_err();
            assert!(e.starts_with("unexpected aggregate device layout"), "{e}");
        }
    }

    #[test]
    fn silence_counts_only_while_others_play() {
        let step = Duration::from_millis(250);
        let t0 = Instant::now();
        let mut watch = SilenceWatch::new(t0, 0);
        let mut polls = 0;
        // One poll per step, with the callback running between polls.
        let mut poll = |watch: &mut SilenceWatch, playing| {
            polls += 1;
            watch.exceeded(t0 + step * polls, u64::from(polls), playing)
        };
        for _ in 0..40 {
            assert!(!poll(&mut watch, false));
        }
        for _ in 0..11 {
            assert!(!poll(&mut watch, true));
        }
        // A break starts the count again.
        assert!(!poll(&mut watch, false));
        for _ in 0..11 {
            assert!(!poll(&mut watch, true));
        }
        assert!(poll(&mut watch, true));
    }

    #[test]
    fn silence_counts_only_while_the_callback_runs() {
        let step = Duration::from_millis(250);
        let t0 = Instant::now();
        let mut watch = SilenceWatch::new(t0, 5);
        for i in 1..=40 {
            assert!(!watch.exceeded(t0 + step * i, 5, true));
        }
        // Eleven running polls, a stalled one, then twelve to reach 3 s.
        let mut callbacks = 5;
        let mut poll = |watch: &mut SilenceWatch, i: u32, running: bool| {
            callbacks += u64::from(running);
            watch.exceeded(t0 + step * i, callbacks, true)
        };
        for i in 41..52 {
            assert!(!poll(&mut watch, i, true));
        }
        assert!(!poll(&mut watch, 52, false));
        for i in 53..64 {
            assert!(!poll(&mut watch, i, true));
        }
        assert!(poll(&mut watch, 64, true));
    }

    #[test]
    fn a_late_poll_counts_as_at_most_two_intervals() {
        // Blocked for a minute, say on the permission prompt: counts 0.5 s.
        let t0 = Instant::now();
        let mut watch = SilenceWatch::new(t0, 0);
        let mut t = t0 + Duration::from_secs(60);
        assert!(!watch.exceeded(t, 1, true));
        for callbacks in 2..11 {
            t += PLAYING_POLL;
            assert!(!watch.exceeded(t, callbacks, true));
        }
        t += PLAYING_POLL;
        assert!(watch.exceeded(t, 11, true));
        assert!(!watch.due(t + PLAYING_POLL / 2));
        assert!(watch.due(t + PLAYING_POLL));
    }

    #[test]
    fn rebuilds_are_limited_within_the_window() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut rebuilds = Rebuilds::default();
        for i in 0..REBUILD_LIMIT as u64 {
            assert!(rebuilds.record(at(500 * i)));
        }
        assert!(!rebuilds.record(at(2_500)));

        // Spread out, they never add up.
        let mut rebuilds = Rebuilds::default();
        let spacing = REBUILD_WINDOW.as_millis() as u64 / REBUILD_LIMIT as u64;
        for i in 0..50 {
            assert!(rebuilds.record(at(spacing * i)));
        }
    }
}
