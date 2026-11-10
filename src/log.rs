//! Process-wide verbosity switches and the `debug!`/`verbose!` macros.
//!
//! `-v/--verbose` prints what zsync is doing; `-V/--debug` (or `DEBUG=zsync`) also prints every
//! command it runs. Both write to stderr so stdout stays clean for data and `--format json`.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);
static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

pub fn set_debug(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
    if on {
        VERBOSE.store(true, Ordering::Relaxed);
    }
}

pub fn verbose_enabled() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

/// `DEBUG=zsync` (or a comma list containing `zsync`/`*`) enables debug output, as in node-zsync.
pub fn debug_from_env(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.split(',').map(str::trim).any(|p| p == "zsync" || p == "*" || p == "zsync*"))
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::debug_enabled() {
            eprintln!("zsync: {}", format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! verbose {
    ($($arg:tt)*) => {
        if $crate::log::verbose_enabled() {
            eprintln!("{}", format!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_env_matching() {
        assert!(debug_from_env(Some("zsync")));
        assert!(debug_from_env(Some("foo, zsync")));
        assert!(debug_from_env(Some("*")));
        assert!(!debug_from_env(Some("zsyncx")));
        assert!(!debug_from_env(None));
    }
}
