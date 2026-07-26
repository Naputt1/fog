#![deny(unsafe_op_in_unsafe_fn)]

use clap::Parser;
use crossterm::event::EnableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use std::io::stdout;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{fs, io};

use fog::app::{App, PendingService};
use fog::config::{Config, HealthCheckConfig, HealthCheckSpec};
use fog::config_watcher;
use fog::proxy::{ProxyInstance, RouteEntry};
use fog::terminal::Terminal;
use fog::theme::Theme;

const DEFAULT_SCROLLBACK: usize = 2000;

/// Command-line interface arguments parsed via clap.
#[derive(Parser)]
#[command(name = "fog", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
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
                    let entry_name = entry
                        .name
                        .as_deref()
                        .unwrap_or_else(|| {
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

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let contents = match fs::read_to_string(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "error: could not read config '{}': {}",
                cli.config.display(),
                e
            );
            std::process::exit(1);
        }
    };

    let config: Config = match serde_json::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: invalid config '{}': {}", cli.config.display(), e);
            std::process::exit(1);
        }
    };

    let config_path = cli
        .config
        .canonicalize()
        .unwrap_or_else(|_| cli.config.clone());
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let sigint = Arc::new(AtomicBool::new(false));
    let sig = sigint.clone();
    if ctrlc::set_handler(move || {
        sig.store(true, Ordering::SeqCst);
    })
    .is_err()
    {
        eprintln!("warning: could not set Ctrl+C handler");
    }

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

    let entries = config.service.unwrap_or_default();
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

        let terminal = if has_deps {
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
                    Terminal::spawn_error(
                        name.clone(),
                        format!("Failed to spawn: {e}"),
                        scrollback,
                    )
                }
            }
        };
        items[idx] = Some(terminal);
    }

    let items: Vec<Terminal> = items.into_iter().map(|t| t.expect("all items should be filled")).collect();

    let proxy = config.proxy.map(|pc| {
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
        )
        .run(terminal)
    })?;

    Ok(())
}
