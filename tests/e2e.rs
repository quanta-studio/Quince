//! End-to-end tests: the real binary wrapping a real zsh, driven through a PTY.
//!
//! The test plays the terminal emulator. It writes keystrokes to the PTY and feeds everything
//! that comes back into a `vt100` screen, which the assertions then read.

use std::fs;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::pty::Winsize;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use quince::pty;
use vt100::Color;

const BIN: &str = env!("CARGO_BIN_EXE_quince");
const TIMEOUT: Duration = Duration::from_secs(10);

/// Where each session's wrapper records what passed through it, inside the test's home.
const TRACE_FILE: &str = ".trace.log";

/// A `.zshrc` that makes for a screen the tests can read.
const PLAIN_RC: &str = "PROMPT='$ '\nunsetopt PROMPT_SP BEEP\n";

/// Background colour of the popup's selected row.
const SELECTED: Color = Color::Idx(25);

fn zsh() -> Option<PathBuf> {
    [
        "/bin/zsh",
        "/usr/bin/zsh",
        "/usr/local/bin/zsh",
        "/opt/homebrew/bin/zsh",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.exists())
}

fn specs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/specs")
}

/// A command with nothing inherited from the developer's environment or dotfiles.
fn isolated(program: &str, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .env("HOME", home)
        .env("ZDOTDIR", home)
        .env("TMPDIR", home.join(".tmp"))
        .env("QUINCE_TRACE", home.join(TRACE_FILE))
        .env("QUINCE_SPECS_DIR", specs_dir())
        .current_dir(home);
    command
}

