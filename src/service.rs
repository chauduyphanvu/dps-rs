//! `dps start`, `stop` and `status`: `dps live` as a per-user launchd agent.
//!
//! `dps start` loads a job labelled `dev.dps.live` into the user's GUI
//! domain from a plist in dps's own folder, which launchd forgets at logout;
//! `dps start --login` writes the plist to ~/Library/LaunchAgents instead,
//! where launchd also loads it at every login. The job runs `dps live
//! --service` once and never restarts it; its output goes to
//! ~/Library/Logs/dps.log. That run writes what `dps status` shows to a
//! state file, and every live run, in the foreground or not, holds a lock so
//! that two never process system audio at once.

use std::ffi::{OsString, c_char, c_int, c_long};
use std::fs::{self, File, TryLockError};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{Dirs, Source};
use crate::live::{self, BACKGROUND_PERMISSION_HINT};

pub const LABEL: &str = "dev.dps.live";
/// How long launchd waits after SIGTERM before SIGKILL. A clean stop takes
/// well under a second; only a start blocked on the permission prompt
/// ignores SIGTERM.
const EXIT_TIMEOUT_SECS: u32 = 5;
/// How long `dps start` waits for the service to start audio or fail.
const START_WAIT: Duration = Duration::from_secs(10);
/// How long `dps start` then waits to hear sound while other apps play;
/// longer than the service takes to give up on a silent tap.
const HEAR_WAIT: Duration = Duration::from_secs(5);
const STOP_WAIT: Duration = Duration::from_secs(EXIT_TIMEOUT_SECS as u64 + 5);
const POLL: Duration = Duration::from_millis(100);

/// The service's files, all under the home directory.
pub struct Paths {
    /// launchd loads this one at every login.
    pub login_plist: PathBuf,
    /// The job for a plain `dps start`, which launchd never loads by itself.
    pub plist: PathBuf,
    pub log: PathBuf,
    pub lock: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    pub fn new(home: &Path) -> Self {
        let own = home.join("Library/Application Support/dps");
        Self {
            login_plist: home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            plist: own.join(format!("{LABEL}.plist")),
            log: home.join("Library/Logs/dps.log"),
            lock: own.join("live.lock"),
            state: own.join("live.state"),
        }
    }

    pub fn from_dirs(dirs: &Dirs) -> Result<Self, String> {
        Ok(Self::new(&dirs.home()?))
    }
}

/// What the plist tells launchd to run.
#[derive(Debug, PartialEq)]
pub struct Job {
    /// The binary's canonical path, then its arguments.
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub log: String,
}

impl Job {
    /// The job that runs `exe` live in the background with `source`, with
    /// the settings file found as it is here.
    fn new(exe: &Path, source: &Source, dirs: &Dirs, paths: &Paths) -> Result<Self, String> {
        let utf8 = |path: &Path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{} is not a UTF-8 path", path.display()))
        };
        let mut arguments = vec![utf8(exe)?, "live".into(), "--service".into()];
        arguments.extend(source.args()?);
        let mut environment = vec![("HOME".to_owned(), utf8(&dirs.home()?)?)];
        if let Some(xdg) = dirs
            .xdg_config_home
            .as_ref()
            .filter(|x| Path::new(x).is_absolute())
        {
            environment.push(("XDG_CONFIG_HOME".into(), utf8(Path::new(xdg))?));
        }
        Ok(Self {
            arguments,
            environment,
            log: utf8(&paths.log)?,
        })
    }

    pub fn plist(&self) -> String {
        let string = |s: &str| format!("<string>{}</string>", escape(s));
        let mut out = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n",
        );
        out += &format!("\t<key>Label</key>\n\t{}\n", string(LABEL));
        out += "\t<key>ProgramArguments</key>\n\t<array>\n";
        for argument in &self.arguments {
            out += &format!("\t\t{}\n", string(argument));
        }
        out += "\t</array>\n\t<key>EnvironmentVariables</key>\n\t<dict>\n";
        for (key, value) in &self.environment {
            out += &format!("\t\t<key>{}</key>\n\t\t{}\n", escape(key), string(value));
        }
        out += "\t</dict>\n";
        // Started once when loaded, never again: a restart loop could leave
        // system audio toggling between muted and not.
        out += "\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<false/>\n";
        // launchd may throttle a Standard agent; audio must not be.
        out += "\t<key>ProcessType</key>\n\t<string>Interactive</string>\n";
        out += &format!("\t<key>ExitTimeOut</key>\n\t<integer>{EXIT_TIMEOUT_SECS}</integer>\n");
        for key in ["StandardOutPath", "StandardErrorPath"] {
            out += &format!("\t<key>{key}</key>\n\t{}\n", string(&self.log));
        }
        out += "</dict>\n</plist>\n";
        out
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The ProgramArguments of a plist `Job::plist` wrote.
pub fn plist_arguments(plist: &str) -> Vec<String> {
    let Some((_, rest)) = plist.split_once("<key>ProgramArguments</key>") else {
        return Vec::new();
    };
    let array = rest.split_once("</array>").map_or(rest, |(array, _)| array);
    array
        .split("<string>")
        .skip(1)
        .filter_map(|s| s.split_once("</string>"))
        .map(|(s, _)| unescape(s))
        .collect()
}

/// The settings a job's arguments select, as messages name them.
pub fn settings_of(arguments: &[String]) -> String {
    let value = |flag: &str| {
        let i = arguments.iter().position(|a| a == flag)?;
        arguments.get(i + 1)
    };
    if let Some(path) = value("--profile") {
        path.clone()
    } else if let Some(name) = value("--preset") {
        format!("preset {name}")
    } else {
        "the settings file".into()
    }
}

/// The job as `launchctl print` shows it.
#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    pub running: bool,
    pub pid: Option<u32>,
    /// How it last ended, e.g. "exit code 1", if it has.
    pub last_exit: Option<String>,
    pub arguments: Vec<String>,
}

impl Loaded {
    /// Its top-level `key = value` lines; nested blocks are indented
    /// further and only `arguments` is read from them.
    pub fn parse(text: &str) -> Self {
        let mut loaded = Self::default();
        let mut lines = text.lines();
        let mut code = None;
        let mut signal = None;
        while let Some(line) = lines.next() {
            let Some(line) = line.strip_prefix('\t').filter(|l| !l.starts_with('\t')) else {
                continue;
            };
            let Some((key, value)) = line.split_once(" = ") else {
                continue;
            };
            match key {
                "state" => loaded.running = value == "running",
                "pid" => loaded.pid = value.parse().ok(),
                "last exit code" if !value.starts_with('(') => code = Some(value.to_owned()),
                "last terminating signal" => signal = Some(value.to_owned()),
                "arguments" => {
                    loaded.arguments = lines
                        .by_ref()
                        .take_while(|l| l.trim() != "}")
                        .map(|l| l.trim().to_owned())
                        .collect();
                }
                _ => {}
            }
        }
        loaded.last_exit = match (signal, code) {
            (Some(signal), _) => Some(format!("signal {signal}")),
            (None, Some(code)) => Some(format!("exit code {code}")),
            (None, None) => None,
        };
        loaded
    }

