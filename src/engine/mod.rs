//! The completion engine: edit buffer in, ranked suggestions out. It knows nothing about
//! terminals.
//!
//! What a spec spells out, and the files on disk, are answered at once. Suggestions a spec
//! computes with a generator take a command to run, so the [`Engine`] answers a request more
//! than once: first with what it has, then again as each generator comes in.

pub mod command;
mod files;
pub mod generate;
pub mod js;
pub mod spec;
pub mod tokenize;
pub mod worker;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use command::Output;
use generate::{Generators, Need, Poll, Scope};
use serde_json::json;
use spec::{Arg, DEFAULT_PRIORITY, Opt, QueryTerm, SpecStore, Static, Subcommand, Template};
use tokenize::{Current, Line};
use worker::Event;

/// Upper bound on suggestions returned for one request.
pub const MAX_ITEMS: usize = 200;

/// How deep `sudo env nohup ...` style nesting is followed.
const MAX_COMMAND_DEPTH: usize = 8;

/// Words that may precede a command without being one.
const SHELL_KEYWORDS: &[&str] = &[
    "if",
    "then",
    "else",
    "elif",
    "do",
    "while",
    "until",
    "!",
    "{",
    "time",
    "noglob",
    "nocorrect",
];

#[derive(Debug, Clone)]
pub struct Request {
    pub seq: u64,
    pub buffer: String,
    /// Byte offset of the cursor in `buffer`.
    pub cursor: usize,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub seq: u64,
    /// The token under the cursor as typed, up to the cursor. Accepting a suggestion replaces
    /// exactly this text.
    pub token: String,
    /// The spec has suggestions for this position that the engine cannot compute, so `items`
    /// is known to be missing some.
    pub incomplete: bool,
    pub items: Vec<Suggestion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    pub description: String,
    /// Shell text that replaces [`Completion::token`].
    pub insert: String,
    /// Characters to move the cursor back after inserting.
    pub cursor_back: usize,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Subcommand,
    Option,
    Arg,
    File,
    Folder,
}

/// Answers requests, running generators as specs call for them.
///
/// Everything here happens on one thread and none of it waits: commands run elsewhere and
/// come back through [`Engine::command_done`]. Answers pile up for [`Engine::take_outgoing`].
pub struct Engine {
    store: SpecStore,
    generators: Generators,
    /// The request being answered. A newer one replaces it.
    current: Option<Request>,
    /// The last answer given to `current`, so the same one is not given twice.
    sent: Option<Completion>,
    outgoing: Vec<Completion>,
}

impl Engine {
    /// `events` is where the commands this engine starts report back; whoever reads it passes
    /// [`Event::CommandDone`] on to [`Engine::command_done`].
    pub fn new(store: SpecStore, events: Sender<Event>) -> Engine {
        Engine {
            store,
            generators: Generators::new(events),
            current: None,
            sent: None,
            outgoing: Vec::new(),
        }
    }

    pub fn request(&mut self, request: Request) {
        self.current = Some(request);
        self.sent = None;
        self.respond();
    }

    pub fn command_done(&mut self, command: u32, output: &Output) {
        if self.generators.command_done(self.store.js(), command, output) {
            self.respond();
        }
    }

    /// Sets the environment generator commands run with: the user's shell's, which may have
    /// moved on from what this process started with.
    pub fn set_environment(&mut self, environment: Vec<(String, String)>) {
        self.generators.set_environment(environment);
    }

    /// Gives up on whatever has run too long as of `now`.
    pub fn tick(&mut self, now: Instant) {
        if self.generators.expire(now) {
            self.respond();
        }
    }

    /// When [`Engine::tick`] next needs calling, if nothing else happens first.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.generators.next_deadline()
    }

    /// No generator is still running for the current request.
    pub fn is_idle(&self) -> bool {
        self.generators.is_idle()
    }

    pub fn take_outgoing(&mut self) -> Vec<Completion> {
        std::mem::take(&mut self.outgoing)
    }

    /// Answers the current request with everything known right now.
    fn respond(&mut self) {
        let Some(request) = self.current.clone() else { return };
        let completion = match line_before_cursor(&request) {
            Some(line) => self.assemble(&request, &line),
            None => Completion {
                seq: request.seq,
                ..Completion::default()
            },
        };
        if self.sent.as_ref() != Some(&completion) {
            self.sent = Some(completion.clone());
            self.outgoing.push(completion);
        }
    }

    fn assemble(&mut self, request: &Request, line: &Line) -> Completion {
        let mut out = outline(&mut self.store, request, line);
        let needs = std::mem::take(&mut out.needs);
        self.generators.enter(Scope {
            cwd: request.cwd.clone(),
            words: out.words.clone(),
        });
        let mut tokens = out.words.clone();
        tokens.push(line.current.value.clone());

        for need in &needs {
            let generator = &need.generator;
            if generator.filters_templates
                && let Some(template) = generator.templates.first()
            {
                match self.filtered_files(need, *template, &request.cwd) {
                    Some((typed_dir, items)) => out.scoped(
                        format!("{}{typed_dir}", need.prefix),
                        file_name(&need.query),
                        false,
                        &items,
                    ),
                    None => out.incomplete = true,
                }
            }
            if !generator.runnable {
                continue;
            }
            match self
                .generators
                .poll(self.store.js(), generator, &tokens, &line.current.value)
            {
                Poll::Ready(items) => {
                    let (kept, query) = self.query_term(need);
                    out.scoped(format!("{}{kept}", need.prefix), &query, need.fuzzy, &items);
                }
                // Not there yet, or never will be: either way the list is short of something.
                Poll::Pending | Poll::Unavailable => out.incomplete = true,
            }
        }
        Completion {
            seq: request.seq,
            token: line.current.raw.clone(),
            incomplete: out.incomplete,
            items: out.finish(),
        }
    }

    /// Splits what a need is completing into the part its generator's suggestions leave alone
    /// and the part they are matched against and replace.
    fn query_term(&mut self, need: &Need) -> (String, String) {
        let query = need.query.as_str();
        let term_start = match &need.generator.query_term {
            QueryTerm::Token => 0,
            QueryTerm::After(text) => query.rfind(text.as_str()).map_or(0, |at| at + text.len()),
            QueryTerm::Function => self
                .store
                .js()
                .and_then(|js| {
                    let number = self.generators.number(js, &need.generator)?;
                    js.query_term(number, query)
                })
                .filter(|term| query.ends_with(term.as_str()))
                .map_or(0, |term| query.len() - term.len()),
        };
        let (kept, term) = query.split_at(term_start);
        (kept.to_owned(), term.to_owned())
    }

    /// The files a template would suggest, put through the generator's own filter. Returns
    /// the directory part of the path as typed along with what the filter let through.
    fn filtered_files(&mut self, need: &Need, template: Template, cwd: &Path) -> Option<(String, Vec<Static>)> {
        let listing = files::list(&need.query, template, cwd);
        let template_name = match template {
            Template::Filepaths => "filepaths",
            Template::Folders => "folders",
        };
        let items: Vec<_> = listing
            .entries
            .iter()
            .map(|(name, is_dir)| {
                json!({
                    "name": if *is_dir { format!("{name}/") } else { name.clone() },
                    "type": if *is_dir { "folder" } else { "file" },
                    "context": { "templateType": template_name },
                })
            })
            .collect();
        let js = self.store.js()?;
        let number = self.generators.number(js, &need.generator)?;
        let kept: serde_json::Value =
            serde_json::from_str(&js.filter_templates(number, &json!(items).to_string())?).ok()?;
        Some((
            listing.typed_dir,
            kept.as_array()?.iter().filter_map(Static::from_json).collect(),
        ))
    }
}

