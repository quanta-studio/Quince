//! The embedded JavaScript engine that Fig specs and their generators run in.
//!
//! Specs are third-party code. They get a QuickJS context with nothing in it but the language
//! and `glue.js`: no file system, no network, no timers. The one thing a generator can do to
//! the outside world is ask for a command to be run, which this module only records; the
//! caller decides how to run it and hands the output back with [`Js::resolve`].
//!
//! Nothing here blocks. Every call into JavaScript runs under a time budget, after which the
//! engine interrupts it.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::{Context, Ctx, Function, Module, Runtime, Value};

const GLUE: &str = include_str!("glue.js");

const MEMORY_LIMIT: usize = 512 << 20;
const STACK_LIMIT: usize = 1 << 20;

/// Time allowed for reading in one spec file.
const LOAD_BUDGET: Duration = Duration::from_secs(2);
/// Time allowed for any other call. Generators do their waiting on commands, not in JavaScript.
const CALL_BUDGET: Duration = Duration::from_millis(500);

/// A command a generator has asked for.
#[derive(Debug, PartialEq, Eq)]
pub struct Spawn {
    /// Identifies the command when its output is handed back.
    pub command: u32,
    /// The generator run that asked.
    pub run: u32,
    /// The request as JSON: `command`, `args`, and optionally `cwd`, `env` and `timeout`.
    pub input: String,
}

/// What the functions `glue.js` calls into have collected for the caller to pick up.
#[derive(Default)]
struct Mailbox {
    commands: Cell<u32>,
    spawned: RefCell<Vec<Spawn>>,
    finished: RefCell<Vec<(u32, Option<String>)>>,
}

pub struct Js {
    runtime: Runtime,
    context: Context,
    mailbox: Rc<Mailbox>,
    /// When the JavaScript now running must be interrupted.
    deadline: Rc<Cell<Option<Instant>>>,
}

