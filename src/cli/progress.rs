//! The two docker-style progress renderers: the `fog kill` shutdown checklist
//! and the detached-start startup checklist.
//!
//! Both renderers write to an injected [`std::io::Write`] instead of stdout, so
//! the bytes they produce are inspectable without a terminal (and the test in
//! this module pins them). They own only layout: the caller passes the TTY and
//! colour decisions, and the checklist builders below stay pure.

use crate::cli::presentation::{
    MARKER_BLUE, MARKER_GREEN, MARKER_RED, colored, strip_ansi, terminal_width, truncate_to,
};
use crate::ipc;
use crate::service_state::ServiceState;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

/// How many trailing log lines `fog kill` shows for a service that has not
/// finished shutting down and has no `shutdown_cmd` to display instead.
const KILL_LOG_TAIL: usize = 4;

/// Checklist markers for a service that has vs. has not stopped.
const SHUTDOWN_DONE: &str = "✓";
const SHUTDOWN_PENDING: &str = "⠙";
/// Checklist marker for a service that came up unhealthy.
const START_FAILED: &str = "✗";

/// Braille frames a still-draining service's marker cycles through on a
/// terminal, one per redraw (~10 Hz), so the checklist visibly spins.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A service's live state while an instance is shutting down.
struct ShutdownService {
    name: String,
    running: bool,
    shutdown_cmd: Option<String>,
}

/// Renders per-service shutdown progress for `fog kill`, docker-style: a
/// checklist of every service, with each not-yet-stopped service followed by
/// its `shutdown_cmd` (or the tail of its log). On a terminal it redraws in
/// place; when piped it appends a line per state change.
pub(crate) struct StopProgress {
    pid: u32,
    tty: bool,
    color: bool,
    script: String,
    project: Option<String>,
    branch: Option<String>,
    /// Whether the live block has been drawn (TTY only).
    drawn: bool,
    /// Whether the initial checklist has been printed (piped only).
    printed: bool,
    last: Vec<ShutdownService>,
    last_render: std::time::Instant,
    /// Index into [`SPINNER_FRAMES`] for the next redraw.
    frame: usize,
}

impl StopProgress {
    pub(crate) fn new(pid: u32, tty: bool, color: bool) -> Self {
        Self {
            pid,
            tty,
            color,
            script: String::new(),
            project: None,
            branch: None,
            drawn: false,
            printed: false,
            last: Vec::new(),
            last_render: std::time::Instant::now() - Duration::from_millis(200),
            frame: 0,
        }
    }

    /// Polls the instance status and redraws, throttled to ~10 Hz so a fast
    /// query loop does not flicker.
    pub(crate) fn tick(&mut self, path: &Path, out: &mut impl Write) -> io::Result<()> {
        if self.last_render.elapsed() < Duration::from_millis(100) {
            return Ok(());
        }
        self.refresh(path, out)
    }

    /// Polls the instance status and redraws unconditionally. `tick` throttles
    /// this; callers that need the latest state immediately (such as the
    /// interactive exit before drawing the settled checklist) call it directly.
    pub(crate) fn refresh(&mut self, path: &Path, out: &mut impl Write) -> io::Result<()> {
        let Ok(status) = ipc::query_status(path) else {
            return Ok(());
        };
        self.last_render = std::time::Instant::now();
        if self.script.is_empty() {
            self.script = status.script.clone();
            self.project = status.project.as_deref().map(crate::project::display_name);
            self.branch = status.branch.clone();
        }
        let services: Vec<ShutdownService> = status
            .services
            .iter()
            .map(|s| ShutdownService {
                name: s.name.clone(),
                running: s.running,
                shutdown_cmd: s.shutdown_cmd.clone(),
            })
            .collect();
        if self.tty {
            self.render_live(out, &services)?;
        } else {
            self.render_appended(out, &services)?;
        }
        self.last = services;
        Ok(())
    }