fn home_dir(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("quince-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(home.join(".tmp")).unwrap();
    // The canonical path is what the shell reports as its working directory.
    home.canonicalize().unwrap()
}

struct Terminal {
    master: OwnedFd,
    child: Child,
    screen: vt100::Parser,
    home: PathBuf,
}

impl Terminal {
    /// Starts the wrapper around `shell` in a terminal of the given size and waits for a prompt.
    fn start(name: &str, shell: &Path, rows: u16, cols: u16) -> Terminal {
        Terminal::launch(name, shell, (rows, cols), (rows, cols), "")
    }

    /// As `start`, for a terminal that tells programs a different size than it really has.
    fn start_reporting(name: &str, shell: &Path, size: (u16, u16), reported: (u16, u16)) -> Terminal {
        Terminal::launch(name, shell, size, reported, "")
    }

    /// As `start`, with extra lines in the user's `.zshrc`.
    fn start_with_rc(name: &str, shell: &Path, rc: &str) -> Terminal {
        Terminal::launch(name, shell, (24, 80), (24, 80), rc)
    }

    fn launch(name: &str, shell: &Path, (rows, cols): (u16, u16), reported: (u16, u16), rc: &str) -> Terminal {
        let home = home_dir(name);
        fs::write(home.join(".zshrc"), format!("{PLAIN_RC}{rc}")).unwrap();
        let mut command = isolated(BIN, &home);
        command.env("SHELL", shell).env("PS1", "$ ");
        let mut terminal = Terminal::open(command, home, (rows, cols), reported);
        terminal.wait_for("the first prompt", |t| t.cursor_row_text() == "$");
        terminal
    }

    /// Opens a terminal the way a terminal emulator does: it runs zsh, not the wrapper. The
    /// home's `.zshrc` has been through `install`, which is what brings the wrapper in.
    fn new_terminal(name: &str, zsh: &Path, zsh_args: &[&str], env: &[(&str, &str)]) -> Terminal {
        let mut terminal = Terminal::new_terminal_running(name, zsh, zsh_args, env);
        terminal.wait_for("the first prompt", |t| t.cursor_row_text() == "$");
        terminal
    }

    /// The prompt of a shell that is not inside a session is showing.
    fn at_plain_prompt(&self) -> bool {
        self.cursor_row_text() == "plain>"
    }

    /// As `new_terminal`, without waiting for a prompt, for a zsh that is not going to show one.
    fn new_terminal_running(name: &str, zsh: &Path, zsh_args: &[&str], env: &[(&str, &str)]) -> Terminal {
        let home = home_dir(name);
        // A shell that is not in a session says so in its prompt, so the tests can tell. Real
        // startup files run programs, and zsh takes over the terminal settings a program
        // leaves behind, so this one runs a program too.
        let rc = format!("{PLAIN_RC}[[ -n ${{QUINCE_SESSION-}} ]] || PROMPT='plain> '\ncommand true\n");
        fs::write(home.join(".zshrc"), rc).unwrap();
        let installed = isolated(BIN, &home).arg("install").output().unwrap();
        assert!(
            installed.status.success(),
            "{}",
            String::from_utf8_lossy(&installed.stderr)
        );
        let mut command = isolated(zsh.to_str().unwrap(), &home);
        command.args(zsh_args).env("SHELL", zsh).envs(env.iter().copied());
        Terminal::open(command, home, (24, 80), (24, 80))
    }

    fn open(command: Command, home: PathBuf, (rows, cols): (u16, u16), reported: (u16, u16)) -> Terminal {
        let size = Winsize {
            ws_row: reported.0,
            ws_col: reported.1,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, child) = pty::spawn(command, &size, None).expect("spawn on a PTY");
        pty::set_nonblocking(master.as_fd()).unwrap();
        Terminal {
            master,
            child,
            screen: vt100::Parser::new(rows, cols, 0),
            home,
        }
    }

    fn send(&mut self, keys: &str) {
        let mut bytes = keys.as_bytes();
        while !bytes.is_empty() {
            match nix::unistd::write(&self.master, bytes) {
                Ok(written) => bytes = &bytes[written..],
                Err(Errno::EAGAIN | Errno::EINTR) => {
                    // The other side may be waiting for us to read before it takes more.
                    self.pump();
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(errno) => panic!("writing to the PTY: {errno}"),
            }
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.report_size(rows, cols);
        self.screen.screen_mut().set_size(rows, cols);
    }

    /// Tells the programs in the terminal it has this size, without changing the screen.
    fn report_size(&mut self, rows: u16, cols: u16) {
        let size = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        pty::set_winsize(self.master.as_fd(), &size).unwrap();
    }

    /// Reads whatever has arrived. Returns false once the other side is gone.
    fn pump(&mut self) -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match nix::unistd::read(&self.master, &mut buf) {
                Ok(0) | Err(Errno::EIO) => return false,
                Ok(n) => self.screen.process(&buf[..n]),
                Err(Errno::EAGAIN) => return true,
                Err(Errno::EINTR) => {}
                Err(errno) => panic!("reading from the PTY: {errno}"),
            }
        }
    }

    fn wait_for(&mut self, what: &str, done: impl Fn(&Terminal) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let alive = self.pump();
            if done(self) {
                return;
            }
            assert!(alive, "the session ended while waiting for {what}; {}", self.dump());
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; {}",
                self.dump()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Keeps reading for `duration`. A terminal never stops reading, and the wrapper, like any
    /// program, is held up when its output has nowhere to go; so the tests must not just sleep.
    fn pump_for(&mut self, duration: Duration) {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            self.pump();
        }
    }

    /// Lets the session settle and checks that `holds` stays true throughout.
    fn stays(&mut self, what: &str, holds: impl Fn(&Terminal) -> bool) {
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            self.pump();
            assert!(holds(self), "{what} did not hold; {}", self.dump());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_exit(mut self) -> i32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().expect("an exit code");
            }
            assert!(Instant::now() < deadline, "timed out waiting for exit; {}", self.dump());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn rows(&self) -> Vec<String> {
        let (_, cols) = self.screen.screen().size();
        self.screen
            .screen()
            .rows(0, cols)
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    fn cursor_row_text(&self) -> String {
        let (row, _) = self.screen.screen().cursor_position();
        self.rows()[usize::from(row)].clone()
    }

    fn contains(&self, text: &str) -> bool {
        self.rows().iter().any(|row| row.contains(text))
    }

    /// The rows directly below the cursor that hold popup text, without their padding.
    fn popup(&self) -> Vec<String> {
        let (cursor_row, _) = self.screen.screen().cursor_position();
        self.rows()
            .into_iter()
            .skip(usize::from(cursor_row) + 1)
            .map(|row| row.trim().to_owned())
            .take_while(|row| !row.is_empty())
            .collect()
    }

    /// Text of the highlighted popup row, if one is highlighted.
    fn selected(&self) -> Option<String> {
        let screen = self.screen.screen();
        let (rows, cols) = screen.size();
        (0..rows).find_map(|row| {
            let highlighted: String = (0..cols)
                .filter_map(|col| screen.cell(row, col))
                .filter(|cell| cell.bgcolor() == SELECTED)
                .map(|cell| cell.contents().to_owned())
                .collect();
            (!highlighted.is_empty()).then(|| highlighted.trim().to_owned())
        })
    }

    /// The screen, with a marker on the cursor's row, and the last of the wrapper's trace: what
    /// a failure message needs to tell a slow test from a wrong byte.
    fn dump(&self) -> String {
        let (cursor_row, cursor_col) = self.screen.screen().cursor_position();
        let rows = self.rows();
        let last = rows
            .iter()
            .rposition(|row| !row.is_empty())
            .unwrap_or(0)
            .max(usize::from(cursor_row));
        let mut text = format!("screen, cursor at row {cursor_row} column {cursor_col}:");
        for (index, row) in rows.iter().enumerate().take(last + 1) {
            let marker = if index == usize::from(cursor_row) { '>' } else { ' ' };
            text.push_str(&format!("\n {marker}|{row}"));
        }
        let trace = fs::read_to_string(self.home.join(TRACE_FILE)).unwrap_or_default();
        let lines: Vec<&str> = trace.lines().collect();
        text.push_str("\nlast bytes through the wrapper (microseconds, where, what):");
        for line in &lines[lines.len().saturating_sub(25)..] {
            let shown: String = line.chars().take(200).collect();
            text.push_str(&format!("\n  {shown}"));
        }
        text
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Hang up rather than kill, as a closing terminal would. The wrapper passes it on and
        // waits for the shell, so nothing is still writing to the home directory when it goes.
        let _ = nix::sys::signal::kill(Pid::from_raw(self.child.id() as i32), Signal::SIGHUP);
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            self.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // A shell that had to be killed may still be saving its history into the directory.
        for _ in 0..100 {
            let _ = fs::remove_dir_all(&self.home);
            if !self.home.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

macro_rules! require_zsh {
    () => {
        match zsh() {
            Some(path) => path,
            None => {
                eprintln!("skipped: zsh is not installed");
                return;
            }
        }
    };
}

#[test]
fn the_shell_works_as_usual_inside_the_wrapper() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("passthrough", &zsh, 24, 80);

    terminal.send("echo sum-$((40 + 2))\r");
    terminal.wait_for("the command output", |t| t.contains("sum-42"));
    // The integration is loaded, and the user's ZDOTDIR is theirs again.
    terminal.send("print -r -- loaded=$_quince_loaded zdotdir=${ZDOTDIR:t} shim=${_quince_shim-gone}\r");
    let expected = format!(
        "loaded=1 zdotdir={} shim=gone",
        terminal.home.file_name().unwrap().display()
    );
    terminal.wait_for("the environment report", |t| t.contains(&expected));
    // Nothing of the temporary startup directory is left, on disk or in the shell's settings.
    assert_eq!(fs::read_dir(terminal.home.join(".tmp")).unwrap().count(), 0);
    terminal.send("[[ ${HISTFILE-} != */.tmp/* ]] && print history-is-the-users\r");
    terminal.wait_for("the history file check", |t| {
        t.rows().iter().any(|row| row == "history-is-the-users")
    });

    terminal.send("exit 7\r");
    assert_eq!(terminal.wait_exit(), 7);
}

#[test]
fn suggestions_appear_filter_and_are_accepted() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("accept", &zsh, 24, 80);

    terminal.send("git ch");
    // The popup can be a frame ahead of the echo of the key that caused it, so the line is
    // part of what is waited for.
    terminal.wait_for("subcommand suggestions", |t| {
        t.cursor_row_text() == "$ git ch"
            && t.popup()
                == [
                    "checkout     Switch branches or restore files",
                    "cherry-pick  Apply the changes of existing commits",
                ]
    });
    assert_eq!(terminal.selected(), None);

    // Tab takes the first suggestion; the popup moves on to what follows it.
    terminal.send("\t");
    terminal.wait_for("the next level of suggestions", |t| {
        t.cursor_row_text() == "$ git checkout" && t.popup().first().is_some_and(|row| row.starts_with("main"))
    });
    assert_eq!(
        terminal.popup().len(),
        5,
        "three branches and two options:\n{}",
        terminal.dump()
    );

    // Arrow keys move the highlight; Enter then takes the highlighted suggestion.
    terminal.send("\x1b[B");
    terminal.wait_for("the first row highlighted", |t| {
        t.selected().is_some_and(|row| row.starts_with("main"))
    });
    terminal.send("\x1b[B");
    terminal.wait_for("the second row highlighted", |t| {
        t.selected().is_some_and(|row| row.starts_with("develop"))
    });
    terminal.send("\r");
    terminal.wait_for("the accepted branch", |t| {
        t.cursor_row_text() == "$ git checkout develop"
    });

    // Enter with nothing highlighted runs the command, and the popup is gone.
    terminal.send("\x15echo ran-$((1 + 1))\r");
    terminal.wait_for("the command to run", |t| {
        t.contains("ran-2") && t.cursor_row_text() == "$"
    });
    terminal.stays("a clean screen", |t| t.popup().is_empty() && !t.contains("develop"));
}

#[test]
fn escape_dismisses_until_the_line_changes() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("dismiss", &zsh, 24, 80);

    terminal.send("git c");
    terminal.wait_for("suggestions", |t| {
        t.cursor_row_text() == "$ git c" && t.popup().len() == 3
    });
    terminal.send("\x1b");
    terminal.wait_for("the popup to close", |t| t.popup().is_empty());
    terminal.stays("the line untouched and the popup closed", |t| {
        t.cursor_row_text() == "$ git c" && t.popup().is_empty()
    });

    terminal.send("o");
    terminal.wait_for("suggestions for the new text", |t| {
        t.popup().len() == 1 && t.popup()[0].starts_with("commit")
    });
}

#[test]
fn generator_suggestions_join_the_popup_and_are_accepted() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("generator", &zsh, 24, 80);

    // The spec is a JavaScript module; its generator runs a command and post-processes it.
    terminal.send("fruit pick ");
    terminal.wait_for("the generator's suggestions among the others", |t| {
        t.popup()
            == [
                "kiwi    Written in the spec",
                "apple   From fruit",
                "banana  From fruit",
                "cherry  From fruit",
                "--all   Every one of them",
            ]
    });
    terminal.send("b");
    terminal.wait_for("the filtered list", |t| t.popup() == ["banana  From fruit"]);
    terminal.send("\t");
    terminal.wait_for("the suggestion applied", |t| {
        t.cursor_row_text() == "$ fruit pick banana"
    });
}

#[test]
fn a_custom_generator_completes_a_path_level_by_level() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("levels", &zsh, 24, 80);
    fs::create_dir_all(terminal.home.join("src/deep dir")).unwrap();
    fs::write(terminal.home.join("src/main.rs"), "").unwrap();
    fs::write(terminal.home.join("notes.txt"), "").unwrap();

    terminal.send("fruit tree ");
    terminal.wait_for("the top level", |t| t.popup() == ["notes.txt", "src/"]);
    terminal.send("s");
    terminal.wait_for("the match", |t| t.popup() == ["src/"]);
    // A directory is left open, and the generator runs again for the level below it.
    terminal.send("\t");
    terminal.wait_for("the next level", |t| {
        t.cursor_row_text() == "$ fruit tree src/" && t.popup() == ["deep dir/", "main.rs"]
    });
    terminal.send("d");
    terminal.wait_for("the match", |t| t.popup() == ["deep dir/"]);
    terminal.send("\t");
    terminal.wait_for("the escaped name after the part already typed", |t| {
        t.cursor_row_text() == r"$ fruit tree src/deep\ dir/"
    });
}

