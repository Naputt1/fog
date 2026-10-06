//! Command-line interface: argument parsing and the `fog` subcommands.

#![deny(unsafe_op_in_unsafe_fn)]

use clap::Parser;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, IsTerminal, Write, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{fs, io};

use crate::app::App;
use crate::completion::CompletionShell;
use crate::config::Config;
use crate::config_watcher;
use crate::ipc;
use crate::theme::Theme;

const DEFAULT_SCROLLBACK: usize = 2000;

/// How long a starter waits for another instance that is mid-start before
/// giving up.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a replacer waits for the old instance to fully exit.
const RECLAIM_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a graceful `fog kill` waits for the instance to drain its services
/// (rendering per-service progress) before suggesting `--force`. Services tear
/// down sequentially, so this must cover the whole teardown, not one service.
const KILL_GRACE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the parent of a detached run waits for the daemon to start serving
/// before reporting failure. Covers the owner-lock wait plus the reclaim.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Command-line interface arguments parsed via clap.
#[derive(Parser)]
#[command(
    name = "fog",
    version = env!("CARGO_PKG_VERSION"),
    about = "Terminal-based service orchestrator & reverse-proxy dashboard",
    after_help = "Built-in commands:\n  fog ls [PID]                    list running instances and service status\n  fog kill [PID|--all]            gracefully shut down a running instance\n  fog kill --force [PID|--all]    forcibly shut down a wedged instance\n  fog restart [PID|--all]         restart a running instance\n  fog logs [PID]                  list services and their status\n  fog logs [PID] --service NAME   print captured output of one service\n  fog logs [PID] -s NAME --tail   limit lines: --head N|-N, --tail N|-N|+N\n  fog index serve [--foreground] [--port N]  run the index server (detached by default)\n  fog index kill                  stop the index server\n  fog index restart               restart the index server\n\nWith no PID, kill/restart/logs target the instance started from the local\nfog.json (same config directory, so the branch is implicit). When several\nlocal instances match, the command lists them: pass an explicit PID, or\n--all (kill/restart) to apply to every local match. Outside a directory\nwith fog.json, --all applies to every running instance.\n\nWhen a kill/restart grace period is not enough (a wedged instance that never\nconsumes its kill flag), --force escalates to SIGTERM then SIGKILL.\n\nRun a script from fog.json:\n  fog <script> [OPTIONS]  (e.g. fog dev)\n\nOverride an allocated port for one run:\n  fog dev --port api=4000       (repeatable; --port web=0 re-randomizes)"
)]
struct Cli {
    /// Script to run (e.g. `fog dev`), or a built-in command
    /// (`ls`, `kill`, `restart`, `logs`, `index`).
    script: Option<String>,

    /// PID of a running fog instance (used with `fog kill <pid>`, `fog restart <pid>`, `fog logs <pid>`).
    /// Omit it in a directory with `fog.json` to target the instance started
    /// from that config.
    pid: Option<u32>,

    /// Apply to every matching instance: the local `fog.json` matches when
    /// run in a directory with a config, otherwise every running instance.
    /// Only used with `fog kill` and `fog restart`; conflicts with `PID`.
    #[arg(long)]
    all: bool,

    /// Keep escalating when the instance does not stop gracefully: after the
    /// normal grace period, send SIGTERM and then SIGKILL to the process tree.
    /// Only used with `fog kill` and `fog restart`.
    #[arg(long)]
    force: bool,

    /// Only used with `fog logs`: print the captured output of this service
    /// instead of listing services. Without it, `fog logs` lists the
    /// available service names and their status.
    #[arg(short, long, value_name = "SERVICE")]
    service: Option<String>,

    /// Only used with `fog logs --service`: keep the first `N` lines, or all
    /// but the last `N` with `-N` (like `head -n`).
    #[arg(long, value_name = "N", allow_hyphen_values = true, value_parser = parse_head_spec)]
    head: Option<LineRange>,

    /// Only used with `fog logs --service`: keep the last `N` lines (or
    /// `-N`), or every line from `N` to the end with `+N` (like `tail -n`).
    #[arg(long, value_name = "N", allow_hyphen_values = true, value_parser = parse_tail_spec)]
    tail: Option<LineRange>,

    /// Path to the configuration file (or a directory containing `fog.json`).
    /// Defaults to `fog.json`.
    #[arg(short, long, default_value = "fog.json")]
    config: std::path::PathBuf,

    /// Save service output to `temp/<name>.txt` on exit.
    #[arg(long, help = "Save service output to temp/<name>.txt on exit")]
    save_logs: bool,

    /// Run the script in the git worktree checked out on this branch.
    #[arg(long)]
    branch: Option<String>,

    /// Print a shell completion script to stdout and exit.
    #[arg(long, value_name = "SHELL")]
    completions: Option<CompletionShell>,

    /// Run the script in the background without the TUI: services keep their
    /// PTYs, health checks and proxy, and their output is captured to
    /// `$TMPDIR/fog-<pid>.logs/` for inspection with `fog logs <pid>`.
    #[arg(short, long)]
    detach: bool,

    /// Ignore shared services: even if `share:true`/`reuse:true` with a passing
    /// `health_check`, start a fresh instance instead of borrowing/reusing.
    #[arg(long)]
    no_share: bool,

    /// Override a top-level `ports` entry for this run, e.g. `--port api=4000`.
    /// Repeatable; use `0` to re-randomize. Names not in the config `ports` map
    /// are added for this run. Only applies when running a script.
    #[arg(long, value_name = "NAME=PORT")]
    port: Vec<String>,

    /// Print verbose setup output (DNS, router, index, port and native-route
    /// details). Warnings are always printed.
    #[arg(short, long)]
    verbose: bool,
}

/// Resolves the config file to use, honoring `--branch`:
///
/// When `--branch <name>` is given, fog runs the script from the git worktree
/// checked out on that branch (a relative `--config` is resolved against the
/// worktree root). Errors out when no worktree has that branch.
fn resolve_run_config(cli: &Cli) -> PathBuf {
    let Some(branch) = &cli.branch else {
        return cli.config.clone();
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let worktrees = crate::worktree::list(&cwd).unwrap_or_else(|| {
        eprintln!("error: --branch requires a git repository (could not list worktrees)");
        std::process::exit(1);
    });

    let Some(wt) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch.as_str()))
    else {
        eprintln!("error: no worktree is checked out on branch '{}'", branch);
        eprintln!("available worktrees:");
        for w in &worktrees {
            let label = w.branch.as_deref().unwrap_or("(detached)");
            eprintln!("  {:<24} {}", label, w.path.display());
        }
        std::process::exit(1);
    };

    eprintln!(
        "switching to worktree {} (branch {})",
        wt.path.display(),
        branch
    );
    if cli.config.is_absolute() {
        cli.config.clone()
    } else {
        wt.path.join(&cli.config)
    }
}

/// Resolves the config path: if `path` is a directory, looks for `fog.json`
/// inside it; otherwise returns the path unchanged.
fn resolve_config_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("fog.json")
    } else {
        path.to_path_buf()
    }
}

