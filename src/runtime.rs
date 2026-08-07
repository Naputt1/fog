use crate::app::PendingService;
use crate::config::{ConfigEntry, HealthCheckConfig, HealthCheckSpec, ScriptConfig};
use crate::ipc::HandoffItem;
use crate::proxy::{ProxyInstance, RouteEntry};
use crate::terminal::Terminal;
use std::collections::HashMap;
use std::path::Path;

/// The runtime state of a script: its service terminals and optional proxy.
pub struct Runtime {
    pub items: Vec<Terminal>,
    pub pending_services: Vec<PendingService>,
    pub proxy: Option<ProxyInstance>,
}

/// Resolves service startup order using topological sort (Kahn's algorithm).
/// Returns indices into `entries` in dependency order, or an error if there
/// is a cycle or a dependency references an unknown service name.
pub fn resolve_dep_order(entries: &[ConfigEntry]) -> Result<Vec<usize>, String> {
    let name_to_idx: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let name = e.name.clone().unwrap_or_else(|| {
                Path::new(&e.path)
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
                        Path::new(&entry.path)
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

/// Lexically normalizes an absolute path, dropping `.` segments, resolving
/// `..` segments, and removing the doubled `./` produced by `join` (so the
/// `cd` command reads `/repo/infra` instead of `/repo/./infra`).
pub fn normalize_service_path(path: &Path) -> String {
    use std::path::Component;
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::RootDir => parts.clear(),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(c) => parts.push(c.to_string_lossy().into_owned()),
            Component::Prefix(_) => {}
        }
    }
    format!("/{}", parts.join("/"))
}

/// Spawns the terminals and (optional) proxy for a script, honoring dependency
/// ordering and adopting any live services handed over in `adopted` (keyed by
/// service name). Any unconsumed handoffs have their fds closed.
///
/// This is shared by CLI startup and in-place worktree switches.
pub fn build(
    script: &ScriptConfig,
    config_dir: &Path,
    save_logs: bool,
    scrollback: usize,
    adopted: &mut HashMap<String, HandoffItem>,
) -> Runtime {
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
        let service_path_str = normalize_service_path(&service_path);

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
                )
            } else {
                Terminal::spawn_reused(
                    name.clone(),
                    service_path_str,
                    entry.cmd.clone(),
                    scrollback,
                )
            };
            t.save_logs = save_logs;
            t.health_checks = health_checks;
            t.shutdown_cmd = entry.shutdown_cmd.clone();
            t.start_health_checks();
            t
        } else if has_deps {
            let deps = entry.depends_on.clone().unwrap_or_default();
            let mut t = Terminal::spawn_pending(name.clone(), scrollback, &deps);
            t.save_logs = save_logs;
            pending_services.push(PendingService {
                name: name.clone(),
                cmd: entry.cmd.clone(),
                path: service_path_str,
                scrollback,
                save_logs,
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
                    t.save_logs = save_logs;
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

    // Close any handoffs whose service is not present in this script's config.
    for (_, handoff) in adopted.drain() {
        // SAFETY: the fd was dupped for transfer and is owned by us.
        unsafe {
            libc::close(handoff.fd);
        }
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

    Runtime {
        items,
        pending_services,
        proxy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, deps: Option<Vec<&str>>) -> ConfigEntry {
        ConfigEntry {
            name: Some(name.to_string()),
            path: ".".to_string(),
            cmd: "true".to_string(),
            health_check: None,
            depends_on: deps.map(|d| d.into_iter().map(String::from).collect()),
            shutdown_cmd: None,
            reuse: false,
        }
    }

    #[test]
    fn test_resolve_dep_order_simple() {
        let entries = vec![entry("a", None), entry("b", Some(vec!["a"]))];
        let order = resolve_dep_order(&entries).unwrap();
        assert_eq!(order, vec![0, 1], "dependency must run first");
    }

    #[test]
    fn test_resolve_dep_order_cycle() {
        let entries = vec![entry("a", Some(vec!["b"])), entry("b", Some(vec!["a"]))];
        assert!(resolve_dep_order(&entries).is_err());
    }

    #[test]
    fn test_resolve_dep_order_unknown_dep() {
        let entries = vec![entry("a", Some(vec!["nope"]))];
        assert!(resolve_dep_order(&entries).is_err());
    }

    #[test]
    fn test_normalize_service_path_strips_dot_segments() {
        assert_eq!(
            normalize_service_path(Path::new("/Users/naputt/git/GEMS/./infra")),
            "/Users/naputt/git/GEMS/infra"
        );
        assert_eq!(
            normalize_service_path(Path::new("/Users/naputt/git/GEMS/.")),
            "/Users/naputt/git/GEMS"
        );
    }

    #[test]
    fn test_normalize_service_path_resolves_parent() {
        assert_eq!(
            normalize_service_path(Path::new("/repo/fog/../cinema_ticket/backend")),
            "/repo/cinema_ticket/backend"
        );
    }

    #[test]
    fn test_normalize_service_path_preserves_absolute() {
        assert_eq!(normalize_service_path(Path::new("/etc")), "/etc");
    }
}