#[test]
fn the_highlight_stays_on_its_row_when_more_suggestions_arrive() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("late", &zsh, 24, 80);

    terminal.send("fruit late ");
    terminal.wait_for("what the spec spells out", |t| {
        t.popup().len() == 2 && t.popup()[1].starts_with("--all")
    });
    terminal.send("\x1b[B");
    terminal.wait_for("the first row highlighted", |t| {
        t.selected().is_some_and(|row| row == "kiwi")
    });
    terminal.send("\x1b[B");
    terminal.wait_for("the second row highlighted", |t| {
        t.selected().is_some_and(|row| row.starts_with("--all"))
    });

    // The generator comes in and puts two rows above the highlighted one.
    terminal.wait_for("the late suggestions", |t| {
        t.popup().len() == 4 && t.popup()[1] == "apple"
    });
    assert!(
        terminal.selected().is_some_and(|row| row.starts_with("--all")),
        "{}",
        terminal.dump()
    );
    terminal.send("\t");
    terminal.wait_for("the highlighted row applied", |t| {
        t.cursor_row_text() == "$ fruit late --all"
    });
}

#[test]
fn an_unaimed_tab_on_an_incomplete_list_goes_to_the_shell() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("defer", &zsh, 24, 80);
    fs::write(terminal.home.join("feature-branch.txt"), "").unwrap();
    let clear_line = |terminal: &mut Terminal| {
        terminal.send("\x15");
        terminal.wait_for("an empty line", |t| t.cursor_row_text() == "$" && t.popup().is_empty());
    };
    let listed = |t: &Terminal| t.popup().len() == 2 && t.popup()[0] == "main";

    // A generator is still out, so the list is known to be missing some. With nothing typed,
    // Tab is zsh's, which completes the one file in the directory, and the popup has stepped
    // aside rather than inserting its first row.
    terminal.send("fruit stuck ");
    terminal.wait_for("the partial list", listed);
    terminal.send("\t");
    terminal.wait_for("zsh's completion", |t| {
        t.cursor_row_text() == "$ fruit stuck feature-branch.txt"
    });

    // With something typed, the best match is a real answer and Tab takes it.
    clear_line(&mut terminal);
    terminal.send("fruit stuck m");
    terminal.wait_for("the match", |t| t.popup() == ["main"]);
    terminal.send("\t");
    terminal.wait_for("the match applied", |t| t.cursor_row_text() == "$ fruit stuck main");

    // And once the user aims at a row, Tab takes that too.
    clear_line(&mut terminal);
    terminal.send("fruit stuck ");
    terminal.wait_for("the partial list again", listed);
    terminal.send("\x1b[B");
    terminal.wait_for("the first row highlighted", |t| {
        t.selected().is_some_and(|row| row == "main")
    });
    terminal.send("\x1b[B");
    terminal.wait_for("the second row highlighted", |t| {
        t.selected().is_some_and(|row| row.starts_with("-c"))
    });
    terminal.send("\t");
    terminal.wait_for("the option", |t| t.cursor_row_text() == "$ fruit stuck -c");
}

