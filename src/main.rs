#![deny(unsafe_op_in_unsafe_fn)]

use clap::Parser;
use crossterm::event::EnableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use std::collections::HashMap;
use std::io::stdout;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{fs, io};

use fog::app::{App, PendingService};
use fog::config::{Config, HealthCheckConfig, HealthCheckSpec};
use fog::config_watcher;
use fog::ipc;
use fog::proxy::{ProxyInstance, RouteEntry};
use fog::terminal::Terminal;
use fog::theme::Theme;

const DEFAULT_SCROLLBACK: usize = 2000;

/// Command-line interface arguments parsed via clap.
#[derive(Parser)]
#[command(name = "fog", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Script to run (e.g. `fog dev`), or a built-in command (`ls`, `kill`).
    script: Option<String>,

    /// PID of a running fog instance (used with `fog kill <pid>`).
    pid: Option<u32>,

    /// Path to the configuration file. Defaults to `fog.json`.
    #[arg(short, long, default_value = "fog.json")]
    config: std::path::PathBuf,

    /// Save service output to `temp/<name>.txt` on exit.
    #[arg(long, help = "Save service output to temp/<name>.txt on exit")]
    save_logs: bool,
}

/// Resolves service startup order using topological sort (Kahn's algorithm).
/// Returns indices into `entries` in dependency order, or an error if there
/// is a cycle or a dependency references an unknown service name.
fn resolve_dep_order(entries: &[fog::config::ConfigEntry]) -> Result<Vec<usize>, String> {
    let name_to_idx: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let name = e.name.clone().unwrap_or_else(|| {
                std::path::Path::new(&e.path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            // Use leak to get a &'static str from the owned String — safe because
            // entries live for the rest of the program and we only need the map
            // during this function.
            let leaked: &'static str = Box::leak(name.into_boxed_str());
            (leaked, i)
        })
        .collect();

    for entry in entries {
        if let Some(deps) = &entry.depends_on {
            for dep in deps {
                if !name_to_idx.contains_key(dep.as_str()) {
                    let entry_name = entry.name.as_deref().unwrap_or_else(|| {
                        std::path::Path::new(&entry.path)
                            .file_name()
                            .unwrap_or_default()
                            .to_str()
                            .unwrap_or("?")
                    });
                    return Err(format!(
                        "service '{}' depends on unknown service '{}'",
                        entry_name, dep
                    ));
                }
            }
        }
    }

    let n = entries.len();
    let mut in_degree = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, entry) in entries.iter().enumerate() {
        if let Some(deps) = &entry.depends_on {
            for dep in deps {
                let dep_idx = name_to_idx[dep.as_str()];
                adj[dep_idx].push(i);
                in_degree[i] += 1;
            }
        }
    }

    let mut queue: Vec<usize> = (0..n).filter(|i| in_degree[*i] == 0).collect();
    let mut order = Vec::new();

    while let Some(idx) = queue.pop() {
        order.push(idx);
        for &next in &adj[idx] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                queue.push(next);
            }
        }
    }

    if order.len() != n {
        return Err("circular dependency detected between services".to_string());
    }

    Ok(order)
}

