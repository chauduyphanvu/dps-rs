//! The keyboard tuner's screen: raw mode, the alternate screen, drawing only
//! the lines that changed, and putting the terminal back on every way out,
//! a panic included.

use std::collections::VecDeque;
use std::ffi::{c_int, c_short, c_uint, c_ulong, c_void};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsFd, AsRawFd};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, Once, OnceLock, PoisonError};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, Clear, ClearType, DisableLineWrap, EndSynchronizedUpdate,
    EnterAlternateScreen,
};
use crossterm::{execute, queue};

use dps::response::response_db;

use super::mix::Readings;
use super::tuner::{Field, Key, Row, SLOTS, Tuner};
use super::{SIG_ERR, signal};

/// Set while the terminal is in the tuner's state; cleared once it is back.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The terminal's settings from before raw mode. A signal handler puts them
/// back, so they are stored before they are needed; there is one screen per
/// process.
static SAVED: OnceLock<Termios> = OnceLock::new();
/// The thread that draws the screen.
static OWNER: OnceLock<ThreadId> = OnceLock::new();
/// Panics on other threads while the screen is up, printed once it is gone.
static DEFERRED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Set by SIGWINCH: the terminal changed size.
static RESIZED: AtomicBool = AtomicBool::new(false);

const SIGWINCH: c_int = 28;
const POLLIN: c_short = 0x1;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

/// `struct termios` from macOS's <sys/termios.h>.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct Termios {
    iflag: c_ulong,
    oflag: c_ulong,
    cflag: c_ulong,
    lflag: c_ulong,
    cc: [u8; 20],
    ispeed: c_ulong,
    ospeed: c_ulong,
}

const TCIFLUSH: c_int = 1;
const TCSANOW: c_int = 0;
const STDIN: c_int = 0;
const STDERR: c_int = 2;

unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: c_uint, timeout_ms: c_int) -> c_int;
    fn tcflush(fd: c_int, action: c_int) -> c_int;
    fn tcgetattr(fd: c_int, termios: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, termios: *const Termios) -> c_int;
    fn cfmakeraw(termios: *mut Termios);
    fn write(fd: c_int, bytes: *const c_void, count: usize) -> isize;
}

/// What undoes `Screen::enter`'s escape sequences: ends a synchronized
/// update a draw may have left open, resets attributes, turns line wrap
/// back on, shows the cursor and leaves the alternate screen.
const RESTORE: &[u8] = b"\x1b[?2026l\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l";

/// How long to wait for the rest of a key's escape sequence after a read
/// ends partway through one.
const SEQUENCE_WAIT: Duration = Duration::from_millis(50);

const LEGEND: [&str; 3] = [
    "j/k Up/Down select   h/l Left/Right change   H/L or Shift: fine   Tab EQ field",
    "Space on/off   b bypass (level matched)   g graph   s save   r revert   q quit",
    "a A/B (* unsaved)   c copy to other   p next preset   m match levels   x blind",
];
/// As many lines as `LEGEND`, so a notice lands on the message line in a
/// blind test too.
const BLIND_LEGEND: [&str; 3] = [
    "a or Tab switch between X and Y   1 prefer X   2 prefer Y",
    "x or Esc end the test and show the tally   q quit",
    "",
];
const MAX_MESSAGE_LINES: usize = 6;

/// Loudness below this shows as "--": BS.1770's absolute gate.
const QUIET_LUFS: f32 = -70.0;
/// How long a gain reduction's peak stays on screen.
const HOLD: Duration = Duration::from_secs(1);
/// A reduction's bar: this many cells of this many dB each.
const BAR_CELLS: usize = 8;
const BAR_CELL_DB: f32 = 1.5;

/// Rows of the response curve, with a row for its frequency scale under it.
const GRAPH_ROWS: usize = 7;
/// The narrowest curve drawn, in columns.
const MIN_GRAPH_COLUMNS: usize = 20;
/// The settings list keeps at least this many rows; with fewer the curve
/// gives way.
const MIN_LIST_ROWS: usize = 5;
/// The curve spans 20 Hz to 20 kHz, in the smallest of these ranges, ± dB,
/// that holds it.
const GRAPH_HZ: (f64, f64) = (20.0, 20_000.0);
const GRAPH_RANGES_DB: [f64; 3] = [6.0, 12.0, 24.0];
/// The frequencies the scale names.
const GRAPH_TICKS: [(f64, &str); 5] = [
    (20.0, "20"),
    (100.0, "100"),
    (1000.0, "1k"),
    (10_000.0, "10k"),
    (20_000.0, "20k"),
];

/// What the screen shows.
#[derive(Clone, Copy)]
pub struct View<'a> {
    pub tuner: &'a Tuner,
    pub name: &'a str,
    pub device: &'a str,
    pub rate: u32,
    pub clamped: u64,
    pub dry_gain: f32,
    /// The system volume and what the loudness contour does at it.
    pub contour: Option<&'a str>,
    pub meters: Readings,
    /// The gain level matching puts on A and on B, in dB.
    pub trim: [f32; 2],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Style {
    Plain,
    Bold,
    Reverse,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub text: String,
    pub style: Style,
}

/// The screen as exactly `height` lines of fewer than `width` characters,
/// moving `top`, the first list row shown, to keep the selection in view.
pub fn layout(view: &View, width: u16, height: u16, top: &mut usize) -> Vec<Line> {
    // The last column stays empty: writing there can wrap and scroll.
    let width = usize::from(width).saturating_sub(1).max(1);
    let height = usize::from(height);
    let line = |text: String, style| Line {
        text: printable(&text).chars().take(width).collect(),
        style,
    };
    let tuner = view.tuner;

    // The question, when there is one, is always shown, under the message.
    let prompt = tuner
        .prompt()
        .map(|p| wrap(&p, width, MAX_MESSAGE_LINES))
        .unwrap_or_default();
    let mut message: Vec<(String, Style)> =
        wrap(tuner.message(), width, MAX_MESSAGE_LINES - prompt.len())
            .into_iter()
            .map(|m| (m, Style::Plain))
            .chain(prompt.into_iter().map(|p| (p, Style::Bold)))
            .collect();
    if message.is_empty() {
        message.push((String::new(), Style::Plain));
    }

    let (mut lines, legend) = match tuner.trial() {
        // Nothing that would tell the slots apart: no settings, meters,
        // curve, slot names or matching gain.
        Some(trial) => (
            vec![
                line(
                    format!("dps live  BLIND  playing {}", trial.playing),
                    Style::Bold,
                ),
                line(format!("trial {}", trial.number), Style::Plain),
            ],
            BLIND_LEGEND,
        ),
        None => {
            let contour = view.contour.map(|c| format!(", {c}")).unwrap_or_default();
            let output = format!("{} at {} Hz{contour}   ", view.device, view.rate);
            let room = width.saturating_sub(output.chars().count());
            let mut lines = vec![
                line(status(view), Style::Bold),
                line(output + &tail(view.name, room), Style::Plain),
            ];
            let leveler = tuner.profile().leveler.enabled;
            for meters in meter_lines(&view.meters, leveler, width) {
                lines.push(line(meters, Style::Plain));
            }
            // The list's heading, the rule under it, the message and the
            // legend.
            let mut fixed = lines.len() + 2 + message.len() + LEGEND.len();
            if tuner.graph() && height >= fixed + GRAPH_ROWS + 1 + MIN_LIST_ROWS {
                let marker = tuner
                    .selected_band()
                    .map(|i| f64::from(tuner.profile().eq.freqs_hz[i]));
                let graph = graph_lines(
                    |f| response_db(tuner.profile(), f, view.rate),
                    marker,
                    width,
                );
                fixed += graph.len();
                lines.extend(graph.into_iter().map(|g| line(g, Style::Plain)));
            }
            let (at, count) = tuner.position();
            lines.push(line(
                format!("{:-<width$}", format!("-- {}/{count} ", at + 1)),
                Style::Plain,
            ));
            let list = height.saturating_sub(fixed).max(1);
            let rows = tuner.rows(view.rate);
            scroll(&rows, list, top);
            for i in *top..*top + list {
                lines.push(match rows.get(i) {
                    Some(row) => {
                        let (text, style) = list_line(row);
                        line(text, style)
                    }
                    None => line(String::new(), Style::Plain),
                });
            }
            (lines, LEGEND)
        }
    };
    let blank = Line {
        text: String::new(),
        style: Style::Plain,
    };
    let body = height.saturating_sub(1 + message.len() + legend.len());
    if lines.len() < body {
        lines.resize(body, blank.clone());
    }
    lines.push(line("-".repeat(width), Style::Plain));
    lines.extend(message.into_iter().map(|(m, style)| line(m, style)));
    lines.extend(legend.iter().map(|l| line((*l).into(), Style::Plain)));
    lines.resize(height, blank);
    lines
}