/// The last component of a partly typed path.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The text left of the cursor, split into words. `None` inside a comment.
fn line_before_cursor(request: &Request) -> Option<Line> {
    let mut cursor = request.cursor.min(request.buffer.len());
    while !request.buffer.is_char_boundary(cursor) {
        cursor -= 1;
    }
    tokenize::tokenize(&request.buffer[..cursor])
}

/// Everything that can be said about `line` without running anything, along with the
/// generators that have more to add.
fn outline<'a>(store: &mut SpecStore, request: &Request, line: &'a Line) -> Collector<'a> {
    let mut out = Collector::new(&line.current);
    if line.redirect {
        files::suggest(&mut out, Template::Filepaths, &request.cwd);
    } else {
        command(store, &line.words, &mut out, &request.cwd, 0);
    }
    out
}

/// The suggestions for a line that need nothing run: what the spec spells out, and files.
/// Where a generator would have more, the result says it is incomplete.
pub fn complete(store: &mut SpecStore, request: &Request) -> Completion {
    let Some(line) = line_before_cursor(request) else {
        return Completion {
            seq: request.seq,
            ..Completion::default()
        };
    };
    let out = outline(store, request, &line);
    Completion {
        seq: request.seq,
        token: line.current.raw.clone(),
        incomplete: out.incomplete || !out.needs.is_empty(),
        items: out.finish(),
    }
}

/// Completes the current token given the completed `words` of one command.
fn command(store: &mut SpecStore, words: &[String], out: &mut Collector, cwd: &Path, depth: usize) {
    let skip = words
        .iter()
        .take_while(|word| is_assignment(word) || SHELL_KEYWORDS.contains(&word.as_str()))
        .count();
    // Generators are given the words of the innermost command, as after `sudo`.
    out.words = words[skip..].to_vec();
    let Some((name, rest)) = words[skip..].split_first() else {
        command_names(store, out, cwd);
        return;
    };
    let name = name.rsplit('/').next().unwrap_or(name);
    let Some(root) = store.load(name) else { return };

    let mut walker = Walker::new(root);
    for (index, word) in rest.iter().enumerate() {
        if walker.step(store, word) == Step::NestedCommand {
            if depth < MAX_COMMAND_DEPTH {
                command(store, &rest[index..], out, cwd, depth + 1);
            }
            return;
        }
    }
    walker.suggest(store, out, cwd);
}

fn command_names(store: &mut SpecStore, out: &mut Collector, cwd: &Path) {
    // An empty prompt gets no popup.
    if out.query.is_empty() {
        return;
    }
    if is_path(&out.query) {
        files::suggest_programs(out, cwd);
        return;
    }
    for name in store.names().iter() {
        out.offer(Offer {
            names: std::slice::from_ref(name),
            kind: Kind::Command,
            ..Offer::default()
        });
    }
}

/// Whether a word typed where a command goes names it by where it is, not by what it is called.
fn is_path(word: &str) -> bool {
    word.contains('/')
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn looks_like_option(word: &str) -> bool {
    word.len() > 1 && word.starts_with('-')
}

pub(crate) fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.to_lowercase().starts_with(&prefix.to_lowercase())
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Continue,
    /// The word begins a command of its own, as after `sudo`.
    NestedCommand,
}

/// An option whose arguments are still being read.
struct Pending {
    opt: Arc<Opt>,
    arg: usize,
    /// At least one value has been read for a variadic argument.
    started: bool,
}

/// Position in a spec after reading the completed words of a command.
struct Walker {
    node: Arc<Subcommand>,
    /// Persistent options inherited from the subcommands above `node`.
    inherited: Vec<Arc<Opt>>,
    arg_index: usize,
    positional_seen: bool,
    pending: Option<Pending>,
    used: Vec<Arc<Opt>>,
    options_ended: bool,
    posix_noncompliant: bool,
}

impl Walker {
    fn new(root: Arc<Subcommand>) -> Self {
        Self {
            posix_noncompliant: root.flags_posix_noncompliant,
            node: root,
            inherited: Vec::new(),
            arg_index: 0,
            positional_seen: false,
            pending: None,
            used: Vec::new(),
            options_ended: false,
        }
    }

    fn step(&mut self, store: &mut SpecStore, word: &str) -> Step {
        if let Some(pending) = self.pending.take() {
            let arg = &pending.opt.args[pending.arg];
            let (variadic, skippable) = (arg.variadic, arg.optional || (arg.variadic && pending.started));
            if !(skippable && !self.options_ended && looks_like_option(word)) {
                if variadic {
                    self.pending = Some(Pending {
                        started: true,
                        ..pending
                    });
                } else if pending.arg + 1 < pending.opt.args.len() {
                    self.pending = Some(Pending {
                        arg: pending.arg + 1,
                        started: false,
                        ..pending
                    });
                }
                return Step::Continue;
            }
        }

        if !self.options_ended && word == "--" {
            self.options_ended = true;
            return Step::Continue;
        }
        if !self.options_ended && looks_like_option(word) {
            self.option(word);
            return Step::Continue;
        }

        if !self.positional_seen
            && let Some(sub) = self
                .node
                .subcommands
                .iter()
                .find(|sub| sub.names.iter().any(|name| name == word))
        {
            self.descend(store, sub.clone());
            return Step::Continue;
        }

        match self.node.args.get(self.arg_index) {
            Some(arg) if arg.is_command => return Step::NestedCommand,
            Some(arg) if !arg.variadic => self.arg_index += 1,
            _ => {}
        }
        self.positional_seen = true;
        Step::Continue
    }

    fn descend(&mut self, store: &mut SpecStore, sub: Arc<Subcommand>) {
        self.inherited
            .extend(self.node.options.iter().filter(|opt| opt.persistent).cloned());
        let loaded = sub.load_spec.as_deref().and_then(|name| store.load(name));
        self.node = loaded.unwrap_or(sub);
        self.posix_noncompliant |= self.node.flags_posix_noncompliant;
        self.arg_index = 0;
        self.positional_seen = false;
        self.used.clear();
    }

