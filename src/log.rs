use std::io::Write;

/// Leading marker for informational setup/progress lines, as opposed to
/// warnings (which start with `⚠`).
pub const INFO_PREFIX: &str = "  + ";

/// Whether `message` is an informational setup line rather than a warning.
pub fn is_info(message: &str) -> bool {
    message.starts_with(INFO_PREFIX)
}

/// Prints setup messages to stderr, omitting informational lines unless
/// `verbose`. Warnings are always printed.
pub fn emit(messages: &[String], verbose: bool) {
    for message in messages {
        if verbose || !is_info(message) {
            eprintln!("{message}");
        }
    }
}

/// Appends setup messages to `<log_dir>/daemon.log` so an interactive run's
/// diagnostics outlive the TUI taking over the screen. Uses the same filter as
/// [`emit`]. Detached runs already redirect their stderr into that file, so
/// they must not call this.
pub fn append_daemon_log(log_dir: &std::path::Path, messages: &[String], verbose: bool) {
    let relevant: Vec<&String> = messages.iter().filter(|m| verbose || !is_info(m)).collect();
    if relevant.is_empty() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))
    else {
        return;
    };
    for line in relevant {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_info_classifies_prefix() {
        assert!(is_info(
            "  + native route frontend -> host.docker.internal:53123"
        ));
        assert!(is_info("  + port api -> 53123"));
        assert!(!is_info("⚠ could not start router"));
        assert!(!is_info("error: bad config"));
        assert!(!is_info("+ missing indent"));
    }

    #[test]
    fn test_append_daemon_log_filters_info_unless_verbose() {
        let dir = std::env::temp_dir().join(format!("fog-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let messages = vec![
            "  + port web -> 1234".to_string(),
            "⚠ docker did not respond".to_string(),
        ];

        append_daemon_log(&dir, &messages, false);
        let content = std::fs::read_to_string(dir.join("daemon.log")).unwrap();
        assert!(content.contains("⚠ docker did not respond"));
        assert!(!content.contains("port web"));

        append_daemon_log(&dir, &messages, true);
        let content = std::fs::read_to_string(dir.join("daemon.log")).unwrap();
        assert!(content.contains("port web"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
