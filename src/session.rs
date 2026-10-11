//! One wrapped shell: the event loop between the user's terminal and the shell's PTY.
//!
//! Bytes flow straight through in both directions. The session watches the shell's output for
//! integration messages, asks the engine for suggestions off-thread, and overlays the popup.
//!
//! Two invariants keep the overlay from corrupting the terminal:
//!
//! - The popup is only ever written while the shell's output stream is between escape
//!   sequences ([`Scanner::is_ground`]).
//! - The popup is taken off the screen before any further shell output is forwarded, so the
//!   real terminal always equals the model plus, at most, one popup.
//!
//! Passing bytes through is the part that must never fail. Everything else, the screen model
//! and the popup, runs behind [`Session::guarded`]: if it panics the overlay resets itself and
//! the shell carries on.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::errno::Errno;
use nix::sys::select::{FdSet, select};
use nix::sys::signal::{Signal, kill};
use nix::sys::time::TimeVal;
use nix::unistd::Pid;
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGTERM, SIGWINCH};

use crate::engine::spec::SpecStore;
use crate::engine::worker::Worker;
use crate::engine::{Completion, Request};
use crate::popup::{self, Frame, Geometry};
use crate::protocol::{self, Message};
use crate::pty::{self, RawMode};
use crate::quiet;
use crate::scan::Scanner;
use crate::shell::{self, ZshShim};
use crate::trace::Trace;

/// Set in the shell's environment. Its presence means "already inside a session", and its
/// value is the token the shell's messages must carry; see [`crate::protocol`].
pub const SESSION_VAR: &str = "QUINCE_SESSION";

/// Asks the terminal to hold rendering until the matching end, so a popup update is one frame.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// zsh keymaps in which text is being typed. In the others, vi command mode for one, keys are
/// commands and the popup stays out of the way.
const TYPING_KEYMAPS: &[&str] = &["main", "emacs", "viins"];

/// Screen size assumed when the terminal reports none.
const FALLBACK_SIZE: (u16, u16) = (24, 80);

fn new_model(rows: u16, cols: u16) -> vt100::Parser {
    let (fallback_rows, fallback_cols) = FALLBACK_SIZE;
    vt100::Parser::new(
        if rows == 0 { fallback_rows } else { rows },
        if cols == 0 { fallback_cols } else { cols },
        0,
    )
}

pub struct Options {
    pub shell: PathBuf,
    /// Start the shell as a login shell.
    pub login: bool,
    pub args: Vec<OsString>,
    /// The wrapper was started by a shell's startup file to take that shell's place, rather
    /// than by the user to get a shell within a shell.
    pub in_place: bool,
}

/// Runs `options.shell` under the wrapper until it exits and returns its exit code.
pub fn run(options: &Options) -> io::Result<i32> {
    let (stdin, stdout) = (io::stdin(), io::stdout());
    let size = pty::winsize(stdout.as_fd())?;

    let token = new_token()?;
    let mut command = Command::new(&options.shell);
    command.args(&options.args).env(SESSION_VAR, &token);
    if options.in_place
        && let Some(level) = std::env::var("SHLVL").ok().and_then(|level| level.parse::<i64>().ok())
    {
        // The shell that started us has counted itself; the one we start counts again.
        command.env("SHLVL", (level - 1).max(0).to_string());
    }
    if options.login {
        let name = options
            .shell
            .file_name()
            .unwrap_or(options.shell.as_os_str())
            .to_string_lossy();
        command.arg0(format!("-{name}"));
    }
    let shim = if shell::is_zsh(&options.shell) {
        Some(ZshShim::install(&mut command)?)
    } else {
        None
    };

    // Raw mode first: the shell's terminal starts out with the settings ours had.
    let raw = RawMode::enter(stdin.as_fd())?;
    let (master, mut child) = pty::spawn(command, &size, Some(raw.original()))?;
    pty::set_nonblocking(master.as_fd())?;

    let (wake_read, wake_write) = nix::unistd::pipe()?;
    pty::set_nonblocking(wake_read.as_fd())?;
    pty::set_nonblocking(wake_write.as_fd())?;
    let signals = Signals::install(&wake_write)?;
    let worker = Worker::spawn(SpecStore::default_dirs(), wake_write)?;

    let mut session = Session::new(master, worker, token, size.ws_row, size.ws_col);
    session.shim = shim;
    let result = session.run(&mut child, stdin.as_fd(), stdout.as_fd(), wake_read.as_fd(), &signals);
    drop(raw);
    result.map(exit_code)
}

