//! The keyboard tuner's model: the settings it lists, what each key does to
//! the profile, and whether the profile differs from what was last saved or
//! loaded. Nothing here touches the terminal; `save` is the only I/O.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use dps::Profile;
use dps::profile::EQ_BANDS;

/// Which compressor a dynamics setting belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stage {
    One,
    Two,
    Stereo,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dynamics {
    Threshold,
    Ratio,
    Attack,
    Release,
}

/// One setting the tuner lists.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Setting {
    InputGain,
    OutputGain,
    ShelfCorner,
    LowShelf,
    HighShelf,
    Dynamics(Stage, Dynamics),
    /// The gain of one EQ band.
    Band(usize),
    StereoEnabled,
    Crossover,
    SideBoost,
    LimiterEnabled,
    Ceiling,
    LimiterRelease,
}

enum Unit {
    Db,
    Hz,
    Ratio,
    Ms,
}

impl Setting {
    /// Every setting, in the order listed.
    fn all() -> Vec<Self> {
        use Dynamics::*;
        let dynamics =
            |stage| [Threshold, Ratio, Attack, Release].map(|d| Self::Dynamics(stage, d));
        [
            Self::InputGain,
            Self::OutputGain,
            Self::ShelfCorner,
            Self::LowShelf,
            Self::HighShelf,
        ]
        .into_iter()
        .chain(dynamics(Stage::One))
        .chain(dynamics(Stage::Two))
        .chain((0..EQ_BANDS).map(Self::Band))
        .chain([Self::StereoEnabled, Self::Crossover, Self::SideBoost])
        .chain(dynamics(Stage::Stereo))
        .chain([Self::LimiterEnabled, Self::Ceiling, Self::LimiterRelease])
        .collect()
    }

    fn group(self) -> &'static str {
        match self {
            Self::InputGain | Self::OutputGain => "Gains",
            Self::ShelfCorner | Self::LowShelf | Self::HighShelf => "Shelves",
            Self::Dynamics(Stage::One, _) => "Compressor 1",
            Self::Dynamics(Stage::Two, _) => "Compressor 2",
            Self::Band(_) => "EQ band gains",
            Self::StereoEnabled
            | Self::Crossover
            | Self::SideBoost
            | Self::Dynamics(Stage::Stereo, _) => "Stereo",
            Self::LimiterEnabled | Self::Ceiling | Self::LimiterRelease => "Limiter",
        }
    }

    fn label(self, profile: &Profile) -> String {
        let text = match self {
            Self::InputGain => "input gain",
            Self::OutputGain => "output gain",
            Self::ShelfCorner => "corner",
            Self::LowShelf => "low shelf",
            Self::HighShelf => "high shelf",
            Self::Dynamics(_, Dynamics::Threshold) => "threshold",
            Self::Dynamics(_, Dynamics::Ratio) => "ratio",
            Self::Dynamics(_, Dynamics::Attack) => "attack",
            Self::Dynamics(_, Dynamics::Release) | Self::LimiterRelease => "release",
            Self::Band(i) => {
                let (freq, q) = (profile.eq.freqs_hz[i], profile.eq.q[i]);
                return format!("{:>7} Hz  Q {q}", freq.to_string());
            }
            Self::StereoEnabled | Self::LimiterEnabled => "enabled",
            Self::Crossover => "crossover",
            Self::SideBoost => "side boost",
            Self::Ceiling => "ceiling",
        };
        text.into()
    }

    /// The value this setting changes; `None` for an on/off switch.
    fn number(self, p: &mut Profile) -> Option<&mut f32> {
        Some(match self {
            Self::InputGain => &mut p.input_gain_db,
            Self::OutputGain => &mut p.output_gain_db,
            Self::ShelfCorner => &mut p.shelf_corner_hz,
            Self::LowShelf => &mut p.low_shelf_db,
            Self::HighShelf => &mut p.high_shelf_db,
            Self::Dynamics(stage, which) => {
                let (threshold, ratio, attack, release) = match stage {
                    Stage::One | Stage::Two => {
                        let c = match stage {
                            Stage::One => &mut p.compressor1,
                            _ => &mut p.compressor2,
                        };
                        (
                            &mut c.threshold_db,
                            &mut c.ratio,
                            &mut c.attack_ms,
                            &mut c.release_ms,
                        )
                    }
                    Stage::Stereo => {
                        let s = &mut p.stereo;
                        (
                            &mut s.threshold_db,
                            &mut s.ratio,
                            &mut s.attack_ms,
                            &mut s.release_ms,
                        )
                    }
                };
                match which {
                    Dynamics::Threshold => threshold,
                    Dynamics::Ratio => ratio,
                    Dynamics::Attack => attack,
                    Dynamics::Release => release,
                }
            }
            Self::Band(i) => &mut p.eq.gains_db[i],
            Self::StereoEnabled | Self::LimiterEnabled => return None,
            Self::Crossover => &mut p.stereo.crossover_hz,
            Self::SideBoost => &mut p.stereo.side_boost_db,
            Self::Ceiling => &mut p.limiter.ceiling_db,
            Self::LimiterRelease => &mut p.limiter.release_ms,
        })
    }

    /// The switch this setting turns on and off, if it is one.
    fn switch(self, p: &mut Profile) -> Option<&mut bool> {
        match self {
            Self::StereoEnabled => Some(&mut p.stereo.enabled),
            Self::LimiterEnabled => Some(&mut p.limiter.enabled),
            _ => None,
        }
    }

    fn unit(self) -> Unit {
        match self {
            Self::ShelfCorner | Self::Crossover => Unit::Hz,
            Self::Dynamics(_, Dynamics::Ratio) => Unit::Ratio,
            Self::Dynamics(_, Dynamics::Attack | Dynamics::Release) | Self::LimiterRelease => {
                Unit::Ms
            }
            _ => Unit::Db,
        }
    }

    /// The coarse and the fine step.
    fn steps(self) -> (f64, f64) {
        match self {
            Self::ShelfCorner => (50.0, 10.0),
            Self::Crossover => (10.0, 1.0),
            Self::Dynamics(_, Dynamics::Threshold | Dynamics::Attack) => (1.0, 0.1),
            Self::Dynamics(_, Dynamics::Release) | Self::LimiterRelease => (10.0, 1.0),
            _ => (0.5, 0.1),
        }
    }

    fn format(self, profile: &mut Profile) -> String {
        let Some(&mut v) = self.number(profile) else {
            let on = *self
                .switch(profile)
                .expect("a setting is a number or a switch");
            return if on { "on" } else { "off" }.into();
        };
        match self.unit() {
            Unit::Db => format!("{} dB", decimal(v, true)),
            Unit::Hz => format!("{v} Hz"),
            Unit::Ratio => format!("{}:1", decimal(v, false)),
            Unit::Ms => format!("{} ms", decimal(v, false)),
        }
    }
}

