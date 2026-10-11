//! Pseudo-terminal plumbing: spawning a process on a new PTY, window sizes and raw mode.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::{Mutex, PoisonError};

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{Winsize, openpty};
use nix::sys::termios::{self, SetArg, Termios};

nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, Winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);

pub fn winsize(tty: BorrowedFd) -> io::Result<Winsize> {
    let mut size = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `size` is a valid `winsize` for the kernel to fill in.
    unsafe { tiocgwinsz(tty.as_raw_fd(), &mut size) }?;
    Ok(size)
}

pub fn set_winsize(tty: BorrowedFd, size: &Winsize) -> io::Result<()> {
    // SAFETY: `size` is a valid `winsize` for the kernel to read.
    unsafe { tiocswinsz(tty.as_raw_fd(), size) }?;
    Ok(())
}

pub fn set_nonblocking(fd: BorrowedFd) -> io::Result<()> {
    let flags = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

fn set_cloexec(fd: BorrowedFd) -> io::Result<()> {
    fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    Ok(())
}

/// `openpty` is not safe to call from several threads at once: on macOS it finds the slave
/// through `ptsname`, which shares one static buffer, so two callers can be handed each other's
/// terminal.
static OPENPTY: Mutex<()> = Mutex::new(());

/// Starts `command` as the session leader of a new pseudo-terminal and returns the master side.
///
/// `termios`, when given, becomes the new terminal's initial line settings.
pub fn spawn(mut command: Command, size: &Winsize, termios: Option<&Termios>) -> io::Result<(OwnedFd, Child)> {
    let pty = {
        let _one_at_a_time = OPENPTY.lock().unwrap_or_else(PoisonError::into_inner);
        openpty(Some(size), termios)?
    };
    // The child must end up with the slave on 0, 1 and 2 and nothing else of ours.
    set_cloexec(pty.master.as_fd())?;
    set_cloexec(pty.slave.as_fd())?;

    command
        .stdin(pty.slave.try_clone()?)
        .stdout(pty.slave.try_clone()?)
        .stderr(pty.slave);
    // SAFETY: the closure runs between fork and exec and only makes async-signal-safe calls.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // Make the slave, already on stdin, this new session's controlling terminal.
            if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    // `command` still owns copies of the slave. They have to close, or the master never sees
    // end-of-file after the child exits.
    drop(command);
    Ok((pty.master, child))
}

/// Puts a terminal into raw mode until dropped.
pub struct RawMode {
    tty: OwnedFd,
    original: Termios,
}

impl RawMode {
    pub fn enter(tty: BorrowedFd) -> io::Result<RawMode> {
        let tty = tty.try_clone_to_owned()?;
        let original = termios::tcgetattr(&tty)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(&tty, SetArg::TCSANOW, &raw)?;
        Ok(RawMode { tty, original })
    }

    /// The settings the terminal had before raw mode.
    pub fn original(&self) -> &Termios {
        &self.original
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(&self.tty, SetArg::TCSADRAIN, &self.original);
    }
}