#[test]
fn vi_mode_gets_exact_edits_and_no_popup_in_command_mode() {
    let zsh = require_zsh!();
    let rc = "bindkey -v\nbindkey -M viins '^G' vi-cmd-mode\n";
    let mut terminal = Terminal::start_with_rc("vi", &zsh, rc);

    // The suggestion differs in case from what was typed, so the typed text has to go.
    terminal.send("git CH");
    terminal.wait_for("suggestions", |t| {
        t.popup().len() == 2 && t.popup()[0].starts_with("checkout")
    });
    terminal.send("\t");
    terminal.wait_for("the replacement", |t| {
        t.cursor_row_text() == "$ git checkout" && t.popup().first().is_some_and(|row| row.starts_with("main"))
    });

    // In command mode keys are commands, not text, and the popup keeps out of it.
    terminal.send("\x07");
    terminal.wait_for("the popup to close", |t| t.popup().is_empty());
    terminal.stays("no popup in command mode", |t| t.popup().is_empty());
    terminal.send("a");
    terminal.wait_for("the popup back in insert mode", |t| {
        t.popup().first().is_some_and(|row| row.starts_with("main"))
    });
}

#[test]
fn a_tab_that_races_the_typing_never_corrupts_the_line() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("race", &zsh, 24, 80);

    let finish = |terminal: &mut Terminal, round: u64| -> String {
        terminal.send("Z");
        terminal.wait_for("the marker", |t| t.cursor_row_text().ends_with('Z'));
        let line = terminal.cursor_row_text();
        // Either the suggestion was applied to the line it was computed for, or not at all.
        assert!(
            line == "$ git chZ" || line == "$ git checkout Z",
            "round {round}: the line was corrupted:\n{}",
            terminal.dump()
        );
        terminal.send("\x15");
        terminal.wait_for("an empty line", |t| t.cursor_row_text() == "$" && t.popup().is_empty());
        line
    };

    for round in 0..40 {
        // Tab arrives on the heels of the text, from back to back up to a few milliseconds
        // later: before the shell has seen the text, while it and the wrapper are still
        // catching up with each other, and after the popup is up. Then more typing follows
        // just as closely.
        terminal.send("git ch");
        terminal.pump_for(Duration::from_micros(round * 150));
        terminal.send("\t");
        terminal.pump_for(Duration::from_micros(round % 4 * 200));
        finish(&mut terminal, round);
    }

    // With the popup up and the line at rest, Tab does apply the suggestion.
    terminal.send("git ch");
    terminal.wait_for("suggestions", |t| t.popup().len() == 2);
    terminal.send("\t");
    terminal.wait_for("the suggestion applied", |t| t.cursor_row_text() == "$ git checkout");
    assert_eq!(finish(&mut terminal, 40), "$ git checkout Z");
}

