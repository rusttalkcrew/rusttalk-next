//! A terminal shared in a call, on this computer: the shell in a pseudo terminal (the
//! sharer's side only), the emulator both sides replay its output in, drawing it, and
//! turning keys into the bytes a terminal sends.

use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{
        Config, Osc52, Term, TermMode,
        cell::{Cell, Flags},
        test::TermSize,
    },
    vte::ansi::{Color, CursorShape, NamedColor, Processor},
};
use anyhow::{Context, Result};
use eframe::egui::{
    Align2, Color32, Event, EventFilter, FontId, Id as UiId, Key, Modifiers, MouseWheelUnit, Pos2,
    Rect, Response, Role, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, pos2, vec2,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rusttalk_core::terminal::MAX_SNAPSHOT;
use unicode_width::UnicodeWidthChar;

use crate::ui;

/// Lines kept above the screen.
const HISTORY: usize = 5000;
/// Lines of history a new viewer gets.
const SNAPSHOT_HISTORY: usize = 500;
/// Output kept for the call while it isn't sent; past this, viewers get a fresh screen.
const PENDING_CAP: usize = 1024 * 1024;
/// The smallest terminal the sharer's tile makes.
const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 5;

/// Combining marks one cell may carry on a viewer's side.
const MAX_MARKS: u8 = 8;
/// Pieces of typing waiting for the shell; more is dropped rather than waited for.
const TYPING_QUEUE: usize = 256;

/// Hands bytes to the thread that writes them to the shell, so nobody waits for a shell
/// that doesn't read (and never while holding the terminal).
#[derive(Clone)]
struct Writer(SyncSender<Vec<u8>>);

impl Writer {
    fn start(mut pty: Box<dyn Write + Send>) -> Result<Self> {
        let (sender, typed) = sync_channel::<Vec<u8>>(TYPING_QUEUE);
        thread::Builder::new()
            .name("terminal input".into())
            .spawn(move || {
                // Ends when the share is gone and nobody can send any more.
                for bytes in typed {
                    if let Err(error) = pty.write_all(&bytes).and_then(|()| pty.flush()) {
                        tracing::debug!(%error, "writing to the shared shell failed");
                        break;
                    }
                }
            })?;
        Ok(Self(sender))
    }

    fn send(&self, bytes: &[u8]) {
        match self.0.try_send(bytes.to_vec()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::debug!("the shared shell isn't reading; dropped typing");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Answers what programs ask the terminal (the cursor position, what it supports) on the
/// sharer's side. Viewers never answer: the sharer's terminal already did.
struct Listener(Option<Writer>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        if let (TermEvent::PtyWrite(text), Some(writer)) = (event, &self.0) {
            writer.send(text.as_bytes());
        }
    }
}

/// Keeps a sharer from piling endless combining marks onto one cell of a viewer's
/// screen: past `MAX_MARKS` in a row, more are dropped. Escape sequences don't count,
/// and only a printed letter starts a new row, so nothing in between resets it.
#[derive(Default)]
struct Marks {
    /// In an escape sequence: 1 after Escape, 2 in a CSI, 3 in a string (OSC and kin).
    escape: u8,
    run: u8,
    /// The start of a letter cut off at the end of the last bytes.
    carry: Vec<u8>,
}

impl Marks {
    fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(bytes);
        let mut out = Vec::with_capacity(input.len());
        let mut chunks = input.utf8_chunks().peekable();
        while let Some(chunk) = chunks.next() {
            for c in chunk.valid().chars() {
                if self.keep(c) {
                    let mut buffer = [0; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                }
            }
            let invalid = chunk.invalid();
            // A letter cut in two waits for the rest.
            if chunks.peek().is_none() && !invalid.is_empty() && invalid.len() < 4 {
                self.carry = invalid.to_vec();
            } else {
                out.extend_from_slice(invalid);
            }
        }
        out
    }

    fn keep(&mut self, c: char) -> bool {
        match (self.escape, c) {
            (_, '\x1b') => self.escape = 1,
            (1, '[') => self.escape = 2,
            (1, ']' | 'P' | '_' | '^' | 'X') => self.escape = 3,
            (1, _) => self.escape = 0,
            (2, '\x40'..='\x7e') => self.escape = 0,
            (3, '\x07') => self.escape = 0,
            (2 | 3, _) => {}
            (_, c) if !c.is_ascii() && UnicodeWidthChar::width(c) == Some(0) => {
                self.run = self.run.saturating_add(1);
                return self.run <= MAX_MARKS;
            }
            (_, '\x20'..='\x7e') => self.run = 0,
            (_, c) if !c.is_ascii() => self.run = 0,
            _ => {}
        }
        true
    }
}

/// A terminal screen and its history, fed with what the shell prints.
pub struct Emulator {
    term: Term<Listener>,
    parser: Processor,
    /// On a viewer's side, what the sharer may pile onto a cell is limited.
    marks: Option<Marks>,
}

impl Emulator {
    /// A viewer's terminal.
    pub fn new(cols: u16, rows: u16) -> Self {
        let mut emulator = Self::with(cols, rows, Listener(None));
        emulator.marks = Some(Marks::default());
        emulator
    }

    fn with(cols: u16, rows: u16, listener: Listener) -> Self {
        let config = Config {
            scrolling_history: HISTORY,
            // Nobody's clipboard is read or written by what the shell prints.
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let size = TermSize::new(cols.into(), rows.into());
        Self {
            term: Term::new(config, &size, listener),
            parser: Processor::new(),
            marks: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        match &mut self.marks {
            Some(marks) => {
                let bytes = marks.filter(bytes);
                self.parser.advance(&mut self.term, &bytes);
            }
            None => self.parser.advance(&mut self.term, bytes),
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.columns() as u16, self.term.screen_lines() as u16)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(TermSize::new(cols.into(), rows.into()));
    }

    /// Bytes that draw this screen, with some history, in a fresh terminal of the same
    /// size, at most `MAX_SNAPSHOT` of them: a very colourful screen first loses history,
    /// then its colours. While a full-screen program runs only its screen is sent, and
    /// the second value is true: the screen underneath can't be read until it ends.
    pub fn snapshot(&self) -> (Vec<u8>, bool) {
        let alt = self.alt();
        for (history, plain) in [
            (SNAPSHOT_HISTORY, false),
            (SNAPSHOT_HISTORY / 5, false),
            (0, false),
            (0, true),
        ] {
            let screen = self.draw(history, plain);
            if screen.len() <= MAX_SNAPSHOT {
                return (screen, alt);
            }
        }
        // Only a screen full of combining marks gets here.
        let mut screen = self.draw(0, true);
        screen.truncate(MAX_SNAPSHOT);
        (screen, alt)
    }

    fn draw(&self, history: usize, plain: bool) -> Vec<u8> {
        let grid = self.term.grid();
        let cols = grid.columns();
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let mut out = Vec::new();
        if alt {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        let mut pen = Pen { last: None, plain };
        let lines = grid.screen_lines() as i32;
        let history = grid.history_size().min(history) as i32;
        for line in -history..lines {
            let row = &grid[Line(line)];
            let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            // A wrapped line fills every cell so the next one wraps onto the line below.
            let end = if wrapped {
                cols
            } else {
                (0..cols)
                    .rev()
                    .find(|col| row[Column(*col)] != Cell::default())
                    .map_or(0, |col| col + 1)
            };
            for col in 0..end {
                let cell = &row[Column(col)];
                if !cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    pen.draw(&mut out, cell);
                }
            }
            if line + 1 < lines && !wrapped {
                pen.set(&mut out, &Cell::default());
                out.extend_from_slice(b"\r\n");
            }
        }
        let cursor = &grid.cursor;
        let (line, col) = (cursor.point.line.0 + 1, cursor.point.column.0 + 1);
        out.extend_from_slice(format!("\x1b[{line};{col}H").as_bytes());
        if cursor.input_needs_wrap {
            // Print the last cell again: the cursor then waits past the line's end.
            pen.draw(&mut out, &grid[cursor.point]);
        }
        pen.set(&mut out, &cursor.template);
        let mode = *self.term.mode();
        for (on, set) in [
            (!mode.contains(TermMode::SHOW_CURSOR), "\x1b[?25l"),
            (mode.contains(TermMode::APP_CURSOR), "\x1b[?1h"),
            (mode.contains(TermMode::APP_KEYPAD), "\x1b="),
            (mode.contains(TermMode::BRACKETED_PASTE), "\x1b[?2004h"),
            (!mode.contains(TermMode::LINE_WRAP), "\x1b[?7l"),
        ] {
            if on {
                out.extend_from_slice(set.as_bytes());
            }
        }
        out
    }

    fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// A full-screen program (an editor, a pager) has the screen.
    pub fn alt(&self) -> bool {
        self.mode().contains(TermMode::ALT_SCREEN)
    }
}

/// The colours and attributes last written in a snapshot, to write only changes;
/// `None` until something is written. A `plain` pen writes no colours at all.
struct Pen {
    last: Option<(Color, Color, Flags)>,
    plain: bool,
}

/// The attributes that SGR sets.
const STYLE: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

impl Pen {
    fn draw(&mut self, out: &mut Vec<u8>, cell: &Cell) {
        self.set(out, cell);
        let mut buffer = [0; 4];
        out.extend_from_slice(cell.c.encode_utf8(&mut buffer).as_bytes());
        for c in cell.zerowidth().unwrap_or_default() {
            out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
        }
    }

    fn set(&mut self, out: &mut Vec<u8>, cell: &Cell) {
        let wanted = (cell.fg, cell.bg, cell.flags & STYLE);
        if self.plain {
            return;
        }
        let current = self.last.unwrap_or((
            Color::Named(NamedColor::Foreground),
            Color::Named(NamedColor::Background),
            Flags::empty(),
        ));
        if current == wanted {
            return;
        }
        let mut sgr = String::from("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "21"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if wanted.2.contains(flag) {
                sgr.push(';');
                sgr.push_str(code);
            }
        }
        sgr_color(&mut sgr, cell.fg, true);
        sgr_color(&mut sgr, cell.bg, false);
        sgr.push('m');
        out.extend_from_slice(sgr.as_bytes());
        self.last = Some(wanted);
    }
}

fn sgr_color(sgr: &mut String, color: Color, fg: bool) {
    use std::fmt::Write as _;
    let (base, bright, extended) = if fg { (30, 90, 38) } else { (40, 100, 48) };
    let _ = match color {
        Color::Named(named) => match named as usize {
            index @ 0..8 => write!(sgr, ";{}", base + index),
            index @ 8..16 => write!(sgr, ";{}", bright + index - 8),
            // The default colours are what the reset already gave.
            _ => Ok(()),
        },
        Color::Indexed(index) => write!(sgr, ";{extended};5;{index}"),
        Color::Spec(rgb) => write!(sgr, ";{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b),
    };
}

/// One piece of what the shell did, in order, for the call.
pub enum Out {
    Bytes(Vec<u8>),
    Resize(u16, u16),
}

/// The sharer's terminal, shared between the window, the call and the thread reading
/// the shell's output.
pub struct Inner {
    pub emulator: Emulator,
    pending: Vec<Out>,
    pending_bytes: usize,
    /// Output was dropped before the call took it.
    overflowed: bool,
    exited: bool,
}

impl Inner {
    /// What happened since the last call, and whether some of it was dropped.
    pub fn take(&mut self) -> (Vec<Out>, bool) {
        self.pending_bytes = 0;
        let overflowed = std::mem::take(&mut self.overflowed);
        (std::mem::take(&mut self.pending), overflowed)
    }

    fn push(&mut self, out: Out) {
        if self.overflowed {
            return;
        }
        if let Out::Bytes(bytes) = &out {
            self.pending_bytes += bytes.len();
            if self.pending_bytes > PENDING_CAP {
                self.pending.clear();
                self.overflowed = true;
                return;
            }
            if let Some(Out::Bytes(last)) = self.pending.last_mut() {
                last.extend_from_slice(bytes);
                return;
            }
        }
        self.pending.push(out);
    }
}

#[derive(Clone)]
pub struct Shared {
    inner: Arc<Mutex<Inner>>,
    writer: Writer,
}

impl Shared {
    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        (self.inner.lock()).unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Type into the shell. Never waits: if the shell doesn't keep up, typing is dropped.
    pub fn write(&self, bytes: &[u8]) {
        self.writer.send(bytes);
    }
}

/// Which shell to start.
#[derive(Clone, Copy, PartialEq)]
pub enum Shell {
    /// PowerShell on Windows, the person's own shell elsewhere.
    Default,
    /// Command Prompt (Windows).
    Cmd,
}

/// The shell to start, with fallbacks to try if it doesn't.
fn commands(shell: Shell) -> Vec<CommandBuilder> {
    let programs: Vec<std::ffi::OsString> = if cfg!(windows) {
        match shell {
            Shell::Cmd => vec!["cmd.exe".into()],
            Shell::Default => vec!["pwsh.exe".into(), "powershell.exe".into()],
        }
    } else {
        let own = std::env::var_os("SHELL").filter(|shell| !shell.is_empty());
        own.into_iter().chain(["/bin/sh".into()]).collect()
    };
    programs
        .into_iter()
        .map(|program| {
            let mut command = CommandBuilder::new(program);
            if let Ok(home) = etcetera::home_dir() {
                command.cwd(home);
            }
            // The shell is the person's own, not part of RustTalk: leave out what the
            // AppImage and RustTalk set for themselves.
            for (name, _) in std::env::vars_os() {
                let own = name.to_str().is_some_and(|name| {
                    name.starts_with("RUSTTALK_")
                        || ["LD_LIBRARY_PATH", "APPDIR", "APPIMAGE", "ARGV0", "OWD"].contains(&name)
                });
                if own {
                    command.env_remove(name);
                }
            }
            command.env("TERM", "xterm-256color");
            command.env("COLORTERM", "truecolor");
            command
        })
        .collect()
}

/// Our own shared terminal: the shell runs as long as this lives.
pub struct Local {
    pub shared: Shared,
    master: Box<dyn MasterPty + Send>,
    /// Asks the thread that watches the shell to end it.
    stop: Arc<AtomicBool>,
    /// Closing it ends every process the shell started (Windows).
    #[cfg(windows)]
    _job: Option<job::Job>,
    /// The size the tile asks for, and since when, so resizing waits for it to settle.
    wanted: Option<((u16, u16), f64)>,
}

impl Local {
    /// Start `shell` in a pseudo terminal of this size. `repaint` is called when output
    /// arrives or the shell ends.
    pub fn start(
        shell: Shell,
        cols: u16,
        rows: u16,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::spawn(commands(shell), cols, rows, repaint)
    }

    fn spawn(
        commands: Vec<CommandBuilder>,
        cols: u16,
        rows: u16,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| anyhow::anyhow!("{error:#}"))
            .context("open a pseudo terminal")?;
        let mut last = None;
        let mut child = None;
        for command in commands {
            match pair.slave.spawn_command(command) {
                Ok(spawned) => {
                    child = Some(spawned);
                    break;
                }
                Err(error) => last = Some(error),
            }
        }
        let Some(mut child) = child else {
            let error = last.map(|error| format!("{error:#}")).unwrap_or_default();
            anyhow::bail!("start a shell: {error}");
        };
        // Only the shell keeps the terminal's other end open, so it ends with the shell.
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(anyhow::Error::msg)?;
        #[cfg(windows)]
        let job = (child.as_raw_handle()).and_then(|process| match job::Job::new(process) {
            Ok(job) => Some(job),
            Err(error) => {
                tracing::warn!(%error, "grouping the shared shell's processes failed");
                None
            }
        });
        let writer = Writer::start(pair.master.take_writer().map_err(anyhow::Error::msg)?)?;
        let shared = Shared {
            inner: Arc::new(Mutex::new(Inner {
                emulator: Emulator::with(cols, rows, Listener(Some(writer.clone()))),
                pending: Vec::new(),
                pending_bytes: 0,
                overflowed: false,
                exited: false,
            })),
            writer,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let repaint = Arc::new(repaint);
        let output = shared.clone();
        let wake = repaint.clone();
        thread::Builder::new()
            .name("terminal output".into())
            .spawn(move || read(reader, &output, &*wake))?;
        let ended = shared.clone();
        thread::Builder::new()
            .name("terminal shell".into())
            .spawn(move || {
                watch(&mut *child, &stopping);
                ended.lock().exited = true;
                repaint();
            })?;
        Ok(Self {
            shared,
            master: pair.master,
            stop,
            #[cfg(windows)]
            _job: job,
            wanted: None,
        })
    }

    /// The shell ended by itself.
    pub fn exited(&self) -> bool {
        self.shared.lock().exited
    }

    /// Follow the tile's size once it has stayed the same for a moment, at `now`.
    pub fn follow(&mut self, wanted: (u16, u16), now: f64) -> bool {
        let wanted = (wanted.0.max(MIN_COLS), wanted.1.max(MIN_ROWS));
        let since = match self.wanted {
            Some((size, since)) if size == wanted => since,
            _ => {
                self.wanted = Some((wanted, now));
                now
            }
        };
        let mut inner = self.shared.lock();
        if inner.emulator.size() == wanted || now - since < 0.3 {
            return inner.emulator.size() != wanted;
        }
        let (cols, rows) = wanted;
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        if let Err(error) = self.master.resize(size) {
            tracing::warn!(error = %format_args!("{error:#}"), "resizing the shared terminal failed");
            return false;
        }
        inner.emulator.resize(cols, rows);
        inner.push(Out::Resize(cols, rows));
        false
    }
}

impl Drop for Local {
    /// Stopping the share ends the shell and what it started.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// How long the shell's processes get to end after a hangup before they're killed.
const GRACE: Duration = Duration::from_secs(2);

/// Wait for the shell to end, or end it when asked. Only this thread reaps the shell,
/// so while it hasn't, its process group can't be another's.
fn watch(child: &mut dyn Child, stop: &AtomicBool) {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::info!(code = status.exit_code(), "shared shell ended");
                return;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "waiting for the shared shell failed");
                return;
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    tracing::info!("ending the shared shell");
    #[cfg(unix)]
    if let Some(pid) = child.process_id() {
        // The shell leads its own session and process group: hang up on all of it,
        // then kill what is left once the grace period is over.
        let group = pid as libc::pid_t;
        unsafe { libc::killpg(group, libc::SIGHUP) };
        let until = Instant::now() + GRACE;
        while Instant::now() < until {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
    #[cfg(windows)]
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::RawHandle;

    use windows::{
        Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        },
        core::PCWSTR,
    };

    /// A job holding the shell and everything it starts; closing it ends them all.
    pub struct Job(HANDLE);

    // The handle is only closed, once, by its owner.
    unsafe impl Send for Job {}

    impl Job {
        pub fn new(process: RawHandle) -> windows::core::Result<Self> {
            unsafe {
                let job = Self(CreateJobObjectW(None, PCWSTR::null())?);
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )?;
                AssignProcessToJobObject(job.0, HANDLE(process))?;
                Ok(job)
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn read(mut reader: Box<dyn Read + Send>, shared: &Shared, repaint: &dyn Fn()) {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let bytes = &buffer[..read];
                let mut inner = shared.lock();
                inner.emulator.feed(bytes);
                inner.push(Out::Bytes(bytes.to_vec()));
                drop(inner);
                repaint();
            }
        }
    }
    shared.lock().exited = true;
    repaint();
}

/// Cell size in points at text size `font`.
fn cell(ui: &Ui, font: f32) -> Vec2 {
    let id = FontId::monospace(font);
    ui.ctx()
        .fonts_mut(|fonts| vec2(fonts.glyph_width(&id, 'M'), fonts.row_height(&id)))
}

/// How many columns and rows fit `rect` at the normal text size.
pub fn fit(ui: &Ui, rect: Rect) -> (u16, u16) {
    let size = cell(ui, ui::TEXT_MONO);
    let cols = (rect.width() / size.x).floor().clamp(0.0, 500.0) as u16;
    let rows = (rect.height() / size.y).floor().clamp(0.0, 200.0) as u16;
    (cols, rows)
}

/// Where the screen is drawn: its top left corner and one cell's size.
#[derive(Clone, Copy)]
struct Layout {
    origin: Pos2,
    cell: Vec2,
}

impl Layout {
    /// The cell under `pos`, and which half of it.
    fn point(&self, pos: Pos2, emulator: &Emulator) -> (Point, Side) {
        let (cols, rows) = emulator.size();
        let at = (pos - self.origin) / self.cell;
        let col = at.x.clamp(0.0, f32::from(cols) - 0.01);
        let row = at.y.clamp(0.0, f32::from(rows) - 1.0) as i32;
        let offset = emulator.term.grid().display_offset() as i32;
        let side = if col.fract() < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        (Point::new(Line(row - offset), Column(col as usize)), side)
    }
}

/// Colours the shell picks, in the theme's terminal palette.
fn color(color: Color, palette: &[Color32; 16], fg: Color32, bg: Color32) -> Color32 {
    let indexed = |index: u8| match index {
        0..16 => palette[usize::from(index)],
        16..232 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            let index = index - 16;
            Color32::from_rgb(level(index / 36), level(index / 6 % 6), level(index % 6))
        }
        _ => Color32::from_gray(8 + (index - 232) * 10),
    };
    match color {
        Color::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => indexed(index),
        Color::Named(named) => match named as usize {
            index @ 0..16 => palette[index],
            _ if named == NamedColor::Background => bg,
            _ => fg,
        },
    }
}

/// Draw `emulator` in `rect`, as large as fits up to the normal text size, with a
/// background in the theme's colours.
fn paint(ui: &Ui, rect: Rect, emulator: &Emulator, focused: bool) -> Layout {
    let c = ui::colors(ui);
    let palette = if ui.visuals().dark_mode {
        &ui::TERMINAL_DARK
    } else {
        &ui::TERMINAL_LIGHT
    };
    let (fg, bg) = (c.text, c.side);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, ui::RADIUS, bg);
    let (cols, rows) = emulator.size();
    let unit = cell(ui, 100.0) / 100.0;
    let font = (ui::TEXT_MONO)
        .min(rect.width() / (f32::from(cols) * unit.x))
        .min(rect.height() / (f32::from(rows) * unit.y))
        .max(2.0);
    let size = cell(ui, font);
    let grid = vec2(f32::from(cols), f32::from(rows)) * size;
    let origin = rect.center() - grid / 2.0;
    let origin = pos2(origin.x.max(rect.left()), origin.y.max(rect.top()));
    let layout = Layout { origin, cell: size };
    let content = emulator.term.renderable_content();
    let offset = content.display_offset as i32;
    let selection = content.selection;
    let id = FontId::monospace(font);
    // Runs of cells that look the same, drawn as one piece of text.
    let mut run = String::new();
    let mut run_at = (0, 0);
    let mut run_color = fg;
    let flush = |run: &mut String, (row, col): (i32, usize), color: Color32| {
        if !run.trim_end().is_empty() {
            let at = origin + vec2(col as f32 * size.x, row as f32 * size.y);
            painter.text(at, Align2::LEFT_TOP, &*run, id.clone(), color);
        }
        run.clear();
    };
    for indexed in content.display_iter {
        let cell = indexed.cell;
        let row = indexed.point.line.0 + offset;
        let col = indexed.point.column.0;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let mut cell_fg = color(cell.fg, palette, fg, bg);
        let mut cell_bg = color(cell.bg, palette, fg, bg);
        if cell.flags.contains(Flags::BOLD)
            && let Color::Named(named) = cell.fg
            && (named as usize) < 8
        {
            cell_fg = palette[named as usize + 8];
        }
        if cell.flags.contains(Flags::DIM) {
            cell_fg = cell_fg.lerp_to_gamma(cell_bg, 0.4);
        }
        let selected = selection.is_some_and(|range| range.contains(indexed.point));
        if cell.flags.contains(Flags::INVERSE) ^ selected {
            std::mem::swap(&mut cell_fg, &mut cell_bg);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            cell_fg = cell_bg;
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2.0
        } else {
            1.0
        };
        let at = origin + vec2(col as f32 * size.x, row as f32 * size.y);
        let spot = Rect::from_min_size(at, vec2(width * size.x, size.y));
        if cell_bg != bg {
            painter.rect_filled(spot, 0.0, cell_bg);
        }
        let lines = cell.flags.intersects(Flags::ALL_UNDERLINES) as u8
            | (u8::from(cell.flags.contains(Flags::STRIKEOUT)) << 1);
        if lines & 1 != 0 {
            let y = spot.bottom() - 1.0;
            painter.line_segment(
                [pos2(spot.left(), y), pos2(spot.right(), y)],
                Stroke::new(1.0, cell_fg),
            );
        }
        if lines & 2 != 0 {
            let y = spot.center().y;
            painter.line_segment(
                [pos2(spot.left(), y), pos2(spot.right(), y)],
                Stroke::new(1.0, cell_fg),
            );
        }
        // Wide and non-ASCII letters may come from another font with other widths, so
        // each goes in its own place.
        let alone = width > 1.0 || !cell.c.is_ascii() || cell.zerowidth().is_some();
        let next_col = run_at.1 + run.chars().count();
        if alone || cell_fg != run_color || row != run_at.0 || col != next_col {
            flush(&mut run, run_at, run_color);
            run_at = (row, col);
            run_color = cell_fg;
        }
        run.push(cell.c);
        if alone {
            run.extend(cell.zerowidth().unwrap_or_default());
            flush(&mut run, run_at, run_color);
            run_at = (row, col + width as usize);
        }
    }
    flush(&mut run, run_at, run_color);
    let cursor = content.cursor;
    if cursor.shape != CursorShape::Hidden {
        let row = cursor.point.line.0 + offset;
        let at = origin + vec2(cursor.point.column.0 as f32, row as f32) * size;
        let spot = Rect::from_min_size(at, size);
        let accent = ui::accent(ui).base;
        if focused {
            painter.rect_filled(spot, 0.0, accent.gamma_multiply(0.7));
        } else {
            painter.rect_stroke(spot, 0.0, Stroke::new(1.0, accent), StrokeKind::Inside);
        }
    }
    layout
}

/// The terminal in `rect`. With `active` it takes the pointer for selecting and
/// scrolling and the keyboard (after a click) for copying; with `typing` too, keys are
/// returned as the bytes to send to the shell.
pub fn show(
    ui: &mut Ui,
    rect: Rect,
    id: UiId,
    emulator: &mut Emulator,
    active: bool,
    typing: bool,
    label: &str,
) -> (Option<Response>, Vec<u8>) {
    if !active {
        paint(ui, rect, emulator, false);
        return (None, Vec::new());
    }
    let response = ui.interact(rect, id, Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Terminal, true, label));
    let focused = response.has_focus();
    let layout = paint(ui, rect, emulator, focused && typing);
    if focused {
        ui.painter().rect_stroke(
            rect,
            ui::RADIUS,
            Stroke::new(2.0, ui::accent(ui).base),
            StrokeKind::Inside,
        );
    }
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    select(ui, &response, layout, emulator);
    if response.hovered() {
        scroll(ui, id, layout, emulator);
    }
    if !focused {
        return (Some(response), Vec::new());
    }
    // Tab, arrows and Escape belong to the terminal, not to moving the focus.
    ui.memory_mut(|memory| {
        memory.set_focus_lock_filter(
            response.id,
            EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            },
        );
    });
    let mode = emulator.mode();
    let mut bytes = Vec::new();
    for event in ui.input(|input| input.events.clone()) {
        let copy = match &event {
            Event::Copy => true,
            Event::Key {
                key: Key::C,
                pressed: true,
                modifiers,
                ..
            } => modifiers.ctrl && modifiers.shift,
            _ => false,
        };
        if copy && let Some(text) = emulator.term.selection_to_string() {
            ui.ctx().copy_text(text);
            emulator.term.selection = None;
            continue;
        }
        if typing {
            bytes.extend(encode(&event, mode));
        }
    }
    if !bytes.is_empty() {
        emulator.term.selection = None;
        emulator.term.scroll_display(Scroll::Bottom);
    }
    (Some(response), bytes)
}

/// The wheel scrolls through the history, a line at a time.
fn scroll(ui: &Ui, id: UiId, layout: Layout, emulator: &mut Emulator) {
    let rows = f32::from(emulator.size().1);
    let delta: f32 = ui.input(|input| {
        (input.events.iter())
            .map(|event| match event {
                Event::MouseWheel { unit, delta, .. } => match unit {
                    MouseWheelUnit::Point => delta.y / layout.cell.y,
                    MouseWheelUnit::Line => delta.y,
                    MouseWheelUnit::Page => delta.y * rows,
                },
                _ => 0.0,
            })
            .sum()
    });
    if delta == 0.0 {
        return;
    }
    let key = id.with("scroll");
    let total = ui.data(|data| data.get_temp::<f32>(key).unwrap_or(0.0)) + delta;
    let lines = total.trunc();
    ui.data_mut(|data| data.insert_temp(key, total - lines));
    if lines != 0.0 {
        emulator.term.scroll_display(Scroll::Delta(lines as i32));
    }
}

/// Drag to select; a click without dragging clears the selection.
fn select(ui: &Ui, response: &Response, layout: Layout, emulator: &mut Emulator) {
    let Some(pos) = response.interact_pointer_pos() else {
        return;
    };
    let (point, side) = layout.point(pos, emulator);
    let term = &mut emulator.term;
    if response.drag_started() {
        let kind = if ui.input(|input| input.modifiers.alt) {
            SelectionType::Block
        } else {
            SelectionType::Simple
        };
        term.selection = Some(Selection::new(kind, point, side));
    } else if response.dragged() {
        if let Some(selection) = &mut term.selection {
            selection.update(point, side);
        }
    } else if response.double_clicked() {
        term.selection = Some(Selection::new(SelectionType::Semantic, point, side));
    } else if response.clicked() {
        term.selection = None;
    }
}

/// The bytes a terminal sends for a key, typed text or a paste.
pub fn encode(event: &Event, mode: TermMode) -> Vec<u8> {
    match event {
        Event::Text(text) => text.as_bytes().to_vec(),
        Event::Paste(text) => paste(text, mode),
        // The window turns Ctrl+C and Ctrl+X into these; without a selection they are
        // the usual interrupt and cut keys.
        Event::Copy => vec![0x03],
        Event::Cut => vec![0x18],
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => key_bytes(*key, *modifiers, mode),
        _ => Vec::new(),
    }
}

/// Pasted text, marked as a paste when the program asked for that. Escape characters
/// are dropped so a paste can't end the marking early.
fn paste(text: &str, mode: TermMode) -> Vec<u8> {
    let text: String = text
        .chars()
        .filter(|c| *c != '\x1b')
        .collect::<String>()
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

fn key_bytes(key: Key, modifiers: Modifiers, mode: TermMode) -> Vec<u8> {
    let app = mode.contains(TermMode::APP_CURSOR);
    // xterm's modifier number: 1, plus 1 for Shift, 2 for Alt and 4 for Ctrl.
    let number =
        1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.ctrl);
    let csi = |end: char| -> Vec<u8> {
        match (number, app) {
            (1, true) => format!("\x1bO{end}"),
            (1, false) => format!("\x1b[{end}"),
            _ => format!("\x1b[1;{number}{end}"),
        }
        .into_bytes()
    };
    let tilde = |code: u8| -> Vec<u8> {
        if number == 1 {
            format!("\x1b[{code}~")
        } else {
            format!("\x1b[{code};{number}~")
        }
        .into_bytes()
    };
    let function = |end: char| -> Vec<u8> {
        if number == 1 {
            format!("\x1bO{end}")
        } else {
            format!("\x1b[1;{number}{end}")
        }
        .into_bytes()
    };
    match key {
        Key::Enter => vec![b'\r'],
        Key::Tab if modifiers.shift => b"\x1b[Z".to_vec(),
        Key::Tab => vec![b'\t'],
        Key::Backspace if modifiers.ctrl => vec![0x08],
        Key::Backspace => vec![0x7f],
        Key::Escape => vec![0x1b],
        Key::ArrowUp => csi('A'),
        Key::ArrowDown => csi('B'),
        Key::ArrowRight => csi('C'),
        Key::ArrowLeft => csi('D'),
        Key::Home => csi('H'),
        Key::End => csi('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => function('P'),
        Key::F2 => function('Q'),
        Key::F3 => function('R'),
        Key::F4 => function('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        // With Ctrl, letters and a few signs give control characters; typed text
        // doesn't come with Ctrl held.
        _ if modifiers.ctrl && !modifiers.alt => {
            let code = match key {
                Key::Space | Key::Num2 => 0,
                Key::OpenBracket => 0x1b,
                Key::Backslash => 0x1c,
                Key::CloseBracket => 0x1d,
                Key::Num6 => 0x1e,
                Key::Slash | Key::Minus => 0x1f,
                _ => match key.name().as_bytes() {
                    [letter @ b'A'..=b'Z'] => letter - b'@',
                    _ => return Vec::new(),
                },
            };
            vec![code]
        }
        // Alt and a letter send Escape first; the letter follows as typed text.
        _ if modifiers.alt && !modifiers.ctrl && !modifiers.mac_cmd => {
            if key.name().len() == 1 {
                vec![0x1b]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// A terminal tile's look before anything is drawn in it: an icon and a short line.
pub fn placeholder(ui: &Ui, rect: Rect, text: &str) {
    let c = ui::colors(ui);
    let painter = ui.painter();
    painter.text(
        rect.center() - vec2(0.0, 10.0),
        Align2::CENTER_CENTER,
        ICON,
        FontId::proportional(ui::TEXT_ICON_LARGE),
        c.muted,
    );
    painter.text(
        rect.center() + vec2(0.0, 16.0),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(ui::TEXT_SMALL),
        c.muted,
    );
}

/// The terminal's icon on buttons and tiles.
pub const ICON: &str = "🖳";

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(emulator: &Emulator) -> Vec<(i32, usize, char, Color, Color, Flags)> {
        let grid = emulator.term.grid();
        let history = grid.history_size().min(SNAPSHOT_HISTORY) as i32;
        let mut cells = Vec::new();
        for line in -history..grid.screen_lines() as i32 {
            for col in 0..grid.columns() {
                let cell = &grid[Line(line)][Column(col)];
                cells.push((line, col, cell.c, cell.fg, cell.bg, cell.flags));
            }
        }
        cells
    }

    fn round_trip(emulator: &Emulator) -> Emulator {
        let (cols, rows) = emulator.size();
        let mut copy = Emulator::new(cols, rows);
        copy.feed(&emulator.snapshot().0);
        assert_eq!(cells(&copy), cells(emulator));
        assert_eq!(
            copy.term.grid().cursor.point,
            emulator.term.grid().cursor.point
        );
        assert_eq!(copy.mode(), emulator.mode());
        copy
    }

    #[test]
    fn a_snapshot_draws_the_same_screen() {
        let mut emulator = Emulator::new(20, 6);
        // Colours, styles, a wide letter, a long line that wraps and enough lines to
        // scroll some into the history.
        emulator.feed(b"\x1b[1;31mred\x1b[0m plain \x1b[4;38;5;200mpink\x1b[0m\r\n");
        emulator.feed("wide \u{4e16} e\u{301}\r\n".as_bytes());
        emulator.feed(b"\x1b[48;2;1;2;3m bg \x1b[0m\x1b[7minv\x1b[0m\x1b[9mx\x1b[2;3mdim\r\n");
        emulator.feed(b"0123456789abcdefghijKLMNOPQRSTUVWXYZ\r\n");
        for line in 0..8 {
            emulator.feed(format!("line {line}\r\n").as_bytes());
        }
        emulator.feed(b"\x1b[32m$ \x1b[0mtyping\x1b[?2004h\x1b[?1h");
        assert!(emulator.term.grid().history_size() > 0);
        let mut copy = round_trip(&emulator);
        // What comes next lands the same on both.
        for screen in [&mut emulator, &mut copy] {
            screen.feed(b" more\r\nnext");
        }
        assert_eq!(cells(&copy), cells(&emulator));
    }

    #[test]
    fn a_snapshot_keeps_a_pending_wrap_and_a_hidden_cursor() {
        let mut emulator = Emulator::new(10, 3);
        emulator.feed(b"\x1b[?25l0123456789");
        assert!(emulator.term.grid().cursor.input_needs_wrap);
        let mut copy = round_trip(&emulator);
        assert!(copy.term.grid().cursor.input_needs_wrap);
        for screen in [&mut emulator, &mut copy] {
            screen.feed(b"x");
        }
        assert_eq!(cells(&copy), cells(&emulator));
    }

    #[test]
    fn a_very_colourful_screen_still_fits_a_snapshot() {
        let mut emulator = Emulator::with(500, 200, Listener(None));
        // Every cell its own colour, on the screen and in the history.
        for line in 0..400u32 {
            for col in 0..500u32 {
                let (r, g, b) = (line % 256, col % 256, (line + col) % 256);
                let cell = format!("\x1b[38;2;{r};{g};{b};48;2;{b};{g};{r}m{}", col % 10);
                emulator.feed(cell.as_bytes());
            }
        }
        assert!(emulator.draw(SNAPSHOT_HISTORY, false).len() > MAX_SNAPSHOT);
        let (screen, _) = emulator.snapshot();
        assert!(screen.len() <= MAX_SNAPSHOT);
        // The screen's text is all there, even if its colours aren't.
        let mut copy = Emulator::new(500, 200);
        copy.feed(&screen);
        let text = |emulator: &Emulator| -> String {
            let grid = emulator.term.grid();
            (0..200)
                .flat_map(|line| (0..500).map(move |col| grid[Line(line)][Column(col)].c))
                .collect()
        };
        assert_eq!(text(&copy), text(&emulator));
    }

    #[test]
    fn a_viewer_takes_only_a_few_combining_marks_per_letter() {
        let mut viewer = Emulator::new(20, 3);
        let marks = "\u{301}".repeat(1000);
        // Escape sequences in between don't start the count again.
        viewer.feed(format!("e{marks}\x1b[0m{marks}").as_bytes());
        let cell = &viewer.term.grid()[Line(0)][Column(0)];
        assert_eq!(cell.zerowidth().unwrap().len(), usize::from(MAX_MARKS));
        // A letter cut between two pieces of output still arrives whole.
        let bytes = "xé\u{301}".as_bytes();
        viewer.feed(&bytes[..2]);
        viewer.feed(&bytes[2..]);
        let grid = viewer.term.grid();
        assert_eq!(grid[Line(0)][Column(2)].c, 'é');
        assert_eq!(grid[Line(0)][Column(2)].zerowidth(), Some(&['\u{301}'][..]));
    }

    #[test]
    fn a_full_screen_program_sends_its_screen() {
        let mut emulator = Emulator::new(20, 4);
        emulator.feed(b"prompt\r\n\x1b[?1049h\x1b[2;3Hin vim");
        let (bytes, alt) = emulator.snapshot();
        assert!(alt);
        let mut copy = Emulator::new(20, 4);
        copy.feed(&bytes);
        assert_eq!(cells(&copy), cells(&emulator));
        assert!(copy.mode().contains(TermMode::ALT_SCREEN));
    }

    #[test]
    fn keys_become_terminal_bytes() {
        let plain = TermMode::default();
        let key = |key, modifiers| {
            encode(
                &Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                plain,
            )
        };
        assert_eq!(key(Key::Enter, Modifiers::NONE), b"\r");
        assert_eq!(key(Key::C, Modifiers::CTRL), [0x03]);
        assert_eq!(key(Key::ArrowUp, Modifiers::NONE), b"\x1b[A");
        assert_eq!(key(Key::ArrowLeft, Modifiers::CTRL), b"\x1b[1;5D");
        assert_eq!(key(Key::Delete, Modifiers::NONE), b"\x1b[3~");
        assert_eq!(key(Key::F5, Modifiers::SHIFT), b"\x1b[15;2~");
        assert_eq!(key(Key::Tab, Modifiers::SHIFT), b"\x1b[Z");
        assert_eq!(key(Key::B, Modifiers::ALT), [0x1b]);
        // Plain letters come as text, not as keys.
        assert!(key(Key::A, Modifiers::NONE).is_empty());
        let app = TermMode::APP_CURSOR;
        let up = Event::Key {
            key: Key::ArrowUp,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert_eq!(encode(&up, app), b"\x1bOA");
        assert_eq!(encode(&Event::Text("é".into()), plain), "é".as_bytes());
        let pasted = Event::Paste("a\nb\x1b[201~".into());
        assert_eq!(encode(&pasted, plain), b"a\rb[201~");
        assert_eq!(
            encode(&pasted, TermMode::BRACKETED_PASTE),
            b"\x1b[200~a\rb[201~\x1b[201~"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_runs_in_the_pseudo_terminal() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "printf hello"]);
        assert_output(command, "hello");
    }

    #[cfg(windows)]
    #[test]
    fn a_shell_runs_in_the_pseudo_terminal() {
        let mut command = CommandBuilder::new("cmd.exe");
        command.args(["/c", "echo hello"]);
        assert_output(command, "hello");
    }

    fn assert_output(command: CommandBuilder, expected: &str) {
        let local = Local::spawn(vec![command], 40, 5, || {}).unwrap();
        let started = std::time::Instant::now();
        let screen = loop {
            let inner = local.shared.lock();
            let grid = inner.emulator.term.grid();
            let text: String = (0..grid.columns())
                .map(|col| grid[Line(0)][Column(col)].c)
                .collect();
            if text.contains(expected) {
                break text;
            }
            drop(inner);
            assert!(started.elapsed().as_secs() < 10, "no output");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(screen.contains(expected), "{screen:?}");
        let (outs, _) = local.shared.lock().take();
        assert!(outs.iter().any(|out| matches!(out, Out::Bytes(_))));
    }
}