impl Js {
    pub fn new() -> rquickjs::Result<Js> {
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(STACK_LIMIT);
        let deadline = Rc::new(Cell::new(None::<Instant>));
        let limit = deadline.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            limit.get().is_some_and(|at| Instant::now() >= at)
        })));

        let context = Context::full(&runtime)?;
        let mailbox = Rc::new(Mailbox::default());
        context.with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            let post = mailbox.clone();
            let spawn = Function::new(ctx.clone(), move |run: u32, input: String| -> u32 {
                let command = post.commands.get() + 1;
                post.commands.set(command);
                post.spawned.borrow_mut().push(Spawn { command, run, input });
                command
            })?;
            globals.set("__spawn", spawn)?;
            let post = mailbox.clone();
            let finish = Function::new(ctx.clone(), move |run: u32, items: Option<String>| {
                post.finished.borrow_mut().push((run, items));
            })?;
            globals.set("__finish", finish)?;
            ctx.eval::<(), _>(GLUE)
        })?;
        Ok(Js {
            runtime,
            context,
            mailbox,
            deadline,
        })
    }

    /// Runs `work` inside the engine, then whatever promise reactions it left queued, all
    /// within `budget`. A JavaScript exception, or running out of time, gives `None`.
    fn call<R>(&self, budget: Duration, work: impl for<'js> FnOnce(&Ctx<'js>) -> rquickjs::Result<R>) -> Option<R> {
        let deadline = Instant::now() + budget;
        self.deadline.set(Some(deadline));
        let result = self.context.with(|ctx| {
            let result = work(&ctx);
            if result.is_err() {
                // Take the exception off the context so it cannot surface in a later call.
                ctx.catch();
            }
            result.ok()
        });
        while self.runtime.is_job_pending() && Instant::now() < deadline {
            // A job that throws has rejected its own promise; there is nothing more to do.
            let _ = self.runtime.execute_pending_job();
        }
        self.deadline.set(None);
        result
    }

    fn function<'js>(ctx: &Ctx<'js>, name: &str) -> rquickjs::Result<Function<'js>> {
        ctx.globals().get(name)
    }

    /// Evaluates a spec module and returns its default export described as JSON, in which each
    /// generator is a number this engine knows it by. `None` if the file is not a usable spec.
    pub fn load_spec(&mut self, path: &Path) -> Option<String> {
        let source = std::fs::read(path).ok()?;
        let name = path.to_string_lossy().into_owned();
        self.call(LOAD_BUDGET, |ctx| {
            let (module, evaluated) = Module::declare(ctx.clone(), name, source)?.eval()?;
            evaluated.finish::<()>()?;
            let spec: Value = module.get("default")?;
            Self::function(ctx, "__describe")?.call((spec,))
        })
        .flatten()
    }

    /// Takes in a generator written as JSON and returns its number.
    pub fn register(&mut self, generator: &str) -> Option<u32> {
        self.call(CALL_BUDGET, |ctx| Self::function(ctx, "__register")?.call((generator,)))
    }

    /// Starts a generator. It may finish at once or ask for commands; either shows up in
    /// [`Js::take_finished`] or [`Js::take_spawned`].
    pub fn start(&mut self, run: u32, generator: u32, tokens: &str, context: &str) {
        self.call(CALL_BUDGET, |ctx| {
            Self::function(ctx, "__start")?.call::<_, ()>((run, generator, tokens, context))
        });
    }

    /// Hands a command's output, as JSON with `stdout`, `stderr` and `status`, to the generator
    /// waiting for it.
    pub fn resolve(&mut self, command: u32, output: &str) {
        self.call(CALL_BUDGET, |ctx| {
            Self::function(ctx, "__resolve")?.call::<_, ()>((command, output))
        });
    }

    /// Asks a generator's `trigger` function whether going from `previous` to `token` calls
    /// for running it again.
    pub fn fires(&mut self, generator: u32, token: &str, previous: &str) -> bool {
        self.call(CALL_BUDGET, |ctx| {
            Self::function(ctx, "__trigger")?.call((generator, token, previous))
        })
        .unwrap_or(false)
    }

    /// Asks a generator's `getQueryTerm` function which part of `token` its suggestions are
    /// matched against.
    pub fn query_term(&mut self, generator: u32, token: &str) -> Option<String> {
        self.call(CALL_BUDGET, |ctx| {
            Self::function(ctx, "__queryTerm")?.call((generator, token))
        })
    }

    /// Passes template suggestions, as a JSON array, through a generator's
    /// `filterTemplateSuggestions` function.
    pub fn filter_templates(&mut self, generator: u32, items: &str) -> Option<String> {
        self.call(CALL_BUDGET, |ctx| {
            Self::function(ctx, "__filterTemplate")?.call((generator, items))
        })
    }

    /// Commands asked for since the last call.
    pub fn take_spawned(&mut self) -> Vec<Spawn> {
        self.mailbox.spawned.take()
    }

    /// Runs that ended since the last call, each with its suggestions as a JSON array, or
    /// `None` if the generator threw.
    pub fn take_finished(&mut self) -> Vec<(u32, Option<String>)> {
        self.mailbox.finished.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value as Json, json};
    use std::path::PathBuf;

    const SPEC: &str = r#"
        const branches = {
          script: ["git", "branch"],
          postProcess: (out, tokens) =>
            out.split("\n").filter(Boolean).map((name) => ({ name, description: `Branch for ${tokens[0]}` })),
        };
        const paths = {
          custom: async (tokens, execute, context) => {
            const { stdout, status } = await execute({ command: "ls", args: [context.currentWorkingDirectory] });
            return status === 0 ? stdout.split("\n") : [];
          },
          trigger: (token, previous) => token.lastIndexOf("/") !== previous.lastIndexOf("/"),
          getQueryTerm: (token) => token.slice(token.lastIndexOf("/") + 1),
        };
        export default {
          name: "git",
          description: "Version control",
          subcommands: [
            { name: "checkout", args: { name: "branch", generators: branches } },
            { name: "merge", args: { generators: [branches, paths, { template: "folders", trigger: "/" }] } },
            { name: "now", args: { generators: { custom: async (tokens) => [tokens.join("+"), null, "", { name: "b" }] } } },
            { name: "boom", args: { generators: { custom: async () => { throw new Error("no"); } } } },
            { name: "spin", args: { generators: { custom: async () => { for (;;) {} } } } },
            { name: "escape", args: { generators: { custom: async () => [typeof require, typeof fetch, typeof process] } } },
          ],
          loadSpec: () => "elsewhere",
        };
    "#;

    fn spec_file(name: &str, source: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("quince-js-{name}-{}.js", std::process::id()));
        std::fs::write(&path, source).unwrap();
        path
    }

    /// Loads [`SPEC`] and returns the engine with the generator number of each subcommand.
    fn loaded(name: &str) -> (Js, Json) {
        let path = spec_file(name, SPEC);
        let mut js = Js::new().unwrap();
        let described: Json = serde_json::from_str(&js.load_spec(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        (js, described)
    }

    fn generator(described: &Json, subcommand: usize, index: usize) -> u32 {
        described["subcommands"][subcommand]["args"]["generators"][index]["__gen"]
            .as_u64()
            .unwrap() as u32
    }

    #[test]
    fn describes_a_spec_without_its_functions() {
        let (_, described) = loaded("describe");
        assert_eq!(described["name"], "git");
        assert_eq!(described["description"], "Version control");
        // Functions do not survive, wherever they are.
        assert_eq!(described.get("loadSpec"), None);

        // A lone generator becomes a list of one; the same generator is the same number.
        let checkout = &described["subcommands"][0]["args"]["generators"];
        assert_eq!(
            checkout,
            &json!([{ "__gen": 1, "runnable": true, "filterTemplate": false }])
        );
        let merge = &described["subcommands"][1]["args"]["generators"];
        assert_eq!(merge[0]["__gen"], 1);
        assert_eq!(
            merge[1],
            json!({ "__gen": 2, "runnable": true, "filterTemplate": false, "trigger": { "fn": true }, "getQueryTerm": { "fn": true } })
        );
        assert_eq!(
            merge[2],
            json!({ "__gen": 3, "runnable": false, "template": "folders", "filterTemplate": false, "trigger": "/" })
        );
    }

    #[test]
    fn a_script_generator_asks_for_its_command_then_post_processes() {
        let (mut js, described) = loaded("script");
        js.start(7, generator(&described, 0, 0), r#"["git","checkout",""]"#, "{}");

        let spawned = js.take_spawned();
        assert_eq!(spawned.len(), 1);
        assert_eq!((spawned[0].command, spawned[0].run), (1, 7));
        assert_eq!(
            serde_json::from_str::<Json>(&spawned[0].input).unwrap(),
            json!({ "command": "git", "args": ["branch"] })
        );
        assert_eq!(js.take_finished(), []);

        js.resolve(1, r#"{"stdout":"main\ndevelop\n","stderr":"","status":0}"#);
        let finished = js.take_finished();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].0, 7);
        assert_eq!(
            serde_json::from_str::<Json>(finished[0].1.as_ref().unwrap()).unwrap(),
            json!([
                { "name": "main", "description": "Branch for git" },
                { "name": "develop", "description": "Branch for git" },
            ])
        );
        // Output for a command nobody is waiting for is ignored.
        js.resolve(1, r#"{"stdout":"again","stderr":"","status":0}"#);
        js.resolve(99, r#"{"stdout":"x","stderr":"","status":0}"#);
        assert_eq!(js.take_finished(), []);
    }

    #[test]
    fn a_custom_generator_gets_the_tokens_the_context_and_a_way_to_run_commands() {
        let (mut js, described) = loaded("custom");
        js.start(
            1,
            generator(&described, 1, 1),
            r#"["git","merge",""]"#,
            r#"{"currentWorkingDirectory":"/work"}"#,
        );
        let spawned = js.take_spawned();
        assert_eq!(
            serde_json::from_str::<Json>(&spawned[0].input).unwrap(),
            json!({ "command": "ls", "args": ["/work"] })
        );
        // Output reaches the spec without its surrounding blank lines, as Fig delivers it.
        js.resolve(
            spawned[0].command,
            r#"{"stdout":"\na\r\nb\n\n","stderr":"","status":0}"#,
        );
        assert_eq!(
            js.take_finished(),
            [(1, Some(r#"[{"name":"a"},{"name":"b"}]"#.to_owned()))]
        );

        // Without commands it finishes on the spot; empty entries are dropped.
        js.start(2, generator(&described, 2, 0), r#"["git","now","x"]"#, "{}");
        assert_eq!(js.take_spawned(), []);
        assert_eq!(
            js.take_finished(),
            [(2, Some(r#"[{"name":"git+now+x"},{"name":"b"}]"#.to_owned()))]
        );
    }

    #[test]
    fn a_generator_that_throws_reports_failure() {
        let (mut js, described) = loaded("throws");
        js.start(5, generator(&described, 3, 0), "[]", "{}");
        assert_eq!(js.take_finished(), [(5, None)]);
    }

    #[test]
    fn a_generator_that_never_returns_is_interrupted() {
        let (mut js, described) = loaded("spins");
        let started = Instant::now();
        js.start(5, generator(&described, 4, 0), "[]", "{}");
        assert!(started.elapsed() < CALL_BUDGET * 4, "took {:?}", started.elapsed());
        assert_eq!(js.take_finished(), []);

        // The engine is still usable afterwards.
        js.start(6, generator(&described, 2, 0), r#"["ok"]"#, "{}");
        assert_eq!(
            js.take_finished(),
            [(6, Some(r#"[{"name":"ok"},{"name":"b"}]"#.to_owned()))]
        );
    }

    #[test]
    fn specs_cannot_reach_outside_the_engine() {
        let (mut js, described) = loaded("sandbox");
        js.start(1, generator(&described, 5, 0), "[]", "{}");
        let (_, items) = js.take_finished().remove(0);
        assert_eq!(
            serde_json::from_str::<Json>(&items.unwrap()).unwrap(),
            json!([{ "name": "undefined" }, { "name": "undefined" }, { "name": "undefined" }])
        );
    }

    #[test]
    fn trigger_and_query_term_functions_are_callable() {
        let (mut js, described) = loaded("functions");
        let paths = generator(&described, 1, 1);
        assert!(js.fires(paths, "src/", "src"));
        assert!(!js.fires(paths, "src/ma", "src/m"));
        assert_eq!(js.query_term(paths, "src/ma").as_deref(), Some("ma"));

        // A generator without the function: never fires, and the whole token is the term.
        let branches = generator(&described, 0, 0);
        assert!(!js.fires(branches, "a", "b"));
        assert_eq!(js.query_term(branches, "feat").as_deref(), Some("feat"));
    }

    #[test]
    fn json_generators_run_like_any_other() {
        let mut js = Js::new().unwrap();
        let id = js.register(r#"{"script":["printf","a\\nb"],"splitOn":"\n"}"#).unwrap();
        js.start(3, id, "[]", "{}");
        let spawned = js.take_spawned();
        assert_eq!(
            serde_json::from_str::<Json>(&spawned[0].input).unwrap(),
            json!({ "command": "printf", "args": ["a\\nb"] })
        );
        js.resolve(spawned[0].command, r#"{"stdout":" a\nb\n","stderr":"","status":0}"#);
        assert_eq!(
            js.take_finished(),
            [(3, Some(r#"[{"name":"a"},{"name":"b"}]"#.to_owned()))]
        );

        // A script with nothing to turn its output into suggestions gives none.
        let id = js.register(r#"{"script":["true"]}"#).unwrap();
        js.start(4, id, "[]", "{}");
        let command = js.take_spawned()[0].command;
        js.resolve(command, r#"{"stdout":"x","stderr":"","status":0}"#);
        assert_eq!(js.take_finished(), [(4, Some("[]".to_owned()))]);
        assert_eq!(js.register("not json"), None);
    }

    #[test]
    fn template_suggestions_pass_through_a_filter() {
        let path = spec_file(
            "filter",
            r#"export default { name: "x", args: { generators: {
                 template: "filepaths",
                 filterTemplateSuggestions: (items) => items.filter((item) => item.name.endsWith(".json")),
               } } };"#,
        );
        let mut js = Js::new().unwrap();
        let described: Json = serde_json::from_str(&js.load_spec(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        let generator = &described["args"]["generators"][0];
        assert_eq!(generator["filterTemplate"], true);
        assert_eq!(generator["runnable"], false);

        let kept = js
            .filter_templates(1, r#"[{"name":"a.json"},{"name":"b.txt"},{"name":"dir/"}]"#)
            .unwrap();
        assert_eq!(kept, r#"[{"name":"a.json"}]"#);
    }

    #[test]
    fn unusable_files_give_no_spec() {
        let mut js = Js::new().unwrap();
        for (name, source) in [
            ("syntax", "export default {"),
            ("throws", "throw new Error('at load'); export default { name: 'x' };"),
            ("function", "export default () => ({ name: 'x' });"),
            ("missing", "export const other = 1;"),
            ("forever", "for (;;) {} export default { name: 'x' };"),
        ] {
            let path = spec_file(&format!("unusable-{name}"), source);
            assert_eq!(js.load_spec(&path), None, "{name}");
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(js.load_spec(Path::new("/nonexistent/spec.js")), None);

        // None of that stops a good spec from loading afterwards.
        let path = spec_file("good", "export default { name: 'fine' };");
        assert_eq!(js.load_spec(&path).as_deref(), Some(r#"{"name":"fine"}"#));
        std::fs::remove_file(path).unwrap();
    }
}