/// A stand-in for zsh-autosuggestions: text shown after the cursor, kept up to date by wrapping
/// the widgets that edit the line. Like the plugin, it wraps what the user has defined at
/// every prompt, and leaves alone any widget whose name starts with a dot or an underscore.
const GHOST_TEXT_RC: &str = r#"
_ghost_refresh() { POSTDISPLAY="  [$BUFFER]" }
_ghost_self_insert() { zle .self-insert; _ghost_refresh }
zle -N self-insert _ghost_self_insert
typeset -gA _ghost_wrapped
_ghost_bind() {
  local widget
  for widget in ${(k)widgets[(R)user:*]}; do
    [[ $widget == (.*|_*|zle-*|ghost-*|self-insert) || -n ${_ghost_wrapped[$widget]-} ]] && continue
    zle -N ghost-$widget ${widgets[$widget]#user:}
    eval "_ghost_wrap_${widget//-/_}() { zle ghost-$widget; _ghost_refresh }"
    zle -N $widget _ghost_wrap_${widget//-/_}
    _ghost_wrapped[$widget]=1
  done
}
autoload -Uz add-zsh-hook
add-zsh-hook precmd _ghost_bind
"#;

#[test]
fn plugins_that_follow_the_line_see_an_accepted_suggestion() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start_with_rc("ghost", &zsh, GHOST_TEXT_RC);

    terminal.send("git ch");
    terminal.wait_for("suggestions under the plugin's text", |t| {
        t.cursor_row_text() == "$ git ch  [git ch]" && t.popup().len() == 2
    });
    // The suggestion is applied by a widget of ours. The plugin has to notice the line
    // changed, or what it shows after the cursor is left over from before.
    terminal.send("\t");
    terminal.wait_for("the plugin's text for the new line", |t| {
        t.cursor_row_text() == "$ git checkout   [git checkout ]"
    });
}

#[test]
fn insert_value_places_the_cursor() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("cursor", &zsh, 24, 80);

    terminal.send("git commit -m");
    terminal.wait_for("the option", |t| {
        t.popup().len() == 1 && t.popup()[0].starts_with("-m, --message")
    });
    terminal.send("\t");
    terminal.wait_for("the quotes", |t| t.cursor_row_text() == "$ git commit -m ''");
    // The cursor sits between the quotes, so typing lands inside them.
    terminal.send("fix");
    terminal.wait_for("text inside the quotes", |t| {
        t.cursor_row_text() == "$ git commit -m 'fix'"
    });
}

#[test]
fn file_suggestions_follow_the_working_directory() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("files", &zsh, 24, 80);
    fs::create_dir_all(terminal.home.join("project/src")).unwrap();
    fs::write(terminal.home.join("project/notes one.txt"), "").unwrap();

    terminal.send("cd project\r");
    terminal.wait_for("the prompt after cd", |t| {
        t.contains("$ cd project") && t.cursor_row_text() == "$"
    });
    terminal.send("git add ");
    terminal.wait_for("the directory listing", |t| {
        t.popup().starts_with(&["notes one.txt".to_owned(), "src/".to_owned()])
    });
    terminal.send("n");
    terminal.wait_for("the filtered listing", |t| t.popup() == ["notes one.txt"]);
    terminal.send("\t");
    terminal.wait_for("the escaped file name", |t| {
        t.cursor_row_text() == r"$ git add notes\ one.txt"
    });
}

