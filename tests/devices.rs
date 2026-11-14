//! `dps devices`, and the commands that follow the default output device,
//! driven through the built binary with the output devices given in
//! `DPS_TEST_DEVICES`, so no test reads the real audio system.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DEVICES: &str =
    "*BuiltInSpeakerDevice=MacBook Pro Speakers;AA:output=AirPods Pro;BB:output=AirPods Max";

/// A per-test directory holding the settings, config and home directories.
struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(test: &str) -> Self {
        let root = std::env::temp_dir().join(format!("dps-devices-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for sub in ["work", "config", "home"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // launchctl answers that nothing is loaded.
        let stub = root.join("launchctl");
        fs::write(
            &stub,
            "#!/bin/sh\necho 'Could not find service \"dev.dps.live\"' >&2\nexit 113\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn dps(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dps"))
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("HOME", self.root.join("home"))
            .env("DPS_LAUNCHCTL", self.root.join("launchctl"))
            .env("DPS_TEST_DEVICES", DEVICES)
            .env("DPS_TEST_OUTPUT", "MacBook Pro Speakers@48000")
            .env("DPS_TEST_OTHERS_PLAYING", "0")
            .output()
            .unwrap()
    }

    fn dir(&self) -> PathBuf {
        self.root.join("config/dps")
    }

    fn settings(&self) -> PathBuf {
        self.dir().join("profile.toml")
    }

    fn config(&self) -> PathBuf {
        self.dir().join("config.toml")
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

/// Exit code 1 and one stderr line starting `error: ` that contains `needle`.
fn fails(out: Output, needle: &str) -> String {
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with("error: ") && err.lines().count() == 1,
        "{err}"
    );
    assert!(err.contains(needle), "{needle:?} not in {err}");
    err
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn devices_lists_each_output_with_the_profile_it_plays_with() {
    let dir = TempDir::new("list");
    let text = ok(dir.dps(&["devices"]));
    assert_eq!(
        text,
        format!(
            "  output device         UID                   profile
* MacBook Pro Speakers  BuiltInSpeakerDevice  settings file
  AirPods Pro           AA:output             settings file
  AirPods Max           BB:output             settings file

* default output. Profile paths are relative to {}; `dps devices assign` gives a device a profile of its own.
",
            dir.dir().display()
        )
    );
    assert!(!dir.dir().exists(), "listing created files");
}

#[test]
fn assign_copies_the_profile_the_device_plays_with_and_maps_it() {
    let dir = TempDir::new("assign");
    let out = dir.dps(&["devices", "assign"]);
    let said = ok(out);
    let own = dir.dir().join("devices/macbook-pro-speakers.toml");
    assert_eq!(
        said,
        format!(
            "wrote {} from {}\nMacBook Pro Speakers (BuiltInSpeakerDevice) now uses it in live \
             mode; rewrote {} (comments in it are not kept); a running dps switches on its own\n",
            own.display(),
            dir.settings().display(),
            dir.config().display()
        )
    );
    // The settings file was created with the default profile to copy it.
    assert_eq!(read(&own), read(&dir.settings()));
    assert_eq!(
        read(&dir.config()),
        "exclude = []\n\n[devices.BuiltInSpeakerDevice]\nname = \"MacBook Pro Speakers\"\nprofile \
         = \"devices/macbook-pro-speakers.toml\"\n"
    );
    let list = ok(dir.dps(&["devices"]));
    assert!(
        list.contains(
            "* MacBook Pro Speakers  BuiltInSpeakerDevice  devices/macbook-pro-speakers.toml\n"
        ),
        "{list}"
    );

    // A hand-edited settings file is copied as it is, comments and all.
    fs::write(dir.settings(), "# mine\noutput_gain_db = -4.0\n").unwrap();
    ok(dir.dps(&["devices", "assign", "--device", "AA:output"]));
    assert_eq!(
        read(&dir.dir().join("devices/airpods-pro.toml")),
        "# mine\noutput_gain_db = -4.0\n"
    );
    // Assigning again keeps the file there, edits and all.
    fs::write(&own, "output_gain_db = -1.0\n").unwrap();
    let said = ok(dir.dps(&["devices", "assign", "--device", "macbook"]));
    assert!(
        said.starts_with(&format!(
            "kept {}, which was already there\n",
            own.display()
        )),
        "{said}"
    );
    assert_eq!(read(&own), "output_gain_db = -1.0\n");
    let names: Vec<_> = fs::read_dir(dir.dir().join("devices"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn assign_starts_from_a_preset_or_a_file_and_replaces_only_with_force() {
    let dir = TempDir::new("assign-from");
    let own = dir.dir().join("devices/airpods-pro.toml");
    let said = ok(dir.dps(&[
        "devices",
        "assign",
        "--device",
        "AirPods Pro",
        "--preset",
        "music",
    ]));
    assert!(said.starts_with(&format!("wrote {} from preset music\n", own.display())));
    let music = read(&own);
    assert!(music.contains("[eq]"), "{music}");

    let err = fails(
        dir.dps(&[
            "devices",
            "assign",
            "--device",
            "AA:output",
            "--preset",
            "voice",
        ]),
        "already exists; add --force to replace it",
    );
    assert!(err.contains(&own.display().to_string()), "{err}");
    assert_eq!(read(&own), music);
    ok(dir.dps(&[
        "devices",
        "assign",
        "--device",
        "AA:output",
        "--preset",
        "voice",
        "--force",
    ]));
    assert_ne!(read(&own), music);

    let mine = dir.work("mine.toml", "# for the AirPods\noutput_gain_db = -5.0\n");
    let mine_arg = mine.to_str().unwrap();
    ok(dir.dps(&[
        "devices",
        "assign",
        "--device",
        "AA:output",
        "--from",
        mine_arg,
        "--force",
    ]));
    assert_eq!(read(&own), read(&mine));

    // A file that is not a usable profile is refused, and nothing changes.
    let bad = dir.work("bad.toml", "output_gain_db = 900.0\n");
    let bad_arg = bad.to_str().unwrap();
    let config = read(&dir.config());
    fails(
        dir.dps(&[
            "devices",
            "assign",
            "--device",
            "AirPods Max",
            "--from",
            bad_arg,
        ]),
        "bad.toml: output_gain_db",
    );
    assert!(!dir.dir().join("devices/airpods-max.toml").exists());
    assert_eq!(read(&dir.config()), config);

    // --force means nothing without --preset or --from.
    let out = dir.dps(&["devices", "assign", "--force"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let out = dir.dps(&["devices", "assign", "--preset", "music", "--from", mine_arg]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn unassign_removes_the_mapping_and_keeps_the_file() {
    let dir = TempDir::new("unassign");
    ok(dir.dps(&[
        "devices",
        "assign",
        "--device",
        "AirPods Max",
        "--preset",
        "headphones",
    ]));
    let own = dir.dir().join("devices/airpods-max.toml");
    let said = ok(dir.dps(&["devices", "unassign", "--device", "max"]));
    assert_eq!(
        said,
        format!(
            "AirPods Max (BB:output) uses the settings file again; kept {}; rewrote {} (comments \
             in it are not kept); a running dps switches on its own\n",
            own.display(),
            dir.config().display()
        )
    );
    assert!(own.exists());
    assert_eq!(read(&dir.config()), "exclude = []\n\n[devices]\n");
    assert_eq!(
        ok(dir.dps(&["devices", "unassign", "--device", "BB:output"])),
        "AirPods Max has no profile of its own; it uses the settings file\n"
    );
}

#[test]
fn a_device_that_is_not_connected_can_still_be_unassigned() {
    let dir = TempDir::new("absent");
    fs::create_dir_all(dir.dir()).unwrap();
    fs::write(
        dir.config(),
        "[devices.GONE]\nname = \"Old DAC\"\nprofile = \"devices/old-dac.toml\"\n",
    )
    .unwrap();
    let list = ok(dir.dps(&["devices"]));
    assert!(
        list.contains(
            "  Old DAC (not connected)  GONE                  devices/old-dac.toml (missing, so \
             the settings file)\n"
        ),
        "{list}"
    );
    let said = ok(dir.dps(&["devices", "unassign", "--device", "old dac"]));
    assert!(
        said.starts_with("Old DAC (GONE) uses the settings file again;"),
        "{said}"
    );
}

#[test]
fn a_device_name_that_matches_none_or_several_is_refused() {
    let dir = TempDir::new("match");
    fails(
        dir.dps(&["devices", "assign", "--device", "airpods"]),
        "--device \"airpods\" matches 2 devices: AirPods Pro (AA:output), AirPods Max (BB:output); \
         give its UID or more of its name",
    );
    fails(
        dir.dps(&["devices", "unassign", "--device", "HDMI"]),
        "no output device matches --device \"HDMI\"; `dps devices` lists them",
    );
    assert!(!dir.dir().exists());
}

#[test]
fn a_broken_config_toml_is_named_and_left_alone() {
    let dir = TempDir::new("broken");
    fs::create_dir_all(dir.dir()).unwrap();
    fs::write(dir.config(), "exclude = [\"a\"]\ndevicez = 1\n").unwrap();
    let err = fails(dir.dps(&["devices"]), "devicez");
    assert!(
        err.contains(&format!("{}:2:", dir.config().display())),
        "{err}"
    );
    fails(dir.dps(&["devices", "assign"]), "devicez");
    let hd = dir.work(
        "hd.txt",
        "Preamp: -1 dB\nFilter 1: ON PK Fc 1000 Hz Gain 2 dB Q 1\n",
    );
    fails(
        dir.dps(&["import", hd.to_str().unwrap()]),
        "; correct it, or name the profile with --profile",
    );
    assert_eq!(read(&dir.config()), "exclude = [\"a\"]\ndevicez = 1\n");
    assert!(!dir.settings().exists());
}

#[test]
fn import_writes_into_the_profile_the_default_output_plays_with() {
    let dir = TempDir::new("import");
    let hd = dir.work(
        "hd.txt",
        "Preamp: -1 dB\nFilter 1: ON PK Fc 1000 Hz Gain 2 dB Q 1\n",
    );
    let hd = hd.to_str().unwrap();

    let said = ok(dir.dps(&["import", hd]));
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(
        lines[..2],
        [
            format!(
                "imported 1 band and a preamp of -1 dB into {}",
                dir.settings().display()
            )
            .as_str(),
            "no output device has a profile of its own, so every one uses the settings file"
        ]
    );

    // Another device's profile leaves the default output on the settings file.
    ok(dir.dps(&[
        "devices",
        "assign",
        "--device",
        "AA:output",
        "--preset",
        "music",
    ]));
    let said = ok(dir.dps(&["import", hd]));
    assert_eq!(
        said.lines().nth(1).unwrap(),
        "the default output device, MacBook Pro Speakers, has no profile of its own, so it uses \
         the settings file"
    );

    ok(dir.dps(&["devices", "assign", "--preset", "voice"]));
    let own = dir.dir().join("devices/macbook-pro-speakers.toml");
    let said = ok(dir.dps(&["import", hd]));
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(
        lines[..2],
        [
            format!(
                "imported 1 band and a preamp of -1 dB into {}",
                own.display()
            )
            .as_str(),
            "that is the profile of the default output device, MacBook Pro Speakers"
        ]
    );
    assert!(read(&own).contains("[correction]"), "{}", read(&own));

    // --profile still wins and says nothing more.
    let mine = dir.work("mine.toml", "");
    let said = ok(dir.dps(&["import", hd, "--profile", mine.to_str().unwrap()]));
    assert!(!said.contains("default output"), "{said}");
}

#[test]
fn doctor_names_the_profile_the_default_output_plays_with() {
    let dir = TempDir::new("doctor");
    // A held lock stands in for a running dps, so the doctor makes no tap.
    let lock = dir
        .root
        .join("home/Library/Application Support/dps/live.lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let file = fs::File::create(&lock).unwrap();
    file.lock().unwrap();
    std::io::Write::write_all(
        &mut &file,
        format!("{} foreground\n", std::process::id()).as_bytes(),
    )
    .unwrap();

    let config_line = |out: Output| {
        let text = stdout(&out);
        text.lines()
            .find(|l| l[9..].starts_with("config: "))
            .unwrap_or_else(|| panic!("no config line in {text}"))
            .to_owned()
    };
    assert_eq!(
        config_line(dir.dps(&["doctor", "--preset", "music"])),
        format!(
            "ok       config: {} does not exist, so every output device plays with the settings \
             file",
            dir.config().display()
        )
    );
    ok(dir.dps(&["devices", "assign", "--preset", "music"]));
    assert_eq!(
        config_line(dir.dps(&["doctor", "--preset", "music"])),
        format!(
            "ok       config: {} gives 1 output device a profile of its own; the default output, \
             MacBook Pro Speakers, plays with {}",
            dir.config().display(),
            dir.dir()
                .join("devices/macbook-pro-speakers.toml")
                .display()
        )
    );
    // A broken config is a problem, but not one live mode stops for.
    fs::write(dir.config(), "nonsense = 1\n").unwrap();
    let out = dir.dps(&["doctor", "--preset", "music"]);
    let line = config_line(out);
    assert!(line.starts_with("problem  config: "), "{line}");
    assert!(line.contains("nonsense"), "{line}");
}