    fn failed(&self) -> bool {
        self.last_exit
            .as_deref()
            .is_some_and(|e| e != "exit code 0")
    }
}

/// launchctl, for the user's GUI domain.
pub struct Launchctl {
    program: OsString,
    domain: String,
}

unsafe extern "C" {
    fn getuid() -> u32;
    fn kill(pid: c_int, signal: c_int) -> c_int;
}

impl Launchctl {
    pub fn new() -> Self {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { getuid() };
        Self {
            // Tests point this at a stand-in; it is not meant for users.
            program: std::env::var_os("DPS_LAUNCHCTL").unwrap_or_else(|| "/bin/launchctl".into()),
            domain: format!("gui/{uid}"),
        }
    }

    fn service(&self) -> String {
        format!("{}/{LABEL}", self.domain)
    }

    fn run(&self, args: &[&str]) -> Result<std::process::Output, String> {
        Command::new(&self.program)
            .args(args)
            .output()
            .map_err(|e| format!("cannot run launchctl: {e}"))
    }

    /// The job, or `None` if it is not loaded.
    pub fn print(&self) -> Result<Option<Loaded>, String> {
        let out = self.run(&["print", &self.service()])?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            Ok(Some(Loaded::parse(&String::from_utf8_lossy(&out.stdout))))
        } else if out.status.code() == Some(113) || stderr.contains("Could not find service") {
            Ok(None)
        } else {
            Err(format!("launchctl print failed: {}", stderr.trim()))
        }
    }

    pub fn bootstrap(&self, plist: &Path) -> Result<(), String> {
        let plist = plist.to_str().ok_or("the plist path is not UTF-8")?;
        let out = self.run(&["bootstrap", &self.domain, plist])?;
        if out.status.success() {
            return Ok(());
        }
        Err(format!(
            "launchctl could not load {plist}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }

    pub fn bootout(&self) -> Result<(), String> {
        let out = self.run(&["bootout", &self.service()])?;
        // It may have gone meanwhile.
        if out.status.success() || self.print()?.is_none() {
            return Ok(());
        }
        Err(format!(
            "launchctl could not stop {LABEL}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { kill(pid as c_int, 0) == 0 }
}

/// Whether apps other than the service at `pid` are playing sound.
fn others_playing(pid: u32) -> bool {
    // Tests answer for the audio system here; it is not meant for users.
    match std::env::var("DPS_TEST_OTHERS_PLAYING") {
        Ok(answer) => answer == "1",
        Err(_) => live::others_playing(pid).unwrap_or(false),
    }
}

/// How far the background run has got.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Starting,
    WaitingForPermission,
    Running,
    Stopped,
    Failed,
}

/// What the background run reports about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub pid: u32,
    /// Seconds since the Unix epoch.
    pub started: u64,
    pub settings: String,
    pub phase: Phase,
    pub device: Option<String>,
    pub rate: Option<u32>,
    /// The profile file the device plays with, when that is not
    /// `settings`: the device's own.
    #[serde(default)]
    pub profile: Option<String>,
    /// The tap has delivered sound.
    #[serde(default)]
    pub heard: bool,
    /// The `exclude` entries of config.toml that cover a running app.
    #[serde(default)]
    pub excluded: Vec<String>,
    /// The last warning or error.
    pub problem: Option<String>,
}

impl State {
    pub fn read(path: &Path) -> Option<Self> {
        toml::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    /// What messages call the profile in use.
    pub fn profile(&self) -> &str {
        self.profile.as_deref().unwrap_or(&self.settings)
    }
}

/// The background run's state file, rewritten whole on every change.
struct StateFile {
    path: PathBuf,
    /// The state, and whether its problem is a failed reload, which the
    /// next good reload clears.
    state: Mutex<(State, bool)>,
}

impl StateFile {
    fn new(path: PathBuf, state: State) -> Self {
        let file = Self {
            path,
            state: Mutex::new((state, false)),
        };
        file.update(|_, _| {});
        file
    }

    fn update(&self, change: impl FnOnce(&mut State, &mut bool)) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (state, reload_problem) = &mut *guard;
        change(state, reload_problem);
        let text = toml::to_string(state).expect("a state always serializes");
        let temp = self.path.with_extension("tmp");
        if let Err(e) = fs::write(&temp, text).and_then(|()| fs::rename(&temp, &self.path)) {
            let _ = writeln!(
                io::stderr(),
                "dps live: cannot write {}: {e}",
                self.path.display()
            );
        }
    }

    fn finish(&self, result: &Result<(), String>) {
        self.update(|state, _| match result {
            Ok(()) => state.phase = Phase::Stopped,
            Err(e) => {
                state.phase = Phase::Failed;
                state.problem = Some(e.clone());
            }
        });
    }
}

impl live::Report for StateFile {
    fn waiting_for_permission(&self) {
        self.update(|state, _| state.phase = Phase::WaitingForPermission);
    }

    fn running(&self, device: &str, rate: u32, profile: &str) {
        self.update(|state, _| {
            state.phase = Phase::Running;
            state.device = Some(device.into());
            state.rate = Some(rate);
            state.profile = (profile != state.settings).then(|| profile.into());
        });
    }

    fn heard(&self) {
        self.update(|state, _| state.heard = true);
    }

    fn reload_failed(&self, message: &str) {
        self.update(|state, reload_problem| {
            state.problem = Some(message.into());
            *reload_problem = true;
        });
    }

    fn reloaded(&self) {
        self.update(|state, reload_problem| {
            if *reload_problem {
                state.problem = None;
                *reload_problem = false;
            }
        });
    }

    fn excluded(&self, apps: &[String]) {
        self.update(|state, _| state.excluded = apps.to_vec());
    }
}

/// Who holds the live lock.
#[derive(Debug, PartialEq)]
pub struct Holder {
    pub pid: Option<u32>,
    pub background: bool,
}

impl Holder {
    fn parse(text: &str) -> Self {
        let mut words = text.split_whitespace();
        Self {
            pid: words.next().and_then(|w| w.parse().ok()),
            background: words.next() == Some("background"),
        }
    }

    pub fn describe(&self) -> String {
        let pid = self.pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
        if self.background {
            format!("dps is already running in the background{pid}")
        } else {
            format!("dps live is already running in the foreground{pid}")
        }
    }
}

/// The lock every live run holds until it exits.
pub struct LiveLock(#[allow(dead_code)] File);

impl LiveLock {
    /// Takes the lock, or says who holds it.
    pub fn take(path: &Path, background: bool) -> Result<Result<Self, Holder>, String> {
        let cannot = |e: io::Error| format!("cannot lock {}: {e}", path.display());
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(cannot)?;
        }
        Ok(match Self::try_take(path).map_err(cannot)? {
            Ok(mut file) => {
                let mode = if background {
                    "background"
                } else {
                    "foreground"
                };
                file.set_len(0).map_err(cannot)?;
                writeln!(file, "{} {mode}", std::process::id()).map_err(cannot)?;
                Ok(Self(file))
            }
            Err(holder) => Err(holder),
        })
    }

    fn try_take(path: &Path) -> io::Result<Result<File, Holder>> {
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Ok(file)),
            Err(TryLockError::WouldBlock) => {
                let mut text = String::new();
                let _ = file.read_to_string(&mut text);
                Ok(Err(Holder::parse(&text)))
            }
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// Who holds the lock, if anyone.
    pub fn holder(path: &Path) -> Result<Option<Holder>, String> {
        if !path.exists() {
            return Ok(None);
        }
        Self::try_take(path)
            .map(Result::err)
            .map_err(|e| format!("cannot lock {}: {e}", path.display()))
    }
}

/// For a foreground `dps live`: the lock, or an error naming who has it.
pub fn lock_foreground(dirs: &Dirs) -> Result<Option<LiveLock>, String> {
    // Without a home there can be no background run to clash with.
    let Ok(paths) = Paths::from_dirs(dirs) else {
        return Ok(None);
    };
    match LiveLock::take(&paths.lock, false)? {
        Ok(lock) => Ok(Some(lock)),
        Err(holder) if holder.background => {
            Err(format!("{}; run `dps stop` first", holder.describe()))
        }
        Err(holder) => Err(format!("{}; quit it first", holder.describe())),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `dps live --service`: the run launchd starts. `live` runs audio until it
/// is stopped.
pub fn serve(
    source: Source,
    dirs: &Dirs,
    live: impl FnOnce(Source, Arc<dyn live::Report>) -> Result<(), String>,
) -> Result<(), String> {
    let paths = Paths::from_dirs(dirs)?;
    let taken = LiveLock::take(&paths.lock, true)?;
    if let Err(holder) = &taken
        && holder.background
    {
        // That run owns the state file.
        return Err(holder.describe());
    }
    let state = Arc::new(StateFile::new(
        paths.state.clone(),
        State {
            pid: std::process::id(),
            started: now(),
            settings: source.name(),
            phase: Phase::Starting,
            device: None,
            rate: None,
            profile: None,
            heard: false,
            excluded: Vec::new(),
            problem: None,
        },
    ));
    let result = match taken {
        Ok(_lock) => {
            // launchd appends to the log, so this keeps it to one run.
            let _ = File::options().write(true).truncate(true).open(&paths.log);
            live(source, Arc::clone(&state) as Arc<dyn live::Report>)
        }
        Err(holder) => Err(format!("{}; quit it first", holder.describe())),
    };
    state.finish(&result);
    result
}

/// Everything `dps status` and `dps doctor` look at.
pub struct Snapshot {
    pub loaded: Option<Loaded>,
    /// The state of the run launchd has now, or had last.
    pub state: Option<State>,
    /// The state file, when the running job should have written it but it
    /// is missing or does not parse.
    pub unreadable_state: Option<PathBuf>,
    /// The login plist's ProgramArguments, if it exists.
    pub login: Option<Vec<String>>,
    /// This dps, by its canonical path.
    pub exe: Option<PathBuf>,
}

impl Snapshot {
    pub fn read(paths: &Paths, launchctl: &Launchctl) -> Result<Self, String> {
        let loaded = launchctl.print()?;
        // A state file from a run that a later `dps stop` ended is stale.
        let state = loaded.as_ref().and_then(|_| State::read(&paths.state));
        let unreadable_state = match &loaded {
            Some(loaded) if loaded.running && loaded.pid.is_some() => {
                let corrupt = paths.state.exists() && state.is_none();
                // A run takes the lock, then writes its state at once.
                let own = state.as_ref().is_some_and(|s| Some(s.pid) == loaded.pid);
                let holds_lock = LiveLock::holder(&paths.lock)?
                    .is_some_and(|h| h.background && h.pid == loaded.pid);
                (corrupt || (holds_lock && !own)).then(|| paths.state.clone())
            }
            _ => None,
        };
        let login = fs::read_to_string(&paths.login_plist)
            .ok()
            .map(|text| plist_arguments(&text));
        let exe = std::env::current_exe().and_then(fs::canonicalize).ok();
        Ok(Self {
            loaded,
            state,
            unreadable_state,
            login,
            exe,
        })
    }

    /// The state if it is the running job's.
    fn current_state(&self) -> Option<&State> {
        let pid = self.loaded.as_ref()?.pid?;
        self.state.as_ref().filter(|s| s.pid == pid)
    }

    /// What the service is doing, as a phrase after "dps is", and whether
    /// that is healthy.
    pub fn service(&self, log: &Path) -> (bool, String) {
        const NOT_RUNNING: &str = "not running in the background";
        let Some(loaded) = &self.loaded else {
            return (true, NOT_RUNNING.into());
        };
        let log = log.display();
        if !loaded.running {
            let failure = self
                .state
                .as_ref()
                .filter(|s| s.phase == Phase::Failed)
                .and_then(|s| s.problem.clone());
            return match (failure, loaded.failed()) {
                (Some(problem), _) => (
                    false,
                    format!("{NOT_RUNNING}: it stopped with an error: {problem}"),
                ),
                (None, true) => (
                    false,
                    format!(
                        "{NOT_RUNNING}: it ended with {}; see {log}",
                        loaded.last_exit.as_deref().unwrap_or("an error")
                    ),
                ),
                (None, false) => (true, NOT_RUNNING.into()),
            };
        }
        let pid = loaded
            .pid
            .map(|p| format!(" (pid {p})"))
            .unwrap_or_default();
        let Some(state) = self.current_state() else {
            let settings = settings_of(&loaded.arguments);
            if let Some(path) = &self.unreadable_state {
                return (
                    false,
                    format!(
                        "running in the background{pid} with {settings}, but its state file {} \
                         is missing or unreadable, so how it is doing is unknown; fix: `dps \
                         stop`, then `dps start`",
                        path.display()
                    ),
                );
            }
            return (
                true,
                format!("starting in the background{pid} with {settings}"),
            );
        };
        let since = local_time(state.started);
        let head = format!(
            "running in the background since {since}{pid} with {}",
            state.profile()
        );
        match (state.phase, &state.device, state.rate) {
            (Phase::WaitingForPermission, _, _) => (
                false,
                format!(
                    "{head}, but it is waiting for permission to record system audio; fix: \
                     answer the macOS prompt, or {BACKGROUND_PERMISSION_HINT}"
                ),
            ),
            (Phase::Running, Some(device), Some(rate)) => {
                (true, format!("{head} on {device} at {rate} Hz"))
            }
            _ => (true, format!("{head}; it is starting")),
        }
    }

    /// Start at login as a phrase after "start at login is", and whether
    /// that is healthy.
    pub fn login(&self) -> (bool, String) {
        let Some(arguments) = &self.login else {
            return (true, "off".into());
        };
        let settings = settings_of(arguments);
        match arguments.first().map(PathBuf::from) {
            Some(program) if program.exists() => (true, format!("on, with {settings}")),
            Some(program) => (
                false,
                format!(
                    "on, but it runs {}, which no longer exists; fix: `dps start --login` \
                     with this dps, or `dps stop --login`",
                    program.display()
                ),
            ),
            None => (
                false,
                "on, but its plist is unreadable; fix: `dps start --login` again, or `dps \
                 stop --login`"
                    .into(),
            ),
        }
    }

    /// A note when the loaded job runs a binary other than this one.
    fn binary_note(&self) -> Option<String> {
        let program = PathBuf::from(self.loaded.as_ref()?.arguments.first()?);
        if !program.exists() {
            return Some(format!(
                "note: the background dps is {}, which no longer exists; `dps stop` then \
                 `dps start` runs this one",
                program.display()
            ));
        }
        let exe = self.exe.as_ref()?;
        (fs::canonicalize(&program).ok().as_ref() != Some(exe)).then(|| {
            format!(
                "note: the background dps is {}, not this one ({}); `dps stop` then `dps \
                 start` runs this one",
                program.display(),
                exe.display()
            )
        })
    }

    /// What `dps status` prints: a line, one for the apps a running service
    /// leaves alone, and another for any warning.
    pub fn status(&self, log: &Path) -> String {
        let (_, service) = self.service(log);
        let (_, login) = self.login();
        let mut out = format!("dps is {service}; start at login is {login}\n");
        if let Some(state) = self
            .current_state()
            .filter(|s| s.phase == Phase::Running && !s.excluded.is_empty())
        {
            out += &format!("apps left alone: {}\n", state.excluded.join(", "));
        }
        let mut notes = Vec::new();
        if let Some(problem) = self
            .current_state()
            .filter(|s| s.phase != Phase::Failed)
            .and_then(|s| s.problem.as_deref())
        {
            notes.push(format!("last warning: {problem}"));
        }
        notes.extend(self.binary_note());
        if !notes.is_empty() {
            out += &notes.join("; ");
            out += "\n";
        }
        out
    }
}

/// What `dps start` makes of the service while it waits.
#[derive(Debug, PartialEq)]
enum Outcome {
    Pending,
    Running(State),
    WaitingForPermission,
    Failed(String),
}

fn outcome(loaded: Option<&Loaded>, state: Option<&State>, log: &Path) -> Outcome {
    let Some(loaded) = loaded else {
        return Outcome::Failed("launchd no longer has the job".into());
    };
    if !loaded.running {
        if loaded.last_exit.is_none() {
            return Outcome::Pending;
        }
        return match state.filter(|s| s.phase == Phase::Failed) {
            Some(State {
                problem: Some(problem),
                ..
            }) => Outcome::Failed(problem.clone()),
            _ => Outcome::Failed(format!(
                "it ended with {}{}",
                loaded.last_exit.as_deref().unwrap_or("an error"),
                last_line(log)
                    .map(|line| format!(": {line}"))
                    .unwrap_or_default()
            )),
        };
    }
    match state.filter(|s| loaded.pid == Some(s.pid)) {
        Some(s) if s.phase == Phase::Running => Outcome::Running(s.clone()),
        Some(s) if s.phase == Phase::WaitingForPermission => Outcome::WaitingForPermission,
        Some(State {
            phase: Phase::Failed,
            problem: Some(problem),
            ..
        }) => Outcome::Failed(problem.clone()),
        _ => Outcome::Pending,
    }
}

/// The log's last non-empty line, which names the reason a run ended.
fn last_line(log: &Path) -> Option<String> {
    let text = fs::read_to_string(log).ok()?;
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    Some(line.trim().to_owned())
}

fn remove(path: &Path) -> Result<bool, String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

fn write_plist(path: &Path, job: &Job) -> Result<(), String> {
    let cannot = |e: io::Error| format!("cannot write {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(cannot)?;
    }
    fs::write(path, job.plist()).map_err(cannot)
}

/// `dps start`.
pub fn start(source: Source, login: bool, dirs: &Dirs) -> Result<(), String> {
    let paths = Paths::from_dirs(dirs)?;
    let launchctl = Launchctl::new();
    // A profile that does not load fails here, not in the background.
    source.load()?;
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| format!("cannot find the dps binary: {e}"))?;
    let job = Job::new(&exe, &source, dirs, &paths)?;
    let at_login = if login {
        format!("; it also starts at every login with {}", source.name())
    } else {
        String::new()
    };

    if let Some(holder) = LiveLock::holder(&paths.lock)?
        && !holder.background
    {
        return Err(format!("{}; quit it first", holder.describe()));
    }
    match launchctl.print()? {
        Some(loaded) if loaded.running => {
            let state = State::read(&paths.state).filter(|s| Some(s.pid) == loaded.pid);
            if let Some(State {
                phase: Phase::WaitingForPermission,
                ..
            }) = state
            {
                return Err(format!(
                    "dps is already running in the background but is waiting for permission to \
                     record system audio: click Allow in the macOS prompt (if no prompt shows, \
                     {BACKGROUND_PERMISSION_HINT})"
                ));
            }
            let running_with = state.map_or_else(
                || settings_of(&loaded.arguments),
                |s| s.profile().to_owned(),
            );
            if login {
                write_plist(&paths.login_plist, &job)?;
            }
            println!(
                "dps is already running in the background with {running_with}; `dps stop` stops \
                 it{at_login}"
            );
            return Ok(());
        }
        // A job that has already ended is unloaded to load it afresh.
        Some(_) => launchctl.bootout()?,
        None => {}
    }

    let plist = if login {
        &paths.login_plist
    } else {
        &paths.plist
    };
    // A start at login that fails would only fail again at every login, so
    // a failure removes it, or puts back the one that was there before.
    let earlier_login = login.then(|| fs::read(plist).ok()).flatten();
    let undo_login = |reason: String| {
        if !login {
            return reason;
        }
        match &earlier_login {
            Some(earlier) => match fs::write(plist, earlier) {
                Ok(()) => format!("{reason}; the earlier start at login was kept"),
                Err(e) => format!(
                    "{reason}; the earlier start at login could not be put back: cannot write {}: \
                     {e}",
                    plist.display()
                ),
            },
            None => match remove(plist) {
                Ok(_) => reason,
                Err(e) => format!("{reason}; {e}"),
            },
        }
    };
    write_plist(plist, &job).map_err(&undo_login)?;
    remove(&paths.state)?;
    launchctl.bootstrap(plist).map_err(&undo_login)?;

    let deadline = Instant::now() + START_WAIT;
    let state = loop {
        thread::sleep(POLL);
        let loaded = launchctl.print()?;
        match outcome(
            loaded.as_ref(),
            State::read(&paths.state).as_ref(),
            &paths.log,
        ) {
            Outcome::Running(state) => break state,
            Outcome::WaitingForPermission => {
                return Err(format!(
                    "dps started in the background but macOS is asking whether dps may record \
                     system audio: click Allow in that prompt and dps starts processing (if no \
                     prompt shows, {BACKGROUND_PERMISSION_HINT})"
                ));
            }
            Outcome::Failed(reason) => {
                return Err(undo_login(format!(
                    "dps could not start in the background: {reason} (log: {})",
                    paths.log.display()
                )));
            }
            Outcome::Pending if Instant::now() >= deadline => {
                return Err(format!(
                    "dps started in the background but has not started processing audio after \
                     {} s; see `dps status` and {}",
                    START_WAIT.as_secs(),
                    paths.log.display()
                ));
            }
            Outcome::Pending => {}
        }
    };

    // While other apps play, sound from the tap shows the permission is
    // there; the service stops itself if only silence comes.
    let deadline = Instant::now() + HEAR_WAIT;
    let mut state = state;
    while !state.heard && others_playing(state.pid) && Instant::now() < deadline {
        thread::sleep(POLL);
        let loaded = launchctl.print()?;
        match outcome(
            loaded.as_ref(),
            State::read(&paths.state).as_ref(),
            &paths.log,
        ) {
            Outcome::Running(now) => state = now,
            Outcome::Failed(reason) => {
                return Err(undo_login(format!(
                    "dps could not start in the background: {reason}"
                )));
            }
            _ => break,
        }
    }

    println!(
        "dps is running in the background on {} at {} Hz with {}; `dps stop` stops it{at_login}",
        state.device.as_deref().unwrap_or("the default output"),
        state.rate.unwrap_or(0),
        state.profile()
    );
    Ok(())
}

/// `dps stop`.
pub fn stop(login: bool, dirs: &Dirs) -> Result<(), String> {
    let paths = Paths::from_dirs(dirs)?;
    let launchctl = Launchctl::new();
    let mut message = match launchctl.print()? {
        Some(loaded) => {
            let state = State::read(&paths.state).filter(|s| Some(s.pid) == loaded.pid);
            launchctl.bootout()?;
            wait_until_gone(&launchctl, loaded.pid, &paths.lock)?;
            match (loaded.running, state) {
                (true, Some(state)) => format!(
                    "stopped dps, which ran in the background since {} with {}",
                    local_time(state.started),
                    state.profile()
                ),
                (true, None) => "stopped dps in the background".into(),
                (false, _) => "dps was not running in the background".into(),
            }
        }
        // launchd may have unloaded a run that has not exited yet, as after a
        // stop that timed out; its files stay until it has. Every background
        // run holds the lock from before it writes its state until it exits.
        None => match LiveLock::holder(&paths.lock)? {
            Some(holder) if holder.background => {
                let (pid, kill) = holder.pid.map_or_else(Default::default, |p| {
                    (
                        format!(" (pid {p})"),
                        format!(", or end it with `kill {p}`"),
                    )
                });
                return Err(format!(
                    "dps in the background{pid} is still running although launchd has unloaded \
                     it; run `dps stop` again once it has stopped{kill}"
                ));
            }
            _ => "dps is not running in the background".into(),
        },
    };
    remove(&paths.plist)?;
    remove(&paths.state)?;
    if login {
        message += if remove(&paths.login_plist)? {
            "; start at login is now off"
        } else {
            "; start at login was already off"
        };
    } else if paths.login_plist.exists() {
        message += "; start at login is still on (`dps stop --login` turns it off)";
    }
    println!("{message}");
    Ok(())
}

/// Waits until launchd has unloaded the job and its process has exited,
/// which also ends its tap.
fn wait_until_gone(launchctl: &Launchctl, pid: Option<u32>, lock: &Path) -> Result<(), String> {
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        let gone = launchctl.print()?.is_none()
            && !pid.is_some_and(alive)
            && LiveLock::holder(lock)?.is_none_or(|h| !h.background);
        if gone {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let pid = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            return Err(format!(
                "dps in the background{pid} did not stop within {} s",
                STOP_WAIT.as_secs()
            ));
        }
        thread::sleep(POLL);
    }
}

/// `dps status`.
pub fn status(dirs: &Dirs) -> Result<(), String> {
    let paths = Paths::from_dirs(dirs)?;
    let snapshot = Snapshot::read(&paths, &Launchctl::new())?;
    print!("{}", snapshot.status(&paths.log));
    Ok(())
}

/// `secs` after the Unix epoch as local time, e.g. "2026-11-13 16:52".
fn local_time(secs: u64) -> String {
    #[repr(C)]
    struct Tm {
        sec: c_int,
        min: c_int,
        hour: c_int,
        mday: c_int,
        mon: c_int,
        year: c_int,
        wday: c_int,
        yday: c_int,
        isdst: c_int,
        gmtoff: c_long,
        zone: *const c_char,
    }
    unsafe extern "C" {
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    }
    let time = secs as i64;
    let mut tm = MaybeUninit::<Tm>::uninit();
    // SAFETY: both pointers are valid; localtime_r fills `tm` or fails.
    if unsafe { localtime_r(&time, tm.as_mut_ptr()) }.is_null() {
        return format!("{secs} s after 1970");
    }
    // SAFETY: filled by the successful call.
    let tm = unsafe { tm.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.year + 1900,
        tm.mon + 1,
        tm.mday,
        tm.hour,
        tm.min
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::TestDir;
    use crate::presets;

    fn job(arguments: &[&str]) -> Job {
        Job {
            arguments: arguments.iter().map(|&a| a.into()).collect(),
            environment: vec![("HOME".into(), "/Users/a".into())],
            log: "/Users/a/Library/Logs/dps.log".into(),
        }
    }

    #[test]
    fn paths_live_under_the_home_directory() {
        let p = Paths::new(Path::new("/Users/a"));
        assert_eq!(
            p.login_plist,
            Path::new("/Users/a/Library/LaunchAgents/dev.dps.live.plist")
        );
        assert_eq!(
            p.plist,
            Path::new("/Users/a/Library/Application Support/dps/dev.dps.live.plist")
        );
        assert_eq!(p.log, Path::new("/Users/a/Library/Logs/dps.log"));
        assert_eq!(
            p.lock,
            Path::new("/Users/a/Library/Application Support/dps/live.lock")
        );
        assert_eq!(
            p.state,
            Path::new("/Users/a/Library/Application Support/dps/live.state")
        );
    }

    #[test]
    fn job_runs_the_canonical_binary_with_absolute_settings() {
        let dir = TestDir::new("service-job");
        let dirs = dir.dirs();
        let paths = Paths::new(&dirs.home().unwrap());
        let exe = Path::new("/opt/bin/dps");
        let mine = dir.0.join("mine.toml");
        fs::write(&mine, "").unwrap();
        let canonical = fs::canonicalize(&mine).unwrap();
        let canonical = canonical.to_str().unwrap();
        let home = dir.0.join("home");
        let xdg = dir.0.join("xdg");
        let environment = vec![
            ("HOME".to_owned(), home.to_str().unwrap().to_owned()),
            (
                "XDG_CONFIG_HOME".to_owned(),
                xdg.to_str().unwrap().to_owned(),
            ),
        ];

        // A relative profile path still names the same file from launchd's
        // working directory, /.
        let relative = relative_to_cwd(&mine);
        let source = Source::new(Some(relative), None, &dirs).unwrap();
        let job = Job::new(exe, &source, &dirs, &paths).unwrap();
        assert_eq!(
            job.arguments,
            ["/opt/bin/dps", "live", "--service", "--profile", canonical]
        );
        assert_eq!(job.environment, environment);
        assert_eq!(job.log, paths.log.to_str().unwrap());

        let music = presets::find("music").unwrap();
        let source = Source::new(None, Some(music), &dirs).unwrap();
        let job = Job::new(exe, &source, &dirs, &paths).unwrap();
        assert_eq!(
            job.arguments,
            ["/opt/bin/dps", "live", "--service", "--preset", "music"]
        );

        let source = Source::new(None, None, &dirs).unwrap();
        let job = Job::new(exe, &source, &dirs, &paths).unwrap();
        assert_eq!(job.arguments, ["/opt/bin/dps", "live", "--service"]);

        // An empty or relative XDG_CONFIG_HOME is left to the default.
        for ignored in ["", "config"] {
            let dirs = Dirs {
                xdg_config_home: Some(ignored.into()),
                ..dir.dirs()
            };
            let job = Job::new(exe, &source, &dirs, &paths).unwrap();
            assert_eq!(job.environment, environment[..1], "{ignored:?}");
        }
        let dirs = dir.dirs();

        let missing = Source::new(Some(dir.0.join("missing.toml")), None, &dirs).unwrap();
        let e = Job::new(exe, &missing, &dirs, &paths).unwrap_err();
        assert!(e.contains("missing.toml"), "{e}");
    }

    /// `path` relative to the current directory, as a user might type it.
    fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let ups = cwd.components().count() - 1;
        let mut relative: PathBuf = std::iter::repeat_n("..", ups).collect();
        relative.push(path.strip_prefix("/").unwrap());
        relative
    }

    #[test]
    fn plist_runs_once_without_restarts_and_logs_both_streams() {
        let text = job(&["/opt/bin/dps", "live", "--service"]).plist();
        assert!(text.starts_with("<?xml version=\"1.0\""), "{text}");
        for want in [
            "<key>Label</key>\n\t<string>dev.dps.live</string>",
            "<key>RunAtLoad</key>\n\t<true/>",
            "<key>KeepAlive</key>\n\t<false/>",
            "<key>ProcessType</key>\n\t<string>Interactive</string>",
            "<key>ExitTimeOut</key>\n\t<integer>5</integer>",
            "<key>StandardOutPath</key>\n\t<string>/Users/a/Library/Logs/dps.log</string>",
            "<key>StandardErrorPath</key>\n\t<string>/Users/a/Library/Logs/dps.log</string>",
            "<key>HOME</key>\n\t\t<string>/Users/a</string>",
        ] {
            assert!(text.contains(want), "{want} not in\n{text}");
        }
        assert!(text.ends_with("</dict>\n</plist>\n"));
    }

    #[test]
    fn plist_arguments_round_trip_with_markup_characters() {
        let arguments = ["/a b/dps", "live", "--profile", "/x/<&>'\".toml"];
        let text = job(&arguments).plist();
        assert!(
            text.contains("<string>/x/&lt;&amp;&gt;'\".toml</string>"),
            "{text}"
        );
        assert_eq!(plist_arguments(&text), arguments);
        assert!(plist_arguments("<plist><dict></dict></plist>").is_empty());
    }

    #[test]
    fn settings_are_named_from_the_arguments() {
        let args = |a: &[&str]| a.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
        assert_eq!(
            settings_of(&args(&["dps", "live", "--service", "--profile", "/p.toml"])),
            "/p.toml"
        );
        assert_eq!(
            settings_of(&args(&["dps", "live", "--service", "--preset", "voice"])),
            "preset voice"
        );
        assert_eq!(
            settings_of(&args(&["dps", "live", "--service"])),
            "the settings file"
        );
    }

    const RUNNING: &str = "gui/501/dev.dps.live = {
\tactive count = 1
\tpath = /Users/a/Library/Application Support/dps/dev.dps.live.plist
\ttype = LaunchAgent
\tstate = running

\tprogram = /opt/bin/dps
\targuments = {
\t\t/opt/bin/dps
\t\tlive
\t\t--service
\t\t--preset
\t\tmusic
\t}

\tenvironment = {
\t\tHOME => /Users/a
\t}

\tdomain = gui/501 [100022]
\texit timeout = 5
\truns = 1
\tpid = 4242
\timmediate reason = speculative
\tlast exit code = (never exited)

\tresource coalition = {
\t\tID = 1266
\t\tstate = active
\t}
}
";

    #[test]
    fn launchctl_print_is_read_for_state_pid_and_arguments() {
        let loaded = Loaded::parse(RUNNING);
        assert_eq!(
            loaded,
            Loaded {
                running: true,
                pid: Some(4242),
                last_exit: None,
                arguments: ["/opt/bin/dps", "live", "--service", "--preset", "music"]
                    .map(String::from)
                    .to_vec(),
            }
        );
        assert!(!loaded.failed());

        let exited = RUNNING
            .replace("state = running", "state = not running")
            .replace("\tpid = 4242\n", "")
            .replace("(never exited)", "1");
        let loaded = Loaded::parse(&exited);
        assert!(!loaded.running);
        assert_eq!(loaded.pid, None);
        assert_eq!(loaded.last_exit.as_deref(), Some("exit code 1"));
        assert!(loaded.failed());

        let clean = exited.replace("last exit code = 1", "last exit code = 0");
        assert!(!Loaded::parse(&clean).failed());

        let killed = exited.replace(
            "\tlast exit code = 1\n",
            "\tlast exit code = 0\n\tlast terminating signal = Killed: 9\n",
        );
        let loaded = Loaded::parse(&killed);
        assert_eq!(loaded.last_exit.as_deref(), Some("signal Killed: 9"));
        assert!(loaded.failed());
    }

    fn state(pid: u32, phase: Phase) -> State {
        State {
            pid,
            started: 1_763_000_000,
            settings: "preset music".into(),
            phase,
            device: Some("Speakers".into()),
            rate: Some(48_000),
            profile: None,
            heard: false,
            excluded: Vec::new(),
            problem: None,
        }
    }

    #[test]
    fn state_file_round_trips_and_clears_only_reload_problems() {
        use live::Report;
        let dir = TestDir::new("service-state");
        let path = dir.0.join("live.state");
        let file = StateFile::new(path.clone(), state(7, Phase::Starting));
        assert_eq!(State::read(&path), Some(state(7, Phase::Starting)));

        file.waiting_for_permission();
        assert_eq!(
            State::read(&path).unwrap().phase,
            Phase::WaitingForPermission
        );
        file.running("Headphones", 44_100, "preset music");
        file.heard();
        let s = State::read(&path).unwrap();
        assert_eq!(
            (s.phase, s.device.as_deref(), s.rate, s.heard),
            (Phase::Running, Some("Headphones"), Some(44_100), true)
        );
        assert_eq!((s.profile.as_deref(), s.profile()), (None, "preset music"));
        // A device with a profile of its own.
        file.running("AirPods", 48_000, "/c/devices/airpods.toml");
        let s = State::read(&path).unwrap();
        assert_eq!(s.profile(), "/c/devices/airpods.toml");
        file.running("Headphones", 44_100, "preset music");
        assert_eq!(State::read(&path).unwrap().profile, None);
        file.excluded(&["us.zoom.xos".into()]);
        assert_eq!(State::read(&path).unwrap().excluded, ["us.zoom.xos"]);

        file.reload_failed("profile not reloaded: bad");
        assert_eq!(
            State::read(&path).unwrap().problem.as_deref(),
            Some("profile not reloaded: bad")
        );
        file.reloaded();
        assert_eq!(State::read(&path).unwrap().problem, None);

        file.finish(&Err("it broke".into()));
        let s = State::read(&path).unwrap();
        assert_eq!(
            (s.phase, s.problem.as_deref()),
            (Phase::Failed, Some("it broke"))
        );
        // A failure is not a reload problem, so a reload leaves it.
        file.reloaded();
        assert_eq!(
            State::read(&path).unwrap().problem.as_deref(),
            Some("it broke")
        );
        file.finish(&Ok(()));
        assert_eq!(State::read(&path).unwrap().phase, Phase::Stopped);
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn the_live_lock_admits_one_holder_and_names_it() {
        let dir = TestDir::new("service-lock");
        let path = dir.0.join("sub/live.lock");
        assert_eq!(LiveLock::holder(&path).unwrap(), None);
        let lock = LiveLock::take(&path, true).unwrap().unwrap();
        let holder = Holder {
            pid: Some(std::process::id()),
            background: true,
        };
        assert_eq!(LiveLock::holder(&path).unwrap(), Some(holder));
        let Err(holder) = LiveLock::take(&path, false).unwrap() else {
            panic!("taken twice");
        };
        assert!(
            holder
                .describe()
                .starts_with("dps is already running in the background (pid ")
        );
        drop(lock);
        assert_eq!(LiveLock::holder(&path).unwrap(), None);

        let _lock = LiveLock::take(&path, false).unwrap().unwrap();
        let holder = LiveLock::holder(&path).unwrap().unwrap();
        assert!(!holder.background);
        assert!(
            holder
                .describe()
                .starts_with("dps live is already running in the foreground")
        );
        assert_eq!(
            Holder::parse("").describe(),
            "dps live is already running in the foreground"
        );
    }

    /// This test binary, which exists, stands in for the job's dps.
    fn this_exe() -> PathBuf {
        fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
    }

    fn loaded(running: bool, pid: Option<u32>, last_exit: Option<&str>) -> Loaded {
        let exe = this_exe().to_str().unwrap().to_owned();
        Loaded {
            running,
            pid,
            last_exit: last_exit.map(String::from),
            arguments: [&*exe, "live", "--service", "--preset", "music"]
                .map(String::from)
                .to_vec(),
        }
    }

    #[test]
    fn start_waits_for_the_services_own_report() {
        let log = Path::new("/nonexistent/dps.log");
        let up = loaded(true, Some(9), None);
        assert_eq!(outcome(Some(&up), None, log), Outcome::Pending);
        // A state from another process is not this run's.
        let other = state(8, Phase::Running);
        assert_eq!(outcome(Some(&up), Some(&other), log), Outcome::Pending);
        let starting = state(9, Phase::Starting);
        assert_eq!(outcome(Some(&up), Some(&starting), log), Outcome::Pending);
        let running = state(9, Phase::Running);
        assert_eq!(
            outcome(Some(&up), Some(&running), log),
            Outcome::Running(running.clone())
        );
        let waiting = state(9, Phase::WaitingForPermission);
        assert_eq!(
            outcome(Some(&up), Some(&waiting), log),
            Outcome::WaitingForPermission
        );

        // Not spawned yet.
        let not_yet = loaded(false, None, None);
        assert_eq!(outcome(Some(&not_yet), None, log), Outcome::Pending);
        // Ended: its own reason, or else launchd's.
        let ended = loaded(false, None, Some("exit code 1"));
        let failed = State {
            problem: Some("bad profile".into()),
            ..state(9, Phase::Failed)
        };
        assert_eq!(
            outcome(Some(&ended), Some(&failed), log),
            Outcome::Failed("bad profile".into())
        );
        assert_eq!(
            outcome(Some(&ended), None, log),
            Outcome::Failed("it ended with exit code 1".into())
        );
        assert!(matches!(outcome(None, None, log), Outcome::Failed(_)));
    }

    #[test]
    fn a_failed_start_quotes_the_last_line_of_the_log() {
        let dir = TestDir::new("service-log");
        let log = dir.0.join("dps.log");
        fs::write(&log, "dps live: starting\nerror: cannot read profile x\n\n").unwrap();
        let ended = loaded(false, None, Some("exit code 1"));
        assert_eq!(
            outcome(Some(&ended), None, &log),
            Outcome::Failed("it ended with exit code 1: error: cannot read profile x".into())
        );
    }

    fn snapshot(loaded: Option<Loaded>, state: Option<State>, login: Option<&[&str]>) -> Snapshot {
        Snapshot {
            loaded,
            state,
            unreadable_state: None,
            login: login.map(|a| a.iter().map(|&s| s.to_owned()).collect()),
            exe: Some(this_exe()),
        }
    }

    #[test]
    fn status_says_how_the_service_is_doing() {
        let log = Path::new("/l/dps.log");
        let status = |s: Snapshot| s.status(log);

        assert_eq!(
            status(snapshot(None, None, None)),
            "dps is not running in the background; start at login is off\n"
        );

        let running = state(9, Phase::Running);
        let since = local_time(running.started);
        let text = status(snapshot(
            Some(loaded(true, Some(9), None)),
            Some(running.clone()),
            None,
        ));
        assert_eq!(
            text,
            format!(
                "dps is running in the background since {since} (pid 9) with preset music on \
                 Speakers at 48000 Hz; start at login is off\n"
            )
        );

        let own = State {
            profile: Some("/c/devices/speakers.toml".into()),
            ..running.clone()
        };
        let text = status(snapshot(Some(loaded(true, Some(9), None)), Some(own), None));
        assert!(
            text.contains("(pid 9) with /c/devices/speakers.toml on Speakers at 48000 Hz;"),
            "{text}"
        );

        let leaving = State {
            excluded: vec!["com.google.Chrome".into(), "us.zoom.xos".into()],
            problem: Some("profile not reloaded: x".into()),
            ..running.clone()
        };
        let text = status(snapshot(
            Some(loaded(true, Some(9), None)),
            Some(leaving),
            None,
        ));
        assert!(
            text.ends_with(
                "; start at login is off\napps left alone: com.google.Chrome, us.zoom.xos\nlast \
                 warning: profile not reloaded: x\n"
            ),
            "{text}"
        );

        let warned = State {
            problem: Some("profile not reloaded: x".into()),
            ..running.clone()
        };
        let text = status(snapshot(
            Some(loaded(true, Some(9), None)),
            Some(warned),
            None,
        ));
        assert!(
            text.ends_with("\nlast warning: profile not reloaded: x\n"),
            "{text}"
        );

        let waiting = state(9, Phase::WaitingForPermission);
        let (ok, line) =
            snapshot(Some(loaded(true, Some(9), None)), Some(waiting), None).service(log);
        assert!(!ok);
        assert!(line.contains("waiting for permission"), "{line}");

        let (ok, line) = snapshot(Some(loaded(true, Some(9), None)), None, None).service(log);
        assert!(ok);
        assert_eq!(line, "starting in the background (pid 9) with preset music");

        let failed = State {
            problem: Some("bad profile".into()),
            ..state(9, Phase::Failed)
        };
        let (ok, line) = snapshot(
            Some(loaded(false, None, Some("exit code 1"))),
            Some(failed),
            None,
        )
        .service(log);
        assert!(!ok);
        assert_eq!(
            line,
            "not running in the background: it stopped with an error: bad profile"
        );

        let (ok, line) = snapshot(
            Some(loaded(false, None, Some("signal Killed: 9"))),
            None,
            None,
        )
        .service(log);
        assert!(!ok);
        assert_eq!(
            line,
            "not running in the background: it ended with signal Killed: 9; see /l/dps.log"
        );

        let (ok, line) =
            snapshot(Some(loaded(false, None, Some("exit code 0"))), None, None).service(log);
        assert!(ok);
        assert_eq!(line, "not running in the background");
    }

    #[test]
    fn status_shows_start_at_login_and_a_missing_binary() {
        let dir = TestDir::new("service-login");
        let exe = dir.0.join("dps");
        fs::write(&exe, "").unwrap();
        let exe = exe.to_str().unwrap();
        let s = snapshot(
            None,
            None,
            Some(&[exe, "live", "--service", "--preset", "voice"]),
        );
        assert_eq!(s.login(), (true, "on, with preset voice".into()));

        let gone = dir.0.join("gone/dps");
        let gone = gone.to_str().unwrap();
        let s = snapshot(None, None, Some(&[gone, "live", "--service"]));
        let (ok, line) = s.login();
        assert!(!ok);
        assert!(
            line.starts_with(&format!("on, but it runs {gone}, which no longer exists")),
            "{line}"
        );

        let mut running = loaded(true, Some(9), None);
        running.arguments[0] = gone.into();
        let text =
            snapshot(Some(running), Some(state(9, Phase::Running)), None).status(Path::new("/l"));
        assert!(
            text.contains(&format!(
                "\nnote: the background dps is {gone}, which no longer exists"
            )),
            "{text}"
        );

        let mut elsewhere = snapshot(Some(loaded(true, Some(9), None)), None, None);
        elsewhere.loaded.as_mut().unwrap().arguments[0] = exe.into();
        elsewhere.exe = Some("/other/dps".into());
        let note = elsewhere.binary_note().unwrap();
        assert!(note.contains("not this one (/other/dps)"), "{note}");
        elsewhere.exe = Some(fs::canonicalize(exe).unwrap());
        assert_eq!(elsewhere.binary_note(), None);
    }

    #[test]
    fn local_time_is_a_date_and_minute() {
        let t = local_time(1_763_000_000);
        assert_eq!(t.len(), "2025-11-13 02:13".len(), "{t}");
        assert!(t.starts_with("2025-11-1"), "{t}");
    }
}