#[test]
fn popup_at_the_bottom_of_the_screen_leaves_no_trace() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("bottom", &zsh, 10, 60);

    terminal.send("printf 'line%s\\n' 1 2 3 4 5 6 7 8 9 10 11 12\r");
    terminal.wait_for("a full screen", |t| t.contains("line12") && t.cursor_row_text() == "$");
    assert_eq!(
        terminal.screen.screen().cursor_position().0,
        9,
        "the prompt is on the last row"
    );

    // There is no room below, so the screen scrolls to make some. The prompt stays visible.
    // The cursor position is part of the wait: zsh ignores a Ctrl-C that lands while it is
    // still redrawing for the key before it, with or without the wrapper.
    terminal.send("git ");
    terminal.wait_for("the popup", |t| {
        t.popup().len() == 7 && t.screen.screen().cursor_position() == (2, 6)
    });
    assert_eq!(terminal.cursor_row_text(), "$ git");

    terminal.send("\x03");
    terminal.wait_for("a fresh prompt", |t| t.cursor_row_text() == "$" && t.popup().is_empty());
    terminal.send("echo done\r");
    terminal.wait_for("the output", |t| {
        t.rows().iter().any(|row| row == "done") && t.cursor_row_text() == "$"
    });
    terminal.stays("no popup text left behind", |t| {
        !t.contains("cherry-pick") && !t.contains("Show the working")
    });

    let rows = terminal.rows();
    let end = usize::from(terminal.screen.screen().cursor_position().0);
    assert_eq!(
        rows[end - 3..=end],
        ["$ git", "$ echo done", "done", "$"],
        "{}",
        terminal.dump()
    );
}

#[test]
fn resizing_reaches_the_shell_and_the_popup_recovers() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("resize", &zsh, 24, 80);

    terminal.send("git ch");
    terminal.wait_for("suggestions", |t| t.popup().len() == 2);
    terminal.resize(30, 100);
    terminal.wait_for("the popup after the resize", |t| {
        t.cursor_row_text() == "$ git ch" && t.popup().len() == 2
    });

    terminal.send("\x15");
    terminal.wait_for("an empty line", |t| t.cursor_row_text() == "$" && t.popup().is_empty());
    terminal.send("print -r -- size=$COLUMNS,$LINES\r");
    terminal.wait_for("the new size in the shell", |t| t.contains("size=100,30"));
    terminal.stays("no popup text left behind", |t| !t.contains("cherry-pick"));
}

#[test]
fn a_terminal_that_reports_no_size_still_gets_suggestions() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start_reporting("unsized", &zsh, (24, 80), (0, 0));

    terminal.send("git ch");
    terminal.wait_for("suggestions", |t| t.popup().len() == 2);
    terminal.send("\x15exit 4\r");
    assert_eq!(terminal.wait_exit(), 4);
}

#[test]
fn the_session_survives_a_one_column_terminal() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("tiny", &zsh, 24, 80);

    terminal.send("git ch");
    terminal.wait_for("suggestions", |t| t.popup().len() == 2);
    // The shell redraws for a width nothing can be laid out in; what lands on the screen in
    // the meantime does not matter.
    terminal.report_size(24, 1);
    terminal.send("e");
    terminal.stays("the session alive", |_| true);
    terminal.report_size(24, 80);

    terminal.send("\x15clear\r");
    terminal.wait_for("a cleared screen", |t| {
        t.rows()[0] == "$" && t.rows()[1..].iter().all(String::is_empty)
    });
    terminal.send("git ch");
    terminal.wait_for("suggestions again", |t| {
        t.cursor_row_text() == "$ git ch" && t.popup().len() == 2
    });
    terminal.send("\x15echo alive-$((3 + 4))\r");
    terminal.wait_for("a working shell", |t| t.contains("alive-7"));
}

#[test]
fn a_large_paste_reaches_the_shell_intact() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("paste", &zsh, 24, 80);

    terminal.send("cat > pasted.txt\r");
    terminal.wait_for("cat to start", |t| {
        t.contains("$ cat > pasted.txt") && t.cursor_row_text().is_empty()
    });
    // Far more than a PTY buffers, with every byte echoed back: both directions are busy.
    let line = format!("{}\n", "0123456789".repeat(6));
    terminal.send(&line.repeat(4000));
    terminal.send("\x04");
    terminal.wait_for("the prompt after cat", |t| t.cursor_row_text() == "$");
    terminal.send("wc -c < pasted.txt\r");
    terminal.wait_for("the byte count", |t| t.contains("244000"));
}

#[test]
fn other_shells_run_without_suggestions() {
    let mut terminal = Terminal::start("sh", Path::new("/bin/sh"), 24, 80);
    terminal.send("echo plain-$((2 * 3))\r");
    terminal.wait_for("the command output", |t| t.contains("plain-6"));
    terminal.send("git ch");
    terminal.stays("no popup", |t| t.popup().is_empty());
    terminal.send("\x15exit 3\r");
    assert_eq!(terminal.wait_exit(), 3);
}