    /// Options valid here: the current subcommand's own, then the inherited ones.
    fn options(&self) -> impl Iterator<Item = &Arc<Opt>> {
        self.node.options.iter().chain(&self.inherited)
    }

    fn find_option(&self, name: &str) -> Option<Arc<Opt>> {
        self.options().find(|opt| opt.names.iter().any(|n| n == name)).cloned()
    }

    /// Recognises `--name=value` (or whatever separator the option declares) and returns the
    /// option with the offset at which the value starts.
    fn split_inline(&self, word: &str) -> Option<(Arc<Opt>, usize)> {
        self.options().find_map(|opt| {
            let separator = opt.separator.as_deref().unwrap_or("=");
            opt.names.iter().find_map(|name| {
                let rest = word.strip_prefix(name.as_str())?;
                rest.starts_with(separator)
                    .then(|| (opt.clone(), name.len() + separator.len()))
            })
        })
    }

    fn option(&mut self, word: &str) {
        if let Some(opt) = self.find_option(word) {
            self.used.push(opt.clone());
            if !opt.args.is_empty() {
                self.pending = Some(Pending {
                    opt,
                    arg: 0,
                    started: false,
                });
            }
            return;
        }
        if let Some((opt, _)) = self.split_inline(word) {
            self.used.push(opt);
            return;
        }
        if self.posix_noncompliant || word.starts_with("--") {
            return;
        }
        // `-abc` is `-a -b -c`. The first flag that takes an argument ends the chain: what
        // follows it in the word is its value, or the next word is.
        let flags = &word[1..];
        for (offset, flag) in flags.char_indices() {
            let Some(opt) = self.find_option(&format!("-{flag}")) else {
                return;
            };
            self.used.push(opt.clone());
            if !opt.args.is_empty() {
                if offset + flag.len_utf8() == flags.len() {
                    self.pending = Some(Pending {
                        opt,
                        arg: 0,
                        started: false,
                    });
                }
                return;
            }
        }
    }

    fn suggest(&self, store: &mut SpecStore, out: &mut Collector, cwd: &Path) {
        if let Some(pending) = &self.pending {
            let arg = &pending.opt.args[pending.arg];
            self.suggest_arg(store, out, arg, cwd);
            if !(arg.optional || (arg.variadic && pending.started)) {
                return;
            }
        }

        if !self.options_ended && out.query.starts_with('-') {
            let typed = out.query.clone();
            match self.split_inline(&typed) {
                Some((opt, value_start)) => {
                    // `--color=al`: complete the value and keep the option in front of it.
                    if let Some(arg) = opt.args.first() {
                        out.prefix = typed[..value_start].to_owned();
                        out.query = typed[value_start..].to_owned();
                        self.suggest_arg(store, out, arg, cwd);
                    }
                }
                None => self.suggest_options(out),
            }
            return;
        }

        if !self.positional_seen {
            for sub in &self.node.subcommands {
                out.offer(Offer {
                    names: &sub.names,
                    description: &sub.description,
                    insert_value: sub.insert_value.as_deref(),
                    kind: Kind::Subcommand,
                    priority: sub.priority,
                    hidden: sub.hidden,
                    ..Offer::default()
                });
            }
        }
        if let Some(arg) = self.node.args.get(self.arg_index) {
            self.suggest_arg(store, out, arg, cwd);
        }
        for extra in &self.node.additional {
            out.offer_static(extra);
        }
        if !self.options_ended {
            self.suggest_options(out);
        }
    }

    fn suggest_arg(&self, store: &mut SpecStore, out: &mut Collector, arg: &Arg, cwd: &Path) {
        if arg.is_command {
            command_names(store, out, cwd);
        }
        let outer = std::mem::replace(&mut out.fuzzy, arg.fuzzy);
        for suggestion in &arg.suggestions {
            out.offer_static(suggestion);
        }
        out.fuzzy = outer;
        // A template may sit on the argument or on one of its generators. A generator that
        // filters its template's suggestions has to be asked; the rest are listed here.
        let plain = arg.generators.iter().filter(|generator| !generator.filters_templates);
        let templates: Vec<Template> = arg
            .templates
            .iter()
            .chain(plain.flat_map(|generator| &generator.templates))
            .copied()
            .collect();
        // `filepaths` lists directories too, so `folders` alongside it would only duplicate.
        let template = match templates.contains(&Template::Filepaths) {
            true => Some(Template::Filepaths),
            false => templates.first().copied(),
        };
        // A path to a command has had its programs listed above; all files would repeat them.
        let listed = arg.is_command && is_path(&out.query);
        if let Some(template) = template.filter(|_| !listed) {
            files::suggest(out, template, cwd);
        }
        for generator in &arg.generators {
            if generator.runnable || generator.filters_templates {
                out.needs.push(Need {
                    generator: generator.clone(),
                    prefix: out.prefix.clone(),
                    query: out.query.clone(),
                    fuzzy: arg.fuzzy,
                });
            }
        }
    }

    fn suggest_options(&self, out: &mut Collector) {
        for opt in self.options() {
            let used = self.used.iter().any(|seen| Arc::ptr_eq(seen, opt));
            if (used && !opt.repeatable) || self.excluded(opt) {
                continue;
            }
            out.offer(Offer {
                names: &opt.names,
                description: &opt.description,
                insert_value: opt.insert_value.as_deref(),
                suffix: opt.separator.as_deref().unwrap_or(" "),
                kind: Kind::Option,
                priority: opt.priority,
                hidden: opt.hidden,
                ..Offer::default()
            });
        }
    }

    /// An option is ruled out by one already on the line if either declares the other exclusive.
    fn excluded(&self, opt: &Opt) -> bool {
        let names_any = |names: &[String], listed: &[String]| names.iter().any(|name| listed.contains(name));
        self.used.iter().any(|seen| {
            !std::ptr::eq(seen.as_ref(), opt)
                && (names_any(&seen.names, &opt.exclusive_on) || names_any(&opt.names, &seen.exclusive_on))
        })
    }
}

/// One thing a spec has on offer, before it is known whether it matches what was typed.
struct Offer<'a> {
    /// Any of these may be typed; the one that matches is what gets inserted.
    names: &'a [String],
    /// Shown in place of the names.
    label: Option<&'a str>,
    description: &'a str,
    /// Inserted as written, in place of the matching name.
    insert_value: Option<&'a str>,
    /// Follows an inserted name.
    suffix: &'a str,
    kind: Kind,
    priority: i32,
    hidden: bool,
}

impl Default for Offer<'_> {
    fn default() -> Self {
        Offer {
            names: &[],
            label: None,
            description: "",
            insert_value: None,
            suffix: " ",
            kind: Kind::Arg,
            priority: DEFAULT_PRIORITY,
            hidden: false,
        }
    }
}

impl Kind {
    /// Where a kind of suggestion goes among others of the same priority: what continues the
    /// command first, its options last.
    fn rank(self) -> u8 {
        match self {
            Kind::Command | Kind::Subcommand => 0,
            Kind::Arg | Kind::File | Kind::Folder => 1,
            Kind::Option => 2,
        }
    }
}

