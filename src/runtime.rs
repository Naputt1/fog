use crate::app::PendingService;
use crate::config::{ConfigEntry, HealthCheckConfig, HealthCheckSpec, ScriptConfig};
use crate::ipc::HandoffItem;
use crate::proxy::{ProxyInstance, RouteEntry};
use crate::terminal::{Terminal, health_checks_pass};
use std::collections::HashMap;
use std::path::Path;

/// The runtime state of a script: its service terminals and optional proxy.
pub struct Runtime {
    pub items: Vec<Terminal>,
    pub pending_services: Vec<PendingService>,
    pub proxy: Option<ProxyInstance>,
}

/// Options for building a runtime with explicit ports and branch.
/// Bundles the many parameters of `build_with_ports` to avoid
/// `clippy::too_many_arguments`.
pub struct BuildOpts<'a> {
    pub script: &'a ScriptConfig,
    pub script_name: &'a str,
    pub config_dir: &'a Path,
    pub project: Option<String>,
    pub save_logs: bool,
    pub scrollback: usize,
    pub log_dir: Option<std::path::PathBuf>,
    pub adopted: &'a mut HashMap<String, HandoffItem>,
    pub ports: &'a crate::ports::PortMap,
    pub branch_override: Option<String>,
    pub no_share: bool,
}

/// Options for spawning a checked terminal.
pub struct TerminalSpawnOpts {
    pub path: String,
    pub cmd: String,
    pub name: String,
    pub scrollback: usize,
    pub save_logs: bool,
    pub log_dir: Option<std::path::PathBuf>,
    pub health_checks: Vec<HealthCheckConfig>,
    pub shutdown_cmd: Option<String>,
    pub branch: Option<String>,
    pub project: Option<String>,
    pub script: String,
    pub injected_env: HashMap<String, String>,
}

