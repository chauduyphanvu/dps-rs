//! `tonewell devices`: the output devices, the profile live mode uses on each,
//! and giving one a profile of its own.
//!
//! Live mode with the settings file follows the output device: a device
//! that config.toml maps to a profile file plays with that file, every
//! other device with the settings file.

use std::fs;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use tonewell::Profile;

use crate::config::{Config, DeviceEntry, Dirs, Source, load_profile};
use crate::presets::{self, Preset};
use crate::{PendingFile, live, write_stdout};

/// A new device profile is checked at one rate; bands too high for
/// another are skipped there, not refused.
const CHECK_RATE: u32 = 48_000;

#[derive(Subcommand)]
pub enum Action {
    /// Give an output device a profile of its own.
    ///
    /// Creates devices/<name>.toml in the config directory, a copy of the
    /// profile the device uses now unless --preset or --from says otherwise,
    /// and maps the device to it in config.toml. A file that is already
    /// there is kept, unless --force replaces it with --preset or --from.
    /// A running tonewell switches to it on its own when that device plays.
    Assign {
        #[command(flatten)]
        device: DeviceArg,
        /// Start the profile from this built-in preset.
        #[arg(long, value_name = "NAME", value_parser = presets::parser(), group = "start")]
        preset: Option<&'static Preset>,
        /// Start the profile from this profile file.
        #[arg(long, value_name = "FILE.toml", group = "start")]
        from: Option<PathBuf>,
        /// Replace the device's profile file with --preset or --from.
        #[arg(long, requires = "start")]
        force: bool,
    },
    /// Make an output device use the settings file again; its own profile
    /// file is kept.
    Unassign {
        #[command(flatten)]
        device: DeviceArg,
    },
}

#[derive(clap::Args)]
pub struct DeviceArg {
    /// The device: its UID, its name, or a unique part of its name
    /// [default: the default output device]
    #[arg(long, value_name = "NAME_OR_UID")]
    device: Option<String>,
}

/// An output device as `tonewell devices` sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub uid: String,
    pub name: String,
    /// The default output device.
    pub default: bool,
    /// Connected now; otherwise only config.toml knows it.
    pub present: bool,
}

pub fn run(action: Option<Action>, dirs: &Dirs) -> Result<(), String> {
    let config_path = dirs.config_file()?;
    let config = Config::load(&config_path)?;
    let devices = known(connected()?, &config);
    match action {
        None => write_stdout(&listing(&devices, &config, &config_path)),
        Some(Action::Assign {
            device,
            preset,
            from,
            force,
        }) => {
            let device = choose(&devices, device.device.as_deref())?;
            let start = match (preset, from) {
                (Some(preset), _) => Start::Preset(preset),
                (None, Some(path)) => Start::File(path),
                (None, None) => Start::Current,
            };
            let said = assign(device, start, force, config, &config_path, dirs)?;
            write_stdout(&said)
        }
        Some(Action::Unassign { device }) => {
            let device = choose(&devices, device.device.as_deref())?;
            let said = unassign(device, config, &config_path)?;
            write_stdout(&said)
        }
    }
}

/// The output devices connected now.
fn connected() -> Result<Vec<Device>, String> {
    // Tests list the devices here, as `uid=name` entries separated by `;`,
    // the default one marked with a leading `*`; it is not meant for users.
    if let Some(list) = std::env::var_os("TONEWELL_TEST_DEVICES") {
        return Ok(list
            .to_string_lossy()
            .split(';')
            .filter_map(|entry| {
                let (default, entry) = match entry.strip_prefix('*') {
                    Some(rest) => (true, rest),
                    None => (false, entry),
                };
                let (uid, name) = entry.split_once('=')?;
                Some(Device {
                    uid: uid.into(),
                    name: name.into(),
                    default,
                    present: true,
                })
            })
            .collect());
    }
    Ok(live::output_devices()?
        .into_iter()
        .map(|d| Device {
            uid: d.uid,
            name: d.name,
            default: d.default,
            present: true,
        })
        .collect())
}

