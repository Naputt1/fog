//! App lifecycle: construction handoff, the event loop, runtime refresh,
//! teardown, and service actions triggered from the UI or IPC.

use super::{App, AppEvent};
use crate::click_tab::TabKind;
use crate::config_watcher;
use crate::ipc;
use crate::runtime;
use crate::terminal::{HealthStatus, Init};
use crossterm::event::{self, DisableMouseCapture};
use crossterm::execute;
use ratatui::DefaultTerminal;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

impl App {
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let health_rx = crate::terminal::health_signal().subscribe();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<AppEvent>();

        // crossterm's event reader is not thread-safe: exactly one thread may
        // read from it. This reader owns the terminal input for the whole run.
        let input_tx = event_tx.clone();
        thread::spawn(move || {
            while let Ok(ev) = event::read() {
                if input_tx.send(AppEvent::Input(ev)).is_err() {
                    break;
                }
            }
        });
        // Bridge health pings onto the same channel so the loop wakes the moment
        // a dependency becomes ready instead of waiting out the poll timeout.
        let health_tx = event_tx.clone();
        thread::spawn(move || {
            while health_rx.recv().is_ok() {
                if health_tx.send(AppEvent::Health).is_err() {
                    break;
                }
            }
        });
        drop(event_tx);

        // How long to block waiting for input before running periodic work.
        const TICK_INTERVAL: Duration = Duration::from_millis(50);
        // How often to refresh process liveness and the IPC snapshot. Drawing
        // is decoupled from this: it happens only when something changed.
        const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

        self.refresh_runtime_state();
        self.force_redraw = true;
        let mut last_refresh = Instant::now();
        let mut refresh_now = false;

        while !self.exit {
            if self.config_rx.try_recv().is_ok() {
                self.reload_config();
                self.force_redraw = true;
            }
            if (self.sigint.load(Ordering::SeqCst)
                || self.ipc_state.kill_flag.load(Ordering::SeqCst))
                && self.prepare_exit()
            {
                break;
            }
            self.handle_control_request();
            for i in 0..self.items.len() {
                if let Err(e) = self.items[i].maybe_auto_start() {
                    self.note_error(format!("auto-start error: {}", e));
                }
            }
            if refresh_now || last_refresh.elapsed() >= REFRESH_INTERVAL {
                if self.refresh_runtime_state() {
                    self.force_redraw = true;
                }
                last_refresh = Instant::now();
                refresh_now = false;
            }
            if self.prune_alerts() {
                self.force_redraw = true;
            }
            // Redraw only when something actually changed; idle ticks do no work.
            if self.needs_redraw() {
                terminal.draw(|frame| self.draw(frame))?;
                self.record_drawn();
            }
            match event_rx.recv_timeout(TICK_INTERVAL) {
                Ok(AppEvent::Input(ev)) => {
                    self.handle_event(ev)?;
                    self.force_redraw = true;
                }
                Ok(AppEvent::Health) => {
                    refresh_now = true;
                    self.force_redraw = true;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            // Drain any queued input without blocking so bursts stay responsive.
            while let Ok(AppEvent::Input(ev)) = event_rx.try_recv() {
                self.handle_event(ev)?;
                self.force_redraw = true;
            }
            self.handle_auto_scroll();
        }
        self.clear_reuse_skip_shutdown_cmds();
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        if !self.errors.is_empty() {
            for err in &self.errors {
                let _ = writeln!(std::io::stderr(), "{}", err);
            }
        }
        Ok(())
    }

    /// Runs the script headlessly, without a TUI. Used by detached (`-d`)
    /// runs: services keep their PTYs, health checks, dependency ordering and
    /// IPC socket, so `fog ls` / `fog kill` / `fog logs` behave exactly as
    /// with the TUI, but nothing is drawn and the loop never blocks on input.
    pub fn run_headless(&mut self) -> io::Result<()> {
        // How long to block waiting for a health ping before polling signals
        // and IPC. Liveness and the IPC snapshot refresh on a slower cadence.
        const TICK_INTERVAL: Duration = Duration::from_millis(50);
        const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

        let health_rx = crate::terminal::health_signal().subscribe();
        let mut last_refresh: Option<Instant> = None;
        while !self.exit {
            if (self.sigint.load(Ordering::SeqCst)
                || self.ipc_state.kill_flag.load(Ordering::SeqCst))
                && self.prepare_exit()
            {
                break;
            }
            self.handle_control_request();
            for i in 0..self.items.len() {
                if let Err(e) = self.items[i].maybe_auto_start() {
                    self.note_error(format!("auto-start error: {}", e));
                }
            }
            // Wake immediately when a dependency becomes ready; the timeout
            // still services signals, IPC and config changes.
            let health_woke = health_rx.recv_timeout(TICK_INTERVAL).is_ok();
            if health_woke || last_refresh.is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL) {
                self.refresh_runtime_state();
                last_refresh = Some(Instant::now());
            }
        }
        self.clear_reuse_skip_shutdown_cmds();
        if !self.errors.is_empty() {
            for err in &self.errors {
                let _ = writeln!(std::io::stderr(), "{}", err);
            }
        }
        Ok(())
    }

