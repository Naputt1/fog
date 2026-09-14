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
}