/// The default output device, if there is one.
pub fn default_device() -> Result<Option<Device>, String> {
    Ok(connected()?.into_iter().find(|d| d.default))
}

/// The connected devices, then those config.toml maps that are not
/// connected.
fn known(mut devices: Vec<Device>, config: &Config) -> Vec<Device> {
    let absent: Vec<Device> = config
        .devices
        .iter()
        .filter(|(uid, _)| !devices.iter().any(|d| &d.uid == *uid))
        .map(|(uid, entry)| Device {
            uid: uid.clone(),
            name: entry.name.clone(),
            default: false,
            present: false,
        })
        .collect();
    devices.extend(absent);
    devices
}

/// The profile file the default output device uses: its own, or `None`
/// for the settings file; and why, as a sentence for messages.
pub fn default_device_profile(dirs: &Dirs) -> Result<(Option<PathBuf>, String), String> {
    let config_path = dirs.config_file()?;
    let config = Config::load(&config_path)
        .map_err(|e| format!("{e}; correct it, or name the profile with --profile"))?;
    if config.devices.is_empty() {
        return Ok((
            None,
            "no output device has a profile of its own, so every one uses the settings file".into(),
        ));
    }
    let default = default_device().map_err(|e| {
        format!(
            "cannot tell which profile the default output device uses: {e}; name one with --profile"
        )
    })?;
    Ok(device_profile(&config, &config_path, default.as_ref()))
}

/// What `default_device_profile` says for `default`, the default device.
fn device_profile(
    config: &Config,
    config_path: &Path,
    default: Option<&Device>,
) -> (Option<PathBuf>, String) {
    let Some(device) = default else {
        return (
            None,
            "there is no default output device, so the settings file is used".into(),
        );
    };
    match config.profile_for(&device.uid, config_dir(config_path)) {
        Some(path) => (
            Some(path),
            format!(
                "that is the profile of the default output device, {}",
                device.name
            ),
        ),
        None => (
            None,
            format!(
                "the default output device, {}, has no profile of its own, so it uses the \
                 settings file",
                device.name
            ),
        ),
    }
}

fn config_dir(config_path: &Path) -> &Path {
    config_path.parent().unwrap_or(Path::new("."))
}

/// What `tonewell devices` prints: one line per device, the default marked,
/// with the profile it uses.
fn listing(devices: &[Device], config: &Config, config_path: &Path) -> String {
    let dir = config_dir(config_path);
    let rows: Vec<[String; 3]> = devices
        .iter()
        .map(|device| {
            let profile = match config.devices.get(&device.uid) {
                Some(entry) => {
                    let missing = !dir.join(&entry.profile).exists();
                    format!(
                        "{}{}",
                        entry.profile.display(),
                        if missing {
                            " (missing, so the settings file)"
                        } else {
                            ""
                        }
                    )
                }
                None => "settings file".into(),
            };
            let name = if device.present {
                device.name.clone()
            } else {
                format!("{} (not connected)", device.name)
            };
            [name, device.uid.clone(), profile]
        })
        .collect();
    let header = ["output device".to_owned(), "UID".into(), "profile".into()];
    let width = |i: usize| {
        rows.iter()
            .chain([&header])
            .map(|r| r[i].chars().count())
            .max()
            .unwrap_or(0)
    };
    let (name_width, uid_width) = (width(0), width(1));
    let line = |marker: char, row: &[String; 3]| {
        format!(
            "{marker} {:name_width$}  {:uid_width$}  {}\n",
            row[0], row[1], row[2]
        )
    };
    let mut out = line(' ', &header);
    for (device, row) in devices.iter().zip(&rows) {
        out += &line(if device.default { '*' } else { ' ' }, row);
    }
    if devices.is_empty() {
        out += "  (no output devices)\n";
    }
    out += &format!(
        "\n* default output. Profile paths are relative to {}; `tonewell devices assign` gives a \
         device a profile of its own.\n",
        dir.display()
    );
    out
}

