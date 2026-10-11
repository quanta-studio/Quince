use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::os::fd::BorrowedFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use quince::engine::spec::SpecStore;
use quince::engine::worker::Event;
use quince::engine::{Engine, Request};
use quince::session::{self, Options, SESSION_VAR};
use quince::setup::{self, Outcome};
use quince::shell;

const USAGE: &str = "\
Fig-style autocomplete for the terminal.

Usage:
  quince [shell] [--login] [-- <shell arguments>]
      Start $SHELL with autocomplete. This is the default command.
  quince install [--zshrc <file>]
      Start automatically in every new terminal, by adding a few lines to your .zshrc.
      The file as it was is kept beside it.
  quince uninstall [--zshrc <file>]
      Take those lines out again.
  quince init zsh
      Print the shell integration, for sourcing from your own startup file.
  quince complete <line> [--cursor <chars>] [--cwd <dir>]
      Print the suggestions for a command line, one per line, tab separated:
      label, inserted text, description.

Specs are read from $QUINCE_SPECS_DIR, or ~/.local/share/quince/specs.
";

/// How long `complete` waits for generators before printing what it has.
const GENERATOR_WAIT: Duration = Duration::from_secs(12);

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("quince: {message}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[OsString]) -> Result<u8, String> {
    let command = args.first().and_then(|arg| arg.to_str());
    match command {
        Some("-h" | "--help" | "help") => print(USAGE),
        Some("-V" | "--version") => print(&format!("quince {}\n", env!("CARGO_PKG_VERSION"))),
        Some("init") => init(&args[1..]),
        Some("install") => install(&args[1..], true),
        Some("uninstall") => install(&args[1..], false),
        Some("complete") => complete(&args[1..]),
        Some("shell") => shell_session(&args[1..]),
        _ => shell_session(args),
    }
}

fn print(text: &str) -> Result<u8, String> {
    // A closed pipe on the other end is not our error to report.
    let _ = io::stdout().write_all(text.as_bytes());
    Ok(0)
}

/// Adds the lines that start a session in every new terminal to the user's `.zshrc`, or with
/// `add` false takes them out.
fn install(args: &[OsString], add: bool) -> Result<u8, String> {
    let zshrc = match args {
        [] => setup::default_zshrc().ok_or("cannot tell where your .zshrc is; pass --zshrc <file>")?,
        [flag, path] if flag == "--zshrc" => PathBuf::from(path),
        _ => return Err("usage: quince install|uninstall [--zshrc <file>]".into()),
    };
    let failed = |error: io::Error| format!("{}: {error}", zshrc.display());
    if !add {
        return print(&match setup::uninstall(&zshrc).map_err(failed)? {
            Outcome::Removed => format!(
                "Removed from {}. Terminals opened from now on start plain.\n",
                zshrc.display()
            ),
            _ => format!("Nothing to remove in {}.\n", zshrc.display()),
        });
    }

    // New terminals run this very binary, wherever it is, rather than whatever a search of
    // the path would find at the time.
    let binary = env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("cannot tell where this program is: {error}"))?;
    let outcome = setup::install(&zshrc, &binary).map_err(failed)?;
    let mut text = match outcome {
        Outcome::Unchanged => format!("Already set up in {}.\n", zshrc.display()),
        _ => format!("Set up in {}.\n", zshrc.display()),
    };
    text.push_str(&format!("New terminals will start {}\n", binary.display()));
    if binary.components().any(|part| part.as_os_str() == "target") {
        text.push_str("That is a build directory: it stops working if the build is cleaned or moved.\n");
    }
    let backup = setup::backup_path(&zshrc);
    if backup.exists() {
        text.push_str(&format!("Your file as it was before is kept at {}\n", backup.display()));
    }
    text.push_str(
        "To skip it for one shell, start it with QUINCE_OFF=1. To undo this, run\n\
         `quince uninstall`. If the program ever fails to start, the terminal\n\
         carries on as a plain shell.\n",
    );
    print(&text)
}

fn init(args: &[OsString]) -> Result<u8, String> {
    match args.first().and_then(|arg| arg.to_str()) {
        Some("zsh") if args.len() == 1 => print(shell::ZSH_INTEGRATION),
        Some(other) => Err(format!("no integration for '{other}' yet; only zsh is supported")),
        None => Err("usage: quince init zsh".into()),
    }
}