    /// `script 'dev', project 'acme', branch 'main'` — enough to tell which
    /// environment is being stopped, even from a directory with no config.
    fn target_label(&self) -> String {
        let mut label = format!("script '{}'", self.script);
        if let Some(project) = &self.project {
            label.push_str(&format!(", project '{project}'"));
        }
        if let Some(branch) = &self.branch {
            label.push_str(&format!(", branch '{branch}'"));
        }
        label
    }

    fn render_live(
        &mut self,
        out: &mut impl Write,
        services: &[ShutdownService],
    ) -> io::Result<()> {
        if !self.drawn {
            writeln!(
                out,
                "Shutting down fog instance {} ({})...",
                self.pid,
                self.target_label()
            )?;
            // Save the cursor so each redraw returns to the block start.
            write!(out, "\x1b7")?;
            self.drawn = true;
        }
        // Restore the cursor and clear everything the previous block drew.
        let pending = SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()];
        self.frame = self.frame.wrapping_add(1);
        write!(out, "\x1b8\x1b[J")?;
        let detail_width = terminal_width().saturating_sub(6);
        for line in shutdown_lines(services, pending, self.color, Some(detail_width), |s| {
            self.detail(s)
        }) {
            writeln!(out, "{line}")?;
        }
        out.flush()
    }

    fn render_appended(
        &mut self,
        out: &mut impl Write,
        services: &[ShutdownService],
    ) -> io::Result<()> {
        if !self.printed {
            self.printed = true;
            writeln!(
                out,
                "Shutting down fog instance {} ({})...",
                self.pid,
                self.target_label()
            )?;
            for line in shutdown_lines(services, SHUTDOWN_PENDING, false, None, |s| self.detail(s))
            {
                writeln!(out, "{line}")?;
            }
            return Ok(());
        }
        // Append a line as each service transitions to stopped.
        for svc in services {
            let was_running = self
                .last
                .iter()
                .find(|p| p.name == svc.name)
                .is_some_and(|p| p.running);
            if was_running && !svc.running {
                writeln!(out, "  {} {}  stopped", SHUTDOWN_DONE, svc.name)?;
            }
        }
        Ok(())
    }

    /// Detail lines for one still-shutting-down service: its `shutdown_cmd`
    /// when configured, otherwise the last few captured log lines.
    fn detail(&self, svc: &ShutdownService) -> Vec<String> {
        if !svc.running {
            return Vec::new();
        }
        if let Some(cmd) = &svc.shutdown_cmd {
            return vec![format!("$ {cmd}")];
        }
        tail_log_lines(self.pid, &svc.name, KILL_LOG_TAIL)
    }

    /// Clears the live block and leaves a settled checklist on the terminal, so
    /// the outcome stays visible instead of being erased.
    pub(crate) fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        if !self.drawn {
            return Ok(());
        }
        self.drawn = false;
        write!(out, "\x1b8\x1b[J")?;
        for line in shutdown_lines(&self.last, SHUTDOWN_PENDING, self.color, None, |_| {
            Vec::new()
        }) {
            writeln!(out, "{line}")?;
        }
        out.flush()
    }
}

/// Builds the checklist rows for the shutdown view. `pending` is the marker
/// shown for a service that has not stopped yet; `detail` supplies the extra
/// lines shown under it, truncated to `detail_width` when set.
fn shutdown_lines(
    services: &[ShutdownService],
    pending: &str,
    color: bool,
    detail_width: Option<usize>,
    detail: impl Fn(&ShutdownService) -> Vec<String>,
) -> Vec<String> {
    let width = services.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for svc in services {
        let (marker, state) = if svc.running {
            (colored(pending, MARKER_BLUE, color), "shutting down")
        } else {
            (
                colored(SHUTDOWN_DONE, MARKER_GREEN, color),
                ServiceState::Stopped.label(),
            )
        };
        lines.push(format!("  {marker} {:<width$}  {state}", svc.name));
        for d in detail(svc) {
            let d = match detail_width {
                Some(width) => truncate_to(&d, width),
                None => d,
            };
            lines.push(format!("      {d}"));
        }
    }
    lines
}

