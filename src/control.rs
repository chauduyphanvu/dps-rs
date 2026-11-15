//! Command-line control of the settings and of a running tonewell.
//!
//! `tonewell get` and `tonewell set` read and change one value of a profile file by
//! its key path, such as `eq.gains_db[3]`; a running tonewell picks a change up
//! through its watch on the file. The paths are those of the profile's
//! TOML, so there is no list of keys here to keep up to date.
//!
//! On macOS every live run also listens on `control.sock` in tonewell's own
//! folder for what belongs to the run rather than to a file: `tonewell bypass`
//! and `tonewell use`. A client connects, writes one request line and reads one
//! reply line, both UTF-8 and ending in a newline; the run answers one
//! request per connection and closes it. The requests:
//!
//! ```text
//! bypass on | bypass off | bypass toggle
//! use preset NAME
//! use file /absolute/path.toml
//! use settings
//! ```
//!
//! The reply is `ok TEXT` or `error TEXT`, TEXT being what to tell the user.
//! The run looks for a connection on each turn of its main loop without
//! waiting, takes at most one per turn, and gives a client `READ_TIMEOUT`
//! in all to send its line.

use std::fs;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::{
    io::{self, Read, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    time::{Duration, Instant},
};

use toml::Value;
use tonewell::Profile;

use crate::config::{Dirs, Source};
#[cfg(target_os = "macos")]
use crate::{config::load_profile, presets, service::Paths};

/// A profile is checked at one rate, as `tonewell import` checks one; bands too
/// high for another are skipped there, not refused.
const CHECK_RATE: u32 = 48_000;

/// One step of a key path: a key of a table, or a position in a list.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
}

/// `eq.gains_db[3]` as its steps: names separated by dots, each followed by
/// any number of `[N]`.
fn parse_key(key: &str) -> Result<Vec<Step>, String> {
    let bad = |why: &str| {
        Err(format!(
            "{key:?} is not a key: {why}; a key is a path such as leveler.enabled or eq.gains_db[3]"
        ))
    };
    let mut steps = Vec::new();
    for part in key.split('.') {
        let (name, mut rest) = part.split_at(part.find('[').unwrap_or(part.len()));
        if name.is_empty() {
            return bad("a name is missing");
        }
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return bad(&format!("{name:?} is not a name"));
        }
        steps.push(Step::Key(name.into()));
        while let Some(inside) = rest.strip_prefix('[') {
            let Some((index, after)) = inside.split_once(']') else {
                return bad("a [ is not closed");
            };
            if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
                return bad(&format!(
                    "[{index}] is not a position, which counts from [0]"
                ));
            }
            let Ok(index) = index.parse() else {
                return bad(&format!("[{index}] is too large"));
            };
            steps.push(Step::Index(index));
            rest = after;
        }
        if !rest.is_empty() {
            return bad(&format!("{rest:?} follows a ]"));
        }
    }
    Ok(steps)
}

/// The profile as TOML values. Going through the text keeps each number as
/// the profile file writes it, where serializing straight to a value would
/// widen 56.4 to 56.400001525878906.
fn document(profile: &Profile) -> Value {
    let text = toml::to_string(profile).expect("a profile always serializes");
    Value::Table(text.parse().expect("tonewell reads the TOML it writes"))
}

/// What a value is, as messages say it.
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Float(_) | Value::Integer(_) => "a number",
        Value::Boolean(_) => "true or false",
        Value::String(_) => "a name",
        Value::Array(_) => "a list",
        Value::Table(_) => "a table",
        Value::Datetime(_) => "a date",
    }
}

/// `a, b and c`.
fn joined<'a>(items: impl Iterator<Item = &'a String>) -> String {
    let items: Vec<&str> = items.map(String::as_str).collect();
    match items.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// The values of the list at `path`, which has `len`, as messages name them.
fn positions(path: &str, len: usize) -> String {
    match len {
        0 => format!("{path} is an empty list"),
        1 => format!("{path} is a list of one value, {path}[0]"),
        _ => format!(
            "{path} is a list of {len} values, {path}[0] to {path}[{}]",
            len - 1
        ),
    }
}

/// What `steps` lead to in `document`, or why they lead nowhere, naming
/// what there is instead.
fn find<'a>(document: &'a mut Value, steps: &[Step]) -> Result<&'a mut Value, String> {
    let mut here = document;
    let mut path = String::new();
    for step in steps {
        let named = if path.is_empty() {
            "the profile".to_owned()
        } else {
            path.clone()
        };
        here = match (step, here) {
            (Step::Key(key), Value::Table(table)) => {
                if !table.contains_key(key) {
                    return Err(format!(
                        "{named} has no key {key}; its keys are {}",
                        joined(table.keys())
                    ));
                }
                &mut table[key.as_str()]
            }
            (Step::Index(_), Value::Table(table)) => {
                return Err(format!(
                    "{named} is a table, not a list; its keys are {}",
                    joined(table.keys())
                ));
            }
            (Step::Index(i), Value::Array(values)) => {
                let len = values.len();
                match values.get_mut(*i) {
                    Some(value) => value,
                    None => return Err(format!("{}; there is no [{i}]", positions(&path, len))),
                }
            }
            (Step::Key(key), Value::Array(values)) => {
                return Err(format!(
                    "{}, so it has no key {key}",
                    positions(&path, values.len())
                ));
            }
            (Step::Key(key), value) => {
                return Err(format!(
                    "{named} is {}, so it has no key {key}",
                    kind(value)
                ));
            }
            (Step::Index(i), value) => {
                return Err(format!("{named} is {}, so it has no [{i}]", kind(value)));
            }
        };
        match step {
            Step::Key(key) if path.is_empty() => path = key.clone(),
            Step::Key(key) => path = format!("{path}.{key}"),
            Step::Index(i) => path += &format!("[{i}]"),
        }
    }
    Ok(here)
}