/// The device `query` names: exact UID, else exact name, else the one
/// whose name contains it, ignoring case; without a query, the default
/// output device.
fn choose<'a>(devices: &'a [Device], query: Option<&str>) -> Result<&'a Device, String> {
    let Some(query) = query else {
        return devices.iter().find(|d| d.default).ok_or_else(|| {
            "there is no default output device; name one with --device (`tonewell devices` lists \
             them)"
                .into()
        });
    };
    if let Some(device) = devices.iter().find(|d| d.uid == query) {
        return Ok(device);
    }
    let lower = query.to_lowercase();
    let tiers: [&dyn Fn(&Device) -> bool; 2] = [&|d| d.name == query, &|d| {
        d.name.to_lowercase().contains(&lower)
    }];
    for tier in tiers {
        let matches: Vec<&Device> = devices.iter().filter(|d| tier(d)).collect();
        match matches[..] {
            [] => continue,
            [device] => return Ok(device),
            _ => {
                let names: Vec<String> = matches
                    .iter()
                    .map(|d| format!("{} ({})", d.name, d.uid))
                    .collect();
                return Err(format!(
                    "--device {query:?} matches {} devices: {}; give its UID or more of its name",
                    matches.len(),
                    names.join(", ")
                ));
            }
        }
    }
    Err(format!(
        "no output device matches --device {query:?}; `tonewell devices` lists them"
    ))
}