/// Loads and parses the config file, exiting with a diagnostic on failure.
fn load_config(path: &Path) -> Config {
    match crate::config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Loads config for `fog index restart` — best-effort, no exit on failure.
fn load_runtime_config_for_index() -> crate::config::Config {
    crate::index::load_runtime_config()
}

/// Lists available script names and exits with an error.
fn list_scripts_and_exit(config: &Config, message: &str) -> ! {
    eprintln!("{message}");
    let mut names: Vec<&String> = config.scripts.keys().collect();
    names.sort();
    for name in names {
        eprintln!("  fog {name}");
    }
    std::process::exit(1);
}

/// Names of reuse-flagged services in a script.
fn reuse_names(script: &crate::config::ScriptConfig) -> Vec<String> {
    script
        .service
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .filter(|e| e.reuse)
                .map(|e| {
                    e.name.clone().unwrap_or_else(|| {
                        Path::new(&e.path)
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Shuts down existing fog instances running the same script in the same
/// project, so a new instance can take their place. Returns any live services
/// handed over, keyed by service name, together with their PTY master fd.
///
/// Only instances serving the same `branch` are reclaimed; instances on a
/// different branch (concurrent multi-branch runs) are left untouched.
///
/// The caller is expected to hold the per-(project, script, branch) owner lock.
fn reclaim_existing(
    project: &str,
    script: &str,
    branch: Option<&str>,
    reuse: &[String],
    timeout: Duration,
) -> HashMap<String, ipc::HandoffItem> {
    let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
    let existing = ipc::find_instances_for(project, script, branch);
    for (pid, path) in &existing {
        eprintln!(
            "replacing existing fog instance (pid {pid}, script {script}, project {project})"
        );
        let outcome = ipc::reclaim(path, reuse);
        if let Some(err) = &outcome.error {
            eprintln!("  warning: could not reclaim instance {pid}: {err}, continuing");
        } else if outcome.incomplete {
            eprintln!("  warning: handoff from instance {pid} was incomplete");
        }
        if outcome.handoffs.is_empty() {
            eprintln!("  old instance {pid} has no live services to reuse");
        } else {
            let names: Vec<&str> = outcome.handoffs.iter().map(|h| h.name.as_str()).collect();
            eprintln!("  reusing live services: {}", names.join(", "));
        }
        for handoff in outcome.handoffs {
            // A duplicate service name from another old instance: close the
            // losing fd so it is not leaked (the process itself stays up).
            if let Some(existing) = adopted.get_mut(&handoff.name) {
                // The handle was dupped for transfer and is owned by us.
                crate::fds::close(existing.fd);
                *existing = handoff;
            } else {
                adopted.insert(handoff.name.clone(), handoff);
            }
        }
        if ipc::wait_for_exit(*pid, timeout) {
            eprintln!("  old instance {pid} stopped");
        } else {
            eprintln!("  warning: instance {pid} did not stop within the timeout");
        }
        wait_for_socket_gone(path);
    }
    adopted
}

/// Waits until the old instance's socket is gone or unreachable, guaranteeing
/// it fully cleaned up (and released its ports) before we spawn replacements.
fn wait_for_socket_gone(path: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !path.exists() || ipc::query_status(path).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Coordinates with any other fog instance running `script` in `project`, then
/// reclaims it. Returns any handed-over services and the owner lock, which the
/// caller must drop once its own services are up.
///
/// This makes concurrent startups deterministic:
/// - the instance that acquires the lock first performs the reclaim, and
/// - an instance that finds a just-started serving instance backs off with a
///   clear error instead of fighting over ports or shared infra.
fn reconcile_instance(
    project: &str,
    script: &str,
    branch: Option<&str>,
    reuse: &[String],
) -> (
    HashMap<String, ipc::HandoffItem>,
    Option<crate::lock::OwnerLock>,
) {
    let attempt_started = crate::lock::now_ms();

    let lock = match crate::lock::OwnerLock::try_acquire(project, script, branch) {
        Ok(crate::lock::AcquireResult::Locked(lock)) => lock,
        Ok(crate::lock::AcquireResult::HeldBy(holder)) => {
            let pid = holder
                .as_ref()
                .map(|h| format!(" (pid {})", h.pid))
                .unwrap_or_default();
            eprintln!(
                "another fog instance{pid} is starting script '{script}' for this project; waiting up to 30s"
            );
            match crate::lock::OwnerLock::acquire_with_timeout(
                project,
                script,
                branch,
                LOCK_WAIT_TIMEOUT,
            ) {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    eprintln!(
                        "error: another fog instance is already starting or stuck starting \
                         script '{script}' for this project; check `fog ls` and `fog kill <pid>`"
                    );
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!(
                        "  warning: could not lock project: {e}, proceeding without coordination"
                    );
                    return (
                        reclaim_existing(project, script, branch, reuse, RECLAIM_WAIT_TIMEOUT),
                        None,
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("  warning: could not lock project: {e}, proceeding without coordination");
            return (
                reclaim_existing(project, script, branch, reuse, RECLAIM_WAIT_TIMEOUT),
                None,
            );
        }
    };

    // We now hold the owner lock. An instance that started after we began our
    // startup is a concurrent starter that beat us and is now serving: back
    // off rather than kill a freshly-started instance.
    let instances = ipc::find_instances_with_status(project, script, branch);
    if let Some((pid, _, _)) = instances
        .iter()
        .find(|(_, _, s)| s.started_at > attempt_started)
    {
        eprintln!(
            "error: another fog instance (pid {pid}) just started script '{script}' for this \
             project and now serves it; use `fog kill {pid}` to replace it"
        );
        std::process::exit(1);
    }

    let adopted = reclaim_existing(project, script, branch, reuse, RECLAIM_WAIT_TIMEOUT);
    (adopted, Some(lock))
}

/// A service line in the `fog ls` sub-table: (service name, state).
type ServiceEntry = (String, String);

/// One running instance: identity columns plus the services sub-table.
struct InstanceRow {
    pid: u32,
    script: String,
    project: String,
    branch: String,
    proxy: String,
    services: Vec<ServiceEntry>,
}

fn cmd_ls() -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        println!("no running fog instances");
        return Ok(());
    }

    let mut rows: Vec<InstanceRow> = Vec::new();
    for (pid, path) in &instances {
        match ipc::query_status(path) {
            Ok(status) => {
                let proxy = match status.proxy {
                    Some(p) if p.running => format!(":{}", p.port),
                    Some(_) => ":down".to_string(),
                    None => "-".to_string(),
                };
                let services = status
                    .services
                    .iter()
                    .map(|s| {
                        let state = if s.running {
                            s.health.clone()
                        } else {
                            "stopped".to_string()
                        };
                        (s.name.clone(), state)
                    })
                    .collect::<Vec<_>>();
                let project = status
                    .project
                    .map(|p| crate::project::display_name(&p))
                    .unwrap_or_else(|| "-".to_string());
                let branch = status.branch.unwrap_or_else(|| "-".to_string());
                rows.push(InstanceRow {
                    pid: *pid,
                    script: status.script,
                    project,
                    branch,
                    proxy,
                    services,
                });
            }
            Err(_) => {
                // Only treat the socket as stale if the owning process is
                // genuinely gone. A live-but-slow instance (e.g. mid-handoff)
                // must not be hidden by deleting its socket.
                if !crate::process::is_pid_alive(*pid) {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    if rows.is_empty() {
        println!("no running fog instances");
        return Ok(());
    }

    let max = |len: usize, header: &str| len.max(header.len());
    let w_pid = rows
        .iter()
        .map(|r| r.pid.to_string().len())
        .max()
        .unwrap_or(0);
    let w_script = rows.iter().map(|r| r.script.len()).max().unwrap_or(0);
    let w_project = rows.iter().map(|r| r.project.len()).max().unwrap_or(0);
    let w_branch = rows.iter().map(|r| r.branch.len()).max().unwrap_or(0);
    let w_proxy = rows.iter().map(|r| r.proxy.len()).max().unwrap_or(0);
    let w_service = rows
        .iter()
        .flat_map(|r| r.services.iter())
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    let w_status = rows
        .iter()
        .flat_map(|r| r.services.iter())
        .map(|(_, state)| state.len())
        .max()
        .unwrap_or(0);

    let w_pid = max(w_pid, "pid");
    let w_script = max(w_script, "script");
    let w_project = max(w_project, "project");
    let w_branch = max(w_branch, "branch");
    let w_proxy = max(w_proxy, "proxy");
    let w_service = max(w_service, "service");
    let w_status = max(w_status, "status");

    println!(
        "{:<w_pid$}  {:<w_script$}  {:<w_project$}  {:<w_branch$}  {:<w_proxy$}",
        "pid", "script", "project", "branch", "proxy"
    );
    for row in rows {
        println!(
            "{:<w_pid$}  {:<w_script$}  {:<w_project$}  {:<w_branch$}  {:<w_proxy$}",
            row.pid, row.script, row.project, row.branch, row.proxy
        );
        if !row.services.is_empty() {
            println!("  {:<w_service$}  {:<w_status$}", "service", "status");
            for (name, state) in row.services {
                println!("  {:<w_service$}  {:<w_status$}", name, state);
            }
        }
        println!();
    }
    Ok(())
}

/// How many trailing log lines `fog kill` shows for a service that has not
/// finished shutting down and has no `shutdown_cmd` to display instead.
const KILL_LOG_TAIL: usize = 4;

/// Checklist markers for a service that has vs. has not stopped.
const SHUTDOWN_DONE: &str = "✓";
const SHUTDOWN_PENDING: &str = "⠙";

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
struct StopProgress {
    pid: u32,
    tty: bool,
    script: String,
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
    fn new(pid: u32) -> Self {
        Self {
            pid,
            tty: stdout().is_terminal(),
            script: String::new(),
            drawn: false,
            printed: false,
            last: Vec::new(),
            last_render: std::time::Instant::now() - Duration::from_millis(200),
            frame: 0,
        }
    }

    /// Polls the instance status and redraws, throttled to ~10 Hz so a fast
    /// query loop does not flicker.
    fn tick(&mut self, path: &Path) {
        if self.last_render.elapsed() < Duration::from_millis(100) {
            return;
        }
        let Ok(status) = ipc::query_status(path) else {
            return;
        };
        self.last_render = std::time::Instant::now();
        if self.script.is_empty() {
            self.script = status.script.clone();
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
            self.render_live(&services);
        } else {
            self.render_appended(&services);
        }
        self.last = services;
    }

    fn render_live(&mut self, services: &[ShutdownService]) {
        if !self.drawn {
            println!(
                "Shutting down fog instance {} (script '{}')...",
                self.pid, self.script
            );
            // Save the cursor so each redraw returns to the block start.
            print!("\x1b7");
            self.drawn = true;
        }
        // Restore the cursor and clear everything the previous block drew.
        let pending = SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()];
        self.frame = self.frame.wrapping_add(1);
        print!("\x1b8\x1b[J");
        for line in shutdown_lines(services, pending, |s| self.detail(s)) {
            println!("{line}");
        }
        let _ = stdout().flush();
    }

    fn render_appended(&mut self, services: &[ShutdownService]) {
        if !self.printed {
            self.printed = true;
            println!(
                "Shutting down fog instance {} (script '{}')...",
                self.pid, self.script
            );
            for line in shutdown_lines(services, SHUTDOWN_PENDING, |s| self.detail(s)) {
                println!("{line}");
            }
            return;
        }
        // Append a line as each service transitions to stopped.
        for svc in services {
            let was_running = self
                .last
                .iter()
                .find(|p| p.name == svc.name)
                .is_some_and(|p| p.running);
            if was_running && !svc.running {
                println!("  {} {}  stopped", SHUTDOWN_DONE, svc.name);
            }
        }
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
    fn finish(&mut self) {
        if !self.drawn {
            return;
        }
        self.drawn = false;
        print!("\x1b8\x1b[J");
        for line in shutdown_lines(&self.last, SHUTDOWN_PENDING, |_| Vec::new()) {
            println!("{line}");
        }
        let _ = stdout().flush();
    }
}

/// Builds the checklist rows for the shutdown view. `pending` is the marker
/// shown for a service that has not stopped yet; `detail` supplies the extra
/// lines shown under it.
fn shutdown_lines(
    services: &[ShutdownService],
    pending: &str,
    detail: impl Fn(&ShutdownService) -> Vec<String>,
) -> Vec<String> {
    let width = services.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for svc in services {
        let (marker, state) = if svc.running {
            (pending, "shutting down")
        } else {
            (SHUTDOWN_DONE, "stopped")
        };
        lines.push(format!("  {marker} {:<width$}  {state}", svc.name));
        for d in detail(svc) {
            lines.push(format!("      {d}"));
        }
    }
    lines
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

/// Asks the instance at `path` to stop and waits for it to exit.
///
/// The instance is first asked to shut down gracefully over IPC (so services
/// tear down cleanly). A wedged instance whose event loop never consumes the
/// kill flag will not exit on its own, so with `force` we escalate: SIGTERM to
/// the process tree, then SIGKILL, which cannot be caught or blocked.
///
/// With `progress`, the wait renders each service's shutdown state
/// (docker-style) from the same IPC status the instance publishes as it tears
/// down.
///
/// Returns `true` once the process is no longer alive.
fn stop_instance(pid: u32, path: &Path, force: bool, progress: bool) -> bool {
    if let Err(e) = ipc::send_kill(path) {
        eprintln!("warning: could not reach instance {pid}: {e}");
    }

    let mut view = progress.then(|| StopProgress::new(pid));
    if let Some(v) = view.as_mut() {
        v.tick(path);
    }

    // A graceful kill waits for the services to drain, rendering progress as
    // they do. `--force` keeps the short grace so it can escalate quickly.
    let grace = if force {
        Duration::from_millis(2500)
    } else {
        KILL_GRACE_TIMEOUT
    };
    if wait_for_pid_exit(pid, grace, path, view.as_mut()) {
        finish_view(view.as_mut());
        return true;
    }

    if !force {
        finish_view(view.as_mut());
        eprintln!("warning: instance {pid} did not stop; retry with `fog kill --force {pid}`");
        return false;
    }

    // SIGTERM first for a chance at a clean tree teardown, then SIGKILL. A
    // fog instance registers a SIGTERM handler (the `ctrlc` termination
    // feature), so a wedged loop can ignore the former but never the latter.
    signal_instance(pid, crate::process::Signal::Term);
    if wait_for_pid_exit(pid, Duration::from_millis(1000), path, view.as_mut()) {
        finish_view(view.as_mut());
        return true;
    }
    signal_instance(pid, crate::process::Signal::Kill);
    let exited = wait_for_pid_exit(pid, Duration::from_millis(2000), path, view.as_mut());
    finish_view(view.as_mut());
    if !exited {
        eprintln!("warning: instance {pid} survived SIGKILL");
    }
    exited
}

/// Finalizes a shutdown progress view, if one is active.
fn finish_view(view: Option<&mut StopProgress>) {
    if let Some(v) = view {
        v.finish();
    }
}

/// Signals the instance's whole process tree and the process itself.
///
/// [`crate::process::signal_tree`] targets the group, which reaches the leader
/// (fog is normally its own group leader), but a target that is not a group
/// leader would be missed — so hit the PID directly too.
fn signal_instance(pid: u32, signal: crate::process::Signal) {
    crate::process::signal_tree(pid, signal);
    // Also signal the process itself: a non-group-leader target is missed by
    // the group kill above.
    let _ = crate::process::kill_process(pid, signal);
}

/// Waits until `pid` is no longer alive, up to `timeout`, polling the
/// instance's shutdown progress while it drains.
fn wait_for_pid_exit(
    pid: u32,
    timeout: Duration,
    path: &Path,
    mut progress: Option<&mut StopProgress>,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(p) = progress.as_deref_mut() {
            p.tick(path);
        }
        if !crate::process::is_pid_alive(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn cmd_kill(pid: Option<u32>, all: bool, cli: &Cli) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        eprintln!("error: no running fog instances");
        std::process::exit(1);
    }

    let targets = resolve_targets(&instances, pid, all, cli, "kill");
    for (target_pid, path) in &targets {
        let target_pid = *target_pid;
        if stop_instance(target_pid, path, cli.force, true) {
            println!("stopped fog instance {target_pid}");
        } else {
            eprintln!("warning: fog instance {target_pid} is still running");
        }
    }

    // Tear down the index server if no instance remains. Best-effort.
    crate::index::maybe_terminate_if_no_instances(None);
    Ok(())
}

fn cmd_restart(pid: Option<u32>, all: bool, cli: &Cli) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        eprintln!("error: no running fog instances");
        std::process::exit(1);
    }

    let targets = resolve_targets(&instances, pid, all, cli, "restart");
    for (target_pid, path) in &targets {
        let target_pid = *target_pid;
        // Capture the target's status before killing so we can relaunch it.
        let status = ipc::query_status(path).unwrap_or_else(|e| {
            eprintln!("error: could not query instance {target_pid}: {e}");
            std::process::exit(1);
        });
        let script = status.script.clone();
        let config_dir = status.config_dir.clone();
        let branch = status.branch.clone();

        // Wait for the old instance to fully exit before relaunching to avoid
        // port conflicts and owner-lock races. `--force` escalates to SIGKILL
        // so a wedged instance does not block the restart.
        if stop_instance(target_pid, path, cli.force, false) {
            println!("stopped fog instance {target_pid} (script '{script}')");
        } else {
            eprintln!(
                "warning: instance {target_pid} did not stop; restarting anyway may conflict"
            );
        }
        // Extra grace for socket file removal.
        wait_for_socket_gone(path);

        // Resolve config path for the relaunch. Prefer the killed instance's
        // config_dir (worktree-accurate), fall back to cli --config.
        let config_path = if let Some(dir) = config_dir {
            let p = PathBuf::from(&dir).join("fog.json");
            if p.exists() {
                p
            } else {
                resolve_config_path(&resolve_run_config(cli))
            }
        } else {
            resolve_config_path(&resolve_run_config(cli))
        };

        // Spawn a detached instance with the same script. The config_path already
        // points at the correct worktree's fog.json, so no --branch is needed
        // (branch is inferred from the worktree containing config_dir).
        let new_pid = spawn_instance_detached(&config_path, &script, None)?;
        println!("restarted fog '{script}' (old pid {target_pid} → new pid {new_pid})");
        if let Some(b) = branch {
            println!("  branch: {b}");
        }
        println!("  status: fog ls {new_pid}");
        println!("  logs:   fog logs {new_pid}");
    }
    Ok(())
}

/// Spawns a `fog <script>` detached instance for restart, mirroring
/// `daemonize` / `index::spawn_detached` but for generic scripts.
fn spawn_instance_detached(
    config_path: &Path,
    script: &str,
    branch: Option<&str>,
) -> io::Result<u32> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fog"));
    let mut cmd = Command::new(&exe);
    cmd.arg("--config")
        .arg(config_path)
        .arg(script)
        .env("FOG_DAEMON_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(b) = branch {
        cmd.arg("--branch").arg(b);
    }
    crate::process::detach_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::other(format!("could not restart fog '{script}': {e}")))?;
    let pid = child.id();
    let socket = ipc::socket_path(pid);
    let deadline = std::time::Instant::now() + DAEMON_READY_TIMEOUT;
    loop {
        if ipc::query_status(&socket).is_ok() {
            return Ok(pid);
        }
        if child.try_wait().ok().flatten().is_some() {
            return Err(io::Error::other(format!(
                "restarted fog '{script}' (pid {pid}) exited during startup; logs: {}",
                ipc::instance_log_dir(pid).display()
            )));
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "restarted fog '{script}' (pid {pid}) did not become ready within {}s; logs: {}",
                DAEMON_READY_TIMEOUT.as_secs(),
                ipc::instance_log_dir(pid).display()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Normalizes a directory for comparison: canonicalized when possible,
/// otherwise an absolute path without symlink resolution.
fn normalize_dir(dir: &Path) -> String {
    if let Ok(c) = dir.canonicalize() {
        return c.to_string_lossy().into_owned();
    }
    if dir.is_absolute() {
        return dir.to_string_lossy().into_owned();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(dir).to_string_lossy().into_owned())
        .unwrap_or_else(|_| dir.to_string_lossy().into_owned())
}

/// Whether an instance's `config_dir` matches the local config directory.
/// Both sides are normalized so symlinked checkouts compare equal.
fn config_dir_matches(instance_dir: Option<&str>, local_dir: &Path) -> bool {
    match instance_dir {
        Some(d) => normalize_dir(Path::new(d)) == normalize_dir(local_dir),
        None => false,
    }
}

/// Resolves the local config directory for PID-less scoping, without the
/// side effects of `resolve_run_config` (no stderr noise, no exit).
///
/// Honors `--branch` (relative `--config` is resolved against that branch's
/// worktree) and `--config` (file or directory). Returns `None` when no
/// `fog.json` exists at the resolved location.
fn local_config_dir(cli: &Cli) -> Option<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base = match &cli.branch {
        Some(branch) => {
            let worktrees = crate::worktree::list(&cwd)?;
            let wt = worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(branch.as_str()))?;
            if cli.config.is_absolute() {
                cli.config.clone()
            } else {
                wt.path.join(&cli.config)
            }
        }
        None => cli.config.clone(),
    };
    let config_path = resolve_config_path(&base);
    if !config_path.is_file() {
        return None;
    }
    let absolute = if config_path.is_absolute() {
        config_path
    } else {
        cwd.join(&config_path)
    };
    absolute.parent().map(Path::to_path_buf)
}

/// Best-effort status snapshot per instance PID; unreachable instances are
/// skipped (stale sockets are left for `cmd_ls` to clean).
fn query_statuses(
    instances: &[(u32, PathBuf)],
) -> std::collections::HashMap<u32, ipc::StatusResponse> {
    let mut out = std::collections::HashMap::new();
    for (pid, path) in instances {
        if let Ok(status) = ipc::query_status(path) {
            out.insert(*pid, status);
        }
    }
    out
}

/// Pure target selection shared by kill/restart/logs.
///
/// - Explicit `pid` always wins (errors when unknown; `--all` + PID is an error).
/// - `--all` with a local config selects every local match (error when none);
///   without a local config it selects everything globally.
/// - No PID, no `--all`: with a local config, one local match is selected,
///   several produce a scoped list error, and zero fall back to the legacy
///   global rule (single instance selected, several listed). Without a local
///   config the legacy global rule applies directly.
///
/// Returns owned `(pid, socket)` pairs so multi-target commands can iterate.
fn select_targets(
    instances: &[(u32, PathBuf)],
    statuses: &std::collections::HashMap<u32, ipc::StatusResponse>,
    pid: Option<u32>,
    all: bool,
    local_dir: Option<&Path>,
    cmd: &str,
) -> Result<Vec<(u32, PathBuf)>, String> {
    if let Some(pid) = pid {
        if all {
            return Err(format!(
                "error: --all cannot be used with a PID (got {pid})"
            ));
        }
        return instances
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(p, path)| vec![(*p, path.clone())])
            .ok_or_else(|| format!("error: no fog instance with pid {pid}"));
    }

    if all {
        match local_dir {
            Some(dir) => {
                let matched: Vec<(u32, PathBuf)> = instances
                    .iter()
                    .filter(|(p, _)| {
                        statuses
                            .get(p)
                            .and_then(|s| s.config_dir.as_deref())
                            .is_some_and(|d| config_dir_matches(Some(d), dir))
                    })
                    .map(|(p, path)| (*p, path.clone()))
                    .collect();
                if matched.is_empty() {
                    return Err(format!(
                        "error: no fog instances from this config ({}); nothing to apply --all to",
                        dir.display()
                    ));
                }
                return Ok(matched);
            }
            // Outside a directory with fog.json, --all is global.
            None => {
                return Ok(instances.to_vec());
            }
        }
    }

    // No PID, no --all: prefer the local config scope when resolvable.
    if let Some(dir) = local_dir {
        let matched: Vec<(u32, PathBuf)> = instances
            .iter()
            .filter(|(p, _)| {
                statuses
                    .get(p)
                    .and_then(|s| s.config_dir.as_deref())
                    .is_some_and(|d| config_dir_matches(Some(d), dir))
            })
            .map(|(p, path)| (*p, path.clone()))
            .collect();
        match matched.len() {
            1 => return Ok(matched),
            0 => { /* fall through to the legacy global rule */ }
            _ => {
                let mut msg = format!(
                    "error: multiple fog instances from this config ({}), specify a pid:",
                    dir.display()
                );
                for (p, _) in &matched {
                    msg.push_str(&format!("\n  fog {cmd} {p}"));
                }
                if matches!(cmd, "kill" | "restart") {
                    msg.push_str(&format!(
                        "\n  fog {cmd} --all   (apply to all {n})",
                        n = matched.len()
                    ));
                }
                return Err(msg);
            }
        }
    }

    if instances.len() == 1 {
        Ok(vec![instances[0].clone()])
    } else {
        let mut msg = "error: multiple fog instances running, specify a pid:".to_string();
        for (p, _) in instances {
            msg.push_str(&format!("\n  fog {cmd} {p}"));
        }
        Err(msg)
    }
}

/// Resolves targets and exits with the rendered error on failure.
fn resolve_targets(
    instances: &[(u32, PathBuf)],
    pid: Option<u32>,
    all: bool,
    cli: &Cli,
    cmd: &str,
) -> Vec<(u32, PathBuf)> {
    let local = local_config_dir(cli);
    let statuses = query_statuses(instances);
    match select_targets(instances, &statuses, pid, all, local.as_deref(), cmd) {
        Ok(targets) => targets,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    }
}

/// Strips ANSI escape sequences from `s`, producing plain text. Used to render
/// the raw PTY output captured in detached log files.
fn strip_ansi(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\x1b' => {
                if i + 1 >= chars.len() {
                    break;
                }
                match chars[i + 1] {
                    // CSI: consume until the final byte (0x40–0x7e).
                    '[' => {
                        i += 2;
                        while i < chars.len() && !('\u{40}'..='\u{7e}').contains(&chars[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    // OSC: consume until BEL or ST (`ESC \`).
                    ']' => {
                        i += 2;
                        loop {
                            if i >= chars.len() {
                                break;
                            }
                            if chars[i] == '\u{07}' {
                                i += 1;
                                break;
                            }
                            if chars[i] == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    // Two-character escape (e.g. ESC M): skip the second char.
                    _ => i += 2,
                }
            }
            // Drop lone carriage returns so `\r\n` renders as clean lines.
            '\r' => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Builds the `(service, status)` rows for `fog logs`: one per script service
/// (`healthy`/`unhealthy`/… while running, `stopped` otherwise), plus `daemon`
/// when its log file exists and `proxy` when one is configured.
fn log_service_rows(status: &ipc::StatusResponse, daemon_exists: bool) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = status
        .services
        .iter()
        .map(|s| {
            let state = if s.running {
                s.health.clone()
            } else {
                "stopped".to_string()
            };
            (s.name.clone(), state)
        })
        .collect();
    if daemon_exists {
        rows.push(("daemon".to_string(), "running".to_string()));
    }
    if let Some(p) = &status.proxy {
        let state = if p.running {
            format!(":{}", p.port)
        } else {
            ":down".to_string()
        };
        rows.push(("proxy".to_string(), state));
    }
    rows
}

/// A `--head`/`--tail` window in the spirit of `head -n` / `tail -n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineRange {
    /// First `n` lines (`--head N`).
    First(usize),
    /// All but the last `n` lines (`--head -N`).
    ExceptLast(usize),
    /// Last `n` lines (`--tail N` / `--tail -N`).
    Last(usize),
    /// From 1-indexed line `n` to the end (`--tail +N`).
    From(usize),
}

/// Parses `--head`: `N` (first N) or `-N` (all but the last N).
fn parse_head_spec(s: &str) -> Result<LineRange, String> {
    let (digits, make): (&str, fn(usize) -> LineRange) = match s.strip_prefix('-') {
        Some(rest) => (rest, LineRange::ExceptLast),
        None => (s, LineRange::First),
    };
    digits
        .parse::<usize>()
        .map(make)
        .map_err(|_| format!("invalid --head value '{s}': expected N or -N"))
}

/// Parses `--tail`: `N`/`-N` (last N) or `+N` (from line N to the end).
fn parse_tail_spec(s: &str) -> Result<LineRange, String> {
    let (digits, make): (&str, fn(usize) -> LineRange) = match s.strip_prefix('+') {
        Some(rest) => (rest, LineRange::From),
        None => (s.strip_prefix('-').unwrap_or(s), LineRange::Last),
    };
    digits
        .parse::<usize>()
        .map(make)
        .map_err(|_| format!("invalid --tail value '{s}': expected N, -N or +N"))
}

/// Resolves a window against a file of `total` lines into a half-open
/// `[start, end)` line index range.
fn resolve_range(range: LineRange, total: usize) -> (usize, usize) {
    match range {
        LineRange::First(n) => (0, n.min(total)),
        LineRange::ExceptLast(n) => (0, total.saturating_sub(n)),
        LineRange::Last(n) => (total.saturating_sub(n), total),
        LineRange::From(n) => (n.saturating_sub(1).min(total), total),
    }
}

/// Resolves `--head`/`--tail` into ordered, non-overlapping line segments.
/// `--head N --tail M` yields the first N and last M lines as two segments
/// (the caller prints an elision marker between them); overlapping windows
/// merge into one.
fn line_segments(
    head: Option<LineRange>,
    tail: Option<LineRange>,
    total: usize,
) -> Vec<(usize, usize)> {
    let mut segs: Vec<(usize, usize)> = Vec::new();
    if let Some(h) = head {
        segs.push(resolve_range(h, total));
    }
    if let Some(t) = tail {
        segs.push(resolve_range(t, total));
    }
    segs.retain(|(start, end)| end > start);
    segs.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(segs.len());
    for (start, end) in segs {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The marker printed between two disjoint selected segments.
fn omitted_marker(n: usize) -> String {
    format!("... {n} lines omitted ...")
}

/// Applies `head`/`tail` to an in-memory slice of lines (used for the proxy
/// request log), inserting the elision marker between disjoint segments.
fn select_lines(lines: &[String], head: Option<LineRange>, tail: Option<LineRange>) -> Vec<String> {
    let segs = line_segments(head, tail, lines.len());
    let mut out = Vec::new();
    for (i, &(start, end)) in segs.iter().enumerate() {
        if i > 0 {
            let omitted = start - segs[i - 1].1;
            if omitted > 0 {
                out.push(omitted_marker(omitted));
            }
        }
        out.extend_from_slice(&lines[start..end]);
    }
    out
}

/// Counts newline-terminated (or final partial) lines in a log file without
/// holding it in memory.
fn count_log_lines(file: &Path) -> io::Result<usize> {
    let mut reader = BufReader::new(fs::File::open(file)?);
    let mut buf = Vec::new();
    let mut count = 0usize;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(count);
        }
        count += 1;
    }
}

/// Streams only the selected line segments of a log file, emitting the
/// elision marker between them. ANSI sequences are stripped and trailing
/// `\r` removed, matching the whole-file path.
fn print_log_slice(
    file: &Path,
    head: Option<LineRange>,
    tail: Option<LineRange>,
) -> io::Result<()> {
    let total = count_log_lines(file)?;
    let segs = line_segments(head, tail, total);
    if segs.is_empty() {
        return Ok(());
    }
    let mut reader = BufReader::new(fs::File::open(file)?);
    let mut buf = Vec::new();
    let mut idx = 0usize;
    for (i, &(start, end)) in segs.iter().enumerate() {
        while idx < start {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                return Ok(());
            }
            idx += 1;
        }
        if i > 0 {
            let omitted = start - segs[i - 1].1;
            if omitted > 0 {
                println!("{}", omitted_marker(omitted));
            }
        }
        while idx < end {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                return Ok(());
            }
            let text = String::from_utf8_lossy(&buf);
            println!("{}", strip_ansi(text.trim_end_matches(['\r', '\n'])));
            idx += 1;
        }
    }
    Ok(())
}

/// Prints one captured log file as a single `==== <script> (<name>) ====`
/// section. With `head`/`tail` set, only the selected line window is printed
/// (ANSI stripped); otherwise the whole file is printed as before.
fn print_log_file(
    script: &str,
    name: &str,
    file: &Path,
    head: Option<LineRange>,
    tail: Option<LineRange>,
) {
    println!("==== {} ({}) ====", script, name);
    if head.is_none() && tail.is_none() {
        match fs::read_to_string(file) {
            Ok(content) => {
                print!("{}", strip_ansi(&content));
                if !content.ends_with('\n') {
                    println!();
                }
            }
            Err(e) => eprintln!("error: could not read {}: {e}", file.display()),
        }
        return;
    }
    if let Err(e) = print_log_slice(file, head, tail) {
        eprintln!("error: could not read {}: {e}", file.display());
    }
}

/// Lists the instance's services and their status, with a hint pointing at
/// `--service`. Used when `fog logs` runs without a service filter and when
/// reporting an unknown `--service` name.
fn print_available_services(target_pid: u32, script: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        println!("(no services for instance {target_pid})");
        return;
    }
    println!("Services for instance {target_pid} (script '{script}'):");
    let w_service = rows
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0)
        .max("service".len());
    let w_status = rows
        .iter()
        .map(|(_, state)| state.len())
        .max()
        .unwrap_or(0)
        .max("status".len());
    println!("  {:<w_service$}  {:<w_status$}", "service", "status");
    for (name, state) in rows {
        println!("  {:<w_service$}  {:<w_status$}", name, state);
    }
    println!();
    println!("Show one service: fog logs {target_pid} --service <name>");
}

/// Prints the captured logs of a running instance.
///
/// Without `service`, lists the available service names and their status.
/// With `service`, prints only that service's captured output (`daemon` reads
/// `daemon.log`; `proxy` streams the live request log over IPC; anything else
/// reads `<service>.log`). `--head`/`--tail` limit which lines are printed.
fn cmd_logs(pid: Option<u32>, service: Option<String>, cli: &Cli) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        eprintln!("error: no running fog instances");
        std::process::exit(1);
    }

    let targets = resolve_targets(&instances, pid, false, cli, "logs");
    let (target_pid, path) = (targets[0].0, &targets[0].1);

    let status = match ipc::query_status(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not query instance {target_pid}: {e}");
            std::process::exit(1);
        }
    };

    let dir = ipc::instance_log_dir(target_pid);
    let rows = log_service_rows(&status, dir.join("daemon.log").is_file());

    let Some(wanted) = service.as_deref() else {
        print_available_services(target_pid, &status.script, &rows);
        return Ok(());
    };

    // Exact match first, then a sanitized filename-stem match (so a service
    // whose name contains `/` etc. is found by either form). A stale
    // `<name>.log` left by a removed service is still printable by name even
    // though it is not listed.
    let canonical = rows
        .iter()
        .map(|(name, _)| name.as_str())
        .find(|name| *name == wanted)
        .or_else(|| {
            let safe = ipc::sanitize_service_name(wanted);
            rows.iter()
                .map(|(name, _)| name.as_str())
                .find(|name| ipc::sanitize_service_name(name) == safe || *name == safe)
        })
        .map(str::to_string);
    let Some(name) = canonical.or_else(|| {
        let file = dir.join(format!("{}.log", ipc::sanitize_service_name(wanted)));
        file.is_file().then(|| wanted.to_string())
    }) else {
        eprintln!("error: unknown service '{wanted}' for instance {target_pid}");
        print_available_services(target_pid, &status.script, &rows);
        std::process::exit(1);
    };

    if name == "proxy" {
        // The proxy log lives in a bounded in-memory queue; fetch enough to
        // resolve any window, but let a plain `--tail N` fetch just N.
        let fetch = match (cli.head, cli.tail) {
            (None, Some(LineRange::Last(n))) => n.max(1),
            _ => 10_000,
        };
        match ipc::query_logs(path, "proxy", fetch) {
            Ok(lines) => {
                println!("==== {} (proxy) ====", status.script);
                if cli.head.is_none() && cli.tail.is_none() {
                    for line in lines {
                        println!("{line}");
                    }
                } else {
                    for line in select_lines(&lines, cli.head, cli.tail) {
                        println!("{line}");
                    }
                }
            }
            Err(e) => {
                eprintln!("error: could not query proxy log on instance {target_pid}: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let file = dir.join(format!("{}.log", ipc::sanitize_service_name(&name)));
    if !file.is_file() {
        eprintln!("error: instance {target_pid} has no captured log for service '{name}'");
        std::process::exit(1);
    }
    print_log_file(&status.script, &name, &file, cli.head, cli.tail);
    Ok(())
}

/// Restores the terminal (raw mode off, alternate screen left, mouse capture
/// disabled) whenever it is dropped or when setup fails partway, so an early
/// `?` or panic never leaves the user's terminal in TUI state.
struct TerminalGuard;

impl TerminalGuard {
    /// Enters raw mode, then the alternate screen with mouse capture. If the
    /// escape sequence fails, the terminal is restored before the error is
    /// returned so a failed setup still leaves it clean.
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(stdout(), EnterAlternateScreen, EnableMouseCapture) {
            Self::restore();
            return Err(e);
        }
        Ok(Self)
    }

    /// Best-effort restore, shared by `Drop` and the panic hook. Failures are
    /// ignored: this runs on paths where there is nothing useful left to do.
    fn restore() {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

fn run_script(name: &str, cli: &Cli) -> io::Result<()> {
    // A `-d` run executes as a background daemon (re-executed by `main` with
    // `FOG_DAEMON_CHILD` set); the daemon child skips the TUI and runs a
    // headless service loop instead.
    let detached = cli.detach || std::env::var_os("FOG_DAEMON_CHILD").is_some();

    // Drop the daemon marker so it does not leak into spawned services. No
    // threads have been spawned yet, so mutating the environment is race-free.
    if detached {
        // SAFETY: the process is still single-threaded at this point.
        unsafe { std::env::remove_var("FOG_DAEMON_CHILD") };
    }

    // Detached daemons redirect their own diagnostics into a per-instance log
    // directory early (so startup/reclaim messages are captured too); every
    // run (interactive or detached) tees each service's raw PTY output into
    // its own file there, which also feeds `fog logs` and the web log viewer.
    let log_dir = if detached {
        let dir = create_log_dir()?;
        redirect_daemon_output(&dir)?;
        Some(dir)
    } else {
        Some(create_log_dir()?)
    };

    let config_path = resolve_config_path(&resolve_run_config(cli));
    let config = load_config(&config_path);
    let script = match config.scripts.get(name) {
        Some(s) => s,
        None => list_scripts_and_exit(&config, &format!("error: unknown script '{}'", name)),
    };

    // Setup diagnostics are collected rather than printed immediately: in
    // interactive mode the TUI takes the screen moments later, so warnings are
    // surfaced in-app and everything is persisted to the instance's daemon.log.
    let mut startup_messages: Vec<String> = Vec::new();

    // Apply the configured dnsmasq wildcard-DNS routes before the TUI enters
    // raw mode, so sudo can prompt on a normal terminal. Best-effort: failures
    // only warn and never block the run.
    if let Some(dnsmasq) = config.dnsmasq.as_ref() {
        startup_messages.extend(crate::dnsmasq::ensure(dnsmasq, detached));
    }

    // Bring up the central reverse-proxy router (Traefik), mirroring the
    // dnsmasq pattern: a host-global resource applied once that every project
    // and branch shares, so no app runs its own conflicting instance.
    if let Some(router) = config.router.as_ref() {
        // TLS certs cover the configured dnsmasq domains so every per-branch
        // hostname is valid over HTTPS.
        let domains = config
            .dnsmasq
            .as_ref()
            .map(|d| d.domains.clone())
            .unwrap_or_default();
        startup_messages.extend(crate::router::ensure(router, &domains));
    }
    // Standalone index server (service directory + web UI). Controlled by
    // fog config (`~/.config/fog/fog.json` alongside `theme`, plus per-project
    // `fog.json` top-level `index`). Both default true; either can opt-out.
    if config.effective_should_serve_index() {
        startup_messages.extend(crate::index::ensure_for_config(&config));
    }

    let config_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.clone());
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let project = crate::project::detect(&config_dir)
        .or_else(|| crate::project::fallback_identity(&config_dir));
    // Branch for instance identity: an explicit `--branch` wins; otherwise it
    // is resolved from the worktree containing the config (so a plain
    // `fog dev` in a worktree gets that worktree's branch).
    let branch = cli
        .branch
        .clone()
        .or_else(|| crate::runtime::resolve_branch(&config_dir));
    let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
    let mut owner_lock: Option<crate::lock::OwnerLock> = None;
    if let Some(ref project) = project {
        // Concurrent scripts (default) start alongside existing instances of the
        // same project+script instead of replacing them, so no coordination or
        // reclaim happens. Only single-instance scripts take over from a previous
        // run (handing over `reuse` services). `--no-share` disables reuse handoff.
        if !script.concurrent && !cli.no_share {
            let reuse = reuse_names(script);
            (adopted, owner_lock) = reconcile_instance(project, name, branch.as_deref(), &reuse);
        } else if !script.concurrent && cli.no_share {
            // Still reclaim the old instance (single-instance semantics) but
            // without adopting any services.
            (adopted, owner_lock) = reconcile_instance(project, name, branch.as_deref(), &[]);
        }
    }

    let sigint = Arc::new(AtomicBool::new(false));
    let sig = sigint.clone();
    if ctrlc::set_handler(move || {
        sig.store(true, Ordering::SeqCst);
    })
    .is_err()
    {
        eprintln!("warning: could not set Ctrl+C handler");
    }

    // Publish the config dir over IPC so the web UI can discover launchable
    // projects for this instance. Set once on the (still unshared) state,
    // before the IPC server spawns.
    let mut ipc_state = ipc::IpcState::new(
        name.to_string(),
        project.clone(),
        branch.clone(),
        cli.no_share,
    );
    ipc_state.config_dir = Some(config_dir.to_string_lossy().into_owned());
    let ipc_state = Arc::new(ipc_state);

    // Restore the terminal on panic (interactive runs only). Installed before
    // raw mode is enabled so an early panic during setup still leaves the
    // terminal usable; the previously installed hook is chained afterwards.
    if !detached {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            TerminalGuard::restore();
            previous_hook(info);
        }));
    }

    // Held for the rest of the function: an `Err` or panic anywhere from here
    // through `ratatui::run` drops the guard and restores the terminal.
    let _terminal_guard = if !detached {
        Some(TerminalGuard::enter()?)
    } else {
        None
    };

    let scrollback = config.max_scrollback.unwrap_or(DEFAULT_SCROLLBACK);
    let sidebar_min = config
        .sidebar
        .as_ref()
        .and_then(|s| s.min_width)
        .unwrap_or(12);
    let sidebar_max = config
        .sidebar
        .as_ref()
        .and_then(|s| s.max_width)
        .unwrap_or(30);
    // A misconfigured min > max would panic in `u16::clamp` on the first draw;
    // normalize the bounds instead.
    let (sidebar_min, sidebar_max) = (sidebar_min.min(sidebar_max), sidebar_min.max(sidebar_max));
    let theme = Theme::from_config(config.theme.as_ref());

    // Allocate ports (top-level `ports: { name: 0 }` => random) and validate.
    // Explicit only: any ${ports.*} template referencing a missing name fails
    // later during template resolution.
    let branch_for_ports = cli
        .branch
        .clone()
        .or_else(|| crate::runtime::resolve_branch(&config_dir));

    // Concurrent starts must agree on a shared resource's ports. Serialize the
    // allocate -> adopt -> publish -> serve critical section under the
    // per-(project, script, branch) owner lock so a racing sibling adopts the
    // winner's ports instead of allocating divergent ones. Single-instance mode
    // already holds this lock (from reclaim) and is released after startup.
    let has_shared = script
        .service
        .as_ref()
        .is_some_and(|s| s.iter().any(|e| e.share));
    let startup_lock = if script.concurrent && !cli.no_share && has_shared {
        project.as_ref().and_then(|p| {
            crate::lock::OwnerLock::acquire_with_timeout(
                p,
                name,
                branch_for_ports.as_deref(),
                std::time::Duration::from_secs(5),
            )
            .ok()
            .flatten()
        })
    } else {
        None
    };

    // Run-time `--port NAME=PORT` overrides are merged onto the config's
    // `ports` map before allocation: they can replace a configured value or
    // define a name the config does not declare. Duplicate or malformed
    // overrides fail fast.
    let overrides = match crate::ports::parse_port_overrides(&cli.port) {
        Ok(m) => m,
        Err(e) => {
            if !detached {
                let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
                let _ = disable_raw_mode();
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("error: {e}"),
            ));
        }
    };
    let mut port_specs = config.ports.clone().unwrap_or_default();
    let ports_defined = config.ports.is_some() || !overrides.is_empty();
    port_specs.extend(overrides);

    let mut port_map = if !port_specs.is_empty() {
        match crate::ports::allocate_ports(&port_specs) {
            Ok(m) => m,
            Err(e) => {
                if !detached {
                    let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
                    let _ = disable_raw_mode();
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("error: {}", e),
                ));
            }
        }
    } else {
        std::collections::HashMap::new()
    };
    // Validate native_routes and ensure ${ports.*} has a top-level `ports` map
    if let Some(routes) = &config.native_routes
        && let Err(e) =
            crate::ports::validate_native_routes(routes, &port_map, branch_for_ports.as_deref())
    {
        if !detached {
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("error: {e}"),
        ));
    }
    if let Err(e) = crate::ports::ensure_ports_defined(
        ports_defined.then_some(&port_specs),
        script,
        config.native_routes.as_ref(),
    ) {
        if !detached {
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("error: {e}"),
        ));
    }
    if let Err(e) = crate::ports::validate_endpoints(script, &port_map, branch_for_ports.as_deref())
    {
        if !detached {
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("error: {e}"),
        ));
    }
    // Adopt the live ports of a sibling that already owns a shared service, so
    // dependent services resolve `${ports.*}` to the borrowed resource's real
    // port instead of this instance's freshly allocated one.
    let owned_shared = crate::runtime::adopt_shared_ports(
        script,
        name,
        &config_dir,
        project.as_deref(),
        branch_for_ports.as_deref(),
        &mut port_map,
        cli.no_share,
    );
    // Publish allocated ports + native routes to IPC so the index server can synthesize
    // native ApiService entries for the Services UI (which otherwise only sees docker).
    {
        *ipc_state.ports.lock().expect("mutex poisoned") = port_map.clone();
        let routes: Vec<crate::ipc::NativeRouteInfo> = config
            .native_routes
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|r| crate::ipc::NativeRouteInfo {
                host: r.host,
                service: r.service,
                port: r.port,
                path_prefix: r.path_prefix,
                endpoint: r.endpoint,
            })
            .collect();
        *ipc_state.native_routes.lock().expect("mutex poisoned") = routes;
    }
    // Only now make this instance discoverable over IPC, so a sibling can never
    // observe it without its allocated ports and adopt a wrong one.
    if let Err(e) = ipc::spawn_server(ipc_state.clone()) {
        if !detached {
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
        }
        return Err(e);
    }
    // Port assignment is settled; let a racing sibling proceed.
    drop(startup_lock);
    // Bring up native Traefik routes for allocated ports (explicit only)
    if let Some(routes) = &config.native_routes {
        let messages = crate::router::ensure_native_routes(
            routes,
            &port_map,
            branch_for_ports.as_deref(),
            &config,
            cli.verbose,
        );
        startup_messages.extend(messages);
    }

    let runtime = crate::runtime::build_with_ports_no_share(
        script,
        name,
        &config_dir,
        project.clone(),
        cli.save_logs,
        scrollback,
        log_dir.clone(),
        &mut adopted,
        &port_map,
        branch_for_ports.clone(),
        cli.no_share,
        &owned_shared,
    )
    .map_err(|e| {
        // Close any handoff fds we duped that the failed build did not consume.
        for (_, handoff) in adopted.drain() {
            crate::fds::close(handoff.fd);
        }
        // Restore the terminal before reporting so it is usable again.
        if !detached {
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
        }
        io::Error::new(io::ErrorKind::InvalidData, format!("error: {}", e))
    })?;
    // Declared endpoint routes: flatten from the built terminals, publish
    // them to IPC (the index server nests them under their parent service) and
    // bring up their Traefik routes.
    {
        let infos = crate::runtime::endpoint_route_infos(&runtime.items);
        if !infos.is_empty() {
            ipc_state
                .native_routes
                .lock()
                .expect("mutex poisoned")
                .extend(infos);
        }
        let sub_routes = crate::runtime::endpoint_routes(&runtime.items);
        if !sub_routes.is_empty() {
            let messages = crate::router::ensure_native_routes(
                &sub_routes,
                &port_map,
                branch_for_ports.as_deref(),
                &config,
                cli.verbose,
            );
            startup_messages.extend(messages);
        }
    }
    // Non-fatal config warnings raised during build (e.g. share without a
    // health check) surface alongside the other startup warnings.
    startup_messages.extend(runtime.warnings.iter().cloned());

    // Log allocated ports for visibility (also useful for `fog logs`)
    if !port_map.is_empty() {
        let mut names: Vec<&String> = port_map.keys().collect();
        names.sort();
        for n in names {
            startup_messages.push(format!("  + port {} -> {}", n, port_map[n]));
        }
    }

    // Expose the proxy's live request log to the IPC server so the web log
    // viewer (and anything else) can stream it. The handle is stable across
    // config hot-reloads, so wiring it once here is enough.
    if let Some(proxy) = runtime.proxy.as_ref() {
        *ipc_state.proxy_logs.lock().expect("mutex poisoned") = Some(proxy.logs_handle());
    }

    // Services are up: release the owner lock so a later worktree switch can
    // replace this instance.
    drop(owner_lock);

    // A detached daemon has no UI to hot-reload, so the config watcher is skipped.
    let (config_rx, config_watcher_stop) = if detached {
        (
            std::sync::mpsc::channel().1,
            Arc::new(AtomicBool::new(false)),
        )
    } else {
        config_watcher::spawn_config_watcher(config_path.clone())
    };

    // Persist setup diagnostics, then surface warnings. Detached runs already
    // redirected stderr into daemon.log, so they only need the historic emit;
    // interactive runs append to daemon.log and show a dismissible overlay.
    let startup_warnings: Vec<String> = if detached {
        crate::log::emit(&startup_messages, cli.verbose);
        Vec::new()
    } else {
        if let Some(dir) = log_dir.as_ref() {
            crate::log::append_daemon_log(dir, &startup_messages, cli.verbose);
        }
        startup_messages
            .iter()
            .filter(|m| !crate::log::is_info(m))
            .cloned()
            .collect()
    };

    let mut app = App::new_with_opts(crate::app::AppCreateOpts {
        items: runtime.items,
        pending_services: runtime.pending_services,
        proxy: runtime.proxy,
        sigint,
        scrollback,
        sidebar_min,
        sidebar_max,
        theme,
        config_path,
        config_rx,
        config_watcher_stop,
        ipc_state,
        config_rel: cli.config.clone(),
        save_logs: cli.save_logs,
        no_share: cli.no_share,
        verbose: cli.verbose,
        startup_messages: startup_warnings,
    });
    if detached {
        app.run_headless()?;
    } else {
        ratatui::run(|terminal| app.run(terminal))?;
    }

    if config.effective_should_serve_index() {
        crate::log::emit(&crate::index::ensure_for_config(&config), cli.verbose);
    }

    ipc::cleanup_socket();

    // Clean up native Traefik routes for this branch (explicit only).
    crate::router::cleanup_native_routes(branch_for_ports.as_deref(), &config);

    // If no fog instances remain, tear down the index server as well.
    // Give the socket file a moment to disappear from the filesystem.
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Use the effective index port (project → global fallback) so a custom
    // port is torn down correctly.
    let port = config.effective_index_port();
    crate::index::maybe_terminate_on_port(port);

    Ok(())
}

