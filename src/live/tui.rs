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
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, Clear, ClearType, DisableLineWrap, EndSynchronizedUpdate,
    EnterAlternateScreen,
};
use crossterm::{execute, queue};

use super::tuner::{Field, Key, Row, Tuner};
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

const LEGEND: [&str; 2] = [
    "j/k Up/Down select   h/l Left/Right change   H/L or Shift: fine   Tab EQ field",
    "Space on/off   b bypass (A/B, L+R level matched)   s save   r revert   q quit",
];
const MAX_MESSAGE_LINES: usize = 6;

/// What the screen shows.
pub struct View<'a> {
    pub tuner: &'a Tuner,
    pub name: &'a str,
    pub device: &'a str,
    pub rate: u32,
    pub clamped: u64,
    pub dry_gain: f32,
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

    let mut status = String::from("dps live  ");
    if tuner.bypass() {
        status += &format!(
            "BYPASS (dry {:+.1} dB, L+R level matched)",
            20.0 * view.dry_gain.log10()
        );
    } else {
        status += "PROCESSED";
    }
    if tuner.dirty() {
        status += "  UNSAVED";
    }
    if view.clamped > 0 {
        status += &format!("  clamped {}", view.clamped);
    }
    match tuner.profile().skipped_eq_bands(view.rate).len() {
        0 => {}
        1 => status += "  1 EQ band skipped",
        n => status += &format!("  {n} EQ bands skipped"),
    }
    let (at, count) = tuner.position();
    let output = format!("{} at {} Hz   ", view.device, view.rate);
    let room = width.saturating_sub(output.chars().count());
    let mut lines = vec![
        line(status, Style::Bold),
        line(output + &tail(view.name, room), Style::Plain),
        line(
            format!("{:-<width$}", format!("-- {}/{count} ", at + 1)),
            Style::Plain,
        ),
    ];

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
    let fixed = lines.len() + 1 + message.len() + LEGEND.len();
    let list = height.saturating_sub(fixed).max(1);
    let rows = tuner.rows(view.rate);
    scroll(&rows, list, top);
    for i in *top..*top + list {
        let Some(row) = rows.get(i) else {
            lines.push(line(String::new(), Style::Plain));
            continue;
        };
        let (marker, style) = if row.selected() {
            ('>', Style::Reverse)
        } else {
            (' ', Style::Plain)
        };
        lines.push(match row {
            Row::Group(name) => line(format!(" {name}"), Style::Bold),
            Row::Setting { label, value, .. } => {
                line(format!("{marker}   {label:<20}{value:>12} "), style)
            }
            Row::Band {
                freq,
                q,
                gain,
                active,
                skipped,
                ..
            } => {
                // Every field keeps a column on each side for the brackets,
                // so the columns stay put as the active field moves, and
                // the gain lines up with the other settings' values.
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
                line(
                    format!("{marker} {freq:>12}{q:<10}{gain:>13}{skipped}"),
                    style,
                )
            }
        });
    }
    lines.push(line("-".repeat(width), Style::Plain));
    lines.extend(message.into_iter().map(|(m, style)| line(m, style)));
    lines.extend(LEGEND.iter().map(|l| line((*l).into(), Style::Plain)));
    lines.resize(
        height,
        Line {
            text: String::new(),
            style: Style::Plain,
        },
    );
    lines
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
        let lines = layout(view, self.size.0, self.size.1, &mut self.top);
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
        assert_eq!(text[0], "dps live  PROCESSED");
        assert_eq!(
            text[1],
            "MacBook Pro Speakers at 48000 Hz   presets/music.toml"
        );
        assert!(text[2].starts_with("-- 1/57 ---"));
        assert_eq!(text[3], " Leveler");
        assert_eq!(selected(&lines), [">   enabled                      off "]);
        assert_eq!(&text[22..], LEGEND);
        // The selection is marked in the text, not only by the highlight.
        assert!(text[4].starts_with('>'));
        assert!(text[5].starts_with(' '));
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
        assert!(texts(&lines)[2].starts_with("-- 57/57 "));
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
        assert_eq!(text[3], " Compressor 2");
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
            "dps live  BYPASS (dry -6.0 dB, L+R level matched)  UNSAVED  clamped 1234"
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
        assert_eq!(&text[22..], LEGEND);
        let rule = text.iter().rposition(|t| t.starts_with("-----")).unwrap();
        let message = &text[rule + 1..22];
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
        for line in LEGEND {
            assert!(line.chars().count() <= 79, "{line}");
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

    /// Where the last character of each setting's value is.
    fn value_ends(lines: &[Line]) -> Vec<usize> {
        lines
            .iter()
            .filter_map(|l| l.text.find(" dB").map(|at| at + 2))
            .collect()
    }

    #[test]
    fn a_band_row_brackets_its_active_field_and_lines_up_with_the_rest() {
        let mut top = 0;
        let rows = |tuner: &Tuner, top: &mut usize| layout(&view(tuner), 80, 24, top);
        let tuner = on_band(Profile::default(), 1, 0);
        let lines = rows(&tuner, &mut top);
        assert_eq!(selected(&lines), [">     56.4 Hz  Q 1.50       [+0.0 dB]"]);
        let text = texts(&lines);
        assert!(text.contains(&" EQ bands"), "{text:#?}");
        assert!(text.contains(&"      30.0 Hz  Q 1.50        +0.0 dB "));
        // Only the selected band shows brackets.
        assert_eq!(text.iter().filter(|t| t.contains('[')).count(), 1);
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
            (48_000, "dps live  PROCESSED"),
            (32_000, "dps live  PROCESSED  1 EQ band skipped"),
            (16_000, "dps live  PROCESSED  2 EQ bands skipped"),
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
            "dps live  PROCESSED  UNSAVED  clamped 5  1 EQ band skipped"
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
        assert_eq!(&text[22..], LEGEND);
        assert_eq!(text[20], "reloaded presets/music.toml");
        assert_eq!(text[21], "save changes to music.toml? y/n (Esc cancels)");
        assert_eq!(lines[21].style, Style::Bold);
        // A message as long as the whole area still leaves the question.
        tuner.set_message("word ".repeat(200));
        let lines = layout(&view(&tuner), 80, 24, &mut top);
        let text = texts(&lines);
        assert_eq!(text[21], "save changes to music.toml? y/n (Esc cancels)");
        assert!(text[20].ends_with("..."), "{}", text[20]);
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