/// `v` with one decimal, or with as many as it needs to be exact.
fn decimal(v: f32, signed: bool) -> String {
    let v = v + 0.0; // never "-0.0"
    let short = if signed {
        format!("{v:+.1}")
    } else {
        format!("{v:.1}")
    };
    if short.parse::<f32>() == Ok(v) {
        short
    } else if signed {
        format!("{v:+}")
    } else {
        format!("{v}")
    }
}

/// The next multiple of `step` above or below `value`, so values stay on
/// the grid of the step and print without rounding noise.
fn snap(value: f32, step: f64, up: bool) -> f32 {
    let k = f64::from(value) / step;
    // A value is on the grid when it is the f32 nearest a multiple, as 0.3
    // or 8000.3 are, though neither is exact.
    let nearest = k.round();
    let k = if (nearest * step) as f32 == value {
        nearest
    } else if up {
        k.floor()
    } else {
        k.ceil()
    };
    let next = if up { k + 1.0 } else { k - 1.0 };
    (next * step) as f32
}

/// A key the tuner knows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    ShiftLeft,
    ShiftRight,
    /// A printable ASCII character.
    Char(char),
    Esc,
    CtrlC,
}

/// What a key asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Up,
    Down,
    Step {
        up: bool,
        fine: bool,
    },
    Toggle,
    Bypass,
    Save,
    Revert,
    Quit,
    /// Answers to "save changes?".
    Yes,
    No,
    Cancel,
}

/// What the caller has to do after an action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Effect {
    None,
    /// Play the changed profile.
    Apply,
    Bypass(bool),
    Save,
    SaveAndQuit,
    Quit,
}

/// One line of the settings list.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Group(&'static str),
    Setting {
        label: String,
        value: String,
        selected: bool,
    },
}

pub struct Tuner {
    profile: Profile,
    /// What was last saved or loaded.
    saved: Profile,
    settings: Vec<Setting>,
    selected: usize,
    bypass: bool,
    /// Asking whether to save before quitting; the question is drawn from
    /// this, not kept in `message`.
    quitting: bool,
    message: String,
    /// The message is a refusal, cleared by the next change that goes through.
    refused: bool,
    /// Where `s` saves, as shown to the user.
    save_to: String,
}

