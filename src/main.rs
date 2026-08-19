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
#[command(name = "fog", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Script to run (e.g. `fog dev`), or a built-in command (`ls`, `kill`, `logs`).
    script: Option<String>,

    /// PID of a running fog instance (used with `fog kill <pid>`, `fog logs <pid>`).
    pid: Option<u32>,

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

fn cmd_kill(pid: Option<u32>) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        eprintln!("error: no running fog instances");
        std::process::exit(1);
    }

    let (target_pid, path) = resolve_instance(&instances, pid, "kill");
    ipc::send_kill(path)?;
    println!("sent kill request to fog instance");

    // Give the instance a moment to exit and clean up its socket, then
    // tear down the index server if it was the last instance.
    // This is best-effort: `fog kill` is fire-and-forget, so we don't
    // block long enough to guarantee the instance is gone.
    let is_last = instances.len() == 1 && instances[0].0 == target_pid;
    if is_last {
        std::thread::sleep(std::time::Duration::from_millis(500));
        // Spin a short wait for the target's socket to become unreachable
        // (up to 2s) before deciding to kill the server.
        for _ in 0..20 {
            if ipc::query_status(path).is_err() && !path.exists() {
                break;
            }
            if !fog::process::is_pid_alive(target_pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        fog::index::maybe_terminate_if_no_instances(None);
    } else if instances.len() > 1 {
        // Multiple instances were running; the killed one may have been the
        // last one that made the server idle (e.g. stale sockets). Do a
        // quick idle check after a short delay without blocking the CLI long.
        std::thread::sleep(std::time::Duration::from_millis(500));
        fog::index::maybe_terminate_if_no_instances(None);
    }
    Ok(())
}

fn cmd_restart(pid: Option<u32>, cli: &Cli) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        eprintln!("error: no running fog instances");
        std::process::exit(1);
    }

    let (target_pid, path) = resolve_instance(&instances, pid, "restart");
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

/// Resolves the instance `pid` refers to, returning its PID and socket path.
///
/// With no `pid`, the single running instance is chosen; multiple instances
/// produce an error listing each (`cmd` names the command in the hint, e.g.
/// `fog kill <pid>`). Exits with an error when nothing matches.
fn resolve_instance<'a>(
    instances: &'a [(u32, PathBuf)],
    pid: Option<u32>,
    cmd: &str,
) -> (u32, &'a PathBuf) {
    match pid {
        Some(pid) => match instances.iter().find(|(p, _)| *p == pid) {
            Some((_, path)) => (pid, path),
            None => {
                eprintln!("error: no fog instance with pid {pid}");
                std::process::exit(1);
            }
        },
        None => {
            if instances.len() == 1 {
                (instances[0].0, &instances[0].1)
            } else {
                eprintln!("error: multiple fog instances running, specify a pid:");
                for (p, _) in instances {
                    eprintln!("  fog {cmd} {p}");
                }
                std::process::exit(1);
            }
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

/// Prints the captured logs of a running instance, one section per service.
fn cmd_logs(pid: Option<u32>) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        eprintln!("error: no running fog instances");
        std::process::exit(1);
    }

    let (target_pid, path) = resolve_instance(&instances, pid, "logs");

    let status = match ipc::query_status(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not query instance {target_pid}: {e}");
            std::process::exit(1);
        }
    };

    let dir = ipc::instance_log_dir(target_pid);
    if !dir.is_dir() {
        eprintln!("error: instance {target_pid} has no captured logs");
        std::process::exit(1);
    }

    let mut files: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "log"))
        .collect();
    files.sort();

    if files.is_empty() {
        println!("(no log files for instance {target_pid})");
        return Ok(());
    }

    for file in files {
        let name = file
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!("==== {} ({}) ====", status.script, name);
        match fs::read_to_string(&file) {
            Ok(content) => {
                print!("{}", strip_ansi(&content));
                if !content.ends_with('\n') {
                    println!();
                }
            }
            Err(e) => eprintln!("error: could not read {}: {e}", file.display()),
        }
    }
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
        // run (handing over `reuse` services).
        if !script.concurrent {
            let reuse = reuse_names(script);
            (adopted, owner_lock) = reconcile_instance(project, name, branch.as_deref(), &reuse);
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
    let mut ipc_state = ipc::IpcState::new(name.to_string(), project.clone(), branch.clone());
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

    let runtime = fog::runtime::build(
        script,
        name,
        &config_dir,
        project.clone(),
        cli.save_logs,
        scrollback,
        log_dir.clone(),
        &mut adopted,
    )
    .map_err(|e| {
        // Restore the terminal before reporting so it is usable again.
        if !detached {
            let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
        }
        io::Error::new(io::ErrorKind::InvalidData, format!("error: {}", e))
    })?;

    // Expose the proxy's live request log to the IPC server so the web log
    // viewer (and anything else) can stream it. The handle is stable across
    // config hot-reloads, so wiring it once here is enough.
    if let Some(proxy) = runtime.proxy.as_ref() {
        *ipc_state
            .proxy_logs
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(proxy.logs_handle());
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

    let mut app = App::new(
        runtime.items,
        runtime.pending_services,
        runtime.proxy,
        sigint,
        scrollback,
        sidebar_min,
        sidebar_max,
        theme,
        config_path,
        config_rx,
        ipc_state,
        cli.config.clone(),
        cli.save_logs,
    );
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

fn main() -> io::Result<()> {
    // `fog index serve/kill/restart` are dispatched before clap so they are not
    // misparsed as the `[PID]` positional (which expects a number).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("index") {
        match argv.get(1).map(String::as_str) {
            Some("serve") => return fog::index::serve(),
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

    // Detach: run the script in the background and return once it is serving.
    // The daemon child (re-executed with FOG_DAEMON_CHILD=1) takes the
    // headless path in run_script and must not re-daemonize.
    if cli.detach && std::env::var_os("FOG_DAEMON_CHILD").is_none() {
        match cli.script.as_deref() {
            Some(name) if !matches!(name, "ls" | "kill" | "logs") => return daemonize(name),
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
        Some("kill") => cmd_kill(cli.pid),
        Some("restart") => cmd_restart(cli.pid, &cli),
        Some("logs") => cmd_logs(cli.pid),
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
}