/// Creates the per-instance log directory for the current process, which
/// every run (interactive or detached) tees each service's output into.
fn create_log_dir() -> io::Result<PathBuf> {
    let dir = ipc::instance_log_dir(std::process::id());
    fs::create_dir_all(&dir)?;
    // Restrict to the owner, matching `ipc::ensure_instance_dir`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dir)?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&dir, perms)?;
    }
    Ok(dir)
}

/// Redirects this process's stdout/stderr into `daemon.log` inside `dir`,
/// so a detached daemon's own diagnostics are captured too. Only called for
/// detached runs — an interactive run keeps stdout/stderr for the TUI.
#[cfg(unix)]
fn redirect_daemon_output(dir: &Path) -> io::Result<()> {
    use std::os::unix::io::IntoRawFd;
    let log = fs::File::create(dir.join("daemon.log"))?;
    // SAFETY: dup2 onto the standard fds is always valid, and the original
    // fd is ours to close.
    let fd = log.into_raw_fd();
    unsafe {
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
        libc::close(fd);
    }
    Ok(())
}

/// Redirects this process's stdout/stderr into `daemon.log` inside `dir`.
///
/// Windows has no `dup2`; instead the standard handles are pointed at the log
/// file, which Rust's stdout/stderr resolve on each write.
#[cfg(windows)]
fn redirect_daemon_output(dir: &Path) -> io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    let log = fs::File::create(dir.join("daemon.log"))?;
    // Leak the handle so the log stays open for the process lifetime.
    let handle = log.into_raw_handle();
    // SAFETY: both are valid standard-handle selectors and the file handle
    // stays open for the process lifetime.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
    Ok(())
}

