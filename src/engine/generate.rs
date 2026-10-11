//! Keeps track of generator runs: starting them, feeding them the output of their commands,
//! and remembering their results for as long as the user stays on the same token.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::command::{self, Environment, Output};
use super::js::Js;
use super::spec::{Generator, Source, Static};
use super::worker::Event;

/// How long a generator gets from start to finish, commands included.
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
/// Most suggestions kept from one generator.
const MAX_SUGGESTIONS: usize = 20_000;

/// A generator the current position on the command line wants suggestions from.
pub struct Need {
    pub generator: Arc<Generator>,
    /// Text of the token that precedes the part being completed, as in `--color=`.
    pub prefix: String,
    /// The part of the token being completed.
    pub query: String,
    /// The argument asks for fuzzy matching.
    pub fuzzy: bool,
}

/// Where the user is: runs are only reused while this stays the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub cwd: PathBuf,
    /// The completed words of the command.
    pub words: Vec<String>,
}

pub enum Poll {
    Ready(Arc<[Static]>),
    /// Running; the result will come with a later event.
    Pending,
    /// The generator failed or cannot be run.
    Unavailable,
}

enum State {
    Running,
    Done(Arc<[Static]>),
    Failed,
}

struct Run {
    number: u32,
    /// The token as of the last time this generator was asked for, which is what a trigger
    /// compares the next one against.
    token: String,
    deadline: Instant,
    state: State,
}

struct Running {
    group: Option<u32>,
    deadline: Instant,
    killed: bool,
}

pub struct Generators {
    events: Sender<Event>,
    scope: Option<Scope>,
    /// The latest run of each generator in the current scope, by the generator's `uid`.
    runs: HashMap<u32, Run>,
    /// Commands that have not reported back yet, including those of runs since abandoned.
    commands: HashMap<u32, Running>,
    /// The engine's numbers for generators that were written as JSON, by `uid`.
    registered: HashMap<u32, Option<u32>>,
    /// The user's shell's exported variables, once it has reported them.
    environment: Option<Arc<Environment>>,
    last_run: u32,
}

impl Generators {
    pub fn new(events: Sender<Event>) -> Generators {
        Generators {
            events,
            scope: None,
            runs: HashMap::new(),
            commands: HashMap::new(),
            registered: HashMap::new(),
            environment: None,
            last_run: 0,
        }
    }

    /// Sets the environment commands run with from here on.
    pub fn set_environment(&mut self, environment: Vec<(String, String)>) {
        self.environment = Some(environment.into());
    }

    /// Moves to `scope`. Results gathered anywhere else are dropped, so coming back to a
    /// command later runs its generators afresh, and commands still running for them are
    /// killed.
    pub fn enter(&mut self, scope: Scope) {
        if self.scope.as_ref() == Some(&scope) {
            return;
        }
        self.scope = Some(scope);
        self.runs.clear();
        for running in self.commands.values_mut() {
            running.kill();
        }
    }

    /// The engine's number for `generator`.
    pub fn number(&mut self, js: &mut Js, generator: &Generator) -> Option<u32> {
        match &generator.source {
            Source::Engine(number) => Some(*number),
            Source::Json(text) => *self
                .registered
                .entry(generator.uid)
                .or_insert_with(|| js.register(text)),
        }
    }

    /// The suggestions of `generator` for the token `token`, starting or restarting it as
    /// needed. `tokens` are the words of the command with the token being completed last.
    pub fn poll(&mut self, js: Option<&mut Js>, generator: &Generator, tokens: &[String], token: &str) -> Poll {
        let Some(js) = js else { return Poll::Unavailable };
        let Some(number) = self.number(js, generator) else {
            return Poll::Unavailable;
        };

        let rerun = match self.runs.get_mut(&generator.uid) {
            None => true,
            Some(run) => {
                let previous = std::mem::replace(&mut run.token, token.to_owned());
                generator
                    .trigger
                    .fires(token, &previous, || js.fires(number, token, &previous))
            }
        };
        if rerun {
            self.start(js, number, generator.uid, tokens, token);
        }
        match &self.runs[&generator.uid].state {
            State::Running => Poll::Pending,
            State::Done(items) => Poll::Ready(items.clone()),
            State::Failed => Poll::Unavailable,
        }
    }

