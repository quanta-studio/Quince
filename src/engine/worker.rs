//! Runs the engine off the terminal's hot path. The session posts requests and is woken through
//! a pipe when an answer is ready.

use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Instant;

use super::command::Output;
use super::spec::SpecStore;
use super::{Completion, Engine, Request};

/// The JavaScript engine recurses on the stack of the thread it runs on.
const STACK_SIZE: usize = 16 << 20;

/// Everything the engine's thread reacts to.
pub enum Event {
    Request(Request),
    /// A command a generator asked for has ended.
    CommandDone {
        command: u32,
        output: Output,
    },
    /// The user's shell's exported variables.
    Environment(Vec<(String, String)>),
    Shutdown,
}

pub struct Worker {
    events: Sender<Event>,
    pub results: Receiver<Completion>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// Starts the engine on a thread of its own, reading specs from `dirs`. `wake` gets one
    /// byte written to it for every answer placed on `results`; a request may be answered
    /// more than once, as its generators come in.
    pub fn spawn(dirs: Vec<PathBuf>, wake: OwnedFd) -> io::Result<Worker> {
        let (events, inbox) = mpsc::channel();
        let (outbox, results) = mpsc::channel();
        let mut wake = std::fs::File::from(wake);
        let for_commands = events.clone();
        let run = move || {
            // If the engine dies the session goes on without suggestions, which is not worth
            // a message over whatever the user is doing.
            crate::quiet::set(true);
            // The engine is built here because the JavaScript engine inside it cannot change
            // threads.
            let mut engine = Engine::new(SpecStore::new(dirs), for_commands);
            loop {
                let event = match engine.next_deadline() {
                    None => inbox.recv().ok(),
                    Some(deadline) => match inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(event) => Some(event),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => Some(Event::Shutdown),
                    },
                };
                // Only the newest line matters, but what is queued behind an older one may
                // not be requests, and all of it has to be dealt with.
                let mut newest = None;
                for event in event.into_iter().chain(inbox.try_iter()) {
                    match event {
                        Event::Request(request) => newest = Some(request),
                        Event::CommandDone { command, output } => engine.command_done(command, &output),
                        Event::Environment(environment) => engine.set_environment(environment),
                        Event::Shutdown => return,
                    }
                }
                if let Some(request) = newest {
                    engine.request(request);
                }
                engine.tick(Instant::now());
                for completion in engine.take_outgoing() {
                    if outbox.send(completion).is_err() {
                        return;
                    }
                    match wake.write(&[0]) {
                        // A full pipe already has a wake-up waiting in it.
                        Err(error) if error.kind() != io::ErrorKind::WouldBlock => return,
                        _ => {}
                    }
                }
            }
        };
        let thread = thread::Builder::new()
            .name("completion".into())
            .stack_size(STACK_SIZE)
            .spawn(run)?;
        Ok(Worker {
            events,
            results,
            thread: Some(thread),
        })
    }

    pub fn environment(&self, environment: Vec<(String, String)>) {
        let _ = self.events.send(Event::Environment(environment));
    }

    pub fn request(&self, request: Request) {
        // The worker only goes away with the session, so a failed send has no one to tell.
        let _ = self.events.send(Event::Request(request));
    }
}

impl Drop for Worker {
    /// Stops the engine and waits for it, so that the commands it was running are killed
    /// rather than left behind when the session ends.
    fn drop(&mut self) {
        let _ = self.events.send(Event::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