/// A secret for one session, unguessable by anything that can only write to the terminal.
fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Signals are turned into flags plus a byte on the wake pipe, so the loop handles them like
/// any other event.
struct Signals {
    resized: Arc<AtomicBool>,
    child: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
}

impl Signals {
    fn install(wake: &OwnedFd) -> io::Result<Signals> {
        let signals = Signals {
            resized: Arc::default(),
            child: Arc::default(),
            quit: Arc::default(),
        };
        for (signal, flag) in [
            (SIGWINCH, &signals.resized),
            (SIGCHLD, &signals.child),
            (SIGTERM, &signals.quit),
            (SIGHUP, &signals.quit),
        ] {
            signal_hook::flag::register(signal, flag.clone())?;
            signal_hook::low_level::pipe::register(signal, wake.try_clone()?)?;
        }
        Ok(signals)
    }
}

/// Keys the popup takes for itself while it is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Tab,
    Enter,
    Escape,
}

impl Key {
    /// Only a read that is exactly one key counts; anything else, a paste for instance, goes
    /// to the shell untouched.
    fn parse(input: &[u8]) -> Option<Key> {
        match input {
            b"\x1b[A" | b"\x1bOA" => Some(Key::Up),
            b"\x1b[B" | b"\x1bOB" => Some(Key::Down),
            b"\t" => Some(Key::Tab),
            b"\r" => Some(Key::Enter),
            b"\x1b" => Some(Key::Escape),
            _ => None,
        }
    }
}

struct Session {
    master: OwnedFd,
    worker: Worker,
    /// What a message must carry to count as coming from the shell.
    token: String,
    scanner: Scanner,
    /// The startup files the shell is reading, kept only until it has read them.
    shim: Option<ZshShim>,
    /// The terminal as the shell has drawn it, without the popup.
    model: vt100::Parser,
    /// The terminal's size as it reports it, rows then columns.
    size: (u16, u16),
    /// The model was rebuilt after a failure and cannot be trusted until the next prompt.
    suspended: bool,
    /// Bytes on their way to the terminal.
    out: Vec<u8>,
    /// Whether `out` contains popup drawing, and so wants presenting as a single frame.
    popup_touched: bool,
    /// Keystrokes on their way to the shell.
    to_shell: VecDeque<u8>,
    trace: Trace,

    /// The shell's line editor is active.
    editing: bool,
    buffer: String,
    /// Byte offset of the cursor in `buffer`.
    cursor: usize,
    cwd: PathBuf,
    /// Keys have gone to the shell since its last report, so `buffer` may be out of date.
    unconfirmed_input: bool,

    /// Sequence number of the latest request; a completion with any other is stale.
    seq: u64,
    completion: Completion,
    selected: Option<usize>,
    scroll: usize,
    /// The user closed the popup for the current line contents.
    dismissed: bool,
    /// Where the popup currently is on screen.
    drawn: Option<Geometry>,
}

impl Session {
    fn new(master: OwnedFd, worker: Worker, token: String, rows: u16, cols: u16) -> Session {
        Session {
            master,
            worker,
            token,
            scanner: Scanner::new(),
            shim: None,
            model: new_model(rows, cols),
            size: (rows, cols),
            suspended: false,
            out: Vec::new(),
            popup_touched: false,
            trace: Trace::from_env(),
            to_shell: VecDeque::new(),
            editing: false,
            buffer: String::new(),
            cursor: 0,
            cwd: std::env::current_dir().unwrap_or_default(),
            unconfirmed_input: false,
            seq: 0,
            completion: Completion::default(),
            selected: None,
            scroll: 0,
            dismissed: false,
            drawn: None,
        }
    }

