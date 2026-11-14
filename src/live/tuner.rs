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
    LevelerEnabled,
    LevelerTarget,
    LevelerMaxBoost,
    LevelerMaxCut,
    LevelerRise,
    LevelerFall,
    InputGain,
    OutputGain,
    ShelfCorner,
    LowShelf,
    HighShelf,
    Dynamics(Stage, Dynamics),
    /// One EQ band: its frequency, Q and gain on one row.
    Band(usize),
    BassEnabled,
    BassCutoff,
    BassHarmonics,
    BassRemoveLows,
    ContourEnabled,
    ContourReference,
    ContourBassPerDb,
    ContourTreblePerDb,
    ContourMaxBass,
    ContourMaxTreble,
    StereoEnabled,
    Crossover,
    SideBoost,
    CrossfeedEnabled,
    CrossfeedCutoff,
    CrossfeedLevel,
    CorrectionEnabled,
    CorrectionPreamp,
    LimiterEnabled,
    Ceiling,
    LimiterRelease,
    Lookahead,
    TruePeak,
}

/// The value of an EQ band that Left and Right change; other settings have
/// only one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Field {
    Freq,
    Q,
    Gain,
}

impl Field {
    /// The next field to the right in the order shown, frequency, Q, gain,
    /// going round; or, if not `forward`, the next to the left.
    fn next(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Gain, true) | (Self::Q, false) => Self::Freq,
            (Self::Freq, true) | (Self::Gain, false) => Self::Q,
            (Self::Q, true) | (Self::Freq, false) => Self::Gain,
        }
    }
}

/// The highest EQ band frequency the tuner steps up to. Nothing above it is
/// audible, and it is Nyquist at 48 kHz, so a band can still be moved to
/// where the chain skips it at 44.1 and 48 kHz. Being fixed, what can be
/// reached does not depend on the device in use while tuning.
const MAX_BAND_HZ: f32 = 24_000.0;

/// The leveler's slowest rate up or down, which validation allows; it is off
/// the rates' grid.
const MIN_LEVELER_RATE: f32 = 0.05;

enum Unit {
    Db,
    Hz,
    Ratio,
    Ms,
    /// An EQ band's frequency.
    BandHz,
    Q,
    Lufs,
    DbPerS,
    /// dB added per dB of volume.
    DbPerDb,
}

/// How a value moves on Left and Right.
#[derive(Clone, Copy)]
enum Grid {
    /// To the next multiple of the coarse or the fine step.
    Linear { coarse: f64, fine: f64 },
    /// Like `Linear`, but a step down that would pass `min` stops on it, so
    /// a minimum off the grid can be reached.
    Floored { coarse: f64, fine: f64, min: f32 },
    /// Along `octave_point`s through `anchor`.
    Octaves { anchor: f64 },
}

/// `Octaves` grid points per octave, and points per coarse step: fine steps
/// of 1/48 octave (a quarter semitone, 1.5 %) and coarse ones of 1/6 octave
/// (a whole tone, 12 %). Q takes the same steps, as what it does is also
/// proportional: its range, 0.1 to 20, is 46 coarse steps.
const POINTS_PER_OCTAVE: i32 = 48;
const POINTS_PER_COARSE_STEP: i32 = 8;

impl Grid {
    fn step(self, value: f32, up: bool, fine: bool) -> f32 {
        match self {
            Self::Linear {
                coarse,
                fine: fine_step,
            } => snap(value, if fine { fine_step } else { coarse }, up),
            Self::Floored {
                coarse,
                fine: fine_step,
                min,
            } => {
                let next = snap(value, if fine { fine_step } else { coarse }, up);
                if !up && next < min && value > min {
                    min
                } else {
                    next
                }
            }
            Self::Octaves { anchor } => {
                let every = if fine { 1 } else { POINTS_PER_COARSE_STEP };
                step_octaves(value, anchor, every, up)
            }
        }
    }
}

/// Point `k` of the grid through `anchor`: `anchor * 2^(k/48)` to three
/// significant figures. Adjacent points stay distinct and in order, as they
/// are 1.45 % apart and rounding moves each by at most 0.5 %.
fn octave_point(anchor: f64, k: i32) -> f32 {
    let exact = anchor * (f64::from(k) / f64::from(POINTS_PER_OCTAVE)).exp2();
    let decimals = 2 - exact.log10().floor() as i32;
    // Dividing by an exact power of ten, not multiplying by an inexact one,
    // gives the f64 nearest the decimal, so the f32 is the one it parses to.
    // Ties (31.25 Hz) go to even, as the formatter rounds them.
    let rounded = if decimals >= 0 {
        let scale = 10f64.powi(decimals);
        (exact * scale).round_ties_even() / scale
    } else {
        let scale = 10f64.powi(-decimals);
        (exact / scale).round_ties_even() * scale
    };
    rounded as f32
}

/// The point of the grid through `anchor` next above or below `value` whose
/// index is a multiple of `every`.
fn step_octaves(value: f32, anchor: f64, every: i32, up: bool) -> f32 {
    let point = |k| octave_point(anchor, k);
    let x = (f64::from(value) / anchor).log2() * f64::from(POINTS_PER_OCTAVE);
    // Rounding moves a point by under half the spacing, so the point one
    // below `x` is below `value` and the one above `x` is above it.
    let k = if up {
        let mut k = x.floor() as i32 - 1;
        while point(k) <= value {
            k += 1;
        }
        (k + every - 1).div_euclid(every) * every
    } else {
        let mut k = x.ceil() as i32 + 1;
        while point(k) >= value {
            k -= 1;
        }
        k.div_euclid(every) * every
    };
    point(k)
}