/// Detach mode entry point: re-executes `fog <script>` as a background daemon
/// and waits until it is serving, printing the PID once `fog ls` can see it.
fn daemonize(script: &str) -> io::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fog"));
    let args: Vec<OsString> = std::env::args_os()
        .skip(1)
        .filter(|a| {
            let s = a.to_string_lossy();
            s != "-d" && s != "--detach" && !s.starts_with("--detach=")
        })
        .collect();

    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .env("FOG_DAEMON_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::process::detach_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::other(format!("could not start detached fog: {e}")))?;
    let pid = child.id();

    // Wait until the daemon's socket serves a status reply (created only after
    // the reconcile/reclaim window), so we report success exactly when
    // `fog ls` / `fog kill` / `fog logs` will work.
    let socket = ipc::socket_path(pid);
    let deadline = std::time::Instant::now() + DAEMON_READY_TIMEOUT;
    loop {
        if ipc::query_status(&socket).is_ok() {
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            eprintln!("error: detached fog '{script}' (pid {pid}) exited during startup");
            eprintln!("  logs: {}", ipc::instance_log_dir(pid).display());
            std::process::exit(1);
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "error: detached fog '{script}' (pid {pid}) did not become ready within {}s",
                DAEMON_READY_TIMEOUT.as_secs()
            );
            eprintln!("  logs: {}", ipc::instance_log_dir(pid).display());
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("fog '{script}' started in background (pid {pid})");
    println!("  status: fog ls {pid}");
    println!("  stop:   fog kill {pid}");
    println!("  logs:   fog logs {pid}");
    println!("  log dir: {}", ipc::instance_log_dir(pid).display());
    Ok(())
}

/// Parses `fog index serve` flags: `--foreground` runs in the foreground
/// (blocking); otherwise the server detaches into the background by default.
/// `--port N` / `--port=N` / `-p N` overrides the configured port.
fn parse_index_serve_args(args: &[String]) -> (bool, Option<u16>) {
    let mut foreground = false;
    let mut port: Option<u16> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--foreground" {
            foreground = true;
        } else if a == "--port" || a == "-p" {
            i += 1;
            match args.get(i).and_then(|s| s.parse::<u16>().ok()) {
                Some(p) => port = Some(p),
                None => {
                    eprintln!(
                        "error: {a} requires a port number (e.g. `fog index serve --port 18080`)"
                    );
                    std::process::exit(1);
                }
            }
        } else if let Some(v) = a.strip_prefix("--port=") {
            match v.parse::<u16>() {
                Ok(p) => port = Some(p),
                Err(_) => {
                    eprintln!("error: invalid --port value '{v}'");
                    std::process::exit(1);
                }
            }
        } else if a == "-h" || a == "--help" {
            println!("Usage: fog index serve [--foreground] [--port N]");
            println!();
            println!("Run the index server. Detaches into the background by default;");
            println!("pass --foreground to block in the terminal.");
            std::process::exit(0);
        } else {
            eprintln!(
                "error: unknown flag '{a}' for `fog index serve` (see `fog index serve --help`)"
            );
            std::process::exit(1);
        }
        i += 1;
    }
    // The detached child (FOG_INDEX_CHILD=1) must block; it never re-detaches.
    if std::env::var_os("FOG_INDEX_CHILD").is_some() {
        foreground = true;
    }
    (foreground, port)
}