impl Tuner {
    pub fn new(profile: Profile, save_to: String) -> Self {
        Self {
            saved: profile.clone(),
            profile,
            settings: Setting::all(),
            selected: 0,
            bypass: false,
            quitting: false,
            message: String::new(),
            refused: false,
            save_to,
        }
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn dirty(&self) -> bool {
        self.profile != self.saved
    }

    pub fn bypass(&self) -> bool {
        self.bypass
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// The question awaiting an answer, if any.
    pub fn prompt(&self) -> Option<String> {
        self.quitting
            .then(|| format!("save changes to {}? y/n (Esc cancels)", self.save_to))
    }

    pub fn set_message(&mut self, message: String) {
        self.message = message;
        self.refused = false;
    }

    /// Records that the profile was saved.
    pub fn saved(&mut self) {
        self.saved = self.profile.clone();
        self.set_message(format!(
            "saved: rewrote {} (comments are not kept)",
            self.save_to
        ));
    }

    /// Replaces the profile with one loaded from disk; true if that threw
    /// away unsaved changes. With nothing left to save, a question about
    /// saving them is dropped.
    pub fn reload(&mut self, profile: Profile) -> bool {
        let edited = self.dirty();
        self.saved = profile.clone();
        self.profile = profile;
        self.quitting = false;
        edited
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut scratch = self.profile.clone();
        let mut rows = Vec::new();
        let mut group = "";
        for (i, setting) in self.settings.iter().enumerate() {
            if setting.group() != group {
                group = setting.group();
                rows.push(Row::Group(group));
            }
            rows.push(Row::Setting {
                label: setting.label(&self.profile),
                value: setting.format(&mut scratch),
                selected: i == self.selected,
            });
        }
        rows
    }

    /// The selected setting's position and the number of settings.
    pub fn position(&self) -> (usize, usize) {
        (self.selected, self.settings.len())
    }

    /// Handles a key; `rate` is what changes are validated at.
    pub fn key(&mut self, key: Key, rate: u32) -> Effect {
        match self.action(key) {
            Some(action) => self.apply(action, rate),
            None => Effect::None,
        }
    }

    pub fn action(&self, key: Key) -> Option<Action> {
        if self.quitting {
            return match key {
                // Ctrl-C twice quits, as it would without the question.
                Key::CtrlC | Key::Char('n' | 'N') => Some(Action::No),
                Key::Char('y' | 'Y') => Some(Action::Yes),
                Key::Esc => Some(Action::Cancel),
                _ => None,
            };
        }
        let step = |up, fine| Action::Step { up, fine };
        Some(match key {
            Key::CtrlC | Key::Char('q') => Action::Quit,
            Key::Up | Key::Char('k') => Action::Up,
            Key::Down | Key::Char('j') => Action::Down,
            Key::Left | Key::Char('h') => step(false, false),
            Key::Right | Key::Char('l') => step(true, false),
            Key::ShiftLeft | Key::Char('H') => step(false, true),
            Key::ShiftRight | Key::Char('L') => step(true, true),
            Key::Char(' ') => Action::Toggle,
            Key::Char('b') => Action::Bypass,
            Key::Char('s') => Action::Save,
            Key::Char('r') => Action::Revert,
            _ => return None,
        })
    }

    pub fn apply(&mut self, action: Action, rate: u32) -> Effect {
        let setting = self.settings[self.selected];
        match action {
            Action::Up => {
                self.selected = self.selected.saturating_sub(1);
                Effect::None
            }
            Action::Down => {
                self.selected = (self.selected + 1).min(self.settings.len() - 1);
                Effect::None
            }
            Action::Step { up, fine } => {
                let mut candidate = self.profile.clone();
                if let Some(on) = setting.switch(&mut candidate) {
                    *on = !*on;
                } else if let Some(value) = setting.number(&mut candidate) {
                    let (coarse, fine_step) = setting.steps();
                    *value = snap(*value, if fine { fine_step } else { coarse }, up);
                }
                self.offer(candidate, rate)
            }
            Action::Toggle => {
                let mut candidate = self.profile.clone();
                match setting.switch(&mut candidate) {
                    Some(on) => {
                        *on = !*on;
                        self.offer(candidate, rate)
                    }
                    None => Effect::None,
                }
            }
            Action::Bypass => {
                self.bypass = !self.bypass;
                Effect::Bypass(self.bypass)
            }
            Action::Save => Effect::Save,
            Action::Revert if !self.dirty() => {
                self.set_message("no unsaved changes to revert".into());
                Effect::None
            }
            Action::Revert => {
                let effect = self.offer(self.saved.clone(), rate);
                if effect == Effect::Apply {
                    self.set_message("reverted to the last saved settings".into());
                }
                effect
            }
            Action::Quit if self.dirty() => {
                self.quitting = true;
                self.set_message(String::new());
                Effect::None
            }
            Action::Quit | Action::No => Effect::Quit,
            Action::Yes => {
                self.quitting = false;
                Effect::SaveAndQuit
            }
            Action::Cancel => {
                self.quitting = false;
                self.set_message("quit cancelled".into());
                Effect::None
            }
        }
    }

    /// Takes `candidate` if it is valid at `rate`, or says why not.
    fn offer(&mut self, candidate: Profile, rate: u32) -> Effect {
        match candidate.validate(rate) {
            Ok(()) => {
                self.profile = candidate;
                if self.refused {
                    self.set_message(String::new());
                }
                Effect::Apply
            }
            Err(e) => {
                self.message = format!("refused: {e}");
                self.refused = true;
                Effect::None
            }
        }
    }
}

/// Writes `profile` to `path` as TOML, replacing the whole file.
pub fn save(path: &Path, profile: &Profile) -> Result<(), String> {
    let text = toml::to_string(profile).expect("a profile always serializes");
    replace(path, |file| file.write_all(text.as_bytes()))
}

/// Replaces the file `path` names with what `write` writes, through a
/// temporary file beside it, so the file is never seen half written. A
/// symlink stays and the file it points to is replaced; that file keeps its
/// permissions. The directory is created if needed.
fn replace(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> Result<(), String> {
    let fail = |e: io::Error| format!("cannot save to {}: {e}", path.display());
    let target = follow_links(path).map_err(fail)?;
    let name = target
        .file_name()
        .ok_or_else(|| format!("cannot save to {}: not a file path", path.display()))?;
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    let permissions = match fs::metadata(&target) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(fail(e)),
    };
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = target.with_file_name(temp_name);
    let written = (|| {
        let mut file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            // A new file gets the usual 0666 less the umask.
            .mode(permissions.as_ref().map_or(0o666, |p| p.mode() & 0o777))
            .open(&temp)?;
        write(&mut file)?;
        if let Some(permissions) = permissions {
            // The umask may have taken bits off the mode given at creation.
            file.set_permissions(permissions)?;
        }
        fs::rename(&temp, &target)
    })();
    written.map_err(|e| {
        let _ = fs::remove_file(&temp);
        fail(e)
    })
}

/// `path` with each symlink it ends in followed, even to a file that does
/// not exist yet.
fn follow_links(path: &Path) -> io::Result<PathBuf> {
    let mut path = path.to_path_buf();
    // As many as the kernel follows (MAXSYMLINKS).
    for _ in 0..32 {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                let link = fs::read_link(&path)?;
                path = match path.parent() {
                    Some(dir) => dir.join(link),
                    None => link,
                };
            }
            _ => return Ok(path),
        }
    }
    Err(io::Error::other("too many levels of symbolic links"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn tuner() -> Tuner {
        Tuner::new(Profile::default(), "test.toml".into())
    }

    fn press(t: &mut Tuner, key: Key) -> Effect {
        t.key(key, RATE)
    }

    /// Moves down to `setting`, which must be at or below the selection.
    fn select(t: &mut Tuner, setting: Setting) {
        let target = t.settings.iter().position(|&s| s == setting).unwrap();
        while t.selected < target {
            assert_eq!(press(t, Key::Down), Effect::None);
        }
        assert_eq!(t.settings[t.selected], setting);
    }

    fn selected(t: &Tuner) -> (String, String) {
        t.rows()
            .into_iter()
            .find_map(|row| match row {
                Row::Setting {
                    label,
                    value,
                    selected: true,
                } => Some((label, value)),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn every_setting_is_listed_once_under_its_group() {
        let rows = tuner().rows();
        let groups: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Group(g) => Some(*g),
                Row::Setting { .. } => None,
            })
            .collect();
        assert_eq!(
            groups,
            [
                "Gains",
                "Shelves",
                "Compressor 1",
                "Compressor 2",
                "EQ band gains",
                "Stereo",
                "Limiter"
            ]
        );
        // 2 gains, 3 shelf values, 4 + 4 compressor values, 11 bands,
        // 7 stereo values and 3 limiter values.
        assert_eq!(rows.len() - groups.len(), 34);
        let labels: Vec<String> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Setting { label, value, .. } => Some(format!("{label}: {value}")),
                Row::Group(_) => None,
            })
            .collect();
        assert_eq!(labels[0], "input gain: +0.0 dB");
        assert_eq!(labels[2], "corner: 1000 Hz");
        assert_eq!(labels[3], "low shelf: -10.0 dB");
        assert_eq!(labels[6], "ratio: 4.0:1");
        assert_eq!(labels[8], "release: 100.0 ms");
        assert_eq!(labels[13], "     30 Hz  Q 1.5: +0.0 dB");
        assert_eq!(labels[14], "   56.4 Hz  Q 1.5: +0.0 dB");
        assert_eq!(labels[23], "  16000 Hz  Q 1.5: +0.0 dB");
        assert_eq!(labels[24], "enabled: off");
        assert_eq!(labels[30], "release: 200.0 ms");
        assert_eq!(labels[31], "enabled: on");
        assert_eq!(labels[32], "ceiling: -1.0 dB");
        assert_eq!(labels[33], "release: 100.0 ms");
    }

    #[test]
    fn selection_moves_and_stops_at_both_ends() {
        let mut t = tuner();
        assert_eq!(press(&mut t, Key::Up), Effect::None);
        assert_eq!(t.position(), (0, 34));
        press(&mut t, Key::Char('j'));
        press(&mut t, Key::Down);
        assert_eq!(selected(&t).0, "corner");
        press(&mut t, Key::Char('k'));
        assert_eq!(selected(&t).0, "output gain");
        for _ in 0..40 {
            press(&mut t, Key::Down);
        }
        assert_eq!(t.position(), (33, 34));
        assert_eq!(t.settings[33], Setting::LimiterRelease);
    }

    #[test]
    fn each_setting_steps_by_its_own_amount() {
        use Dynamics::*;
        // (setting, after one coarse step up, after one fine step down from there)
        let cases = [
            (Setting::InputGain, "+0.5 dB", "+0.4 dB"),
            (Setting::OutputGain, "+7.5 dB", "+7.4 dB"),
            (Setting::ShelfCorner, "1050 Hz", "1040 Hz"),
            (Setting::HighShelf, "+8.5 dB", "+8.4 dB"),
            (
                Setting::Dynamics(Stage::One, Threshold),
                "-19.0 dB",
                "-19.1 dB",
            ),
            (Setting::Dynamics(Stage::One, Ratio), "4.5:1", "4.4:1"),
            (Setting::Dynamics(Stage::Two, Attack), "11.0 ms", "10.9 ms"),
            (
                Setting::Dynamics(Stage::Two, Release),
                "110.0 ms",
                "109.0 ms",
            ),
            (Setting::Band(4), "+0.5 dB", "+0.4 dB"),
            (Setting::Crossover, "210 Hz", "209 Hz"),
            (Setting::SideBoost, "+8.5 dB", "+8.4 dB"),
            (
                Setting::Dynamics(Stage::Stereo, Release),
                "210.0 ms",
                "209.0 ms",
            ),
            (Setting::Ceiling, "-0.5 dB", "-0.6 dB"),
            (Setting::LimiterRelease, "110.0 ms", "109.0 ms"),
        ];
        for (setting, up, fine_down) in cases {
            let mut t = tuner();
            select(&mut t, setting);
            assert_eq!(press(&mut t, Key::Right), Effect::Apply, "{setting:?}");
            assert_eq!(selected(&t).1, up, "{setting:?}");
            assert_eq!(press(&mut t, Key::ShiftLeft), Effect::Apply);
            assert_eq!(selected(&t).1, fine_down, "{setting:?}");
        }
    }

    #[test]
    fn steps_land_on_the_grid_and_store_exact_values() {
        let mut t = tuner();
        for code in [Key::Char('l'), Key::Char('L'), Key::Char('L')] {
            press(&mut t, code);
        }
        // 0 + 0.5 + 0.1 + 0.1, with no float drift.
        assert_eq!(t.profile().input_gain_db, 0.7);
        for _ in 0..3 {
            press(&mut t, Key::Char('h'));
        }
        // 0.7 snaps down to 0.5, then 0.0, then -0.5.
        assert_eq!(t.profile().input_gain_db, -0.5);
        // A value off the grid moves to the next grid line.
        let p = Profile {
            input_gain_db: 1.23,
            ..Profile::default()
        };
        let mut t = Tuner::new(p, String::new());
        press(&mut t, Key::Right);
        assert_eq!(t.profile().input_gain_db, 1.5);
        press(&mut t, Key::Char('H'));
        assert_eq!(t.profile().input_gain_db, 1.4);
    }

    #[test]
    fn the_stereo_switch_toggles_with_space_or_left_and_right() {
        let mut t = tuner();
        select(&mut t, Setting::StereoEnabled);
        assert_eq!(press(&mut t, Key::Char(' ')), Effect::Apply);
        assert!(t.profile().stereo.enabled);
        assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        assert!(!t.profile().stereo.enabled);
        assert_eq!(press(&mut t, Key::Char('l')), Effect::Apply);
        assert!(t.profile().stereo.enabled);
        // Space does nothing on a number.
        let mut t = tuner();
        assert_eq!(press(&mut t, Key::Char(' ')), Effect::None);
        assert!(!t.dirty());
    }

    #[test]
    fn the_limiter_switch_toggles_like_the_stereo_one_and_leaves_its_values() {
        let mut t = tuner();
        select(&mut t, Setting::LimiterEnabled);
        assert_eq!(selected(&t), ("enabled".into(), "on".into()));
        assert_eq!(press(&mut t, Key::Char(' ')), Effect::Apply);
        assert!(!t.profile().limiter.enabled);
        assert_eq!(selected(&t).1, "off");
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);
        assert!(t.profile().limiter.enabled);
        assert_eq!(press(&mut t, Key::Char('H')), Effect::Apply);
        assert!(!t.profile().limiter.enabled);
        // Only the limiter's switch moved.
        let want = Profile {
            limiter: dps::profile::LimiterParams {
                enabled: false,
                ..Profile::default().limiter
            },
            ..Profile::default()
        };
        assert_eq!(t.profile(), &want);
    }

