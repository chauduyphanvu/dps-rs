//! `dps get` and `dps set`, and on macOS `dps bypass` and `dps use` with no
//! dps running, through the built binary. Its home and config directories
//! are in a per-test directory, and the output devices come from
//! `DPS_TEST_DEVICES`, so no test touches the real settings or audio system.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use dps::Profile;

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(test: &str) -> Self {
        let root = std::env::temp_dir().join(format!("dps-control-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for sub in ["work", "config", "home"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self { root }
    }

    fn dps(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dps"))
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("HOME", self.root.join("home"))
            .env(
                "DPS_TEST_DEVICES",
                "*BuiltInSpeakerDevice=MacBook Pro Speakers;AA:output=AirPods Pro",
            )
            .output()
            .unwrap()
    }

    fn dir(&self) -> PathBuf {
        self.root.join("config/dps")
    }

    fn settings(&self) -> PathBuf {
        self.dir().join("profile.toml")
    }

    fn work(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root.join("work").join(name);
        fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(out: Output) -> String {
    assert!(out.status.success(), "dps failed: {}", stderr(&out));
    stdout(&out)
}

/// Exit code 1, nothing on stdout, and one stderr line `error: <message>`.
fn fails(out: Output, message: &str) {
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert_eq!(stdout(&out), "");
    assert_eq!(stderr(&out), format!("error: {message}\n"));
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn profile(path: &Path) -> Profile {
    toml::from_str(&read(path)).unwrap()
}

#[test]
fn set_then_get_round_trips_and_creates_a_missing_settings_file() {
    let dir = TempDir::new("round-trip");
    let settings = dir.settings();
    let out = dir.dps(&["set", "eq.gains_db[3]", "2.5"]);
    assert_eq!(
        stderr(&out),
        format!(
            "created settings file {} with the default profile\n",
            settings.display()
        )
    );
    assert_eq!(
        ok(out),
        format!("eq.gains_db[3]: 0.0 -> 2.5 in {}\n", settings.display())
    );
    let mut want = Profile::default();
    want.eq.gains_db[3] = 2.5;
    assert_eq!(profile(&settings), want);
    assert_eq!(ok(dir.dps(&["get", "eq.gains_db[3]"])), "2.5\n");

    // The same value again leaves the file alone.
    let before = read(&settings);
    assert_eq!(
        ok(dir.dps(&["set", "eq.gains_db[3]", "2.5"])),
        format!("eq.gains_db[3] is already 2.5 in {}\n", settings.display())
    );
    assert_eq!(read(&settings), before);

    for (key, value, got) in [
        ("leveler.enabled", "on", "true"),
        ("correction.bands", "x", ""),
        ("limiter.true_peak", "false", "false"),
    ] {
        let out = dir.dps(&["set", key, value]);
        if got.is_empty() {
            assert_eq!(out.status.code(), Some(1), "{key}");
            continue;
        }
        ok(out);
        assert_eq!(ok(dir.dps(&["get", key])), format!("{got}\n"));
    }
}

#[test]
fn by_adds_a_negative_or_positive_delta() {
    let dir = TempDir::new("by");
    let path = dir.settings().display().to_string();
    ok(dir.dps(&["get"]));
    assert_eq!(
        ok(dir.dps(&["set", "eq.gains_db[3]", "--by", "+2"])),
        format!("eq.gains_db[3]: 0.0 -> 2.0 in {path}\n")
    );
    assert_eq!(
        ok(dir.dps(&["set", "eq.gains_db[3]", "--by", "-1.5"])),
        format!("eq.gains_db[3]: 2.0 -> 0.5 in {path}\n")
    );
    assert_eq!(
        ok(dir.dps(&["set", "--by=-3", "output_gain_db"])),
        format!("output_gain_db: 7.0 -> 4.0 in {path}\n")
    );
    assert_eq!(ok(dir.dps(&["get", "eq.gains_db[3]"])), "0.5\n");
    let out = dir.dps(&["set", "eq.gains_db[3]", "--by", "x"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let out = dir.dps(&["set", "eq.gains_db[3]", "1", "--by", "1"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn a_negative_value_is_taken_as_the_value() {
    let dir = TempDir::new("negative");
    let path = dir.settings().display().to_string();
    ok(dir.dps(&["get"]));
    assert_eq!(
        ok(dir.dps(&["set", "leveler.target_lufs", "-16"])),
        format!("leveler.target_lufs: -14.0 -> -16.0 in {path}\n")
    );
    assert_eq!(ok(dir.dps(&["get", "leveler.target_lufs"])), "-16.0\n");
}

#[test]
fn a_refused_value_leaves_the_file_byte_for_byte() {
    let dir = TempDir::new("refused");
    let settings = dir.settings();
    fs::create_dir_all(dir.dir()).unwrap();
    let text = "# my settings\noutput_gain_db = 1.0\n";
    fs::write(&settings, text).unwrap();
    let path = settings.display();
    fails(
        dir.dps(&["set", "eq.gains_db[3]", "30"]),
        &format!("eq.gains_db[3] = 30 is outside -24..=24; {path} was left as it was"),
    );
    fails(
        dir.dps(&["set", "output_gain_db", "--by", "200"]),
        &format!("output_gain_db = 201 is outside -120..=120; {path} was left as it was"),
    );
    fails(
        dir.dps(&["set", "output_gain_db", "loud"]),
        &format!("output_gain_db is a number, so it cannot be \"loud\"; {path} was left as it was"),
    );
    fails(
        dir.dps(&["set", "eq", "1"]),
        &format!(
            "eq is a table; set one of its keys: freqs_hz, gains_db and q; {path} was left as it was"
        ),
    );
    let out = dir.dps(&["set", "leveler.enable", "on"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out)
            .starts_with("error: leveler has no key enable; its keys are enabled, fall_db_per_s, "),
        "{}",
        stderr(&out)
    );
    let out = dir.dps(&["get", "eq.gains_db[11]"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(read(&settings), text);
}

#[test]
fn profile_names_another_file_and_a_comment_lost_is_said() {
    let dir = TempDir::new("profile");
    let mine = dir.work("mine.toml", "# for the desk\noutput_gain_db = -1.0\n");
    let mine_arg = mine.to_str().unwrap();
    assert_eq!(
        ok(dir.dps(&["set", "-p", mine_arg, "output_gain_db", "-2"])),
        format!(
            "output_gain_db: -1.0 -> -2.0 in {mine_arg}\nrewrote the file, so comments in it are \
             not kept\n"
        )
    );
    assert_eq!(profile(&mine).output_gain_db, -2.0);
    assert!(!read(&mine).contains('#'));
    assert_eq!(
        ok(dir.dps(&["get", "--profile", mine_arg, "output_gain_db"])),
        "-2.0\n"
    );
    assert!(!dir.settings().exists());
    let missing = dir.root.join("work/missing.toml");
    let out = dir.dps(&["get", "-p", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).starts_with("error: cannot read profile "));
}

#[test]
fn get_prints_a_table_a_list_or_the_whole_profile_as_toml() {
    let dir = TempDir::new("get");
    let whole = ok(dir.dps(&["get"]));
    assert_eq!(whole, ok(dir.dps(&["profile"])));
    let leveler = ok(dir.dps(&["get", "leveler"]));
    assert_eq!(
        leveler,
        "enabled = false\nfall_db_per_s = 4.0\nmax_boost_db = 12.0\nmax_cut_db = 12.0\n\
         rise_db_per_s = 1.0\ntarget_lufs = -14.0\n"
    );
    assert_eq!(
        ok(dir.dps(&["get", "eq.freqs_hz"])),
        "[30.0, 56.4, 106.0, 199.0, 375.0, 705.0, 1327.0, 2497.0, 4698.0, 8839.0, 16000.0]\n"
    );
    assert_eq!(ok(dir.dps(&["get", "correction.bands"])), "[]\n");
}

#[cfg(target_os = "macos")]
#[test]
fn set_writes_to_the_default_outputs_own_profile() {
    let dir = TempDir::new("device");
    fs::create_dir_all(dir.dir().join("devices")).unwrap();
    fs::write(
        dir.dir().join("config.toml"),
        "[devices.BuiltInSpeakerDevice]\nname = \"MacBook Pro Speakers\"\nprofile = \
         \"devices/speakers.toml\"\n",
    )
    .unwrap();
    let speakers = dir.dir().join("devices/speakers.toml");
    fs::write(&speakers, "output_gain_db = -1.0\n").unwrap();
    fs::write(dir.settings(), "output_gain_db = 3.0\n").unwrap();

    assert_eq!(
        ok(dir.dps(&["set", "output_gain_db", "-2"])),
        format!("output_gain_db: -1.0 -> -2.0 in {}\n", speakers.display())
    );
    assert_eq!(profile(&speakers).output_gain_db, -2.0);
    assert_eq!(read(&dir.settings()), "output_gain_db = 3.0\n");
    assert_eq!(ok(dir.dps(&["get", "output_gain_db"])), "-2.0\n");
    let settings = dir.settings();
    assert_eq!(
        ok(dir.dps(&["get", "-p", settings.to_str().unwrap(), "output_gain_db"])),
        "3.0\n"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn bypass_and_use_with_nothing_running_say_how_to_start_dps() {
    let dir = TempDir::new("not-running");
    let not_running = "dps is not running; start it with `dps` or `dps start`";
    for args in [
        &["bypass"][..],
        &["bypass", "on"],
        &["bypass", "toggle"],
        &["use", "--settings"],
    ] {
        fails(dir.dps(args), not_running);
    }
    fails(
        dir.dps(&["use", "night"]),
        &format!("{not_running} (`dps start --preset night` starts it that way)"),
    );
    let mine = dir.work("mine.toml", "output_gain_db = -2.0\n");
    let canonical = fs::canonicalize(&mine).unwrap();
    fails(
        dir.dps(&["use", mine.to_str().unwrap()]),
        &format!(
            "{not_running} (`dps start --profile {}` starts it that way)",
            canonical.display()
        ),
    );
    fails(
        dir.dps(&["use", "nite"]),
        "nite is neither a preset (headphones, laptop-speakers, music, night and voice) nor a \
         file",
    );
    let bad = dir.work("bad.toml", "output_gain_db = \"loud\"\n");
    let out = dir.dps(&["use", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("bad.toml:1:"), "{}", stderr(&out));
    assert!(!dir.root.join("home/Library").exists());
}
