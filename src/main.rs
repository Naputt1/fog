#![deny(unsafe_op_in_unsafe_fn)]

use clap::Parser;
use crossterm::event::EnableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use std::collections::HashMap;
use std::io::stdout;
use std::path::{Path, PathBuf};
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

/// Command-line interface arguments parsed via clap.
#[derive(Parser)]
#[command(name = "fog", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Script to run (e.g. `fog dev`), or a built-in command (`ls`, `kill`).
    script: Option<String>,

    /// PID of a running fog instance (used with `fog kill <pid>`).
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
/// The caller is expected to hold the per-(project, script) owner lock.
fn reclaim_existing(
    project: &str,
    script: &str,
    reuse: &[String],
    timeout: Duration,
) -> HashMap<String, ipc::HandoffItem> {
    let mut adopted = HashMap::new();
    let existing = ipc::find_instances_for(project, script);
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
            adopted.entry(handoff.name.clone()).or_insert(handoff);
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
    reuse: &[String],
) -> (
    HashMap<String, ipc::HandoffItem>,
    Option<fog::lock::OwnerLock>,
) {
    let attempt_started = fog::lock::now_ms();

    let lock = match fog::lock::OwnerLock::try_acquire(project, script) {
        Ok(fog::lock::AcquireResult::Locked(lock)) => lock,
        Ok(fog::lock::AcquireResult::HeldBy(holder)) => {
            let pid = holder
                .as_ref()
                .map(|h| format!(" (pid {})", h.pid))
                .unwrap_or_default();
            eprintln!(
                "another fog instance{pid} is starting script '{script}' for this project; waiting up to 30s"
            );
            match fog::lock::OwnerLock::acquire_with_timeout(project, script, LOCK_WAIT_TIMEOUT) {
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
                        reclaim_existing(project, script, reuse, RECLAIM_WAIT_TIMEOUT),
                        None,
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("  warning: could not lock project: {e}, proceeding without coordination");
            return (
                reclaim_existing(project, script, reuse, RECLAIM_WAIT_TIMEOUT),
                None,
            );
        }
    };

    // We now hold the owner lock. An instance that started after we began our
    // startup is a concurrent starter that beat us and is now serving: back
    // off rather than kill a freshly-started instance.
    let instances = ipc::find_instances_with_status(project, script);
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

    let adopted = reclaim_existing(project, script, reuse, RECLAIM_WAIT_TIMEOUT);
    (adopted, Some(lock))
}

fn cmd_ls() -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        println!("no running fog instances");
        return Ok(());
    }

    let mut rows: Vec<(u32, String, String, String, String)> = Vec::new();
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
                        format!("{}:{}", s.name, state)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let project = status
                    .project
                    .map(|p| {
                        Path::new(&p)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or(p)
                    })
                    .unwrap_or_else(|| "-".to_string());
                rows.push((*pid, status.script, project, proxy, services));
            }
            Err(_) => {
                // Stale socket: remove it and skip.
                let _ = fs::remove_file(path);
            }
        }
    }

    if rows.is_empty() {
        println!("no running fog instances");
        return Ok(());
    }

    let w_pid = rows
        .iter()
        .map(|r| r.0.to_string().len())
        .max()
        .unwrap_or(3);
    let w_script = rows.iter().map(|r| r.1.len()).max().unwrap_or(6);
    let w_project = rows.iter().map(|r| r.2.len()).max().unwrap_or(7);
    let w_proxy = rows.iter().map(|r| r.3.len()).max().unwrap_or(5);

    println!(
        "{:<w_pid$}  {:<w_script$}  {:<w_project$}  {:<w_proxy$}  services",
        "pid", "script", "project", "proxy"
    );
    for (pid, script, project, proxy, services) in rows {
        println!(
            "{:<w_pid$}  {:<w_script$}  {:<w_project$}  {:<w_proxy$}  {}",
            pid, script, project, proxy, services
        );
    }
    Ok(())
}

fn cmd_kill(pid: Option<u32>) -> io::Result<()> {
    let instances = ipc::find_instances()?;

    if instances.is_empty() {
        eprintln!("error: no running fog instances");
        std::process::exit(1);
    }

    let target = match pid {
        Some(pid) => match instances.iter().find(|(p, _)| *p == pid) {
            Some((_, path)) => Some(path),
            None => {
                eprintln!("error: no fog instance with pid {pid}");
                std::process::exit(1);
            }
        },
        None => {
            if instances.len() == 1 {
                Some(&instances[0].1)
            } else {
                eprintln!("error: multiple fog instances running, specify a pid:");
                for (p, _) in &instances {
                    eprintln!("  fog kill {p}");
                }
                std::process::exit(1);
            }
        }
    };

    match target {
        Some(path) => {
            ipc::send_kill(path)?;
            println!("sent kill request to fog instance");
        }
        None => {
            eprintln!("error: no fog instance to kill");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn run_script(name: &str, cli: &Cli) -> io::Result<()> {
    let config_path = resolve_config_path(&resolve_run_config(cli));
    let config = load_config(&config_path);
    let script = match config.scripts.get(name) {
        Some(s) => s,
        None => list_scripts_and_exit(&config, &format!("error: unknown script '{}'", name)),
    };

    let config_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.clone());
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let project =
        fog::project::detect(&config_dir).or_else(|| fog::project::fallback_identity(&config_dir));
    let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
    let mut owner_lock: Option<fog::lock::OwnerLock> = None;
    if let Some(ref project) = project {
        let reuse = reuse_names(script);
        (adopted, owner_lock) = reconcile_instance(project, name, &reuse);
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

    let ipc_state = Arc::new(ipc::IpcState::new(name.to_string(), project));
    ipc::spawn_server(ipc_state.clone())?;

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;

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
    let theme = Theme::from_config(config.theme.as_ref());

    let runtime = fog::runtime::build(script, &config_dir, cli.save_logs, scrollback, &mut adopted);

    // Services are up: release the owner lock so a later worktree switch can
    // replace this instance.
    drop(owner_lock);

    let config_rx = config_watcher::spawn_config_watcher(config_path.clone());

    ratatui::run(|terminal| {
        App::new(
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
        )
        .run(terminal)
    })?;

    ipc::cleanup_socket();

    Ok(())
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        print!("{}", fog::completion::generate(shell));
        return Ok(());
    }

    match cli.script.as_deref() {
        Some("ls") => cmd_ls(),
        Some("kill") => cmd_kill(cli.pid),
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
}