fn run(args: &[&str]) -> Output {
    let home = home_dir(&format!(
        "cli-{}",
        args.join("-").replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    ));
    let output = isolated(BIN, &home).args(args).output().unwrap();
    fs::remove_dir_all(&home).unwrap();
    output
}

#[test]
fn complete_prints_suggestions() {
    let output = run(&["complete", "git ch"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "checkout\tcheckout \tSwitch branches or restore files\n\
         cherry-pick\tcherry-pick \tApply the changes of existing commits\n"
    );

    let output = run(&["complete", "git checkout --version", "--cursor", "12"]);
    assert_eq!(String::from_utf8(output.stdout).unwrap().lines().count(), 1);

    // It waits for generators, where a session would show the rest first.
    let output = run(&["complete", "fruit pick b"]);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "banana\tbanana \tFrom fruit\n"
    );
}

#[test]
fn init_prints_the_integration_and_rejects_other_shells() {
    let output = run(&["init", "zsh"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("add-zle-hook-widget line-pre-redraw")
    );

    let output = run(&["init", "fish"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("only zsh is supported")
    );
}

#[test]
fn refuses_to_run_without_a_terminal_or_inside_itself() {
    let output = run(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr).unwrap().contains("must be a terminal"));

    let home = home_dir("nested");
    let output = isolated(BIN, &home).env("QUINCE_SESSION", "1").output().unwrap();
    fs::remove_dir_all(&home).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("already running inside")
    );
}

/// Not a test: prints how long a keystroke takes to show up in the popup.
///
///   cargo test --release --test e2e popup_latency -- --ignored --nocapture
#[test]
#[ignore]
fn popup_latency() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("latency", &zsh, 24, 80);
    let mut samples = Vec::new();
    for _ in 0..20 {
        terminal.send("git c");
        terminal.wait_for("three suggestions", |t| t.popup().len() == 3);
        let start = Instant::now();
        terminal.send("h");
        while terminal.popup().len() != 2 {
            assert!(start.elapsed() < TIMEOUT, "{}", terminal.dump());
            terminal.pump();
        }
        samples.push(start.elapsed());
        terminal.send("\x15");
        terminal.wait_for("an empty line", |t| t.cursor_row_text() == "$" && t.popup().is_empty());
    }
    samples.sort();
    println!(
        "keystroke to popup: median {:?}, worst {:?}",
        samples[samples.len() / 2],
        samples[samples.len() - 1]
    );
}

// What follows is about starting automatically: the terminal runs zsh, and the lines `install`
// put in `.zshrc` bring the wrapper in.

#[test]
fn a_new_terminal_starts_inside_a_session_and_ends_with_it() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::new_terminal("auto", &zsh, &["-i"], &[]);

    // One shell deep as far as the user can tell, and not a login shell since zsh was not.
    terminal.send("print -r -- loaded=${_quince_loaded-no} level=$SHLVL login=$options[login]\r");
    terminal.wait_for("the session report", |t| t.contains("loaded=1 level=1 login=off"));
    terminal.send("git ch");
    terminal.wait_for("suggestions", |t| {
        t.cursor_row_text() == "$ git ch" && t.popup().len() == 2
    });

    // Leaving the shell closes the terminal, with the shell's exit status.
    terminal.send("\x15exit 5\r");
    assert_eq!(terminal.wait_exit(), 5);
}

#[test]
fn a_login_shell_gets_a_login_shell() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::new_terminal("login", &zsh, &["-l"], &[]);
    terminal.send("print -r -- loaded=${_quince_loaded-no} login=$options[login]\r");
    terminal.wait_for("the session report", |t| t.contains("loaded=1 login=on"));
    terminal.send("exit\r");
    assert_eq!(terminal.wait_exit(), 0);
}

#[test]
fn the_terminal_outlives_a_wrapper_that_dies() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::new_terminal("dies", &zsh, &["-i"], &[]);

    // The wrapper is the session shell's parent. Kill it outright, as a crash would.
    terminal.send("kill -9 $PPID\r");
    terminal.wait_for("a plain shell", Terminal::at_plain_prompt);
    terminal.send("print -r -- session=${QUINCE_SESSION-none} sum=$((20 + 3))\r");
    terminal.wait_for("the plain shell's report", |t| t.contains("session=none sum=23"));

    // The terminal is in a usable state: lines start at the left margin, and it still ends.
    terminal.send("printf 'one\\ntwo\\n'\r");
    terminal.wait_for("tidy output", |t| {
        t.rows().iter().any(|row| row == "one") && t.rows().iter().any(|row| row == "two") && t.at_plain_prompt()
    });
    terminal.send("exit 3\r");
    assert_eq!(terminal.wait_exit(), 3);
}