/// A service's live state while an instance is starting up.
struct StartService {
    name: String,
    running: bool,
    health: String,
}

/// Maps a service's running flag and health string to its rendered start
/// state: the text to show, whether it is up, and whether it came up unhealthy.
///
/// The checklist wording ("started", "waiting") differs from
/// [`ServiceState::label`] on purpose: it collapses every up state into
/// `started`, and a service that has not come up into `waiting`.
fn start_service_state(running: bool, health: &str) -> (&'static str, bool, bool) {
    match ServiceState::parse(health) {
        Some(ServiceState::Healthy) => ("started", true, false),
        Some(state @ ServiceState::Unhealthy) => (state.label(), false, true),
        Some(state @ ServiceState::Starting) => (state.label(), false, false),
        // `pending` without a live process is a service still waiting on a
        // dependency's health check.
        Some(ServiceState::Pending) if !running => ("waiting", false, false),
        _ if running => ("started", true, false),
        _ => ("waiting", false, false),
    }
}

/// Applies the settled state: once the daemon has latched startup completion, a
/// service its snapshot still shows as waiting reads as started rather than
/// spinning — the daemon is the authority on when startup finished.
fn settled_state(
    label: &'static str,
    ready: bool,
    failed: bool,
    settled: bool,
) -> (&'static str, bool) {
    if settled && !failed && !ready {
        ("started", true)
    } else {
        (label, ready)
    }
}

/// Builds the startup checklist: setup phases first, then each service's start
/// state. While `all_done` is false the last phase (or unfinished service) spins.
fn start_lines(
    phases: &[String],
    services: &[StartService],
    pending: &str,
    all_done: bool,
    color: bool,
) -> Vec<String> {
    let width = services.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for (i, phase) in phases.iter().enumerate() {
        let running = !all_done && i + 1 == phases.len();
        let marker = if running {
            colored(pending, MARKER_BLUE, color)
        } else {
            colored(SHUTDOWN_DONE, MARKER_GREEN, color)
        };
        lines.push(format!("  {marker} {phase}"));
    }
    for svc in services {
        let (state, ready, failed) = start_service_state(svc.running, &svc.health);
        let (state, ready) = settled_state(state, ready, failed, all_done);
        let marker = if failed {
            colored(START_FAILED, MARKER_RED, color)
        } else if ready {
            colored(SHUTDOWN_DONE, MARKER_GREEN, color)
        } else {
            colored(pending, MARKER_BLUE, color)
        };
        lines.push(format!("  {marker} {:<width$}  {state}", svc.name));
    }
    lines
}

/// Renders docker-style startup progress for `fog <script> -d`: the daemon's
/// setup phases, then each service as it comes up. On a terminal it redraws in
/// place; when piped it appends a line as each phase appears and each service
/// changes state, mirroring how `fog kill` behaves without a TTY.
pub(crate) struct StartProgress {
    pid: u32,
    script: String,
    tty: bool,
    color: bool,
    drawn: bool,
    printed: bool,
    phases_printed: usize,
    last: Vec<(String, String)>,
    frame: usize,
}

impl StartProgress {
    pub(crate) fn new(pid: u32, script: &str, tty: bool, color: bool) -> Self {
        Self {
            pid,
            script: script.to_string(),
            tty,
            color,
            drawn: false,
            printed: false,
            phases_printed: 0,
            last: Vec::new(),
            frame: 0,
        }
    }

