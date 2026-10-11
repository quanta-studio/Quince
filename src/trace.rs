//! An optional record of everything that crosses the wrapper, for diagnosing terminal problems.
//!
//! Set `QUINCE_TRACE` to a file path and each chunk of bytes is appended to it as one
//! line: microseconds since the session started, where the bytes were seen, and the bytes
//! themselves with everything unprintable escaped. The file holds whatever was typed and shown,
//! so it is as sensitive as the session.

use std::fs::File;
use std::io::Write;
use std::time::Instant;

pub struct Trace {
    file: Option<File>,
    started: Instant,
}

impl Trace {
    pub fn from_env() -> Trace {
        let file =
            std::env::var_os("QUINCE_TRACE").and_then(|path| File::options().create(true).append(true).open(path).ok());
        Trace {
            file,
            started: Instant::now(),
        }
    }

    /// `point` names where the bytes were seen: `keys` from the terminal, `to-shell`,
    /// `from-shell` or `to-terminal`.
    pub fn record(&mut self, point: &str, bytes: &[u8]) {
        let Some(file) = &mut self.file else { return };
        let micros = self.started.elapsed().as_micros();
        // A trace that cannot be written is not worth disturbing the session for.
        let _ = writeln!(file, "{micros:>10} {point:<11} {}", bytes.escape_ascii());
    }
}