fn complete(args: &[OsString]) -> Result<u8, String> {
    let mut line = None;
    let mut cursor = None;
    let mut cwd = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--cursor") => {
                let value = args
                    .next()
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<usize>().ok());
                cursor = Some(value.ok_or("--cursor needs a number of characters")?);
            }
            Some("--cwd") => cwd = Some(PathBuf::from(args.next().ok_or("--cwd needs a directory")?)),
            Some(text) if line.is_none() => line = Some(text.to_owned()),
            _ => return Err(format!("unexpected argument '{}'", arg.to_string_lossy())),
        }
    }
    let buffer = line.ok_or("usage: quince complete <line> [--cursor <chars>] [--cwd <dir>]")?;
    let cursor = match cursor {
        Some(chars) => buffer
            .char_indices()
            .nth(chars)
            .map_or(buffer.len(), |(offset, _)| offset),
        None => buffer.len(),
    };
    let cwd = match cwd {
        Some(dir) => dir,
        None => env::current_dir().map_err(|error| format!("cannot read the current directory: {error}"))?,
    };

    // Unlike a session, which shows what it has and adds to it, this waits for the generators.
    let (events, inbox) = mpsc::channel();
    let mut engine = Engine::new(SpecStore::new(SpecStore::default_dirs()), events);
    engine.request(Request {
        seq: 0,
        buffer,
        cursor,
        cwd,
    });
    let mut completion = engine.take_outgoing().pop().unwrap_or_default();
    let give_up = Instant::now() + GENERATOR_WAIT;
    while !engine.is_idle() && Instant::now() < give_up {
        if let Ok(Event::CommandDone { command, output }) = inbox.recv_timeout(Duration::from_millis(20)) {
            engine.command_done(command, &output);
        }
        engine.tick(Instant::now());
        if let Some(latest) = engine.take_outgoing().pop() {
            completion = latest;
        }
    }
    let mut text = String::new();
    for item in &completion.items {
        text.push_str(&format!("{}\t{}\t{}\n", item.label, item.insert, item.description));
    }
    print(&text)
}

/// Where a session started from a `.zshrc` says that it ended in the ordinary way. The shell
/// that started it is waiting on this descriptor; hearing nothing, it carries on as a plain
/// shell, which is what makes a failure to start, or a crash, something the user survives.
struct Report(BorrowedFd<'static>);

impl Report {
    fn open(argument: Option<&OsString>) -> Result<Report, String> {
        let number = argument
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|number| *number > 2)
            .ok_or("--report-fd needs the number of an open descriptor above 2")?;
        // SAFETY: nothing else in this program uses the descriptor; if it is not open, the
        // call below says so and it is never touched again.
        let fd = unsafe { BorrowedFd::borrow_raw(number) };
        // It is ours alone: the shell and everything started in the session must not hold
        // it open, or the shell waiting on it would wait for them too.
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|_| format!("descriptor {number} is not open"))?;
        Ok(Report(fd))
    }

    fn ended(&self, code: u8) {
        let _ = nix::unistd::write(self.0, format!("{}{code}", setup::REPORT_PREFIX).as_bytes());
    }
}

fn shell_session(args: &[OsString]) -> Result<u8, String> {
    let mut login = false;
    let mut report = None;
    let mut shell_args = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--login") => login = true,
            Some("--report-fd") => report = Some(Report::open(args.next())?),
            Some("--") => shell_args.extend(args.by_ref().cloned()),
            _ => return Err(format!("unexpected argument '{}'; see --help", arg.to_string_lossy())),
        }
    }

    if env::var_os(SESSION_VAR).is_some() {
        return Err("this shell is already running inside quince".into());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("standard input and output must be a terminal".into());
    }
    let shell = env::var_os("SHELL").map_or_else(|| PathBuf::from("/bin/zsh"), PathBuf::from);
    if !shell::is_zsh(&shell) {
        eprintln!(
            "quince: no integration for {} yet; it will run without suggestions",
            shell.display()
        );
    }

    let code = session::run(&Options {
        shell,
        login,
        args: shell_args,
        in_place: report.is_some(),
    })
    .map_err(|error| error.to_string())?;
    let code = u8::try_from(code).unwrap_or(1);
    if let Some(report) = report {
        report.ended(code);
    }
    Ok(code)
}
