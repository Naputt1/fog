//! One vocabulary for service state, shared by the CLI, the index API, and
//! the TUI.
//!
//! The wire strings predate this module: the index JSON, the IPC socket, and
//! the SPA match them verbatim, so [`ServiceState::as_str`] is a compatibility
//! contract rather than a display choice. Renderers keep their own colours and
//! wording; only the state names and their semantic weights live here.

use std::fmt;

/// A service's state, whether it describes the process (`Running`/`Stopped`)
/// or its health probe (`Healthy`/`Unhealthy`/...).
///
/// `Pending` is a service waiting on a dependency before it can start, and
/// `Unknown` is a service whose health has not been probed (or that reports no
/// health at all); both are produced by the daemon, so they are states here
/// rather than presentation details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Pending,
    Starting,
    Running,
    Healthy,
    Unhealthy,
    Stopped,
    Failed,
    Unknown,
}

/// Every state, in wire order. Exhaustiveness here is what lets the tests
/// prove the wire contract, so a new variant must be added deliberately.
pub const ALL_STATES: [ServiceState; 8] = [
    ServiceState::Pending,
    ServiceState::Starting,
    ServiceState::Running,
    ServiceState::Healthy,
    ServiceState::Unhealthy,
    ServiceState::Stopped,
    ServiceState::Failed,
    ServiceState::Unknown,
];

impl ServiceState {
    /// The state as it appears on the wire (index JSON, IPC, SPA).
    ///
    /// These strings are consumed by clients outside this repository's control,
    /// so renaming one is a breaking change; change [`Self::label`] instead when
    /// only the displayed wording should move.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// Parses an inbound state string, returning `None` for anything this
    /// build does not know.
    ///
    /// Callers must decide what an unknown state means for them (permissive for
    /// forward compatibility, blocking where the state drives a decision). The
    /// one thing they must not do is coerce it into a healthy state.
    pub fn parse(raw: &str) -> Option<Self> {
        ALL_STATES.into_iter().find(|state| state.as_str() == raw)
    }

    /// Wording shown to a human in status tables and the TUI.
    ///
    /// Kept separate from [`Self::as_str`] so display text can change without
    /// touching the wire contract. It matches the wire names today because they
    /// are already the words users see.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// Semantic weight of the state, so each renderer can pick its own colours
    /// (ANSI, ratatui theme colours, CSS classes) without matching state names.
    pub fn tone(self) -> Tone {
        match self {
            Self::Pending | Self::Starting => Tone::Info,
            Self::Running | Self::Healthy => Tone::Success,
            Self::Unhealthy | Self::Failed => Tone::Error,
            Self::Stopped | Self::Unknown => Tone::Neutral,
        }
    }

    /// The state of a service known only by whether its process is up.
    pub fn from_running(running: bool) -> Self {
        if running {
            Self::Running
        } else {
            Self::Stopped
        }
    }
}

impl fmt::Display for ServiceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for ServiceState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Semantic weight shared by every renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Info,
    Success,
    Warning,
    Error,
}

#[cfg(test)]
mod tests {
    use super::{ALL_STATES, ServiceState, Tone};

    #[test]
    fn every_state_round_trips_through_its_wire_name() {
        for state in ALL_STATES {
            assert_eq!(ServiceState::parse(state.as_str()), Some(state));
            // `Display` is what CLI messages and log lines use; it must not
            // drift from the wire form.
            assert_eq!(state.to_string(), state.as_str());
        }
    }

    #[test]
    fn unknown_input_is_not_coerced_into_a_state() {
        assert_eq!(ServiceState::parse(""), None);
        assert_eq!(ServiceState::parse("healthy-ish"), None);
        // The wire form is lowercase; casing is the caller's problem, not a
        // silent alias.
        assert_eq!(ServiceState::parse("Healthy"), None);
    }

    #[test]
    fn wire_strings_are_stable() {
        // Kept as a literal: the index JSON and the SPA match these verbatim, so
        // a rename must fail here instead of silently breaking the web UI.
        assert_eq!(
            ALL_STATES.map(ServiceState::as_str),
            [
                "pending",
                "starting",
                "running",
                "healthy",
                "unhealthy",
                "stopped",
                "failed",
                "unknown",
            ]
        );
    }

    #[test]
    fn json_uses_the_wire_name() {
        assert_eq!(
            serde_json::to_string(&ServiceState::Stopped).expect("serialize state"),
            "\"stopped\""
        );
    }

    #[test]
    fn labels_and_tones_cover_every_state() {
        for state in ALL_STATES {
            assert_eq!(state.label(), state.as_str());
        }
        assert_eq!(ServiceState::Healthy.tone(), Tone::Success);
        assert_eq!(ServiceState::Running.tone(), Tone::Success);
        assert_eq!(ServiceState::Starting.tone(), Tone::Info);
        assert_eq!(ServiceState::Pending.tone(), Tone::Info);
        assert_eq!(ServiceState::Unhealthy.tone(), Tone::Error);
        assert_eq!(ServiceState::Failed.tone(), Tone::Error);
        assert_eq!(ServiceState::Stopped.tone(), Tone::Neutral);
        assert_eq!(ServiceState::Unknown.tone(), Tone::Neutral);
        assert_eq!(ServiceState::from_running(true), ServiceState::Running);
        assert_eq!(ServiceState::from_running(false), ServiceState::Stopped);
    }
}