impl Setting {
    /// Every setting, in the order listed.
    fn all() -> Vec<Self> {
        use Dynamics::*;
        let dynamics =
            |stage| [Threshold, Ratio, Attack, Release].map(|d| Self::Dynamics(stage, d));
        [
            Self::LevelerEnabled,
            Self::LevelerTarget,
            Self::LevelerMaxBoost,
            Self::LevelerMaxCut,
            Self::LevelerRise,
            Self::LevelerFall,
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
        .chain([
            Self::BassEnabled,
            Self::BassCutoff,
            Self::BassHarmonics,
            Self::BassRemoveLows,
            Self::ContourEnabled,
            Self::ContourReference,
            Self::ContourBassPerDb,
            Self::ContourTreblePerDb,
            Self::ContourMaxBass,
            Self::ContourMaxTreble,
            Self::StereoEnabled,
            Self::Crossover,
            Self::SideBoost,
        ])
        .chain(dynamics(Stage::Stereo))
        .chain([
            Self::CrossfeedEnabled,
            Self::CrossfeedCutoff,
            Self::CrossfeedLevel,
            Self::CorrectionEnabled,
            Self::CorrectionPreamp,
            Self::LimiterEnabled,
            Self::Ceiling,
            Self::LimiterRelease,
            Self::Lookahead,
            Self::TruePeak,
        ])
        .collect()
    }

    fn group(self) -> &'static str {
        match self {
            Self::LevelerEnabled
            | Self::LevelerTarget
            | Self::LevelerMaxBoost
            | Self::LevelerMaxCut
            | Self::LevelerRise
            | Self::LevelerFall => "Leveler",
            Self::InputGain | Self::OutputGain => "Gains",
            Self::ShelfCorner | Self::LowShelf | Self::HighShelf => "Shelves",
            Self::Dynamics(Stage::One, _) => "Compressor 1",
            Self::Dynamics(Stage::Two, _) => "Compressor 2",
            Self::Band(_) => "EQ bands",
            Self::BassEnabled | Self::BassCutoff | Self::BassHarmonics | Self::BassRemoveLows => {
                "Bass"
            }
            Self::ContourEnabled
            | Self::ContourReference
            | Self::ContourBassPerDb
            | Self::ContourTreblePerDb
            | Self::ContourMaxBass
            | Self::ContourMaxTreble => "Contour",
            Self::StereoEnabled
            | Self::Crossover
            | Self::SideBoost
            | Self::Dynamics(Stage::Stereo, _) => "Stereo",
            Self::CrossfeedEnabled | Self::CrossfeedCutoff | Self::CrossfeedLevel => "Crossfeed",
            Self::CorrectionEnabled | Self::CorrectionPreamp => "Correction",
            Self::LimiterEnabled
            | Self::Ceiling
            | Self::LimiterRelease
            | Self::Lookahead
            | Self::TruePeak => "Limiter",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::LevelerTarget => "target",
            Self::LevelerMaxBoost => "max boost",
            Self::LevelerMaxCut => "max cut",
            Self::LevelerRise => "rise",
            Self::LevelerFall => "fall",
            Self::InputGain => "input gain",
            Self::OutputGain => "output gain",
            Self::ShelfCorner => "corner",
            Self::LowShelf => "low shelf",
            Self::HighShelf => "high shelf",
            Self::Dynamics(_, Dynamics::Threshold) => "threshold",
            Self::Dynamics(_, Dynamics::Ratio) => "ratio",
            Self::Dynamics(_, Dynamics::Attack) => "attack",
            Self::Dynamics(_, Dynamics::Release) | Self::LimiterRelease => "release",
            Self::Band(_) => "EQ band",
            Self::LevelerEnabled
            | Self::BassEnabled
            | Self::ContourEnabled
            | Self::StereoEnabled
            | Self::CrossfeedEnabled
            | Self::CorrectionEnabled
            | Self::LimiterEnabled => "enabled",
            Self::BassCutoff | Self::CrossfeedCutoff => "cutoff",
            Self::BassHarmonics => "harmonics",
            Self::BassRemoveLows => "remove lows",
            Self::ContourReference => "reference",
            Self::ContourBassPerDb => "bass per dB",
            Self::ContourTreblePerDb => "treble per dB",
            Self::ContourMaxBass => "max bass",
            Self::ContourMaxTreble => "max treble",
            Self::Crossover => "crossover",
            Self::SideBoost => "side boost",
            Self::CrossfeedLevel => "level",
            Self::CorrectionPreamp => "preamp",
            Self::Ceiling => "ceiling",
            Self::Lookahead => "lookahead",
            Self::TruePeak => "true peak",
        }
    }

    /// The value this setting changes, `field` of it for an EQ band; `None`
    /// for an on/off switch.
    fn number(self, field: Field, p: &mut Profile) -> Option<&mut f32> {
        Some(match self {
            Self::LevelerTarget => &mut p.leveler.target_lufs,
            Self::LevelerMaxBoost => &mut p.leveler.max_boost_db,
            Self::LevelerMaxCut => &mut p.leveler.max_cut_db,
            Self::LevelerRise => &mut p.leveler.rise_db_per_s,
            Self::LevelerFall => &mut p.leveler.fall_db_per_s,
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
            Self::Band(i) => match field {
                Field::Freq => &mut p.eq.freqs_hz[i],
                Field::Q => &mut p.eq.q[i],
                Field::Gain => &mut p.eq.gains_db[i],
            },
            Self::LevelerEnabled
            | Self::BassEnabled
            | Self::BassRemoveLows
            | Self::ContourEnabled
            | Self::StereoEnabled
            | Self::CrossfeedEnabled
            | Self::CorrectionEnabled
            | Self::LimiterEnabled
            | Self::TruePeak => return None,
            Self::BassCutoff => &mut p.bass.cutoff_hz,
            Self::BassHarmonics => &mut p.bass.harmonics_db,
            Self::ContourReference => &mut p.contour.reference_db,
            Self::ContourBassPerDb => &mut p.contour.bass_per_db,
            Self::ContourTreblePerDb => &mut p.contour.treble_per_db,
            Self::ContourMaxBass => &mut p.contour.max_bass_db,
            Self::ContourMaxTreble => &mut p.contour.max_treble_db,
            Self::Crossover => &mut p.stereo.crossover_hz,
            Self::SideBoost => &mut p.stereo.side_boost_db,
            Self::CrossfeedCutoff => &mut p.crossfeed.cutoff_hz,
            Self::CrossfeedLevel => &mut p.crossfeed.level_db,
            Self::CorrectionPreamp => &mut p.correction.preamp_db,
            Self::Ceiling => &mut p.limiter.ceiling_db,
            Self::LimiterRelease => &mut p.limiter.release_ms,
            Self::Lookahead => &mut p.limiter.lookahead_ms,
        })
    }

    /// The switch this setting turns on and off, if it is one.
    fn switch(self, p: &mut Profile) -> Option<&mut bool> {
        match self {
            Self::LevelerEnabled => Some(&mut p.leveler.enabled),
            Self::BassEnabled => Some(&mut p.bass.enabled),
            Self::BassRemoveLows => Some(&mut p.bass.remove_lows),
            Self::ContourEnabled => Some(&mut p.contour.enabled),
            Self::StereoEnabled => Some(&mut p.stereo.enabled),
            Self::CrossfeedEnabled => Some(&mut p.crossfeed.enabled),
            Self::CorrectionEnabled => Some(&mut p.correction.enabled),
            Self::LimiterEnabled => Some(&mut p.limiter.enabled),
            Self::TruePeak => Some(&mut p.limiter.true_peak),
            _ => None,
        }
    }

    fn unit(self, field: Field) -> Unit {
        match self {
            Self::Band(_) => match field {
                Field::Freq => Unit::BandHz,
                Field::Q => Unit::Q,
                Field::Gain => Unit::Db,
            },
            Self::ShelfCorner | Self::Crossover | Self::BassCutoff | Self::CrossfeedCutoff => {
                Unit::Hz
            }
            Self::Dynamics(_, Dynamics::Ratio) => Unit::Ratio,
            Self::Dynamics(_, Dynamics::Attack | Dynamics::Release)
            | Self::LimiterRelease
            | Self::Lookahead => Unit::Ms,
            Self::LevelerTarget => Unit::Lufs,
            Self::LevelerRise | Self::LevelerFall => Unit::DbPerS,
            Self::ContourBassPerDb | Self::ContourTreblePerDb => Unit::DbPerDb,
            _ => Unit::Db,
        }
    }

    fn grid(self, field: Field) -> Grid {
        let linear = |coarse, fine| Grid::Linear { coarse, fine };
        match self {
            Self::Band(_) => match field {
                Field::Freq => Grid::Octaves { anchor: 1000.0 },
                Field::Q => Grid::Octaves { anchor: 1.0 },
                Field::Gain => linear(0.5, 0.1),
            },
            Self::ShelfCorner | Self::CrossfeedCutoff => linear(50.0, 10.0),
            Self::Crossover => linear(10.0, 1.0),
            Self::BassCutoff => linear(5.0, 1.0),
            Self::Dynamics(_, Dynamics::Threshold | Dynamics::Attack) => linear(1.0, 0.1),
            Self::Dynamics(_, Dynamics::Release) | Self::LimiterRelease => linear(10.0, 1.0),
            Self::LevelerRise | Self::LevelerFall => Grid::Floored {
                coarse: 0.5,
                fine: 0.1,
                min: MIN_LEVELER_RATE,
            },
            Self::ContourBassPerDb | Self::ContourTreblePerDb => linear(0.05, 0.01),
            _ => linear(0.5, 0.1),
        }
    }

