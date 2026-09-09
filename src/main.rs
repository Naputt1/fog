#![deny(unsafe_op_in_unsafe_fn)]

use clap::Parser;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::stdout;
use std::os::unix::io::IntoRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{fs, io};

use fog::app::App;
use fog::completion::CompletionShell;
use fog::config::Config;
use fog::config_watcher;
use fog::ipc;
use fog::theme::Theme;

const DEFAULT_SCROLLBACK: usize = 2000;

/// How long a starter waits for another instance that is mid-start before
/// giving up.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a replacer waits for the old instance to fully exit.
const RECLAIM_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the parent of a detached run waits for the daemon to start serving
/// before reporting failure. Covers the owner-lock wait plus the reclaim.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Command-line interface arguments parsed via clap.
#[derive(Parser)]
#[command(
    name = "fog",
    version = env!("CARGO_PKG_VERSION"),
    about = "Terminal-based service orchestrator & reverse-proxy dashboard",
    after_help = "Built-in commands:\n  fog ls [PID]                    list running instances and service status\n  fog kill [PID|--all]            gracefully shut down a running instance\n  fog restart [PID|--all]         restart a running instance\n  fog logs [PID]                  list services and their status\n  fog logs [PID] --service NAME   print captured output of one service\n  fog index serve [--foreground] [--port N]  run the index server (detached by default)\n  fog index kill                  stop the index server\n  fog index restart               restart the index server\n\nWith no PID, kill/restart/logs target the instance started from the local\nfog.json (same config directory, so the branch is implicit). When several\nlocal instances match, the command lists them: pass an explicit PID, or\n--all (kill/restart) to apply to every local match. Outside a directory\nwith fog.json, --all applies to every running instance.\n\nRun a script from fog.json:\n  fog <script> [OPTIONS]  (e.g. fog dev)"
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

    /// Only used with `fog logs`: print the captured output of this service
    /// instead of listing services. Without it, `fog logs` lists the
    /// available service names and their status.
    #[arg(short, long, value_name = "SERVICE")]
    service: Option<String>,

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
    let worktrees = fog::worktree::list(&cwd).unwrap_or_else(|| {
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
    match fog::config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Loads config for `fog index restart` — best-effort, no exit on failure.
fn load_runtime_config_for_index() -> fog::config::Config {
    fog::index::load_runtime_config()
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
fn reuse_names(script: &fog::config::ScriptConfig) -> Vec<String> {
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
                // SAFETY: the fd was dupped for transfer and is owned by us.
                unsafe { libc::close(existing.fd) };
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
    Option<fog::lock::OwnerLock>,
) {
    let attempt_started = fog::lock::now_ms();

    let lock = match fog::lock::OwnerLock::try_acquire(project, script, branch) {
        Ok(fog::lock::AcquireResult::Locked(lock)) => lock,
        Ok(fog::lock::AcquireResult::HeldBy(holder)) => {
            let pid = holder
                .as_ref()
                .map(|h| format!(" (pid {})", h.pid))
                .unwrap_or_default();
            eprintln!(
                "another fog instance{pid} is starting script '{script}' for this project; waiting up to 30s"
            );
            match fog::lock::OwnerLock::acquire_with_timeout(
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

/// Returns a human-readable project name for `fog ls`.
///
/// The instance's project identity is the git common dir (e.g.
/// `/repo/.git`), shared by every worktree; strip the trailing `.git`
/// component so the repo name is shown instead.
fn project_display_name(project: &str) -> String {
    let path = Path::new(project);
    if path.file_name().is_some_and(|n| n == ".git") {
        path.parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.to_string())
    } else {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.to_string())
    }
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
                    .map(|p| project_display_name(&p))
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
                if !fog::process::is_pid_alive(*pid) {
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

fn cmd_kill(pid: Option<u32>, all: bool, cli: &Cli) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        eprintln!("error: no running fog instances");
        std::process::exit(1);
    }

    let targets = resolve_targets(&instances, pid, all, cli, "kill");
    for (target_pid, path) in &targets {
        match ipc::send_kill(path) {
            Ok(()) => println!("sent kill request to fog instance {target_pid}"),
            Err(e) => eprintln!("warning: could not kill instance {target_pid}: {e}"),
        }
    }

    // Give the instance(s) a moment to exit and clean up, then tear down the
    // index server if nothing remains. Best-effort: fire-and-forget.
    if targets.len() == 1 {
        let (target_pid, path) = &targets[0];
        let target_pid = *target_pid;
        if instances.len() == 1 && instances[0].0 == target_pid {
            std::thread::sleep(std::time::Duration::from_millis(500));
            for _ in 0..20 {
                if ipc::query_status(path).is_err() && !path.exists() {
                    break;
                }
                if !fog::process::is_pid_alive(target_pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    } else {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    fog::index::maybe_terminate_if_no_instances(None);
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

        ipc::send_kill(path)?;
        println!("sent kill request to fog instance {target_pid} (script '{script}')");

        // Wait for the old instance to fully exit before relaunching to avoid
        // port conflicts and owner-lock races.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if !fog::process::is_pid_alive(target_pid) && ipc::query_status(path).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
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
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
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
            let worktrees = fog::worktree::list(&cwd)?;
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
fn query_statuses(instances: &[(u32, PathBuf)]) -> std::collections::HashMap<u32, ipc::StatusResponse> {
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
            return Err(format!("error: --all cannot be used with a PID (got {pid})"));
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
                    msg.push_str(&format!("\n  fog {cmd} --all   (apply to all {n})", n = matched.len()));
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
    match select_targets(
        instances,
        &statuses,
        pid,
        all,
        local.as_deref(),
        cmd,
    ) {
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

/// Prints one captured log file as a single `==== <script> (<name>) ====`
/// section with ANSI escape sequences stripped.
fn print_log_file(script: &str, name: &str, file: &Path) {
    println!("==== {} ({}) ====", script, name);
    match fs::read_to_string(file) {
        Ok(content) => {
            print!("{}", strip_ansi(&content));
            if !content.ends_with('\n') {
                println!();
            }
        }
        Err(e) => eprintln!("error: could not read {}: {e}", file.display()),
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
/// reads `<service>.log`).
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
        match ipc::query_logs(path, "proxy", 10_000) {
            Ok(lines) => {
                println!("==== {} (proxy) ====", status.script);
                for line in lines {
                    println!("{line}");
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
    print_log_file(&status.script, &name, &file);
    Ok(())
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

    // Apply the configured dnsmasq wildcard-DNS routes before the TUI enters
    // raw mode, so sudo can prompt on a normal terminal. Best-effort: failures
    // only warn and never block the run.
    if let Some(dnsmasq) = config.dnsmasq.as_ref() {
        for msg in fog::dnsmasq::ensure(dnsmasq, detached) {
            eprintln!("{msg}");
        }
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
        for msg in fog::router::ensure(router, &domains) {
            eprintln!("{msg}");
        }
    }
    // Standalone index server (service directory + web UI). Controlled by
    // fog config (`~/.config/fog/fog.json` alongside `theme`, plus per-project
    // `fog.json` top-level `index`). Both default true; either can opt-out.
    if config.effective_should_serve_index() {
        for msg in fog::index::ensure_for_config(&config) {
            eprintln!("{msg}");
        }
    }

    let config_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.clone());
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let project =
        fog::project::detect(&config_dir).or_else(|| fog::project::fallback_identity(&config_dir));
    // Branch for instance identity: an explicit `--branch` wins; otherwise it
    // is resolved from the worktree containing the config (so a plain
    // `fog dev` in a worktree gets that worktree's branch).
    let branch = cli
        .branch
        .clone()
        .or_else(|| fog::runtime::resolve_branch(&config_dir));
    let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
    let mut owner_lock: Option<fog::lock::OwnerLock> = None;
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
    ipc::spawn_server(ipc_state.clone())?;

    if !detached {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    }

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
        .or_else(|| fog::runtime::resolve_branch(&config_dir));
    let port_map = if let Some(specs) = &config.ports {
        match fog::ports::allocate_ports(specs) {
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
            fog::ports::validate_native_routes(routes, &port_map, branch_for_ports.as_deref())
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
    if let Err(e) = fog::ports::ensure_ports_defined(
        config.ports.as_ref(),
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
    // Publish allocated ports + native routes to IPC so the index server can synthesize
    // native ApiService entries for the Services UI (which otherwise only sees docker).
    {
        *ipc_state.ports.lock().expect("mutex poisoned") = port_map.clone();
        let routes: Vec<fog::ipc::NativeRouteInfo> = config
            .native_routes
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|r| fog::ipc::NativeRouteInfo {
                host: r.host,
                service: r.service,
                port: r.port,
                path_prefix: r.path_prefix,
            })
            .collect();
        *ipc_state.native_routes.lock().expect("mutex poisoned") = routes;
    }
    // Bring up native Traefik routes for allocated ports (explicit only)
    if let Some(routes) = &config.native_routes {
        for msg in fog::router::ensure_native_routes(
            routes,
            &port_map,
            branch_for_ports.as_deref(),
            &config,
        ) {
            eprintln!("{msg}");
        }
    }

    let runtime = fog::runtime::build_with_ports_no_share(
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
    )
    .map_err(|e| {
        // Restore the terminal before reporting so it is usable again.
        if !detached {
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
        }
        io::Error::new(io::ErrorKind::InvalidData, format!("error: {}", e))
    })?;
    // Log allocated ports for visibility (also useful for `fog logs`)
    if !port_map.is_empty() {
        let mut names: Vec<&String> = port_map.keys().collect();
        names.sort();
        for n in names {
            eprintln!("  + port {} -> {}", n, port_map[n]);
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
    let config_rx = if detached {
        std::sync::mpsc::channel().1
    } else {
        config_watcher::spawn_config_watcher(config_path.clone(), Arc::new(AtomicBool::new(false)))
    };

    let mut app = App::new_with_opts(fog::app::AppCreateOpts {
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
        ipc_state,
        config_rel: cli.config.clone(),
        save_logs: cli.save_logs,
        no_share: cli.no_share,
    });
    if detached {
        app.run_headless()?;
    } else {
        ratatui::run(|terminal| app.run(terminal))?;
    }

    if config.effective_should_serve_index() {
        for msg in fog::index::ensure_for_config(&config) {
            eprintln!("{msg}");
        }
    }

    ipc::cleanup_socket();

    // Clean up native Traefik routes for this branch (explicit only).
    fog::router::cleanup_native_routes(branch_for_ports.as_deref(), &config);

    // If no fog instances remain, tear down the index server as well.
    // Give the socket file a moment to disappear from the filesystem.
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Use the effective index port (project → global fallback) so a custom
    // port is torn down correctly.
    let port = config.effective_index_port();
    fog::index::maybe_terminate_on_port(port);

    Ok(())
}

/// Creates the per-instance log directory for the current process, which
/// every run (interactive or detached) tees each service's output into.
fn create_log_dir() -> io::Result<PathBuf> {
    let dir = ipc::instance_log_dir(std::process::id());
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Redirects this process's stdout/stderr into `daemon.log` inside `dir`,
/// so a detached daemon's own diagnostics are captured too. Only called for
/// detached runs — an interactive run keeps stdout/stderr for the TUI.
fn redirect_daemon_output(dir: &Path) -> io::Result<()> {
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
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
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
    let port = fog::index::resolve_serve_port(explicit_port);
    let network = fog::index::resolve_serve_network();
    if foreground {
        fog::index::serve_foreground(port, network)
    } else {
        fog::index::serve_detached(port, &network)
    }
}

fn main() -> io::Result<()> {
    // `fog index serve/kill/restart` are dispatched before clap so they are not
    // misparsed as the `[PID]` positional (which expects a number).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("index") {
        match argv.get(1).map(String::as_str) {
            Some("serve") => return cmd_index_serve(&argv[2..]),
            Some("kill") => {
                // `fog index kill` terminates the index server unconditionally.
                let killed = fog::index::kill_server(None);
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
                fog::index::kill_for_config(&cfg);
                std::thread::sleep(std::time::Duration::from_millis(300));
                let port = cfg.index_port();
                let network = cfg.index_network();
                for msg in fog::index::ensure_with_port(port, &network) {
                    eprintln!("{msg}");
                }
                if fog::index::is_server_started(port) {
                    println!("index server restarted on :{port}");
                    println!(
                        "  logs: {}",
                        fog::index::index_log_dir(port).join("daemon.log").display()
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
        print!("{}", fog::completion::generate(shell));
        return Ok(());
    }

    // `--service` only applies to `fog logs`; reject it elsewhere before the
    // detach path so it can never leak into a daemon child's arguments.
    if cli.service.is_some() && cli.script.as_deref() != Some("logs") {
        eprintln!("error: --service only applies to `fog logs` (e.g. `fog logs --service api`)");
        std::process::exit(1);
    }

    // `--all` only applies to `fog kill` / `fog restart`, and conflicts with
    // an explicit PID.
    if cli.all {
        match cli.script.as_deref() {
            Some("kill") | Some("restart") => {}
            Some("logs") => {
                eprintln!("error: --all only applies to `fog kill` and `fog restart`; for logs, pass an explicit PID");
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
        let dir = std::env::temp_dir().join(format!(
            "fog-resolve-config-{}-{}",
            std::process::id(),
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

    #[test]
    fn test_project_display_name_strips_git_common_dir() {
        // The project identity is the git common dir (`/repo/.git`); the
        // display name must be the repo directory, not `.git`.
        assert_eq!(project_display_name("/Users/alice/dev/fog/.git"), "fog");
        assert_eq!(project_display_name("/Users/alice/dev/fog"), "fog");
    }

    #[test]
    fn test_project_display_name_fallback() {
        // Fallback identity (non-git) is a plain directory path; the display
        // name is its basename. A bare/rootless path falls back to itself.
        assert_eq!(project_display_name("/tmp/my-project"), "my-project");
        assert_eq!(project_display_name("fog"), "fog");
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

    fn target_fixtures() -> (Vec<(u32, PathBuf)>, std::collections::HashMap<u32, ipc::StatusResponse>) {
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
        let got = select_targets(&instances, &statuses, Some(103), false, Some(&local), "kill").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 103);
    }

    #[test]
    fn test_select_targets_unknown_pid_errors() {
        let (instances, statuses) = target_fixtures();
        let err = select_targets(&instances, &statuses, Some(999), false, None, "kill").unwrap_err();
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
        let err = select_targets(&instances, &statuses, None, true, Some(&local), "kill").unwrap_err();
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
        let err = select_targets(&instances, &statuses, None, false, Some(&local), "kill").unwrap_err();
        assert!(err.contains("multiple fog instances from this config"));
        assert!(err.contains("fog kill 101"));
        assert!(err.contains("fog kill 102"));
        assert!(!err.contains("103"), "scoped error must not list other configs");
        assert!(err.contains("--all"));
    }

    #[test]
    fn test_select_targets_pidless_multi_local_logs_has_no_all_hint() {
        let (instances, statuses) = target_fixtures();
        let local = PathBuf::from(statuses[&101].config_dir.as_deref().unwrap());
        let err = select_targets(&instances, &statuses, None, false, Some(&local), "logs").unwrap_err();
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
        let got = select_targets(&instances, &statuses, None, false, Some(&elsewhere), "kill").unwrap();
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
        let err = select_targets(&instances, &statuses, None, false, Some(&elsewhere), "kill").unwrap_err();
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
        assert!(config_dir_matches(
            Some(&dir.to_string_lossy()),
            &dir
        ));
        assert!(!config_dir_matches(Some("/definitely/not/here"), &dir));
        assert!(!config_dir_matches(None, &dir));
        let _ = fs::remove_dir_all(&dir);
    }
}
