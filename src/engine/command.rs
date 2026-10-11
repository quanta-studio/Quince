//! Runs the commands generators ask for, off the engine's thread.
//!
//! A command runs detached from the terminal: no input, output captured, and in a session of
//! its own, so it can neither read the user's keys nor write over the screen, and can be killed
//! together with anything it started.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};

use super::worker::Event;
use crate::session::SESSION_VAR;

/// How long a command may run when the spec does not say. This is Fig's default.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// No spec gets to wait longer than this, whatever it asks for.
const MAX_TIMEOUT: Duration = Duration::from_secs(30);

const MAX_STDOUT: u64 = 8 << 20;
const MAX_STDERR: u64 = 64 << 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    /// The exit code, or -1 if the command could not be run or was killed.
    pub status: i32,
}

impl Output {
    pub fn failed() -> Output {
        Output {
            stdout: String::new(),
            stderr: String::new(),
            status: -1,
        }
    }

    /// The form a generator receives.
    pub fn to_json(&self) -> String {
        json!({ "stdout": self.stdout, "stderr": self.stderr, "status": self.status }).to_string()
    }
}

/// A command that has been started.
pub struct Started {
    /// The process group to kill, if the command got as far as running.
    pub group: Option<u32>,
    pub deadline: Instant,
}

/// A set of environment variables, as name and value.
pub type Environment = [(String, String)];

/// Starts the command described by `input`, a generator's request as JSON, and returns at
/// once. Its output arrives later as [`Event::CommandDone`] carrying `command`; that event is
/// sent in every case, including when the command cannot be started.
///
/// The command runs in `default_dir` and with `environment`, the user's shell's, unless the
/// request says otherwise. Without an `environment` it inherits this process's.
pub fn start(
    command: u32,
    input: &str,
    default_dir: &Path,
    environment: Option<&Environment>,
    events: &Sender<Event>,
) -> Started {
    let input: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let timeout = input
        .get("timeout")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis)
        .min(MAX_TIMEOUT);
    let deadline = Instant::now() + timeout;
    let failed = |events: &Sender<Event>| {
        let _ = events.send(Event::CommandDone {
            command,
            output: Output::failed(),
        });
        Started { group: None, deadline }
    };

    let Some(program) = input
        .get("command")
        .and_then(Value::as_str)
        .filter(|program| !program.is_empty())
    else {
        return failed(events);
    };
    let dir = input.get("cwd").and_then(Value::as_str).map_or(default_dir, Path::new);
    let mut process = Command::new(program);
    process
        .args(
            input
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(environment) = environment {
        // The shell's variables in place of ours, which also decides where the program is
        // looked up. The session's token stays behind, and the directory is the one being
        // run in, which the shell does not report with the rest.
        process
            .env_clear()
            .envs(environment.iter().filter(|(name, _)| name != SESSION_VAR).cloned())
            .env("PWD", dir);
    }
    for (name, value) in input.get("env").and_then(Value::as_object).into_iter().flatten() {
        match value.as_str() {
            Some(value) => process.env(name, value),
            None => process.env_remove(name),
        };
    }
    // SAFETY: the closure runs between fork and exec and makes one async-signal-safe call.
    unsafe {
        process.pre_exec(|| {
            // A session of its own: no controlling terminal, and a process group to kill.
            libc::setsid();
            Ok(())
        });
    }
    let Ok(mut child) = process.spawn() else {
        return failed(events);
    };

    let group = child.id();
    let events = events.clone();
    let reader = thread::Builder::new().name("generator-command".into()).spawn(move || {
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        // Both pipes have to be read at once, or a command filling one would stall.
        let errors = thread::spawn(move || {
            let Some(mut stderr) = stderr else { return String::new() };
            let kept = read_up_to(&mut stderr, MAX_STDERR);
            // The rest is thrown away, but still read, so the command is never left blocked.
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            kept
        });
        let mut stdout = stdout.expect("standard output was piped");
        let kept = read_up_to(&mut stdout, MAX_STDOUT);
        if kept.len() as u64 >= MAX_STDOUT && stdout.read(&mut [0]).is_ok_and(|more| more > 0) {
            kill(group);
        }
        let status = child.wait().ok().and_then(|status| status.code()).unwrap_or(-1);
        let output = Output {
            stdout: kept,
            stderr: errors.join().unwrap_or_default(),
            status,
        };
        let _ = events.send(Event::CommandDone { command, output });
    });
    if reader.is_err() {
        kill(group);
    }
    Started {
        group: Some(group),
        deadline,
    }
}

/// Kills a command and everything it started.
pub fn kill(group: u32) {
    let _ = killpg(Pid::from_raw(group as i32), Signal::SIGKILL);
}