    fn run(
        &mut self,
        child: &mut Child,
        stdin: BorrowedFd,
        stdout: BorrowedFd,
        wake: BorrowedFd,
        signals: &Signals,
    ) -> io::Result<ExitStatus> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let (input_ready, output_ready, shell_writable, woken) = {
                let mut readable = FdSet::new();
                readable.insert(stdin);
                readable.insert(self.master.as_fd());
                readable.insert(wake);
                let mut writable = FdSet::new();
                if !self.to_shell.is_empty() {
                    writable.insert(self.master.as_fd());
                }
                match select(
                    None::<libc::c_int>,
                    &mut readable,
                    &mut writable,
                    None::<&mut FdSet>,
                    None::<&mut TimeVal>,
                ) {
                    Ok(_) => {}
                    Err(Errno::EINTR) => continue,
                    Err(errno) => return Err(errno.into()),
                }
                (
                    readable.contains(stdin),
                    readable.contains(self.master.as_fd()),
                    writable.contains(self.master.as_fd()),
                    readable.contains(wake),
                )
            };

            let mut shell_gone = false;
            if woken {
                while matches!(nix::unistd::read(wake, &mut buf), Ok(n) if n > 0) {}
                if signals.quit.swap(false, Ordering::Relaxed) {
                    self.guarded(Session::erase);
                    self.flush(stdout, true)?;
                    let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGHUP);
                    return child.wait();
                }
                if signals.resized.swap(false, Ordering::Relaxed) {
                    self.resize(stdout)?;
                }
                while let Ok(completion) = self.worker.results.try_recv() {
                    self.on_completion(completion, stdout)?;
                }
                // Background jobs can keep the PTY open after the shell itself is gone.
                shell_gone = signals.child.swap(false, Ordering::Relaxed) && child.try_wait()?.is_some();
            }
            if output_ready || shell_gone {
                // Once the shell has exited, read out whatever it left behind and stop.
                loop {
                    match nix::unistd::read(&self.master, &mut buf) {
                        Ok(0) | Err(Errno::EIO) => return child.wait(),
                        Ok(n) => self.on_output(&buf[..n], stdout)?,
                        Err(Errno::EAGAIN) if shell_gone => return child.wait(),
                        Err(Errno::EAGAIN | Errno::EINTR) => {}
                        Err(errno) => return Err(errno.into()),
                    }
                    if !shell_gone {
                        break;
                    }
                }
            }
            if input_ready {
                match nix::unistd::read(stdin, &mut buf) {
                    Ok(0) => {
                        // The terminal went away.
                        let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGHUP);
                        return child.wait();
                    }
                    Ok(n) => self.on_input(&buf[..n], stdout)?,
                    Err(Errno::EAGAIN | Errno::EINTR) => {}
                    Err(errno) => return Err(errno.into()),
                }
            }
            if shell_writable || !self.to_shell.is_empty() {
                self.pump_shell()?;
            }
        }
    }

    /// Output from the shell: forward it, keep the model in step, act on any messages in it.
    fn on_output(&mut self, chunk: &[u8], stdout: BorrowedFd) -> io::Result<()> {
        self.trace.record("from-shell", chunk);
        let ground_before = self.scanner.is_ground();
        let (mut forward, mut messages) = (Vec::with_capacity(chunk.len()), Vec::new());
        self.scanner.feed(chunk, &mut forward, &mut messages);

        self.guarded(Session::erase);
        self.out.extend_from_slice(&forward);
        self.guarded(|session| {
            session.model.process(&forward);
            for payload in &messages {
                if let Some(message) = protocol::parse(payload, &session.token) {
                    session.on_message(message);
                }
            }
            session.draw();
        });
        self.flush(stdout, ground_before)
    }

    /// Runs overlay work so that a panic in it cannot take the shell down. The screen model is
    /// third-party code fed whatever programs print, and none of the overlay is needed to keep
    /// the shell usable. After a failure the model starts over and the popup stays off until
    /// the next prompt, when the cursor is back at a known column.
    fn guarded<R>(&mut self, work: impl FnOnce(&mut Session) -> R) -> Option<R> {
        quiet::set(true);
        let result = panic::catch_unwind(AssertUnwindSafe(|| work(self)));
        quiet::set(false);
        if result.is_err() {
            if let Some(geometry) = self.drawn.take() {
                popup::blank(&mut self.out, geometry);
                self.popup_touched = true;
            }
            self.model = new_model(self.size.0, self.size.1);
            self.suspended = true;
            self.leave_editor();
        }
        result.ok()
    }

    fn on_message(&mut self, message: Message) {
        match message {
            Message::Cwd(path) => {
                // Sent just before a prompt is printed, which starts at the left margin.
                self.suspended = false;
                self.cwd = path.into();
                // A prompt also means the shell is through its startup files.
                self.shim = None;
            }
            Message::Environment(environment) => self.worker.environment(environment),
            Message::Exec | Message::Finish => self.leave_editor(),
            Message::Buffer { text, cursor, keymap } => {
                let cursor = text.char_indices().nth(cursor).map_or(text.len(), |(offset, _)| offset);
                let changed = text != self.buffer || cursor != self.cursor;
                self.editing = TYPING_KEYMAPS.contains(&keymap.as_str());
                self.unconfirmed_input = false;
                self.buffer = text;
                self.cursor = cursor;
                if changed {
                    self.dismissed = false;
                    self.request();
                }
            }
        }
    }

    fn leave_editor(&mut self) {
        self.editing = false;
        self.dismissed = false;
        self.completion.items.clear();
    }

    fn request(&mut self) {
        self.seq += 1;
        self.selected = None;
        self.scroll = 0;
        if self.buffer.trim().is_empty() {
            self.completion = Completion {
                seq: self.seq,
                ..Completion::default()
            };
            return;
        }
        self.worker.request(Request {
            seq: self.seq,
            buffer: self.buffer.clone(),
            cursor: self.cursor,
            cwd: self.cwd.clone(),
        });
    }

    /// An answer from the engine. A request may be answered more than once: first with what
    /// the spec spells out, then again as generators add to it.
    fn on_completion(&mut self, completion: Completion, stdout: BorrowedFd) -> io::Result<()> {
        if completion.seq == self.seq {
            // The list may have grown around the highlighted row; keep hold of the row.
            let highlighted = self.selected.and_then(|index| self.completion.items.get(index));
            self.selected = highlighted.and_then(|row| completion.items.iter().position(|item| item == row));
            self.completion = completion;
            self.redraw(stdout)?;
        }
        Ok(())
    }

    /// Input from the user. Everything goes to the shell except keys the popup claims.
    fn on_input(&mut self, input: &[u8], stdout: BorrowedFd) -> io::Result<()> {
        self.trace.record("keys", input);
        // The popup only acts on a line it is sure about: its suggestions must be for the
        // line as the shell last reported it, with nothing typed since.
        let popup_is_current = self.drawn.is_some() && self.completion.seq == self.seq && !self.unconfirmed_input;
        if popup_is_current && let Some(key) = Key::parse(input) {
            let taken = self.guarded(|session| session.on_popup_key(key)) == Some(true);
            if taken || self.dismissed {
                self.redraw(stdout)?;
            }
            if taken {
                return Ok(());
            }
        }

        // Do not wait for the shell to say so when a key is about to end the line.
        if input.iter().any(|byte| matches!(byte, b'\r' | b'\n' | 0x03 | 0x04)) {
            self.leave_editor();
            self.redraw(stdout)?;
        }
        self.unconfirmed_input = true;
        self.to_shell.extend(input);
        Ok(())
    }

    /// Returns false for a key the popup leaves to the shell.
    fn on_popup_key(&mut self, key: Key) -> bool {
        let count = self.completion.items.len();
        match (key, self.selected) {
            (Key::Down, None) => self.selected = Some(0),
            (Key::Down, Some(index)) => self.selected = Some((index + 1) % count),
            // With nothing selected, Up and Enter keep their usual meaning in the shell.
            (Key::Up | Key::Enter, None) => return false,
            (Key::Up, Some(0)) => self.selected = None,
            (Key::Up, Some(index)) => self.selected = Some(index - 1),
            // With nothing typed to match against, the first row of a list known to be missing
            // entries is an arbitrary pick. The shell's own completion can supply what the
            // engine could not, so Tab goes to it, and the popup gets out of the way of
            // whatever it shows.
            (Key::Tab, None) if self.completion.incomplete && self.completion.token.is_empty() => {
                self.dismissed = true;
                return false;
            }
            (Key::Tab, selected) => self.accept(selected.unwrap_or(0)),
            (Key::Enter, Some(index)) => self.accept(index),
            (Key::Escape, _) => self.dismissed = true,
        }
        true
    }

    /// Asks the shell to put the chosen suggestion in place of the token under the cursor. The
    /// shell makes the edit only if its line is still the one the suggestion was computed for.
    fn accept(&mut self, index: usize) {
        let item = &self.completion.items[index];
        let left = &self.buffer[..self.cursor];
        let Some(before) = left.strip_suffix(self.completion.token.as_str()) else {
            return;
        };
        let insert: String = item.insert.chars().filter(|c| !c.is_control()).collect();
        let request = protocol::accept_request(
            &self.buffer,
            left.chars().count(),
            &format!("{before}{insert}"),
            item.cursor_back,
        );
        self.to_shell.extend(request);
        // Nothing more is taken from this popup until the shell reports the result.
        self.unconfirmed_input = true;
    }

    fn resize(&mut self, stdout: BorrowedFd) -> io::Result<()> {
        let size = pty::winsize(stdout)?;
        pty::set_winsize(self.master.as_fd(), &size)?;
        self.size = (size.ws_row, size.ws_col);
        // The terminal has reflowed, so where the popup ended up is unknown. The shell repaints
        // from its prompt down on a resize, which clears it.
        self.drawn = None;
        self.guarded(|session| {
            let (rows, cols) = new_model(session.size.0, session.size.1).screen().size();
            session.model.screen_mut().set_size(rows, cols);
        });
        Ok(())
    }

    fn wants_popup(&self) -> bool {
        self.editing
            && !self.suspended
            && !self.dismissed
            && !self.completion.items.is_empty()
            && !self.model.screen().alternate_screen()
    }

    // `erase` and `draw` build their bytes on the side and append them in one go, so a failure
    // part-way through never leaves half a popup in `out`.

    fn erase(&mut self) {
        if let Some(geometry) = self.drawn {
            let mut bytes = Vec::new();
            popup::erase(&mut bytes, geometry, self.model.screen());
            self.out.append(&mut bytes);
            self.drawn = None;
            self.popup_touched = true;
        }
    }

    fn draw(&mut self) {
        if !self.wants_popup() || !self.scanner.is_ground() {
            return;
        }
        let (rows, cols) = self.model.screen().size();
        let (_, cursor_col) = self.model.screen().cursor_position();
        let token_width = popup::display_width(&self.completion.token);
        let Some(geometry) = popup::layout(&self.completion.items, token_width, cursor_col, rows, cols) else {
            return;
        };

        // Keep the selection inside the visible window.
        let visible = usize::from(geometry.height);
        if let Some(selected) = self.selected {
            self.scroll = self.scroll.clamp(selected.saturating_sub(visible - 1), selected);
        }
        self.scroll = self.scroll.min(self.completion.items.len().saturating_sub(visible));

        let mut bytes = Vec::new();
        popup::reserve(&mut bytes, geometry.height);
        self.model.process(&bytes);
        let frame = Frame {
            items: &self.completion.items,
            selected: self.selected,
            scroll: self.scroll,
        };
        popup::draw(&mut bytes, geometry, &frame, self.model.screen());
        self.out.append(&mut bytes);
        self.drawn = Some(geometry);
        self.popup_touched = true;
    }

    /// Brings the popup on screen in line with the current state, outside of shell output.
    fn redraw(&mut self, stdout: BorrowedFd) -> io::Result<()> {
        self.guarded(|session| {
            session.erase();
            session.draw();
        });
        self.flush(stdout, true)
    }

    /// Writes `out` to the terminal. `ground_before` says whether the first byte of `out`
    /// starts on a sequence boundary; only then, and if it also ends on one, can the whole
    /// write be bracketed as a single frame.
    fn flush(&mut self, stdout: BorrowedFd, ground_before: bool) -> io::Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        if std::mem::take(&mut self.popup_touched) && ground_before && self.scanner.is_ground() {
            self.out.splice(0..0, SYNC_BEGIN.iter().copied());
            self.out.extend_from_slice(SYNC_END);
        }
        self.trace.record("to-terminal", &self.out);
        let result = write_all(stdout, &self.out);
        self.out.clear();
        result
    }

    /// Hands queued keystrokes to the shell, as many as it will take right now.
    fn pump_shell(&mut self) -> io::Result<()> {
        while !self.to_shell.is_empty() {
            let (front, _) = self.to_shell.as_slices();
            match nix::unistd::write(&self.master, front) {
                Ok(written) => {
                    self.trace.record("to-shell", &front[..written]);
                    self.to_shell.drain(..written);
                }
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => {}
                Err(errno) => return Err(errno.into()),
            }
        }
        Ok(())
    }
}

