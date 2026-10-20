//! `TWR_TRACE_STARTUP=<file>`: the steps of starting twr, and of switching the TUI's main
//! pane, timed. Every twr process that has the variable (the `twr` that opens the TUI, the hub
//! it starts, which inherits it) appends one line per step to the file:
//!
//! ```text
//! <microseconds since the Unix epoch> <pid> <step>
//! ```
//!
//! Each line is one `write` to a file opened for appending, so the processes' lines never mix.
//! Without the variable (or when the file can't be opened) nothing is written and a step costs
//! one check of a flag.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// The variable naming the file.
pub const TRACE_ENV: &str = "TWR_TRACE_STARTUP";

fn file() -> Option<&'static Mutex<std::fs::File>> {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = std::env::var_os(TRACE_ENV).filter(|p| !p.is_empty())?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

/// Whether steps are recorded.
pub fn enabled() -> bool {
    file().is_some()
}

/// Records `step` now, when steps are recorded.
pub fn mark(step: &str) {
    if let Some(f) = file() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let line = format!("{now} {} {step}\n", std::process::id());
        let mut f = f.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = f.write_all(line.as_bytes());
    }
}

/// Records the step `f` says, when steps are recorded (`f` runs only then).
pub fn mark_with(f: impl FnOnce() -> String) {
    if enabled() {
        mark(&f());
    }
}