/// Loads and parses the config file, exiting with a diagnostic on failure.
fn load_config(path: &Path) -> Config {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not read config '{}': {}", path.display(), e);
            std::process::exit(1);
        }
    };

    match serde_json::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: invalid config '{}': {}", path.display(), e);
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
        let handoffs = match ipc::reclaim(path, reuse) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("  warning: could not reach instance {pid}: {e}, continuing");
                continue;
            }
        };
        if handoffs.is_empty() {
            eprintln!("  old instance {pid} has no live services to reuse");
        } else {
            let names: Vec<&str> = handoffs.iter().map(|h| h.name.as_str()).collect();
            eprintln!("  reusing live services: {}", names.join(", "));
        }
        for handoff in handoffs {
            adopted.entry(handoff.name.clone()).or_insert(handoff);
        }
        if ipc::wait_for_exit(*pid, timeout) {
            eprintln!("  old instance {pid} stopped");
        } else {
            eprintln!("  warning: instance {pid} did not stop within the timeout");
        }
    }
    adopted
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
    let config = load_config(&cli.config);
    let script = match config.scripts.get(name) {
        Some(s) => s,
        None => list_scripts_and_exit(&config, &format!("error: unknown script '{}'", name)),
    };

    let config_path = cli
        .config
        .canonicalize()
        .unwrap_or_else(|_| cli.config.clone());
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let project =
        fog::project::detect(&config_dir).or_else(|| fog::project::fallback_identity(&config_dir));
    let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
    if let Some(ref project) = project {
        let reuse = reuse_names(script);
        adopted = reclaim_existing(project, name, &reuse, Duration::from_secs(15));
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

    let entries = script.service.clone().unwrap_or_default();
    let dep_order = resolve_dep_order(&entries).unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1);
    });

    let n = entries.len();
    let mut items: Vec<Option<Terminal>> = (0..n).map(|_| None).collect();
    let mut pending_services: Vec<PendingService> = Vec::new();

    for &idx in &dep_order {
        let entry = &entries[idx];
        let service_path = config_dir.join(&entry.path);
        let name = entry.name.clone().unwrap_or_else(|| {
            service_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        let service_path_str = service_path.to_string_lossy().into_owned();

        let health_checks: Vec<HealthCheckConfig> = match &entry.health_check {
            Some(HealthCheckSpec::Single(c)) => vec![c.clone()],
            Some(HealthCheckSpec::Multiple(v)) => v.clone(),
            None => vec![],
        };

        let has_deps = entry.depends_on.is_some();

        let terminal = if entry.reuse {
            if health_checks.is_empty() {
                eprintln!(
                    "warning: service '{}' has reuse: true but no health_check; \
                     fog cannot verify it is already running",
                    name
                );
            }
            let mut t = if let Some(handoff) = adopted.remove(&name) {
                Terminal::adopt(
                    service_path_str.clone(),
                    entry.cmd.clone(),
                    name.clone(),
                    scrollback,
                    handoff.fd,
                    handoff.pid,
                    handoff.scrollback,
                )
            } else {
                Terminal::spawn_reused(
                    name.clone(),
                    service_path_str,
                    entry.cmd.clone(),
                    scrollback,
                )
            };
            t.save_logs = cli.save_logs;
            t.health_checks = health_checks;
            t.shutdown_cmd = entry.shutdown_cmd.clone();
            t.start_health_checks();
            t
        } else if has_deps {
            let deps = entry.depends_on.clone().unwrap_or_default();
            let mut t = Terminal::spawn_pending(name.clone(), scrollback, &deps);
            t.save_logs = cli.save_logs;
            pending_services.push(PendingService {
                name: name.clone(),
                cmd: entry.cmd.clone(),
                path: service_path_str,
                scrollback,
                save_logs: cli.save_logs,
                dep_names: deps,
                health_checks,
                shutdown_cmd: entry.shutdown_cmd.clone(),
                tab_index: idx,
            });
            t
        } else {
            let shutdown_cmd = entry.shutdown_cmd.clone();
            match Terminal::spawn_command(&service_path_str, &entry.cmd, name.clone(), scrollback) {
                Ok(mut t) => {
                    t.save_logs = cli.save_logs;
                    t.health_checks = health_checks;
                    t.shutdown_cmd = shutdown_cmd;
                    t.start_health_checks();
                    t
                }
                Err(e) => {
                    Terminal::spawn_error(name.clone(), format!("Failed to spawn: {e}"), scrollback)
                }
            }
        };
        items[idx] = Some(terminal);
    }

    let items: Vec<Terminal> = items
        .into_iter()
        .map(|t| t.expect("all items should be filled"))
        .collect();

    let proxy = script.proxy.clone().map(|pc| {
        let routes: Vec<RouteEntry> = pc
            .routes
            .into_iter()
            .map(|r| RouteEntry {
                path: r.path,
                host: r.host,
                upstream: r.upstream,
                ws: r.ws.unwrap_or(false),
            })
            .collect();
        let max_log_entries = pc.max_log_entries.unwrap_or(1000);
        let mut p = ProxyInstance::new(
            pc.port,
            pc.host,
            routes,
            max_log_entries,
            pc.tls_cert,
            pc.tls_key,
        );
        p.start();
        p
    });

    let config_rx = config_watcher::spawn_config_watcher(config_path.clone());

    ratatui::run(|terminal| {
        App::new(
            items,
            pending_services,
            proxy,
            sigint,
            scrollback,
            sidebar_min,
            sidebar_max,
            theme,
            config_path,
            config_rx,
            ipc_state,
        )
        .run(terminal)
    })?;

    ipc::cleanup_socket();

    Ok(())
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    match cli.script.as_deref() {
        Some("ls") => cmd_ls(),
        Some("kill") => cmd_kill(cli.pid),
        Some(name) => run_script(name, &cli),
        None => {
            let config = load_config(&cli.config);
            if config.scripts.is_empty() {
                eprintln!("error: no scripts defined in '{}'", cli.config.display());
                std::process::exit(1);
            }
            list_scripts_and_exit(&config, "error: no script specified")
        }
    }
}
