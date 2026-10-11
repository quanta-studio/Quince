//! Keeps panics off the user's screen where the code is prepared for them.
//!
//! The wrapper's standard error is the terminal the user is working in. A panic that is caught
//! and recovered from, or that only stops suggestions, must not print over it. Threads that
//! are in that position say so here; a panic anywhere else is reported as usual.

use std::cell::Cell;
use std::io::Write;
use std::panic;
use std::sync::Once;

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Says whether a panic on this thread, from now on, is kept off the terminal. It goes to the
/// trace file instead, if there is one.
pub fn set(quiet: bool) {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !QUIET.get() {
                return default(info);
            }
            let trace = std::env::var_os("QUINCE_TRACE");
            if let Some(mut file) = trace.and_then(|path| std::fs::File::options().append(true).open(path).ok()) {
                let _ = writeln!(file, "{:>10} panic       {info}", "");
            }
        }));
    });
    QUIET.set(quiet);
}