#[test]
fn a_wrapper_that_cannot_start_leaves_a_plain_shell() {
    let zsh = require_zsh!();
    // The wrapper is told to run a shell that does not exist.
    let mut terminal = Terminal::new_terminal_running("nostart", &zsh, &["-i"], &[("SHELL", "/nonexistent/zsh")]);
    terminal.wait_for("a prompt after the complaint", |t| {
        t.contains("quince:") && t.at_plain_prompt()
    });
    terminal.send("print -r -- session=${QUINCE_SESSION-none} sum=$((20 + 3))\r");
    terminal.wait_for("a working shell", |t| t.contains("session=none sum=23"));
    terminal.send("exit 4\r");
    assert_eq!(terminal.wait_exit(), 4);
}

#[test]
fn opting_out_and_one_off_commands_are_left_alone() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::new_terminal_running("off", &zsh, &["-i"], &[("QUINCE_OFF", "1")]);
    terminal.wait_for("a plain shell", Terminal::at_plain_prompt);
    terminal.send("exit\r");
    assert_eq!(terminal.wait_exit(), 0);

    // `zsh -i -c` is how editors ask a shell for its environment. It must run and return.
    let report = "print -r -- session=${QUINCE_SESSION-none} sum=$((40 + 2))";
    let mut terminal = Terminal::new_terminal_running("command", &zsh, &["-i", "-c", report], &[]);
    terminal.wait_for("the command's output", |t| t.contains("session=none sum=42"));
    assert_eq!(terminal.wait_exit(), 0);
}

#[test]
fn a_shell_replaced_inside_a_session_keeps_its_suggestions() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::new_terminal("exec", &zsh, &["-i"], &[]);
    terminal.send("typeset -g marker=first; exec zsh\r");
    terminal.wait_for("the new shell", |t| {
        t.contains("exec zsh") && t.cursor_row_text() == "$"
    });
    terminal.send("print -r -- marker=${marker-gone} loaded=${_quince_loaded-no}\r");
    terminal.wait_for("the integration in the new shell", |t| {
        t.contains("marker=gone loaded=1")
    });
    terminal.send("git ch");
    terminal.wait_for("suggestions", |t| {
        t.cursor_row_text() == "$ git ch" && t.popup().len() == 2
    });
    terminal.send("\t");
    terminal.wait_for("the suggestion applied", |t| t.cursor_row_text() == "$ git checkout");
}

#[test]
fn text_on_the_screen_cannot_pose_as_the_shell() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("forged", &zsh, 24, 80);
    fs::write(terminal.home.join("notes.txt"), "").unwrap();

    // What a file or a remote host could print: the wrapper's own escape sequence, claiming
    // the shell has moved to another directory. Without the session's token it is nothing.
    terminal.send("printf '\\033]6973;P;/\\007\\033]6973;guess;P;/\\007seen\\n'\r");
    terminal.wait_for("the output", |t| {
        t.rows().iter().any(|row| row == "seen") && t.cursor_row_text() == "$"
    });
    terminal.send("git add ");
    terminal.wait_for("files of the real directory", |t| {
        t.popup().first().is_some_and(|row| row == "notes.txt")
    });
}

#[test]
fn generator_commands_run_with_the_shells_environment() {
    let zsh = require_zsh!();
    let mut terminal = Terminal::start("environment", &zsh, 24, 80);
    fs::create_dir(terminal.home.join("sub")).unwrap();

    // Set after the wrapper started, so only the shell knows it.
    terminal.send("export FRUIT_KIND=citrus; cd sub\r");
    terminal.wait_for("the prompt", |t| t.contains("cd sub") && t.cursor_row_text() == "$");
    terminal.send("fruit env ");
    terminal.wait_for("the shell's variable and directory", |t| t.popup() == ["citrus", "sub"]);
}

#[test]
fn install_and_uninstall_say_what_they_did() {
    let home = home_dir("install");
    let zshrc = home.join("elsewhere.zshrc");
    fs::write(&zshrc, "alias g=git\n").unwrap();
    let run = |args: &[&str]| isolated(BIN, &home).args(args).output().unwrap();
    let said = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();

    let output = run(&["install", "--zshrc", zshrc.to_str().unwrap()]);
    assert!(output.status.success());
    assert!(
        said(&output).starts_with(&format!("Set up in {}.", zshrc.display())),
        "{}",
        said(&output)
    );
    // The tests run a build, and the user is told what that means.
    assert!(said(&output).contains("build directory"), "{}", said(&output));
    assert!(said(&output).contains(".before-quince"));
    assert!(said(&run(&["install", "--zshrc", zshrc.to_str().unwrap()])).starts_with("Already set up"));

    assert!(said(&run(&["uninstall", "--zshrc", zshrc.to_str().unwrap()])).starts_with("Removed from"));
    assert_eq!(fs::read_to_string(&zshrc).unwrap(), "alias g=git\n");
    assert!(said(&run(&["uninstall", "--zshrc", zshrc.to_str().unwrap()])).starts_with("Nothing to remove"));

    // With no file named it is the one zsh reads.
    assert!(said(&run(&["install"])).starts_with(&format!("Set up in {}.", home.join(".zshrc").display())));
    assert_eq!(run(&["install", "extra"]).status.code(), Some(2));
    fs::remove_dir_all(&home).unwrap();
}
