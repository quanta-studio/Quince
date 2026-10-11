//! Completion specs in the Fig format.
//!
//! A spec comes from a `.js` file, which is a Fig spec as published, or from a `.json` file,
//! which is the same structure written by hand without functions. Either way it reaches this
//! module as JSON: the JavaScript engine describes a `.js` spec with its functions left out and
//! each generator replaced by a number it can be run by.
//!
//! Loading is lenient on purpose. Specs are third-party data, so a field with an unexpected
//! shape is skipped rather than failing the whole spec.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;

use super::js::Js;

/// Fig's default priority for a suggestion; the scale runs from 0 to 100.
pub const DEFAULT_PRIORITY: i32 = 50;

#[derive(Debug, Default)]
pub struct Subcommand {
    pub names: Vec<String>,
    pub description: String,
    pub subcommands: Vec<Arc<Subcommand>>,
    pub options: Vec<Arc<Opt>>,
    pub args: Vec<Arg>,
    pub additional: Vec<Static>,
    /// Name of another spec that holds this subcommand's real definition.
    pub load_spec: Option<String>,
    /// Single-dash words are whole options (`-version`), never chains of short flags.
    pub flags_posix_noncompliant: bool,
    pub insert_value: Option<String>,
    pub priority: i32,
    pub hidden: bool,
}

#[derive(Debug, Default)]
pub struct Opt {
    pub names: Vec<String>,
    pub description: String,
    pub args: Vec<Arg>,
    /// Also valid on every subcommand below the one that defines it.
    pub persistent: bool,
    pub repeatable: bool,
    /// The option and its argument are joined by this (`--color=always`) instead of a space.
    pub separator: Option<String>,
    pub exclusive_on: Vec<String>,
    pub insert_value: Option<String>,
    pub priority: i32,
    pub hidden: bool,
}

#[derive(Debug, Default)]
pub struct Arg {
    pub name: String,
    pub description: String,
    pub optional: bool,
    pub variadic: bool,
    /// The argument is itself a command line (`sudo <command>`).
    pub is_command: bool,
    /// Suggestions match when the typed letters appear in them in order, not only at the start.
    pub fuzzy: bool,
    pub suggestions: Vec<Static>,
    pub templates: Vec<Template>,
    /// Suggestions the spec computes at run time.
    pub generators: Vec<Arc<Generator>>,
}

/// A suggestion as a spec gives it, written out literally or returned by a generator.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Static {
    pub names: Vec<String>,
    /// Shown instead of the names when present.
    pub display_name: Option<String>,
    pub description: String,
    pub insert_value: Option<String>,
    /// Fig's `type`, such as `folder` or `option`.
    pub kind: Option<String>,
    pub priority: i32,
    pub hidden: bool,
}

/// A source of suggestions that has to be run: a command whose output is turned into
/// suggestions, or a function of the spec's own.
#[derive(Debug)]
pub struct Generator {
    /// Tells generators apart within this process.
    pub uid: u32,
    pub source: Source,
    /// There is something to run, as opposed to only a template.
    pub runnable: bool,
    pub templates: Vec<Template>,
    /// Template suggestions go through the generator's own filter function.
    pub filters_templates: bool,
    pub trigger: Trigger,
    pub query_term: QueryTerm,
}

/// Where a generator's definition lives.
#[derive(Debug)]
pub enum Source {
    /// Inside the JavaScript engine, under this number.
    Engine(u32),
    /// Written as JSON, which the engine takes in the first time the generator runs.
    Json(String),
}

/// When a generator that has already run for a token runs again as the token changes.
#[derive(Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Never: one run serves the whole token.
    Once,
    Change,
    /// When the last position of this text in the token moves, as `/` does while typing a path.
    LastIndexOf(String),
    /// When the token grows to this length.
    Threshold(usize),
    /// When the token becomes exactly one of these.
    Match(Vec<String>),
    /// The spec decides, with a function.
    Function,
}

/// Which part of the token a generator's suggestions are matched against and replace.
#[derive(Debug, PartialEq, Eq)]
pub enum QueryTerm {
    Token,
    /// Whatever follows the last occurrence of this text.
    After(String),
    /// The spec decides, with a function.
    Function,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    Filepaths,
    Folders,
}