/// A single value as `tonewell get` prints it: a name without its quotes.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(name) => name.clone(),
        value => value.to_string(),
    }
}

/// What `tonewell get` prints for `key` in `profile`: the whole profile without
/// a key, a table or a list as TOML, a single value as it is.
pub fn lookup(profile: &Profile, key: Option<&str>) -> Result<String, String> {
    let Some(key) = key else {
        return Ok(toml::to_string(profile).expect("a profile always serializes"));
    };
    let steps = parse_key(key)?;
    let mut tree = document(profile);
    Ok(match find(&mut tree, &steps)? {
        Value::Table(table) => toml::to_string(table).expect("a table always serializes"),
        value @ Value::Array(_) => format!("{value}\n"),
        value => format!("{}\n", scalar(value)),
    })
}

/// What `tonewell set` does to a value.
pub enum Edit {
    /// Replace it with this, read as the kind of value it is.
    To(String),
    /// Add this to the number.
    By(f64),
}

/// A value `edit` changed in a profile.
#[derive(Debug)]
pub struct Change {
    pub profile: Profile,
    /// The value before and after, as `tonewell get` prints them.
    pub old: String,
    pub new: String,
}

/// `text` as a finite number.
pub fn finite(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(v),
        Ok(_) => Err(format!("{text} is not a finite number")),
        Err(_) => Err(format!("{text:?} is not a number")),
    }
}

/// `profile` with the value at `key` changed by `change`, checked as a
/// running tonewell would check it.
pub fn edit(profile: &Profile, key: &str, change: &Edit) -> Result<Change, String> {
    let steps = parse_key(key)?;
    let mut tree = document(profile);
    let slot = find(&mut tree, &steps)?;
    let old = slot.clone();
    *slot = match (&old, change) {
        (Value::Table(table), _) => {
            return Err(format!(
                "{key} is a table; set one of its keys: {}",
                joined(table.keys())
            ));
        }
        (Value::Array(values), _) => {
            return Err(format!(
                "{}; set one value at a time",
                positions(key, values.len())
            ));
        }
        (Value::Float(_), Edit::To(text)) => Value::Float(
            finite(text).map_err(|_| format!("{key} is a number, so it cannot be {text:?}"))?,
        ),
        (Value::Float(old), Edit::By(delta)) => Value::Float(old + delta),
        (Value::Boolean(_), Edit::To(text)) => {
            Value::Boolean(match text.to_ascii_lowercase().as_str() {
                "true" | "on" => true,
                "false" | "off" => false,
                _ => {
                    return Err(format!(
                        "{key} is true or false (or on or off), so it cannot be {text:?}"
                    ));
                }
            })
        }
        (Value::String(_), Edit::To(text)) => Value::String(text.clone()),
        (old, Edit::By(_)) => {
            return Err(format!(
                "{key} is {}, not a number, so --by cannot change it; give the new value instead",
                kind(old)
            ));
        }
        (old, Edit::To(_)) => {
            return Err(format!(
                "{key} is {}, which tonewell set cannot set",
                kind(old)
            ));
        }
    };
    let changed: Profile = tree
        .try_into()
        .map_err(|e: toml::de::Error| format!("{key}: {}", e.message().trim()))?;
    changed.validate(CHECK_RATE)?;
    let new = scalar(find(&mut document(&changed), &steps).expect("the key is still there"));
    Ok(Change {
        profile: changed,
        old: scalar(&old),
        new,
    })
}

/// The profile file `tonewell get` and `tonewell set` work on: `profile`, else the
/// settings file, created with the default profile if missing.
fn target(profile: Option<PathBuf>, dirs: &Dirs) -> Result<(PathBuf, Profile), String> {
    let source = Source::new(profile, None, dirs)?;
    let loaded = source.load()?;
    let (Source::File(path) | Source::Settings(path)) = source else {
        unreachable!("get and set take no preset");
    };
    Ok((path, loaded))
}

/// `tonewell get`: prints the value of `key` in `profile`, else the settings file.
pub fn get(key: Option<&str>, profile: Option<PathBuf>, dirs: &Dirs) -> Result<(), String> {
    let (_, loaded) = target(profile, dirs)?;
    crate::write_stdout(&lookup(&loaded, key)?)
}