fn read_up_to(stream: &mut impl Read, limit: u64) -> String {
    let mut bytes = Vec::new();
    let _ = stream.take(limit).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};

    fn run(input: Value) -> (Output, Duration) {
        run_with(input, None)
    }

    fn run_with(input: Value, environment: Option<&Environment>) -> (Output, Duration) {
        let (events, inbox): (_, Receiver<Event>) = channel();
        let begun = Instant::now();
        let started = start(1, &input.to_string(), Path::new("/"), environment, &events);
        loop {
            match inbox.recv_timeout(Duration::from_millis(20)) {
                Ok(Event::CommandDone { command: 1, output }) => return (output, begun.elapsed()),
                Ok(_) => panic!("unexpected event"),
                Err(_) => {
                    if let Some(group) = started.group.filter(|_| Instant::now() >= started.deadline) {
                        kill(group);
                    }
                }
            }
            assert!(begun.elapsed() < Duration::from_secs(20), "the command never reported");
        }
    }

    #[test]
    fn captures_output_and_status() {
        let (output, _) = run(json!({ "command": "sh", "args": ["-c", "echo out; echo err >&2; exit 3"] }));
        assert_eq!(
            output,
            Output {
                stdout: "out\n".into(),
                stderr: "err\n".into(),
                status: 3
            }
        );
        assert_eq!(output.to_json(), r#"{"status":3,"stderr":"err\n","stdout":"out\n"}"#);
    }

    #[test]
    fn honours_the_directory_and_environment_asked_for() {
        let (output, _) = run(json!({ "command": "pwd" }));
        assert_eq!(output.stdout, "/\n");
        let (output, _) = run(json!({ "command": "pwd", "cwd": "/usr" }));
        assert_eq!(output.stdout, "/usr\n");
        let (output, _) = run(json!({
            "command": "sh", "args": ["-c", "echo $GREETING-${HOME-unset}"],
            "env": { "GREETING": "hello", "HOME": null },
        }));
        assert_eq!(output.stdout, "hello-unset\n");
    }

    #[test]
    fn runs_with_the_shells_environment_when_given_one() {
        let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        let shell = [
            pair("PATH", "/usr/bin:/bin"),
            pair("FRUIT", "kiwi"),
            pair(SESSION_VAR, "token"),
            pair("PWD", "/stale"),
        ];
        let show = "echo $FRUIT-${HOME-unset}-${QUINCE_SESSION-unset}-$PWD";
        // Only what the shell has, without the token, and with the directory it runs in.
        let (output, _) = run_with(
            json!({ "command": "sh", "args": ["-c", show], "cwd": "/usr" }),
            Some(&shell),
        );
        assert_eq!(output.stdout, "kiwi-unset-unset-/usr\n");
        // The spec's own variables still go on top.
        let (output, _) = run_with(
            json!({ "command": "sh", "args": ["-c", show], "env": { "FRUIT": "plum", "HOME": "/h" } }),
            Some(&shell),
        );
        assert_eq!(output.stdout, "plum-/h-unset-/\n");
        // The program is looked up on the shell's path, not ours.
        let nowhere = [pair("PATH", "/nonexistent")];
        assert_eq!(run_with(json!({ "command": "sh" }), Some(&nowhere)).0, Output::failed());
    }

    #[test]
    fn has_no_terminal_and_no_input() {
        // Reading the terminal or standard input finds nothing rather than the user's keys.
        let (output, elapsed) = run(json!({ "command": "sh", "args": ["-c", "cat; cat /dev/tty; echo done"] }));
        assert_eq!(output.stdout, "done\n");
        assert!(elapsed < Duration::from_secs(2));
    }

    #[test]
    fn reports_failure_for_what_cannot_be_run() {
        for input in [
            json!({ "command": "/nonexistent/program" }),
            json!({ "args": ["x"] }),
            json!("not a request"),
            json!(null),
        ] {
            assert_eq!(run(input.clone()).0, Output::failed(), "{input}");
        }
    }

    #[test]
    fn a_command_past_its_deadline_can_be_killed_with_its_children() {
        let (output, elapsed) =
            run(json!({ "command": "sh", "args": ["-c", "echo early; sleep 30 & sleep 30"], "timeout": 150 }));
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
        assert_eq!(output.stdout, "early\n");
        assert_eq!(output.status, -1);
    }

    #[test]
    fn a_flood_of_output_is_cut_off() {
        let (output, elapsed) = run(json!({ "command": "sh", "args": ["-c", "yes abcdefghijklmnopqrstuvwxyz"] }));
        assert_eq!(output.stdout.len() as u64, MAX_STDOUT);
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    }
}
