//! `dps doctor`: checks what dps needs here and says how to fix what is
//! missing.

use std::io::{self, Write};
use std::process::{Command, ExitCode, Stdio};

use crate::config::{Dirs, Source, load_profile};

/// One check's result.
struct Check {
    name: &'static str,
    ok: bool,
    /// Whether live mode needs it; the exit status counts only these.
    needed: bool,
    text: String,
}

impl Check {
    fn new(name: &'static str, ok: bool, text: impl Into<String>) -> Self {
        Self {
            name,
            ok,
            needed: true,
            text: text.into(),
        }
    }

    fn line(&self) -> String {
        let verdict = if self.ok { "ok" } else { "problem" };
        format!("{verdict:<8} {}: {}\n", self.name, self.text)
    }
}

/// Prints each check as it finishes; fails if live mode cannot work. A
/// source that cannot be found is the settings check's problem.
pub fn run(source: Result<Source, String>, dirs: &Dirs) -> ExitCode {
    let mut healthy = true;
    let mut report = |check: Check| {
        healthy &= check.ok || !check.needed;
        let _ = io::stdout().write_all(check.line().as_bytes());
    };
    #[cfg(target_os = "macos")]
    {
        report(macos_check(macos_version()));
        let output = macos::default_output();
        let rate = output.as_ref().ok().map(|(_, rate)| *rate);
        report(macos::output_check(&output));
        report(settings_check(&source, rate.unwrap_or(48_000)));
        report(macos::capture_check(dirs));
        report(ffmpeg_check(&[found("ffmpeg"), found("ffprobe")]));
        for check in macos::service_checks(dirs) {
            report(check);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = dirs;
        report(settings_check(&source, 48_000));
        report(ffmpeg_check(&[found("ffmpeg"), found("ffprobe")]));
    }
    if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn settings_check(source: &Result<Source, String>, rate: u32) -> Check {
    let source = match source {
        Ok(source) => source,
        Err(e) => return Check::new("settings", false, e.clone()),
    };
    let name = source.name();
    let profile = match source {
        Source::Settings(path) if !path.exists() => {
            return Check::new(
                "settings",
                true,
                format!(
                    "{} does not exist yet; dps creates it with the default profile",
                    path.display()
                ),
            );
        }
        Source::Settings(path) | Source::File(path) => load_profile(path),
        Source::Preset(preset) => Ok(preset.profile()),
    };
    let fix = match source {
        Source::Preset(_) => String::new(),
        _ => format!("; fix: correct {name} (`dps profile` prints the default with every key)"),
    };
    match profile {
        Ok(profile) => match profile.validate(rate) {
            Ok(()) => {
                let skipped = profile
                    .describe_skipped_eq_bands(rate)
                    .map(|bands| format!(", though at this rate the EQ skips {bands}"))
                    .unwrap_or_default();
                Check::new(
                    "settings",
                    true,
                    format!("{name} is valid at {rate} Hz{skipped}"),
                )
            }
            Err(e) => Check::new(
                "settings",
                false,
                format!("{name} does not fit {rate} Hz: {e}{fix}"),
            ),
        },
        Err(e) => Check::new("settings", false, format!("{e}{fix}")),
    }
}

/// What running `program -version` showed.
#[derive(Debug, PartialEq)]
enum Found {
    Runs,
    Missing,
    /// It started but failed, as a broken install does.
    Fails(String),
}

fn found(program: &'static str) -> (&'static str, Found) {
    let status = Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let found = match status {
        Ok(status) if status.success() => Found::Runs,
        Ok(status) => Found::Fails(match status.code() {
            Some(code) => format!("exit code {code}"),
            None => "a signal".into(),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Found::Missing,
        Err(e) => Found::Fails(e.to_string()),
    };
    (program, found)
}

fn ffmpeg_check(programs: &[(&str, Found)]) -> Check {
    let missing: Vec<&str> = programs
        .iter()
        .filter(|(_, found)| *found == Found::Missing)
        .map(|(name, _)| *name)
        .collect();
    let failing: Vec<String> = programs
        .iter()
        .filter_map(|(name, found)| match found {
            Found::Fails(how) => Some(format!("`{name} -version` failed with {how}")),
            _ => None,
        })
        .collect();
    let mut problems = Vec::new();
    if !missing.is_empty() {
        problems.push(format!("{} not on PATH", missing.join(" and ")));
    }
    problems.extend(failing);
    let mut check = if problems.is_empty() {
        Check::new(
            "ffmpeg",
            true,
            "ffmpeg and ffprobe run (needed only for files other than WAV)",
        )
    } else {
        let fix = if missing.is_empty() {
            "brew reinstall ffmpeg"
        } else {
            "brew install ffmpeg"
        };
        Check::new(
            "ffmpeg",
            false,
            format!(
                "{} (needed only for files other than WAV); fix: {fix}",
                problems.join("; ")
            ),
        )
    };
    check.needed = false;
    check
}

/// "27.2" or "14.2.1" as (major, minor).
#[cfg(any(target_os = "macos", test))]
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    Some((major, minor))
}

#[cfg(target_os = "macos")]
fn macos_version() -> Result<String, String> {
    let out = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .map_err(|e| format!("cannot run sw_vers: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(any(target_os = "macos", test))]
fn macos_check(version: Result<String, String>) -> Check {
    let version = match version {
        Ok(version) => version,
        Err(e) => return Check::new("macOS", false, format!("{e}; fix: check /usr/bin/sw_vers")),
    };
    match parse_version(&version) {
        Some(v) if v >= (14, 2) => Check::new(
            "macOS",
            true,
            format!("{version} (system audio taps need 14.2 or later)"),
        ),
        Some(_) => Check::new(
            "macOS",
            false,
            format!("{version} has no system audio taps; fix: update to macOS 14.2 or later"),
        ),
        None => Check::new(
            "macOS",
            false,
            format!("cannot read the version {version:?}; fix: check /usr/bin/sw_vers"),
        ),
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Check;
    use crate::config::Dirs;
    use crate::live::{self, Capture};
    use crate::service::{Launchctl, LiveLock, Paths, Snapshot};

    /// The default output device's name and rate.
    pub fn default_output() -> Result<(String, u32), String> {
        // Tests name the device here, as `name@rate` or else an error; it is
        // not meant for users.
        let Some(test) = std::env::var_os("DPS_TEST_OUTPUT") else {
            return live::default_output();
        };
        let test = test.to_string_lossy();
        match test.rsplit_once('@').map(|(n, r)| (n, r.parse())) {
            Some((name, Ok(rate))) => Ok((name.to_owned(), rate)),
            _ => Err(test.into_owned()),
        }
    }

    pub fn output_check(output: &Result<(String, u32), String>) -> Check {
        match output {
            Ok((name, rate)) => Check::new("output", true, format!("{name} at {rate} Hz")),
            Err(e) => Check::new(
                "output",
                false,
                format!("{e}; fix: choose an output in System Settings > Sound"),
            ),
        }
    }

    pub fn capture_check(dirs: &Dirs) -> Check {
        let holder = Paths::from_dirs(dirs).and_then(|paths| LiveLock::holder(&paths.lock));
        match holder {
            Ok(Some(holder)) => {
                return Check::new(
                    "capture",
                    true,
                    format!(
                        "not checked: {}, and a second tap would hear its output",
                        holder.describe()
                    ),
                );
            }
            Ok(None) => {}
            Err(e) => return Check::new("capture", false, e),
        }
        capture_verdict(live::check_capture())
    }

    pub fn capture_verdict(capture: Capture) -> Check {
        const FROM_HERE: &str = "(for apps started from here; the background service needs dps \
                                 itself allowed, which `dps start` checks)";
        match capture {
            Capture::Ran { heard: true, .. } => Check::new(
                "capture",
                true,
                format!("a test tap heard system audio {FROM_HERE}"),
            ),
            Capture::Ran {
                heard: false,
                others_playing: false,
            } => Check::new(
                "capture",
                true,
                format!(
                    "a test tap started; nothing was playing, so it could not hear anything \
                     {FROM_HERE}"
                ),
            ),
            Capture::Ran {
                heard: false,
                others_playing: true,
            } => Check::new(
                "capture",
                false,
                format!(
                    "a test tap heard only silence while other apps played, so this app \
                     probably lacks the permission; fix: {}",
                    live::PERMISSION_HINT
                ),
            ),
            Capture::Waiting => Check::new(
                "capture",
                false,
                "macOS is asking for permission to record system audio; fix: answer the \
                 prompt, then run dps doctor again",
            ),
            Capture::Failed(e) => Check::new(
                "capture",
                false,
                format!(
                    "{e}; fix: if this is a permission problem, {}",
                    live::PERMISSION_HINT
                ),
            ),
        }
    }

    pub fn service_checks(dirs: &Dirs) -> Vec<Check> {
        let snapshot = Paths::from_dirs(dirs)
            .and_then(|paths| Snapshot::read(&paths, &Launchctl::new()).map(|s| (s, paths)));
        let (snapshot, paths) = match snapshot {
            Ok(found) => found,
            Err(e) => return vec![Check::new("service", false, e)],
        };
        let (ok, service) = snapshot.service(&paths.log);
        let fix = if ok || service.contains("fix: ") {
            String::new()
        } else {
            format!("; fix: see {}, then `dps start` again", paths.log.display())
        };
        let (login_ok, login) = snapshot.login();
        vec![
            Check::new("service", ok, format!("{service}{fix}")),
            Check::new("start at login", login_ok, login),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::TestDir;
    use crate::presets;

    #[test]
    fn lines_say_ok_or_problem_and_only_needed_problems_fail() {
        let check = Check::new("settings", false, "x is wrong; fix: y");
        assert_eq!(check.line(), "problem  settings: x is wrong; fix: y\n");
        assert_eq!(
            Check::new("output", true, "Speakers at 48000 Hz").line(),
            "ok       output: Speakers at 48000 Hz\n"
        );
        let missing = ffmpeg_check(&[("ffmpeg", Found::Missing), ("ffprobe", Found::Missing)]);
        assert!(!missing.ok && !missing.needed);
        assert_eq!(
            missing.text,
            "ffmpeg and ffprobe not on PATH (needed only for files other than WAV); fix: brew \
             install ffmpeg"
        );
        let one = ffmpeg_check(&[("ffmpeg", Found::Runs), ("ffprobe", Found::Missing)]);
        assert!(one.text.starts_with("ffprobe not on PATH"), "{}", one.text);
        let ok = ffmpeg_check(&[("ffmpeg", Found::Runs), ("ffprobe", Found::Runs)]);
        assert!(ok.ok);
        assert_eq!(
            ok.text,
            "ffmpeg and ffprobe run (needed only for files other than WAV)"
        );
        // One that starts but fails is no better than none.
        let broken = ffmpeg_check(&[
            ("ffmpeg", Found::Fails("exit code 134".into())),
            ("ffprobe", Found::Runs),
        ]);
        assert!(!broken.ok && !broken.needed);
        assert_eq!(
            broken.text,
            "`ffmpeg -version` failed with exit code 134 (needed only for files other than \
             WAV); fix: brew reinstall ffmpeg"
        );
    }

    #[test]
    fn macos_versions_from_14_2_have_taps() {
        assert_eq!(parse_version("27.2"), Some((27, 2)));
        assert_eq!(parse_version("14.2.1\n"), Some((14, 2)));
        assert_eq!(parse_version("15"), Some((15, 0)));
        assert_eq!(parse_version("x.1"), None);
        assert!(macos_check(Ok("14.2".into())).ok);
        assert!(macos_check(Ok("27.2".into())).ok);
        let old = macos_check(Ok("14.1.2".into()));
        assert!(!old.ok);
        assert!(
            old.text.contains("fix: update to macOS 14.2 or later"),
            "{}",
            old.text
        );
        assert!(!macos_check(Ok("".into())).ok);
        assert!(!macos_check(Err("cannot run sw_vers: gone".into())).ok);
    }

    #[test]
    fn settings_are_parsed_and_validated_at_the_output_rate() {
        let dir = TestDir::new("doctor-settings");
        let dirs = dir.dirs();
        let settings = dirs.settings_file().unwrap();

        let check = settings_check(&Ok(Source::Settings(settings.clone())), 48_000);
        assert!(check.ok);
        assert!(
            check
                .text
                .ends_with("does not exist yet; dps creates it with the default profile")
        );
        assert!(!settings.exists(), "the doctor creates nothing");

        // Once it exists, it is read like any profile.
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs_write(&settings, "output_gain_db = 900.0\n");
        let check = settings_check(&Ok(Source::Settings(settings.clone())), 48_000);
        assert!(!check.ok);
        assert!(check.text.contains("output_gain_db"), "{}", check.text);
        assert!(
            check.text.ends_with(
                "; fix: correct {} (`dps profile` prints the default with every key)"
                    .replace("{}", &settings.display().to_string())
                    .as_str()
            ),
            "{}",
            check.text
        );

        let mine = dir.0.join("mine.toml");
        fs_write(&mine, "output_gain_db = -2.0\n");
        let check = settings_check(&Ok(Source::File(mine.clone())), 48_000);
        assert_eq!(
            check.text,
            format!("{} is valid at 48000 Hz", mine.display())
        );

        fs_write(&mine, "nonsense = 1\n");
        let check = settings_check(&Ok(Source::File(mine.clone())), 48_000);
        assert!(!check.ok);
        assert!(check.text.contains("nonsense"), "{}", check.text);
        assert!(
            check
                .text
                .ends_with("(`dps profile` prints the default with every key)")
        );

        // The top EQ bands do not fit under 4 kHz, so they are skipped.
        let voice = presets::find("voice").unwrap();
        let check = settings_check(&Ok(Source::Preset(voice)), 8_000);
        assert!(check.ok, "{}", check.text);
        assert_eq!(
            check.text,
            "preset voice is valid at 8000 Hz, though at this rate the EQ skips eq.freqs_hz[8] \
             (4698 Hz), eq.freqs_hz[9] (8839 Hz) and eq.freqs_hz[10] (16000 Hz)"
        );
        assert_eq!(
            settings_check(&Ok(Source::Preset(voice)), 44_100).text,
            "preset voice is valid at 44100 Hz"
        );
        // A 1 kHz shelf corner does not fit under a quarter of 3 kHz; a
        // preset cannot be corrected, so there is no fix to suggest.
        let check = settings_check(&Ok(Source::Preset(voice)), 3_000);
        assert!(!check.ok);
        assert!(
            check
                .text
                .starts_with("preset voice does not fit 3000 Hz: shelf_corner_hz"),
            "{}",
            check.text
        );
        assert!(!check.text.contains("fix:"), "{}", check.text);

        let check = settings_check(
            &Err("cannot locate the settings file: HOME is not set".into()),
            48_000,
        );
        assert!(!check.ok);
        assert_eq!(
            check.text,
            "cannot locate the settings file: HOME is not set"
        );
    }

    fn fs_write(path: &std::path::Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_results_are_explained() {
        use crate::live::Capture;
        let verdict = macos::capture_verdict;
        let heard = verdict(Capture::Ran {
            heard: true,
            others_playing: true,
        });
        assert!(heard.ok && heard.text.starts_with("a test tap heard system audio"));
        let quiet = verdict(Capture::Ran {
            heard: false,
            others_playing: false,
        });
        assert!(quiet.ok && quiet.text.contains("nothing was playing"));
        let silent = verdict(Capture::Ran {
            heard: false,
            others_playing: true,
        });
        assert!(!silent.ok && silent.text.contains("fix: allow your terminal app"));
        let waiting = verdict(Capture::Waiting);
        assert!(!waiting.ok && waiting.text.contains("fix: answer the prompt"));
        let failed = verdict(Capture::Failed("cannot create the tap".into()));
        assert!(
            !failed.ok
                && failed
                    .text
                    .starts_with("cannot create the tap; fix: if this")
        );
    }
}