/// `tonewell set`: changes the value of `key` in `profile`, else the settings
/// file, and says how.
pub fn set(key: &str, change: Edit, profile: Option<PathBuf>, dirs: &Dirs) -> Result<(), String> {
    let (path, loaded) = target(profile, dirs)?;
    let shown = path.display();
    let Change { profile, old, new } =
        edit(&loaded, key, &change).map_err(|e| format!("{e}; {shown} was left as it was"))?;
    if profile == loaded {
        return crate::write_stdout(&format!("{key} is already {old} in {shown}\n"));
    }
    // Every comment starts with #, and no value a profile holds has one.
    let commented = fs::read_to_string(&path).is_ok_and(|text| text.contains('#'));
    crate::import::rewrite(&path, &profile)?;
    let mut said = format!("{key}: {old} -> {new} in {shown}\n");
    if commented {
        said += "rewrote the file, so comments in it are not kept\n";
    }
    crate::write_stdout(&said)
}

/// How long a live run waits, in all, for a client's request line.
#[cfg(target_os = "macos")]
const READ_TIMEOUT: Duration = Duration::from_millis(100);
/// How long a client waits for the reply. The run's main loop can be held
/// up for a while by a change of output device.
#[cfg(target_os = "macos")]
const REPLY_WAIT: Duration = Duration::from_secs(5);
/// The longest request or reply line, in bytes.
#[cfg(target_os = "macos")]
const MAX_LINE: usize = 4096;

#[cfg(target_os = "macos")]
const NOT_RUNNING: &str = "tonewell is not running; start it with `tonewell` or `tonewell start`";

/// What `tonewell bypass` asks for.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Switch {
    On,
    Off,
    Toggle,
}

/// What a client asks a live run to do.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Bypass(Switch),
    /// Play the built-in preset of this name on every output device.
    Preset(String),
    /// Play the profile file at this absolute path on every output device.
    File(PathBuf),
    /// Play the settings file, or an output device's own profile.
    Settings,
}

#[cfg(target_os = "macos")]
impl Request {
    pub fn parse(line: &str) -> Result<Self, String> {
        let unknown = || Err(format!("unknown request {line:?}"));
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        Ok(match (verb, rest) {
            ("bypass", "on") => Self::Bypass(Switch::On),
            ("bypass", "off") => Self::Bypass(Switch::Off),
            ("bypass", "toggle") => Self::Bypass(Switch::Toggle),
            ("use", "settings") => Self::Settings,
            ("use", rest) => match rest.split_once(' ') {
                Some(("preset", name)) if !name.is_empty() => Self::Preset(name.into()),
                Some(("file", path)) if Path::new(path).is_absolute() => Self::File(path.into()),
                _ => return unknown(),
            },
            _ => return unknown(),
        })
    }

    /// The request line, without its newline. A path must be UTF-8 and
    /// hold no line break.
    pub fn line(&self) -> String {
        match self {
            Self::Bypass(Switch::On) => "bypass on".into(),
            Self::Bypass(Switch::Off) => "bypass off".into(),
            Self::Bypass(Switch::Toggle) => "bypass toggle".into(),
            Self::Preset(name) => format!("use preset {name}"),
            Self::File(path) => format!("use file {}", path.display()),
            Self::Settings => "use settings".into(),
        }
    }
}

/// The reply line for `answer`.
#[cfg(target_os = "macos")]
fn reply_line(answer: &Result<String, String>) -> String {
    let (word, text) = match answer {
        Ok(text) => ("ok", text),
        Err(text) => ("error", text),
    };
    format!("{word} {}\n", text.replace(['\r', '\n'], " "))
}

/// The answer a reply line gives.
#[cfg(target_os = "macos")]
fn parse_reply(line: &str) -> Result<String, String> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let (word, text) = line.split_once(' ').unwrap_or((line, ""));
    match word {
        "ok" => Ok(text.into()),
        "error" => Err(text.into()),
        _ => Err(format!(
            "the running tonewell answered {line:?}, which this tonewell does not understand; is it another \
             version?"
        )),
    }
}

/// A live run's control socket, removed when dropped.
#[cfg(target_os = "macos")]
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

#[cfg(target_os = "macos")]
impl Server {
    /// Listens at `path`, for this user alone. Whatever is there is
    /// replaced: the caller holds the live lock, so a socket there is left
    /// over from a run that crashed.
    pub fn bind(path: &Path) -> Result<Self, String> {
        let cannot = |e: io::Error| format!("cannot listen on {}: {e}", path.display());
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(cannot(e)),
            _ => {}
        }
        let server = Self {
            listener: UnixListener::bind(path).map_err(cannot)?,
            path: path.to_owned(),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(cannot)?;
        server.listener.set_nonblocking(true).map_err(cannot)?;
        Ok(server)
    }

    /// The next client's request, if one has connected, without waiting
    /// for one; a client that is slow to send its line costs at most
    /// `READ_TIMEOUT`.
    pub fn next(&self) -> Option<Call> {
        let (stream, _) = self.listener.accept().ok()?;
        let request = read_request(&stream).and_then(|line| Request::parse(&line));
        Some(Call { stream, request })
    }
}

#[cfg(target_os = "macos")]
impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A client's request, answered once.
#[cfg(target_os = "macos")]
pub struct Call {
    stream: UnixStream,
    /// The request, or why it cannot be read.
    pub request: Result<Request, String>,
}

#[cfg(target_os = "macos")]
impl Call {
    /// Sends `answer` and hangs up; a client that has gone is not missed.
    pub fn answer(mut self, answer: Result<String, String>) {
        let _ = self.stream.write_all(reply_line(&answer).as_bytes());
    }
}