    /// Redraws from the daemon's reported phases and (once serving) status.
    /// `all_done` settles the checklist for the final render.
    pub(crate) fn render(
        &mut self,
        out: &mut impl Write,
        phases: &[String],
        status: Option<&ipc::StatusResponse>,
        all_done: bool,
    ) -> io::Result<()> {
        let services: Vec<StartService> = status
            .map(|s| {
                s.services
                    .iter()
                    .map(|svc| StartService {
                        name: svc.name.clone(),
                        running: svc.running,
                        health: svc.health.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if self.tty {
            let pending = SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()];
            self.frame = self.frame.wrapping_add(1);
            if !self.drawn {
                writeln!(out, "Starting fog '{}' (pid {})...", self.script, self.pid)?;
                write!(out, "\x1b7")?;
                self.drawn = true;
            }
            write!(out, "\x1b8\x1b[J")?;
            for line in start_lines(phases, &services, pending, all_done, self.color) {
                writeln!(out, "{line}")?;
            }
            out.flush()
        } else {
            self.render_appended(out, phases, &services, all_done)
        }
    }

    /// Piped output: appends the header once, then a line per new phase and per
    /// service whose rendered label changed (mirroring `fog kill` without a TTY).
    fn render_appended(
        &mut self,
        out: &mut impl Write,
        phases: &[String],
        services: &[StartService],
        all_done: bool,
    ) -> io::Result<()> {
        if !self.printed {
            self.printed = true;
            writeln!(out, "Starting fog '{}' (pid {})...", self.script, self.pid)?;
        }
        for phase in phases.iter().skip(self.phases_printed) {
            writeln!(out, "  {SHUTDOWN_DONE} {phase}")?;
        }
        self.phases_printed = phases.len();
        let mut labels = Vec::with_capacity(services.len());
        for svc in services {
            let (label, ready, failed) = start_service_state(svc.running, &svc.health);
            let (label, ready) = settled_state(label, ready, failed, all_done);
            let marker = if failed {
                START_FAILED
            } else if ready {
                SHUTDOWN_DONE
            } else {
                "·"
            };
            let prev = self
                .last
                .iter()
                .find(|(name, _)| name == &svc.name)
                .map(|(_, label)| label.as_str());
            if prev != Some(label) {
                writeln!(out, "  {marker} {}  {label}", svc.name)?;
            }
            labels.push((svc.name.clone(), label.to_string()));
        }
        self.last = labels;
        Ok(())
    }

    /// Leaves the settled checklist on screen: the final render already drew
    /// every row complete.
    pub(crate) fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.drawn {
            self.drawn = false;
            out.flush()?;
        }
        Ok(())
    }
}

/// The last `n` non-empty lines of a service's captured log, ANSI stripped.
fn tail_log_lines(pid: u32, name: &str, n: usize) -> Vec<String> {
    let file = ipc::instance_log_dir(pid).join(format!("{}.log", ipc::sanitize_service_name(name)));
    let Ok(content) = fs::read_to_string(&file) else {
        return Vec::new();
    };
    last_lines(&strip_ansi(&content), n)
}

/// The last `n` non-empty lines of `text`, right-trimmed.
fn last_lines(text: &str, n: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() > n {
        lines.drain(0..lines.len() - n);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::presentation::ANSI_RESET;

    #[test]
    fn test_last_lines_keeps_nonempty_tail() {
        let text = "one\ntwo\n\nthree\nfour\nfive\n";
        assert_eq!(last_lines(text, 4), vec!["two", "three", "four", "five"]);
        // Fewer lines than requested returns them all.
        assert_eq!(
            last_lines(text, 10),
            vec!["one", "two", "three", "four", "five"]
        );
        assert!(last_lines("", 4).is_empty());
    }

    #[test]
    fn test_shutdown_lines_checklist() {
        let services = vec![
            ShutdownService {
                name: "api".into(),
                running: false,
                shutdown_cmd: None,
            },
            ShutdownService {
                name: "web".into(),
                running: true,
                shutdown_cmd: Some("docker compose down".into()),
            },
            ShutdownService {
                name: "db".into(),
                running: true,
                shutdown_cmd: None,
            },
        ];
        let lines = shutdown_lines(&services, SHUTDOWN_PENDING, false, None, |s| {
            match s.name.as_str() {
                "web" => vec!["$ docker compose down".to_string()],
                "db" => vec!["db ready".to_string(), "db serving".to_string()],
                _ => Vec::new(),
            }
        });
        assert_eq!(
            lines,
            vec![
                "  ✓ api  stopped",
                "  ⠙ web  shutting down",
                "      $ docker compose down",
                "  ⠙ db   shutting down",
                "      db ready",
                "      db serving",
            ]
        );
    }

    #[test]
    fn test_shutdown_lines_uses_spinner_frame() {
        let services = vec![ShutdownService {
            name: "web".into(),
            running: true,
            shutdown_cmd: None,
        }];
        let lines = shutdown_lines(&services, SPINNER_FRAMES[3], false, None, |_| Vec::new());
        assert_eq!(
            lines,
            vec![format!("  {} web  shutting down", SPINNER_FRAMES[3])]
        );
    }

    #[test]
    fn test_shutdown_lines_colors_markers() {
        let services = vec![
            ShutdownService {
                name: "api".into(),
                running: false,
                shutdown_cmd: None,
            },
            ShutdownService {
                name: "web".into(),
                running: true,
                shutdown_cmd: None,
            },
        ];
        let lines = shutdown_lines(&services, SHUTDOWN_PENDING, true, None, |_| Vec::new());
        assert_eq!(
            lines,
            vec![
                format!("  {}✓{} api  stopped", MARKER_GREEN, ANSI_RESET),
                format!("  {}⠙{} web  shutting down", MARKER_BLUE, ANSI_RESET),
            ]
        );
    }

    #[test]
    fn test_start_service_state_maps_health() {
        assert_eq!(
            start_service_state(true, "healthy"),
            ("started", true, false)
        );
        assert_eq!(
            start_service_state(true, "unhealthy"),
            ("unhealthy", false, true)
        );
        assert_eq!(
            start_service_state(true, "starting"),
            ("starting", false, false)
        );
        // `pending` without a live process is still waiting on a dependency.
        assert_eq!(
            start_service_state(false, "pending"),
            ("waiting", false, false)
        );
        // A service with no health checks is up as soon as it runs.
        assert_eq!(
            start_service_state(true, "unknown"),
            ("started", true, false)
        );
        assert_eq!(
            start_service_state(false, "unknown"),
            ("waiting", false, false)
        );
    }

    #[test]
    fn test_start_lines_spins_last_phase_then_settles() {
        let phases = vec!["dnsmasq".to_string(), "allocating ports".to_string()];
        assert_eq!(
            start_lines(&phases, &[], SPINNER_FRAMES[3], false, false),
            vec![
                "  ✓ dnsmasq".to_string(),
                format!("  {} allocating ports", SPINNER_FRAMES[3]),
            ]
        );
        assert_eq!(
            start_lines(&phases, &[], SPINNER_FRAMES[3], true, false),
            vec![
                "  ✓ dnsmasq".to_string(),
                "  ✓ allocating ports".to_string(),
            ]
        );
    }

    #[test]
    fn test_start_lines_marks_service_states() {
        let services = vec![
            StartService {
                name: "db".into(),
                running: true,
                health: "healthy".into(),
            },
            StartService {
                name: "api".into(),
                running: false,
                health: "pending".into(),
            },
            StartService {
                name: "web".into(),
                running: true,
                health: "unhealthy".into(),
            },
        ];
        assert_eq!(
            start_lines(&[], &services, SPINNER_FRAMES[0], false, true),
            vec![
                format!("  {MARKER_GREEN}✓{ANSI_RESET} db   started"),
                format!("  {MARKER_BLUE}⠋{ANSI_RESET} api  waiting"),
                format!("  {MARKER_RED}✗{ANSI_RESET} web  unhealthy"),
            ]
        );
    }

    #[test]
    fn test_settled_state_forces_started_when_settled() {
        assert_eq!(
            settled_state("waiting", false, false, false),
            ("waiting", false)
        );
        // Once the daemon has latched startup completion, a still-waiting row
        // reads as started rather than spinning.
        assert_eq!(
            settled_state("waiting", false, false, true),
            ("started", true)
        );
        // Already-ready and failed rows are unchanged.
        assert_eq!(
            settled_state("started", true, false, true),
            ("started", true)
        );
        assert_eq!(
            settled_state("unhealthy", false, true, true),
            ("unhealthy", false)
        );
    }
}