    fn format(self, field: Field, profile: &mut Profile) -> String {
        let Some(&mut v) = self.number(field, profile) else {
            let on = *self
                .switch(profile)
                .expect("a setting is a number or a switch");
            return if on { "on" } else { "off" }.into();
        };
        match self.unit(field) {
            Unit::Db => format!("{} dB", decimal(v, 1, true)),
            Unit::Hz => format!("{v} Hz"),
            Unit::Ratio => format!("{}:1", decimal(v, 1, false)),
            Unit::Ms => format!("{} ms", decimal(v, 1, false)),
            Unit::BandHz => format!("{} Hz", significant(v)),
            Unit::Q => format!("Q {}", significant(v)),
            Unit::Lufs => format!("{} LUFS", decimal(v, 1, true)),
            Unit::DbPerS => format!("{} dB/s", decimal(v, 1, false)),
            Unit::DbPerDb => format!("{} dB/dB", decimal(v, 2, false)),
        }
    }
}

/// Positive `v` to three significant figures, as the octave grids hold
/// values, or with as many as it needs to be exact.
fn significant(v: f32) -> String {
    let decimals = (2 - f64::from(v).log10().floor() as i32).max(0) as usize;
    let short = format!("{v:.decimals$}");
    if short.parse::<f32>() == Ok(v) {
        short
    } else {
        format!("{v}")
    }
}

