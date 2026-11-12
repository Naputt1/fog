//! The error type every fallible fog operation returns.
//!
//! Printers own the user-facing prefix (`error: `, a service name, a config
//! path), so [`FogError`]'s `Display` renders the message alone. A recovery
//! hint travels beside the message instead of being glued into it, so one
//! `cli::die` path can put it on its own `hint:` line, and an underlying cause
//! stays reachable through [`std::error::Error::source`] for logging.

use std::fmt;

/// A failure whose message is shown to the user as written.
#[derive(Debug)]
pub struct FogError {
    message: String,
    hint: Option<String>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl FogError {
    /// An error with no hint and no lower-level cause.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
            source: None,
        }
    }

    /// Attaches a recovery hint, printed by the caller with its own label.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Attaches the underlying cause. It is not printed, because the message
    /// already names the operation that failed.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The message without any caller-owned prefix.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The recovery hint, when one was attached.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// Prefixes the message while keeping any hint and cause.
    ///
    /// Used where a layer adds context to an error it did not author (for
    /// example a worktree switch labelling a config failure).
    pub fn context(mut self, prefix: impl fmt::Display) -> Self {
        self.message = format!("{prefix}: {}", self.message);
        self
    }
}

impl fmt::Display for FogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

impl From<std::io::Error> for FogError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string()).with_source(error)
    }
}

impl From<&str> for FogError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

// Call sites that already built a `String` (a `format!` result, a message
// collected from a validation pass) pass it by reference, so the same
// `die`/`fail` entry point accepts them without cloning.
impl From<&String> for FogError {
    fn from(message: &String) -> Self {
        Self::new(message)
    }
}

/// `Result` for fog operations that can fail with a user-facing [`FogError`].
pub type FogResult<T> = Result<T, FogError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_prints_only_the_message() {
        let error = FogError::new("could not bind :80").with_hint("run once as root");
        assert_eq!(error.to_string(), "could not bind :80");
        assert_eq!(error.message(), "could not bind :80");
        assert_eq!(error.hint(), Some("run once as root"));
    }

    #[test]
    fn io_errors_keep_their_cause() {
        let error = FogError::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert_eq!(error.to_string(), "denied");
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn context_prefixes_the_message_and_keeps_the_hint() {
        let error = FogError::new("bad json")
            .with_hint("fix it")
            .context("switch worktree");
        assert_eq!(error.to_string(), "switch worktree: bad json");
        assert_eq!(error.hint(), Some("fix it"));
    }
}