fn one_or_many(value: Option<&Value>) -> &[Value] {
    match value {
        Some(Value::Array(items)) => items,
        Some(Value::Null) | None => &[],
        Some(single) => std::slice::from_ref(single),
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    one_or_many(value)
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn optional_text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn flag(value: &Value, key: &str) -> bool {
    match value.get(key) {
        Some(Value::Bool(b)) => *b,
        // `isRepeatable` may be a count instead of a boolean.
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n > 0.0),
        _ => false,
    }
}

fn templates(value: Option<&Value>) -> Vec<Template> {
    strings(value)
        .iter()
        .filter_map(|name| match name.as_str() {
            "filepaths" => Some(Template::Filepaths),
            "folders" => Some(Template::Folders),
            _ => None,
        })
        .collect()
}

fn priority(value: &Value) -> i32 {
    value
        .get("priority")
        .and_then(Value::as_f64)
        .map_or(DEFAULT_PRIORITY, |p| p.clamp(0.0, 100.0).round() as i32)
}

impl Subcommand {
    pub fn from_json(value: &Value) -> Option<Subcommand> {
        let names = strings(value.get("name"));
        if names.is_empty() {
            return None;
        }
        Some(Subcommand {
            names,
            description: text(value, "description"),
            subcommands: one_or_many(value.get("subcommands"))
                .iter()
                .filter_map(Subcommand::from_json)
                .map(Arc::new)
                .collect(),
            options: one_or_many(value.get("options"))
                .iter()
                .filter_map(Opt::from_json)
                .map(Arc::new)
                .collect(),
            args: one_or_many(value.get("args"))
                .iter()
                .filter_map(Arg::from_json)
                .collect(),
            additional: one_or_many(value.get("additionalSuggestions"))
                .iter()
                .filter_map(Static::from_json)
                .collect(),
            load_spec: optional_text(value, "loadSpec"),
            flags_posix_noncompliant: value
                .get("parserDirectives")
                .is_some_and(|directives| flag(directives, "flagsArePosixNoncompliant")),
            insert_value: optional_text(value, "insertValue"),
            priority: priority(value),
            hidden: flag(value, "hidden"),
        })
    }
}

impl Opt {
    fn from_json(value: &Value) -> Option<Opt> {
        let names = strings(value.get("name"));
        if names.is_empty() {
            return None;
        }
        // `requiresSeparator` is `true` (meaning "=") or the separator itself; `requiresEquals`
        // is its older spelling.
        let separator = match value.get("requiresSeparator").or_else(|| value.get("requiresEquals")) {
            Some(Value::Bool(true)) => Some("=".to_owned()),
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        };
        Some(Opt {
            names,
            description: text(value, "description"),
            args: one_or_many(value.get("args"))
                .iter()
                .filter_map(Arg::from_json)
                .collect(),
            persistent: flag(value, "isPersistent"),
            repeatable: flag(value, "isRepeatable"),
            separator,
            exclusive_on: strings(value.get("exclusiveOn")),
            insert_value: optional_text(value, "insertValue"),
            priority: priority(value),
            hidden: flag(value, "hidden"),
        })
    }
}

impl Arg {
    fn from_json(value: &Value) -> Option<Arg> {
        if !value.is_object() {
            return None;
        }
        Some(Arg {
            name: text(value, "name"),
            description: text(value, "description"),
            optional: flag(value, "isOptional"),
            variadic: flag(value, "isVariadic"),
            is_command: flag(value, "isCommand"),
            fuzzy: value.get("filterStrategy").and_then(Value::as_str) == Some("fuzzy"),
            suggestions: one_or_many(value.get("suggestions"))
                .iter()
                .filter_map(Static::from_json)
                .collect(),
            templates: templates(value.get("template")),
            generators: one_or_many(value.get("generators"))
                .iter()
                .filter_map(Generator::from_json)
                .map(Arc::new)
                .collect(),
        })
    }
}

impl Generator {
    fn from_json(value: &Value) -> Option<Generator> {
        static NEXT_UID: AtomicU32 = AtomicU32::new(1);
        if !value.is_object() {
            return None;
        }
        // The engine marks the generators it holds; anything else was written as JSON, where a
        // script is the only thing there can be to run.
        let (source, runnable) = match value.get("__gen").and_then(Value::as_u64) {
            Some(number) => (Source::Engine(number as u32), flag(value, "runnable")),
            None => (
                Source::Json(value.to_string()),
                value.get("script").is_some_and(|script| !script.is_null()),
            ),
        };
        Some(Generator {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            source,
            runnable,
            templates: templates(value.get("template")),
            filters_templates: flag(value, "filterTemplate"),
            trigger: Trigger::from_json(value.get("trigger")),
            query_term: QueryTerm::from_json(value.get("getQueryTerm")),
        })
    }
}

impl Trigger {
    fn from_json(value: Option<&Value>) -> Trigger {
        match value {
            Some(Value::String(text)) if !text.is_empty() => Trigger::LastIndexOf(text.clone()),
            Some(object @ Value::Object(_)) if flag(object, "fn") => Trigger::Function,
            Some(object @ Value::Object(_)) => match object.get("on").and_then(Value::as_str) {
                Some("change") => Trigger::Change,
                Some("threshold") => {
                    Trigger::Threshold(object.get("length").and_then(Value::as_u64).unwrap_or(1) as usize)
                }
                Some("match") => Trigger::Match(strings(object.get("string"))),
                _ => Trigger::Once,
            },
            _ => Trigger::Once,
        }
    }

    /// Whether going from `previous` to `token` calls for a new run. `ask` is consulted only
    /// when the spec decides with a function.
    pub fn fires(&self, token: &str, previous: &str, ask: impl FnOnce() -> bool) -> bool {
        match self {
            Trigger::Once => false,
            Trigger::Change => token != previous,
            Trigger::LastIndexOf(text) => token.rfind(text.as_str()) != previous.rfind(text.as_str()),
            Trigger::Threshold(length) => previous.chars().count() < *length && token.chars().count() >= *length,
            Trigger::Match(texts) => token != previous && texts.iter().any(|text| text == token),
            Trigger::Function => token != previous && ask(),
        }
    }
}

impl QueryTerm {
    fn from_json(value: Option<&Value>) -> QueryTerm {
        match value {
            Some(Value::String(text)) if !text.is_empty() => QueryTerm::After(text.clone()),
            Some(object @ Value::Object(_)) if flag(object, "fn") => QueryTerm::Function,
            _ => QueryTerm::Token,
        }
    }
}

impl Static {
    /// A suggestion is either a bare string or an object with a `name`.
    pub fn from_json(value: &Value) -> Option<Static> {
        if let Some(name) = value.as_str() {
            return (!name.is_empty()).then(|| Static {
                names: vec![name.to_owned()],
                priority: DEFAULT_PRIORITY,
                ..Static::default()
            });
        }
        let names = strings(value.get("name"));
        if names.is_empty() {
            return None;
        }
        Some(Static {
            names,
            display_name: optional_text(value, "displayName").filter(|name| !name.is_empty()),
            description: text(value, "description"),
            insert_value: optional_text(value, "insertValue"),
            kind: optional_text(value, "type"),
            priority: priority(value),
            hidden: flag(value, "hidden"),
        })
    }
}

/// Loads specs from disk on first use and keeps them.
///
/// A spec named `git` lives at `<dir>/git.js` or `<dir>/git.json`; nested names such as
/// `aws/s3` map to subdirectories.
pub struct SpecStore {
    dirs: Vec<PathBuf>,
    cache: HashMap<String, Option<Arc<Subcommand>>>,
    names: Option<Arc<[String]>>,
    js: JsState,
}

/// The JavaScript engine is only started once something needs it.
enum JsState {
    Unstarted,
    Ready(Js),
    Failed,
}

impl SpecStore {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            cache: HashMap::new(),
            names: None,
            js: JsState::Unstarted,
        }
    }

    /// The JavaScript engine the `.js` specs live in, or `None` if it could not be started.
    pub fn js(&mut self) -> Option<&mut Js> {
        if matches!(self.js, JsState::Unstarted) {
            self.js = Js::new().map_or(JsState::Failed, JsState::Ready);
        }
        match &mut self.js {
            JsState::Ready(js) => Some(js),
            _ => None,
        }
    }

    /// The directories specs are read from: `$QUINCE_SPECS_DIR` (colon separated) if
    /// set, otherwise `specs` under the XDG data directory.
    pub fn default_dirs() -> Vec<PathBuf> {
        if let Some(dirs) = std::env::var_os("QUINCE_SPECS_DIR") {
            return std::env::split_paths(&dirs)
                .filter(|dir| !dir.as_os_str().is_empty())
                .collect();
        }
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
        data_home.map(|dir| dir.join("quince/specs")).into_iter().collect()
    }

    /// Registers a spec directly, bypassing the disk.
    pub fn insert(&mut self, name: &str, spec: Subcommand) {
        self.cache.insert(name.to_owned(), Some(Arc::new(spec)));
        self.names = None;
    }

    pub fn load(&mut self, name: &str) -> Option<Arc<Subcommand>> {
        if let Some(cached) = self.cache.get(name) {
            return cached.clone();
        }
        let spec = self.read(name).map(Arc::new);
        self.cache.insert(name.to_owned(), spec.clone());
        spec
    }

    fn read(&mut self, name: &str) -> Option<Subcommand> {
        if !is_safe_name(name) {
            return None;
        }
        for dir in self.dirs.clone() {
            let script = dir.join(format!("{name}.js"));
            let described = match script.is_file() {
                true => self.js().and_then(|js| js.load_spec(&script)).map(String::into_bytes),
                false => std::fs::read(dir.join(format!("{name}.json"))).ok(),
            };
            let spec = described.and_then(|json| Subcommand::from_json(&serde_json::from_slice(&json).ok()?));
            if spec.is_some() {
                return spec;
            }
        }
        None
    }

    /// Names of all top-level specs, sorted.
    pub fn names(&mut self) -> Arc<[String]> {
        if let Some(names) = &self.names {
            return names.clone();
        }
        let on_disk = self
            .dirs
            .iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter_map(|file| {
                file.strip_suffix(".js")
                    .or_else(|| file.strip_suffix(".json"))
                    .map(str::to_owned)
            })
            // The Fig package keeps an `index.js` next to its specs.
            .filter(|name| is_safe_name(name) && name != "index");
        let registered = self
            .cache
            .iter()
            .filter(|(name, spec)| spec.is_some() && !name.contains('/'))
            .map(|(name, _)| name.clone());
        let mut names: Vec<String> = on_disk.chain(registered).collect();
        names.sort();
        names.dedup();
        let names: Arc<[String]> = names.into();
        self.names = Some(names.clone());
        names
    }
}

