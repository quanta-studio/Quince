//! Starting automatically: the lines in the user's `.zshrc` that run each new terminal's shell
//! inside the wrapper, and putting them there and taking them out again.
//!
//! The lines run the wrapper as a child of the terminal's own shell, which waits for it. That
//! shell is the safety net: only a session that ended in the ordinary way tells it to exit. If
//! the wrapper is missing, cannot start, or dies, the shell simply carries on, and the user is
//! left with a working terminal rather than a closed one.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const START_OPEN: &str = "# >>> quince: start >>>";
const START_CLOSE: &str = "# <<< quince: start <<<";
const LOAD_OPEN: &str = "# >>> quince: integration >>>";
const LOAD_CLOSE: &str = "# <<< quince: integration <<<";

/// What a session that ended normally writes to the descriptor it was told to report on,
/// followed by the shell's exit status.
pub const REPORT_PREFIX: &str = "exit:";

/// A block of ours in the file is opened and not closed, or closed and not opened. The file
/// is left alone rather than guessed at.
#[derive(Debug, PartialEq, Eq)]
pub struct Damaged;

impl fmt::Display for Damaged {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("the quince lines in this file are incomplete; remove them by hand and try again")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Installed,
    /// The lines were there and have been rewritten, for a binary that moved for instance.
    Updated,
    Unchanged,
    Removed,
    /// There was nothing to remove.
    Absent,
}

/// The file zsh reads at the start of an interactive shell.
pub fn default_zshrc() -> Option<PathBuf> {
    let dir = std::env::var_os("ZDOTDIR")
        .filter(|dir| !dir.is_empty())
        .or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(dir).join(".zshrc"))
}

/// `path` as a single-quoted zsh word.
fn quoted(path: &Path) -> io::Result<String> {
    let text = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path to quince is not valid UTF-8"))?;
    Ok(format!("'{}'", text.replace('\'', r"'\''")))
}