/// Resolves service startup order using topological sort (Kahn's algorithm).
/// Returns indices into `entries` in dependency order, or an error if there
/// is a cycle or a dependency references an unknown service name.
pub fn resolve_dep_order(entries: &[ConfigEntry]) -> Result<Vec<usize>, String> {
    let name_to_idx: HashMap<String, usize> = entries
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
            (name, i)
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

/// Resolves the git branch of the worktree containing `config_dir`, if any.
/// Returns `None` when the directory is not inside a git worktree (or the
/// worktree is detached).
pub fn resolve_branch(config_dir: &Path) -> Option<String> {
    let worktrees = crate::worktree::list(config_dir)?;
    crate::worktree::containing_worktree(&worktrees, config_dir).and_then(|w| w.branch.clone())
}

/// Resolves the `compose_file` of any `docker`-kind health checks against the
/// service's working directory, so the background check thread can locate the
/// compose file without any working-directory context. Non-docker checks (and
/// docker checks without an explicit `compose_file`) are left unchanged.
fn resolve_docker_compose_paths(
    checks: Vec<HealthCheckConfig>,
    service_path: &Path,
) -> Vec<HealthCheckConfig> {
    checks
        .into_iter()
        .map(|mut c| {
            if matches!(c.kind, crate::config::HealthCheckKind::Docker)
                && let Some(file) = c.compose_file.take()
            {
                let absolute = Path::new(&file);
                let absolute = if absolute.is_absolute() {
                    absolute.to_path_buf()
                } else {
                    service_path.join(&file)
                };
                c.compose_file = Some(normalize_service_path(&absolute));
            }
            c
        })
        .collect()
}

/// Computes a service entry's aggregate health checks: its own plus those of
/// its declared endpoints, with any `docker`-kind `compose_file` resolved
/// absolute (the background check thread has no working-directory context).
///
/// Endpoints carry their own checks. Folding them into the service's aggregate
/// lets `depends_on` gating and `share`/`reuse` probing account for every
/// exposed endpoint, while each endpoint keeps its own status thread for
/// display. Duplicate checks are collapsed.
fn entry_health_checks(entry: &ConfigEntry, service_path: &Path) -> Vec<HealthCheckConfig> {
    let mut checks: Vec<HealthCheckConfig> = match &entry.health_check {
        Some(HealthCheckSpec::Single(c)) => vec![c.clone()],
        Some(HealthCheckSpec::Multiple(v)) => v.clone(),
        None => vec![],
    };
    for endpoint in resolve_endpoint_compose_paths(entry, service_path) {
        for check in crate::terminal::endpoint_health_checks(&endpoint.health_check) {
            if !checks.contains(&check) {
                checks.push(check);
            }
        }
    }
    resolve_docker_compose_paths(checks, service_path)
}

/// Adopts the live ports of an already-running sibling for any `share` (or
/// `reuse`) service this instance is about to borrow.
///
/// `ports` is allocated per instance, so without this a borrower hands its
/// dependents a fresh random port while the borrowed container keeps listening
/// on the owner's port — e.g. an API's `DATABASE_URL` would point at the wrong
/// Postgres port. Called before templates are resolved so `${ports.*}` in
/// dependent services resolves to the borrowed resource's real port.
///
/// Only same-branch siblings are considered (shared resources are
/// branch-scoped, e.g. `red-fox-infra-${FOG_BRANCH}`).
pub fn adopt_shared_ports(
    script: &ScriptConfig,
    script_name: &str,
    config_dir: &Path,
    project: Option<&str>,
    branch: Option<&str>,
    ports: &mut crate::ports::PortMap,
    no_share: bool,
) {
    if no_share {
        return;
    }
    let Some(project) = project else {
        return;
    };
    let siblings = crate::ipc::find_instances_with_status(project, script_name, branch);
    // Mirror the borrow decision: a sibling started with --no-share owns the
    // resource without yielding it, so nothing is borrowed (and nothing is
    // adopted) while one is alive.
    if siblings.is_empty() || siblings.iter().any(|(_, _, s)| s.no_share) {
        return;
    }
    let raw = script.service.clone().unwrap_or_default();
    for entry in &raw {
        let shared = if script.concurrent {
            entry.share
        } else {
            entry.reuse
        };
        if !shared {
            continue;
        }
        let service_path = config_dir.join(&entry.path);
        let resolved = match resolve_service_templates(entry, ports, branch) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let health_checks = entry_health_checks(&resolved, &service_path);
        if health_checks.is_empty() || !health_checks_pass(&health_checks, branch) {
            continue;
        }
        // Scan the *raw* entry: `resolved` has already substituted the
        // templates, so it no longer names the ports to adopt.
        let names = crate::ports::entry_referenced_ports(entry);
        if names.is_empty() {
            continue;
        }
        for (_, _, status) in &siblings {
            if names.iter().all(|n| status.ports.contains_key(n)) {
                for name in &names {
                    if let Some(port) = status.ports.get(name) {
                        ports.insert(name.clone(), *port);
                    }
                }
                break;
            }
        }
    }
}

/// Spawns a terminal that runs `cmd` and wires up its health checks and
/// shutdown command. Used for services that should be started directly —
/// non-reused services, and reused services whose resource is currently down.
#[allow(clippy::too_many_arguments)]
fn spawn_checked_terminal(
    path: &str,
    cmd: &str,
    name: &str,
    scrollback: usize,
    save_logs: bool,
    log_dir: Option<std::path::PathBuf>,
    health_checks: Vec<HealthCheckConfig>,
    shutdown_cmd: Option<String>,
    branch: Option<String>,
    project: Option<String>,
    script: &str,
    injected_env: HashMap<String, String>,
) -> Terminal {
    spawn_checked_terminal_with_opts(TerminalSpawnOpts {
        path: path.to_string(),
        cmd: cmd.to_string(),
        name: name.to_string(),
        scrollback,
        save_logs,
        log_dir,
        health_checks,
        shutdown_cmd,
        branch,
        project,
        script: script.to_string(),
        injected_env,
    })
}

/// Preferred version using `TerminalSpawnOpts` to avoid `clippy::too_many_arguments`.
pub fn spawn_checked_terminal_with_opts(opts: TerminalSpawnOpts) -> Terminal {
    match Terminal::spawn_command(
        &opts.path,
        &opts.cmd,
        opts.name.clone(),
        opts.scrollback,
        opts.log_dir,
        opts.branch,
        opts.injected_env,
    ) {
        Ok(mut t) => {
            t.save_logs = opts.save_logs;
            t.health_checks = opts.health_checks;
            t.shutdown_cmd = opts.shutdown_cmd;
            t.project = opts.project;
            t.script = opts.script;
            t.start_health_checks();
            t
        }
        Err(e) => {
            Terminal::spawn_error(opts.name, format!("Failed to spawn: {e}"), opts.scrollback)
        }
    }
}

fn resolve_service_templates(
    entry: &ConfigEntry,
    ports: &crate::ports::PortMap,
    branch: Option<&str>,
) -> Result<ConfigEntry, String> {
    let mut e = entry.clone();
    // cmd
    if crate::ports::has_template(&e.cmd) {
        e.cmd = crate::ports::resolve_template(&e.cmd, ports, branch).map_err(|err| {
            format!(
                "service '{}' cmd template error: {}",
                e.name.as_deref().unwrap_or("?"),
                err
            )
        })?;
    }
    if let Some(cmd) = &e.shutdown_cmd
        && crate::ports::has_template(cmd)
    {
        e.shutdown_cmd = Some(crate::ports::resolve_template(cmd, ports, branch).map_err(
            |err| {
                format!(
                    "service '{}' shutdown_cmd template error: {}",
                    e.name.as_deref().unwrap_or("?"),
                    err
                )
            },
        )?);
    }
    if let Some(env) = &e.env {
        let mut resolved = HashMap::new();
        for (k, v) in env {
            let rv = if crate::ports::has_template(v) {
                crate::ports::resolve_template(v, ports, branch).map_err(|err| {
                    format!(
                        "service '{}' env.{} template error: {}",
                        e.name.as_deref().unwrap_or("?"),
                        k,
                        err
                    )
                })?
            } else {
                v.clone()
            };
            resolved.insert(k.clone(), rv);
        }
        e.env = Some(resolved);
    }
    let resolve_one = |c: &HealthCheckConfig| -> Result<HealthCheckConfig, String> {
        let mut nc = c.clone();
        if crate::ports::has_template(&nc.target) {
            nc.target =
                crate::ports::resolve_template(&nc.target, ports, branch).map_err(|err| {
                    format!(
                        "service '{}' health_check.target template error: {}",
                        e.name.as_deref().unwrap_or("?"),
                        err
                    )
                })?;
        }
        if let Some(f) = &nc.compose_file
            && crate::ports::has_template(f)
        {
            nc.compose_file = Some(crate::ports::resolve_template(f, ports, branch).map_err(
                |err| {
                    format!(
                        "service '{}' health_check.compose_file template error: {}",
                        e.name.as_deref().unwrap_or("?"),
                        err
                    )
                },
            )?);
        }
        Ok(nc)
    };
    if let Some(hc) = &e.health_check {
        e.health_check = Some(match hc {
            HealthCheckSpec::Single(c) => HealthCheckSpec::Single(resolve_one(c)?),
            HealthCheckSpec::Multiple(v) => {
                let mut out = Vec::new();
                for c in v {
                    out.push(resolve_one(c)?);
                }
                HealthCheckSpec::Multiple(out)
            }
        });
    }
    if let Some(subs) = &e.endpoint {
        let mut resolved = Vec::with_capacity(subs.len());
        for sub in subs {
            let mut s = sub.clone();
            if let Some(host) = &s.host
                && crate::ports::has_template(host)
            {
                s.host = Some(crate::ports::resolve_template(host, ports, branch).map_err(
                    |err| {
                        format!(
                            "service '{}' endpoint '{}' host template error: {}",
                            e.name.as_deref().unwrap_or("?"),
                            s.name,
                            err
                        )
                    },
                )?);
            }
            if let Some(port) = &s.port
                && crate::ports::has_template(port)
            {
                s.port = Some(crate::ports::resolve_template(port, ports, branch).map_err(
                    |err| {
                        format!(
                            "service '{}' endpoint '{}' port template error: {}",
                            e.name.as_deref().unwrap_or("?"),
                            s.name,
                            err
                        )
                    },
                )?);
            }
            if let Some(prefix) = &s.path_prefix
                && crate::ports::has_template(prefix)
            {
                s.path_prefix = Some(
                    crate::ports::resolve_template(prefix, ports, branch).map_err(|err| {
                        format!(
                            "service '{}' endpoint '{}' path_prefix template error: {}",
                            e.name.as_deref().unwrap_or("?"),
                            s.name,
                            err
                        )
                    })?,
                );
            }
            if let Some(hc) = &s.health_check {
                s.health_check = Some(match hc {
                    HealthCheckSpec::Single(c) => HealthCheckSpec::Single(resolve_one(c)?),
                    HealthCheckSpec::Multiple(v) => {
                        let mut out = Vec::new();
                        for c in v {
                            out.push(resolve_one(c)?);
                        }
                        HealthCheckSpec::Multiple(out)
                    }
                });
            }
            resolved.push(s);
        }
        e.endpoint = Some(resolved);
    }
    Ok(e)
}

/// Resolves each endpoint's `docker`-kind health checks against the service
/// directory (so `compose_file` is absolute for the background check thread),
/// returning the endpoints with their checks rewritten in place.
fn resolve_endpoint_compose_paths(
    entry: &ConfigEntry,
    service_path: &Path,
) -> Vec<crate::config::EndpointConfig> {
    entry
        .endpoint
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|mut e| {
            if let Some(hc) = e.health_check.take() {
                e.health_check = Some(match hc {
                    HealthCheckSpec::Single(c) => HealthCheckSpec::Single(
                        resolve_docker_compose_paths(vec![c], service_path)
                            .into_iter()
                            .next()
                            .expect("one check in, one check out"),
                    ),
                    HealthCheckSpec::Multiple(v) => {
                        HealthCheckSpec::Multiple(resolve_docker_compose_paths(v, service_path))
                    }
                });
            }
            e
        })
        .collect()
}