/// The top line: the slots, the heard one in brackets and each marked `*`
/// while it differs from what was saved, whether bypass is on, the matching
/// gain while the slots differ, and what else is worth knowing.
fn status(view: &View) -> String {
    let tuner = view.tuner;
    let slots: Vec<String> = SLOTS
        .iter()
        .enumerate()
        .map(|(s, name)| {
            let star = if tuner.differs(s) { "*" } else { "" };
            if s == tuner.slot() {
                format!("[{name}{star}]")
            } else {
                format!("{name}{star}")
            }
        })
        .collect();
    let mut status = format!("dps live  {}  ", slots.join(" "));
    if tuner.bypass() {
        status += &format!("BYPASS (dry {:+.1} dB)", 20.0 * view.dry_gain.log10());
    } else {
        status += "PROCESSED";
    }
    if tuner.compared() {
        status += &if !tuner.matching() {
            "  match off".into()
        } else {
            match (0..2).find(|&s| view.trim[s] <= -0.05) {
                Some(s) => format!("  trim {} {:.1} dB", SLOTS[s], view.trim[s]),
                None => "  trim 0.0 dB".into(),
            }
        };
    }
    if view.clamped > 0 {
        status += &format!("  clamped {}", view.clamped);
    }
    match tuner.profile().skipped_eq_bands(view.rate).len() {
        0 => {}
        1 => status += "  1 EQ band skipped",
        n => status += &format!("  {n} EQ bands skipped"),
    }
    status
}

/// One row of the settings list, and how to show it.
fn list_line(row: &Row) -> (String, Style) {
    let (marker, style) = if row.selected() {
        ('>', Style::Reverse)
    } else {
        (' ', Style::Plain)
    };
    match row {
        Row::Group(name) => (format!(" {name}"), Style::Bold),
        Row::Setting { label, value, .. } => (format!("{marker}   {label:<20}{value:>12} "), style),
        Row::Band {
            freq,
            q,
            gain,
            active,
            skipped,
            ..
        } => {
            // Every field keeps a column on each side for the brackets, so
            // the columns stay put as the active field moves, and the gain
            // lines up with the other settings' values.
            let cell = |field, text: &str| {
                if *active == Some(field) {
                    format!("[{text}]")
                } else {
                    format!(" {text} ")
                }
            };
            let (freq, q, gain) = (
                cell(Field::Freq, freq),
                cell(Field::Q, q),
                cell(Field::Gain, gain),
            );
            let skipped = if *skipped { " skipped" } else { "" };
            (
                format!("{marker} {freq:>12}{q:<10}{gain:>13}{skipped}"),
                style,
            )
        }
    }
}

/// The curve of `response` (dB at a frequency) from 20 Hz to 20 kHz on a
/// log axis, in braille dots two across and four down a character, with
/// the range on the left, a dashed 0 dB line, a dotted line up at `marker`
/// Hz, and a frequency scale under it; nothing if `width` is too narrow.
fn graph_lines(response: impl Fn(f64) -> f64, marker: Option<f64>, width: usize) -> Vec<String> {
    const LABEL: usize = 4;
    let Some(columns) = width.checked_sub(LABEL).filter(|&c| c >= MIN_GRAPH_COLUMNS) else {
        return Vec::new();
    };
    let (low, high) = GRAPH_HZ;
    let xs = 2 * columns;
    let x_of = |f: f64| ((xs - 1) as f64 * (f / low).ln() / (high / low).ln()).round();
    let dbs: Vec<f64> = (0..xs)
        .map(|x| response(low * (high / low).powf(x as f64 / (xs - 1) as f64)))
        .collect();
    let peak = dbs.iter().fold(0.0f64, |m, d| m.max(d.abs()));
    let range = GRAPH_RANGES_DB
        .into_iter()
        .find(|&r| peak <= r + 0.05)
        .unwrap_or(GRAPH_RANGES_DB[2]);
    // One dot row fewer than the cells hold, so 0 dB has a row of its own,
    // in the middle.
    let last = 4 * GRAPH_ROWS - 2;
    let zero = last / 2;
    let y_of = |db: f64| {
        ((range - db) / (2.0 * range) * last as f64)
            .round()
            .clamp(0.0, last as f64) as usize
    };
    let mut dots = vec![[false; 2 * 4 * GRAPH_ROWS]; columns];
    let mut set = |x: usize, y: usize| dots[x / 2][2 * y + x % 2] = true;
    for x in (0..xs).step_by(2) {
        set(x, zero);
    }
    if let Some(f) = marker.filter(|f| (low..=high).contains(f)) {
        let x = x_of(f) as usize;
        for y in (0..=last).step_by(2) {
            set(x, y);
        }
    }
    let mut previous = y_of(dbs[0]);
    for (x, &db) in dbs.iter().enumerate() {
        // Down to the dot beside the last one, so a steep stretch is one line.
        let y = y_of(db);
        let span = if previous < y {
            previous + 1..=y
        } else {
            y..=previous.saturating_sub(1).max(y)
        };
        for y in span {
            set(x, y);
        }
        previous = y;
    }
    // Braille dot bits by row within a character, left column then right.
    const BITS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    let label = |row: usize| {
        let rows = |y: usize| y / 4;
        let db = if row == rows(0) {
            range
        } else if row == rows(zero) {
            0.0
        } else if row == rows(last) {
            -range
        } else {
            return " ".repeat(LABEL);
        };
        format!(
            "{:>3} ",
            if db == 0.0 {
                "0".into()
            } else {
                format!("{db:+}")
            }
        )
    };
    let mut lines: Vec<String> = (0..GRAPH_ROWS)
        .map(|row| {
            let cells = dots.iter().map(|cell| {
                let mut bits = 0;
                for (r, pair) in BITS.iter().enumerate() {
                    for (c, bit) in pair.iter().enumerate() {
                        if cell[2 * (4 * row + r) + c] {
                            bits |= bit;
                        }
                    }
                }
                if bits == 0 {
                    ' '
                } else {
                    char::from_u32(0x2800 + bits).expect("braille patterns are chars")
                }
            });
            label(row) + &cells.collect::<String>()
        })
        .collect();
    let mut scale = vec![' '; columns];
    let mut free = 0;
    for (f, name) in GRAPH_TICKS {
        let len = name.len();
        let centre = x_of(f) as usize / 2;
        let start = centre.saturating_sub(len / 2).min(columns - len);
        if start < free {
            continue;
        }
        scale[start..start + len]
            .iter_mut()
            .zip(name.chars())
            .for_each(|(s, c)| *s = c);
        free = start + len + 1;
    }
    lines.push(" ".repeat(LABEL) + scale.iter().collect::<String>().trim_end());
    lines
}

/// The meters in one line if it fits `width`, else two: the loudness and
/// the leveler, then the gain reductions, with bars if they fit.
fn meter_lines(meters: &Readings, leveler: bool, width: usize) -> Vec<String> {
    let lufs = |v: f32| {
        if v > QUIET_LUFS {
            format!("{v:.1}")
        } else {
            "--".into()
        }
    };
    let loudness = format!(
        "in {:>5}  out {:>5} LUFS",
        lufs(meters.input_lufs),
        lufs(meters.output_lufs)
    );
    let gain = if leveler {
        // Rounded first, and + 0.0, so a gain just under 0 shows as +0.0.
        format!(
            "{:+.1}",
            (meters.chain.leveler_db * 10.0).round() / 10.0 + 0.0
        )
    } else {
        "off".into()
    };
    let unit = if leveler { " dB" } else { "" };
    let c = &meters.chain;
    let reductions = [
        ("c1", c.compressor1_db),
        ("c2", c.compressor2_db),
        ("lim", c.limiter_db),
    ];
    let barred = reductions
        .map(|(name, db)| format!("{name:<3} {:>5} {}", reduction(db), bar(db)))
        .join("   ")
        + " dB";
    let one = format!("{loudness}   leveler {gain}{unit}   {barred}");
    if one.chars().count() <= width {
        return vec![one];
    }
    if barred.chars().count() <= width {
        return vec![format!("{loudness}   leveler {gain}{unit}"), barred];
    }
    let plain = reductions
        .map(|(name, db)| format!("{name} {}", reduction(db)))
        .join("  ")
        + " dB";
    vec![format!("{loudness}  lev {gain}"), plain]
}

/// A gain reduction as the change it makes, e.g. "-4.2".
fn reduction(db: f32) -> String {
    if db >= 0.05 {
        format!("-{db:.1}")
    } else {
        "0.0".into()
    }
}

