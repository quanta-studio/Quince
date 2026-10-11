# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`quince` is a Fig-style autocomplete for the terminal: as you type a command, a popup lists subcommands, options and arguments for the CLI under the cursor.

It exists because the hosted lineage ended. Fig became Amazon Q Developer CLI, which became Kiro CLI, and Kiro CLI 3.0 (rolling out October 2026) retires inline shell autocomplete and the CLI desktop app (<https://kiro.dev/blog/cli-3/>).

## Commands

```sh
cargo run                                    # start $SHELL wrapped with autocomplete
cargo run -- complete "git ch"               # the engine alone: print suggestions for a line
cargo test                                   # unit tests and end-to-end tests
cargo test --lib engine::tests::inline_option_values     # one unit test
cargo test --test e2e suggestions_appear                 # one end-to-end test
cargo clippy --all-targets -- -D warnings
cargo fmt                                    # line width is 120, see rustfmt.toml
scripts/install-fig-specs.sh [destination]   # download the real Fig specs
cargo run -- install --zshrc <file>          # add the auto-start lines to a .zshrc (uninstall removes them)
cargo test --release --test e2e popup_latency -- --ignored --nocapture   # keystroke-to-popup time
cargo test --release --lib every_installed_spec -- --ignored --nocapture # load every installed spec, report failures
QUINCE_TRACE=/tmp/trace.log cargo run                          # log every byte crossing the wrapper
```

Specs are read from `$QUINCE_SPECS_DIR`, else `~/.local/share/quince/specs`. Without specs there are no suggestions. A spec named `git` is `git.js`, a Fig spec exactly as published, or `git.json`, the same structure written by hand without functions. `install-fig-specs.sh` fetches the published package, pinned by checksum, and needs only curl, tar and shasum.

`cargo run -- complete "<line>"` waits for generators and prints the final list, which makes it the quickest way to see what a real spec does. It runs the spec's commands for real, in the current directory.

`install` with no `--zshrc` edits the real `~/.zshrc`, and it points new terminals at the binary that ran it. Never run it from a build in `target/` against a real home directory: every terminal the developer opens would then depend on that build. To try auto-start against real startup files, copy them into a scratch directory and point `ZDOTDIR` at it.

The end-to-end tests (`tests/e2e.rs`) run the real binary around a real zsh inside a PTY and assert on a `vt100` screen. They skip themselves if zsh is missing. They use an isolated `HOME`, never the developer's dotfiles. A failed wait prints the screen and the tail of the wrapper's trace.

When terminal behaviour looks wrong, read the trace before theorising: it shows the keys, what went to the shell, what came back and what was drawn, with timestamps. The file contains everything typed and shown.

## Decisions

Made by the project owner. Do not revisit them without being asked.

1. **In-terminal rendering through a PTY wrapper.** The popup is drawn with escape sequences inside the user's own terminal. There is no desktop app, overlay window, Accessibility API or input method.
2. **Rust core.**
3. **Completion data: Fig specs first, zsh's native completion as fallback** for commands that have no spec.
4. **Fig's JavaScript runs in an embedded QuickJS engine** (`rquickjs`), not in Node and not precompiled away.

The first target environment is macOS + zsh + iTerm2 (the owner's setup). Other shells run through the wrapper untouched, without suggestions.

## Architecture

```
terminal  <-->  session (PTY wrapper)  <-->  zsh + integration.zsh
                   |        ^
          Request  v        |  Completion, more than once per request
                engine thread  <-->  QuickJS (specs and their generators)
                   |        ^
          command  v        |  output
                 one thread per command
```

- **`session.rs`** is the event loop between the terminal and the shell's PTY. Bytes pass straight through; the popup is an overlay written into the same stream. It uses `select`, because `poll` and kqueue do not work on terminals on macOS.
- **`scan.rs`** reads the shell's output on the way through. It removes the integration's private escape sequences and tracks whether the stream is between escape sequences.
- **`protocol.rs`** documents and implements both directions of the conversation with the shell. Read its module comment before touching `integration.zsh`.
- **`shell/`** gets the integration loaded without editing the user's dotfiles: zsh is started with `ZDOTDIR` pointing at a temporary directory of shims that source the user's real files and then `integration.zsh`.
- **`setup.rs`** is auto-start: the lines `install` puts in the user's `.zshrc`, one block at the top that starts the wrapper and one at the bottom that loads the integration in later shells of a session.
- **`popup.rs`** turns suggestions into bytes. It never sees the session.
- **`engine/`** knows nothing about terminals. It runs on a thread of its own, so completion work never delays a keystroke.
  - `tokenize` splits the line, the `Walker` in `mod.rs` follows the words through a spec, and a `Collector` gathers what matches. That much is answered at once.
  - `js.rs` is the embedded JavaScript engine, and `glue.js` is the code that runs inside it ahead of any spec. A spec's functions never leave the engine: `spec.rs` gets the spec described as JSON, with each generator replaced by a number.
  - `generate.rs` tracks generator runs and `command.rs` runs the commands they ask for, in the shell's directory and with the shell's exported environment, both of which the integration reports. Each result makes the `Engine` answer the same request again with more in it, which is why the session can receive several completions for one line.
  - `worker.rs` is the thread's event loop: requests in, command results in, completions out.

### Rules the design depends on

Breaking any of these corrupts the user's terminal or command line, usually only in some timing or terminal state that the tests may not hit.

- **The popup is written only when the output stream is at a sequence boundary** (`Scanner::is_ground`), and **taken off the screen before any further shell output is forwarded**. So the real terminal always equals the model plus at most one popup.
- **Popup rows are positioned relative to the cursor; only columns are absolute.** The screen model's idea of the cursor row can drift from the real terminal, so nothing may depend on it. `popup::reserve` is the one operation that changes the screen for good (it scrolls to make room), and its bytes must also be fed to the model.
- **The overlay must never take the shell down.** The `vt100` model is third-party code fed arbitrary output, and it does panic (a one-column terminal is enough). All model and popup work goes through `Session::guarded`; after a failure the model is rebuilt and the popup stays off until the next prompt. Forwarding bytes must stay outside the guard.
- **The wrapper never edits the command line by typing.** Accepting a suggestion sends a request that a zsh widget applies only if the buffer and cursor are still what the suggestion was computed for. Typed keystrokes cannot delete reliably in vi mode and can land on a line that has moved on.
- **The popup only takes keys when it is sure about the line**: its suggestions are for the latest reported buffer and no input has been forwarded since. Otherwise the key goes to the shell. This relies on `integration.zsh` reporting on every redraw, changed or not.
- **A lone Tab on an empty token goes to zsh when the list is known to be incomplete** (`Completion::incomplete`): a generator is still running, or failed. The first row of a partial list is an arbitrary pick, and the shell's own completion may know better.
- **A message is the shell's only if it carries the session's token.** The shell talks to the wrapper through escape sequences on the terminal, and so can anything else that gets text onto the screen: a file being printed, a remote host. Messages decide which directory and environment generator commands run in, so a forged one is a way to run code. The wrapper makes up a token per session and passes it in the shell's environment; `protocol::parse` drops whatever lacks it. Any new message must go through the same check.
- **The terminal's own shell is the safety net for auto-start.** The lines in `.zshrc` run the wrapper as a child and wait; only a session that reports a clean end makes that shell exit. If the wrapper is missing, cannot start, or dies, the shell restores the terminal's settings and carries on as a plain shell. Do not turn this into `exec`, and do not let anything but a clean end write to the report descriptor: either would mean a wrapper bug can leave the user unable to open a terminal.
- **The engine thread never waits.** Commands run on their own threads and report back as events; every call into JavaScript has a time budget and is interrupted past it. A generator that blocked the thread would freeze suggestions for the whole session.
- **Specs are someone else's code.** Inside the engine they have the language and `glue.js` and nothing else: no files, no network, no timers. The one thing they can do is ask for a command, which is the Fig contract and means a spec can run anything as the user, so specs are only installed from a pinned source. Commands run in their own session with no input and captured output, so they can neither read the user's keys nor write on the screen, and they are killed on timeout, on too much output, and when the user moves on. Never hand JavaScript a capability directly.
- **`glue.js` follows Fig's own engine, quirks included.** Specs are written against its behaviour: command output arrives with surrounding newlines stripped, and many specs break without that. When a real spec misbehaves, look at what Fig does (`packages/autocomplete` in the Amazon Q repository) before working around the spec.
- **Generator results last only while the user stays on one token.** They are keyed by directory and the words before the cursor, and dropped on leaving, so coming back runs the generator afresh. Within a token the spec's `trigger` decides when to run again.

### Writing end-to-end tests

- Wait for a screen state that only the final result can produce. A popup from the previous line stays up until the new one arrives, and typing `git ch` passes through the popups for `g`, `git ` and `git c` on the way, so a row count alone can match too early.
- Put the command line in the same wait as the popup. The popup can be a frame ahead of the echo of the key that caused it.
- Do not send Ctrl-C until the echo of the previous key is on screen. zsh ignores an interrupt that lands while it is still redrawing, with or without the wrapper.
- Keep reading the PTY while waiting (`pump_for`, `wait_for`), never plain `sleep`. A PTY holds about a kilobyte; the wrapper blocks on output nobody reads, as any program would.
- These races only show under load. Before trusting a new test, run several copies of the test binary at once, a few times over. Six copies is the ceiling on macOS: it allows 511 pseudo-terminals, each test holds two, and past that `openpty` fails with an error that looks like a bug and is not.
- Make the test's startup files behave like real ones where it matters. Real ones run programs, and zsh takes over the terminal settings a program leaves behind; a fallback that looked fine with an empty `.zshrc` left a real terminal in raw mode. The auto-start tests' `.zshrc` runs a program for that reason.

## Not built yet

- **Specs that are built at run time.** `generateSpec` (34 specs, among them `php artisan`, `rails` and `composer`) and `loadSpec` given as a function are ignored, so those commands get only what is written out. They need the walk through the spec to wait on JavaScript, which it does not do today.
- **Parts of the generator contract.** `cache` is ignored: every new token runs its generators again. Fuzzy matching is honoured on arguments (`filterStrategy`) but not on subcommands. Specs whose default export is a function (six, for versioned CLIs) do not load.
- **Crash isolation for the JavaScript engine.** It runs in the wrapper's process. A panic there only stops suggestions, but a crash inside QuickJS itself would take the shell with it. Running the engine as a child process would close that gap.
- **zsh fallback** for commands without a spec. zsh's completion system has no "complete this line" query API, so results have to be captured, by driving compsys in a hidden `zpty` session or by hooking `compadd`.
- **Installing the program itself.** `install` wires up whatever binary ran it; putting that binary somewhere permanent (`cargo install --path .`) is left to the user, and nothing updates it.
- **Shells other than zsh.** bash and fish each need their own integration and accept mechanism.
- **Resizing against a real terminal.** The tests cover it against `vt100`, which does not reflow lines the way iTerm2 does.

## Prior art

All permissively licensed. Read the relevant one before designing a component.

- **[aws/amazon-q-developer-cli-autocomplete](https://github.com/aws/amazon-q-developer-cli-autocomplete)** (MIT and Apache-2.0): the open-sourced Fig codebase. `crates/figterm` is its PTY wrapper; `packages/autocomplete` is the completion engine, including how generators are run.
- **[withfig/autocomplete](https://github.com/withfig/autocomplete)** (MIT; the npm metadata says ISC): the specs, published as `@withfig/autocomplete` with the schema types in `@withfig/autocomplete-types`. The last npm release was 2.692.3 in May 2025, so assume spec fixes are ours to carry.
- **[microsoft/inshellisense](https://github.com/microsoft/inshellisense)** (MIT, TypeScript): the same UI approach on the same specs.
