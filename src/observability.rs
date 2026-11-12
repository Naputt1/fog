//! Opt-in structured logging for a program whose real terminal belongs to the
//! TUI.
//!
//! fog is a TUI first: the process owns the terminal in raw mode on the
//! alternate screen, so anything written to stderr after startup lands in the
//! middle of the dashboard. Tracing therefore never writes to the terminal. It
//! appends to `<instance log dir>/tracing.log`, next to the per-service
//! `*.log` files that `--save-logs` and the daemon tee already use, and only
//! when `RUST_LOG` asks for it: an unset `RUST_LOG` installs no subscriber, so
//! a normal run pays one environment lookup and nothing else — no file handle,
//! no writer, no per-event filter.
//!
//! Every failure here is silent on purpose. An unparseable filter, an
//! unwritable log directory or an already-installed subscriber all leave fog
//! running without tracing; a debugging aid must never be the reason a dev
//! environment refuses to start.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// File tracing appends to inside the instance log directory.
pub const LOG_FILE: &str = "tracing.log";

/// Returns the file tracing writes to for the current process.
pub fn log_path() -> PathBuf {
    crate::ipc::instance_log_dir(std::process::id()).join(LOG_FILE)
}

/// Installs the `RUST_LOG` subscriber for this process.
///
/// Returns the path of the log file when a subscriber was installed. `None`
/// means tracing stays off: `RUST_LOG` is unset or empty, the directives do
/// not parse, or the log file could not be opened.
pub fn init() -> Option<PathBuf> {
    let directives = std::env::var("RUST_LOG").ok()?;
    if directives.trim().is_empty() {
        return None;
    }
    let path = log_path();
    install(&directives, &path).then_some(path)
}

/// Installs a subscriber filtered by `directives`, appending to `path`.
///
/// Returns whether the subscriber became the global default. False covers
/// every degradation path, including a process that already has one.
fn install(directives: &str, path: &Path) -> bool {
    // `try_new` rather than `new`: the lossy constructor prints a warning for
    // malformed directives, and stderr at this point is the TUI's screen.
    let Ok(filter) = tracing_subscriber::EnvFilter::try_new(directives) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    // The directory normally exists by now (`create_log_dir` runs before the
    // TUI takes over), but `fog --help` and early failures never get that far.
    let _ = std::fs::create_dir_all(parent);
    let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
        return false;
    };
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        // No escape sequences: this file is read with `less` and `grep`, and a
        // raw control sequence in a log makes an incident unreadable.
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .finish();
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return false;
    }
    // One line per run so an empty trace is distinguishable from a run where
    // tracing never came up, and so interleaved instances can be told apart.
    tracing::debug!(
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        log = %path.display(),
        "tracing started"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_path_lives_in_instance_log_dir() {
        let path = log_path();
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(LOG_FILE));
        assert_eq!(
            path.parent(),
            Some(crate::ipc::instance_log_dir(std::process::id()).as_path())
        );
    }

    #[test]
    fn test_install_writes_events_to_the_file() {
        let dir = std::env::temp_dir().join(format!("fog-tracing-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(LOG_FILE);

        assert!(install("debug", &path), "install should succeed once");
        tracing::debug!(marker = "tracing-test-marker", "written to file");

        let content = std::fs::read_to_string(&path).expect("log file readable");
        assert!(content.contains("tracing-test-marker"), "{content}");
        assert!(
            !content.contains('\u{1b}'),
            "log must not contain terminal escapes"
        );

        // A second install cannot replace the global subscriber; it must
        // report failure instead of panicking.
        assert!(!install("debug", &path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_install_gives_up_on_an_unwritable_path() {
        // A file where the log directory should be: `create_dir_all` and
        // `open` both fail, and init must degrade instead of panicking.
        let blocker =
            std::env::temp_dir().join(format!("fog-tracing-blocked-{}", std::process::id()));
        std::fs::write(&blocker, b"not a directory").expect("blocker file writable");
        let path = blocker.join(LOG_FILE);
        assert!(!install("debug", &path));
        let _ = std::fs::remove_file(&blocker);
    }
}