    #[test]
    fn invalid_changes_are_refused_with_the_reason() {
        let mut t = tuner();
        select(&mut t, Setting::LowShelf);
        // The low shelf is a cut: 0 dB is its top.
        for _ in 0..20 {
            press(&mut t, Key::Right);
        }
        assert_eq!(t.profile().low_shelf_db, 0.0);
        assert_eq!(press(&mut t, Key::Right), Effect::None);
        assert_eq!(t.profile().low_shelf_db, 0.0);
        assert_eq!(
            t.message(),
            "refused: low_shelf_db = 0.5 is outside -24..=0"
        );
        // The next change that goes through clears the refusal.
        assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        assert_eq!(t.message(), "");

        let mut t = tuner();
        select(&mut t, Setting::Dynamics(Stage::One, Dynamics::Attack));
        for _ in 0..9 {
            assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        }
        assert_eq!(t.profile().compressor1.attack_ms, 1.0);
        assert_eq!(press(&mut t, Key::Left), Effect::None);
        assert!(
            t.message()
                .starts_with("refused: compressor1.attack_ms = 0 is outside"),
            "{}",
            t.message()
        );

        // The ceiling stays at or below full scale, and the release at or
        // above 1 ms.
        let mut t = tuner();
        select(&mut t, Setting::Ceiling);
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);
        assert_eq!(t.profile().limiter.ceiling_db, 0.0);
        assert_eq!(press(&mut t, Key::ShiftRight), Effect::None);
        assert_eq!(
            t.message(),
            "refused: limiter.ceiling_db = 0.1 is outside -24..=0"
        );
        select(&mut t, Setting::LimiterRelease);
        for _ in 0..9 {
            assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        }
        assert_eq!(t.profile().limiter.release_ms, 10.0);
        assert_eq!(press(&mut t, Key::Left), Effect::None);
        assert_eq!(
            t.message(),
            "refused: limiter.release_ms = 0 is outside 1..=2000"
        );
    }

    #[test]
    fn changes_are_validated_at_the_running_rate() {
        // At 8 kHz the shelf corner must stay under 8000 / 4 - 1 = 1999 Hz;
        // the EQ bands are moved under 4 kHz so only the corner can fail.
        let mut p = Profile {
            shelf_corner_hz: 1950.0,
            ..Profile::default()
        };
        p.eq.freqs_hz = [
            30.0, 50.0, 80.0, 120.0, 200.0, 300.0, 500.0, 800.0, 1000.0, 1500.0, 2000.0,
        ];
        let mut t = Tuner::new(p, String::new());
        select(&mut t, Setting::ShelfCorner);
        assert_eq!(t.key(Key::Right, 8000), Effect::None);
        assert!(
            t.message().contains("shelf_corner_hz = 2000"),
            "{}",
            t.message()
        );
        assert_eq!(t.key(Key::Right, 48_000), Effect::Apply);
    }

    #[test]
    fn dirty_means_different_from_the_last_save_or_load() {
        let mut t = tuner();
        assert!(!t.dirty());
        press(&mut t, Key::Right);
        assert!(t.dirty());
        press(&mut t, Key::Left);
        assert!(!t.dirty(), "back where it started");
        press(&mut t, Key::Right);
        t.saved();
        assert!(!t.dirty());
        assert_eq!(
            t.message(),
            "saved: rewrote test.toml (comments are not kept)"
        );
        press(&mut t, Key::Right);
        assert!(t.dirty());
        let loaded = Profile {
            output_gain_db: -3.0,
            ..Profile::default()
        };
        assert!(t.reload(loaded.clone()), "unsaved changes were replaced");
        assert_eq!(t.profile(), &loaded);
        assert!(!t.dirty());
        assert!(!t.reload(Profile::default()));
    }

    #[test]
    fn revert_goes_back_to_the_last_saved_settings() {
        let mut t = tuner();
        assert_eq!(press(&mut t, Key::Char('r')), Effect::None);
        assert_eq!(t.message(), "no unsaved changes to revert");
        press(&mut t, Key::Right); // input gain 0.5
        t.saved();
        press(&mut t, Key::Right); // 1.0
        press(&mut t, Key::Down);
        press(&mut t, Key::Left); // output gain 6.5
        assert_eq!(press(&mut t, Key::Char('r')), Effect::Apply);
        assert_eq!(t.profile().input_gain_db, 0.5);
        assert_eq!(t.profile().output_gain_db, 7.0);
        assert!(!t.dirty());
        assert_eq!(t.message(), "reverted to the last saved settings");
    }

    #[test]
    fn bypass_save_and_quit_keys() {
        let mut t = tuner();
        assert_eq!(press(&mut t, Key::Char('b')), Effect::Bypass(true));
        assert!(t.bypass());
        assert_eq!(press(&mut t, Key::Char('b')), Effect::Bypass(false));
        assert_eq!(press(&mut t, Key::Char('s')), Effect::Save);
        // Nothing to save: quit at once, with q or Ctrl-C.
        assert_eq!(press(&mut t, Key::Char('q')), Effect::Quit);
        assert_eq!(press(&mut t, Key::CtrlC), Effect::Quit);
        // Keys without a meaning do nothing.
        for key in [Key::Char('x'), Key::Char('Q'), Key::Esc, Key::Char('y')] {
            assert_eq!(press(&mut t, key), Effect::None, "{key:?}");
        }
        assert!(!t.dirty());
    }

    #[test]
    fn quitting_with_unsaved_changes_asks_first() {
        let mut t = tuner();
        press(&mut t, Key::Right);
        assert_eq!(press(&mut t, Key::Char('q')), Effect::None);
        assert_eq!(
            t.prompt().as_deref(),
            Some("save changes to test.toml? y/n (Esc cancels)")
        );
        // While asking, other keys do nothing.
        assert_eq!(press(&mut t, Key::Right), Effect::None);
        assert_eq!(t.profile().input_gain_db, 0.5);
        assert_eq!(press(&mut t, Key::Esc), Effect::None);
        assert_eq!(t.prompt(), None);
        assert_eq!(t.message(), "quit cancelled");
        // Keys work again after Esc.
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);

        press(&mut t, Key::Char('q'));
        assert_eq!(press(&mut t, Key::Char('y')), Effect::SaveAndQuit);
        // A failed save leaves the tuner running and asking nothing.
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);

        press(&mut t, Key::Char('q'));
        assert_eq!(press(&mut t, Key::Char('n')), Effect::Quit);

        let mut t = tuner();
        press(&mut t, Key::Right);
        assert_eq!(press(&mut t, Key::CtrlC), Effect::None);
        assert_eq!(press(&mut t, Key::CtrlC), Effect::Quit);
    }

    #[test]
    fn a_message_while_asking_leaves_the_question_up() {
        let mut t = tuner();
        press(&mut t, Key::Right);
        press(&mut t, Key::Char('q'));
        t.set_message("now on External Headphones at 44100 Hz".into());
        assert_eq!(t.message(), "now on External Headphones at 44100 Hz");
        assert!(t.prompt().is_some());
        // Still asking: keys other than the answers do nothing.
        assert_eq!(press(&mut t, Key::Right), Effect::None);
        assert_eq!(t.profile().input_gain_db, 0.5);
        assert_eq!(press(&mut t, Key::Char('n')), Effect::Quit);
    }

    #[test]
    fn a_reload_while_asking_drops_the_question() {
        // The reload replaced the unsaved changes, so there is nothing left
        // to save: y no longer saves, and q quits at once.
        let mut t = tuner();
        press(&mut t, Key::Right);
        press(&mut t, Key::Char('q'));
        assert!(t.reload(Profile::default()));
        assert_eq!(t.prompt(), None);
        assert_eq!(press(&mut t, Key::Char('y')), Effect::None);
        assert_eq!(press(&mut t, Key::Char('q')), Effect::Quit);
    }

    #[test]
    fn steps_never_stand_still_and_land_on_the_next_grid_line() {
        // Every fine-grid value across each setting's range, as the tuner
        // stores it, stepped both ways by the fine and the coarse step. The
        // values are j fine steps, so the grid lines are exact integers.
        // (coarse, fine, fine steps per coarse, lowest j, highest j)
        let grids: [(f64, f64, i32, i32, i32); 4] = [
            (0.5, 0.1, 5, -1200, 1200),     // gains, shelves, ratio, bands
            (1.0, 0.1, 10, -1200, 100_000), // threshold and attack
            (10.0, 1.0, 10, 0, 10_000),     // release and crossover
            (50.0, 10.0, 5, 0, 2_400),      // shelf corner
        ];
        for (coarse, fine, per, lo, hi) in grids {
            for j in lo..=hi {
                let value = (f64::from(j) * fine) as f32;
                for up in [true, false] {
                    let next_fine = if up { j + 1 } else { j - 1 };
                    let want = (f64::from(next_fine) * fine) as f32;
                    assert_eq!(snap(value, fine, up), want, "{value} fine up={up}");
                    let next_coarse = if up {
                        j.div_euclid(per) + 1
                    } else {
                        (j + per - 1).div_euclid(per) - 1
                    };
                    let want = (f64::from(next_coarse) * coarse) as f32;
                    assert_eq!(snap(value, coarse, up), want, "{value} coarse up={up}");
                }
            }
        }
    }

    #[test]
    fn values_near_a_grid_line_step_from_it() {
        // A hand-written 1.497 is off the grid: it rounds up to 1.5, not
        // past it, and 1.45 and 1.05 go to the nearest line in the step's
        // direction.
        for (value, step, up, want) in [
            (1.497, 0.5, true, 1.5),
            (1.45, 0.5, true, 1.5),
            (1.05, 0.5, false, 1.0),
            (1.23, 0.5, false, 1.0),
            (0.3, 0.1, false, 0.2),
            (0.7, 0.1, true, 0.8),
        ] {
            assert_eq!(snap(value, step, up), want, "{value} {step} up={up}");
        }
        // A large attack time takes a fine step like any other.
        let mut p = Profile::default();
        p.compressor1.attack_ms = 8000.3;
        let mut t = Tuner::new(p, String::new());
        select(&mut t, Setting::Dynamics(Stage::One, Dynamics::Attack));
        assert_eq!(press(&mut t, Key::ShiftRight), Effect::Apply);
        assert_eq!(t.profile().compressor1.attack_ms, 8000.4);
    }

    #[test]
    fn negative_zero_prints_as_zero() {
        assert_eq!(decimal(-0.0, true), "+0.0");
        assert_eq!(decimal(-0.0, false), "0.0");
        // As a hand-written profile can hold it.
        let p: Profile = toml::from_str("low_shelf_db = -0.0").unwrap();
        let t = Tuner::new(p, String::new());
        let low = t.rows().into_iter().find_map(|row| match row {
            Row::Setting { label, value, .. } if label == "low shelf" => Some(value),
            _ => None,
        });
        assert_eq!(low.as_deref(), Some("+0.0 dB"));
    }

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dps-tuner-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn saved_toml_reads_back_as_the_same_profile() {
        let mut t = tuner();
        let keys = [
            Key::Char('L'), // input gain 0.1
            Key::Down,
            Key::Char('h'), // output gain 6.5
            Key::Char('H'), // 6.4
            Key::Down,
            Key::Char('l'), // corner 1050
        ];
        for code in keys {
            press(&mut t, code);
        }
        select(&mut t, Setting::Band(10));
        press(&mut t, Key::Char('L'));
        press(&mut t, Key::Char('L'));
        press(&mut t, Key::Char('L')); // 0.3
        select(&mut t, Setting::StereoEnabled);
        press(&mut t, Key::Char(' '));
        select(&mut t, Setting::LimiterEnabled);
        press(&mut t, Key::Char(' '));
        press(&mut t, Key::Down);
        press(&mut t, Key::Char('H')); // ceiling -1.1
        press(&mut t, Key::Down);
        press(&mut t, Key::Char('l')); // release 110

        let dir = scratch_dir("save");
        let path = dir.join("nested").join("tuned.toml");
        save(&path, t.profile()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let back: Profile = toml::from_str(&text).unwrap();
        assert_eq!(&back, t.profile());
        assert_eq!(back.input_gain_db, 0.1);
        assert_eq!(back.output_gain_db, 6.4);
        assert_eq!(back.shelf_corner_hz, 1050.0);
        assert_eq!(back.eq.gains_db[10], 0.3);
        assert!(back.stereo.enabled);
        assert!(!back.limiter.enabled);
        assert_eq!(back.limiter.ceiling_db, -1.1);
        assert_eq!(back.limiter.release_ms, 110.0);
        assert!(
            text.contains("gains_db = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.3]"),
            "{text}"
        );
        assert!(
            text.contains("[limiter]\nenabled = false\nceiling_db = -1.1\nrelease_ms = 110.0\n"),
            "{text}"
        );

        // Saving again replaces the file and leaves no temporary file.
        save(&path, &Profile::default()).unwrap();
        let back: Profile = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, Profile::default());
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["tuned.toml"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn names(dir: &Path) -> Vec<OsString> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn saving_through_a_symlink_replaces_the_file_it_points_to() {
        // The link stays a link, and the file keeps its mode, 0600 or one
        // the umask would otherwise take bits off.
        for mode in [0o600, 0o664] {
            let dir = scratch_dir(&format!("link-{mode:o}"));
            fs::create_dir_all(dir.join("real")).unwrap();
            let real = dir.join("real").join("settings.toml");
            fs::write(&real, "# tuned by hand\noutput_gain_db = -1.0\n").unwrap();
            fs::set_permissions(&real, fs::Permissions::from_mode(mode)).unwrap();
            let link = dir.join("settings.toml");
            std::os::unix::fs::symlink("real/settings.toml", &link).unwrap();
            let profile = Profile {
                output_gain_db: -2.0,
                ..Profile::default()
            };
            save(&link, &profile).unwrap();
            assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
            let back: Profile = toml::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
            assert_eq!(back, profile);
            let saved_mode = fs::metadata(&real).unwrap().permissions().mode() & 0o777;
            assert_eq!(saved_mode, mode, "{saved_mode:o}");
            assert_eq!(names(&dir.join("real")), ["settings.toml"]);
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn saving_through_a_dangling_symlink_creates_the_file() {
        let dir = scratch_dir("dangling");
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("settings.toml");
        let real = dir.join("elsewhere").join("dps.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        save(&link, &Profile::default()).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        let back: Profile = toml::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(back, Profile::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_that_fails_halfway_leaves_the_old_file_whole() {
        let dir = scratch_dir("torn");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        fs::write(&path, "output_gain_db = -1.0\n").unwrap();
        let e = replace(&path, |file| {
            file.write_all(b"output_gain")?;
            Err(io::Error::other("disk full"))
        })
        .unwrap_err();
        assert!(e.ends_with("disk full"), "{e}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "output_gain_db = -1.0\n"
        );
        assert_eq!(names(&dir), ["settings.toml"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_save_says_why_and_leaves_nothing_behind() {
        let dir = scratch_dir("blocked");
        fs::create_dir_all(&dir).unwrap();
        // A directory where the file should go.
        let path = dir.join("taken.toml");
        fs::create_dir(&path).unwrap();
        let e = save(&path, &Profile::default()).unwrap_err();
        assert!(
            e.starts_with(&format!("cannot save to {}", path.display())),
            "{e}"
        );
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["taken.toml"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