/// The lines that go first in `.zshrc`. They stand aside for anything that is not a person at
/// a terminal: scripts, `zsh -c`, editors asking for the environment, dumb terminals.
fn start_block(binary: &str) -> String {
    format!(
        r#"{START_OPEN}
# Runs this terminal's shell inside quince. `quince uninstall` removes
# these lines, and QUINCE_OFF=1 in the environment skips them for one shell.
if [[ -o interactive && -z ${{QUINCE_SESSION-}}${{QUINCE_OFF-}}${{ZSH_EXECUTION_STRING-}} && -t 0 && -t 1 && -t 2 && ${{TERM:-dumb}} != dumb && -x {binary} ]]; then
  _quince_tty=$(command stty -g 2>/dev/null)
  if [[ -o login ]]; then
    _quince_end=$({binary} shell --login --report-fd 3 3>&1 1>&2)
  else
    _quince_end=$({binary} shell --report-fd 3 3>&1 1>&2)
  fi
  # A session that ended in the ordinary way says so, and this shell ends with it. Otherwise
  # it could not start or stopped unexpectedly, and this shell carries on as a plain one.
  case $_quince_end in
    ({REPORT_PREFIX}|{REPORT_PREFIX}*[^0-9]*) ;;
    ({REPORT_PREFIX}*) unset HISTFILE; exit ${{_quince_end#{REPORT_PREFIX}}} ;;
  esac
  # If it died it left the terminal in raw mode, which zsh does not undo by itself.
  [[ -n $_quince_tty ]] && command stty "$_quince_tty" 2>/dev/null
  unset _quince_end _quince_tty
fi
{START_CLOSE}
"#
    )
}

/// The lines that go last in `.zshrc`. The session's first shell is given the integration by
/// the wrapper; these cover the shells started inside it afterwards.
fn load_block(binary: &str) -> String {
    format!(
        r#"{LOAD_OPEN}
# For shells started inside a session other than its first, such as after `exec zsh`.
if [[ -o interactive && -n ${{QUINCE_SESSION-}} && -z ${{_quince_loaded-}}${{_quince_shim-}} && -x {binary} ]]; then
  eval "$({binary} init zsh)"
fi
{LOAD_CLOSE}
"#
    )
}

/// `text` with our blocks taken out, along with the blank line that set each apart.
pub fn without_blocks(text: &str) -> Result<String, Damaged> {
    let mut kept: Vec<&str> = Vec::new();
    let mut closing: Option<&str> = None;
    let mut just_closed_start = false;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if let Some(close) = closing {
            if bare == close {
                closing = None;
                just_closed_start = close == START_CLOSE;
            } else if [START_OPEN, LOAD_OPEN].contains(&bare) {
                return Err(Damaged);
            }
            continue;
        }
        let after_start = std::mem::take(&mut just_closed_start);
        match bare {
            START_OPEN => closing = Some(START_CLOSE),
            LOAD_OPEN => {
                if kept.last().is_some_and(|last| last.trim().is_empty()) {
                    kept.pop();
                }
                closing = Some(LOAD_CLOSE);
            }
            START_CLOSE | LOAD_CLOSE => return Err(Damaged),
            "" if after_start => {}
            _ => kept.push(line),
        }
    }
    match closing {
        Some(_) => Err(Damaged),
        None => Ok(kept.concat()),
    }
}

/// `text` with our blocks in place for `binary`: one at the very top, one at the very bottom.
pub fn with_blocks(text: &str, binary: &Path) -> Result<String, io::Error> {
    let binary = quoted(binary)?;
    let mut body =
        without_blocks(text).map_err(|damaged| io::Error::new(io::ErrorKind::InvalidData, damaged.to_string()))?;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    let gap = if body.is_empty() { "" } else { "\n" };
    Ok(format!("{}\n{body}{gap}{}", start_block(&binary), load_block(&binary)))
}

/// Adds our lines to `zshrc`, or brings them up to date, so that new terminals start `binary`.
/// The file as it was before the first change is kept beside it.
pub fn install(zshrc: &Path, binary: &Path) -> io::Result<Outcome> {
    let target = resolve(zshrc);
    let before = read(&target)?;
    let after = with_blocks(before.as_deref().unwrap_or_default(), binary)?;
    if before.as_deref() == Some(after.as_str()) {
        return Ok(Outcome::Unchanged);
    }
    let had_blocks = before.as_deref().is_some_and(|text| text.contains(START_OPEN));
    if let Some(before) = &before {
        let backup = backup_path(&target);
        if !backup.exists() {
            fs::write(&backup, before)?;
        }
    }
    write(&target, &after)?;
    Ok(if had_blocks {
        Outcome::Updated
    } else {
        Outcome::Installed
    })
}

/// Takes our lines out of `zshrc`.
pub fn uninstall(zshrc: &Path) -> io::Result<Outcome> {
    let target = resolve(zshrc);
    let Some(before) = read(&target)? else {
        return Ok(Outcome::Absent);
    };
    let after =
        without_blocks(&before).map_err(|damaged| io::Error::new(io::ErrorKind::InvalidData, damaged.to_string()))?;
    if after == before {
        return Ok(Outcome::Absent);
    }
    write(&target, &after)?;
    Ok(Outcome::Removed)
}

/// Where the untouched original is kept.
pub fn backup_path(zshrc: &Path) -> PathBuf {
    let mut name = resolve(zshrc).into_os_string();
    name.push(".before-quince");
    PathBuf::from(name)
}

/// A `.zshrc` is often a link into a dotfiles directory. The file it points to is the one to
/// change, so the link stays a link.
fn resolve(zshrc: &Path) -> PathBuf {
    fs::canonicalize(zshrc).unwrap_or_else(|_| zshrc.to_owned())
}

fn read(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Replaces the file in one step, so a shell starting at that moment reads either the old
/// contents or the new, never half of one.
fn write(path: &Path, text: &str) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".quince-new");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, text)?;
    if let Ok(existing) = fs::metadata(path) {
        fs::set_permissions(&temporary, fs::Permissions::from_mode(existing.permissions().mode()))?;
    }
    fs::rename(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BINARY: &str = "/opt/bin/quince";

    fn installed(text: &str) -> String {
        with_blocks(text, Path::new(BINARY)).unwrap()
    }

    #[test]
    fn puts_one_block_first_and_one_last() {
        let text = installed("export EDITOR=vim\nsource ~/.oh-my-zsh/oh-my-zsh.sh\n");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], START_OPEN);
        assert_eq!(lines.last(), Some(&LOAD_CLOSE));
        let start_ends = lines.iter().position(|line| *line == START_CLOSE).unwrap();
        let load_begins = lines.iter().position(|line| *line == LOAD_OPEN).unwrap();
        // The user's own lines sit untouched between the two, set apart by a blank line.
        assert_eq!(
            lines[start_ends + 1..load_begins],
            ["", "export EDITOR=vim", "source ~/.oh-my-zsh/oh-my-zsh.sh", ""]
        );
        assert!(text.contains("'/opt/bin/quince' shell --login --report-fd 3 3>&1 1>&2"));
        assert!(text.contains("eval \"$('/opt/bin/quince' init zsh)\""));
    }

    #[test]
    fn taking_the_blocks_out_gives_back_the_file() {
        for original in [
            "",
            "export EDITOR=vim\n",
            "\n# starts with a blank line\nalias g=git\n",
            "ends with blank lines\n\n\n",
            "windows\r\nline endings\r\n",
        ] {
            assert_eq!(without_blocks(&installed(original)).unwrap(), original, "{original:?}");
        }
        // The one change that survives: a last line is given its newline.
        assert_eq!(without_blocks(&installed("no newline")).unwrap(), "no newline\n");
        // A file with nothing of ours in it is not touched.
        assert_eq!(without_blocks("alias g=git\n\n").unwrap(), "alias g=git\n\n");
    }

    #[test]
    fn installing_again_changes_nothing_and_a_new_binary_replaces_the_old() {
        let once = installed("alias g=git\n");
        assert_eq!(installed(&once), once);

        let moved = with_blocks(&once, Path::new("/somewhere else/it's here")).unwrap();
        assert!(!moved.contains(BINARY));
        // Quoted for the shell, apostrophe and all.
        assert!(moved.contains(r"-x '/somewhere else/it'\''s here' ]]"));
        assert_eq!(moved.matches(START_OPEN).count(), 1);
        assert_eq!(without_blocks(&moved).unwrap(), "alias g=git\n");
    }

    #[test]
    fn a_block_moved_by_hand_is_still_found() {
        let text = format!("first\n{}middle\n{}last\n", start_block("'/b'"), load_block("'/b'"));
        assert_eq!(without_blocks(&text).unwrap(), "first\nmiddle\nlast\n");
    }

    #[test]
    fn a_damaged_block_is_left_alone() {
        for damaged in [
            format!("{START_OPEN}\nif true; then\n"),
            format!("alias g=git\n{START_CLOSE}\n"),
            format!("{START_OPEN}\n{START_OPEN}\n{START_CLOSE}\n"),
            format!("{LOAD_OPEN}\nstuff\n{START_CLOSE}\n"),
        ] {
            assert_eq!(without_blocks(&damaged), Err(Damaged), "{damaged:?}");
            assert!(with_blocks(&damaged, Path::new(BINARY)).is_err());
        }
    }

    #[test]
    fn the_start_block_is_valid_zsh_that_leaves_a_script_alone() {
        // Parsed by zsh itself, and run where none of its conditions hold: not interactive,
        // so it must do nothing at all.
        let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
            .into_iter()
            .find(|path| Path::new(path).exists())
        else {
            return;
        };
        let script = format!("{}print reached-the-end\n", installed("print in-the-middle\n"));
        let output = std::process::Command::new(zsh)
            .args(["-f", "-c", &script])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "in-the-middle\nreached-the-end\n"
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quince-setup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn install_edits_the_file_keeps_the_original_and_uninstall_restores_it() {
        let dir = scratch("file");
        let zshrc = dir.join(".zshrc");
        fs::write(&zshrc, "alias g=git\n").unwrap();
        fs::set_permissions(&zshrc, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(install(&zshrc, Path::new(BINARY)).unwrap(), Outcome::Installed);
        assert!(fs::read_to_string(&zshrc).unwrap().starts_with(START_OPEN));
        assert_eq!(fs::read_to_string(backup_path(&zshrc)).unwrap(), "alias g=git\n");
        assert_eq!(fs::metadata(&zshrc).unwrap().permissions().mode() & 0o777, 0o600);

        assert_eq!(install(&zshrc, Path::new(BINARY)).unwrap(), Outcome::Unchanged);
        assert_eq!(install(&zshrc, Path::new("/elsewhere/bin")).unwrap(), Outcome::Updated);
        // The copy kept is of the file before any of this, not of an earlier install.
        assert_eq!(fs::read_to_string(backup_path(&zshrc)).unwrap(), "alias g=git\n");

        assert_eq!(uninstall(&zshrc).unwrap(), Outcome::Removed);
        assert_eq!(fs::read_to_string(&zshrc).unwrap(), "alias g=git\n");
        assert_eq!(uninstall(&zshrc).unwrap(), Outcome::Absent);
        // Nothing of the write is left lying about.
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, [".zshrc", ".zshrc.before-quince"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_created_and_a_linked_one_stays_a_link() {
        let dir = scratch("link");
        let fresh = dir.join("fresh/.zshrc");
        fs::create_dir(dir.join("fresh")).unwrap();
        assert_eq!(install(&fresh, Path::new(BINARY)).unwrap(), Outcome::Installed);
        assert!(!backup_path(&fresh).exists(), "there was nothing to keep a copy of");
        assert_eq!(uninstall(&fresh).unwrap(), Outcome::Removed);
        assert_eq!(fs::read_to_string(&fresh).unwrap(), "");
        assert_eq!(uninstall(&dir.join("fresh/absent")).unwrap(), Outcome::Absent);

        // A .zshrc kept in a dotfiles directory and linked into place.
        fs::create_dir(dir.join("dotfiles")).unwrap();
        fs::write(dir.join("dotfiles/zshrc"), "alias g=git\n").unwrap();
        let link = dir.join(".zshrc");
        std::os::unix::fs::symlink(dir.join("dotfiles/zshrc"), &link).unwrap();
        assert_eq!(install(&link, Path::new(BINARY)).unwrap(), Outcome::Installed);
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(
            fs::read_to_string(dir.join("dotfiles/zshrc"))
                .unwrap()
                .starts_with(START_OPEN)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_is_not_written() {
        let dir = scratch("damaged");
        let zshrc = dir.join(".zshrc");
        let damaged = format!("{START_OPEN}\nhalf a block\n");
        fs::write(&zshrc, &damaged).unwrap();
        assert!(install(&zshrc, Path::new(BINARY)).is_err());
        assert!(uninstall(&zshrc).is_err());
        assert_eq!(fs::read_to_string(&zshrc).unwrap(), damaged);
        fs::remove_dir_all(dir).unwrap();
    }
}