fn write_all(fd: BorrowedFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match nix::unistd::write(fd, bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::EINTR | Errno::EAGAIN) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::Path;

    use super::*;
    use crate::engine::{Kind, Suggestion};

    /// A session wired to files instead of a shell and a terminal. Whatever it would have
    /// written to the terminal ends up in `terminal`.
    struct Harness {
        session: Session,
        terminal: File,
        path: PathBuf,
    }

    impl Harness {
        fn new(name: &str, rows: u16, cols: u16) -> Harness {
            let path = std::env::temp_dir().join(format!("quince-session-{name}-{}", std::process::id()));
            let terminal = File::create(&path).unwrap();
            let (_wake_read, wake_write) = nix::unistd::pipe().unwrap();
            let worker = Worker::spawn(Vec::new(), wake_write).unwrap();
            let shell = File::options().write(true).open("/dev/null").unwrap();
            Harness {
                session: Session::new(shell.into(), worker, "t0ken".into(), rows, cols),
                terminal,
                path,
            }
        }

        fn shell_prints(&mut self, bytes: &[u8]) {
            self.session.on_output(bytes, self.terminal.as_fd()).unwrap();
        }

        /// Reports `line` as the edit buffer and answers the resulting request with `labels`.
        fn suggest(&mut self, line: &str, token: &str, labels: &[&str]) {
            self.shell_prints(format!("\x1b]6973;t0ken;B;{};main;{line}\x07", line.chars().count()).as_bytes());
            let items = labels
                .iter()
                .map(|label| Suggestion {
                    label: (*label).to_owned(),
                    description: String::new(),
                    insert: format!("{label} "),
                    cursor_back: 0,
                    kind: Kind::Subcommand,
                })
                .collect();
            let completion = Completion {
                seq: self.session.seq,
                token: token.to_owned(),
                incomplete: false,
                items,
            };
            self.session.on_completion(completion, self.terminal.as_fd()).unwrap();
        }

        fn written(&self) -> Vec<u8> {
            std::fs::read(&self.path).unwrap()
        }

        /// The terminal's screen after everything written so far.
        fn screen(&self, rows: u16, cols: u16) -> Vec<String> {
            let mut parser = vt100::Parser::new(rows, cols, 0);
            parser.process(&self.written());
            parser
                .screen()
                .rows(0, cols)
                .map(|row| row.trim_end().to_owned())
                .collect()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn shows_the_popup_for_the_reported_line() {
        let mut harness = Harness::new("popup", 10, 40);
        harness.shell_prints(b"\x1b]6973;t0ken;P;/tmp\x07$ git ch");
        harness.suggest("git ch", "ch", &["checkout", "cherry-pick"]);

        assert!(harness.session.drawn.is_some());
        let screen = harness.screen(10, 40);
        assert_eq!(screen[..3], ["$ git ch", "      checkout", "      cherry-pick"]);
        // The private messages never reach the terminal.
        assert!(!String::from_utf8_lossy(&harness.written()).contains("6973"));
        assert_eq!(harness.session.cwd, Path::new("/tmp"));
    }

    #[test]
    fn shell_output_passes_through_whatever_the_overlay_does() {
        // A one-column terminal is more than the screen model can take.
        let mut harness = Harness::new("tiny", 24, 1);
        let output = "$ 日本語 wide text\r\n\x1b[1;31mred\x1b[0m\r\n".repeat(50);
        for chunk in output.as_bytes().chunks(7) {
            harness.shell_prints(chunk);
        }
        assert_eq!(harness.written(), output.as_bytes());
    }

    #[test]
    fn an_unknown_terminal_size_falls_back() {
        let harness = Harness::new("unsized", 0, 0);
        assert_eq!(harness.session.model.screen().size(), FALLBACK_SIZE);
    }

    #[test]
    fn a_failing_overlay_cleans_up_and_resumes_at_the_next_prompt() {
        let mut harness = Harness::new("recover", 10, 40);
        harness.shell_prints(b"\x1b]6973;t0ken;P;/tmp\x07$ git ch");
        harness.suggest("git ch", "ch", &["checkout", "cherry-pick"]);
        assert!(harness.session.drawn.is_some());

        assert_eq!(harness.session.guarded(|_| -> u8 { panic!("the model blew up") }), None);
        harness.session.flush(harness.terminal.as_fd(), true).unwrap();
        assert!(harness.session.drawn.is_none());
        assert_eq!(harness.screen(10, 40)[..3], ["$ git ch", "", ""], "the popup is wiped");

        // The rebuilt model does not know where the cursor is, so nothing is drawn with it.
        harness.suggest("git che", "che", &["checkout", "cherry-pick"]);
        assert!(harness.session.drawn.is_none());

        // A new prompt starts at a known place, and the popup is back.
        harness.shell_prints(b"\r\n\x1b]6973;t0ken;P;/tmp\x07$ git ch");
        harness.suggest("git ch", "ch", &["checkout", "cherry-pick"]);
        assert!(harness.session.drawn.is_some());
        assert_eq!(
            harness.screen(10, 40)[1..4],
            ["$ git ch", "      checkout", "      cherry-pick"]
        );
    }

    #[test]
    fn the_popup_waits_for_a_sequence_boundary() {
        let mut harness = Harness::new("boundary", 10, 40);
        harness.shell_prints(b"\x1b]6973;t0ken;P;/tmp\x07$ git ch");
        harness.suggest("git ch", "ch", &["checkout"]);
        assert!(harness.session.drawn.is_some());

        // Output that stops in the middle of an escape sequence: the popup comes off first and
        // must not be put back until the sequence is complete.
        harness.shell_prints(b"e\x1b[3");
        assert!(harness.session.drawn.is_none());
        let so_far = harness.written();
        assert!(so_far.ends_with(b"e\x1b[3"), "nothing may follow the partial sequence");

        harness.shell_prints(b"2mc");
        assert!(harness.session.drawn.is_some());
        assert_eq!(harness.screen(10, 40)[..2], ["$ git chec", "        checkout"]);
    }

    #[test]
    fn only_a_lone_key_is_a_popup_key() {
        assert_eq!(Key::parse(b"\x1b[B"), Some(Key::Down));
        assert_eq!(Key::parse(b"\x1bOA"), Some(Key::Up));
        assert_eq!(Key::parse(b"\t"), Some(Key::Tab));
        assert_eq!(Key::parse(b"\r"), Some(Key::Enter));
        assert_eq!(Key::parse(b"\x1b"), Some(Key::Escape));
        // Pasted text that happens to contain a tab or a newline is not a key press.
        assert_eq!(Key::parse(b"a\tb"), None);
        assert_eq!(Key::parse(b"ls\r"), None);
        assert_eq!(Key::parse(b"\x1b[B\x1b[B"), None);
        assert_eq!(Key::parse(b""), None);
    }
}