/// A gain reduction as a bar of `BAR_CELLS` cells, in eighths of a cell.
fn bar(db: f32) -> String {
    const PARTS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (db / BAR_CELL_DB * 8.0)
        .round()
        .clamp(0.0, (8 * BAR_CELLS) as f32) as usize;
    let (full, part) = (eighths / 8, eighths % 8);
    let mut bar = "█".repeat(full);
    if part > 0 {
        bar.push(PARTS[part]);
    }
    let drawn = full + usize::from(part > 0);
    bar + &"·".repeat(BAR_CELLS - drawn)
}

/// Holds each gain reduction's peak on screen for `HOLD`, so it can be read.
#[derive(Default)]
struct Hold {
    /// Each peak and when it was reached.
    peaks: [(f32, Option<Instant>); 3],
}

impl Hold {
    /// `readings` with each reduction replaced by its held peak.
    fn apply(&mut self, mut readings: Readings, now: Instant) -> Readings {
        let c = &mut readings.chain;
        for (value, (peak, since)) in [
            &mut c.compressor1_db,
            &mut c.compressor2_db,
            &mut c.limiter_db,
        ]
        .into_iter()
        .zip(&mut self.peaks)
        {
            let expired = since.is_none_or(|t| now.saturating_duration_since(t) >= HOLD);
            if *value >= *peak || expired {
                (*peak, *since) = (*value, Some(now));
            }
            *value = *peak;
        }
        readings
    }
}

/// Moves `top` so the selected row is among the `height` rows shown, with
/// its group's heading when it is the group's first setting.
fn scroll(rows: &[Row], height: usize, top: &mut usize) {
    let selected = rows.iter().position(Row::selected).unwrap_or(0);
    let heading =
        usize::from(height > 1 && selected > 0 && matches!(rows[selected - 1], Row::Group(_)));
    *top = (*top).min(selected - heading);
    if selected >= *top + height {
        *top = selected + 1 - height;
    }
    *top = (*top).min(rows.len().saturating_sub(height));
}

/// The end of `text` in at most `width` characters, starting with "..." if
/// it was cut, since a path's end names the file.
fn tail(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.into();
    }
    let keep = width.saturating_sub(3);
    "..."
        .chars()
        .take(width)
        .chain(text.chars().skip(count - keep))
        .collect()
}

/// `text` broken at spaces into at most `max` lines of `width` characters;
/// a word longer than a line, such as a path, is broken across lines.
fn wrap(text: &str, width: usize, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let len = current.chars().count();
        if len > 0 && len + 1 + word.chars().count() > width {
            lines.push(mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
        while current.chars().count() > width {
            lines.push(current.chars().take(width).collect());
            current = current.chars().skip(width).collect();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.len() > max {
        lines.truncate(max);
        let Some(last) = lines.last_mut() else {
            return lines;
        };
        *last = last
            .chars()
            .take(width.saturating_sub(3))
            .collect::<String>()
            + "...";
    }
    lines
}

/// `text` with every control character, which the terminal would act on,
/// shown as `?`; tabs and line breaks become spaces.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' => ' ',
            c if c.is_control() => '?',
            c => c,
        })
        .collect()
}

/// Turns terminal input into keys; anything else is skipped. An escape
/// sequence cut off at the end of one read is kept for the next.
#[derive(Default)]
struct Parser {
    pending: Vec<u8>,
}

impl Parser {
    /// The keys `bytes` completes.
    fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let input = mem::take(&mut self.pending);
        let mut keys = Vec::new();
        let mut rest = &input[..];
        while !rest.is_empty() {
            let Some((key, length)) = next_key(rest) else {
                // Longer than any key's sequence: not one.
                if rest.len() <= 32 {
                    self.pending = rest.to_vec();
                }
                break;
            };
            keys.extend(key);
            rest = &rest[length..];
        }
        keys
    }

    /// Whether a sequence is waiting for its end.
    fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// No more input came: an ESC on its own was the Esc key, and the start
    /// of a sequence on its own is dropped.
    fn flush(&mut self) -> Option<Key> {
        let esc = self.pending == [0x1b];
        self.pending.clear();
        esc.then_some(Key::Esc)
    }
}

/// The key at the start of `bytes`, if it is one the tuner knows, and how
/// many bytes it takes; `None` if `bytes` ends partway through it.
fn next_key(bytes: &[u8]) -> Option<(Option<Key>, usize)> {
    Some(match bytes {
        [] | [0x1b] => return None,
        [0x03, ..] => (Some(Key::CtrlC), 1),
        [b'\t', ..] => (Some(Key::Tab), 1),
        // CSI or SS3: parameter bytes, then a final byte in 0x40..=0x7e.
        [0x1b, b'[' | b'O', rest @ ..] => {
            for (n, &byte) in rest.iter().enumerate() {
                match byte {
                    0x20..=0x3f => {}
                    0x40..=0x7e => {
                        let key = match (&rest[..n], byte) {
                            (b"", b'A') => Some(Key::Up),
                            (b"", b'B') => Some(Key::Down),
                            (b"", b'C') => Some(Key::Right),
                            (b"", b'D') => Some(Key::Left),
                            (b"1;2", b'C') => Some(Key::ShiftRight),
                            (b"1;2", b'D') => Some(Key::ShiftLeft),
                            (b"", b'Z') => Some(Key::BackTab),
                            _ => None,
                        };
                        return Some((key, 2 + n + 1));
                    }
                    // A control byte cuts the sequence short and counts on
                    // its own, as a terminal would treat it: Ctrl-C still
                    // quits.
                    _ => return Some((None, 2 + n)),
                }
            }
            return None;
        }
        // Alt and a key arrive as ESC and the key in one read; the tuner
        // has no Alt keys, so both are skipped rather than taken as the key.
        [0x1b, 0x20..=0x7e, ..] => (None, 2),
        [0x1b, ..] => (Some(Key::Esc), 1),
        [byte @ 0x20..=0x7e, ..] => (Some(Key::Char(char::from(*byte))), 1),
        [_, ..] => (None, 1),
    })
}

/// The terminal while the tuner owns it. Dropping it puts the terminal back.
pub struct Screen {
    /// Standard input, read unbuffered so that `poll` sees every byte not
    /// yet taken.
    input: File,
    parser: Parser,
    keys: VecDeque<Key>,
    drawn: Vec<Line>,
    size: (u16, u16),
    top: usize,
    clear: bool,
    hold: Hold,
}

impl Screen {
    pub fn enter() -> io::Result<Self> {
        install_panic_hook();
        let _ = OWNER.set(thread::current().id());
        let input = File::from(io::stdin().as_fd().try_clone_to_owned()?);
        let saved = raw_mode(input.as_raw_fd())?;
        SAVED.get_or_init(|| saved);
        ACTIVE.store(true, Relaxed);
        // From here on, any way out of this function restores the terminal.
        let mut screen = Self {
            input,
            parser: Parser::default(),
            keys: VecDeque::new(),
            drawn: Vec::new(),
            size: (80, 24),
            top: 0,
            clear: true,
            hold: Hold::default(),
        };
        extern "C" fn on_resize(_: c_int) {
            RESIZED.store(true, Relaxed);
        }
        // SAFETY: the handler only stores an atomic.
        if unsafe { signal(SIGWINCH, on_resize as extern "C" fn(c_int) as usize) } == SIG_ERR {
            return Err(io::Error::last_os_error());
        }
        execute!(io::stderr(), EnterAlternateScreen, Hide, DisableLineWrap)?;
        screen.size = usable(terminal::size()?);
        Ok(screen)
    }

    /// The next key pressed within `timeout`, if any. An `UnexpectedEof`
    /// error means the terminal is gone.
    pub fn next_key(&mut self, timeout: Duration) -> io::Result<Option<Key>> {
        if self.keys.is_empty() && self.readable(timeout)? {
            self.read()?;
            while self.parser.waiting() && self.readable(SEQUENCE_WAIT)? {
                self.read()?;
            }
            self.keys.extend(self.parser.flush());
        }
        if RESIZED.swap(false, Relaxed) {
            self.size = usable(terminal::size()?);
            self.clear = true;
        }
        Ok(self.keys.pop_front())
    }