/// The line a client sends, up to its newline or the end of what it sends,
/// within `READ_TIMEOUT` of now.
#[cfg(target_os = "macos")]
fn read_request(mut stream: &UnixStream) -> Result<String, String> {
    let cannot = |e: io::Error| format!("cannot read the request: {e}");
    // On macOS an accepted socket inherits the listener's non-blocking mode.
    stream.set_nonblocking(false).map_err(cannot)?;
    stream
        .set_write_timeout(Some(READ_TIMEOUT))
        .map_err(cannot)?;
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut line = Vec::new();
    let mut buf = [0; 512];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // Each read gets only what is left, so a client sending a byte at a
        // time cannot keep the run waiting.
        stream.set_read_timeout(Some(left)).map_err(cannot)?;
        match stream.read(&mut buf) {
            Ok(0) => return String::from_utf8(line).map_err(|_| "the request is not UTF-8".into()),
            Ok(n) => line.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(cannot(e)),
        }
        if let Some(end) = line.iter().position(|&b| b == b'\n') {
            line.truncate(end);
            return String::from_utf8(line).map_err(|_| "the request is not UTF-8".into());
        }
        if line.len() > MAX_LINE {
            return Err(format!("the request is longer than {MAX_LINE} bytes"));
        }
    }
    Err(format!(
        "no request line came within {} ms",
        READ_TIMEOUT.as_millis()
    ))
}

/// Sends `request` to the live run listening at `path` and returns its
/// answer; `start`, if given, is how to start tonewell doing it instead when
/// none is running.
#[cfg(target_os = "macos")]
fn send(path: &Path, request: &Request, start: Option<&str>) -> Result<String, String> {
    let not_running = || match start {
        Some(start) => format!("{NOT_RUNNING} (`{start}` starts it that way)"),
        None => NOT_RUNNING.into(),
    };
    if !path.exists() {
        return Err(not_running());
    }
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        // A socket left over from a run that crashed refuses.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(not_running());
        }
        Err(e) => {
            return Err(format!(
                "cannot reach the running tonewell at {}: {e}",
                path.display()
            ));
        }
    };
    let cannot = |e: io::Error| {
        if matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            format!(
                "the running tonewell did not answer within {} s; it may be restarting audio or \
                 waiting for permission to record; try again",
                REPLY_WAIT.as_secs()
            )
        } else {
            format!("cannot talk to the running tonewell: {e}")
        }
    };
    stream.set_read_timeout(Some(REPLY_WAIT)).map_err(cannot)?;
    stream.set_write_timeout(Some(REPLY_WAIT)).map_err(cannot)?;
    stream
        .write_all(format!("{}\n", request.line()).as_bytes())
        .map_err(cannot)?;
    let mut reply = Vec::new();
    stream
        .take(MAX_LINE as u64)
        .read_to_end(&mut reply)
        .map_err(cannot)?;
    parse_reply(&String::from_utf8_lossy(&reply))
}

/// Sends `request` to the live run and prints its answer.
#[cfg(target_os = "macos")]
fn ask(request: &Request, start: Option<&str>, dirs: &Dirs) -> Result<(), String> {
    let path = Paths::from_dirs(dirs)?.control;
    let text = send(&path, request, start)?;
    crate::write_stdout(&format!("{text}\n"))
}

/// `tonewell bypass`.
#[cfg(target_os = "macos")]
pub fn bypass(switch: Switch, dirs: &Dirs) -> Result<(), String> {
    ask(&Request::Bypass(switch), None, dirs)
}