/// Flattens every terminal's declared endpoints into tagged native routes.
///
/// Only entries that resolve to both a `host` and a `port` produce a route;
/// the rest are display + health only. `service` is the parent service's
/// display name and `endpoint` is the endpoint name, so route generation
/// and the index server can attribute each route back to its endpoint.
pub fn endpoint_routes(items: &[Terminal]) -> Vec<crate::config::NativeRouteConfig> {
    let mut routes = Vec::new();
    for item in items {
        for sub in &item.endpoints {
            let (Some(host), Some(port)) = (sub.config.host.clone(), sub.config.port.clone())
            else {
                continue;
            };
            routes.push(crate::config::NativeRouteConfig {
                host,
                service: item.name.clone(),
                port,
                path_prefix: sub.config.path_prefix.clone(),
                endpoint: Some(sub.config.name.clone()),
            });
        }
    }
    routes
}

/// Builds the IPC route-info list for `items`' declared endpoints.
///
/// Unlike [`endpoint_routes`] this includes *every* declared endpoint,
/// even those without a route (`host`/`port` empty) — the index server uses it
/// to nest and health-label all of them, and to suppress the matching docker
/// entries. Route generation still uses [`endpoint_routes`].
pub fn endpoint_route_infos(items: &[Terminal]) -> Vec<crate::ipc::NativeRouteInfo> {
    let mut out = Vec::new();
    for item in items {
        for sub in &item.endpoints {
            out.push(crate::ipc::NativeRouteInfo {
                host: sub.config.host.clone().unwrap_or_default(),
                service: item.name.clone(),
                port: sub.config.port.clone().unwrap_or_default(),
                path_prefix: sub.config.path_prefix.clone(),
                endpoint: Some(sub.config.name.clone()),
            });
        }
    }
    out
}