    /// Prepares any requested handoffs, waits for the IPC thread to send them,
    /// and marks the app as exiting. Returns `true` when the caller should
    /// break out of its run loop.
    pub(crate) fn prepare_exit(&mut self) -> bool {
        self.perform_handoff();
        // Give the IPC thread a moment to send any handoffs before we
        // drop our terminals.
        if self
            .ipc_state
            .handoff_req
            .lock()
            .expect("mutex poisoned")
            .is_some()
        {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while std::time::Instant::now() < deadline
                && !self.ipc_state.handoff_done.load(Ordering::SeqCst)
            {
                thread::sleep(Duration::from_millis(20));
            }
            // If the transfer never completed (e.g. the connection
            // dropped), close any prepared-but-unsent fds so they are
            // not leaked. Reuse services themselves survive: they were
            // marked handed-off and are not killed on teardown.
            if !self.ipc_state.handoff_done.load(Ordering::SeqCst) {
                let fds: Vec<_> = std::mem::take(
                    &mut *self
                        .ipc_state
                        .handoff_results
                        .lock()
                        .expect("mutex poisoned"),
                )
                .into_iter()
                .map(|h| h.fd)
                .collect();
                for fd in fds {
                    // These handles were dupped for transfer and are owned by
                    // this instance until sent.
                    crate::fds::close(fd);
                }
            }
        }
        self.exit = true;
        true
    }

    /// Skips the `shutdown_cmd` of services a replacing instance asked to
    /// reuse, so the shared resource stays up across the handoff.
    pub(crate) fn clear_reuse_skip_shutdown_cmds(&mut self) {
        let reuse_skip = self
            .ipc_state
            .reuse_skip
            .lock()
            .expect("mutex poisoned")
            .clone();
        for item in &mut self.items {
            if reuse_skip.contains(&item.name) {
                item.shutdown_cmd = None;
            }
        }
    }