/// `tonewell use`: plays the preset or profile file `target` names, or with no
/// target the settings file, in the live run.
#[cfg(target_os = "macos")]
pub fn play(target: Option<PathBuf>, dirs: &Dirs) -> Result<(), String> {
    let Some(target) = target else {
        return ask(&Request::Settings, None, dirs);
    };
    if let Some(preset) = target.to_str().and_then(presets::find) {
        let start = format!("tonewell start --preset {}", preset.name);
        return ask(&Request::Preset(preset.name.into()), Some(&start), dirs);
    }
    let path = fs::canonicalize(&target).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            let names: Vec<String> = presets::PRESETS.iter().map(|p| p.name.into()).collect();
            format!(
                "{} is neither a preset ({}) nor a file",
                target.display(),
                joined(names.iter())
            )
        } else {
            format!("cannot read profile {}: {e}", target.display())
        }
    })?;
    load_profile(&path)?;
    let Some(text) = path.to_str().filter(|p| !p.contains(['\n', '\r'])) else {
        return Err(format!(
            "{} cannot be sent to the running tonewell: only a UTF-8 path without line breaks can",
            path.display()
        ));
    };
    let start = format!("tonewell start --profile {text}");
    ask(&Request::File(path), Some(&start), dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonewell::profile::{BandKind, CorrectionBand};

    fn steps(key: &str) -> Vec<Step> {
        parse_key(key).unwrap_or_else(|e| panic!("{key}: {e}"))
    }

    fn key(name: &str) -> Step {
        Step::Key(name.into())
    }

    #[test]
    fn a_key_is_names_separated_by_dots_each_with_any_positions() {
        assert_eq!(steps("output_gain_db"), [key("output_gain_db")]);
        assert_eq!(steps("leveler.enabled"), [key("leveler"), key("enabled")]);
        assert_eq!(
            steps("eq.gains_db[3]"),
            [key("eq"), key("gains_db"), Step::Index(3)]
        );
        assert_eq!(
            steps("correction.bands[0].gain_db"),
            [
                key("correction"),
                key("bands"),
                Step::Index(0),
                key("gain_db")
            ]
        );
        assert_eq!(
            steps("a[1][22]"),
            [key("a"), Step::Index(1), Step::Index(22)]
        );
        assert_eq!(
            steps("compressor1.ratio"),
            [key("compressor1"), key("ratio")]
        );
    }

    #[test]
    fn a_malformed_key_says_what_is_wrong() {
        for (bad, why) in [
            ("", "a name is missing"),
            ("eq.", "a name is missing"),
            (".eq", "a name is missing"),
            ("eq..q", "a name is missing"),
            ("[3]", "a name is missing"),
            ("eq.gains_db[3", "a [ is not closed"),
            ("eq.gains_db[]", "[] is not a position"),
            ("eq.gains_db[x]", "[x] is not a position"),
            ("eq.gains_db[-1]", "[-1] is not a position"),
            ("eq.gains_db[+1]", "[+1] is not a position"),
            ("eq.gains_db[1]x", "\"x\" follows a ]"),
            ("eq gains", "\"eq gains\" is not a name"),
            ("eq.gains-db", "\"gains-db\" is not a name"),
            ("a[99999999999999999999999]", "is too large"),
        ] {
            let e = parse_key(bad).unwrap_err();
            assert!(e.starts_with(&format!("{bad:?} is not a key: ")), "{e}");
            assert!(e.contains(why), "{why:?} not in {e:?}");
            assert!(
                e.ends_with("such as leveler.enabled or eq.gains_db[3]"),
                "{e}"
            );
        }
    }

    fn with_bands() -> Profile {
        let mut p = Profile::default();
        p.correction.bands = vec![
            CorrectionBand {
                kind: BandKind::LowShelf,
                freq_hz: 105.0,
                q: 0.7,
                gain_db: 4.5,
            },
            CorrectionBand::default(),
        ];
        p
    }

    fn get(profile: &Profile, key: &str) -> String {
        lookup(profile, Some(key)).unwrap_or_else(|e| panic!("{key}: {e}"))
    }

    #[test]
    fn get_prints_a_single_value_as_it_is() {
        let p = with_bands();
        assert_eq!(get(&p, "output_gain_db"), "7.0\n");
        assert_eq!(get(&p, "leveler.enabled"), "false\n");
        assert_eq!(get(&p, "leveler.target_lufs"), "-14.0\n");
        assert_eq!(get(&p, "compressor1.ratio"), "4.0\n");
        assert_eq!(get(&p, "eq.freqs_hz[1]"), "56.4\n");
        assert_eq!(get(&p, "eq.gains_db[3]"), "0.0\n");
        assert_eq!(get(&p, "correction.bands[0].kind"), "low_shelf\n");
        assert_eq!(get(&p, "correction.bands[0].q"), "0.7\n");
        assert_eq!(get(&p, "limiter.true_peak"), "true\n");
    }

    #[test]
    fn get_prints_a_table_or_a_list_as_toml_and_no_key_as_the_whole_profile() {
        let p = with_bands();
        assert_eq!(lookup(&p, None).unwrap(), toml::to_string(&p).unwrap());
        assert_eq!(
            get(&p, "eq.gains_db"),
            "[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]\n"
        );
        let leveler = get(&p, "leveler");
        assert_eq!(
            toml::from_str::<tonewell::profile::LevelerParams>(&leveler).unwrap(),
            p.leveler,
            "{leveler}"
        );
        assert!(leveler.contains("target_lufs = -14.0\n"), "{leveler}");
        let band = get(&p, "correction.bands[1]");
        assert_eq!(
            toml::from_str::<CorrectionBand>(&band).unwrap(),
            p.correction.bands[1]
        );
        assert_eq!(
            get(&p, "correction.bands"),
            "[{ freq_hz = 105.0, gain_db = 4.5, kind = \"low_shelf\", q = 0.7 }, { freq_hz = \
             1000.0, gain_db = 0.0, kind = \"peaking\", q = 1.0 }]\n"
        );
        let correction = get(&p, "correction");
        assert_eq!(
            toml::from_str::<tonewell::profile::CorrectionParams>(&correction).unwrap(),
            p.correction,
            "{correction}"
        );
        assert_eq!(get(&Profile::default(), "correction.bands"), "[]\n");
    }

    fn get_error(profile: &Profile, key: &str) -> String {
        lookup(profile, Some(key)).unwrap_err()
    }

    #[test]
    fn an_unknown_key_lists_the_keys_where_it_was_looked_for() {
        let p = with_bands();
        assert_eq!(
            get_error(&p, "output_gain"),
            "the profile has no key output_gain; its keys are bass, compressor1, compressor2, \
             contour, correction, crossfeed, eq, high_shelf_db, input_gain_db, leveler, limiter, \
             low_shelf_db, output_gain_db, shelf_corner_hz and stereo"
        );
        assert_eq!(
            get_error(&p, "leveler.enable"),
            "leveler has no key enable; its keys are enabled, fall_db_per_s, max_boost_db, \
             max_cut_db, rise_db_per_s and target_lufs"
        );
        assert_eq!(
            get_error(&p, "correction.bands[1].gain"),
            "correction.bands[1] has no key gain; its keys are freq_hz, gain_db, kind and q"
        );
    }

    #[test]
    fn a_key_that_does_not_fit_the_shape_says_what_is_there() {
        let p = with_bands();
        assert_eq!(
            get_error(&p, "eq.gains_db[11]"),
            "eq.gains_db is a list of 11 values, eq.gains_db[0] to eq.gains_db[10]; there is no \
             [11]"
        );
        assert_eq!(
            get_error(&Profile::default(), "correction.bands[0].q"),
            "correction.bands is an empty list; there is no [0]"
        );
        assert_eq!(
            get_error(&p, "eq.gains_db.x"),
            "eq.gains_db is a list of 11 values, eq.gains_db[0] to eq.gains_db[10], so it has no \
             key x"
        );
        assert_eq!(
            get_error(&p, "eq[0]"),
            "eq is a table, not a list; its keys are freqs_hz, gains_db and q"
        );
        assert_eq!(
            get_error(&p, "output_gain_db.x"),
            "output_gain_db is a number, so it has no key x"
        );
        assert_eq!(
            get_error(&p, "leveler.enabled[0]"),
            "leveler.enabled is true or false, so it has no [0]"
        );
        assert!(get_error(&p, "eq..q").contains("is not a key"));
    }

    fn set(profile: &Profile, key: &str, to: &str) -> Change {
        edit(profile, key, &Edit::To(to.into())).unwrap_or_else(|e| panic!("{key} {to}: {e}"))
    }

    fn set_error(profile: &Profile, key: &str, change: Edit) -> String {
        edit(profile, key, &change).unwrap_err()
    }

    #[test]
    fn set_changes_a_number_and_says_it_before_and_after() {
        let p = with_bands();
        let c = set(&p, "eq.gains_db[3]", "2.5");
        assert_eq!((c.old.as_str(), c.new.as_str()), ("0.0", "2.5"));
        let mut want = p.clone();
        want.eq.gains_db[3] = 2.5;
        assert_eq!(c.profile, want);

        // A whole number, a negative one and an exponent are all numbers.
        assert_eq!(set(&p, "output_gain_db", "3").profile.output_gain_db, 3.0);
        let c = set(&p, "leveler.target_lufs", "-16");
        assert_eq!((c.old.as_str(), c.new.as_str()), ("-14.0", "-16.0"));
        assert_eq!(set(&p, "limiter.release_ms", "1e2").new, "100.0");
        let c = set(&p, "correction.bands[1].gain_db", "-3.2");
        assert_eq!(c.profile.correction.bands[1].gain_db, -3.2);
        assert_eq!(c.new, "-3.2");
        assert_eq!(
            set(&p, "compressor2.ratio", "2").profile.compressor2.ratio,
            2.0
        );
    }

    #[test]
    fn by_adds_to_a_number_and_the_result_is_said_as_it_is_stored() {
        let p = with_bands();
        let c = edit(&p, "eq.gains_db[3]", &Edit::By(2.0)).unwrap();
        assert_eq!((c.old.as_str(), c.new.as_str()), ("0.0", "2.0"));
        let c = edit(&c.profile, "eq.gains_db[3]", &Edit::By(-1.5)).unwrap();
        assert_eq!((c.old.as_str(), c.new.as_str()), ("2.0", "0.5"));
        // 0.7 + 0.1 is 0.7999999999999999 in f64; the profile keeps f32.
        let c = edit(&p, "correction.bands[0].q", &Edit::By(0.1)).unwrap();
        assert_eq!(c.new, "0.8");
        assert_eq!(c.profile.correction.bands[0].q, 0.8);
        assert_eq!(
            set_error(&p, "leveler.enabled", Edit::By(1.0)),
            "leveler.enabled is true or false, not a number, so --by cannot change it; give the \
             new value instead"
        );
        assert!(set_error(&p, "correction.bands[0].kind", Edit::By(1.0)).contains("is a name"));
    }

    #[test]
    fn set_takes_true_false_on_and_off_for_a_switch() {
        let p = Profile::default();
        for (text, on) in [
            ("true", true),
            ("on", true),
            ("ON", true),
            ("false", false),
            ("off", false),
            ("False", false),
        ] {
            let c = set(&p, "stereo.enabled", text);
            assert_eq!(c.profile.stereo.enabled, on, "{text}");
            assert_eq!(c.new, on.to_string());
        }
        assert_eq!(set(&p, "limiter.true_peak", "off").old, "true");
        for bad in ["yes", "1", ""] {
            assert_eq!(
                set_error(&p, "stereo.enabled", Edit::To(bad.into())),
                format!("stereo.enabled is true or false (or on or off), so it cannot be {bad:?}")
            );
        }
    }

    #[test]
    fn set_takes_a_band_kind_by_name() {
        let p = with_bands();
        let c = set(&p, "correction.bands[1].kind", "high_shelf");
        assert_eq!(c.profile.correction.bands[1].kind, BandKind::HighShelf);
        assert_eq!((c.old.as_str(), c.new.as_str()), ("peaking", "high_shelf"));
        for name in ["peaking", "low_shelf"] {
            assert_eq!(set(&p, "correction.bands[0].kind", name).new, name);
        }
        let e = set_error(&p, "correction.bands[0].kind", Edit::To("notch".into()));
        assert!(e.starts_with("correction.bands[0].kind: "), "{e}");
        for named in ["notch", "peaking", "low_shelf", "high_shelf"] {
            assert!(e.contains(named), "{named} not in {e}");
        }
    }

    #[test]
    fn a_value_out_of_range_is_refused_with_the_range_and_nothing_changes() {
        let p = with_bands();
        assert_eq!(
            set_error(&p, "eq.gains_db[3]", Edit::To("30".into())),
            "eq.gains_db[3] = 30 is outside -24..=24"
        );
        assert_eq!(
            set_error(&p, "output_gain_db", Edit::By(200.0)),
            "output_gain_db = 207 is outside -120..=120"
        );
        assert_eq!(
            set_error(&p, "correction.bands[0].q", Edit::To("0".into())),
            "correction.bands[0].q = 0 is outside 0.1..=20"
        );
        assert!(
            set_error(&p, "bass.cutoff_hz", Edit::To("1e40".into()))
                .starts_with("bass.cutoff_hz = inf is outside")
        );
        // A rate-dependent limit is checked at 48 kHz.
        assert!(
            set_error(&p, "shelf_corner_hz", Edit::To("12000".into()))
                .starts_with("shelf_corner_hz = 12000 is outside 1..=11999")
        );
    }

    #[test]
    fn set_refuses_a_value_of_the_wrong_kind_and_a_whole_table_or_list() {
        let p = with_bands();
        assert_eq!(
            set_error(&p, "output_gain_db", Edit::To("loud".into())),
            "output_gain_db is a number, so it cannot be \"loud\""
        );
        for not_finite in ["nan", "inf", "-inf"] {
            let e = set_error(&p, "output_gain_db", Edit::To(not_finite.into()));
            assert!(e.contains("is a number, so it cannot be"), "{e}");
        }
        assert_eq!(
            set_error(&p, "leveler", Edit::To("1".into())),
            "leveler is a table; set one of its keys: enabled, fall_db_per_s, max_boost_db, \
             max_cut_db, rise_db_per_s and target_lufs"
        );
        assert_eq!(
            set_error(&p, "eq.q", Edit::By(1.0)),
            "eq.q is a list of 11 values, eq.q[0] to eq.q[10]; set one value at a time"
        );
        assert_eq!(
            set_error(&p, "correction.bands[2].q", Edit::To("1".into())),
            "correction.bands is a list of 2 values, correction.bands[0] to correction.bands[1]; \
             there is no [2]"
        );
        assert!(set_error(&p, "nope", Edit::To("1".into())).starts_with("the profile has no key"));
    }

    #[test]
    fn finite_numbers_may_have_a_sign() {
        assert_eq!(finite("+2"), Ok(2.0));
        assert_eq!(finite("-1.5"), Ok(-1.5));
        assert_eq!(finite("x"), Err("\"x\" is not a number".into()));
        assert_eq!(finite("NaN"), Err("NaN is not a finite number".into()));
        assert_eq!(finite("1e999"), Err("1e999 is not a finite number".into()));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod socket_tests {
    use super::*;
    use crate::config::tests::TestDir;
    use std::os::unix::fs::FileTypeExt;
    use std::thread;

    #[test]
    fn every_request_reads_back_from_its_line() {
        for request in [
            Request::Bypass(Switch::On),
            Request::Bypass(Switch::Off),
            Request::Bypass(Switch::Toggle),
            Request::Preset("night".into()),
            Request::File("/a b/my profile.toml".into()),
            Request::Settings,
        ] {
            assert_eq!(
                Request::parse(&request.line()),
                Ok(request.clone()),
                "{request:?}"
            );
        }
        assert_eq!(Request::Bypass(Switch::Toggle).line(), "bypass toggle");
        assert_eq!(
            Request::File("/x/p.toml".into()).line(),
            "use file /x/p.toml"
        );
    }

    #[test]
    fn a_malformed_request_is_named() {
        for line in [
            "",
            "bypass",
            "bypass maybe",
            "bypass on now",
            "Bypass on",
            "use",
            "use preset",
            "use preset ",
            "use file",
            "use file relative/p.toml",
            "use settings now",
            "use night",
            "status",
        ] {
            assert_eq!(
                Request::parse(line),
                Err(format!("unknown request {line:?}")),
                "{line:?}"
            );
        }
    }

    #[test]
    fn replies_are_one_line_of_ok_or_error() {
        assert_eq!(reply_line(&Ok("bypass is on".into())), "ok bypass is on\n");
        assert_eq!(reply_line(&Err("no\nway".into())), "error no way\n");
        for answer in [Ok("a b".to_owned()), Err("c".to_owned()), Ok(String::new())] {
            assert_eq!(parse_reply(&reply_line(&answer)), answer);
        }
        let e = parse_reply("hello\n").unwrap_err();
        assert!(e.contains("does not understand"), "{e}");
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_socket_is_this_users_alone_and_goes_when_dropped() {
        let dir = TestDir::new("sock");
        let path = dir.0.join("control.sock");
        let server = Server::bind(&path).unwrap();
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
        assert_eq!(mode(&path), 0o600);
        assert!(server.next().is_none(), "nobody has connected");
        drop(server);
        assert!(!path.exists());
        assert!(Server::bind(&dir.0.join("missing/control.sock")).is_err());
    }

    /// Answers the first request that comes to `server` with `answer`,
    /// waiting at most a few seconds for it.
    fn answer_one(server: &Server, answer: impl FnOnce(&Request) -> Result<String, String>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(call) = server.next() {
                let reply = call.request.clone().and_then(|r| answer(&r));
                call.answer(reply);
                return;
            }
            assert!(Instant::now() < deadline, "no request came");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_request_is_answered_and_a_stale_socket_is_replaced() {
        let dir = TestDir::new("ask");
        let path = dir.0.join("control.sock");
        // A run that crashed leaves its socket, which refuses connections.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert_eq!(
            send(&path, &Request::Settings, None),
            Err(NOT_RUNNING.into())
        );
        let server = Server::bind(&path).unwrap();
        let client = {
            let path = path.clone();
            thread::spawn(move || send(&path, &Request::Bypass(Switch::Toggle), None))
        };
        answer_one(&server, |request| {
            assert_eq!(request, &Request::Bypass(Switch::Toggle));
            Ok("bypass is on".into())
        });
        assert_eq!(client.join().unwrap(), Ok("bypass is on".into()));

        let client = {
            let path = path.clone();
            thread::spawn(move || send(&path, &Request::Preset("night".into()), None))
        };
        answer_one(&server, |_| Err("a refusal".into()));
        assert_eq!(client.join().unwrap(), Err("a refusal".into()));
    }

    #[test]
    fn a_client_that_says_nothing_or_too_little_costs_only_the_timeout() {
        let dir = TestDir::new("mute");
        let path = dir.0.join("control.sock");
        let server = Server::bind(&path).unwrap();
        let mut silent = UnixStream::connect(&path).unwrap();
        let started = Instant::now();
        let call = server.next().expect("a connection");
        let waited = started.elapsed();
        assert!(
            waited >= READ_TIMEOUT && waited < READ_TIMEOUT + Duration::from_millis(400),
            "{waited:?}"
        );
        assert_eq!(
            call.request,
            Err("no request line came within 100 ms".into())
        );
        call.answer(Err("no request line came within 100 ms".into()));
        let mut reply = String::new();
        silent.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "error no request line came within 100 ms\n");

        // One byte at a time, slower than the timeout in all.
        let dribble = {
            let path = path.clone();
            thread::spawn(move || {
                let mut stream = UnixStream::connect(&path).unwrap();
                for byte in b"bypass on\n" {
                    if stream.write_all(&[*byte]).is_err() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(30));
                }
            })
        };
        let started = Instant::now();
        let call = loop {
            if let Some(call) = server.next() {
                break call;
            }
            thread::sleep(Duration::from_millis(1));
        };
        assert!(call.request.is_err());
        assert!(started.elapsed() < READ_TIMEOUT + Duration::from_millis(400));
        drop(call);
        dribble.join().unwrap();

        // Neither held up the next client.
        let client = {
            let path = path.clone();
            thread::spawn(move || send(&path, &Request::Bypass(Switch::Off), None))
        };
        answer_one(&server, |_| Ok("bypass is off".into()));
        assert_eq!(client.join().unwrap(), Ok("bypass is off".into()));
    }

    #[test]
    fn a_line_that_does_not_parse_or_ends_without_a_newline_is_still_read() {
        let dir = TestDir::new("line");
        let path = dir.0.join("control.sock");
        let server = Server::bind(&path).unwrap();
        for (sent, read) in [
            (&b"bypass on"[..], Ok(Request::Bypass(Switch::On))),
            (b"use settings\nmore\n", Ok(Request::Settings)),
            (
                b"frobnicate\n",
                Err("unknown request \"frobnicate\"".to_owned()),
            ),
            (b"\xff\n", Err("the request is not UTF-8".to_owned())),
        ] {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(sent).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let call = server.next().expect("a connection");
            assert_eq!(call.request, read, "{sent:?}");
        }
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(&[b'x'; MAX_LINE + 600]).unwrap();
        let call = server.next().expect("a connection");
        assert_eq!(
            call.request,
            Err(format!("the request is longer than {MAX_LINE} bytes"))
        );
    }

    #[test]
    fn with_nothing_listening_the_client_says_how_to_start_tonewell() {
        let dir = TestDir::new("none");
        let path = dir.0.join("control.sock");
        assert_eq!(
            send(&path, &Request::Bypass(Switch::On), None),
            Err("tonewell is not running; start it with `tonewell` or `tonewell start`".into())
        );
        assert_eq!(
            send(
                &path,
                &Request::Preset("night".into()),
                Some("tonewell start --preset night")
            ),
            Err(
                "tonewell is not running; start it with `tonewell` or `tonewell start` (`tonewell start --preset \
                 night` starts it that way)"
                    .into()
            )
        );
    }
}