/// Gathers the suggestions that match what has been typed.
pub(crate) struct Collector<'a> {
    current: &'a Current,
    /// The text suggestions are matched against. Usually the whole token.
    pub(crate) query: String,
    /// Text of the token that precedes `query` and must be kept, as in `--color=`.
    prefix: String,
    /// Whether suggestions being offered right now may match anywhere, not only at the start.
    fuzzy: bool,
    /// The words of the command being completed, without the token under the cursor.
    words: Vec<String>,
    incomplete: bool,
    /// Each with how closely it matched, its priority, and its kind's rank.
    items: Vec<(Closeness, i32, u8, Suggestion)>,
    /// Generators with more to add at this position.
    needs: Vec<Need>,
}

impl<'a> Collector<'a> {
    fn new(current: &'a Current) -> Self {
        Self {
            current,
            query: current.value.clone(),
            prefix: String::new(),
            fuzzy: false,
            words: Vec::new(),
            incomplete: false,
            items: Vec::new(),
            needs: Vec::new(),
        }
    }

    fn offer(&mut self, offer: Offer) {
        // Hidden suggestions only appear once typed in full.
        let closeness = |name: &String| match offer.hidden {
            true => (*name == self.query).then_some(Closeness::Start),
            false => Closeness::of(name, &self.query, self.fuzzy),
        };
        // Of several names, the one that matches best is the one that gets inserted.
        let best = offer.names.iter().filter_map(|name| Some((closeness(name)?, name)));
        let Some((closeness, matched)) = best.min_by_key(|(closeness, _)| *closeness) else {
            return;
        };
        let (insert, cursor_back) = match offer.insert_value {
            Some(literal) => self.literal(literal),
            // A directory is left open, so the next level can be completed straight away.
            None => (self.shell_text(matched, offer.suffix, !matched.ends_with('/')), 0),
        };
        let suggestion = Suggestion {
            label: offer.label.map_or_else(|| offer.names.join(", "), str::to_owned),
            description: offer.description.lines().next().unwrap_or_default().trim().to_owned(),
            insert,
            cursor_back,
            kind: offer.kind,
        };
        self.items
            .push((closeness, offer.priority, offer.kind.rank(), suggestion));
    }

    /// Offers a suggestion as a spec gives it, literally or out of a generator.
    fn offer_static(&mut self, item: &Static) {
        let open = item.names.iter().all(|name| name.ends_with('/'));
        let kind = match item.kind.as_deref() {
            Some("folder") => Kind::Folder,
            Some("file") => Kind::File,
            Some("subcommand") => Kind::Subcommand,
            Some("option") => Kind::Option,
            _ if open => Kind::Folder,
            _ => Kind::Arg,
        };
        self.offer(Offer {
            names: &item.names,
            label: item.display_name.as_deref(),
            description: &item.description,
            insert_value: item.insert_value.as_deref(),
            suffix: if open { "" } else { " " },
            kind,
            priority: item.priority,
            hidden: item.hidden,
        });
    }

    /// Offers `items` as completions of `query` alone, with `prefix` kept in front. This is
    /// how a generator completes one component of a path and leaves the rest of it be.
    fn scoped(&mut self, prefix: String, query: &str, fuzzy: bool, items: &[Static]) {
        let outer = (
            std::mem::replace(&mut self.prefix, prefix),
            std::mem::replace(&mut self.query, query.to_owned()),
            std::mem::replace(&mut self.fuzzy, fuzzy),
        );
        for item in items {
            self.offer_static(item);
        }
        (self.prefix, self.query, self.fuzzy) = outer;
    }

    pub(crate) fn file(&mut self, typed_dir: &str, name: &str, is_dir: bool) {
        // A directory is left open so the next level can be completed straight away.
        let (label, insert, kind) = match is_dir {
            true => (
                format!("{name}/"),
                self.shell_text(&format!("{typed_dir}{name}/"), "", false),
                Kind::Folder,
            ),
            false => (
                name.to_owned(),
                self.shell_text(&format!("{typed_dir}{name}"), " ", true),
                Kind::File,
            ),
        };
        let suggestion = Suggestion {
            label,
            description: String::new(),
            insert,
            cursor_back: 0,
            kind,
        };
        self.items
            .push((Closeness::Start, DEFAULT_PRIORITY, kind.rank(), suggestion));
    }

    /// Quotes `value` the way the token was started and appends `suffix`.
    fn shell_text(&self, value: &str, suffix: &str, close_quote: bool) -> String {
        let mut text = tokenize::quote(&format!("{}{value}", self.prefix), self.current.quote);
        if close_quote && let Some(quote) = self.current.quote {
            text.push(quote);
        }
        text.push_str(suffix);
        text
    }

    /// A spec's `insertValue` is inserted as written. `{cursor}` marks where the cursor ends up.
    fn literal(&self, insert_value: &str) -> (String, usize) {
        let clean: String = insert_value.chars().filter(|c| !c.is_control()).collect();
        let (before, after) = clean.split_once("{cursor}").unwrap_or((&clean, ""));
        (format!("{}{before}{after}", self.prefix), after.chars().count())
    }

    /// What matches from its first letter comes first, then highest priority. Within a
    /// priority, what continues the command comes before its options, and otherwise the order
    /// the spec lists things in is kept.
    fn finish(mut self) -> Vec<Suggestion> {
        self.items
            .sort_by_key(|(closeness, priority, rank, _)| (*closeness, std::cmp::Reverse(*priority), *rank));
        self.items.truncate(MAX_ITEMS);
        self.items.into_iter().map(|(.., suggestion)| suggestion).collect()
    }
}

/// How a suggestion matches what was typed, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Closeness {
    /// It starts with it.
    Start,
    /// It contains it.
    Within,
    /// It contains its letters, in order, with others between.
    Scattered,
}