    /// Runs the main event loop until exit is requested.
    ///
    /// Draws the UI on every tick and processes keyboard and mouse events.
    ///
    /// # Arguments
    /// * `terminal` - The ratatui terminal to render to.
    ///
    /// # Errors
    /// Returns an error if terminal rendering or event polling fails.
    pub(crate) fn reload_config(&mut self) {
        let ports = self.ipc_state.ports.lock().expect("mutex poisoned").clone();
        let branch =
            runtime::resolve_branch(self.config_path.parent().unwrap_or_else(|| Path::new(".")));
        let had_proxy = self.proxy.is_some();
        match config_watcher::reload_config(
            &self.config_path,
            &self.ipc_state.script,
            &mut self.proxy,
            &mut self.theme,
            &ports,
            branch.as_deref(),
        ) {
            Ok(_) => {
                let has_proxy = self.proxy.is_some();
                if had_proxy != has_proxy {
                    // Reconcile the proxy tab with the proxy's new presence.
                    if has_proxy {
                        self.tabs.insert_at(0, "proxy".to_string(), TabKind::Proxy);
                        self.proxy_tab_index = Some(0);
                    } else {
                        self.tabs.remove(0);
                        self.proxy_tab_index = None;
                    }
                    self.last_drawn_proxy_fp = (0, 0);
                }
                // Keep the IPC-exposed live log handle in step with the proxy.
                *self.ipc_state.proxy_logs.lock().expect("mutex poisoned") =
                    self.proxy.as_ref().map(|p| p.logs_handle());
            }
            Err(e) => self.note_error(e),
        }
    }

    /// Extracts live services requested for handover by a replacing instance
    /// and publishes them for the IPC thread to send over the socket.
    pub(crate) fn perform_handoff(&mut self) {
        if self.no_share {
            self.ipc_state
                .handoff_prepared
                .store(true, Ordering::SeqCst);
            return;
        }
        let req = self
            .ipc_state
            .handoff_req
            .lock()
            .expect("mutex poisoned")
            .clone();
        let Some(names) = req else {
            return;
        };
        let mut results = Vec::new();
        for item in &mut self.items {
            if names.contains(&item.name)
                && (item.reused || item.shared)
                && let Some(handoff) = item.extract_handoff()
            {
                results.push(handoff);
            }
        }
        *self
            .ipc_state
            .handoff_results
            .lock()
            .expect("mutex poisoned") = results;
        // Signal the IPC thread that the handoffs are ready to send, so it
        // never sends an empty set before we have prepared ours.
        self.ipc_state
            .handoff_prepared
            .store(true, Ordering::SeqCst);
    }

    /// Refreshes process liveness, tab status, and the IPC snapshot. Runs on a
    /// slow cadence (and on health changes) instead of every frame.
    ///
    /// Returns `true` when a visible status changed, so the caller can redraw.
    pub(crate) fn refresh_runtime_state(&mut self) -> bool {
        self.check_pending();

        let mut changed = false;
        let proxy_offset = usize::from(self.proxy_tab_index.is_some());
        for (i, item) in self.items.iter_mut().enumerate() {
            item.refresh_status();
            if let Some(entry) = self.tabs.entries.get_mut(i + proxy_offset) {
                let health = item.get_health_status();
                let pending = health == HealthStatus::Pending;
                if entry.stopped != item.stopped
                    || entry.process_running != item.process_running
                    || entry.pending != pending
                    || entry.health_status != health
                {
                    changed = true;
                }
                entry.stopped = item.stopped;
                entry.process_running = item.process_running;
                entry.pending = pending;
                entry.health_status = health;
            }
        }

        self.update_shared_state();

        if let Some(ref p) = self.proxy
            && let Some(entry) = self
                .tabs
                .entries
                .iter_mut()
                .find(|e| e.kind == TabKind::Proxy)
            && entry.stopped != !p.is_running()
        {
            entry.stopped = !p.is_running();
            changed = true;
        }

        // Propagate unhealthy status through dependency chains.
        let n = self.items.len();
        for _ in 0..n {
            let mut round_changed = false;
            for i in 0..n {
                let deps = &self.items[i].dep_names;
                if deps.is_empty() {
                    continue;
                }
                let dep_unhealthy = deps.iter().any(|dep| {
                    self.tabs
                        .entries
                        .iter()
                        .find(|e| e.name == *dep)
                        .map(|e| e.health_status == HealthStatus::Unhealthy)
                        .unwrap_or(false)
                });
                if dep_unhealthy
                    && let Some(entry) = self.tabs.entries.get_mut(i + proxy_offset)
                    && entry.health_status != HealthStatus::Unhealthy
                {
                    entry.health_status = HealthStatus::Unhealthy;
                    round_changed = true;
                    changed = true;
                }
            }
            if !round_changed {
                break;
            }
        }

        changed
    }