/// Spawns the terminals and (optional) proxy for a script, honoring dependency
/// ordering and adopting any live services handed over in `adopted` (keyed by
/// service name). Any unconsumed handoffs have their fds closed.
///
/// This is shared by CLI startup and in-place worktree switches.
///
/// # Errors
/// Returns an error string when the script's dependency graph is invalid (a
/// cycle or a dependency on an unknown service).
#[allow(clippy::too_many_arguments)]
pub fn build(
    script: &ScriptConfig,
    script_name: &str,
    config_dir: &Path,
    project: Option<String>,
    save_logs: bool,
    scrollback: usize,
    log_dir: Option<std::path::PathBuf>,
    adopted: &mut HashMap<String, HandoffItem>,
) -> Result<Runtime, String> {
    build_with_ports(
        script,
        script_name,
        config_dir,
        project,
        save_logs,
        scrollback,
        log_dir,
        adopted,
        &HashMap::new(),
        None,
    )
}

/// Same as `build` but with explicit `ports` allocation and `branch` override.
///
/// The caller (e.g. `main.rs`) allocates `ports` via `crate::ports::allocate_ports`
/// and resolves templates there; this variant is used when the caller has
/// already resolved branch+ports. When `ports` is empty, no template
/// substitution is performed (backward-compat).
#[allow(clippy::too_many_arguments)]
pub fn build_with_ports(
    script: &ScriptConfig,
    script_name: &str,
    config_dir: &Path,
    project: Option<String>,
    save_logs: bool,
    scrollback: usize,
    log_dir: Option<std::path::PathBuf>,
    adopted: &mut HashMap<String, HandoffItem>,
    ports: &crate::ports::PortMap,
    branch_override: Option<String>,
) -> Result<Runtime, String> {
    build_with_opts(BuildOpts {
        script,
        script_name,
        config_dir,
        project,
        save_logs,
        scrollback,
        log_dir,
        adopted,
        ports,
        branch_override,
        no_share: false,
    })
}

/// Same as `build_with_ports` but with `no_share` flag to ignore shared services.
#[allow(clippy::too_many_arguments)]
pub fn build_with_ports_no_share(
    script: &ScriptConfig,
    script_name: &str,
    config_dir: &Path,
    project: Option<String>,
    save_logs: bool,
    scrollback: usize,
    log_dir: Option<std::path::PathBuf>,
    adopted: &mut HashMap<String, HandoffItem>,
    ports: &crate::ports::PortMap,
    branch_override: Option<String>,
    no_share: bool,
) -> Result<Runtime, String> {
    build_with_opts(BuildOpts {
        script,
        script_name,
        config_dir,
        project,
        save_logs,
        scrollback,
        log_dir,
        adopted,
        ports,
        branch_override,
        no_share,
    })
}