    fn start(&mut self, js: &mut Js, number: u32, uid: u32, tokens: &[String], token: &str) {
        self.last_run += 1;
        let run = Run {
            number: self.last_run,
            token: token.to_owned(),
            deadline: Instant::now() + RUN_TIMEOUT,
            state: State::Running,
        };
        let cwd = self.scope.as_ref().map(|scope| scope.cwd.clone()).unwrap_or_default();
        let context = json!({
            "currentWorkingDirectory": cwd,
            "environmentVariables": match &self.environment {
                Some(environment) => environment.iter().cloned().collect::<HashMap<_, _>>(),
                None => std::env::vars().collect(),
            },
            "currentProcess": "zsh",
            "sshPrefix": "",
            "searchTerm": token,
        });
        // The run is on record before the engine is called: a generator that needs no command
        // finishes inside this very call.
        self.runs.insert(uid, run);
        js.start(self.last_run, number, &json!(tokens).to_string(), &context.to_string());
        self.absorb(js);
    }

    /// Acts on what the engine has left behind: commands to start and runs that ended.
    /// Returns whether a run of the current scope ended.
    fn absorb(&mut self, js: &mut Js) -> bool {
        let cwd = self
            .scope
            .as_ref()
            .map_or(Path::new("/"), |scope| scope.cwd.as_path())
            .to_owned();
        for spawn in js.take_spawned() {
            let started = command::start(
                spawn.command,
                &spawn.input,
                &cwd,
                self.environment.as_deref(),
                &self.events,
            );
            let mut running = Running {
                group: started.group,
                deadline: started.deadline,
                killed: false,
            };
            // A run that was abandoned before it got to ask has no use for the answer.
            if !self.runs.values().any(|run| run.number == spawn.run) {
                running.kill();
            }
            self.commands.insert(spawn.command, running);
        }

        let mut ended = false;
        for (number, items) in js.take_finished() {
            let Some(run) = self.runs.values_mut().find(|run| run.number == number) else {
                continue;
            };
            run.state = match items.as_deref().map(parse_suggestions) {
                Some(Some(items)) => State::Done(items),
                _ => State::Failed,
            };
            ended = true;
        }
        ended
    }

    /// Passes a command's output on to the generator waiting for it. Returns whether a run of
    /// the current scope ended as a result.
    pub fn command_done(&mut self, js: Option<&mut Js>, command: u32, output: &Output) -> bool {
        self.commands.remove(&command);
        let Some(js) = js else { return false };
        js.resolve(command, &output.to_json());
        self.absorb(js)
    }

    /// Kills commands that have run too long and gives up on runs that have. Returns whether a
    /// run of the current scope ended as a result.
    pub fn expire(&mut self, now: Instant) -> bool {
        for running in self.commands.values_mut().filter(|running| now >= running.deadline) {
            running.kill();
        }
        let mut ended = false;
        for run in self.runs.values_mut() {
            if matches!(run.state, State::Running) && now >= run.deadline {
                run.state = State::Failed;
                ended = true;
            }
        }
        ended
    }

    /// When [`Generators::expire`] next has something to do.
    pub fn next_deadline(&self) -> Option<Instant> {
        let commands = self
            .commands
            .values()
            .filter(|running| !running.killed)
            .map(|running| running.deadline);
        let runs = self
            .runs
            .values()
            .filter(|run| matches!(run.state, State::Running))
            .map(|run| run.deadline);
        commands.chain(runs).min()
    }

    /// Nothing in the current scope is still running.
    pub fn is_idle(&self) -> bool {
        !self.runs.values().any(|run| matches!(run.state, State::Running))
    }
}

impl Drop for Generators {
    fn drop(&mut self) {
        for running in self.commands.values_mut() {
            running.kill();
        }
    }
}

impl Running {
    fn kill(&mut self) {
        if let Some(group) = self.group.filter(|_| !self.killed) {
            command::kill(group);
        }
        self.killed = true;
    }
}

/// Reads a generator's result: a JSON array of suggestions.
fn parse_suggestions(text: &str) -> Option<Arc<[Static]>> {
    let Value::Array(items) = serde_json::from_str(text).ok()? else {
        return None;
    };
    Some(
        items
            .iter()
            .take(MAX_SUGGESTIONS)
            .filter_map(Static::from_json)
            .collect(),
    )
}