    pub(crate) fn update_shared_state(&self) {
        let mut services = self.ipc_state.services.lock().expect("mutex poisoned");
        services.clear();
        for item in &self.items {
            services.push(ipc::ServiceStatus {
                name: item.name.clone(),
                running: !item.stopped && item.process_running,
                health: format!("{:?}", item.get_health_status()).to_lowercase(),
                endpoints: item.endpoint_statuses(),
            });
        }
        let mut proxy = self.ipc_state.proxy.lock().expect("mutex poisoned");
        *proxy = self.proxy.as_ref().map(|p| ipc::ProxyStatus {
            running: p.is_running(),
            port: p.bound_port(),
        });
        // Live terminal raw output for web emulation (same process as TUI, raw ANSI bytes).
        let mut snaps = self
            .ipc_state
            .terminal_snapshots
            .lock()
            .expect("mutex poisoned");
        for item in &self.items {
            let chunks = item.drain_raw_output();
            if chunks.is_empty() {
                continue;
            }
            let entry = snaps.entry(item.name.clone()).or_default();
            for chunk in chunks {
                let b64 =
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &chunk);
                entry.push(b64);
                if entry.len() > 500 {
                    entry.remove(0);
                }
            }
        }
    }

    /// Executes one per-service control request published by the IPC thread
    /// (if any) and publishes the verdict back to the waiting connection.
    ///
    /// The app loop is the ONLY writer of `control_result`/`control_done`;
    /// the IPC thread only publishes the request and waits for the signal.
    pub(crate) fn handle_control_request(&mut self) {
        let req = self
            .ipc_state
            .control_req
            .lock()
            .expect("mutex poisoned")
            .take();
        let Some(req) = req else {
            return;
        };
        self.force_redraw = true;
        let resp = self.execute_service_action(&req);
        *self
            .ipc_state
            .control_result
            .lock()
            .expect("mutex poisoned") = Some(resp);
        self.ipc_state.control_done.store(true, Ordering::SeqCst);
    }

    /// Runs a single control action against the named service and returns the
    /// [`ipc::ControlResponse`] to report back to the requesting client.
    pub(crate) fn execute_service_action(
        &mut self,
        req: &ipc::ServiceActionRequest,
    ) -> ipc::ControlResponse {
        if req.name == "proxy" {
            return ipc::ControlResponse {
                ok: false,
                reason: "unsupported".to_string(),
            };
        }
        let Some(idx) = self.items.iter().position(|t| t.name == req.name) else {
            return ipc::ControlResponse {
                ok: false,
                reason: "unknown service".to_string(),
            };
        };
        match req.action {
            ipc::ServiceAction::TerminalInput { ref data } => {
                let bytes = match base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    data,
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        return ipc::ControlResponse {
                            ok: false,
                            reason: format!("invalid base64: {e}"),
                        };
                    }
                };
                self.items[idx].write(&bytes);
                ipc::ControlResponse {
                    ok: true,
                    reason: String::new(),
                }
            }
            ipc::ServiceAction::TerminalResize { cols, rows } => {
                self.items[idx].resize(cols, rows);
                ipc::ControlResponse {
                    ok: true,
                    reason: String::new(),
                }
            }
            ipc::ServiceAction::Stop => match self.items[idx].stop() {
                Ok(()) => ipc::ControlResponse {
                    ok: true,
                    reason: String::new(),
                },
                Err(e) => ipc::ControlResponse {
                    ok: false,
                    reason: e.to_string(),
                },
            },
            ipc::ServiceAction::Restart => match self.items[idx].restart() {
                Ok(()) => ipc::ControlResponse {
                    ok: true,
                    reason: String::new(),
                },
                Err(e) => ipc::ControlResponse {
                    ok: false,
                    reason: e.to_string(),
                },
            },
            ipc::ServiceAction::Start => {
                let item = &self.items[idx];
                if !item.stopped && item.process_running {
                    return ipc::ControlResponse {
                        ok: false,
                        reason: "already running".to_string(),
                    };
                }
                // The terminal remembers the command it was spawned with; a
                // not-yet-started pending service still holds its real
                // path/cmd in `pending_services`.
                let (path, cmd) = match &item.init {
                    Init::Command { path, cmd } if !path.is_empty() && !cmd.is_empty() => {
                        (path.clone(), cmd.clone())
                    }
                    _ => match self.pending_services.iter().find(|ps| ps.name == req.name) {
                        Some(ps) => (ps.path.clone(), ps.cmd.clone()),
                        None => {
                            return ipc::ControlResponse {
                                ok: false,
                                reason: "cannot start service: no command configured".to_string(),
                            };
                        }
                    },
                };
                match self.items[idx].start(&path, &cmd) {
                    Ok(()) => {
                        // If this was still a pending service, promote it exactly
                        // like `check_pending` would so it is not started twice
                        // once its dependencies become ready.
                        if let Some(ps_idx) = self
                            .pending_services
                            .iter()
                            .position(|ps| ps.name == req.name)
                        {
                            let ps = self.pending_services.remove(ps_idx);
                            let item = &mut self.items[idx];
                            item.log_dir = ps.log_dir.clone();
                            item.health_checks = ps.health_checks;
                            item.shutdown_cmd = ps.shutdown_cmd;
                            item.dep_names = ps.dep_names.clone();
                            item.save_logs = ps.save_logs;
                            if !item.health_checks.is_empty() {
                                item.start_health_checks();
                            }
                        }
                        ipc::ControlResponse {
                            ok: true,
                            reason: String::new(),
                        }
                    }
                    Err(e) => ipc::ControlResponse {
                        ok: false,
                        reason: e.to_string(),
                    },
                }
            }
        }
    }

    /// Checks pending services and starts them once all dependencies are ready.
    pub(crate) fn check_pending(&mut self) {
        let ready = self
            .pending_services
            .iter()
            .map(|ps| {
                let all_deps_ready = ps.dep_names.iter().all(|dep| {
                    self.items
                        .iter()
                        .find(|t| t.name == *dep)
                        .map(|t| t.is_ready())
                        .unwrap_or(false)
                });
                (ps.tab_index, all_deps_ready)
            })
            .collect::<Vec<_>>();

        // Process in reverse index order so removals don't shift pending positions
        for (tab_index, all_ready) in ready.into_iter().rev() {
            if !all_ready {
                continue;
            }
            let ps_idx = self
                .pending_services
                .iter()
                .position(|p| p.tab_index == tab_index);
            let Some(idx) = ps_idx else { continue };
            let ps = self.pending_services.remove(idx);
            let name = ps.name.clone();
            let mut start_error: Option<String> = None;

            if let Some(item) = self.items.get_mut(tab_index) {
                item.log_dir = ps.log_dir.clone();
                item.injected_env = ps.injected_env.clone();
                match item.start(&ps.path, &ps.cmd) {
                    Ok(()) => {
                        item.health_checks = ps.health_checks;
                        item.shutdown_cmd = ps.shutdown_cmd;
                        item.dep_names = ps.dep_names.clone();
                        item.save_logs = ps.save_logs;
                        if !item.health_checks.is_empty() {
                            item.start_health_checks();
                        }
                    }
                    Err(e) => start_error = Some(format!("failed to start '{name}': {e}")),
                }
                // Update tab entry (tabs include the proxy, items do not)
                if let Some(entry) = self
                    .tabs
                    .entries
                    .get_mut(tab_index + usize::from(self.proxy_tab_index.is_some()))
                {
                    entry.pending = false;
                }
            }
            if let Some(err) = start_error {
                self.note_error(err);
            }
        }
    }
}