/// Preferred entry point using `BuildOpts` to avoid `clippy::too_many_arguments`.
pub fn build_with_opts(opts: BuildOpts) -> Result<Runtime, String> {
    let BuildOpts {
        script,
        script_name,
        config_dir,
        project,
        save_logs,
        scrollback,
        log_dir,
        adopted,
        ports,
        branch_override,
        no_share,
    } = opts;
    // Resolve branch: override from caller (allocated ports context) wins,
    // otherwise infer from git worktree.
    let branch = branch_override.or_else(|| resolve_branch(config_dir));

    // Validate share + random-port footgun: share services must not use random ports
    // (they would diverge per instance while sharing one backing resource).
    // Since ports map is per-instance, a random allocation would be useless for share.
    // We allow fixed ports only for share services.
    // Detect via has_template on health_check target/cmd? For now, error if share
    // service has any template referencing ports with spec 0 is not directly visible,
    // so we simply forbid share services from using any ${ports.*} template when
    // the corresponding spec was 0 — but ports map already resolved, so we catch
    // by checking if the service had a template and ports contains that key: allow.
    // Simpler: forbid share+template entirely with a warning? Keep as error if
    // share service uses ports template and concurrency true? For v1, allow but
    // document risk; we error only if share service references a port that was
    // allocated as random and health_check uses it — that's actually okay to
    // diverge? We'll keep lenient and just resolve.

    // Clone and template-resolve entries when ports non-empty or branch present.
    let raw_entries = script.service.clone().unwrap_or_default();
    let entries: Vec<ConfigEntry> = if ports.is_empty() && branch.is_none() {
        raw_entries
    } else {
        if ports.is_empty() {
            crate::ports::ensure_ports_defined(None, script, None)?;
        }
        raw_entries
            .iter()
            .map(|e| resolve_service_templates(e, ports, branch.as_deref()))
            .collect::<Result<Vec<_>, _>>()?
    };
    let dep_order = resolve_dep_order(&entries)?;

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

        let health_checks: Vec<HealthCheckConfig> = entry_health_checks(entry, &service_path);
        // Endpoints keep their own status thread for display, so resolve them
        // separately (the aggregate above already folded in their checks).
        let endpoints = resolve_endpoint_compose_paths(entry, &service_path);

        let has_deps = entry.depends_on.is_some();

        // Which flag governs sharing depends on the script's run mode: single
        // instance (concurrent: false) shares via `reuse` (handed over during a
        // reclaim/worktree switch); concurrent mode shares via `share` (borrowed
        // if already up). The other flag is ignored in its mode.
        // `--no-share` disables both mechanisms globally.
        let shared = if no_share {
            false
        } else if script.concurrent {
            entry.share
        } else {
            entry.reuse
        };
        let share_flag = if script.concurrent { "share" } else { "reuse" };

        // Identity metadata carried on every terminal so reuse teardown can ask
        // "are any other instances still serving this (project, script)?".
        let project = project.clone();
        let script_name = script_name.to_string();

        let mut terminal = if shared {
            if let Some(handoff) = adopted.remove(&name) {
                let mut t = Terminal::adopt(
                    service_path_str.clone(),
                    entry.cmd.clone(),
                    name.clone(),
                    scrollback,
                    handoff.fd,
                    handoff.pid,
                    log_dir.clone(),
                );
                t.save_logs = save_logs;
                t.health_checks = health_checks;
                t.shutdown_cmd = entry.shutdown_cmd.clone();
                t.injected_env = entry.env.clone().unwrap_or_default();
                t.branch = branch.clone();
                t.project = project;
                t.script = script_name;
                // Verify the borrowed resource immediately instead of waiting
                // for the periodic thread's first check, so the tab and its
                // dependents learn its true state right away.
                t.probe_health();
                t.start_health_checks();
                t
            } else if health_checks.is_empty() {
                let injected_env = entry.env.clone().unwrap_or_default();
                let t = spawn_checked_terminal(
                    &service_path_str,
                    &entry.cmd,
                    &name,
                    scrollback,
                    save_logs,
                    log_dir.clone(),
                    health_checks,
                    entry.shutdown_cmd.clone(),
                    branch.clone(),
                    project,
                    &script_name,
                    injected_env,
                );
                // build() runs while the TUI is already in raw/alternate-screen
                // mode, so a config warning must render in the tab instead of
                // stderr (which would corrupt the layout).
                t.notice(&format!(
                    "⚠ service '{name}' has {share_flag}: true but no health_check; \
                     fog cannot verify it is already running, starting it\n"
                ));
                t
            } else if health_checks_pass(&health_checks, branch.as_deref()) {
                // Check if any existing instance on this branch has --no-share
                // set; if so, don't borrow from it — start fresh instead. A
                // sibling on another branch owns different (branch-suffixed)
                // resources, so it must not block borrowing here.
                let dominated = project.as_ref().is_some_and(|p| {
                    crate::ipc::find_instances_with_status(p, &script_name, branch.as_deref())
                        .iter()
                        .any(|(_, _, s)| s.no_share)
                });
                if !dominated {
                    // The resource is genuinely up: borrow it instead of re-running
                    // the start command.
                    let mut reused = Terminal::spawn_reused(
                        name.clone(),
                        service_path_str,
                        entry.cmd.clone(),
                        scrollback,
                    );
                    reused.save_logs = save_logs;
                    reused.health_checks = health_checks;
                    reused.shutdown_cmd = entry.shutdown_cmd.clone();
                    reused.injected_env = entry.env.clone().unwrap_or_default();
                    reused.branch = branch.clone();
                    reused.project = project;
                    reused.script = script_name;
                    reused.log_dir = log_dir.clone();
                    reused.persist_borrow_notice();
                    // The probe just passed, so seed Healthy to avoid a short
                    // "stopped" flicker before the background thread's first check.
                    reused.set_health_status(crate::terminal::HealthStatus::Healthy);
                    reused.start_health_checks();
                    reused
                } else {
                    let injected_env = entry.env.clone().unwrap_or_default();
                    spawn_checked_terminal(
                        &service_path_str,
                        &entry.cmd,
                        &name,
                        scrollback,
                        save_logs,
                        log_dir.clone(),
                        health_checks,
                        entry.shutdown_cmd.clone(),
                        branch.clone(),
                        project,
                        &script_name,
                        injected_env,
                    )
                }
            } else {
                // Nothing is running: start the service immediately instead of
                // waiting out the reuse grace period with a misleading
                // "reusing already-running" tab. The tab itself shows the
                // start command output, so no stderr notice is needed (stderr
                // would corrupt the already-active TUI).
                let injected_env = entry.env.clone().unwrap_or_default();
                spawn_checked_terminal(
                    &service_path_str,
                    &entry.cmd,
                    &name,
                    scrollback,
                    save_logs,
                    log_dir.clone(),
                    health_checks,
                    entry.shutdown_cmd.clone(),
                    branch.clone(),
                    project,
                    &script_name,
                    injected_env,
                )
            }
        } else if has_deps {
            let deps = entry.depends_on.clone().unwrap_or_default();
            let mut t = Terminal::spawn_pending(name.clone(), scrollback, &deps);
            t.save_logs = save_logs;
            t.branch = branch.clone();
            t.project = project;
            t.script = script_name;
            let injected_env = entry.env.clone().unwrap_or_default();
            t.injected_env = injected_env.clone();
            pending_services.push(PendingService {
                name: name.clone(),
                cmd: entry.cmd.clone(),
                path: service_path_str,
                scrollback,
                save_logs,
                log_dir: log_dir.clone(),
                dep_names: deps,
                health_checks,
                shutdown_cmd: entry.shutdown_cmd.clone(),
                injected_env,
                tab_index: idx,
            });
            t
        } else {
            let injected_env = entry.env.clone().unwrap_or_default();
            spawn_checked_terminal(
                &service_path_str,
                &entry.cmd,
                &name,
                scrollback,
                save_logs,
                log_dir.clone(),
                health_checks,
                entry.shutdown_cmd.clone(),
                branch.clone(),
                project,
                &script_name,
                injected_env,
            )
        };
        // Mark shared resources so teardown keeps them alive (skips the
        // `shutdown_cmd`) while any sibling instance still serves the same
        // project+script. This matters when a shared service was started here
        // (not borrowed) and another instance is using it.
        if shared {
            terminal.shared = true;
        }
        // Declared endpoint endpoints (health tracked independently of the
        // parent). Started immediately so the index/UI has readings as soon as
        // the service appears; a pending service's endpoints simply report
        // unhealthy until it starts.
        terminal.set_endpoints(endpoints);
        terminal.start_endpoint_health_checks();
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

    let proxy = match script.proxy.clone() {
        Some(mut pc) => {
            // Resolve upstream/host templates with ports+branch
            for route in &mut pc.routes {
                if crate::ports::has_template(&route.upstream) {
                    route.upstream =
                        crate::ports::resolve_template(&route.upstream, ports, branch.as_deref())
                            .map_err(|e| format!("proxy upstream template error: {e}"))?;
                }
                if let Some(h) = &route.host
                    && crate::ports::has_template(h)
                {
                    route.host = Some(
                        crate::ports::resolve_template(h, ports, branch.as_deref())
                            .map_err(|e| format!("proxy host template error: {e}"))?,
                    );
                }
            }
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
            let terminal_cfg = script.terminal.clone().unwrap_or_default();
            let mut p = ProxyInstance::new(
                pc.port,
                pc.host,
                routes,
                max_log_entries,
                pc.tls_cert,
                pc.tls_key,
            )
            .with_terminal_config(terminal_cfg);
            p.start();
            Some(p)
        }
        None => None,
    };

    Ok(Runtime {
        items,
        pending_services,
        proxy,
    })
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
            endpoint: None,
            depends_on: deps.map(|d| d.into_iter().map(String::from).collect()),
            shutdown_cmd: None,
            env: None,
            reuse: false,
            share: false,
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

    fn reuse_entry(name: &str, health: Option<HealthCheckSpec>) -> ConfigEntry {
        ConfigEntry {
            name: Some(name.to_string()),
            path: ".".to_string(),
            cmd: "true".to_string(),
            health_check: health,
            endpoint: None,
            depends_on: None,
            shutdown_cmd: None,
            env: None,
            reuse: true,
            share: false,
        }
    }

    fn share_entry(name: &str, health: Option<HealthCheckSpec>) -> ConfigEntry {
        ConfigEntry {
            name: Some(name.to_string()),
            path: ".".to_string(),
            cmd: "true".to_string(),
            health_check: health,
            endpoint: None,
            depends_on: None,
            shutdown_cmd: None,
            env: None,
            reuse: false,
            share: true,
        }
    }

    fn script_with(entries: Vec<ConfigEntry>) -> ScriptConfig {
        ScriptConfig {
            service: Some(entries),
            proxy: None,
            terminal: None,
            concurrent: false,
        }
    }

    fn script_with_concurrent(entries: Vec<ConfigEntry>) -> ScriptConfig {
        ScriptConfig {
            service: Some(entries),
            proxy: None,
            terminal: None,
            concurrent: true,
        }
    }

    fn tcp_health(target: &str) -> HealthCheckSpec {
        HealthCheckSpec::Single(HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target: target.to_string(),
            compose_file: None,
            interval_ms: None,
            timeout_ms: Some(100),
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        })
    }

    /// Binds a fake live instance socket (`$TMPDIR/fog-<pid>.sock`) that answers
    /// status requests, so `find_instances_with_status` treats it as a sibling.
    fn spawn_status_instance(
        pid: u32,
        project: &str,
        script: &str,
        branch: Option<&str>,
        no_share: bool,
        ports: crate::ports::PortMap,
    ) -> std::path::PathBuf {
        use std::os::unix::net::UnixListener;
        let state = std::sync::Arc::new(crate::ipc::IpcState::new(
            script.to_string(),
            Some(project.to_string()),
            branch.map(str::to_string),
            no_share,
        ));
        *state.ports.lock().expect("mutex poisoned") = ports;
        let path = std::env::temp_dir().join(format!("fog-{pid}.sock"));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                crate::ipc::handle_connection(stream, state.clone());
            }
        });
        path
    }

    #[test]
    fn test_adopt_shared_ports_uses_same_branch_sibling_ports() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let project = format!("fog-test-adopt-{}", std::process::id());
        let sibling = spawn_status_instance(
            std::process::id().wrapping_add(21),
            &project,
            "dev",
            Some("mine"),
            false,
            [("db".to_string(), 55555u16)].into_iter().collect(),
        );

        let mut db = share_entry("db", Some(tcp_health(&addr.to_string())));
        db.env = Some(
            [(
                "DATABASE_URL".to_string(),
                "postgres://x@127.0.0.1:${ports.db}/x".to_string(),
            )]
            .into_iter()
            .collect(),
        );
        let script = script_with_concurrent(vec![db]);
        let mut ports: crate::ports::PortMap = [("db".to_string(), 12345u16)].into_iter().collect();

        adopt_shared_ports(
            &script,
            "dev",
            Path::new("."),
            Some(&project),
            Some("mine"),
            &mut ports,
            false,
        );

        let _ = std::fs::remove_file(&sibling);
        assert_eq!(
            ports["db"], 55555,
            "a borrowed shared service must adopt the owner's live port"
        );
    }

    #[test]
    fn test_adopt_shared_ports_ignores_other_branch_sibling() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let project = format!("fog-test-adopt-branch-{}", std::process::id());
        let sibling = spawn_status_instance(
            std::process::id().wrapping_add(22),
            &project,
            "dev",
            Some("other"),
            false,
            [("db".to_string(), 55555u16)].into_iter().collect(),
        );

        let db = share_entry("db", Some(tcp_health(&addr.to_string())));
        let script = script_with_concurrent(vec![db]);
        let mut ports: crate::ports::PortMap = [("db".to_string(), 12345u16)].into_iter().collect();

        adopt_shared_ports(
            &script,
            "dev",
            Path::new("."),
            Some(&project),
            Some("mine"),
            &mut ports,
            false,
        );

        let _ = std::fs::remove_file(&sibling);
        assert_eq!(
            ports["db"], 12345,
            "another branch owns different resources and must not be adopted"
        );
    }

    #[test]
    fn test_adopt_shared_ports_ignores_no_share_sibling() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let project = format!("fog-test-adopt-noshare-{}", std::process::id());
        let sibling = spawn_status_instance(
            std::process::id().wrapping_add(23),
            &project,
            "dev",
            Some("mine"),
            true,
            [("db".to_string(), 55555u16)].into_iter().collect(),
        );

        let db = share_entry("db", Some(tcp_health(&addr.to_string())));
        let script = script_with_concurrent(vec![db]);
        let mut ports: crate::ports::PortMap = [("db".to_string(), 12345u16)].into_iter().collect();

        adopt_shared_ports(
            &script,
            "dev",
            Path::new("."),
            Some(&project),
            Some("mine"),
            &mut ports,
            false,
        );

        let _ = std::fs::remove_file(&sibling);
        assert_eq!(
            ports["db"], 12345,
            "a --no-share sibling yields nothing to adopt"
        );
    }

    #[test]
    fn test_build_reuse_down_resource_starts_immediately() {
        let script = script_with(vec![reuse_entry("infra", Some(tcp_health("127.0.0.1:1")))]);
        let mut adopted = HashMap::new();
        let rt = build(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
        )
        .unwrap();
        assert!(
            !rt.items[0].reused,
            "a down reused resource must be started, not borrowed"
        );
    }

    #[test]
    fn test_build_reuse_up_resource_borrows() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let script = script_with(vec![reuse_entry(
            "infra",
            Some(tcp_health(&addr.to_string())),
        )]);
        let mut adopted = HashMap::new();
        let rt = build(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
        )
        .unwrap();
        assert!(rt.items[0].reused, "an up reused resource must be borrowed");
        assert_eq!(
            rt.items[0].get_health_status(),
            crate::terminal::HealthStatus::Healthy,
            "a passing probe seeds the reused terminal as healthy"
        );
    }

    #[test]
    fn test_build_reuse_without_health_check_starts() {
        let script = script_with(vec![reuse_entry("infra", None)]);
        let mut adopted = HashMap::new();
        let rt = build(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
        )
        .unwrap();
        assert!(
            !rt.items[0].reused,
            "reuse without a health check cannot verify, so it starts"
        );
    }

    #[test]
    fn test_build_concurrent_share_up_resource_borrows() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let script =
            script_with_concurrent(vec![share_entry("db", Some(tcp_health(&addr.to_string())))]);
        let mut adopted = HashMap::new();
        let rt = build(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
        )
        .unwrap();
        assert!(rt.items[0].reused, "an up shared resource must be borrowed");
        assert!(
            rt.items[0].shared,
            "a borrowed shared resource must stay marked shared for teardown"
        );
    }

    #[test]
    fn test_build_concurrent_share_down_resource_starts() {
        let script =
            script_with_concurrent(vec![share_entry("db", Some(tcp_health("127.0.0.1:1")))]);
        let mut adopted = HashMap::new();
        let rt = build(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
        )
        .unwrap();
        assert!(
            !rt.items[0].reused,
            "a down shared resource must be started, not borrowed"
        );
        assert!(
            rt.items[0].shared,
            "a started shared resource stays marked shared so siblings keep it alive"
        );
    }

    #[test]
    fn test_build_concurrent_reuse_flag_ignored() {
        // In concurrent mode only `share` governs; a `reuse`-flagged service is
        // a plain per-instance service and must be spawned, not borrowed.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let script =
            script_with_concurrent(vec![reuse_entry("db", Some(tcp_health(&addr.to_string())))]);
        let mut adopted = HashMap::new();
        let rt = build(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
        )
        .unwrap();
        assert!(
            !rt.items[0].reused,
            "reuse is ignored in concurrent mode: the service must be started"
        );
        assert!(
            !rt.items[0].shared,
            "a non-share service in concurrent mode is not shared"
        );
    }

    #[test]
    fn test_build_single_instance_share_flag_ignored() {
        // In single-instance mode only `reuse` governs; a `share`-flagged
        // service is a plain per-instance service and must be spawned.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let script = script_with(vec![share_entry("db", Some(tcp_health(&addr.to_string())))]);
        let mut adopted = HashMap::new();
        let rt = build(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
        )
        .unwrap();
        assert!(
            !rt.items[0].reused,
            "share is ignored in single-instance mode: the service must be started"
        );
        assert!(!rt.items[0].shared);
    }

    #[test]
    fn test_build_no_share_ignores_share_even_when_healthy() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let script =
            script_with_concurrent(vec![share_entry("db", Some(tcp_health(&addr.to_string())))]);
        let mut adopted = HashMap::new();
        let rt = build_with_ports_no_share(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
            &HashMap::new(),
            None,
            true,
        )
        .unwrap();
        assert!(
            !rt.items[0].reused,
            "--no-share must not borrow an up shared resource"
        );
        assert!(
            !rt.items[0].shared,
            "--no-share must not mark the terminal as shared"
        );
    }

    #[test]
    fn test_build_no_share_ignores_reuse_even_when_healthy() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let script = script_with(vec![reuse_entry(
            "infra",
            Some(tcp_health(&addr.to_string())),
        )]);
        let mut adopted = HashMap::new();
        let rt = build_with_ports_no_share(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
            &HashMap::new(),
            None,
            true,
        )
        .unwrap();
        assert!(
            !rt.items[0].reused,
            "--no-share must not borrow an up reused resource"
        );
        assert!(!rt.items[0].shared);
    }

    fn sub(name: &str, host: Option<&str>, port: Option<&str>) -> crate::config::EndpointConfig {
        crate::config::EndpointConfig {
            name: name.to_string(),
            host: host.map(String::from),
            port: port.map(String::from),
            path_prefix: None,
            health_check: None,
        }
    }

    #[test]
    fn test_resolve_endpoint_templates() {
        let mut e = entry("infra", None);
        e.endpoint = Some(vec![crate::config::EndpointConfig {
            name: "web".to_string(),
            host: Some("web.${branch}.acme".to_string()),
            port: Some("${ports.web}".to_string()),
            path_prefix: Some("/v1".to_string()),
            health_check: None,
        }]);
        let pm: crate::ports::PortMap = [("web".to_string(), 8080u16)].into_iter().collect();
        let resolved = resolve_service_templates(&e, &pm, Some("main")).unwrap();
        let sub = &resolved.endpoint.as_ref().unwrap()[0];
        assert_eq!(sub.host.as_deref(), Some("web.main.acme"));
        assert_eq!(sub.port.as_deref(), Some("8080"));
        assert_eq!(sub.path_prefix.as_deref(), Some("/v1"));
    }

    #[test]
    fn test_build_folds_endpoint_checks_into_service_health() {
        let l1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a1 = l1.local_addr().unwrap();
        let l2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a2 = l2.local_addr().unwrap();
        // Service-level check duplicates the first endpoint's check.
        let mut entry = share_entry("db", Some(tcp_health(&a1.to_string())));
        let mut e1 = sub("one", None, None);
        e1.health_check = Some(tcp_health(&a1.to_string()));
        let mut e2 = sub("two", None, None);
        e2.health_check = Some(tcp_health(&a2.to_string()));
        entry.endpoint = Some(vec![e1, e2]);

        let script = script_with_concurrent(vec![entry]);
        let mut adopted = HashMap::new();
        let rt = build(
            &script,
            "dev",
            Path::new("."),
            None,
            false,
            100,
            None,
            &mut adopted,
        )
        .unwrap();
        assert!(rt.items[0].reused, "an up shared resource is borrowed");
        assert_eq!(rt.items[0].endpoints.len(), 2);
        assert_eq!(
            rt.items[0].health_checks.len(),
            2,
            "service + endpoint checks are folded and deduplicated"
        );
    }

    #[test]
    fn test_endpoint_routes_flatten_host_and_port() {
        let mut t = Terminal::spawn_reused("infra".into(), ".".into(), "true".into(), 10);
        t.set_endpoints(vec![
            sub("web", Some("web.acme"), Some("8080")),
            // No host: display/health only, no route.
            sub("db", None, Some("5432")),
        ]);
        let routes = endpoint_routes(&[t]);
        assert_eq!(routes.len(), 1, "only entries with host+port become routes");
        assert_eq!(routes[0].service, "infra");
        assert_eq!(routes[0].endpoint.as_deref(), Some("web"));
        assert_eq!(routes[0].host, "web.acme");
        assert_eq!(routes[0].port, "8080");
    }

    #[test]
    fn test_resolve_docker_compose_paths_relative() {
        let checks = vec![HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Docker,
            target: "postgres".into(),
            compose_file: Some("docker-compose.yml".into()),
            interval_ms: None,
            timeout_ms: None,
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        }];
        let resolved = resolve_docker_compose_paths(checks, Path::new("/repo/app/infra"));
        assert_eq!(
            resolved[0].compose_file.as_deref(),
            Some("/repo/app/infra/docker-compose.yml")
        );
    }

    #[test]
    fn test_resolve_docker_compose_paths_defaults_untouched() {
        let checks = vec![HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Docker,
            target: "postgres".into(),
            compose_file: None,
            interval_ms: None,
            timeout_ms: None,
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        }];
        let resolved = resolve_docker_compose_paths(checks, Path::new("/repo/app/infra"));
        assert_eq!(resolved[0].compose_file, None);
    }

    #[test]
    fn test_resolve_docker_compose_paths_ignores_non_docker() {
        let checks = vec![HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target: "localhost:5432".into(),
            compose_file: None,
            interval_ms: None,
            timeout_ms: None,
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        }];
        let resolved = resolve_docker_compose_paths(checks, Path::new("/repo/app/infra"));
        assert_eq!(resolved[0].compose_file, None);
    }
}