impl Closeness {
    /// Case is ignored. Anything short of a match at the start only counts when `fuzzy`.
    fn of(name: &str, typed: &str, fuzzy: bool) -> Option<Closeness> {
        let (name, typed) = (name.to_lowercase(), typed.to_lowercase());
        if name.starts_with(&typed) {
            return Some(Closeness::Start);
        }
        if !fuzzy {
            return None;
        }
        if name.contains(&typed) {
            return Some(Closeness::Within);
        }
        let mut letters = name.chars();
        typed
            .chars()
            .all(|wanted| letters.any(|letter| letter == wanted))
            .then_some(Closeness::Scattered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> SpecStore {
        let mut store = SpecStore::new(Vec::new());
        let specs = [
            json!({
                "name": "git",
                "options": [
                    { "name": "--version", "description": "Show version" },
                    { "name": ["-C"], "args": { "name": "path", "template": "folders" }, "isPersistent": true },
                    { "name": "--no-pager", "isPersistent": true },
                ],
                "subcommands": [
                    { "name": "add", "description": "Add file contents to the index",
                      "args": { "name": "pathspec", "template": "filepaths", "isVariadic": true, "isOptional": true },
                      "options": [
                          { "name": ["-A", "--all"], "exclusiveOn": ["-u"] },
                          { "name": ["-u", "--update"] },
                          { "name": ["-v", "--verbose"], "isRepeatable": true },
                      ] },
                    { "name": "checkout", "description": "Switch branches",
                      "args": { "name": "branch", "suggestions": ["main", { "name": "develop", "description": "Integration" }] },
                      "options": [{ "name": "-b", "args": { "name": "new branch" } }] },
                    { "name": "cherry-pick", "description": "Apply commits" },
                    { "name": "commit", "priority": 90,
                      "options": [
                          { "name": ["-m", "--message"], "insertValue": "-m '{cursor}'", "args": { "name": "msg" } },
                          { "name": "--amend" },
                          { "name": "--cleanup", "requiresSeparator": true,
                            "args": { "suggestions": ["strip", "scissors", "verbatim"] } },
                          { "name": "--fixup", "args": { "name": "commit", "isOptional": true, "suggestions": ["HEAD"] } },
                      ] },
                    { "name": "remote", "subcommands": [{ "name": "add" }, { "name": ["remove", "rm"] }] },
                    { "name": "secret", "hidden": true },
                ],
            }),
            json!({ "name": "sudo", "options": [{ "name": "-u", "args": { "name": "user" } }], "args": { "name": "command", "isCommand": true } }),
            json!({ "name": "arch", "args": { "name": "program", "template": "filepaths", "isCommand": true } }),
            json!({ "name": "go", "parserDirectives": { "flagsArePosixNoncompliant": true },
                    "subcommands": [{ "name": "build", "options": [{ "name": "-v" }, { "name": "-race" }] }] }),
            json!({ "name": "aws", "subcommands": [{ "name": "s3", "loadSpec": "aws/s3" }] }),
            json!({ "name": "s3", "subcommands": [{ "name": "ls" }, { "name": "cp" }] }),
            json!({ "name": "kubectl", "subcommands": [{ "name": "get", "options": [{ "name": "-o" }],
                    "args": { "name": "resource", "generators": { "script": ["kubectl", "api-resources"] } } }] }),
        ];
        store.insert(
            "merge",
            Subcommand::from_json(&json!({
                "name": "merge",
                "options": [{ "name": "--log" }],
                "args": { "name": "branch", "filterStrategy": "fuzzy", "suggestions": [
                    "main", "feature/login", "fix/typo", { "name": "login-page", "priority": 10 }, { "name": ["lg", "long"] },
                ] },
            }))
            .unwrap(),
        );
        for (name, spec) in ["git", "sudo", "arch", "go", "aws", "aws/s3", "kubectl"]
            .into_iter()
            .zip(&specs)
        {
            store.insert(name, Subcommand::from_json(spec).unwrap());
        }
        store
    }

    fn complete_in(buffer: &str, cwd: &Path) -> Completion {
        let request = Request {
            seq: 7,
            buffer: buffer.to_owned(),
            cursor: buffer.len(),
            cwd: cwd.to_owned(),
        };
        complete(&mut store(), &request)
    }

    fn labels(buffer: &str) -> Vec<String> {
        complete_in(buffer, Path::new("/nonexistent"))
            .items
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    fn inserts(buffer: &str) -> Vec<String> {
        complete_in(buffer, Path::new("/nonexistent"))
            .items
            .into_iter()
            .map(|item| item.insert)
            .collect()
    }

    #[test]
    fn subcommands_then_options_ranked_by_priority() {
        assert_eq!(
            labels("git "),
            [
                "commit",
                "add",
                "checkout",
                "cherry-pick",
                "remote",
                "--version",
                "-C",
                "--no-pager"
            ]
        );
    }

    #[test]
    fn filters_by_prefix_ignoring_case() {
        assert_eq!(labels("git ch"), ["checkout", "cherry-pick"]);
        assert_eq!(labels("git CHE"), ["checkout", "cherry-pick"]);
        assert_eq!(labels("git zzz"), Vec::<String>::new());
    }

    #[test]
    fn reports_the_token_and_what_replaces_it() {
        let completion = complete_in("git ch", Path::new("/nonexistent"));
        assert_eq!(completion.seq, 7);
        assert_eq!(completion.token, "ch");
        assert_eq!(completion.items[0].insert, "checkout ");
        assert_eq!(completion.items[0].description, "Switch branches");
        assert_eq!(completion.items[0].kind, Kind::Subcommand);
    }

    #[test]
    fn completes_at_the_cursor_not_the_end() {
        let request = Request {
            seq: 1,
            buffer: "git ch --version".into(),
            cursor: 6,
            cwd: "/nonexistent".into(),
        };
        let completion = complete(&mut store(), &request);
        assert_eq!(completion.token, "ch");
        assert_eq!(completion.items.len(), 2);
    }

    #[test]
    fn hidden_suggestions_need_the_full_name() {
        assert_eq!(labels("git sec"), Vec::<String>::new());
        assert_eq!(labels("git secret"), ["secret"]);
    }

    #[test]
    fn descends_into_subcommands() {
        assert_eq!(labels("git remote "), ["add", "remove, rm", "-C", "--no-pager"]);
        assert_eq!(inserts("git remote r"), ["remove "]);
        // An alias inserts the name that was being typed.
        assert_eq!(inserts("git remote rm"), ["rm "]);
    }

    #[test]
    fn static_argument_suggestions() {
        assert_eq!(labels("git checkout "), ["main", "develop", "-b", "-C", "--no-pager"]);
        assert_eq!(labels("git checkout d"), ["develop"]);
        // After the only positional argument there is nothing more to offer but options.
        assert_eq!(labels("git checkout main "), ["-b", "-C", "--no-pager"]);
    }

    #[test]
    fn options_when_a_dash_is_typed() {
        assert_eq!(
            labels("git commit -"),
            ["-m, --message", "--amend", "--cleanup", "--fixup", "-C", "--no-pager"]
        );
        assert_eq!(labels("git commit --a"), ["--amend"]);
        // Persistent options are inherited; non-persistent ones are not.
        assert!(!labels("git commit --").contains(&"--version".to_owned()));
    }

    #[test]
    fn used_and_exclusive_options_are_dropped() {
        assert_eq!(labels("git add -u -"), ["-v, --verbose", "-C", "--no-pager"]);
        assert_eq!(labels("git add -A -"), ["-v, --verbose", "-C", "--no-pager"]);
        assert_eq!(
            labels("git add -v -"),
            ["-A, --all", "-u, --update", "-v, --verbose", "-C", "--no-pager"]
        );
        // Chained short flags count as used too.
        assert_eq!(labels("git add -vu -"), ["-v, --verbose", "-C", "--no-pager"]);
    }

    #[test]
    fn an_option_argument_takes_over() {
        // `-b` needs a value, and the spec does not say what it could be.
        assert_eq!(labels("git checkout -b "), Vec::<String>::new());
        assert_eq!(
            labels("git checkout -b topic "),
            ["main", "develop", "-C", "--no-pager"]
        );
        // An optional argument leaves the rest on offer.
        assert_eq!(
            labels("git commit --fixup "),
            ["HEAD", "-m, --message", "--amend", "--cleanup", "-C", "--no-pager"]
        );
        assert_eq!(labels("git commit --fixup --am"), ["--amend"]);
    }

    #[test]
    fn inline_option_values() {
        assert_eq!(inserts("git commit --cl"), ["--cleanup="]);
        assert_eq!(labels("git commit --cleanup="), ["strip", "scissors", "verbatim"]);
        assert_eq!(
            inserts("git commit --cleanup=s"),
            ["--cleanup=strip ", "--cleanup=scissors "]
        );
        assert_eq!(
            labels("git commit --cleanup=strip -"),
            ["-m, --message", "--amend", "--fixup", "-C", "--no-pager"]
        );
    }

    #[test]
    fn insert_value_places_the_cursor() {
        let completion = complete_in("git commit -m", Path::new("/nonexistent"));
        assert_eq!(completion.items[0].insert, "-m ''");
        assert_eq!(completion.items[0].cursor_back, 1);
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(labels("git checkout -- "), ["main", "develop"]);
        assert_eq!(labels("git checkout -- -"), Vec::<String>::new());
    }

    #[test]
    fn nested_commands() {
        assert_eq!(labels("sudo gi"), ["git"]);
        assert_eq!(labels("sudo git ch"), ["checkout", "cherry-pick"]);
        assert_eq!(labels("sudo -u root git ch"), ["checkout", "cherry-pick"]);
    }

    #[test]
    fn prefixes_that_are_not_the_command() {
        assert_eq!(labels("GIT_PAGER=cat git ch"), ["checkout", "cherry-pick"]);
        assert_eq!(labels("if git ch"), ["checkout", "cherry-pick"]);
        assert_eq!(labels("/usr/bin/git ch"), ["checkout", "cherry-pick"]);
        assert_eq!(labels("ls | git ch"), ["checkout", "cherry-pick"]);
    }

    #[test]
    fn command_names_come_from_the_specs() {
        let dir = fixture_dir("names");
        for name in ["git", "go", "npm"] {
            std::fs::write(dir.join(format!("{name}.json")), "{}").unwrap();
        }
        let mut store = SpecStore::new(vec![dir.clone()]);
        let request = |buffer: &str| Request {
            seq: 0,
            buffer: buffer.into(),
            cursor: buffer.len(),
            cwd: dir.clone(),
        };
        let names = |store: &mut SpecStore, buffer: &str| -> Vec<String> {
            complete(store, &request(buffer))
                .items
                .into_iter()
                .map(|item| item.insert)
                .collect()
        };
        assert_eq!(names(&mut store, "g"), ["git ", "go "]);
        assert_eq!(names(&mut store, ""), Vec::<String>::new());
        // A path is not a command name, and `git.json` here is not a program either.
        assert_eq!(names(&mut store, "./g"), Vec::<String>::new());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn single_dash_long_options() {
        assert_eq!(labels("go build -"), ["-v", "-race"]);
        // `-race` is one option here, not `-r -a -c -e`.
        assert_eq!(labels("go build -race -"), ["-v"]);
    }

    #[test]
    fn load_spec_swaps_in_another_spec() {
        assert_eq!(labels("aws s3 "), ["ls", "cp"]);
    }

    #[test]
    fn says_when_suggestions_are_missing() {
        let at = |buffer: &str| complete_in(buffer, Path::new("/nonexistent"));
        // The resources come from a generator, which is not run: only the option is offered.
        let completion = at("kubectl get ");
        assert!(completion.incomplete);
        assert_eq!(completion.items.len(), 1);
        // Options and subcommands are written out in the spec, so those lists are whole.
        assert!(!at("kubectl get -").incomplete);
        assert!(!at("kubectl ").incomplete);
        assert!(!at("git checkout ").incomplete);
    }

    #[test]
    fn fuzzy_arguments_match_anywhere_with_the_closest_first() {
        // From the start, then inside, then scattered; priority only orders within those.
        assert_eq!(labels("merge log"), ["login-page", "feature/login", "lg, long"]);
        assert_eq!(labels("merge FL"), ["feature/login"]);
        assert_eq!(labels("merge m"), ["main"]);
        // Of an item's names, the closest match is the one inserted. The others here only have
        // the letters scattered through them, and among those the higher priority leads.
        assert_eq!(inserts("merge lon"), ["long ", "feature/login ", "login-page "]);
        // Options are not the argument's suggestions: they still match from the start only.
        assert_eq!(labels("merge -l"), Vec::<String>::new());
        assert_eq!(labels("merge --l"), ["--log"]);
        // Without the spec asking for it, nothing matches in the middle.
        assert_eq!(labels("git checkout ai"), Vec::<String>::new());
    }

    #[test]
    fn unknown_commands_and_comments_get_nothing() {
        assert_eq!(labels("frobnicate --he"), Vec::<String>::new());
        assert_eq!(labels("git status # ch"), Vec::<String>::new());
    }

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quince-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_templates() {
        let dir = fixture_dir("files");
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::create_dir_all(dir.join("my docs")).unwrap();
        for file in [
            "Cargo.toml",
            "main.rs",
            ".hidden",
            "src/lib.rs",
            "src/Main.rs",
            "my docs/a b.txt",
        ] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let items = |buffer: &str| -> Vec<(String, String)> {
            complete_in(buffer, &dir)
                .items
                .into_iter()
                .map(|item| (item.label, item.insert))
                .collect()
        };
        let pair = |label: &str, insert: &str| (label.to_owned(), insert.to_owned());

        // Sorted without regard to case, hidden files left out, then the options.
        let all = items("git add ");
        assert_eq!(
            all[..4],
            [
                pair("Cargo.toml", "Cargo.toml "),
                pair("main.rs", "main.rs "),
                pair("my docs/", r"my\ docs/"),
                pair("src/", "src/"),
            ]
        );
        assert_eq!(items("git add ."), [pair(".hidden", ".hidden ")]);
        assert_eq!(items("git add src/m"), [pair("Main.rs", "src/Main.rs ")]);
        assert_eq!(
            items("git add src/"),
            [
                pair("lib.rs", "src/lib.rs "),
                pair("Main.rs", "src/Main.rs "),
                pair("nested/", "src/nested/")
            ]
        );
        // The quoting style the user started with is kept.
        assert_eq!(items("git add \"my docs/a"), [pair("a b.txt", "\"my docs/a b.txt\" ")]);
        assert_eq!(items(r"git add my\ docs/a"), [pair("a b.txt", r"my\ docs/a\ b.txt ")]);
        // `folders` lists directories only.
        assert_eq!(items("git -C s"), [pair("src/", "src/")]);
        // A redirection target is a file whatever the command.
        assert_eq!(items("frobnicate > Ca"), [pair("Cargo.toml", "Cargo.toml ")]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_in_place_of_a_command_lists_what_can_be_run() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fixture_dir("programs");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        for (file, mode) in [
            ("build.sh", 0o755),
            ("notes.md", 0o644),
            ("scripts/deploy", 0o700),
            ("scripts/data.csv", 0o644),
        ] {
            std::fs::write(dir.join(file), "").unwrap();
            std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(mode)).unwrap();
        }
        std::os::unix::fs::symlink("scripts/deploy", dir.join("ship")).unwrap();
        std::os::unix::fs::symlink("notes.md", dir.join("readme")).unwrap();
        let items = |buffer: &str| -> Vec<(String, String)> {
            complete_in(buffer, &dir)
                .items
                .into_iter()
                .map(|item| (item.label, item.insert))
                .collect()
        };
        let pair = |label: &str, insert: &str| (label.to_owned(), insert.to_owned());

        // Programs and the directories that lead to them, a link counting as what it points at.
        assert_eq!(
            items("./"),
            [
                pair("build.sh", "./build.sh "),
                pair("scripts/", "./scripts/"),
                pair("ship", "./ship ")
            ]
        );
        assert_eq!(items("./b"), [pair("build.sh", "./build.sh ")]);
        assert_eq!(items("scripts/"), [pair("deploy", "scripts/deploy ")]);
        assert_eq!(items("./n"), []);
        // The same wherever a command goes.
        assert_eq!(items("sudo ./b"), [pair("build.sh", "./build.sh ")]);
        assert_eq!(items("ls | ./scripts/d"), [pair("deploy", "./scripts/deploy ")]);
        // A command that may also be any file is listed once, and as a command.
        assert_eq!(items("arch ./b"), [pair("build.sh", "./build.sh ")]);
        assert_eq!(items("arch ./n"), []);
        assert_eq!(items("arch n"), [pair("notes.md", "notes.md ")]);
        // As an argument, a path is still any file.
        assert_eq!(items("git add ./n"), [pair("notes.md", "./notes.md ")]);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// The engine with generators, run against real commands.
#[cfg(test)]
mod generator_tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    /// A directory of specs, and somewhere for their commands to leave files.
    struct Bench {
        dir: PathBuf,
        engine: Engine,
        inbox: Receiver<Event>,
        seq: u64,
    }

    const SPEC: &str = r#"
        const count = (name) => ["sh", "-c", `echo run >> ${name}.count; printf 'apple\nbanana\ncherry'`];
        export default {
          name: "fruit",
          options: [{ name: "--ripe" }],
          subcommands: [
            { name: "pick", options: [{ name: "--all" }], args: { isVariadic: true, suggestions: ["kiwi"], generators: {
                script: count("pick"),
                postProcess: (out, tokens) => out.split("\n").map((name) => ({ name, description: `for ${tokens.join(" ")}` })),
            } } },
            { name: "path", args: { generators: {
                script: (tokens) => ["sh", "-c", `echo run >> path.count; printf 'one/\ntwo\n'`],
                splitOn: "\n",
                trigger: "/",
                getQueryTerm: "/",
            } } },
            { name: "where", args: { generators: {
                custom: async (tokens, execute, context) => {
                  const { stdout } = await execute({ command: "pwd" });
                  return [
                    { name: stdout.trim().split("/").pop(), description: context.currentWorkingDirectory === stdout.trim() ? "same" : "different" },
                    { name: "tokens", displayName: tokens.join("+"), insertValue: "T{cursor}!", priority: 90 },
                  ];
                },
                trigger: (token, previous) => token.length === 3,
                getQueryTerm: (token) => token.slice(token.lastIndexOf(":") + 1),
            } } },
            { name: "broken", args: { suggestions: ["fine"], generators: { custom: async () => { throw new Error("no"); } } } },
            { name: "slow", args: { suggestions: ["fine"], generators: { script: ["sleep", "30"], scriptTimeout: 150, splitOn: "\n" } } },
            { name: "missing", args: { suggestions: ["fine"], generators: { script: ["/nonexistent/program"], splitOn: "\n" } } },
            { name: "json", args: { generators: {
                template: "filepaths",
                filterTemplateSuggestions: (items) => items.filter((item) => item.name.endsWith(".json") || item.name.endsWith("/")),
            } } },
            { name: "inner", args: { generators: { template: "folders" } } },
          ],
        };
    "#;

    impl Bench {
        fn new(name: &str) -> Bench {
            let dir = std::env::temp_dir().join(format!("quince-generators-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("specs")).unwrap();
            let dir = dir.canonicalize().unwrap();
            std::fs::write(dir.join("specs/fruit.js"), SPEC).unwrap();
            std::fs::write(
                dir.join("specs/sudo.json"),
                r#"{ "name": "sudo", "args": { "name": "command", "isCommand": true } }"#,
            )
            .unwrap();
            std::fs::write(
                dir.join("specs/plain.json"),
                r#"{ "name": "plain", "args": { "generators": { "script": ["printf", "x y\nz"], "splitOn": "\n" } } }"#,
            )
            .unwrap();
            let (events, inbox) = channel();
            let engine = Engine::new(SpecStore::new(vec![dir.join("specs")]), events);
            Bench {
                dir,
                engine,
                inbox,
                seq: 0,
            }
        }

        /// Asks for `buffer` and returns the answers given before any command reports back.
        fn ask(&mut self, buffer: &str) -> Vec<Completion> {
            self.seq += 1;
            self.engine.request(Request {
                seq: self.seq,
                buffer: buffer.to_owned(),
                cursor: buffer.len(),
                cwd: self.dir.clone(),
            });
            self.engine.take_outgoing()
        }

        /// Lets every generator finish and returns the answers that came of it.
        fn settle(&mut self) -> Vec<Completion> {
            let give_up = Instant::now() + Duration::from_secs(20);
            let mut answers = Vec::new();
            while !self.engine.is_idle() {
                assert!(Instant::now() < give_up, "generators never finished");
                if let Ok(Event::CommandDone { command, output }) = self.inbox.recv_timeout(Duration::from_millis(10)) {
                    self.engine.command_done(command, &output);
                }
                self.engine.tick(Instant::now());
                answers.extend(self.engine.take_outgoing());
            }
            answers
        }

        /// The final answer for `buffer`.
        fn complete(&mut self, buffer: &str) -> Completion {
            let mut answers = self.ask(buffer);
            answers.extend(self.settle());
            answers.pop().expect("an answer")
        }

        fn runs(&self, name: &str) -> usize {
            std::fs::read_to_string(self.dir.join(format!("{name}.count"))).map_or(0, |text| text.lines().count())
        }
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn labels(completion: &Completion) -> Vec<&str> {
        completion.items.iter().map(|item| item.label.as_str()).collect()
    }

    #[test]
    fn answers_at_once_then_again_with_the_generator() {
        let mut bench = Bench::new("twice");
        let first = bench.ask("fruit pick ");
        assert_eq!(first.len(), 1);
        assert_eq!(labels(&first[0]), ["kiwi", "--all"]);
        assert!(first[0].incomplete, "the generator is still out");

        let later = bench.settle();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].seq, first[0].seq);
        // The generator's suggestions sit with the argument's own, ahead of the options.
        assert_eq!(labels(&later[0]), ["kiwi", "apple", "banana", "cherry", "--all"]);
        assert!(!later[0].incomplete);
        assert_eq!(later[0].items[1].description, "for fruit pick");
        assert_eq!(later[0].items[1].insert, "apple ");
    }

    #[test]
    fn one_run_serves_the_whole_token() {
        let mut bench = Bench::new("once");
        bench.complete("fruit pick ");
        // Typing on filters what is already there: answered in one go, nothing run.
        let answers = bench.ask("fruit pick b");
        assert_eq!(answers.len(), 1);
        assert_eq!(labels(&answers[0]), ["banana"]);
        assert!(!answers[0].incomplete);
        assert!(bench.engine.is_idle());
        assert_eq!(bench.runs("pick"), 1);

        // A new word is a new place on the line, and the generator runs for it afresh.
        assert_eq!(labels(&bench.complete("fruit pick banana c")), ["cherry"]);
        assert_eq!(bench.runs("pick"), 2);
        // So does coming back to where it ran before.
        bench.complete("fruit pick ");
        assert_eq!(bench.runs("pick"), 3);
    }

    #[test]
    fn a_trigger_runs_the_generator_again_and_a_query_term_narrows_the_match() {
        let mut bench = Bench::new("trigger");
        assert_eq!(labels(&bench.complete("fruit path ")), ["one/", "two"]);
        assert_eq!(labels(&bench.complete("fruit path o")), ["one/"]);
        assert_eq!(bench.runs("path"), 1);

        // The slash moves, so the generator is asked again. Its suggestions complete what
        // follows the slash and leave what precedes it alone.
        let completion = bench.complete("fruit path dir/");
        assert_eq!(bench.runs("path"), 2);
        assert_eq!(labels(&completion), ["one/", "two"]);
        let completion = bench.complete("fruit path dir/t");
        assert_eq!(bench.runs("path"), 2);
        assert_eq!(completion.token, "dir/t");
        assert_eq!(completion.items[0].insert, "dir/two ");
        // A directory is left open for the next level.
        assert_eq!(bench.complete("fruit path dir/o").items[0].insert, "dir/one/");
    }

    #[test]
    fn custom_generators_run_commands_and_decide_their_own_trigger_and_query() {
        let mut bench = Bench::new("custom");
        let completion = bench.complete("fruit where ");
        let directory = bench.dir.file_name().unwrap().to_str().unwrap().to_owned();
        // Higher priority first; the command ran in the directory the user is in.
        assert_eq!(labels(&completion), ["fruit+where+", directory.as_str()]);
        assert_eq!(completion.items[1].description, "same");
        assert_eq!(
            (completion.items[0].insert.as_str(), completion.items[0].cursor_back),
            ("T!", 1)
        );

        // The spec's own function says which part of the token is being completed...
        let completion = bench.complete("fruit where a:t");
        assert_eq!(labels(&completion), ["fruit+where+a:t"]);
        assert_eq!(completion.items[0].insert, "a:T!");
        // ...and that function's trigger fired on the way, at three characters.
        assert_eq!(labels(&bench.complete("fruit where a:to")), ["fruit+where+a:t"]);
    }

    #[test]
    fn a_generator_that_fails_leaves_the_rest_and_says_so() {
        let mut bench = Bench::new("fails");
        let completion = bench.complete("fruit broken ");
        assert_eq!(labels(&completion), ["fine"]);
        assert!(completion.incomplete);
    }

    #[test]
    fn a_command_that_cannot_run_or_overruns_gives_nothing() {
        let mut bench = Bench::new("commands");
        let completion = bench.complete("fruit missing ");
        assert_eq!(labels(&completion), ["fine"]);
        assert!(!completion.incomplete);

        let started = Instant::now();
        let completion = bench.complete("fruit slow ");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(labels(&completion), ["fine"]);
    }

    #[test]
    fn leaving_a_position_stops_what_was_running_for_it() {
        let mut bench = Bench::new("abandon");
        assert!(bench.ask("fruit slow ")[0].incomplete);
        // Moving on answers straight away; the sleep is killed rather than waited for.
        let started = Instant::now();
        assert_eq!(labels(&bench.complete("fruit pick ch")), ["cherry"]);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn template_suggestions_go_through_the_specs_filter() {
        let mut bench = Bench::new("filter");
        std::fs::create_dir_all(bench.dir.join("conf/deep")).unwrap();
        for file in ["a.json", "b.txt", "conf/c.json", "conf/d.md"] {
            std::fs::write(bench.dir.join(file), "").unwrap();
        }
        assert_eq!(labels(&bench.complete("fruit json ")), ["a.json", "conf/", "specs/"]);
        let completion = bench.complete("fruit json conf/");
        assert_eq!(labels(&completion), ["c.json", "deep/"]);
        assert_eq!(completion.items[0].insert, "conf/c.json ");
        assert_eq!(completion.items[1].insert, "conf/deep/");
        // A template on a generator with no filter is listed directly.
        assert_eq!(labels(&bench.complete("fruit inner ")), ["conf/", "specs/"]);
    }

    #[test]
    fn generators_written_as_json_run_too_and_their_names_are_quoted() {
        let mut bench = Bench::new("json");
        let completion = bench.complete("plain ");
        assert_eq!(labels(&completion), ["x y", "z"]);
        assert_eq!(completion.items[0].insert, r"x\ y ");
        assert_eq!(bench.complete("plain \"x").items[0].insert, "\"x y\" ");
    }

    #[test]
    fn a_nested_command_gets_its_own_words() {
        let mut bench = Bench::new("nested");
        let completion = bench.complete("sudo fruit where ");
        assert_eq!(completion.items[0].label, "fruit+where+");
    }

    #[test]
    fn an_answer_is_not_repeated_when_nothing_changed() {
        let mut bench = Bench::new("repeat");
        // The generator comes back empty, which changes nothing but completeness.
        let first = bench.ask("fruit missing zzz");
        assert!(first[0].items.is_empty() && first[0].incomplete);
        let later = bench.settle();
        assert_eq!(later.len(), 1);
        assert!(later[0].items.is_empty() && !later[0].incomplete);
        // Asking again for the same line is answered again, once.
        assert_eq!(bench.ask("fruit missing zzz").len(), 1);
        assert_eq!(bench.settle(), []);
    }
}
