//! Drives `tonewell calibrate` through the built binary as far as it goes
//! without audio: its arguments and help. Nothing here plays or records.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

/// A per-test directory, removed on drop, holding the binary's home and
/// config directories so that no test touches the real settings file.
struct TempDir(PathBuf);

impl TempDir {
    fn new(test: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("tonewell-calibrate-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for sub in ["config", "home"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self(root)
    }

    fn tonewell(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tonewell"))
            .args(args)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("HOME", self.0.join("home"))
            .output()
            .unwrap()
    }

    /// Nothing was created under the home or config directories.
    fn untouched(&self) -> bool {
        ["config", "home"]
            .iter()
            .all(|sub| fs::read_dir(self.0.join(sub)).unwrap().next().is_none())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_level_outside_the_range_is_refused_with_the_range() {
    let dir = TempDir::new("level");
    for level in ["-5", "-61", "0", "-12.5e3", "loud", "nan"] {
        let out = dir.tonewell(&["calibrate", "--level", level, "--dry-run"]);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(2), "{level}: {err}");
        assert!(
            err.contains(&format!("invalid value '{level}' for '--level <DB>'")),
            "{level}: {err}"
        );
        assert!(
            err.contains("expected a level in dBFS from -60 to -12"),
            "{level}: {err}"
        );
        assert!(out.stdout.is_empty());
    }
    // `=` works too, and so does a value with no flag after it.
    let out = dir.tonewell(&["calibrate", "--level=-70"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("from -60 to -12"));
    assert!(dir.untouched());
}

#[test]
fn the_help_explains_the_measurement() {
    let dir = TempDir::new("help");
    let out = dir.tonewell(&["calibrate", "--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let help = stdout(&out);
    for needle in [
        "microphone",
        "100 Hz",
        "8 kHz",
        "[correction]",
        "Ctrl-C",
        "system volume",
        "tonewell stop",
        "--profile <FILE.toml>",
        "--level <DB>",
        "[default: -30]",
        "--dry-run",
    ] {
        assert!(help.contains(needle), "{needle:?} not in {help}");
    }
    let out = dir.tonewell(&["--help"]);
    let help = stdout(&out);
    assert!(help.contains("calibrate"), "{help}");
    assert!(help.contains("tonewell calibrate "), "{help}");
    assert!(dir.untouched());
}