/// Spec names come from the command line, so they must not be able to leave the spec directory.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn loads_the_fig_shapes() {
        let spec = Subcommand::from_json(&json!({
            "name": "tool",
            "description": "A tool",
            "parserDirectives": { "flagsArePosixNoncompliant": true },
            "subcommands": [
                { "name": ["remove", "rm"], "priority": 76.4, "loadSpec": "tool/remove" },
                { "description": "no name, dropped" },
            ],
            "options": [
                { "name": ["-m", "--message"], "args": { "name": "msg" }, "isRepeatable": 2 },
                { "name": "--color", "requiresSeparator": true, "args": { "suggestions": ["always", { "name": "never", "description": "Off" }] } },
                { "name": "--sep", "requiresSeparator": ":", "exclusiveOn": ["--color"], "isPersistent": true },
            ],
            "args": [
                { "name": "path", "template": ["filepaths", "history"], "isVariadic": true, "isOptional": true },
                { "name": "ref", "generators": { "script": ["git", "branch"] }, "template": "folders" },
            ],
            "additionalSuggestions": ["extra"],
        }))
        .unwrap();

        assert_eq!(spec.names, ["tool"]);
        assert_eq!(spec.description, "A tool");
        assert!(spec.flags_posix_noncompliant);
        assert_eq!(spec.subcommands.len(), 1);
        assert_eq!(spec.subcommands[0].names, ["remove", "rm"]);
        assert_eq!(spec.subcommands[0].priority, 76);
        assert_eq!(spec.subcommands[0].load_spec.as_deref(), Some("tool/remove"));

        let message = &spec.options[0];
        assert_eq!(message.names, ["-m", "--message"]);
        assert_eq!(message.args[0].name, "msg");
        assert!(message.repeatable);
        assert_eq!(message.priority, DEFAULT_PRIORITY);

        let color = &spec.options[1];
        assert_eq!(color.separator.as_deref(), Some("="));
        assert_eq!(color.args[0].suggestions[0].names, ["always"]);
        assert_eq!(color.args[0].suggestions[1].description, "Off");

        let sep = &spec.options[2];
        assert_eq!(sep.separator.as_deref(), Some(":"));
        assert_eq!(sep.exclusive_on, ["--color"]);
        assert!(sep.persistent);

        assert_eq!(spec.args[0].templates, [Template::Filepaths]);
        assert!(spec.args[0].variadic && spec.args[0].optional);
        assert_eq!(spec.args[1].templates, [Template::Folders]);
        // A generator written as JSON has a script to run, and is kept as text for the engine.
        let generator = &spec.args[1].generators[0];
        assert!(generator.runnable);
        assert!(matches!(&generator.source, Source::Json(text) if text.contains("branch")));
        assert_eq!(generator.trigger, Trigger::Once);
        assert_eq!(generator.query_term, QueryTerm::Token);
        assert_eq!(spec.additional[0].names, ["extra"]);
    }

    #[test]
    fn tolerates_unexpected_shapes() {
        let spec = Subcommand::from_json(&json!({
            "name": "odd",
            "description": 42,
            "subcommands": "not a list",
            "options": [null, 7, { "name": [] }, { "name": "--ok", "args": [null] }],
            "args": { "suggestions": [null, 3, "", "fine"], "generators": [], "template": { "__fn": true } },
            "loadSpec": { "__fn": true },
            "priority": "high",
        }))
        .unwrap();

        assert_eq!(spec.description, "");
        assert!(spec.subcommands.is_empty());
        assert_eq!(spec.options.len(), 1);
        assert_eq!(spec.options[0].names, ["--ok"]);
        assert!(spec.options[0].args.is_empty());
        assert_eq!(spec.args[0].suggestions.len(), 1);
        assert!(spec.args[0].generators.is_empty());
        assert!(spec.args[0].templates.is_empty());
        assert_eq!(spec.load_spec, None);
        assert_eq!(spec.priority, DEFAULT_PRIORITY);
        assert!(Subcommand::from_json(&json!({ "description": "nameless" })).is_none());
    }

    #[test]
    fn reads_generators_as_the_engine_describes_them() {
        let spec = Subcommand::from_json(&json!({
            "name": "tool",
            "args": { "generators": [
                { "__gen": 4, "runnable": true, "filterTemplate": false, "trigger": { "fn": true }, "getQueryTerm": { "fn": true } },
                { "__gen": 5, "runnable": false, "template": ["folders"], "filterTemplate": true, "trigger": "/", "getQueryTerm": "/" },
                { "__gen": 6, "runnable": true, "trigger": { "on": "threshold", "length": 3 } },
                { "__gen": 7, "runnable": true, "trigger": { "on": "match", "string": ["a", "b"] } },
                { "__gen": 8, "runnable": true, "trigger": { "on": "change" } },
                "not a generator",
            ] },
        }))
        .unwrap();
        let generators = &spec.args[0].generators;
        assert_eq!(generators.len(), 5);

        assert!(matches!(generators[0].source, Source::Engine(4)));
        assert!(generators[0].runnable);
        assert_eq!(generators[0].trigger, Trigger::Function);
        assert_eq!(generators[0].query_term, QueryTerm::Function);

        assert!(!generators[1].runnable);
        assert!(generators[1].filters_templates);
        assert_eq!(generators[1].templates, [Template::Folders]);
        assert_eq!(generators[1].trigger, Trigger::LastIndexOf("/".into()));
        assert_eq!(generators[1].query_term, QueryTerm::After("/".into()));

        assert_eq!(generators[2].trigger, Trigger::Threshold(3));
        assert_eq!(generators[3].trigger, Trigger::Match(vec!["a".into(), "b".into()]));
        assert_eq!(generators[4].trigger, Trigger::Change);
        // Each generator can be told from the others.
        assert_ne!(generators[0].uid, generators[1].uid);
    }

    #[test]
    fn triggers_fire_on_the_changes_they_name() {
        let never = || -> bool { panic!("only a function trigger asks the spec") };
        assert!(!Trigger::Once.fires("abc", "ab", never));
        assert!(Trigger::Change.fires("abc", "ab", never));
        assert!(!Trigger::Change.fires("abc", "abc", never));

        let slash = Trigger::LastIndexOf("/".into());
        assert!(slash.fires("src/", "src", never));
        assert!(!slash.fires("src/ma", "src/m", never));
        assert!(slash.fires("src", "src/", never));

        let threshold = Trigger::Threshold(3);
        assert!(threshold.fires("abc", "ab", never));
        assert!(!threshold.fires("abcd", "abc", never));
        assert!(!threshold.fires("ab", "a", never));

        let exact = Trigger::Match(vec!["@".into()]);
        assert!(exact.fires("@", "", never));
        assert!(!exact.fires("@a", "@", never));

        assert!(Trigger::Function.fires("b", "a", || true));
        assert!(!Trigger::Function.fires("b", "a", || false));
        // The spec is not asked when nothing changed.
        assert!(!Trigger::Function.fires("a", "a", never));
    }

    #[test]
    fn reads_what_a_suggestion_looks_like() {
        let item = Static::from_json(&json!({
            "name": ["feat/x", "x"], "displayName": "x (remote)", "description": "Branch",
            "insertValue": "feat/x", "type": "folder", "priority": 80, "hidden": true, "icon": "ignored",
        }))
        .unwrap();
        assert_eq!(item.names, ["feat/x", "x"]);
        assert_eq!(item.display_name.as_deref(), Some("x (remote)"));
        assert_eq!(item.kind.as_deref(), Some("folder"));
        assert_eq!((item.priority, item.hidden), (80, true));
        assert_eq!(Static::from_json(&json!({ "displayName": "nameless" })), None);
    }

    /// Not a test: loads every spec in `$QUINCE_SPECS_DIR` and reports how it went.
    ///
    ///   cargo test --release --lib every_installed_spec -- --ignored --nocapture
    #[test]
    #[ignore]
    fn every_installed_spec_loads() {
        fn spec_names(root: &std::path::Path, dir: &std::path::Path, names: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    spec_names(root, &path, names);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "js" || extension == "json")
                {
                    let name = path.with_extension("");
                    names.push(name.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        fn count(spec: &Subcommand, subcommands: &mut usize, generators: &mut usize) {
            *subcommands += spec.subcommands.len();
            let args = spec
                .args
                .iter()
                .chain(spec.options.iter().flat_map(|option| &option.args));
            *generators += args.map(|arg| arg.generators.len()).sum::<usize>();
            for sub in &spec.subcommands {
                count(sub, subcommands, generators);
            }
        }

        let dirs = SpecStore::default_dirs();
        let mut names = Vec::new();
        for dir in &dirs {
            spec_names(dir, dir, &mut names);
        }
        names.sort();
        let mut store = SpecStore::new(dirs);
        let (mut loaded, mut subcommands, mut generators) = (0, 0, 0);
        let (mut failed, mut slowest) = (Vec::new(), (std::time::Duration::ZERO, String::new()));
        let started = std::time::Instant::now();
        for name in &names {
            let began = std::time::Instant::now();
            match store.load(name) {
                Some(spec) => {
                    loaded += 1;
                    count(&spec, &mut subcommands, &mut generators);
                }
                None => failed.push(name.as_str()),
            }
            if began.elapsed() > slowest.0 {
                slowest = (began.elapsed(), name.clone());
            }
        }
        println!("{loaded} of {} specs loaded in {:?}", names.len(), started.elapsed());
        println!("{subcommands} subcommands, {generators} generators");
        println!("slowest: {} in {:?}", slowest.1, slowest.0);
        println!("not specs ({}): {}", failed.len(), failed.join(" "));
    }

    #[test]
    fn spec_names_cannot_escape_the_directory() {
        for ok in ["git", "aws/s3", "g++", "python3.12", "@scope/tool"] {
            assert!(is_safe_name(ok), "{ok}");
        }
        for bad in [
            "",
            "..",
            "../etc/passwd",
            "/etc/passwd",
            "a/../b",
            ".hidden",
            "a//b",
            "a b",
            "a\0b",
        ] {
            assert!(!is_safe_name(bad), "{bad}");
        }
    }

    #[test]
    fn store_reads_caches_and_lists() {
        let dir = std::env::temp_dir().join(format!("quince-store-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("aws")).unwrap();
        std::fs::write(dir.join("git.json"), r#"{"name":"git","subcommands":[{"name":"add"}]}"#).unwrap();
        std::fs::write(dir.join("aws/s3.json"), r#"{"name":"s3"}"#).unwrap();
        std::fs::write(dir.join("broken.json"), "{ not json").unwrap();
        std::fs::write(dir.join("README.md"), "ignored").unwrap();
        // A published Fig spec, as JavaScript, and the package's index file beside it.
        std::fs::write(
            dir.join("npm.js"),
            "export default { name: 'npm', args: { generators: { custom: async () => ['x'] } } };",
        )
        .unwrap();
        std::fs::write(dir.join("index.js"), "export default ['npm'];").unwrap();

        let mut store = SpecStore::new(vec![dir.clone()]);
        assert_eq!(store.load("git").unwrap().subcommands[0].names, ["add"]);
        assert_eq!(store.load("aws/s3").unwrap().names, ["s3"]);
        assert!(store.load("broken").is_none());
        assert!(store.load("missing").is_none());
        assert!(store.load("../git").is_none());
        assert_eq!(&*store.names(), ["broken", "git", "npm"]);
        let npm = store.load("npm").unwrap();
        assert!(matches!(npm.args[0].generators[0].source, Source::Engine(_)));
        assert!(store.load("index").is_none());

        // Served from memory once loaded.
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(store.load("git").is_some());
    }
}