/// Entry point for `fog index serve`: detaches into a background service by
/// default (logs to `$TMPDIR/fog-index-<port>.logs/daemon.log`), or blocks
/// with `--foreground`.
fn cmd_index_serve(args: &[String]) -> io::Result<()> {
    let (foreground, explicit_port) = parse_index_serve_args(args);
    let port = crate::index::resolve_serve_port(explicit_port);
    let network = crate::index::resolve_serve_network();
    if foreground {
        crate::index::serve_foreground(port, network)
    } else {
        crate::index::serve_detached(port, &network)
    }
}

pub fn run() -> io::Result<()> {
    // `fog index serve/kill/restart` are dispatched before clap so they are not
    // misparsed as the `[PID]` positional (which expects a number).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("index") {
        match argv.get(1).map(String::as_str) {
            Some("serve") => return cmd_index_serve(&argv[2..]),
            Some("kill") => {
                // `fog index kill` terminates the index server unconditionally.
                let killed = crate::index::kill_server(None);
                if killed {
                    println!("index server stopped");
                } else {
                    eprintln!("index server not running");
                }
                return Ok(());
            }
            Some("restart") => {
                // `fog index restart` kills then re-ensures the server. This is
                // a manual override that ignores `index.enabled:false` so the
                // user can force-start the index even for projects that opt out
                // of auto-serving it on `fog <script>`.
                let cfg = load_runtime_config_for_index();
                crate::index::kill_for_config(&cfg);
                std::thread::sleep(std::time::Duration::from_millis(300));
                let port = cfg.index_port();
                let network = cfg.index_network();
                let verbose = argv.iter().any(|a| a == "-v" || a == "--verbose");
                crate::log::emit(&crate::index::ensure_with_port(port, &network), verbose);
                if crate::index::is_server_started(port) {
                    println!("index server restarted on :{port}");
                    println!(
                        "  logs: {}",
                        crate::index::index_log_dir(port)
                            .join("daemon.log")
                            .display()
                    );
                } else {
                    eprintln!("error: index server did not start on :{port}");
                    std::process::exit(1);
                }
                return Ok(());
            }
            _ => {}
        }
    }

    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        print!("{}", crate::completion::generate(shell));
        return Ok(());
    }

    // `--service` only applies to `fog logs`; reject it elsewhere before the
    // detach path so it can never leak into a daemon child's arguments.
    if cli.service.is_some() && cli.script.as_deref() != Some("logs") {
        eprintln!("error: --service only applies to `fog logs` (e.g. `fog logs --service api`)");
        std::process::exit(1);
    }

    // `--head`/`--tail` only apply to `fog logs`, and only alongside a service
    // selection (the plain listing is already short).
    if cli.head.is_some() || cli.tail.is_some() {
        if cli.script.as_deref() != Some("logs") {
            eprintln!(
                "error: --head/--tail only apply to `fog logs` (e.g. `fog logs <pid> -s api --tail 50`)"
            );
            std::process::exit(1);
        }
        if cli.service.is_none() {
            eprintln!(
                "error: --head/--tail require --service (e.g. `fog logs <pid> -s api --tail 50`)"
            );
            std::process::exit(1);
        }
    }

    // `--all` only applies to `fog kill` / `fog restart`, and conflicts with
    // an explicit PID.
    if cli.all {
        match cli.script.as_deref() {
            Some("kill") | Some("restart") => {}
            Some("logs") => {
                eprintln!(
                    "error: --all only applies to `fog kill` and `fog restart`; for logs, pass an explicit PID"
                );
                std::process::exit(1);
            }
            _ => {
                eprintln!("error: --all only applies to `fog kill` and `fog restart`");
                std::process::exit(1);
            }
        }
        if cli.pid.is_some() {
            eprintln!("error: --all cannot be used with a PID");
            std::process::exit(1);
        }
    }

    // `--force` only applies to `fog kill` / `fog restart`.
    if cli.force && !matches!(cli.script.as_deref(), Some("kill") | Some("restart")) {
        eprintln!("error: --force only applies to `fog kill` and `fog restart`");
        std::process::exit(1);
    }

    // `--port` only applies when running a script.
    if !cli.port.is_empty() {
        match cli.script.as_deref() {
            Some(name) if !matches!(name, "ls" | "kill" | "restart" | "logs" | "index") => {}
            _ => {
                eprintln!(
                    "error: --port only applies when running a script (e.g. `fog dev --port api=4000`)"
                );
                std::process::exit(1);
            }
        }
    }

    // Detach: run the script in the background and return once it is serving.
    // The daemon child (re-executed with FOG_DAEMON_CHILD=1) takes the
    // headless path in run_script and must not re-daemonize.
    if cli.detach && std::env::var_os("FOG_DAEMON_CHILD").is_none() {
        match cli.script.as_deref() {
            Some(name) if !matches!(name, "ls" | "kill" | "restart" | "logs" | "index") => {
                return daemonize(name);
            }
            Some(_) => {
                eprintln!("error: --detach only applies to running a script (e.g. `fog dev -d`)");
                std::process::exit(1);
            }
            None => {
                eprintln!("error: --detach requires a script (e.g. `fog dev -d`)");
                std::process::exit(1);
            }
        }
    }

    match cli.script.as_deref() {
        Some("ls") => cmd_ls(),
        Some("kill") => cmd_kill(cli.pid, cli.all, &cli),
        Some("restart") => cmd_restart(cli.pid, cli.all, &cli),
        Some("logs") => cmd_logs(cli.pid, cli.service.clone(), &cli),
        Some(name) => run_script(name, &cli),
        None => {
            let config_path = resolve_config_path(&resolve_run_config(&cli));
            let config = load_config(&config_path);
            if config.scripts.is_empty() {
                eprintln!("error: no scripts defined in '{}'", config_path.display());
                std::process::exit(1);
            }
            list_scripts_and_exit(&config, "error: no script specified")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        // Many tests in this binary call `temp_dir` concurrently. The process
        // id is shared, and the clock reports two calls made in quick
        // succession as the same nanosecond, so a per-process counter is what
        // keeps the paths unique. Without it two tests can share a directory
        // and one test's cleanup deletes another's files mid-run.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fog-resolve-config-{}-{}-{}",
            std::process::id(),
            seq,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_resolve_config_path_directory() {
        let dir = temp_dir();
        fs::write(dir.join("fog.json"), "{}").unwrap();
        assert_eq!(resolve_config_path(&dir), dir.join("fog.json"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_config_path_file() {
        let dir = temp_dir();
        let file = dir.join("custom.json");
        fs::write(&file, "{}").unwrap();
        assert_eq!(resolve_config_path(&file), file);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_config_path_non_existent() {
        let dir = temp_dir();
        let missing = dir.join("missing.json");
        assert_eq!(resolve_config_path(&missing), missing);
        let _ = fs::remove_dir_all(&dir);
    }

    fn test_status(
        services: Vec<ipc::ServiceStatus>,
        proxy: Option<ipc::ProxyStatus>,
    ) -> ipc::StatusResponse {
        ipc::StatusResponse {
            pid: 1234,
            script: "dev".to_string(),
            services,
            proxy,
            project: None,
            branch: None,
            config_dir: None,
            started_at: 0,
            ports: Default::default(),
            native_routes: Vec::new(),
            no_share: false,
        }
    }

    fn svc(name: &str, running: bool, health: &str) -> ipc::ServiceStatus {
        ipc::ServiceStatus {
            name: name.to_string(),
            running,
            health: health.to_string(),
            endpoints: Vec::new(),
            shutdown_cmd: None,
        }
    }

    #[test]
    fn test_log_service_rows_reports_health_and_stopped() {
        let status = test_status(
            vec![svc("api", true, "healthy"), svc("worker", false, "unknown")],
            None,
        );
        assert_eq!(
            log_service_rows(&status, false),
            vec![
                ("api".to_string(), "healthy".to_string()),
                ("worker".to_string(), "stopped".to_string()),
            ]
        );
    }

    #[test]
    fn test_log_service_rows_appends_daemon_and_proxy() {
        let status = test_status(
            vec![svc("api", true, "healthy")],
            Some(ipc::ProxyStatus {
                running: true,
                port: 3000,
            }),
        );
        assert_eq!(
            log_service_rows(&status, true),
            vec![
                ("api".to_string(), "healthy".to_string()),
                ("daemon".to_string(), "running".to_string()),
                ("proxy".to_string(), ":3000".to_string()),
            ]
        );
        // No daemon.log on disk and no proxy configured: no extra rows.
        let status = test_status(vec![svc("api", true, "healthy")], None);
        assert_eq!(
            log_service_rows(&status, false),
            vec![("api".to_string(), "healthy".to_string())]
        );
    }

    #[test]
    fn test_log_service_rows_proxy_down() {
        let status = test_status(
            Vec::new(),
            Some(ipc::ProxyStatus {
                running: false,
                port: 3000,
            }),
        );
        assert_eq!(
            log_service_rows(&status, false),
            vec![("proxy".to_string(), ":down".to_string())]
        );
    }

    #[test]
    fn test_strip_ansi_removes_escape_sequences() {
        let input = "\x1b[1;31merror\x1b[0m \x1b[38;2;255;128;0mok\r\nplain";
        let out = strip_ansi(input);
        assert_eq!(out, "error ok\nplain");
    }

    #[test]
    fn test_strip_ansi_handles_osc() {
        // OSC 52 clipboard / OSC title sequences must be consumed entirely.
        let input = "\x1b]52;c;QUJD\x07title\x1b]0;fog\x1b\\rest";
        let out = strip_ansi(input);
        assert_eq!(out, "titlerest");
    }

    #[test]
    fn test_strip_ansi_preserves_plain_text() {
        assert_eq!(strip_ansi("hello world"), "hello world");
        assert_eq!(strip_ansi(""), "");
    }

    fn status_with_dir(config_dir: Option<&str>) -> ipc::StatusResponse {
        let mut s = test_status(vec![], None);
        s.config_dir = config_dir.map(str::to_string);
        s
    }

    fn target_fixtures() -> (
        Vec<(u32, PathBuf)>,
        std::collections::HashMap<u32, ipc::StatusResponse>,
    ) {
        // Fixed pseudo-dirs (need not exist): normalize_dir falls back to the
        // absolute string, so equal strings still match.
        let dir_a = format!("/tmp/fog-test-scope-a-{}", std::process::id());
        let dir_b = format!("/tmp/fog-test-scope-b-{}", std::process::id());
        let instances = vec![
            (101, PathBuf::from("/tmp/fog-101.sock")),
            (102, PathBuf::from("/tmp/fog-102.sock")),
            (103, PathBuf::from("/tmp/fog-103.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(101, status_with_dir(Some(&dir_a)));
        statuses.insert(102, status_with_dir(Some(&dir_a)));
        statuses.insert(103, status_with_dir(Some(&dir_b)));
        (instances, statuses)
    }

    #[test]
    fn test_select_targets_explicit_pid_wins_over_local() {
        let (instances, statuses) = target_fixtures();
        let local = PathBuf::from("/elsewhere");
        let got = select_targets(
            &instances,
            &statuses,
            Some(103),
            false,
            Some(&local),
            "kill",
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 103);
    }

    #[test]
    fn test_select_targets_unknown_pid_errors() {
        let (instances, statuses) = target_fixtures();
        let err =
            select_targets(&instances, &statuses, Some(999), false, None, "kill").unwrap_err();
        assert!(err.contains("no fog instance with pid 999"));
    }

    #[test]
    fn test_select_targets_pid_and_all_conflict() {
        let (instances, statuses) = target_fixtures();
        let err = select_targets(&instances, &statuses, Some(101), true, None, "kill").unwrap_err();
        assert!(err.contains("--all cannot be used with a PID"));
    }

    #[test]
    fn test_select_targets_all_scoped_to_local() {
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let instances = vec![
            (11, PathBuf::from("/tmp/fog-11.sock")),
            (12, PathBuf::from("/tmp/fog-12.sock")),
            (13, PathBuf::from("/tmp/fog-13.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(11, status_with_dir(Some(&dir_a.to_string_lossy())));
        statuses.insert(12, status_with_dir(Some(&dir_a.to_string_lossy())));
        statuses.insert(13, status_with_dir(Some(&dir_b.to_string_lossy())));
        let got = select_targets(&instances, &statuses, None, true, Some(&dir_a), "kill").unwrap();
        let mut pids: Vec<u32> = got.iter().map(|(p, _)| *p).collect();
        pids.sort();
        assert_eq!(pids, vec![11, 12]);
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn test_select_targets_all_local_no_match_errors() {
        let (instances, statuses) = target_fixtures();
        let local = temp_dir();
        let err =
            select_targets(&instances, &statuses, None, true, Some(&local), "kill").unwrap_err();
        assert!(err.contains("no fog instances from this config"));
        let _ = fs::remove_dir_all(&local);
    }

    #[test]
    fn test_select_targets_all_global_without_local() {
        let (instances, statuses) = target_fixtures();
        let got = select_targets(&instances, &statuses, None, true, None, "kill").unwrap();
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn test_select_targets_pidless_single_local_match() {
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let instances = vec![
            (21, PathBuf::from("/tmp/fog-21.sock")),
            (22, PathBuf::from("/tmp/fog-22.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(21, status_with_dir(Some(&dir_a.to_string_lossy())));
        statuses.insert(22, status_with_dir(Some(&dir_b.to_string_lossy())));
        let got = select_targets(&instances, &statuses, None, false, Some(&dir_b), "logs").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 22);
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn test_select_targets_pidless_multi_local_lists_scoped() {
        let (instances, statuses) = target_fixtures();
        // dir of pid 101/102: recover from statuses.
        let local = PathBuf::from(statuses[&101].config_dir.as_deref().unwrap());
        let err =
            select_targets(&instances, &statuses, None, false, Some(&local), "kill").unwrap_err();
        assert!(err.contains("multiple fog instances from this config"));
        assert!(err.contains("fog kill 101"));
        assert!(err.contains("fog kill 102"));
        assert!(
            !err.contains("103"),
            "scoped error must not list other configs"
        );
        assert!(err.contains("--all"));
    }

    #[test]
    fn test_select_targets_pidless_multi_local_logs_has_no_all_hint() {
        let (instances, statuses) = target_fixtures();
        let local = PathBuf::from(statuses[&101].config_dir.as_deref().unwrap());
        let err =
            select_targets(&instances, &statuses, None, false, Some(&local), "logs").unwrap_err();
        assert!(err.contains("fog logs 101"));
        assert!(!err.contains("--all"));
    }

    #[test]
    fn test_select_targets_pidless_zero_local_falls_back_to_single() {
        let dir_a = temp_dir();
        let instances = vec![(31, PathBuf::from("/tmp/fog-31.sock"))];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(31, status_with_dir(Some(&dir_a.to_string_lossy())));
        let elsewhere = temp_dir();
        let got =
            select_targets(&instances, &statuses, None, false, Some(&elsewhere), "kill").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 31);
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn test_select_targets_pidless_zero_local_multi_lists_global() {
        let dir_a = temp_dir();
        let dir_b = temp_dir();
        let instances = vec![
            (41, PathBuf::from("/tmp/fog-41.sock")),
            (42, PathBuf::from("/tmp/fog-42.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(41, status_with_dir(Some(&dir_a.to_string_lossy())));
        statuses.insert(42, status_with_dir(Some(&dir_b.to_string_lossy())));
        let elsewhere = temp_dir();
        let err = select_targets(&instances, &statuses, None, false, Some(&elsewhere), "kill")
            .unwrap_err();
        assert!(err.contains("multiple fog instances running"));
        assert!(err.contains("fog kill 41"));
        assert!(err.contains("fog kill 42"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn test_select_targets_ignores_instances_without_config_dir() {
        let dir_a = temp_dir();
        let instances = vec![
            (51, PathBuf::from("/tmp/fog-51.sock")),
            (52, PathBuf::from("/tmp/fog-52.sock")),
        ];
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(51, status_with_dir(None));
        statuses.insert(52, status_with_dir(Some(&dir_a.to_string_lossy())));
        // Only pid 52 matches the local dir; pid 51 (unknown dir) is ignored
        // rather than making the result ambiguous.
        let got = select_targets(&instances, &statuses, None, false, Some(&dir_a), "logs").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 52);
        let _ = fs::remove_dir_all(&dir_a);
    }

    #[test]
    fn test_config_dir_matches_normalizes() {
        let dir = temp_dir();
        assert!(config_dir_matches(Some(&dir.to_string_lossy()), &dir));
        assert!(!config_dir_matches(Some("/definitely/not/here"), &dir));
        assert!(!config_dir_matches(None, &dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cli_accepts_force_flag() {
        let cli = Cli::try_parse_from(["fog", "kill", "--force"]).unwrap();
        assert!(cli.force);
        assert_eq!(cli.script.as_deref(), Some("kill"));

        let cli = Cli::try_parse_from(["fog", "restart", "--all", "--force"]).unwrap();
        assert!(cli.force && cli.all);
    }

    #[cfg(unix)]
    #[test]
    fn test_cli_accepts_port_overrides() {
        let cli =
            Cli::try_parse_from(["fog", "dev", "--port", "api=4000", "--port=web=0"]).unwrap();
        assert_eq!(cli.port, vec!["api=4000".to_string(), "web=0".to_string()]);
        assert_eq!(cli.script.as_deref(), Some("dev"));
    }

    #[test]
    fn test_cli_accepts_logs_head_tail() {
        let cli = Cli::try_parse_from(["fog", "logs", "-s", "api", "--tail", "50"]).unwrap();
        assert_eq!(cli.tail, Some(LineRange::Last(50)));
        // A leading '-' must be accepted as the value, not parsed as a flag.
        let cli = Cli::try_parse_from(["fog", "logs", "-s", "api", "--head", "-50"]).unwrap();
        assert_eq!(cli.head, Some(LineRange::ExceptLast(50)));
        let cli = Cli::try_parse_from(["fog", "logs", "-s", "api", "--tail", "+51"]).unwrap();
        assert_eq!(cli.tail, Some(LineRange::From(51)));
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_instance_force_kills_unresponsive_pid() {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn sleeper");
        let pid = child.id();
        // Reap concurrently: otherwise the killed child lingers as a zombie and
        // `is_pid_alive` (bare `kill(pid, 0)`) keeps reporting it as alive.
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });
        // No instance socket exists, so the graceful IPC attempt fails; force
        // must still terminate the process.
        let bogus = std::env::temp_dir().join(format!("fog-nonexistent-{pid}.sock"));
        let stopped = stop_instance(pid, &bogus, true, false);
        assert!(stopped, "force stop must terminate the process");
        reaper.join().expect("reaper thread");
        assert!(!crate::process::is_pid_alive(pid));
    }

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
        let lines = shutdown_lines(&services, SHUTDOWN_PENDING, |s| match s.name.as_str() {
            "web" => vec!["$ docker compose down".to_string()],
            "db" => vec!["db ready".to_string(), "db serving".to_string()],
            _ => Vec::new(),
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
        let lines = shutdown_lines(&services, SPINNER_FRAMES[3], |_| Vec::new());
        assert_eq!(
            lines,
            vec![format!("  {} web  shutting down", SPINNER_FRAMES[3])]
        );
    }

    #[test]
    fn test_parse_head_spec() {
        assert_eq!(parse_head_spec("50").unwrap(), LineRange::First(50));
        assert_eq!(parse_head_spec("-50").unwrap(), LineRange::ExceptLast(50));
        // A leading '+' is accepted as the plain count.
        assert_eq!(parse_head_spec("+50").unwrap(), LineRange::First(50));
        assert!(parse_head_spec("x").is_err());
    }

    #[test]
    fn test_parse_tail_spec() {
        assert_eq!(parse_tail_spec("50").unwrap(), LineRange::Last(50));
        assert_eq!(parse_tail_spec("-50").unwrap(), LineRange::Last(50));
        assert_eq!(parse_tail_spec("+51").unwrap(), LineRange::From(51));
        assert!(parse_tail_spec("x").is_err());
    }

    #[test]
    fn test_line_segments_bookends_and_merge() {
        assert_eq!(
            line_segments(Some(LineRange::First(5)), Some(LineRange::Last(10)), 100),
            vec![(0, 5), (90, 100)]
        );
        // Overlapping windows merge into the whole file.
        assert_eq!(
            line_segments(Some(LineRange::First(60)), Some(LineRange::Last(60)), 100),
            vec![(0, 100)]
        );
        // Touching windows (end == start) merge too.
        assert_eq!(
            line_segments(Some(LineRange::First(50)), Some(LineRange::Last(50)), 100),
            vec![(0, 100)]
        );
    }

    #[test]
    fn test_line_segments_single_and_signed() {
        assert_eq!(
            line_segments(Some(LineRange::First(5)), None, 100),
            vec![(0, 5)]
        );
        assert_eq!(
            line_segments(None, Some(LineRange::From(51)), 100),
            vec![(50, 100)]
        );
        assert_eq!(
            line_segments(Some(LineRange::ExceptLast(10)), None, 100),
            vec![(0, 90)]
        );
        // Windows larger than the file clamp to everything.
        assert_eq!(
            line_segments(Some(LineRange::First(200)), None, 100),
            vec![(0, 100)]
        );
        // A zero window selects nothing.
        assert_eq!(line_segments(Some(LineRange::First(0)), None, 100), vec![]);
        assert_eq!(line_segments(None, Some(LineRange::Last(0)), 100), vec![]);
    }

    #[test]
    fn test_select_lines_inserts_omission_marker() {
        let lines: Vec<String> = (1..=100).map(|i| i.to_string()).collect();
        let got = select_lines(&lines, Some(LineRange::First(2)), Some(LineRange::Last(2)));
        assert_eq!(got, vec!["1", "2", "... 96 lines omitted ...", "99", "100"]);
        // No marker for a single window.
        assert_eq!(
            select_lines(&lines, Some(LineRange::First(2)), None),
            vec!["1", "2"]
        );
        // Full overlap prints the whole file with no marker.
        assert_eq!(
            select_lines(&lines, None, Some(LineRange::Last(100))).len(),
            100
        );
    }

    #[test]
    fn test_count_log_lines_includes_partial_last_line() {
        let dir = temp_dir();
        let file = dir.join("count.log");
        fs::write(&file, "a\nb\nc").unwrap();
        assert_eq!(count_log_lines(&file).unwrap(), 3);
        fs::write(&file, "a\n").unwrap();
        assert_eq!(count_log_lines(&file).unwrap(), 1);
        fs::write(&file, "").unwrap();
        assert_eq!(count_log_lines(&file).unwrap(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