/// `v` with `places` decimals, or with as many as it needs to be exact.
fn decimal(v: f32, places: usize, signed: bool) -> String {
    let v = v + 0.0; // never "-0.0"
    let short = if signed {
        format!("{v:+.places$}")
    } else {
        format!("{v:.places$}")
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
    Tab,
    /// Shift+Tab.
    BackTab,
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
    /// Move to the EQ band field to the right, or to the left.
    NextField,
    PreviousField,
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
    Band {
        freq: String,
        q: String,
        gain: String,
        /// The field Left and Right change, on the selected band only.
        active: Option<Field>,
        /// The chain skips the band at the rate the rows were made for.
        skipped: bool,
        selected: bool,
    },
}

impl Row {
    pub fn selected(&self) -> bool {
        match self {
            Self::Group(_) => false,
            Self::Setting { selected, .. } | Self::Band { selected, .. } => *selected,
        }
    }
}

pub struct Tuner {
    profile: Profile,
    /// What was last saved or loaded.
    saved: Profile,
    settings: Vec<Setting>,
    selected: usize,
    /// The field of whichever EQ band is selected that Left and Right change.
    field: Field,
    bypass: bool,
    /// Asking whether to save before quitting; the question is drawn from
    /// this, not kept in `message`.
    quitting: bool,
    message: String,
    /// The message is about the last change, a refusal or a skipped band,
    /// and is cleared by the next change that goes through.
    transient: bool,
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
            field: Field::Gain,
            bypass: false,
            quitting: false,
            message: String::new(),
            transient: false,
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
        self.transient = false;
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

    /// The settings list, with the EQ bands the chain skips at `rate` marked.
    pub fn rows(&self, rate: u32) -> Vec<Row> {
        let mut scratch = self.profile.clone();
        let skipped = self.profile.skipped_eq_bands(rate);
        let mut rows = Vec::new();
        let mut group = "";
        for (i, &setting) in self.settings.iter().enumerate() {
            if setting.group() != group {
                group = setting.group();
                rows.push(Row::Group(group));
            }
            let selected = i == self.selected;
            let mut format = |field| setting.format(field, &mut scratch);
            rows.push(match setting {
                Setting::Band(band) => Row::Band {
                    freq: format(Field::Freq),
                    q: format(Field::Q),
                    gain: format(Field::Gain),
                    active: selected.then_some(self.field),
                    skipped: skipped.contains(&band),
                    selected,
                },
                _ => Row::Setting {
                    label: setting.label().into(),
                    value: format(self.field),
                    selected,
                },
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
            Key::Tab => Action::NextField,
            Key::BackTab => Action::PreviousField,
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
                } else if let Some(value) = setting.number(self.field, &mut candidate) {
                    let next = setting.grid(self.field).step(*value, up, fine);
                    if let (Setting::Band(i), Field::Freq) = (setting, self.field)
                        && up
                        && next > MAX_BAND_HZ
                    {
                        return self.refuse(format!(
                            "eq.freqs_hz[{i}] = {next} Hz is above the tuner's {MAX_BAND_HZ} Hz limit"
                        ));
                    }
                    *value = next;
                }
                let effect = self.offer(candidate, rate);
                if let Setting::Band(i) = setting
                    && effect == Effect::Apply
                    && self.profile.skipped_eq_bands(rate).contains(&i)
                {
                    self.message = format!(
                        "this band is skipped at {rate} Hz: a band's centre and bandwidth \
                         (freq / Q) must be below {} Hz",
                        f64::from(rate) / 2.0
                    );
                    self.transient = true;
                }
                effect
            }
            Action::NextField | Action::PreviousField => {
                if let Setting::Band(_) = setting {
                    self.field = self.field.next(action == Action::NextField);
                }
                Effect::None
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
                if self.transient {
                    self.set_message(String::new());
                }
                Effect::Apply
            }
            Err(e) => self.refuse(e),
        }
    }

    fn refuse(&mut self, reason: String) -> Effect {
        self.message = format!("refused: {reason}");
        self.transient = true;
        Effect::None
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

    /// Where `setting` is in the list.
    fn position(setting: Setting) -> usize {
        Setting::all().iter().position(|&s| s == setting).unwrap()
    }

    /// Moves down to `setting`, which must be at or below the selection.
    fn select(t: &mut Tuner, setting: Setting) {
        let target = t.settings.iter().position(|&s| s == setting).unwrap();
        while t.selected < target {
            assert_eq!(press(t, Key::Down), Effect::None);
        }
        assert_eq!(t.settings[t.selected], setting);
    }

    /// The selected row's label and value; for a band, its frequency and Q,
    /// and its gain.
    fn selected(t: &Tuner) -> (String, String) {
        t.rows(RATE)
            .into_iter()
            .find_map(|row| match row {
                Row::Setting {
                    label,
                    value,
                    selected: true,
                } => Some((label, value)),
                Row::Band {
                    freq,
                    q,
                    gain,
                    selected: true,
                    ..
                } => Some((format!("{freq}  {q}"), gain)),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn every_setting_is_listed_once_under_its_group() {
        let rows = tuner().rows(RATE);
        let groups: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Group(g) => Some(*g),
                _ => None,
            })
            .collect();
        assert_eq!(
            groups,
            [
                "Leveler",
                "Gains",
                "Shelves",
                "Compressor 1",
                "Compressor 2",
                "EQ bands",
                "Bass",
                "Contour",
                "Stereo",
                "Crossfeed",
                "Correction",
                "Limiter"
            ]
        );
        // 6 leveler values, 2 gains, 3 shelf values, 4 + 4 compressor
        // values, 11 bands, 4 bass values, 6 contour values, 7 stereo
        // values, 3 crossfeed values, 2 correction values and 5 limiter
        // values.
        assert_eq!(rows.len() - groups.len(), 57);
        let labels: Vec<String> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Setting { label, value, .. } => Some(format!("{label}: {value}")),
                Row::Band { freq, q, gain, .. } => Some(format!("{freq}  {q}: {gain}")),
                Row::Group(_) => None,
            })
            .collect();
        let compressor = [
            "threshold: -20.0 dB",
            "ratio: 4.0:1",
            "attack: 10.0 ms",
            "release: 100.0 ms",
        ];
        let want: Vec<&str> = [
            "enabled: off",
            "target: -14.0 LUFS",
            "max boost: +12.0 dB",
            "max cut: +12.0 dB",
            "rise: 1.0 dB/s",
            "fall: 4.0 dB/s",
            "input gain: +0.0 dB",
            "output gain: +7.0 dB",
            "corner: 1000 Hz",
            "low shelf: -10.0 dB",
            "high shelf: +8.0 dB",
        ]
        .into_iter()
        .chain(compressor)
        .chain(compressor)
        .chain([
            "30.0 Hz  Q 1.50: +0.0 dB",
            "56.4 Hz  Q 1.50: +0.0 dB",
            "106 Hz  Q 1.50: +0.0 dB",
            "199 Hz  Q 1.50: +0.0 dB",
            "375 Hz  Q 1.50: +0.0 dB",
            "705 Hz  Q 1.50: +0.0 dB",
            // Off the three-figure grid, shown exactly.
            "1327 Hz  Q 1.50: +0.0 dB",
            "2497 Hz  Q 1.50: +0.0 dB",
            "4698 Hz  Q 1.50: +0.0 dB",
            "8839 Hz  Q 1.50: +0.0 dB",
            "16000 Hz  Q 1.50: +0.0 dB",
            "enabled: off",
            "cutoff: 100 Hz",
            "harmonics: +0.0 dB",
            "remove lows: off",
            "enabled: off",
            "reference: -12.0 dB",
            "bass per dB: 0.35 dB/dB",
            "treble per dB: 0.10 dB/dB",
            "max bass: +9.0 dB",
            "max treble: +4.0 dB",
            "enabled: off",
            "crossover: 200 Hz",
            "side boost: +8.0 dB",
            "threshold: -30.0 dB",
            "ratio: 4.0:1",
            "attack: 10.0 ms",
            "release: 200.0 ms",
            "enabled: off",
            "cutoff: 700 Hz",
            "level: -4.5 dB",
            "enabled: off",
            "preamp: +0.0 dB",
            "enabled: on",
            "ceiling: -1.0 dB",
            "release: 100.0 ms",
            "lookahead: 1.5 ms",
            "true peak: on",
        ])
        .collect();
        assert_eq!(labels, want);
    }

    #[test]
    fn selection_moves_and_stops_at_both_ends() {
        let mut t = tuner();
        assert_eq!(press(&mut t, Key::Up), Effect::None);
        assert_eq!(t.position(), (0, 57));
        press(&mut t, Key::Char('j'));
        press(&mut t, Key::Down);
        assert_eq!(selected(&t).0, "max boost");
        press(&mut t, Key::Char('k'));
        assert_eq!(selected(&t).0, "target");
        for _ in 0..60 {
            press(&mut t, Key::Down);
        }
        assert_eq!(t.position(), (56, 57));
        assert_eq!(t.settings[56], Setting::TruePeak);
    }

    #[test]
    fn each_setting_steps_by_its_own_amount() {
        use Dynamics::*;
        // (setting, after one coarse step up, after one fine step down from there)
        let cases = [
            (Setting::LevelerTarget, "-13.5 LUFS", "-13.6 LUFS"),
            (Setting::LevelerMaxBoost, "+12.5 dB", "+12.4 dB"),
            (Setting::LevelerMaxCut, "+12.5 dB", "+12.4 dB"),
            (Setting::LevelerRise, "1.5 dB/s", "1.4 dB/s"),
            (Setting::LevelerFall, "4.5 dB/s", "4.4 dB/s"),
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
            (Setting::BassCutoff, "105 Hz", "104 Hz"),
            (Setting::BassHarmonics, "+0.5 dB", "+0.4 dB"),
            (Setting::ContourReference, "-11.5 dB", "-11.6 dB"),
            (Setting::ContourBassPerDb, "0.40 dB/dB", "0.39 dB/dB"),
            (Setting::ContourTreblePerDb, "0.15 dB/dB", "0.14 dB/dB"),
            (Setting::ContourMaxBass, "+9.5 dB", "+9.4 dB"),
            (Setting::ContourMaxTreble, "+4.5 dB", "+4.4 dB"),
            (Setting::Crossover, "210 Hz", "209 Hz"),
            (Setting::SideBoost, "+8.5 dB", "+8.4 dB"),
            (
                Setting::Dynamics(Stage::Stereo, Release),
                "210.0 ms",
                "209.0 ms",
            ),
            (Setting::CrossfeedCutoff, "750 Hz", "740 Hz"),
            (Setting::CrossfeedLevel, "-4.0 dB", "-4.1 dB"),
            (Setting::CorrectionPreamp, "+0.5 dB", "+0.4 dB"),
            (Setting::Ceiling, "-0.5 dB", "-0.6 dB"),
            (Setting::LimiterRelease, "110.0 ms", "109.0 ms"),
            (Setting::Lookahead, "2.0 ms", "1.9 ms"),
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
        select(&mut t, Setting::InputGain);
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
        select(&mut t, Setting::InputGain);
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
        select(&mut t, Setting::InputGain);
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
        select(&mut t, Setting::InputGain);
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
        select(&mut t, Setting::InputGain);
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
        select(&mut t, Setting::InputGain);
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
        select(&mut t, Setting::InputGain);
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
        assert_eq!(decimal(-0.0, 1, true), "+0.0");
        assert_eq!(decimal(-0.0, 1, false), "0.0");
        assert_eq!(decimal(-0.0, 2, false), "0.00");
        // As a hand-written profile can hold it.
        let p: Profile = toml::from_str("low_shelf_db = -0.0").unwrap();
        let t = Tuner::new(p, String::new());
        let low = t.rows(RATE).into_iter().find_map(|row| match row {
            Row::Setting { label, value, .. } if label == "low shelf" => Some(value),
            _ => None,
        });
        assert_eq!(low.as_deref(), Some("+0.0 dB"));
    }

    /// Point `k` of the grid through `anchor`, worked out apart from
    /// `octave_point`: `anchor * 2^(k/48)` to three significant figures, as
    /// the formatter rounds it.
    fn point(anchor: f64, k: i32) -> f32 {
        format!("{:.2e}", anchor * 2f64.powf(f64::from(k) / 48.0))
            .parse()
            .unwrap()
    }

    /// The point of the grid through `anchor` nearest `value` in the
    /// direction of `up` whose index is a multiple of `every`, found by
    /// trying every index.
    fn next_point(anchor: f64, value: f32, every: i32, up: bool) -> f32 {
        let candidates = (-1200..=600)
            .filter(|k: &i32| k.rem_euclid(every) == 0)
            .map(|k| point(anchor, k));
        if up {
            candidates.filter(|&p| p > value).reduce(f32::min).unwrap()
        } else {
            candidates.filter(|&p| p < value).reduce(f32::max).unwrap()
        }
    }

    /// Moves up or down to band `band` and makes `field` the active one,
    /// with Tab.
    fn select_field(t: &mut Tuner, band: usize, field: Field) {
        let target = t
            .settings
            .iter()
            .position(|&s| s == Setting::Band(band))
            .unwrap();
        while t.selected != target {
            let key = if t.selected < target {
                Key::Down
            } else {
                Key::Up
            };
            assert_eq!(press(t, key), Effect::None);
        }
        while t.field != field {
            assert_eq!(press(t, Key::Tab), Effect::None);
        }
    }

    /// The field shown as active, and on which band row.
    fn active(t: &Tuner) -> Vec<(usize, Field)> {
        let bands = t
            .rows(RATE)
            .into_iter()
            .filter(|r| matches!(r, Row::Band { .. }));
        bands
            .enumerate()
            .filter_map(|(i, r)| match r {
                Row::Band {
                    active: Some(f), ..
                } => Some((i, f)),
                _ => None,
            })
            .collect()
    }

    /// Which bands the rows mark as skipped at `rate`.
    fn marked_skipped(t: &Tuner, rate: u32) -> Vec<usize> {
        let bands = t
            .rows(rate)
            .into_iter()
            .filter(|r| matches!(r, Row::Band { .. }));
        bands
            .enumerate()
            .filter_map(|(i, r)| match r {
                Row::Band { skipped: true, .. } => Some(i),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tab_moves_through_a_bands_fields_and_the_field_stays_across_bands() {
        let mut t = tuner();
        select(&mut t, Setting::Band(0));
        assert_eq!(active(&t), [(0, Field::Gain)]);
        // Tab goes right through frequency, Q and gain; Shift+Tab goes left.
        for want in [Field::Freq, Field::Q, Field::Gain, Field::Freq] {
            assert_eq!(press(&mut t, Key::Tab), Effect::None);
            assert_eq!(active(&t), [(0, want)]);
        }
        for want in [Field::Gain, Field::Q, Field::Freq, Field::Gain, Field::Q] {
            assert_eq!(press(&mut t, Key::BackTab), Effect::None);
            assert_eq!(active(&t), [(0, want)]);
        }
        assert!(!t.dirty());
        // Q stays the field on the next band, where Right changes that
        // band's Q and nothing else.
        press(&mut t, Key::Down);
        assert_eq!(active(&t), [(1, Field::Q)]);
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);
        let mut want = Profile::default();
        want.eq.q[1] = 1.59;
        assert_eq!(t.profile(), &want);
        // Up through the settings above the bands, which step as before,
        // and back: Q is still the field.
        press(&mut t, Key::Up);
        press(&mut t, Key::Up);
        assert_eq!(selected(&t), ("release".into(), "100.0 ms".into()));
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);
        assert_eq!(t.profile().compressor2.release_ms, 110.0);
        select(&mut t, Setting::Band(7));
        assert_eq!(active(&t), [(7, Field::Q)]);
        // Below the bands, a switch still switches.
        select(&mut t, Setting::BassEnabled);
        assert_eq!(active(&t), []);
        assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        assert!(t.profile().bass.enabled);
        // A reload keeps the field too.
        assert!(t.reload(Profile::default()));
        press(&mut t, Key::Up);
        assert_eq!(active(&t), [(10, Field::Q)]);
    }

    #[test]
    fn tab_does_nothing_on_other_rows() {
        let mut t = tuner();
        t.set_message("now on External Headphones at 44100 Hz".into());
        for setting in [
            Setting::InputGain,
            Setting::Dynamics(Stage::Two, Dynamics::Release),
            Setting::StereoEnabled,
            Setting::LimiterRelease,
        ] {
            select(&mut t, setting);
            for key in [Key::Tab, Key::BackTab, Key::Tab] {
                assert_eq!(press(&mut t, key), Effect::None, "{setting:?}");
            }
            assert_eq!(t.settings[t.selected], setting);
        }
        assert!(!t.dirty());
        assert_eq!(t.message(), "now on External Headphones at 44100 Hz");
        // The field did not move: on a band, gain is still the one changed.
        let first_band = position(Setting::Band(0));
        while t.selected > first_band {
            press(&mut t, Key::Up);
        }
        assert_eq!(t.settings[t.selected], Setting::Band(0));
        assert_eq!(active(&t), [(0, Field::Gain)]);
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);
        assert_eq!(t.profile().eq.gains_db[0], 0.5);
        // While asking whether to save, Tab does nothing either.
        press(&mut t, Key::Char('q'));
        assert_eq!(press(&mut t, Key::Tab), Effect::None);
        assert_eq!(press(&mut t, Key::BackTab), Effect::None);
        assert!(t.prompt().is_some());
        assert_eq!(press(&mut t, Key::Esc), Effect::None);
        assert_eq!(active(&t), [(0, Field::Gain)]);
    }

    #[test]
    fn frequency_and_q_grids_are_48ths_of_an_octave_to_three_figures() {
        // Every point from below the lowest band frequency any rate allows
        // to above the tuner's limit, and over Q's whole range, stepped both
        // ways by the fine and the coarse step.
        for (anchor, lo, hi) in [(1000.0, -820, 230), (1.0, -165, 212)] {
            for k in lo..=hi {
                let value = point(anchor, k);
                assert_eq!(octave_point(anchor, k), value, "{anchor} {k}");
                let coarse_up = (k.div_euclid(8) + 1) * 8;
                let coarse_down = (k + 7).div_euclid(8) * 8 - 8;
                for (every, up, want) in [
                    (1, true, k + 1),
                    (1, false, k - 1),
                    (8, true, coarse_up),
                    (8, false, coarse_down),
                ] {
                    assert_eq!(
                        step_octaves(value, anchor, every, up),
                        point(anchor, want),
                        "{anchor} {value} every {every} up={up}"
                    );
                }
            }
        }
        // Values off the grid, as the defaults and hand-written files have
        // them, go to the nearest point in the step's direction.
        for value in [30.0, 56.4, 106.0, 1327.0, 4698.0, 8839.0, 30_000.0, 0.05] {
            for (every, up) in [(1, true), (1, false), (8, true), (8, false)] {
                assert_eq!(
                    step_octaves(value, 1000.0, every, up),
                    next_point(1000.0, value, every, up),
                    "{value} every {every} up={up}"
                );
            }
        }
        for value in [std::f32::consts::FRAC_1_SQRT_2, 2.5, 20.0, 0.1, 1.0] {
            for (every, up) in [(1, true), (1, false), (8, true), (8, false)] {
                assert_eq!(
                    step_octaves(value, 1.0, every, up),
                    next_point(1.0, value, every, up),
                    "Q {value} every {every} up={up}"
                );
            }
        }
        // As the keys step them, from numbers worked out by hand.
        let mut t = tuner();
        select_field(&mut t, 10, Field::Freq);
        let mut freqs = vec![];
        for key in [Key::Right, Key::ShiftLeft, Key::Left, Key::Left] {
            assert_eq!(press(&mut t, key), Effect::Apply);
            freqs.push(t.profile().eq.freqs_hz[10]);
        }
        assert_eq!(freqs, [18_000.0, 17_700.0, 16_000.0, 14_300.0]);
        select_field(&mut t, 0, Field::Freq);
        for key in [Key::ShiftRight, Key::ShiftLeft, Key::ShiftLeft, Key::Right] {
            press(&mut t, key);
            freqs.push(t.profile().eq.freqs_hz[0]);
        }
        assert_eq!(&freqs[4..], [30.4, 29.9, 29.5, 31.2]);
        assert_eq!(selected(&t).0, "31.2 Hz  Q 1.50");
        press(&mut t, Key::Tab);
        for key in [
            Key::ShiftRight,
            Key::Right,
            Key::Left,
            Key::Left,
            Key::ShiftLeft,
        ] {
            press(&mut t, key);
            freqs.push(t.profile().eq.q[0]);
        }
        assert_eq!(&freqs[8..], [1.52, 1.59, 1.41, 1.26, 1.24]);
        assert_eq!(selected(&t).0, "31.2 Hz  Q 1.24");
    }

    #[test]
    fn frequency_and_q_return_to_their_start_and_never_drift() {
        let mut p = Profile::default();
        p.eq.freqs_hz[5] = 1000.0;
        let mut t = Tuner::new(p, String::new());
        select_field(&mut t, 5, Field::Freq);
        // 200 fine steps up and back, each on the grid.
        for (key, step) in [(Key::ShiftRight, 1), (Key::ShiftLeft, -1)] {
            for i in 1..=200 {
                assert_eq!(press(&mut t, key), Effect::Apply);
                let k = if step > 0 { i } else { 200 - i };
                assert_eq!(t.profile().eq.freqs_hz[5], point(1000.0, k), "{k}");
            }
        }
        assert_eq!(t.profile().eq.freqs_hz[5], 1000.0);
        assert!(!t.dirty());
        // Coarse steps up to the limit and back down.
        for _ in 0..27 {
            assert_eq!(press(&mut t, Key::Right), Effect::Apply);
        }
        assert_eq!(t.profile().eq.freqs_hz[5], 22_600.0);
        for _ in 0..27 {
            assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        }
        assert_eq!(t.profile().eq.freqs_hz[5], 1000.0);
        // A thousand fine steps up stop at the limit, a thousand down stop
        // at the lowest frequency 48 kHz allows (0.048 Hz), and 688 back up
        // come to 1000 Hz exactly.
        for _ in 0..1000 {
            press(&mut t, Key::ShiftRight);
        }
        assert_eq!(t.profile().eq.freqs_hz[5], 24_000.0);
        for _ in 0..1000 {
            press(&mut t, Key::ShiftLeft);
        }
        assert_eq!(t.profile().eq.freqs_hz[5], 0.0484);
        for _ in 0..688 {
            assert_eq!(press(&mut t, Key::ShiftRight), Effect::Apply);
        }
        assert_eq!(t.profile().eq.freqs_hz[5], 1000.0);

        // Q likewise, from the default 1.5, which is on the fine grid, and
        // from 1.41, which is on the coarse one.
        press(&mut t, Key::Tab);
        for _ in 0..150 {
            press(&mut t, Key::ShiftRight);
        }
        assert_eq!(t.profile().eq.q[5], point(1.0, 178));
        for _ in 0..150 {
            press(&mut t, Key::ShiftLeft);
        }
        assert_eq!(t.profile().eq.q[5], 1.5);
        press(&mut t, Key::Left);
        assert_eq!(t.profile().eq.q[5], 1.41);
        for _ in 0..40 {
            press(&mut t, Key::Left);
        }
        assert_eq!(t.profile().eq.q[5], 0.111);
        for _ in 0..50 {
            press(&mut t, Key::Right);
        }
        assert_eq!(t.profile().eq.q[5], 18.0);
        for _ in 0..22 {
            assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        }
        assert_eq!(t.profile().eq.q[5], 1.41);
    }

    #[test]
    fn frequency_and_q_stop_at_their_limits_with_the_reason() {
        let mut p = Profile::default();
        p.eq.freqs_hz[0] = 0.0484;
        let mut t = Tuner::new(p.clone(), String::new());
        select_field(&mut t, 0, Field::Freq);
        // The lowest frequency is the rate's: fs / 1e6.
        assert_eq!(press(&mut t, Key::ShiftLeft), Effect::None);
        assert_eq!(
            t.message(),
            "refused: eq.freqs_hz[0] = 0.0477 Hz must be at least 0.048 Hz at 48000 Hz sample rate"
        );
        assert_eq!(t.profile(), &p);
        assert_eq!(t.key(Key::ShiftRight, 192_000), Effect::None);
        assert!(
            t.message()
                .starts_with("refused: eq.freqs_hz[0] = 0.0491 Hz must be at least 0.192 Hz"),
            "{}",
            t.message()
        );
        // The next change that goes through clears the refusal.
        assert_eq!(press(&mut t, Key::ShiftRight), Effect::Apply);
        assert_eq!(t.message(), "");

        // The highest is the tuner's 24 kHz, whatever the rate.
        let mut p = Profile::default();
        p.eq.freqs_hz[9] = 23_600.0;
        p.eq.freqs_hz[10] = 30_000.0;
        p.eq.q[1] = 0.101;
        p.eq.q[2] = 19.9;
        p.eq.q[3] = 20.0;
        let mut t = Tuner::new(p, String::new());
        select_field(&mut t, 9, Field::Freq);
        assert_eq!(t.key(Key::ShiftRight, 96_000), Effect::Apply);
        assert_eq!(t.profile().eq.freqs_hz[9], 24_000.0);
        assert_eq!(t.key(Key::ShiftRight, 96_000), Effect::None);
        assert_eq!(
            t.message(),
            "refused: eq.freqs_hz[9] = 24300 Hz is above the tuner's 24000 Hz limit"
        );
        assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        assert_eq!(t.profile().eq.freqs_hz[9], 22_600.0);
        assert_eq!(press(&mut t, Key::Right), Effect::None);
        assert_eq!(
            t.message(),
            "refused: eq.freqs_hz[9] = 25400 Hz is above the tuner's 24000 Hz limit"
        );
        // A band set above it by hand can come down but not go up.
        press(&mut t, Key::Down);
        assert_eq!(press(&mut t, Key::ShiftRight), Effect::None);
        assert_eq!(t.profile().eq.freqs_hz[10], 30_000.0);
        assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        assert_eq!(t.profile().eq.freqs_hz[10], 28_500.0);

        // Q stays within 0.1..=20, the range validation sets.
        select_field(&mut t, 1, Field::Q);
        assert_eq!(press(&mut t, Key::ShiftLeft), Effect::None);
        assert_eq!(t.message(), "refused: eq.q[1] = 0.0992 is outside 0.1..=20");
        assert_eq!(press(&mut t, Key::Down), Effect::None);
        assert_eq!(press(&mut t, Key::ShiftRight), Effect::None);
        assert_eq!(t.message(), "refused: eq.q[2] = 20.2 is outside 0.1..=20");
        assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        assert_eq!(t.profile().eq.q[2], 18.0);
        assert_eq!(t.message(), "");
        press(&mut t, Key::Down);
        assert_eq!(press(&mut t, Key::ShiftLeft), Effect::Apply);
        assert_eq!(t.profile().eq.q[3], 19.9);
    }

    #[test]
    fn bands_the_chain_skips_are_marked_and_said() {
        // At 44.1 kHz Nyquist is 22050 Hz: the top band, moved up from
        // 22000 Hz, is skipped at 22300 Hz.
        let mut p = Profile::default();
        p.eq.freqs_hz[10] = 22_000.0;
        let mut t = Tuner::new(p, String::new());
        select_field(&mut t, 10, Field::Freq);
        assert_eq!(marked_skipped(&t, 44_100), []);
        assert_eq!(t.key(Key::ShiftRight, 44_100), Effect::Apply);
        assert_eq!(t.profile().eq.freqs_hz[10], 22_300.0);
        assert_eq!(marked_skipped(&t, 44_100), [10]);
        let said = "this band is skipped at 44100 Hz: a band's centre and bandwidth (freq / Q) \
                    must be below 22050 Hz";
        assert_eq!(t.message(), said);
        // It can go further, and a change to another field says it again.
        assert_eq!(t.key(Key::Right, 44_100), Effect::Apply);
        t.set_message(String::new());
        press(&mut t, Key::BackTab);
        assert_eq!(t.key(Key::Right, 44_100), Effect::Apply);
        assert_eq!(t.profile().eq.gains_db[10], 0.5);
        assert_eq!(t.message(), said);
        // Played again, the notice goes.
        press(&mut t, Key::Tab);
        assert_eq!(t.key(Key::ShiftLeft, 44_100), Effect::Apply);
        assert_eq!(t.message(), said);
        assert_eq!(t.key(Key::ShiftLeft, 44_100), Effect::Apply);
        assert_eq!(t.profile().eq.freqs_hz[10], 22_000.0);
        assert_eq!(marked_skipped(&t, 44_100), []);
        assert_eq!(t.message(), "");

        // A small Q skips a band too: at 48 kHz, 16 kHz with Q 0.667 spans
        // 23988 Hz and plays, with Q 0.658 it spans 24316 Hz.
        let mut p = Profile::default();
        p.eq.q[10] = 0.667;
        let mut t = Tuner::new(p, String::new());
        select_field(&mut t, 10, Field::Q);
        assert_eq!(marked_skipped(&t, RATE), []);
        assert_eq!(press(&mut t, Key::ShiftLeft), Effect::Apply);
        assert_eq!(t.profile().eq.q[10], 0.658);
        assert_eq!(marked_skipped(&t, RATE), [10]);
        assert!(t.message().starts_with("this band is skipped at 48000 Hz"));
        assert_eq!(press(&mut t, Key::ShiftRight), Effect::Apply);
        assert_eq!(marked_skipped(&t, RATE), []);

        // The marks follow the rate the rows are made for: the bands at or
        // above Nyquist, or with freq / Q there, worked out in f64.
        let mut p = Profile::default();
        p.eq.q = [1.5, 0.2, 1.5, 1.5, 1.5, 0.15, 1.5, 0.5, 1.5, 0.9, 1.5];
        let t = Tuner::new(p.clone(), String::new());
        for rate in [
            8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 96_000,
        ] {
            let nyquist = f64::from(rate) / 2.0;
            let want: Vec<usize> = (0..EQ_BANDS)
                .filter(|&i| {
                    let (f, q) = (f64::from(p.eq.freqs_hz[i]), f64::from(p.eq.q[i]));
                    f >= nyquist || f / q >= nyquist
                })
                .collect();
            assert_eq!(marked_skipped(&t, rate), want, "{rate} Hz");
        }
        assert_eq!(marked_skipped(&t, 32_000), [10]);
        assert_eq!(marked_skipped(&t, 16_000), [9, 10]);
        assert_eq!(marked_skipped(&t, 8_000), [5, 7, 8, 9, 10]);
    }

    #[test]
    fn a_step_changes_only_its_own_value_of_its_own_band() {
        for band in 0..EQ_BANDS {
            for field in [Field::Freq, Field::Q, Field::Gain] {
                for (key, up, every) in [(Key::Right, true, 8), (Key::ShiftLeft, false, 1)] {
                    let mut t = tuner();
                    select_field(&mut t, band, field);
                    assert_eq!(press(&mut t, key), Effect::Apply, "{band} {field:?}");
                    let mut want = Profile::default();
                    let eq = &mut want.eq;
                    match field {
                        Field::Freq => {
                            eq.freqs_hz[band] = next_point(1000.0, eq.freqs_hz[band], every, up)
                        }
                        Field::Q => eq.q[band] = next_point(1.0, 1.5, every, up),
                        Field::Gain => eq.gains_db[band] = if up { 0.5 } else { -0.1 },
                    }
                    assert_eq!(t.profile(), &want, "{band} {field:?} {key:?}");
                }
            }
        }
    }

    /// A small deterministic generator, so a failing walk can be replayed.
    struct Walk(u64);

    impl Walk {
        fn next(&mut self, below: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % below
        }
    }

    #[test]
    fn random_walks_over_the_bands_save_and_read_back_exactly() {
        let dir = scratch_dir("walk");
        let path = dir.join("walk.toml");
        let mut walk = Walk(0x2545_f491_4f6c_dd1d);
        let first = position(Setting::Band(0));
        let last = first + EQ_BANDS - 1;
        let mut changes = 0;
        for round in 0..24 {
            let rate = [44_100, 48_000, 96_000][round % 3];
            let mut t = tuner();
            select(&mut t, Setting::Band(walk.next(EQ_BANDS as u64) as usize));
            for _ in 0..400 {
                let key = match walk.next(12) {
                    0 if t.selected > first => Key::Up,
                    1 if t.selected < last => Key::Down,
                    2 => Key::Tab,
                    3 => Key::BackTab,
                    4 | 5 => Key::Left,
                    6 | 7 => Key::Right,
                    8 | 9 => Key::ShiftLeft,
                    _ => Key::ShiftRight,
                };
                let before = t.profile().clone();
                let effect = t.key(key, rate);
                // A chain is posted exactly when something changed.
                match effect {
                    Effect::Apply => assert_ne!(t.profile(), &before, "{key:?}"),
                    Effect::None => assert_eq!(t.profile(), &before, "{key:?}"),
                    other => panic!("{other:?} from {key:?}"),
                }
                changes += usize::from(effect == Effect::Apply);
                let p = t.profile();
                assert_eq!(p.validate(rate), Ok(()));
                for i in 0..EQ_BANDS {
                    let (f, q) = (p.eq.freqs_hz[i], p.eq.q[i]);
                    assert!(f <= MAX_BAND_HZ, "{f}");
                    // Each value is a default or has three figures at most.
                    let three = |v: f32| format!("{v:.2e}").parse::<f32>() == Ok(v);
                    let default = Profile::default().eq;
                    assert!(three(f) || f == default.freqs_hz[i], "{f}");
                    assert!(three(q), "{q}");
                }
            }
            save(&path, t.profile()).unwrap();
            let back: Profile = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(&back, t.profile(), "round {round}");
        }
        assert!(changes > 5000, "{changes}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_reachable_frequency_and_q_is_saved_as_its_three_figures() {
        // Frequencies from the lowest 8 kHz allows up to the tuner's limit,
        // Q over its range, eleven to a profile.
        let freqs: Vec<i32> = (-812..=220).collect();
        let qs: Vec<i32> = (-159..=207).collect();
        assert_eq!(point(1000.0, -812), 0.00808);
        assert_eq!(point(1000.0, 220), MAX_BAND_HZ);
        assert_eq!((point(1.0, -159), point(1.0, 207)), (0.101, 19.9));
        for (anchor, ks, key) in [(1000.0, freqs, "freqs_hz"), (1.0, qs, "q")] {
            for chunk in ks.chunks(EQ_BANDS) {
                let mut p = Profile::default();
                let values = if anchor == 1.0 {
                    &mut p.eq.q
                } else {
                    &mut p.eq.freqs_hz
                };
                for (slot, &k) in values.iter_mut().zip(chunk) {
                    *slot = octave_point(anchor, k);
                }
                let text = toml::to_string(&p).unwrap();
                let back: Profile = toml::from_str(&text).unwrap();
                assert_eq!(back, p);
                // The file holds the decimal itself, such as 56.5, not the
                // f32's binary expansion.
                let line = text
                    .lines()
                    .find(|l| l.starts_with(&format!("{key} = [")))
                    .unwrap();
                let saved: Vec<f64> = line[key.len() + 4..line.len() - 1]
                    .split(", ")
                    .map(|n| n.parse().unwrap())
                    .collect();
                for (&saved, &k) in saved.iter().zip(chunk) {
                    let decimal = format!("{:.2e}", anchor * 2f64.powf(f64::from(k) / 48.0));
                    assert_eq!(saved, decimal.parse::<f64>().unwrap(), "{line}");
                }
            }
        }
    }

    #[test]
    fn frequencies_and_q_show_three_figures_or_all_they_need() {
        for (v, want) in [
            (30.0, "30.0"),
            (56.5, "56.5"),
            (9.44, "9.44"),
            (0.0484, "0.0484"),
            (1000.0, "1000"),
            (16_000.0, "16000"),
            (1327.0, "1327"),
            (0.1, "0.100"),
            (1.5, "1.50"),
            (20.0, "20.0"),
            (0.707, "0.707"),
            (16_000.5, "16000.5"),
            (99.95, "99.95"),
            (std::f32::consts::FRAC_1_SQRT_2, "0.70710677"),
        ] {
            assert_eq!(significant(v), want);
        }
    }

    #[test]
    fn new_settings_step_within_their_range_and_are_refused_past_it() {
        // (setting, its name, lowest, highest, a step past each, writes it)
        type Set = fn(&mut Profile, f32);
        let cases: [(Setting, &str, f32, f32, f32, f32, Set); 16] = [
            (
                Setting::LevelerTarget,
                "leveler.target_lufs",
                -40.0,
                -5.0,
                -40.5,
                -4.5,
                |p, v| p.leveler.target_lufs = v,
            ),
            (
                Setting::LevelerMaxBoost,
                "leveler.max_boost_db",
                0.0,
                24.0,
                -0.5,
                24.5,
                |p, v| p.leveler.max_boost_db = v,
            ),
            (
                Setting::LevelerMaxCut,
                "leveler.max_cut_db",
                0.0,
                24.0,
                -0.5,
                24.5,
                |p, v| p.leveler.max_cut_db = v,
            ),
            (
                Setting::LevelerRise,
                "leveler.rise_db_per_s",
                0.05,
                20.0,
                0.0,
                20.5,
                |p, v| p.leveler.rise_db_per_s = v,
            ),
            (
                Setting::LevelerFall,
                "leveler.fall_db_per_s",
                0.05,
                40.0,
                0.0,
                40.5,
                |p, v| p.leveler.fall_db_per_s = v,
            ),
            (
                Setting::BassCutoff,
                "bass.cutoff_hz",
                40.0,
                250.0,
                35.0,
                255.0,
                |p, v| p.bass.cutoff_hz = v,
            ),
            (
                Setting::BassHarmonics,
                "bass.harmonics_db",
                -24.0,
                12.0,
                -24.5,
                12.5,
                |p, v| p.bass.harmonics_db = v,
            ),
            (
                Setting::ContourReference,
                "contour.reference_db",
                -60.0,
                0.0,
                -60.5,
                0.5,
                |p, v| p.contour.reference_db = v,
            ),
            (
                Setting::ContourBassPerDb,
                "contour.bass_per_db",
                0.0,
                1.0,
                -0.05,
                1.05,
                |p, v| p.contour.bass_per_db = v,
            ),
            (
                Setting::ContourTreblePerDb,
                "contour.treble_per_db",
                0.0,
                1.0,
                -0.05,
                1.05,
                |p, v| p.contour.treble_per_db = v,
            ),
            (
                Setting::ContourMaxBass,
                "contour.max_bass_db",
                0.0,
                18.0,
                -0.5,
                18.5,
                |p, v| p.contour.max_bass_db = v,
            ),
            (
                Setting::ContourMaxTreble,
                "contour.max_treble_db",
                0.0,
                12.0,
                -0.5,
                12.5,
                |p, v| p.contour.max_treble_db = v,
            ),
            (
                Setting::CrossfeedCutoff,
                "crossfeed.cutoff_hz",
                300.0,
                2000.0,
                250.0,
                2050.0,
                |p, v| p.crossfeed.cutoff_hz = v,
            ),
            (
                Setting::CrossfeedLevel,
                "crossfeed.level_db",
                -24.0,
                -1.0,
                -24.5,
                -0.5,
                |p, v| p.crossfeed.level_db = v,
            ),
            (
                Setting::CorrectionPreamp,
                "correction.preamp_db",
                -24.0,
                12.0,
                -24.5,
                12.5,
                |p, v| p.correction.preamp_db = v,
            ),
            (
                Setting::Lookahead,
                "limiter.lookahead_ms",
                0.0,
                10.0,
                -0.5,
                10.5,
                |p, v| p.limiter.lookahead_ms = v,
            ),
        ];
        for (setting, name, lo, hi, below, above, set) in cases {
            let mut t = tuner();
            select(&mut t, setting);
            // Coarse steps up to the top and one past it, then down to the
            // bottom and one past that; every step on the way is valid.
            for (key, end, past) in [(Key::Right, hi, above), (Key::Left, lo, below)] {
                let mut steps = 0;
                while press(&mut t, key) == Effect::Apply {
                    assert_eq!(t.profile().validate(RATE), Ok(()), "{name}");
                    steps += 1;
                    assert!(steps < 1000, "{name} never stops");
                }
                let mut want = Profile::default();
                set(&mut want, end);
                assert_eq!(t.profile(), &want, "{name} {key:?}");
                assert_eq!(
                    t.message(),
                    format!("refused: {name} = {past} is outside {lo}..={hi}"),
                    "{name}"
                );
                // A fine step back goes through.
                let back = if key == Key::Right {
                    Key::ShiftLeft
                } else {
                    Key::ShiftRight
                };
                assert_eq!(press(&mut t, back), Effect::Apply, "{name}");
                assert_eq!(press(&mut t, key), Effect::Apply, "{name}");
            }
        }
    }

    #[test]
    fn leveler_rates_reach_their_minimum_off_the_grid() {
        // 0.05 dB/s is not a multiple of either step: a step down that would
        // pass it stops on it, and from it a step up goes back on the grid.
        let mut t = tuner();
        select(&mut t, Setting::LevelerRise);
        let mut rates = vec![];
        while press(&mut t, Key::ShiftLeft) == Effect::Apply {
            rates.push(t.profile().leveler.rise_db_per_s);
        }
        assert_eq!(rates, [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.05]);
        assert_eq!(selected(&t).1, "0.05 dB/s");
        assert_eq!(
            t.message(),
            "refused: leveler.rise_db_per_s = 0 is outside 0.05..=20"
        );
        assert_eq!(press(&mut t, Key::ShiftRight), Effect::Apply);
        assert_eq!(t.profile().leveler.rise_db_per_s, 0.1);
        assert_eq!(press(&mut t, Key::Left), Effect::Apply);
        assert_eq!(t.profile().leveler.rise_db_per_s, 0.05);
        assert_eq!(press(&mut t, Key::Right), Effect::Apply);
        assert_eq!(t.profile().leveler.rise_db_per_s, 0.5);
        // A hand-written value between the minimum and the grid.
        let mut p = Profile::default();
        p.leveler.fall_db_per_s = 0.07;
        let mut t = Tuner::new(p, String::new());
        select(&mut t, Setting::LevelerFall);
        assert_eq!(press(&mut t, Key::ShiftLeft), Effect::Apply);
        assert_eq!(t.profile().leveler.fall_db_per_s, 0.05);
        assert_eq!(press(&mut t, Key::Left), Effect::None);
    }

    #[test]
    fn new_switches_toggle_with_space_or_left_and_right() {
        type Set = fn(&mut Profile, bool);
        let cases: [(Setting, &str, Set); 7] = [
            (Setting::LevelerEnabled, "off", |p, on| {
                p.leveler.enabled = on
            }),
            (Setting::BassEnabled, "off", |p, on| p.bass.enabled = on),
            (Setting::BassRemoveLows, "off", |p, on| {
                p.bass.remove_lows = on
            }),
            (Setting::ContourEnabled, "off", |p, on| {
                p.contour.enabled = on
            }),
            (Setting::CrossfeedEnabled, "off", |p, on| {
                p.crossfeed.enabled = on
            }),
            (Setting::CorrectionEnabled, "off", |p, on| {
                p.correction.enabled = on
            }),
            (Setting::TruePeak, "on", |p, on| p.limiter.true_peak = on),
        ];
        for (setting, shown, set) in cases {
            let mut t = tuner();
            select(&mut t, setting);
            assert_eq!(selected(&t).1, shown, "{setting:?}");
            let was = shown == "on";
            let mut flipped = Profile::default();
            set(&mut flipped, !was);
            // Only this switch moves, each way.
            for (key, want) in [
                (Key::Char(' '), &flipped),
                (Key::Left, &Profile::default()),
                (Key::Char('l'), &flipped),
                (Key::ShiftLeft, &Profile::default()),
            ] {
                assert_eq!(press(&mut t, key), Effect::Apply, "{setting:?} {key:?}");
                assert_eq!(t.profile(), want, "{setting:?} {key:?}");
            }
            assert!(!t.dirty());
            press(&mut t, Key::Char(' '));
            assert_eq!(selected(&t).1, if was { "off" } else { "on" });
        }
    }

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dps-tuner-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn saved_toml_reads_back_as_the_same_profile() {
        let mut t = tuner();
        select(&mut t, Setting::InputGain);
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
            text.contains(
                "[limiter]\nenabled = false\nceiling_db = -1.1\nrelease_ms = 110.0\n\
                 lookahead_ms = 1.5\ntrue_peak = true\n"
            ),
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
