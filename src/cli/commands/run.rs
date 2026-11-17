//! `fog <script>`: the full run path — config, reclaim, ports, IPC, the TUI,
//! and teardown.
//!
//! This is the only module here that owns an owner lock and an instance
//! directory, so it is also the only one that may decide an instance already
//! exists: everything else either reads state or kills it.

use crate::app::App;
use crate::cli::commands::daemon::{
    ProgressLog, create_log_dir, note_phase, print_daemon_started, redirect_daemon_output,
};
use crate::cli::commands::reclaim::{
    all_service_names, reclaim_takeover, reconcile_instance, reuse_names,
};
use crate::cli::presentation::color_enabled;
use crate::cli::progress::StopProgress;
use crate::cli::{
    Cli, list_scripts_and_exit, load_config, resolve_config_path, resolve_run_config,
};
use crate::config_watcher;
use crate::ipc;
use crate::theme::Theme;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, IsTerminal, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How much scrollback the embedded terminal keeps.
const DEFAULT_SCROLLBACK: usize = 2000;

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

pub(crate) fn run_script(name: &str, cli: &Cli) -> io::Result<()> {
    run_script_inner(name, cli, None)
}

/// Shared body of `fog <script>` and `fog attach <pid>`. When `takeover_pid` is
/// set, the run adopts that instance's live services (a detach successor or a
/// reattach) instead of reconciling with a peer instance.
pub(crate) fn run_script_inner(name: &str, cli: &Cli, takeover_pid: Option<u32>) -> io::Result<()> {
    // A `-d` run executes as a background daemon (re-executed by `main` with
    // `FOG_DAEMON_CHILD` set); the daemon child skips the TUI and runs a
    // headless service loop instead.
    // A `d` detach re-executes this process with `FOG_DETACH_FROM=<pid>` set:
    // the child is a headless takeover that adopts the named instance's live
    // services via handoff.
    let takeover_from = takeover_pid.or_else(|| {
        std::env::var("FOG_DETACH_FROM")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
    });
    // A takeover requested via `fog attach` is interactive (it runs the TUI);
    // only `-d` and the detach daemon child skip the UI.
    let detached = cli.detach || std::env::var_os("FOG_DAEMON_CHILD").is_some();

    // Drop the daemon/takeover markers so they do not leak into spawned
    // services. No threads have been spawned yet, so mutating the environment
    // is race-free.
    if detached {
        // SAFETY: the process is still single-threaded at this point.
        unsafe {
            std::env::remove_var("FOG_DAEMON_CHILD");
            std::env::remove_var("FOG_DETACH_FROM");
        }
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
    // A detached daemon narrates setup to its parent through an append-only
    // phase file, since the IPC socket (the other progress channel) does not
    // exist until the ports are settled.
    let mut progress = detached
        .then(|| ProgressLog::create(log_dir.as_deref().expect("log dir is always created")));
    note_phase(&mut progress, "resolving config");

    let config_path = resolve_config_path(&resolve_run_config(cli));
    let config = load_config(&config_path);
    let script = match config.scripts.get(name) {
        Some(s) => s,
        None => list_scripts_and_exit(&config, &format!("unknown script '{name}'")),
    };
    let config_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.clone());
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    // `--no-share` / `--share` overrides for this run, resolved against the
    // script that is actually about to start. A bad `--share` name is a usage
    // error, reported before anything is torn down or allocated.
    let share = crate::runtime::SharePolicy {
        no_share: cli.no_share,
        forced: cli.share.iter().cloned().collect(),
    };
    validate_forced_share(&share, script, name, &config_dir);

    // Setup diagnostics are collected rather than printed immediately: in
    // interactive mode the TUI takes the screen moments later, so warnings are
    // surfaced in-app and everything is persisted to the instance's daemon.log.
    let mut startup_messages: Vec<String> = Vec::new();

    // Apply the configured dnsmasq wildcard-DNS routes before the TUI enters
    // raw mode, so sudo can prompt on a normal terminal. Best-effort: failures
    // only warn and never block the run.
    if let Some(dnsmasq) = config.dnsmasq.as_ref() {
        note_phase(&mut progress, "dnsmasq");
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
        note_phase(&mut progress, "router");
        startup_messages.extend(crate::router::ensure(router, &domains));
    }
    // Standalone index server (service directory + web UI). Controlled by
    // fog config (`~/.config/fog/fog.json` alongside `theme`, plus per-project
    // `fog.json` top-level `index`). Both default true; either can opt-out.
    if config.effective_should_serve_index() {
        note_phase(&mut progress, "index server");
        startup_messages.extend(crate::index::ensure_for_config(&config));
    }

    // Every start — TUI, daemon or worktree switch — keeps the project in the
    // index's "known project" list after it stops. Only launchable configs
    // (those with a `fog.json`) are remembered: the picker launches via that
    // file, so a `-c some-other.json` dir would only show up unlaunchable.
    crate::index::registry::remember(&config_dir);

    // A config kept outside its repo (e.g. `-c /tmp/override.json` pointing at
    // a repo worktree) takes the identity of the repo its services run in, so
    // the index does not label it after the config's folder.
    let service_paths: Vec<PathBuf> = script
        .service
        .iter()
        .flatten()
        .map(|entry| config_dir.join(&entry.path))
        .collect();
    let project = crate::project::identity(&config_dir, &service_paths);
    // Branch for instance identity: an explicit `--branch` wins; otherwise it
    // is resolved from the worktree containing the config (so a plain
    // `fog dev` in a worktree gets that worktree's branch).
    let branch = cli
        .branch
        .clone()
        .or_else(|| crate::runtime::resolve_branch(&config_dir));
    let takeover = takeover_from.is_some();
    // Capture the target's live ports before reclaiming it (and waiting for it
    // to exit), so adopted services keep their real assignments; auto ports
    // would otherwise be re-randomized.
    let takeover_ports = takeover_from
        .and_then(|pid| ipc::query_status(&ipc::socket_path(pid)).ok())
        .map(|s| s.ports);
    let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
    let mut owner_lock: Option<crate::lock::OwnerLock> = None;
    if let Some(from_pid) = takeover_from {
        // Detach/reattach takeover: reclaim exactly this instance, adopting
        // every live service (not just `reuse` ones) so the session keeps
        // running.
        note_phase(&mut progress, "adopting services");
        match reclaim_takeover(from_pid, &all_service_names(script, &config_dir)) {
            Some(handoffs) => adopted = handoffs,
            None => std::process::exit(1),
        }
    } else if let Some(ref project) = project {
        // Concurrent scripts (default) start alongside existing instances of the
        // same project+script instead of replacing them, so no coordination or
        // reclaim happens. Only single-instance scripts take over from a previous
        // run (handing over `reuse` services). `--no-share` disables reuse handoff.
        if !script.concurrent && !share.no_share {
            let reuse = reuse_names(script, &config_dir, &share);
            note_phase(&mut progress, "reclaiming previous instance");
            (adopted, owner_lock) = reconcile_instance(project, name, branch.as_deref(), &reuse);
        } else if !script.concurrent && share.no_share {
            // Still reclaim the old instance (single-instance semantics) but
            // without adopting any services.
            note_phase(&mut progress, "reclaiming previous instance");
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
        share.no_share,
    );
    ipc_state.config_dir = Some(config_dir.to_string_lossy().into_owned());
    ipc_state.config_path = Some(config_path.to_string_lossy().into_owned());
    // Publish the forced names so `fog attach` can rebuild this invocation.
    ipc_state.forced_share = share.forced.iter().cloned().collect();
    // Cold-start provenance: `references::resolve` passes the parent's pid and
    // identity when it spawns this instance, so `fog ls` and `fog kill` can
    // show and optionally stop instances this one caused to start.
    ipc_state.autostarted_by = std::env::var(crate::references::AUTOSTART_BY_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok());
    ipc_state.autostarted_by_label = std::env::var(crate::references::AUTOSTART_PARENT_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
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
    let has_shared = script.service.as_ref().is_some_and(|s| {
        s.iter().any(|e| {
            let name = crate::runtime::entry_name(e, &config_dir);
            share.shared(&name, e.share)
        })
    });
    let startup_lock = if script.concurrent && !share.no_share && has_shared {
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
    note_phase(&mut progress, "allocating ports");
    let mut port_specs = config.ports.clone().unwrap_or_default();
    // Cross-instance overrides are values, not allocations: they must not be
    // probed or re-bound locally, so keep them out of `allocate_ports`.
    let mut external_overrides = std::collections::HashMap::new();
    for (key, port) in overrides {
        if crate::ports::parse_external_port_key(&key).is_some() {
            external_overrides.insert(key, port);
        } else {
            port_specs.insert(key, port);
        }
    }
    let ports_defined = config.ports.is_some() || !port_specs.is_empty();

    // Adopt the live ports of a sibling that already owns a shared service
    // *before* allocating: a shared resource with a fixed port keeps listening
    // on it in the sibling, so allocating (and probing) that port locally would
    // fail with "already in use" even though this instance is about to borrow
    // it. Names adopted here are skipped by allocation; the sibling's values win
    // over local allocation and `--port` overrides, because they are where the
    // resource actually listens.
    let mut adopted_ports = std::collections::HashMap::new();
    let owned_shared = crate::runtime::adopt_shared_ports(
        script,
        name,
        &config_dir,
        project.as_deref(),
        branch_for_ports.as_deref(),
        &mut adopted_ports,
        &share,
    );
    let local_specs: std::collections::HashMap<String, u16> = port_specs
        .iter()
        .filter(|(key, _)| !adopted_ports.contains_key(*key))
        .map(|(key, port)| (key.clone(), *port))
        .collect();
    let mut port_map = if !local_specs.is_empty() {
        match crate::ports::allocate_ports(&local_specs) {
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
    // A detach takeover inherits the target's real port assignments (including
    // auto-allocated ones) so adopted services and the proxy/router keep
    // pointing at the ports the live processes actually listen on.
    if let Some(target_ports) = takeover_ports {
        for (name, port) in target_ports {
            port_map.insert(name, port);
        }
    }
    // An explicit `--port api@repo:branch[=PORT]` wins over lookup and
    // auto-start; references already present in `port_map` are left untouched.
    port_map.extend(external_overrides);
    // Adopted sibling ports land last: they are the borrowed resource's real
    // address, which dependent templates must resolve to. Adoption and
    // cross-instance resolution run before validation so every template sees
    // its final port.
    port_map.extend(adopted_ports);
    // Resolve `${ports.<name>@<repo>:<branch>[:<script>]}` against live fog
    // instances; a `:script` reference may start its target when it is not
    // running. An explicit `--port <key>=PORT` already in `port_map` wins.
    if let Err(e) = crate::references::resolve(
        script,
        config.native_routes.as_ref(),
        crate::references::Owner {
            config_dir: &config_dir,
            project: project.as_deref(),
            branch: branch_for_ports.as_deref(),
            script_name: name,
        },
        &mut port_map,
        &mut startup_messages,
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

    note_phase(&mut progress, "starting services");
    let runtime = crate::runtime::build_with_share_policy(
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
        share.clone(),
        takeover,
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

    // Interactive runs can detach: build the recipe to relaunch this exact
    // invocation as a headless takeover daemon when `d` is pressed.
    let detach = if detached {
        None
    } else {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fog"));
        // Rebuild the invocation from the resolved script + config rather than
        // copying argv: an attach session's argv is `fog attach <pid>`, which
        // must not be re-executed by its detach successor.
        let mut args: Vec<OsString> = vec![
            OsString::from(name),
            OsString::from("--config"),
            OsString::from(config_path.as_os_str()),
        ];
        if let Some(branch) = cli.branch.as_ref() {
            args.push(OsString::from("--branch"));
            args.push(OsString::from(branch));
        }
        if cli.save_logs {
            args.push(OsString::from("--save-logs"));
        }
        if cli.verbose {
            args.push(OsString::from("--verbose"));
        }
        // Keep the forced services across a detach: the successor rebuilds the
        // run from these args, so dropping them would restart a borrowed
        // resource instead of taking it over.
        for service in &share.forced {
            args.push(OsString::from("--share"));
            args.push(OsString::from(service));
        }
        Some(crate::app::DetachSpawn::new(exe, args, std::process::id()))
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
        share: share.clone(),
        verbose: cli.verbose,
        detach,
        startup_messages: startup_warnings,
    });
    if detached {
        app.run_headless()?;
    } else {
        // `App::run` returns as soon as exit is requested without tearing
        // services down. `ratatui::run` has restored the alternate screen by
        // the time it returns, so teardown happens on the normal terminal and
        // can render the same docker-style per-service shutdown progress
        // `fog kill` shows (reusing the instance's own IPC status).
        let run_result = ratatui::run(|terminal| app.run(terminal));
        // `ratatui::run` leaves the alternate screen, but the `TerminalGuard`
        // also entered it (and enabled mouse capture); restore it explicitly so
        // progress is printed to the normal screen.
        TerminalGuard::restore();
        if app.detach_spawned() {
            // `d` handed the whole session to a detached successor. Services
            // keep running; dropping the handed-off terminals at process exit
            // is a no-op for their processes, and their `shutdown_cmd` is
            // skipped. Report where the detached instance lives and release our
            // socket so the successor can publish its own.
            if let Some(pid) = app.detach_pid() {
                print_daemon_started(name, pid);
            }
            app.report_errors();
            run_result?;
            ipc::cleanup_socket();
            return Ok(());
        }
        let path = ipc::current_socket_path();
        // Each `teardown_service` blocks for the whole teardown (kill grace +
        // reap), so a spinner advanced only at service boundaries would look
        // frozen. Pump the view from a background thread at the same cadence
        // `fog kill` uses while the main thread tears services down.
        let mut view =
            StopProgress::new(std::process::id(), stdout().is_terminal(), color_enabled());
        let _ = view.refresh(&path, &mut stdout());
        let stop = Arc::new(AtomicBool::new(false));
        let render = {
            let stop = Arc::clone(&stop);
            let path = path.clone();
            let mut view = view;
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = view.tick(&path, &mut stdout());
                    std::thread::sleep(Duration::from_millis(50));
                }
                view
            })
        };
        app.clear_reuse_skip_shutdown_cmds();
        for i in 0..app.service_count() {
            app.teardown_service(i);
        }
        stop.store(true, Ordering::Relaxed);
        if let Ok(mut view) = render.join() {
            // Pick up the final state before the settled checklist is drawn.
            let _ = view.refresh(&path, &mut stdout());
            let _ = view.finish(&mut stdout());
        }
        app.report_errors();
        run_result?;
    }

    if config.effective_should_serve_index() {
        crate::log::emit(&crate::index::ensure_for_config(&config), cli.verbose);
    }

    ipc::cleanup_socket();

    // Clean up native Traefik routes for this branch (explicit only).
    crate::router::cleanup_native_routes(branch_for_ports.as_deref(), &config);

    // The index server is left running after the last instance exits so its
    // SPA can launch the next one; stop it with `fog index kill`.

    Ok(())
}

/// Rejects a `--share NAME` the script does not define, before the run touches
/// any instance, port or route. `SERVICE` is the name the TUI shows (an
/// explicit `name`, else the `path` basename).
fn validate_forced_share(
    share: &crate::runtime::SharePolicy,
    script: &crate::config::ScriptConfig,
    script_name: &str,
    config_dir: &std::path::Path,
) {
    if share.forced.is_empty() {
        return;
    }
    let known = all_service_names(script, config_dir);
    let unknown: Vec<&str> = share
        .forced
        .iter()
        .map(String::as_str)
        .filter(|name| !known.iter().any(|k| k.as_str() == *name))
        .collect();
    if unknown.is_empty() {
        return;
    }
    let hint = if known.is_empty() {
        format!("script '{script_name}' has no services")
    } else {
        format!("services in '{script_name}': {}", known.join(", "))
    };
    let quoted: Vec<String> = unknown.iter().map(|n| format!("'{n}'")).collect();
    crate::cli::usage_error(
        crate::error::FogError::new(format!(
            "--share: unknown service {} in script '{script_name}'",
            quoted.join(", ")
        )),
        &hint,
    );
}