/// The file name a device's profile gets: its name lowercased, each run of
/// other characters than letters and digits a `-`; from the UID for a name
/// with neither.
fn slug(name: &str, uid: &str) -> String {
    let slugged = |text: &str| {
        let mut out = String::new();
        for c in text.chars().flat_map(char::to_lowercase) {
            if c.is_alphanumeric() {
                out.push(c);
            } else if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_end_matches('-').to_owned()
    };
    match slugged(name) {
        s if s.is_empty() => slugged(uid),
        s => s,
    }
}

/// What a new device profile starts as.
enum Start {
    Preset(&'static Preset),
    File(PathBuf),
    /// A copy of the profile the device uses now.
    Current,
}

/// `tonewell devices assign`; what to print.
fn assign(
    device: &Device,
    start: Start,
    force: bool,
    mut config: Config,
    config_path: &Path,
    dirs: &Dirs,
) -> Result<String, String> {
    let dir = config_dir(config_path);
    let mut slugged = slug(&device.name, &device.uid);
    if slugged.is_empty() {
        slugged = "device".into();
    }
    let relative = PathBuf::from("devices").join(format!("{slugged}.toml"));
    let file = dir.join(&relative);
    let mut out = String::new();
    if file.exists() && (matches!(start, Start::Current) || !force) {
        if !matches!(start, Start::Current) {
            return Err(format!(
                "{} already exists; add --force to replace it, or leave out --preset and --from \
                 to keep it",
                file.display()
            ));
        }
        out += &format!("kept {}, which was already there\n", file.display());
    } else {
        let (text, from) = match start {
            Start::Preset(preset) => (preset.toml.to_owned(), format!("preset {}", preset.name)),
            Start::File(path) => (checked(&path)?, path.display().to_string()),
            Start::Current => {
                let current = match config.profile_for(&device.uid, dir) {
                    Some(own) if own.exists() => own,
                    _ => {
                        let settings = dirs.settings_file()?;
                        Source::Settings(settings.clone()).load()?;
                        settings
                    }
                };
                (checked(&current)?, current.display().to_string())
            }
        };
        write_new(&file, &text)?;
        out += &format!("wrote {} from {from}\n", file.display());
    }
    let before = config.devices.insert(
        device.uid.clone(),
        DeviceEntry {
            name: device.name.clone(),
            profile: relative.clone(),
        },
    );
    if let Some(before) = before.filter(|b| b.profile != relative) {
        out += &format!(
            "{} used {} before; that file is kept\n",
            device.name,
            before.profile.display()
        );
    }
    config.save(config_path)?;
    out += &format!(
        "{} ({}) now uses it in live mode; rewrote {} (comments in it are not kept); a running \
         tonewell switches on its own\n",
        device.name,
        device.uid,
        config_path.display()
    );
    Ok(out)
}

/// The text of the profile file at `path`, once it has been read and
/// checked as a profile.
fn checked(path: &Path) -> Result<String, String> {
    let profile: Profile = load_profile(path)?;
    profile
        .validate(CHECK_RATE)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    fs::read_to_string(path).map_err(|e| format!("cannot read profile {}: {e}", path.display()))
}

/// Writes `text` to `path` whole or not at all, creating the directory.
fn write_new(path: &Path, text: &str) -> Result<(), String> {
    let cannot = |e: std::io::Error| format!("cannot write {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(cannot)?;
    }
    let (pending, mut file) = PendingFile::beside(path)?;
    std::io::Write::write_all(&mut file, text.as_bytes()).map_err(cannot)?;
    drop(file);
    pending.persist(path)
}

/// `tonewell devices unassign`; what to print.
fn unassign(device: &Device, mut config: Config, config_path: &Path) -> Result<String, String> {
    let Some(entry) = config.devices.remove(&device.uid) else {
        return Ok(format!(
            "{} has no profile of its own; it uses the settings file\n",
            device.name
        ));
    };
    config.save(config_path)?;
    Ok(format!(
        "{} ({}) uses the settings file again; kept {}; rewrote {} (comments in it are not \
         kept); a running tonewell switches on its own\n",
        device.name,
        device.uid,
        config_dir(config_path).join(&entry.profile).display(),
        config_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(uid: &str, name: &str, default: bool) -> Device {
        Device {
            uid: uid.into(),
            name: name.into(),
            default,
            present: true,
        }
    }

    fn rig() -> Vec<Device> {
        vec![
            device("BuiltInSpeakerDevice", "MacBook Pro Speakers", true),
            device("AA:output", "AirPods Pro", false),
            device("BB:output", "AirPods Max", false),
            device("USB-1", "Studio DAC", false),
            device("USB-2", "studio dac", false),
        ]
    }

    fn chosen(devices: &[Device], query: Option<&str>) -> Result<String, String> {
        choose(devices, query).map(|d| d.uid.clone())
    }

    #[test]
    fn a_device_is_chosen_by_uid_then_name_then_a_unique_part_of_it() {
        let devices = rig();
        assert_eq!(chosen(&devices, None).unwrap(), "BuiltInSpeakerDevice");
        assert_eq!(chosen(&devices, Some("AA:output")).unwrap(), "AA:output");
        assert_eq!(chosen(&devices, Some("AirPods Max")).unwrap(), "BB:output");
        // An exact name wins over a case-insensitive match of another.
        assert_eq!(chosen(&devices, Some("studio dac")).unwrap(), "USB-2");
        assert_eq!(
            chosen(&devices, Some("pro speak")).unwrap(),
            "BuiltInSpeakerDevice"
        );
        assert_eq!(chosen(&devices, Some("MAX")).unwrap(), "BB:output");

        let e = chosen(&devices, Some("airpods")).unwrap_err();
        assert_eq!(
            e,
            "--device \"airpods\" matches 2 devices: AirPods Pro (AA:output), AirPods Max \
             (BB:output); give its UID or more of its name"
        );
        let e = chosen(&devices, Some("Studio")).unwrap_err();
        assert!(e.contains("matches 2 devices"), "{e}");
        let e = chosen(&devices, Some("HDMI")).unwrap_err();
        assert_eq!(
            e,
            "no output device matches --device \"HDMI\"; `tonewell devices` lists them"
        );
        let e = chosen(&devices[1..], None).unwrap_err();
        assert!(e.starts_with("there is no default output device"), "{e}");
    }

    #[test]
    fn a_device_config_toml_knows_can_be_chosen_while_not_connected() {
        let mut config = Config::default();
        config.devices.insert(
            "GONE".into(),
            DeviceEntry {
                name: "Old DAC".into(),
                profile: "devices/old-dac.toml".into(),
            },
        );
        config.devices.insert(
            "AA:output".into(),
            DeviceEntry {
                name: "AirPods Pro".into(),
                profile: "devices/airpods-pro.toml".into(),
            },
        );
        let devices = known(rig(), &config);
        assert_eq!(devices.len(), 6);
        assert!(!devices[5].present && devices[5].name == "Old DAC");
        assert_eq!(chosen(&devices, Some("old")).unwrap(), "GONE");
        assert_eq!(chosen(&devices, Some("GONE")).unwrap(), "GONE");
        assert_eq!(chosen(&devices, None).unwrap(), "BuiltInSpeakerDevice");
    }

    #[test]
    fn slugs_are_lowercase_words_joined_by_dashes() {
        assert_eq!(slug("MacBook Pro Speakers", "x"), "macbook-pro-speakers");
        assert_eq!(slug("  Bob's AirPods (2) ", "x"), "bob-s-airpods-2");
        assert_eq!(slug("Écouteurs Äpfel", "x"), "écouteurs-äpfel");
        assert_eq!(
            slug("--", "AppleUSBAudioEngine:Foo:1"),
            "appleusbaudioengine-foo-1"
        );
        assert_eq!(slug("", ""), "");
    }

    #[test]
    fn the_listing_marks_the_default_and_names_each_profile() {
        let dir = crate::config::tests::TestDir::new("devices-listing");
        let config_path = dir.0.join("config.toml");
        fs::create_dir_all(dir.0.join("devices")).unwrap();
        fs::write(dir.0.join("devices/airpods-pro.toml"), "").unwrap();
        let mut config = Config::default();
        for (uid, name, file) in [
            ("AA:output", "AirPods Pro", "devices/airpods-pro.toml"),
            ("USB-1", "Studio DAC", "devices/studio-dac.toml"),
            ("GONE", "Old DAC", "devices/old-dac.toml"),
        ] {
            config.devices.insert(
                uid.into(),
                DeviceEntry {
                    name: name.into(),
                    profile: file.into(),
                },
            );
        }
        let devices = known(rig()[..4].to_vec(), &config);
        let text = listing(&devices, &config, &config_path);
        let want = format!(
            "  output device            UID                   profile
* MacBook Pro Speakers     BuiltInSpeakerDevice  settings file
  AirPods Pro              AA:output             devices/airpods-pro.toml
  AirPods Max              BB:output             settings file
  Studio DAC               USB-1                 devices/studio-dac.toml (missing, so the settings file)
  Old DAC (not connected)  GONE                  devices/old-dac.toml (missing, so the settings file)

* default output. Profile paths are relative to {}; `tonewell devices assign` gives a device a profile of its own.
",
            dir.0.display()
        );
        assert_eq!(text, want);
        assert!(listing(&[], &Config::default(), &config_path).contains("(no output devices)"));
    }

    #[test]
    fn the_default_device_profile_says_why() {
        let mut config = Config::default();
        config.devices.insert(
            "AA:output".into(),
            DeviceEntry {
                name: "AirPods Pro".into(),
                profile: "devices/airpods-pro.toml".into(),
            },
        );
        let path = Path::new("/c/tonewell/config.toml");
        let devices = rig();
        assert_eq!(
            device_profile(&config, path, Some(&devices[1])),
            (
                Some(PathBuf::from("/c/tonewell/devices/airpods-pro.toml")),
                "that is the profile of the default output device, AirPods Pro".into()
            )
        );
        assert_eq!(
            device_profile(&config, path, Some(&devices[0])),
            (
                None,
                "the default output device, MacBook Pro Speakers, has no profile of its own, so \
                 it uses the settings file"
                    .into()
            )
        );
        assert_eq!(device_profile(&config, path, None).0, None);
    }
}