    fn read(&mut self) -> io::Result<()> {
        let mut bytes = [0; 1024];
        match self.input.read(&mut bytes) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the terminal closed",
            )),
            Ok(n) => {
                let keys = self.parser.feed(&bytes[..n]);
                self.keys.extend(keys);
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Waits up to `timeout` for input; false on timeout or a signal.
    fn readable(&self, timeout: Duration) -> io::Result<bool> {
        let mut fd = PollFd {
            fd: self.input.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        let ms = c_int::try_from(timeout.as_millis()).unwrap_or(c_int::MAX);
        // SAFETY: one valid pollfd for the duration of the call.
        match unsafe { poll(&mut fd, 1, ms) } {
            -1 => match io::Error::last_os_error() {
                e if e.kind() == io::ErrorKind::Interrupted => Ok(false),
                e => Err(e),
            },
            ready => Ok(ready > 0),
        }
    }

    /// Makes the next draw repaint everything, for when something else may
    /// have written to the terminal.
    pub fn invalidate(&mut self) {
        self.clear = true;
    }

    pub fn draw(&mut self, view: &View) -> io::Result<()> {
        let view = View {
            meters: self.hold.apply(view.meters, Instant::now()),
            ..*view
        };
        let lines = layout(&view, self.size.0, self.size.1, &mut self.top);
        let mut frame = Vec::new();
        queue!(frame, BeginSynchronizedUpdate)?;
        let empty = frame.len();
        if self.clear {
            queue!(frame, Clear(ClearType::All))?;
            self.drawn.clear();
            self.clear = false;
        }
        for (row, line) in lines.iter().enumerate() {
            if self.drawn.get(row) == Some(line) {
                continue;
            }
            queue!(frame, MoveTo(0, row as u16))?;
            match line.style {
                Style::Plain => {}
                Style::Bold => queue!(frame, SetAttribute(Attribute::Bold))?,
                Style::Reverse => queue!(frame, SetAttribute(Attribute::Reverse))?,
            }
            queue!(
                frame,
                Print(&line.text),
                SetAttribute(Attribute::Reset),
                Clear(ClearType::UntilNewLine)
            )?;
        }
        self.drawn = lines;
        if frame.len() == empty {
            return Ok(());
        }
        queue!(frame, EndSynchronizedUpdate)?;
        let mut err = io::stderr().lock();
        err.write_all(&frame)?;
        err.flush()
    }

    /// Bytes that show `message` on the line above the legend, for another
    /// thread to write while this one is busy.
    pub fn notice(&self, message: &str) -> Vec<u8> {
        let (width, height) = self.size;
        let text: String = printable(message)
            .chars()
            .take(usize::from(width).saturating_sub(1))
            .collect();
        let mut bytes = Vec::new();
        let row = height.saturating_sub(LEGEND.len() as u16 + 1);
        // Writing to a Vec cannot fail.
        let _ = queue!(
            bytes,
            MoveTo(0, row),
            Clear(ClearType::CurrentLine),
            Print(text)
        );
        bytes
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

/// The size a terminal reports, or 80 by 24 if it reports none.
fn usable((width, height): (u16, u16)) -> (u16, u16) {
    if width == 0 || height == 0 {
        (80, 24)
    } else {
        (width, height)
    }
}

/// Puts the terminal on `fd` in raw mode; returns its settings from before.
fn raw_mode(fd: c_int) -> io::Result<Termios> {
    let mut saved = MaybeUninit::<Termios>::uninit();
    // SAFETY: tcgetattr fills the whole struct when it returns 0.
    let saved = unsafe {
        if tcgetattr(fd, saved.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        saved.assume_init()
    };
    let mut raw = saved;
    // SAFETY: plain calls on a valid termios.
    unsafe {
        cfmakeraw(&mut raw);
        if tcsetattr(fd, TCSANOW, &raw) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(saved)
}

/// Puts the terminal back to `saved` through `write` and `set_mode`, which
/// write bytes to it and set its mode.
fn put_back(saved: &Termios, write: &mut impl FnMut(&[u8]), set_mode: &mut impl FnMut(&Termios)) {
    write(RESTORE);
    set_mode(saved);
}

/// Writes to standard error and sets standard input's mode, with calls that
/// are safe in a signal handler.
fn put_back_now(saved: &Termios) {
    put_back(
        saved,
        // SAFETY: write() and tcsetattr() on valid buffers.
        &mut |bytes| unsafe {
            write(STDERR, bytes.as_ptr().cast(), bytes.len());
        },
        &mut |mode| unsafe {
            tcsetattr(STDIN, TCSANOW, mode);
        },
    );
}

fn restore() {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    if let Some(saved) = SAVED.get() {
        put_back_now(saved);
    }
    // Cleared only now, so a signal handler that runs meanwhile puts the
    // terminal back itself rather than leaving it half done.
    ACTIVE.store(false, Relaxed);
    // Keys typed for the tuner but not read, say while quitting, would
    // otherwise go to the shell.
    // SAFETY: plain calls on stdin; SIG_DFL installs no code.
    unsafe {
        tcflush(io::stdin().as_raw_fd(), TCIFLUSH);
        signal(SIGWINCH, 0);
    }
    let deferred = mem::take(&mut *DEFERRED.lock().unwrap_or_else(PoisonError::into_inner));
    for message in deferred {
        let _ = writeln!(io::stderr(), "dps live: another thread {message}");
    }
}

/// Puts the terminal back from a signal handler, before the process ends:
/// only write() and tcsetattr(), nothing that locks or allocates.
pub fn restore_in_signal_handler() {
    if ACTIVE.load(Relaxed)
        && let Some(saved) = SAVED.get()
    {
        put_back_now(saved);
    }
}

/// A new pseudo-terminal: its controlling side, then its terminal side.
#[cfg(test)]
pub fn pty() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
    unsafe extern "C" {
        fn openpty(
            master: *mut c_int,
            slave: *mut c_int,
            name: *mut std::ffi::c_char,
            termios: *const Termios,
            size: *const c_void,
        ) -> c_int;
    }
    let (mut master, mut slave) = (0, 0);
    let (no_name, no_mode, no_size) = (std::ptr::null_mut(), std::ptr::null(), std::ptr::null());
    // SAFETY: openpty fills both descriptors, which are then owned here.
    unsafe {
        let opened = openpty(&mut master, &mut slave, no_name, no_mode, no_size);
        assert_eq!(opened, 0, "{}", io::Error::last_os_error());
        (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
    }
}

/// The settings of the terminal on `fd`.
#[cfg(test)]
fn mode(fd: c_int) -> Termios {
    let mut t = MaybeUninit::<Termios>::uninit();
    // SAFETY: tcgetattr fills the whole struct when it returns 0.
    unsafe {
        assert_eq!(tcgetattr(fd, t.as_mut_ptr()), 0);
        t.assume_init()
    }
}

/// Whether the terminal on `fd` is in canonical mode, as raw mode is not.
#[cfg(test)]
pub fn canonical(fd: c_int) -> bool {
    const ICANON: c_ulong = 0x100;
    mode(fd).lflag & ICANON != 0
}

/// Puts the terminal back before a panic on the drawing thread is reported,
/// so the report is readable and stays on screen.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if ACTIVE.load(Relaxed) {
                if OWNER.get() != Some(&thread::current().id()) {
                    // Printed now, it would land on the tuner's screen and
                    // vanish with it.
                    if let Ok(mut deferred) = DEFERRED.try_lock() {
                        deferred.push(info.to_string());
                    }
                    return;
                }
                restore();
            }
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::tuner::Action;
    use dps::Profile;

    fn view(tuner: &Tuner) -> View<'_> {
        View {
            tuner,
            name: "presets/music.toml",
            device: "MacBook Pro Speakers",
            rate: 48_000,
            clamped: 0,
            dry_gain: 1.0,
            contour: None,
            meters: Readings::default(),
            trim: [0.0; 2],
        }
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    fn selected(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .filter(|l| l.style == Style::Reverse)
            .map(|l| l.text.as_str())
            .collect()
    }

    #[test]
    fn the_whole_view_fits_80_by_24() {
        let tuner = Tuner::new(Profile::default(), "music.toml".into());
        let mut top = 0;
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        let text = texts(&lines);
        assert_eq!(lines.len(), 24);
        assert!(text.iter().all(|t| t.chars().count() <= 79), "{text:#?}");
        assert_eq!(text[0], "dps live  [A] B  PROCESSED");
        assert_eq!(
            text[1],
            "MacBook Pro Speakers at 48000 Hz   presets/music.toml"
        );
        assert_eq!(text[2], "in    --  out    -- LUFS   leveler off");
        assert_eq!(
            text[3],
            "c1    0.0 ········   c2    0.0 ········   lim   0.0 ········ dB"
        );
        assert!(text[4].starts_with("-- 1/57 ---"));
        assert_eq!(text[5], " Leveler");
        assert_eq!(selected(&lines), [">   enabled                      off "]);
        assert_eq!(&text[24 - LEGEND.len()..], LEGEND);
        // The selection is marked in the text, not only by the highlight.
        assert!(text[6].starts_with('>'));
        assert!(text[7].starts_with(' '));
    }

    fn readings() -> Readings {
        Readings {
            input_lufs: -18.24,
            output_lufs: -14.06,
            chain: dps::Meters {
                leveler_db: 3.52,
                compressor1_db: 4.2,
                compressor2_db: 1.0,
                limiter_db: 0.3,
            },
        }
    }

    /// The meter lines laid out at `width` with the leveler on.
    fn meters_at(meters: Readings, width: u16) -> Vec<String> {
        let mut profile = Profile::default();
        profile.leveler.enabled = true;
        let tuner = Tuner::new(profile, String::new());
        let mut top = 0;
        let view = View {
            meters,
            ..view(&tuner)
        };
        let lines = layout(&view, width, 30, &mut top);
        let rule = lines.iter().position(|l| l.text.starts_with("--")).unwrap();
        lines[2..rule].iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn meters_show_under_the_status_lines_in_two_lines_at_80_columns() {
        assert_eq!(
            meters_at(readings(), 80),
            [
                "in -18.2  out -14.1 LUFS   leveler +3.5 dB",
                "c1   -4.2 ██▊·····   c2   -1.0 ▋·······   lim  -0.3 ▎······· dB",
            ]
        );
        // Wide enough, one line.
        let wide = meters_at(readings(), 120);
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0], meters_at(readings(), 80).join("   "));
        // Silence, no reduction, the leveler off.
        let tuner = Tuner::new(Profile::default(), String::new());
        let mut top = 0;
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        assert_eq!(
            texts(&lines)[2..4],
            [
                "in    --  out    -- LUFS   leveler off",
                "c1    0.0 ········   c2    0.0 ········   lim   0.0 ········ dB",
            ]
        );
    }

    #[test]
    fn meters_drop_their_bars_at_40_columns_and_are_cut_below() {
        assert_eq!(
            meters_at(readings(), 40),
            [
                "in -18.2  out -14.1 LUFS  lev +3.5",
                "c1 -4.2  c2 -1.0  lim -0.3 dB"
            ]
        );
        let narrow = meters_at(readings(), 20);
        assert_eq!(narrow, ["in -18.2  out -14.1", "c1 -4.2  c2 -1.0  l"]);
        // Bars fill an eighth of a cell per 0.1875 dB, up to 12 dB.
        assert_eq!(bar(0.0), "········");
        assert_eq!(bar(0.05), "········");
        assert_eq!(bar(0.1), "▏·······");
        assert_eq!(bar(0.3), "▎·······");
        assert_eq!(bar(1.5), "█·······");
        assert_eq!(bar(12.0), "████████");
        assert_eq!(bar(30.0), "████████");
        assert_eq!(reduction(0.04), "0.0");
        assert_eq!(reduction(12.34), "-12.3");
    }

    #[test]
    fn a_reductions_peak_holds_for_a_second() {
        let mut hold = Hold::default();
        let start = Instant::now();
        let mut at = |c1: f32, lim: f32, ms: u64| {
            let mut r = Readings::default();
            (r.chain.compressor1_db, r.chain.limiter_db) = (c1, lim);
            let shown = hold.apply(r, start + Duration::from_millis(ms));
            (shown.chain.compressor1_db, shown.chain.limiter_db)
        };
        assert_eq!(at(6.0, 0.0, 0), (6.0, 0.0));
        assert_eq!(at(1.0, 0.5, 500), (6.0, 0.5));
        assert_eq!(at(2.0, 0.1, 999), (6.0, 0.5));
        // A second after its peak, each shows what it reads now.
        assert_eq!(at(2.0, 0.1, 1000), (2.0, 0.5));
        assert_eq!(at(0.0, 0.0, 1500), (2.0, 0.0));
        // A higher peak shows at once.
        assert_eq!(at(7.0, 3.0, 1600), (7.0, 3.0));
    }

    /// The braille dots set in character `column` of `line`, as a bit mask.
    fn dots(line: &str, column: usize) -> u32 {
        match line.chars().nth(column) {
            Some(c @ '\u{2800}'..='\u{28ff}') => c as u32 - 0x2800,
            _ => 0,
        }
    }

    /// A tuner on the default profile with one band changed, the curve on.
    fn graphed(band: usize, freq: f32, gain: f32) -> Tuner {
        let mut p = Profile::default();
        p.eq.freqs_hz[band] = freq;
        p.eq.gains_db[band] = gain;
        let mut tuner = Tuner::new(p, String::new());
        tuner.apply(Action::Graph, 48_000);
        tuner
    }

    /// The laid-out lines from the meters to the list's heading.
    fn graph_of(tuner: &Tuner, width: u16, height: u16) -> Vec<String> {
        let mut top = 0;
        let lines = layout(&view(tuner), width, height, &mut top);
        let heading = lines
            .iter()
            .position(|l| l.text.starts_with("-- "))
            .unwrap();
        lines[4..heading].iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn one_band_up_12_db_peaks_in_its_column_on_the_top_row() {
        // 80 columns leave 75 for the curve, 150 dots across: 1 kHz is dot
        // 149 log(1000 / 20) / log(1000) = 84.4 from the left, in the 43rd
        // character after the 4 of the labels.
        let tuner = graphed(6, 1000.0, 12.0);
        let graph = graph_of(&tuner, 80, 24);
        assert_eq!(graph.len(), 8, "{graph:#?}");
        assert!(graph[0].starts_with("+12 "), "{graph:#?}");
        assert!(graph[3].starts_with("  0 "), "{graph:#?}");
        assert!(graph[6].starts_with("-12 "), "{graph:#?}");
        // The top row of dots (bits 0x01 and 0x08) is set only around 1 kHz.
        let top: Vec<usize> = (0..80)
            .filter(|&c| dots(&graph[0], c) & 0x09 != 0)
            .collect();
        assert!(
            !top.is_empty() && top.iter().all(|&c| c.abs_diff(46) <= 1),
            "{top:?}"
        );
        // Away from the band the curve is on the 0 dB row, and nothing is
        // drawn below it: a boost never dips.
        assert_eq!(dots(&graph[3], 6), 0x12, "{}", graph[3]);
        for row in &graph[4..7] {
            assert!(row[4..].trim().is_empty(), "{graph:#?}");
        }
        assert_eq!(
            graph[7],
            "    20              100                      1k                       10k   20k"
        );
        // Every character the curve uses gets through to the terminal.
        for row in &graph {
            assert_eq!(&printable(row), row);
        }
    }

    #[test]
    fn the_curve_picks_the_smallest_range_that_holds_it() {
        for (gain, range) in [(5.0, " +6"), (6.0, " +6"), (12.0, "+12"), (-18.0, "+24")] {
            let graph = graph_of(&graphed(5, 705.0, gain), 80, 30);
            assert!(graph[0].starts_with(range), "{gain} dB: {graph:#?}");
        }
        // Past 24 dB the curve is cut at the edge: two bands at +24 dB
        // overlap to more than that.
        let mut p = Profile::default();
        p.eq.gains_db[5] = 24.0;
        p.eq.gains_db[6] = 24.0;
        let mut tuner = Tuner::new(p, String::new());
        tuner.apply(Action::Graph, 48_000);
        let graph = graph_of(&tuner, 80, 30);
        assert!(graph[0].starts_with("+24"));
        assert!(graph[0][4..].chars().any(|c| c != ' '));
    }

    #[test]
    fn the_selected_band_is_marked_and_the_curve_follows_changes() {
        // On band 7 (2497 Hz), a dotted line goes down the curve's column
        // for it: 149 log(2497 / 20) / log(1000) = 104.2 dots, the 53rd
        // character after the labels, in rows the curve leaves empty.
        let mut tuner = Tuner::new(Profile::default(), String::new());
        tuner.apply(Action::Graph, 48_000);
        let unmarked = graph_of(&tuner, 80, 24);
        for _ in 0..19 + 7 {
            tuner.apply(Action::Down, 48_000);
        }
        let marked = graph_of(&tuner, 80, 24);
        for row in [0, 1, 5, 6] {
            assert_eq!(dots(&marked[row], 4 + 52), 0x05, "{marked:#?}");
            assert_eq!(dots(&unmarked[row], 4 + 52), 0, "{unmarked:#?}");
        }
        // Turning the band up changes the curve with the next draw.
        let up = Action::Step {
            up: true,
            fine: false,
        };
        for _ in 0..12 {
            tuner.apply(up, 48_000);
        }
        let boosted = graph_of(&tuner, 80, 24);
        assert_ne!(boosted, marked);
        assert!(boosted[0].starts_with(" +6"), "{boosted:#?}");
        assert!(dots(&boosted[0], 4 + 52) & 0x09 != 0, "{boosted:#?}");
    }

    #[test]
    fn the_curve_gives_way_to_the_list_on_a_short_or_narrow_terminal() {
        let tuner = graphed(6, 1000.0, 12.0);
        // At 80 by 24 the list keeps 6 rows; at 23 it keeps the 5 it needs.
        for (width, height, shown) in [
            (80, 24, true),
            (80, 23, true),
            (80, 22, false),
            (24, 40, false),
            (25, 40, true),
        ] {
            let mut top = 0;
            let lines = layout(&view(&tuner), width, height, &mut top);
            assert_eq!(lines.len(), usize::from(height));
            let has = lines.iter().any(|l| l.text.starts_with("+12 "));
            assert_eq!(has, shown, "{width}x{height}: {:#?}", texts(&lines));
            assert_eq!(selected(&lines).len(), 1, "{width}x{height}");
            let limit = usize::from(width) - 1;
            assert!(lines.iter().all(|l| l.text.chars().count() <= limit));
        }
    }

    #[test]
    fn the_list_scrolls_to_keep_the_selection_in_view() {
        let mut tuner = Tuner::new(Profile::default(), String::new());
        let mut top = 0;
        for _ in 0..56 {
            tuner.apply(Action::Down, 48_000);
            let lines = layout(&view(&tuner), 80, 24, &mut top);
            assert_eq!(selected(&lines).len(), 1, "{:#?}", texts(&lines));
        }
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        assert!(selected(&lines)[0].contains("true peak"));
        assert!(texts(&lines)[4].starts_with("-- 57/57 "));
        assert!(texts(&lines).contains(&" Limiter"), "{:#?}", texts(&lines));
        assert!(top > 0);
        // Back up to compressor 2's first setting: its heading shows too.
        for _ in 0..41 {
            tuner.apply(Action::Up, 48_000);
            layout(&view(&tuner), 80, 24, &mut top);
        }
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        let text = texts(&lines);
        assert!(selected(&lines)[0].contains("threshold"));
        assert_eq!(text[5], " Compressor 2");
    }

    #[test]
    fn the_status_line_shows_bypass_unsaved_changes_and_clamping() {
        let mut tuner = Tuner::new(Profile::default(), String::new());
        tuner.apply(Action::Bypass, 48_000);
        tuner.apply(
            Action::Step {
                up: true,
                fine: false,
            },
            48_000,
        );
        let mut top = 0;
        let lines = layout(
            &View {
                clamped: 1234,
                dry_gain: 0.5,
                ..view(&tuner)
            },
            80,
            24,
            &mut top,
        );
        assert_eq!(
            lines[0].text,
            "dps live  [A*] B  BYPASS (dry -6.0 dB)  trim 0.0 dB  clamped 1234"
        );
    }

    #[test]
    fn a_long_message_wraps_above_the_legend() {
        let mut tuner = Tuner::new(Profile::default(), String::new());
        let warning = "warning: only silence from the system audio tap for 3 s while other apps \
             have audio output running; if you hear nothing while something is playing, the \
             Screen & System Audio Recording permission is probably missing: allow your \
             terminal app in System Settings";
        tuner.set_message(warning.into());
        let mut top = 0;
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        let text = texts(&lines);
        assert_eq!(lines.len(), 24);
        let legend = 24 - LEGEND.len();
        assert_eq!(&text[legend..], LEGEND);
        let rule = text.iter().rposition(|t| t.starts_with("-----")).unwrap();
        let message = &text[rule + 1..legend];
        assert!(message.len() > 1, "{message:?}");
        assert_eq!(message.join(" "), warning);
        assert!(message.iter().all(|m| m.chars().count() <= 79));
        assert_eq!(selected(&lines).len(), 1);
    }

    /// The keys in one read with nothing after it.
    fn keys(bytes: &[u8]) -> Vec<Key> {
        let mut parser = Parser::default();
        let mut keys = parser.feed(bytes);
        keys.extend(parser.flush());
        keys
    }

    /// The keys in reads that follow each other closely.
    fn reads(chunks: &[&[u8]]) -> Vec<Key> {
        let mut parser = Parser::default();
        let mut keys: Vec<Key> = chunks.iter().flat_map(|c| parser.feed(c)).collect();
        keys.extend(parser.flush());
        keys
    }

    #[test]
    fn a_sequence_split_across_reads_is_still_one_key() {
        use Key::*;
        // Home and End, cut after ESC [ or after ESC: never H or L.
        for split in [
            [&b"\x1b["[..], b"H"],
            [b"\x1b", b"[H"],
            [b"\x1bO", b"F"],
            [b"\x1b[1", b";2H"],
        ] {
            assert_eq!(reads(&split), [], "{split:?}");
        }
        assert_eq!(reads(&[b"\x1b", b"[1;2C"]), [ShiftRight]);
        assert_eq!(
            reads(&[b"j\x1b[1;", b"2D", b"k"]),
            [Char('j'), ShiftLeft, Char('k')]
        );
        // Nothing more came: a lone ESC was Esc, half a sequence is dropped.
        let mut parser = Parser::default();
        assert_eq!(parser.feed(b"\x1b"), []);
        assert!(parser.waiting());
        assert_eq!(parser.flush(), Some(Esc));
        assert_eq!(parser.feed(b"\x1b["), []);
        assert_eq!(parser.flush(), None);
        assert!(!parser.waiting());
    }

    #[test]
    fn ctrl_c_inside_a_sequence_still_quits() {
        use Key::*;
        assert_eq!(keys(b"\x1b[\x03"), [CtrlC]);
        assert_eq!(reads(&[b"\x1b[", b"\x03"]), [CtrlC]);
        assert_eq!(keys(b"\x1b[1;\x03q"), [CtrlC, Char('q')]);
    }

    #[test]
    fn alt_keys_are_not_their_plain_keys() {
        for alt in [b"\x1bh", b"\x1bl", b"\x1bH", b"\x1bq", b"\x1bj"] {
            assert_eq!(keys(alt), [], "{alt:?}");
        }
        assert_eq!(keys(b"\x1bhj"), [Key::Char('j')]);
    }

    #[test]
    fn terminal_input_becomes_keys() {
        use Key::*;
        // Arrows as terminals send them, in normal and application mode.
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1bOA\x1bOB\x1bOC\x1bOD"),
            [Up, Down, Right, Left, Up, Down, Right, Left]
        );
        // xterm's Shift+Right and Shift+Left.
        assert_eq!(keys(b"\x1b[1;2C\x1b[1;2D"), [ShiftRight, ShiftLeft]);
        assert_eq!(
            keys(b"jkhlHL bsrqyn"),
            "jkhlHL bsrqyn".chars().map(Char).collect::<Vec<_>>()
        );
        assert_eq!(keys(b"\x03"), [CtrlC]);
        // Esc alone, or twice.
        assert_eq!(keys(b"\x1b"), [Esc]);
        assert_eq!(keys(b"\x1b\x1b"), [Esc, Esc]);
    }

    #[test]
    fn other_input_is_skipped() {
        use Key::*;
        // Other control keys, Ctrl+arrows, F1, Delete, Home, UTF-8 text.
        assert_eq!(keys(b"\x04\x0c\r\x7f"), []);
        assert_eq!(keys(b"\x1b[1;5C\x1bOP\x1b[3~\x1b[H"), []);
        assert_eq!(keys("é".as_bytes()), []);
        assert_eq!(keys(b"\x1b[1;5Cj\x1b[3~k"), [Char('j'), Char('k')]);
        // A sequence cut off at the end of a read is dropped.
        assert_eq!(keys(b"j\x1b[1;"), [Char('j')]);
    }

    #[test]
    fn tab_and_shift_tab_are_keys() {
        use Key::*;
        assert_eq!(keys(b"\t\x1b[Z\tZ"), [Tab, BackTab, Tab, Char('Z')]);
        // Shift+Tab cut off anywhere is still one key, not Esc and Z.
        for split in [[&b"\x1b"[..], b"[Z"], [b"\x1b[", b"Z"]] {
            assert_eq!(reads(&split), [BackTab], "{split:?}");
        }
        assert_eq!(
            reads(&[b"j\x1b", b"[Z\t", b"k"]),
            [Char('j'), BackTab, Tab, Char('k')]
        );
        // Alt+Z, and Ctrl+Shift+Tab as some terminals send it, are neither.
        assert_eq!(keys(b"\x1bZ"), []);
        assert_eq!(keys(b"\x1b[1;5Z"), []);
    }

    #[test]
    fn the_legend_names_tab_and_fits_80_columns() {
        assert!(LEGEND[0].contains("Tab EQ field"));
        assert!(LEGEND[1].contains("g graph"));
        for key in ["a A/B", "c copy", "p next preset", "m match", "x blind"] {
            assert!(LEGEND[2].contains(key), "{key}");
        }
        for key in ["a or Tab", "1 prefer X", "2 prefer Y", "x or Esc"] {
            assert!(BLIND_LEGEND.iter().any(|l| l.contains(key)), "{key}");
        }
        for line in LEGEND.iter().chain(&BLIND_LEGEND) {
            assert!(line.chars().count() <= 79, "{line}");
        }
    }

    /// A tuner with B holding a preset, A heard.
    fn compared() -> Tuner {
        let mut tuner = Tuner::new(Profile::default(), "music.toml".into());
        tuner.apply(Action::Switch, 48_000);
        tuner.apply(Action::Preset, 48_000);
        tuner.apply(Action::Switch, 48_000);
        tuner
    }

    #[test]
    fn the_status_line_shows_the_slots_and_the_matching_gain() {
        let mut tuner = compared();
        let status = |tuner: &Tuner, trim: [f32; 2]| {
            let mut top = 0;
            let view = View {
                trim,
                ..view(tuner)
            };
            layout(&view, 80, 24, &mut top)[0].text.clone()
        };
        assert_eq!(
            status(&tuner, [0.0, -1.34]),
            "dps live  [A] B*  PROCESSED  trim B -1.3 dB"
        );
        assert_eq!(
            status(&tuner, [-0.02, 0.0]),
            "dps live  [A] B*  PROCESSED  trim 0.0 dB"
        );
        tuner.apply(Action::Switch, 48_000);
        assert_eq!(
            status(&tuner, [-2.0, 0.0]),
            "dps live  A [B*]  PROCESSED  trim A -2.0 dB"
        );
        tuner.apply(Action::Matching, 48_000);
        assert_eq!(
            status(&tuner, [0.0; 2]),
            "dps live  A [B*]  PROCESSED  match off"
        );
        // The same in both: no matching to speak of.
        tuner.apply(Action::Copy, 48_000);
        assert_eq!(status(&tuner, [0.0; 2]), "dps live  A* [B*]  PROCESSED");
    }

    #[test]
    fn the_blind_screen_shows_nothing_that_tells_the_slots_apart() {
        let mut tuner = compared();
        tuner.apply(Action::Graph, 48_000);
        tuner.apply(Action::Blind, 48_000);
        tuner.apply(Action::Vote(0), 48_000);
        tuner.apply(Action::Vote(1), 48_000);
        let playing = tuner.trial().unwrap().playing;
        let mut top = 0;
        let view = View {
            clamped: 12,
            dry_gain: 0.5,
            meters: readings(),
            trim: [0.0, -1.3],
            ..view(&tuner)
        };
        let lines = layout(&view, 80, 24, &mut top);
        let text = texts(&lines);
        assert_eq!(lines.len(), 24);
        assert_eq!(text[0], format!("dps live  BLIND  playing {playing}"));
        assert_eq!(lines[0].style, Style::Bold);
        assert_eq!(text[1], "trial 3");
        assert_eq!(&text[24 - BLIND_LEGEND.len()..], BLIND_LEGEND);
        assert_eq!(
            text[24 - BLIND_LEGEND.len() - 1],
            "voted for Y; trial 3: X and Y drawn again"
        );
        let all = text.join("\n");
        for giveaway in [
            "dB", "Hz", "LUFS", "ms", ":1", "trim", "match", "[A", "[B", "A*", "B*", "clamped",
            "music", "MacBook",
        ] {
            assert!(!all.contains(giveaway), "{giveaway:?} in {text:#?}");
        }
        let words: Vec<&str> = all.split(|c: char| !c.is_alphanumeric()).collect();
        assert!(!words.contains(&"A") && !words.contains(&"B"), "{text:#?}");
        // No list row, so no selection to show either.
        assert!(selected(&lines).is_empty());
        assert!(!all.chars().any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)));
    }

    #[test]
    fn any_terminal_size_gives_exactly_that_many_lines_with_every_panel() {
        let plain = Tuner::new(Profile::default(), String::new());
        let mut graphed = compared();
        graphed.apply(Action::Graph, 48_000);
        let mut blind = compared();
        blind.apply(Action::Blind, 48_000);
        for tuner in [&plain, &graphed, &blind] {
            for (width, height) in [
                (1, 1),
                (10, 3),
                (20, 8),
                (40, 10),
                (40, 30),
                (80, 24),
                (80, 40),
                (120, 50),
                (200, 100),
                (0, 0),
            ] {
                let mut top = 0;
                let view = View {
                    meters: readings(),
                    trim: [0.0, -1.3],
                    ..view(tuner)
                };
                let lines = layout(&view, width, height, &mut top);
                assert_eq!(lines.len(), usize::from(height), "{width}x{height}");
                let limit = usize::from(width).saturating_sub(1).max(1);
                assert!(
                    lines.iter().all(|l| l.text.chars().count() <= limit),
                    "{width}x{height}: {:#?}",
                    texts(&lines)
                );
            }
        }
    }

    /// The tuner with band `band` selected and `tabs` presses of Tab made.
    fn on_band(profile: Profile, band: usize, tabs: usize) -> Tuner {
        let mut tuner = Tuner::new(profile, String::new());
        // 19 settings come before the bands.
        for _ in 0..19 + band {
            tuner.apply(Action::Down, 48_000);
        }
        for _ in 0..tabs {
            tuner.apply(Action::NextField, 48_000);
        }
        tuner
    }

    /// Where the last character of each listed setting's value is.
    fn value_ends(lines: &[Line]) -> Vec<usize> {
        lines
            .iter()
            .filter(|l| l.text.starts_with([' ', '>']))
            .filter_map(|l| l.text.find(" dB").map(|at| at + 2))
            .collect()
    }

    #[test]
    fn a_band_row_brackets_its_active_field_and_lines_up_with_the_rest() {
        let mut top = 0;
        // Tall enough to show six dB values above the band.
        let rows = |tuner: &Tuner, top: &mut usize| layout(&view(tuner), 80, 25, top);
        let tuner = on_band(Profile::default(), 1, 0);
        let lines = rows(&tuner, &mut top);
        assert_eq!(selected(&lines), [">     56.4 Hz  Q 1.50       [+0.0 dB]"]);
        let text = texts(&lines);
        assert!(text.contains(&" EQ bands"), "{text:#?}");
        assert!(text.contains(&"      30.0 Hz  Q 1.50        +0.0 dB "));
        // Only the selected band shows brackets.
        let listed = text.iter().filter(|t| t.starts_with([' ', '>']));
        assert_eq!(listed.filter(|t| t.contains('[')).count(), 1);
        // Every dB value ends in the same column, band or not.
        let ends = value_ends(&lines);
        assert!(ends.len() >= 6, "{text:#?}");
        assert!(ends.iter().all(|&e| e == 35), "{text:#?}");
        // Tab moves the brackets and nothing else.
        let tuner = on_band(Profile::default(), 1, 1);
        let lines = rows(&tuner, &mut top);
        assert_eq!(selected(&lines), [">    [56.4 Hz] Q 1.50        +0.0 dB "]);
        let tuner = on_band(Profile::default(), 1, 2);
        let lines = rows(&tuner, &mut top);
        assert_eq!(selected(&lines), [">     56.4 Hz [Q 1.50]       +0.0 dB "]);
        assert!(value_ends(&lines).iter().all(|&e| e == 35));
        let tuner = on_band(Profile::default(), 6, 0);
        let lines = rows(&tuner, &mut top);
        assert_eq!(selected(&lines), [">     1327 Hz  Q 1.50       [+0.0 dB]"]);
    }

    #[test]
    fn band_rows_with_the_widest_values_fit_80_columns() {
        // A frequency written by hand with a decimal, the widest Q and gain
        // on the grids, at 32 kHz where both top bands are skipped.
        let mut p = Profile::default();
        p.eq.freqs_hz[0] = 0.0484;
        p.eq.freqs_hz[9] = 24_000.0;
        p.eq.freqs_hz[10] = 16_000.5;
        p.eq.q[0] = 19.9;
        p.eq.q[9] = 20.0;
        p.eq.q[10] = 0.101;
        p.eq.gains_db = [-24.0; 11];
        p.eq.gains_db[9] = 24.0;
        for tabs in 0..3 {
            let tuner = on_band(p.clone(), 10, tabs);
            let mut top = 0;
            let lines = layout(
                &View {
                    rate: 32_000,
                    ..view(&tuner)
                },
                80,
                24,
                &mut top,
            );
            let text = texts(&lines);
            assert!(text.iter().all(|t| t.chars().count() <= 79), "{text:#?}");
            let want = [
                ">  16000.5 Hz  Q 0.101     [-24.0 dB] skipped",
                "> [16000.5 Hz] Q 0.101      -24.0 dB  skipped",
                ">  16000.5 Hz [Q 0.101]     -24.0 dB  skipped",
            ][tabs];
            assert_eq!(selected(&lines), [want]);
            assert!(
                text.contains(&"     24000 Hz  Q 20.0       +24.0 dB  skipped"),
                "{text:#?}"
            );
            assert!(value_ends(&lines).iter().all(|&e| e == 35), "{text:#?}");
            assert!(text[0].ends_with("  2 EQ bands skipped"), "{}", text[0]);
        }
        // The lowest band shows its frequency in full.
        let tuner = on_band(p, 0, 1);
        let mut top = 0;
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        assert_eq!(selected(&lines), [">  [0.0484 Hz] Q 19.9       -24.0 dB "]);
    }

    #[test]
    fn the_status_line_counts_the_bands_the_chain_skips() {
        let tuner = Tuner::new(Profile::default(), String::new());
        for (rate, want) in [
            (48_000, "dps live  [A] B  PROCESSED"),
            (32_000, "dps live  [A] B  PROCESSED  1 EQ band skipped"),
            (16_000, "dps live  [A] B  PROCESSED  2 EQ bands skipped"),
        ] {
            let mut top = 0;
            let lines = layout(
                &View {
                    rate,
                    ..view(&tuner)
                },
                80,
                24,
                &mut top,
            );
            assert_eq!(lines[0].text, want);
        }
        // After unsaved changes and clamping.
        let mut tuner = on_band(Profile::default(), 10, 0);
        tuner.apply(
            Action::Step {
                up: true,
                fine: false,
            },
            32_000,
        );
        let mut top = 0;
        let view = View {
            rate: 32_000,
            clamped: 5,
            ..view(&tuner)
        };
        let lines = layout(&view, 80, 24, &mut top);
        assert_eq!(
            lines[0].text,
            "dps live  [A*] B  PROCESSED  trim 0.0 dB  clamped 5  1 EQ band skipped"
        );
        assert!(selected(&lines)[0].ends_with(" skipped"));
    }

    #[test]
    fn a_question_stays_on_screen_under_any_message() {
        let mut tuner = Tuner::new(Profile::default(), "music.toml".into());
        tuner.apply(
            Action::Step {
                up: true,
                fine: false,
            },
            48_000,
        );
        tuner.apply(Action::Quit, 48_000);
        tuner.set_message("reloaded presets/music.toml".into());
        let mut top = 0;
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        let text = texts(&lines);
        let legend = 24 - LEGEND.len();
        assert_eq!(&text[legend..], LEGEND);
        assert_eq!(text[legend - 2], "reloaded presets/music.toml");
        assert_eq!(
            text[legend - 1],
            "save changes in A to music.toml? y/n (Esc cancels)"
        );
        assert_eq!(lines[legend - 1].style, Style::Bold);
        // A message as long as the whole area still leaves the question.
        tuner.set_message("word ".repeat(200));
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        let text = texts(&lines);
        assert_eq!(
            text[legend - 1],
            "save changes in A to music.toml? y/n (Esc cancels)"
        );
        assert!(text[legend - 2].ends_with("..."), "{}", text[legend - 2]);
        assert_eq!(selected(&lines).len(), 1);
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        assert_eq!(printable("a\x1b[2Jb\tc\u{9b}d\r\n"), "a?[2Jb c?d  ");
        let mut tuner = Tuner::new(Profile::default(), String::new());
        tuner.set_message("profile not reloaded: x.toml:2: `gain = \x1b]0;owned\x07`".into());
        let mut top = 0;
        let device = "Evil\x1b[2J\x1b[?1049l Speakers";
        let name = "/tmp/\x1b[31mred.toml";
        let lines = layout(
            &View {
                device,
                name,
                ..view(&tuner)
            },
            80,
            24,
            &mut top,
        );
        for line in &lines {
            assert!(!line.text.chars().any(char::is_control), "{:?}", line.text);
        }
        let screen = Screen {
            input: File::open("/dev/null").unwrap(),
            parser: Parser::default(),
            keys: VecDeque::new(),
            drawn: Vec::new(),
            size: (80, 24),
            top: 0,
            clear: false,
            hold: Hold::default(),
        };
        let notice = screen.notice("waiting\x1b[2J");
        let text = &notice[notice.iter().rposition(|&b| b == b'K').unwrap() + 1..];
        assert_eq!(text, b"waiting?[2J");
    }

    #[test]
    fn a_terminal_without_a_size_counts_as_80_by_24() {
        assert_eq!(usable((0, 0)), (80, 24));
        assert_eq!(usable((0, 40)), (80, 24));
        assert_eq!(usable((120, 0)), (80, 24));
        assert_eq!(usable((120, 40)), (120, 40));
    }

    #[test]
    fn termios_matches_the_c_layout() {
        // From <sys/termios.h>: four unsigned longs, cc_t[NCCS = 20], then
        // two speed_t (unsigned long); 72 bytes on 64-bit macOS.
        assert_eq!(mem::size_of::<Termios>(), 72);
        assert_eq!(mem::offset_of!(Termios, cc), 32);
        assert_eq!(mem::offset_of!(Termios, ispeed), 56);
    }

    #[test]
    fn putting_back_leaves_raw_mode_shows_the_cursor_and_leaves_the_alternate_screen() {
        let saved = Termios {
            iflag: 0x2b02,
            oflag: 0x3,
            cflag: 0x4b00,
            lflag: 0x5cb,
            cc: [7; 20],
            ispeed: 38_400,
            ospeed: 38_400,
        };
        let (mut written, mut modes) = (Vec::new(), Vec::new());
        put_back(&saved, &mut |b| written.extend_from_slice(b), &mut |m| {
            modes.push(*m)
        });
        assert_eq!(modes, [saved]);
        assert_eq!(written, RESTORE);
        // Each thing `enter` and `draw` turn on is turned off again.
        let mut undo = Vec::new();
        execute!(
            undo,
            EndSynchronizedUpdate,
            SetAttribute(Attribute::Reset),
            crossterm::terminal::EnableLineWrap,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        )
        .unwrap();
        assert_eq!(written, undo);
    }

    #[test]
    fn raw_mode_comes_off_with_the_saved_settings() {
        const ICANON: c_ulong = 0x100;
        const ECHO: c_ulong = 0x8;
        const ISIG: c_ulong = 0x80;
        let (_master, slave) = pty();
        let slave = slave.as_raw_fd();
        let before = mode(slave);
        assert_ne!(before.lflag & (ICANON | ECHO | ISIG), 0);
        let saved = raw_mode(slave).unwrap();
        assert_eq!(saved, before);
        assert_eq!(mode(slave).lflag & (ICANON | ECHO | ISIG), 0);
        let mut written = Vec::new();
        put_back(&saved, &mut |b| written.extend_from_slice(b), &mut |m| {
            // SAFETY: a valid terminal descriptor and settings.
            assert_eq!(unsafe { tcsetattr(slave, TCSANOW, m) }, 0);
        });
        // The kernel marks pending input (PENDIN) when canonical mode comes
        // back; that bit is its own state, not a setting.
        const PENDIN: c_ulong = 0x2000_0000;
        let after = mode(slave);
        assert_eq!(
            Termios {
                lflag: after.lflag & !PENDIN,
                ..after
            },
            before
        );
    }

    #[test]
    fn wrapping_caps_the_lines_and_marks_the_cut() {
        let text = "one two three four five six seven";
        assert_eq!(
            wrap(text, 9, 10),
            ["one two", "three", "four five", "six seven"]
        );
        assert_eq!(wrap(text, 9, 2), ["one two", "three..."]);
        assert_eq!(wrap("", 9, 2), Vec::<String>::new());
        assert_eq!(wrap(text, 9, 0), Vec::<String>::new());
        assert_eq!(
            wrap("saved to /a/bb/ccc.toml now", 8, 10),
            ["saved to", "/a/bb/cc", "c.toml", "now"]
        );
    }

    #[test]
    fn a_long_settings_name_keeps_its_end_and_the_device_shows() {
        let tuner = Tuner::new(Profile::default(), String::new());
        let name = format!("/Users/someone/{}tuned.toml", "deep/".repeat(20));
        let mut top = 0;
        let lines = layout(
            &View {
                name: &name,
                ..view(&tuner)
            },
            80,
            24,
            &mut top,
        );
        let second = &lines[1].text;
        assert_eq!(second.chars().count(), 79);
        assert!(
            second.starts_with("MacBook Pro Speakers at 48000 Hz   ..."),
            "{second}"
        );
        assert!(second.ends_with("deep/tuned.toml"), "{second}");
        assert_eq!(tail("abcdef", 4), "...f");
        assert_eq!(tail("abcdef", 2), "..");
        assert_eq!(tail("abc", 4), "abc");
    }

    #[test]
    fn any_terminal_size_gives_exactly_that_many_lines() {
        let tuner = Tuner::new(Profile::default(), String::new());
        for (width, height) in [(1, 1), (10, 3), (40, 10), (80, 24), (200, 100), (0, 0)] {
            let mut top = 0;
            let lines = layout(&view(&tuner), width, height, &mut top);
            assert_eq!(lines.len(), usize::from(height));
            let limit = usize::from(width).saturating_sub(1).max(1);
            assert!(lines.iter().all(|l| l.text.chars().count() <= limit));
        }
    }
}
