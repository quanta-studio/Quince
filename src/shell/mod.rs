//! Getting the shell integration loaded in the shell the wrapper starts.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The zsh integration script, as printed by `quince init zsh`.
pub const ZSH_INTEGRATION: &str = include_str!("zsh/integration.zsh");

const ZSH_SHIMS: &[(&str, &str)] = &[
    (".zshenv", include_str!("zsh/zshenv")),
    (".zprofile", include_str!("zsh/zprofile")),
    (".zshrc", include_str!("zsh/zshrc")),
    ("integration.zsh", ZSH_INTEGRATION),
];

pub fn is_zsh(shell: &Path) -> bool {
    shell
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.trim_start_matches('-') == "zsh")
}

/// A private directory of zsh startup files that load the user's own and then the integration.
/// It is removed when dropped, so it must outlive the shell's startup, and need not outlive it:
/// a session that is killed later then leaves nothing behind.
pub struct ZshShim {
    dir: PathBuf,
}

impl ZshShim {
    /// Creates the directory and points `command`'s zsh at it through `ZDOTDIR`.
    pub fn install(command: &mut Command) -> io::Result<ZshShim> {
        let shim = ZshShim {
            dir: private_temp_dir()?,
        };
        for (name, contents) in ZSH_SHIMS {
            fs::write(shim.dir.join(name), contents)?;
        }
        // The shims hand ZDOTDIR back to the user's value, which they learn from this.
        match std::env::var_os("ZDOTDIR") {
            Some(user) => command.env("QUINCE_USER_ZDOTDIR", user),
            None => command.env_remove("QUINCE_USER_ZDOTDIR"),
        };
        command.env("ZDOTDIR", &shim.dir);
        Ok(shim)
    }
}

impl Drop for ZshShim {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Makes a fresh directory only this user can read. The shell runs what is inside, so it must
/// never be a directory somebody else got to first: creation fails if the name is taken.
fn private_temp_dir() -> io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    for _ in 0..16 {
        let dir = base.join(format!("quince-{pid}-{seed:08x}"));
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223)
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a private temporary directory",
    ))
}
