# Quince

Fig-style autocomplete for the terminal. As you type a command, a popup under the cursor lists the subcommands, options and arguments that fit, with live data where a command has it: git branches, npm scripts, directories.

Quince exists because the hosted lineage ended. Fig became Amazon Q Developer CLI, which became Kiro CLI, and Kiro CLI 3.0 retires inline shell autocomplete. Quince runs the same community-written completion specs, entirely on your machine.

**Status:** early. It works day to day on macOS with zsh, which is what it is developed on. Other shells run inside it unchanged, without suggestions.

## Install

You need a Rust toolchain (1.88 or newer) and a C compiler, which builds the embedded JavaScript engine.

```sh
cargo install --git https://github.com/quanta-studio/Quince
```

Then fetch the completion specs. This downloads the published [`@withfig/autocomplete`](https://github.com/withfig/autocomplete) package, pinned by checksum, into `~/.local/share/quince/specs`. It needs curl, tar and shasum.

```sh
curl -fsSL https://raw.githubusercontent.com/quanta-studio/Quince/main/scripts/install-fig-specs.sh | sh
```

## Use

```sh
quince              # start your shell with autocomplete; `exit` to leave
quince install      # start it automatically in every new terminal
quince uninstall    # undo that
```

`quince install` adds a few lines to your `.zshrc` and keeps a copy of the file as it was. If Quince is missing, cannot start, or dies, the terminal carries on as a plain shell. `QUINCE_OFF=1` skips it for one shell.

While the popup is showing:

| Key | Does |
|---|---|
| Tab | takes the first suggestion, or the highlighted one |
| Down, Up | move the highlight |
| Enter | takes the highlighted suggestion; with none highlighted, runs the command as usual |
| Esc | closes the popup |

## How it works

Quince starts your shell inside a pseudo-terminal and passes every byte through untouched. A small zsh integration tells it what is on the command line; it works out what could come next and draws the popup into the same terminal with ordinary escape sequences. There is no desktop app and no accessibility permission.

Specs are the JavaScript modules Fig published. They run in an embedded QuickJS engine with no access to files, the network or timers.

## What to know before you trust it

- **Specs run commands.** A spec's generator decides what to run to compute suggestions, as you, with your environment: typing `git checkout ` runs `git branch`. That is how Fig worked. Only install specs you trust; the install script pins the official package by checksum.
- **Suggestions are applied by your shell, not typed in.** An accepted suggestion is only applied if the command line is still exactly what it was computed for.
- **The JavaScript engine runs in Quince's own process.** A crash inside it would end that terminal's session.

## Not there yet

- Commands whose specs are generated at run time (`php artisan`, `rails`, `composer` and others) get only what is written out in the spec.
- Commands with no spec get nothing; falling back to zsh's own completion is planned.
- bash and fish have no integration.

## Development

```sh
cargo test                                   # unit tests, and end-to-end tests that drive a real zsh
cargo clippy --all-targets -- -D warnings
cargo run -- complete "git checkout "        # what the engine suggests for a line
```

`CLAUDE.md` describes the architecture and the rules the design depends on.

## Credits

Quince stands on [Fig](https://github.com/withfig), whose spec format and community-written [completion specs](https://github.com/withfig/autocomplete) it uses, and learned from [Amazon Q Developer CLI](https://github.com/aws/amazon-q-developer-cli-autocomplete) and [inshellisense](https://github.com/microsoft/inshellisense). It is not affiliated with Fig, Amazon or Microsoft.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

The completion specs are a separate work, downloaded at install time under their own MIT license.
