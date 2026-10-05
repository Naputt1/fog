use crate::click_tab::{ClickTab, TabKind};
use crate::config::HealthCheckConfig;
use crate::config_watcher;
use crate::ipc::{self, IpcState};
use crate::keybinding;
use crate::proxy::ProxyInstance;
use crate::runtime;
use crate::selection;
use crate::terminal::{HealthStatus, Init, Terminal};
use crate::theme::Theme;
use crate::worktree::{self, Worktree};
use crossterm::event::{
    self, DisableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use ratatui::{DefaultTerminal, layout::Rect};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::{
    io,
    time::{Duration, Instant},
};

mod switch;
use switch::SwitchPopup;
mod render;
mod tabs;

enum Mode {
    Normal,
    TerminalInput,
    ProxyFilter,
}

/// Events delivered to the TUI loop by the input-reader and health-forwarder
/// threads. Fusing them onto one channel lets the loop wake instantly on either
/// input or a service health change.
enum AppEvent {
    /// A crossterm input event.
    Input(Event),
    /// A service health status changed; re-check pending dependents.
    Health,
}

/// A service waiting for its dependencies to become ready.
pub struct PendingService {
    /// Display name of the service.
    pub name: String,
    /// Shell command to execute.
    pub cmd: String,
    /// Working directory path.
    pub path: String,
    /// Maximum scrollback lines.
    pub scrollback: usize,
    /// Whether to save logs on exit.
    pub save_logs: bool,
    /// Directory to tee this service's raw PTY output into (`<name>.log`),
    /// used by detached (`-d`) runs so an external agent can tail it.
    pub log_dir: Option<PathBuf>,
    /// Names of services this depends on.
    pub dep_names: Vec<String>,
    /// Health check configurations for this service.
    pub health_checks: Vec<HealthCheckConfig>,
    /// Shell command to run on shutdown.
    pub shutdown_cmd: Option<String>,
    /// Resolved env vars for this service (from `${ports.*}` templates).
    pub injected_env: std::collections::HashMap<String, String>,
    /// Index in the `items` vec where this service's terminal lives.
    pub tab_index: usize,
}

/// Main application state managing terminals, the proxy, tabs, and input handling.
pub struct App {
    items: Vec<Terminal>,
    pending_services: Vec<PendingService>,
    proxy: Option<ProxyInstance>,
    sigint: Arc<AtomicBool>,
    theme: Theme,
    scrollback: usize,
    tabs: ClickTab,
    mode: Mode,
    scroll_offset: usize,
    exit: bool,
    selecting: bool,
    select_start: Option<(usize, usize)>,
    select_end: Option<(usize, usize)>,
    content_area: Rect,
    show_help: bool,
    errors: Vec<String>,
    proxy_filter: String,
    config_path: std::path::PathBuf,
    /// The `--config` value as passed on the command line (may be relative),
    /// used to resolve a target worktree's config when switching.
    config_rel: PathBuf,
    /// Whether service output is saved to `temp/<name>.txt` on exit.
    save_logs: bool,
    config_rx: std::sync::mpsc::Receiver<()>,
    proxy_tab_index: Option<usize>,
    sidebar_min: u16,
    sidebar_max: u16,
    scrollbar_dragging: bool,
    auto_scrolling: Option<bool>,
    auto_scroll_col: u16,
    content_layout: selection::RowLayout,
    switch_popup: Option<SwitchPopup>,
    config_watcher_stop: Arc<AtomicBool>,
    ipc_state: Arc<IpcState>,
    /// Branch shown in the content panel's top title. Tracked separately from
    /// `ipc_state.branch` (which is set once at startup) so a worktree switch
    /// updates the title without changing the instance's IPC identity.
    title_branch: Option<String>,
    /// Whether `--no-share` was passed: when true shared/reuse services are
    /// always started fresh instead of borrowed/handed over.
    no_share: bool,
    /// Whether `--verbose` was passed: gates informational setup output.
    verbose: bool,
    /// Non-fatal startup warnings, shown as a dismissible overlay.
    startup_messages: Vec<String>,
    /// Whether the startup-warning overlay is currently visible.
    show_startup_popup: bool,
    /// Set when something happened that requires the next frame to be drawn
    /// (input, health change, config reload, IPC control, resize).
    force_redraw: bool,
    /// Visible terminal's screen generation at the last draw.
    last_drawn_gen: usize,
    /// Proxy request-log fingerprint at the last draw.
    last_drawn_proxy_fp: (usize, u64),
    /// When a frame was last drawn, bounding how long a missed invalidation can
    /// leave the UI stale.
    last_draw: Instant,
}

/// Options for creating an `App` without `clippy::too_many_arguments`.
pub struct AppCreateOpts {
    pub items: Vec<Terminal>,
    pub pending_services: Vec<PendingService>,
    pub proxy: Option<ProxyInstance>,
    pub sigint: Arc<AtomicBool>,
    pub scrollback: usize,
    pub sidebar_min: u16,
    pub sidebar_max: u16,
    pub theme: Theme,
    pub config_path: std::path::PathBuf,
    pub config_rx: std::sync::mpsc::Receiver<()>,
    /// Stop flag owned by the active config watcher, set when it is replaced.
    pub config_watcher_stop: Arc<AtomicBool>,
    pub ipc_state: Arc<IpcState>,
    pub config_rel: PathBuf,
    pub save_logs: bool,
    pub no_share: bool,
    pub verbose: bool,
    /// Non-fatal startup warnings, shown as a dismissible overlay and
    /// reprinted to stderr on exit.
    pub startup_messages: Vec<String>,
}

impl App {
    /// Preferred constructor using `AppCreateOpts`.
    pub fn new_with_opts(opts: AppCreateOpts) -> Self {
        let AppCreateOpts {
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
            config_watcher_stop,
            ipc_state,
            config_rel,
            save_logs,
            no_share,
            verbose,
            startup_messages,
        } = opts;
        let show_startup_popup = !startup_messages.is_empty();
        let title_branch = ipc_state.branch.clone();
        let (tabs, proxy_tab_index) = Self::build_tabs(
            &items,
            &pending_services,
            proxy.is_some(),
            sidebar_min,
            sidebar_max,
        );

        Self {
            items,
            pending_services,
            proxy,
            sigint,
            scrollback,
            theme,
            tabs,
            mode: Mode::Normal,
            scroll_offset: 0,
            exit: false,
            selecting: false,
            select_start: None,
            select_end: None,
            content_area: Rect::default(),
            show_help: false,
            errors: startup_messages.clone(),
            proxy_filter: String::new(),
            config_path,
            config_rel,
            save_logs,
            config_rx,
            config_watcher_stop,
            ipc_state,
            title_branch,
            proxy_tab_index,
            sidebar_min,
            sidebar_max,
            scrollbar_dragging: false,
            auto_scrolling: None,
            auto_scroll_col: 0,
            content_layout: Vec::new(),
            switch_popup: None,
            no_share,
            verbose,
            startup_messages,
            show_startup_popup,
            force_redraw: true,
            last_drawn_gen: 0,
            last_drawn_proxy_fp: (0, 0),
            last_draw: Instant::now(),
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
    fn reload_config(&mut self) {
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
            Err(e) => self.errors.push(e),
        }
    }

    /// Extracts live services requested for handover by a replacing instance
    /// and publishes them for the IPC thread to send over the socket.
    fn perform_handoff(&mut self) {
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
                    self.errors.push(format!("auto-start error: {}", e));
                }
            }
            if refresh_now || last_refresh.elapsed() >= REFRESH_INTERVAL {
                if self.refresh_runtime_state() {
                    self.force_redraw = true;
                }
                last_refresh = Instant::now();
                refresh_now = false;
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

    /// Prepares any requested handoffs, waits for the IPC thread to send them,
    /// and marks the app as exiting. Returns `true` when the caller should
    /// break out of its run loop.
    fn prepare_exit(&mut self) -> bool {
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
                    self.errors.push(format!("auto-start error: {}", e));
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

    /// Skips the `shutdown_cmd` of services a replacing instance asked to
    /// reuse, so the shared resource stays up across the handoff.
    fn clear_reuse_skip_shutdown_cmds(&mut self) {
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

    fn handle_event(&mut self, ev: Event) -> io::Result<()> {
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let idx_before = self.tabs.index;
                    self.tabs.click(mouse.column, mouse.row);
                    if self.tabs.index != idx_before {
                        self.on_tab_switch();
                        return Ok(());
                    }
                    if self.handle_scrollbar_click(mouse.column, mouse.row) {
                        self.scrollbar_dragging = true;
                        return Ok(());
                    }
                    let layout = self.active_layout();
                    if let Some(pos) = selection::screen_to_content(
                        mouse.column,
                        mouse.row,
                        self.content_area,
                        self.scroll_offset,
                        self.current_total_lines(),
                        layout,
                    ) {
                        self.selecting = true;
                        self.select_start = Some(pos);
                        self.select_end = Some(pos);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if self.scrollbar_dragging {
                        self.handle_scrollbar_drag(mouse.row);
                    } else if self.selecting {
                        let inner_y = self.content_area.y.saturating_add(1);
                        let inner_h = self.content_area.height.saturating_sub(2);
                        if mouse.row < inner_y {
                            self.auto_scrolling = Some(true);
                            self.auto_scroll_col = mouse.column;
                            self.step_auto_scroll();
                        } else if mouse.row >= inner_y.saturating_add(inner_h) {
                            self.auto_scrolling = Some(false);
                            self.auto_scroll_col = mouse.column;
                            self.step_auto_scroll();
                        } else {
                            self.auto_scrolling = None;
                            let layout = self.active_layout();
                            if let Some(pos) = selection::screen_to_content(
                                mouse.column,
                                mouse.row,
                                self.content_area,
                                self.scroll_offset,
                                self.current_total_lines(),
                                layout,
                            ) {
                                self.select_end = Some(pos);
                            }
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.auto_scrolling = None;
                    if self.scrollbar_dragging {
                        self.scrollbar_dragging = false;
                    }
                    if self.selecting {
                        self.selecting = false;
                        if let (Some(start), Some(end)) = (self.select_start, self.select_end)
                            && let Some(idx) = self.service_tab_index()
                        {
                            selection::copy_selection(start, end, &self.items, idx);
                        }
                        self.select_start = None;
                        self.select_end = None;
                    }
                }
                MouseEventKind::ScrollUp => {
                    self.scroll_to(self.scroll_offset.saturating_add(3));
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_to(self.scroll_offset.saturating_sub(3));
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn on_tab_switch(&mut self) {
        self.scroll_offset = 0;
        self.scrollbar_dragging = false;
        self.auto_scrolling = None;
        self.content_layout.clear();
        selection::clear_selection(
            &mut self.selecting,
            &mut self.select_start,
            &mut self.select_end,
        );
        self.proxy_filter.clear();
        if self.is_proxy_tab() {
            self.mode = Mode::Normal;
        } else if let Some(item) = self.service_tab_index().and_then(|i| self.items.get(i)) {
            self.mode = if item.is_shell() {
                Mode::TerminalInput
            } else {
                Mode::Normal
            };
        }
    }

    fn is_shell_tab(&self, idx: usize) -> bool {
        self.item_index_for_tab(idx)
            .and_then(|i| self.items.get(i))
            .map(|t| t.is_shell())
            .unwrap_or(false)
    }

    /// Records a setup warning: shown in the startup overlay (info lines are
    /// omitted) and always reprinted to stderr once the TUI exits.
    fn note_setup_message(&mut self, msg: String) {
        if !crate::log::is_info(&msg) {
            self.startup_messages.push(msg.clone());
            self.show_startup_popup = true;
        }
        self.errors.push(msg);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.show_startup_popup {
            self.show_startup_popup = false;
            // Any key dismisses the overlay; quit keys also take effect.
            if key.code == KeyCode::Char('q')
                || (key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q'))
            {
                self.exit = true;
            }
            return;
        }
        if self.show_help {
            match key.code {
                KeyCode::Char('?') => self.show_help = false,
                KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => self.exit = true,
                _ => self.show_help = false,
            }
            return;
        }

        if self.switch_popup.is_some() {
            self.handle_switch_key(key);
            return;
        }

        // With no tabs (empty script), tab navigation would divide by zero;
        // only quitting and opening a shell terminal remain useful.
        if self.tabs.entries.is_empty() {
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('t') => {}
                _ => return,
            }
        }

        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('q') => {
                    self.exit = true;
                    return;
                }
                KeyCode::Char('n') => {
                    let prev = self.tabs.index;
                    self.tabs.index = (self.tabs.index + 1) % self.tabs.entries.len();
                    if prev != self.tabs.index {
                        self.on_tab_switch();
                    }
                    return;
                }
                KeyCode::Char('p') => {
                    let prev = self.tabs.index;
                    self.tabs.index =
                        (self.tabs.index + self.tabs.entries.len() - 1) % self.tabs.entries.len();
                    if prev != self.tabs.index {
                        self.on_tab_switch();
                    }
                    return;
                }
                KeyCode::Char('t') => {
                    self.new_terminal();
                    return;
                }
                _ => {}
            }
        }

        if matches!(self.mode, Mode::ProxyFilter) {
            match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.proxy_filter.clear();
                }
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                }
                KeyCode::Backspace => {
                    self.proxy_filter.pop();
                }
                KeyCode::Char(c) if !c.is_control() => {
                    self.proxy_filter.push(c);
                }
                _ => {}
            }
            return;
        }

        match self.mode {
            Mode::TerminalInput => self.handle_terminal_key(key),
            Mode::Normal => self.handle_normal_key(key),
            Mode::ProxyFilter => {}
        }
    }

    fn handle_terminal_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return;
        }
        if let Some(item) = self.service_tab_index().and_then(|i| self.items.get_mut(i))
            && let Some(bytes) = keybinding::key_to_bytes(key)
        {
            item.write(&bytes);
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.exit = true,
            KeyCode::Esc => {}
            KeyCode::Char('j') | KeyCode::Right => {
                let prev = self.tabs.index;
                self.tabs.index = (self.tabs.index + 1) % self.tabs.entries.len();
                if prev != self.tabs.index {
                    self.on_tab_switch();
                }
            }
            KeyCode::Char('k') | KeyCode::Left => {
                let prev = self.tabs.index;
                self.tabs.index =
                    (self.tabs.index + self.tabs.entries.len() - 1) % self.tabs.entries.len();
                if prev != self.tabs.index {
                    self.on_tab_switch();
                }
            }
            KeyCode::Down => {
                self.scroll_to(self.scroll_offset.saturating_sub(1));
            }
            KeyCode::Up => {
                self.scroll_to(self.scroll_offset.saturating_add(1));
            }
            KeyCode::PageUp => {
                let h = self.content_height();
                self.scroll_to(self.scroll_offset.saturating_add(h as usize));
            }
            KeyCode::PageDown => {
                let h = self.content_height();
                self.scroll_to(self.scroll_offset.saturating_sub(h as usize));
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll_to(self.current_total_lines());
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll_offset = 0;
            }
            KeyCode::Char('i') => {
                if !self.is_proxy_tab() {
                    self.mode = Mode::TerminalInput;
                }
            }
            KeyCode::Char('R') => self.restart_current(),
            KeyCode::Char('t') => self.new_terminal(),
            KeyCode::Char('d') => self.close_tab(),
            KeyCode::Char('/') => {
                if self.is_proxy_tab() {
                    self.mode = Mode::ProxyFilter;
                }
            }
            KeyCode::Char('s') => self.open_switch_popup(),
            KeyCode::Char('?') => self.show_help = !self.show_help,
            _ => {}
        }
    }

    /// Opens the worktree-switch popup, listing the repository's worktrees.
    fn open_switch_popup(&mut self) {
        let config_dir = self
            .config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        match worktree::list(&config_dir) {
            Some(worktrees) if !worktrees.is_empty() => {
                self.switch_popup = Some(SwitchPopup {
                    running: self.running_branches(),
                    worktrees,
                    filter: String::new(),
                    selected: 0,
                    searching: false,
                    status: None,
                });
            }
            _ => {
                self.errors
                    .push("no git worktrees found in this repository".to_string());
            }
        }
    }

    /// Branches with a live fog instance serving this project's script,
    /// discovered by scanning the project's IPC sockets. The instance
    /// scanning already excludes this process, so the current branch is only
    /// listed when a second instance serves it.
    fn running_branches(&self) -> Vec<String> {
        let Some(project) = self.ipc_state.project.as_deref() else {
            return Vec::new();
        };
        let mut running = Vec::new();
        for (_, _, status) in ipc::find_instances_any_branch(project, &self.ipc_state.script) {
            if let Some(branch) = status.branch
                && !running.contains(&branch)
            {
                running.push(branch);
            }
        }
        running
    }

    /// Handles keys while the worktree-switch popup is open. In search mode
    /// (`f` toggled) typing filters the list; browsing accepts `f`, arrows,
    /// Enter (switch), `d` (terminate), and Esc.
    fn handle_switch_key(&mut self, key: KeyEvent) {
        // A status message lingers only until the next key press.
        if let Some(p) = &mut self.switch_popup {
            p.status = None;
        }
        let searching = self
            .switch_popup
            .as_ref()
            .map(|p| p.searching)
            .unwrap_or(false);
        match key.code {
            KeyCode::Char('f') if !searching => {
                if let Some(p) = &mut self.switch_popup {
                    p.searching = true;
                    p.selected = 0;
                }
            }
            KeyCode::Char('d') if !searching => self.terminate_selected_branch(),
            KeyCode::Esc if searching => {
                if let Some(p) = &mut self.switch_popup {
                    p.searching = false;
                }
            }
            KeyCode::Esc => {
                self.switch_popup = None;
            }
            KeyCode::Enter => {
                let selected = self.switch_popup.as_ref().and_then(|p| {
                    let matches = p.matches();
                    if matches.is_empty() {
                        None
                    } else {
                        Some(matches[p.selected.min(matches.len() - 1)].clone())
                    }
                });
                if let Some(wt) = selected {
                    let config_dir = self.config_path.parent().unwrap_or_else(|| Path::new("."));
                    let already_here = self
                        .switch_popup
                        .as_ref()
                        .map(|p| worktree::is_current_worktree(&wt, &p.worktrees, config_dir))
                        .unwrap_or_else(|| wt.contains(config_dir));
                    if already_here {
                        if let Some(p) = &mut self.switch_popup {
                            p.status = Some("already on this branch".to_string());
                        }
                        return;
                    }
                    let result = self.switch_worktree(&wt);
                    if let Err(e) = result {
                        if let Some(p) = &mut self.switch_popup {
                            p.status = Some(e);
                        } else {
                            self.errors.push(e);
                        }
                    } else {
                        self.switch_popup = None;
                    }
                }
            }
            KeyCode::Up | KeyCode::Down => {
                if let Some(p) = &mut self.switch_popup {
                    let len = p.matches().len();
                    if len > 0 {
                        p.selected = if matches!(key.code, KeyCode::Up) {
                            (p.selected + len - 1) % len
                        } else {
                            (p.selected + 1) % len
                        };
                    }
                }
            }
            KeyCode::Backspace if searching => {
                if let Some(p) = &mut self.switch_popup {
                    p.filter.pop();
                    p.selected = 0;
                }
            }
            KeyCode::Char(c) if searching && !c.is_control() => {
                if let Some(p) = &mut self.switch_popup {
                    p.filter.push(c);
                    p.selected = 0;
                }
            }
            _ => {}
        }
    }

    /// Terminates every live fog instance serving the selected worktree's
    /// branch (via IPC kill requests with a SIGTERM fallback) and reports the
    /// outcome in the popup's transient status line. This instance is never
    /// a target: the instance scan excludes the current process, so `d` on
    /// the current branch only kills *other* instances sharing that branch.
    fn terminate_selected_branch(&mut self) {
        let (branch, is_current) = {
            let Some(popup) = &self.switch_popup else {
                return;
            };
            let matches = popup.matches();
            if matches.is_empty() {
                return;
            }
            let wt = &matches[popup.selected.min(matches.len() - 1)];
            let config_dir = self.config_path.parent().unwrap_or_else(|| Path::new("."));
            let is_current = worktree::is_current_worktree(wt, &popup.worktrees, config_dir);
            (wt.branch.clone(), is_current)
        };
        // Detached worktrees have `branch == None`. They are still killable
        // by matching `branch == None` instances (legacy/detached runs), so
        // we don't early-return here — we let the `find_*` scan decide.
        let project = self.ipc_state.project.clone();
        let script = self.ipc_state.script.clone();
        let instances = match project {
            Some(project) => ipc::find_instances_with_status(&project, &script, branch.as_deref())
                .into_iter()
                .map(|(pid, path, _)| (pid, path))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        let terminated = ipc::terminate_instances(&instances);
        if let Some(popup) = &mut self.switch_popup {
            popup.status = Some(match terminated {
                0 if branch.is_none() => {
                    "no running instances on this branch (detached)".to_string()
                }
                0 if is_current => "no other instances on this branch".to_string(),
                0 => "no running instances on this branch".to_string(),
                1 => "terminated 1 instance".to_string(),
                n => format!("terminated {n} instances"),
            });
        }
    }

    /// Switches this instance to run the given worktree's script in place:
    /// shared (reuse) services are handed over internally, non-reuse services
    /// are torn down, and tabs/proxy/config-watcher are rebuilt from the
    /// target worktree's config file.
    ///
    /// Returns `Ok(())` on success (popup should be closed) or `Err(msg)` on
    /// failure (caller should keep the popup open and show `msg` as status).
    fn switch_worktree(&mut self, wt: &Worktree) -> Result<(), String> {
        // Guard against switching to the worktree we are already on.
        let current_dir = self.config_path.parent().unwrap_or_else(|| Path::new("."));
        let already_here = if let Some(list) = worktree::list(current_dir) {
            worktree::is_current_worktree(wt, &list, current_dir)
        } else {
            wt.contains(current_dir)
        };
        if already_here {
            return Err("already on this branch".to_string());
        }
        // Resolve and validate the target worktree's config first, so a failure
        // leaves the current instance untouched.
        //
        // When `--config` was an absolute path that lives inside the current
        // worktree (the `cargo run -- --config /…/worktree/<id>/ui` case), we
        // rebase it onto the target worktree instead of sharing the same file.
        // Absolute paths outside any worktree (e.g. `~/.config/fog/fog.json`)
        // stay shared, per user preference.
        let config_path = if self.config_rel.is_absolute() {
            if let Some(list) = worktree::list(current_dir) {
                let cur_root = worktree::containing_worktree(&list, &self.config_path)
                    .or_else(|| worktree::containing_worktree(&list, &self.config_rel));
                if let Some(cur_root) = cur_root {
                    let cur_can = cur_root
                        .path
                        .canonicalize()
                        .unwrap_or_else(|_| cur_root.path.clone());
                    let cfg_can = self
                        .config_path
                        .canonicalize()
                        .unwrap_or_else(|_| self.config_path.clone());
                    let rel_can = self
                        .config_rel
                        .canonicalize()
                        .unwrap_or_else(|_| self.config_rel.clone());
                    // Try canonical strip first, then raw strip for non-existent paths
                    let rel = cfg_can
                        .strip_prefix(&cur_can)
                        .or_else(|_| rel_can.strip_prefix(&cur_can))
                        .ok()
                        .or_else(|| {
                            self.config_path
                                .strip_prefix(&cur_root.path)
                                .or_else(|_| self.config_rel.strip_prefix(&cur_root.path))
                                .ok()
                        });
                    if let Some(rel) = rel {
                        wt.path.join(rel)
                    } else {
                        // Inside repo but strip failed (e.g. symlinks) — fall back to share
                        self.config_rel.clone()
                    }
                } else {
                    // Absolute outside any worktree -> share
                    self.config_rel.clone()
                }
            } else {
                self.config_rel.clone()
            }
        } else {
            wt.path.join(&self.config_rel)
        };
        let config =
            crate::config::load(&config_path).map_err(|e| format!("switch worktree: {e}"))?;
        let script_name = self.ipc_state.script.clone();
        let Some(script) = config.scripts.get(&script_name) else {
            return Err(format!(
                "switch worktree: script '{}' not found in '{}'",
                script_name,
                config_path.display()
            ));
        };
        // Validate the dependency graph up front so a bad target script leaves
        // the current instance untouched instead of tearing it down first.
        let entries = script.service.clone().unwrap_or_default();
        if let Err(e) = runtime::resolve_dep_order(&entries) {
            return Err(format!("switch worktree: {e}"));
        }
        let config_path = config_path.canonicalize().unwrap_or(config_path);
        let config_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        // Allocate ports and validate before tearing down the current instance,
        // so a failure leaves the running services untouched.
        let branch_for_ports = runtime::resolve_branch(&config_dir);
        let port_map = if let Some(specs) = &config.ports {
            crate::ports::allocate_ports(specs)
                .map_err(|e| format!("switch worktree: port allocation: {e}"))?
        } else {
            std::collections::HashMap::new()
        };
        if let Err(e) = crate::ports::ensure_ports_defined(
            config.ports.as_ref(),
            script,
            config.native_routes.as_ref(),
        ) {
            return Err(format!("switch worktree: {e}"));
        }
        // Adopt the live ports of a sibling that already owns a shared service
        // on the target branch, so its dependents resolve to the real port.
        let mut port_map = port_map;
        let owned_shared = runtime::adopt_shared_ports(
            script,
            &script_name,
            &config_dir,
            self.ipc_state.project.as_deref(),
            branch_for_ports.as_deref(),
            &mut port_map,
            self.no_share,
        );

        // Build the new runtime *before* tearing down the old one, so a
        // build failure doesn't leave the UI empty.
        let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
        if !self.no_share {
            for item in &mut self.items {
                if item.reused || item.shared {
                    if let Some(handoff) = item.extract_handoff() {
                        adopted.insert(handoff.name.clone(), handoff);
                    } else {
                        item.preserve_for_reuse();
                    }
                }
            }
        }
        let title_branch = branch_for_ports.clone();
        let built = match runtime::build_with_ports_no_share(
            script,
            &script_name,
            &config_dir,
            self.ipc_state.project.clone(),
            self.save_logs,
            self.scrollback,
            None,
            &mut adopted,
            &port_map,
            branch_for_ports.clone(),
            self.no_share,
            &owned_shared,
        ) {
            Ok(b) => b,
            Err(e) => {
                // Close any fds we duped for handoff that didn't get consumed
                for (_, handoff) in adopted.drain() {
                    crate::fds::close(handoff.fd);
                }
                return Err(format!("switch worktree: {e}"));
            }
        };

        // The project identity is the repo's git-common-dir, shared by every
        // worktree, so `ipc_state.project` stays unchanged across switches.
        self.title_branch = title_branch;

        // Tear down the old services and proxy now so their ports are free
        // before the new worktree's services start.
        self.proxy = None;
        self.items.clear();
        self.pending_services.clear();
        self.tabs = ClickTab::new(Vec::new(), self.sidebar_min, self.sidebar_max);
        self.proxy_tab_index = None;
        // Publish new ports/routes to IPC before ensuring Traefik, so the index UI
        // can synthesize native entries for the Services page.
        {
            *self.ipc_state.ports.lock().expect("mutex poisoned") = port_map.clone();
            let mut routes: Vec<crate::ipc::NativeRouteInfo> = config
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
            // Declared endpoint routes, flattened from the freshly built
            // terminals (resolved against the new ports/branch).
            routes.extend(runtime::endpoint_route_infos(&built.items));
            *self.ipc_state.native_routes.lock().expect("mutex poisoned") = routes;
        }
        // Clean up stale native routes from the previous branch before ensuring the new ones
        let prev_branch = runtime::resolve_branch(
            self.config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        );
        if prev_branch != branch_for_ports {
            crate::router::cleanup_native_routes(prev_branch.as_deref(), &config);
        }
        if let Some(routes) = &config.native_routes {
            for msg in crate::router::ensure_native_routes(
                routes,
                &port_map,
                branch_for_ports.as_deref(),
                &config,
                self.verbose,
            ) {
                self.note_setup_message(msg);
            }
        }
        // Declared endpoint routes for the new branch.
        for msg in crate::router::ensure_native_routes(
            &runtime::endpoint_routes(&built.items),
            &port_map,
            branch_for_ports.as_deref(),
            &config,
            self.verbose,
        ) {
            self.errors.push(msg);
        }

        let (tabs, proxy_tab_index) = Self::build_tabs(
            &built.items,
            &built.pending_services,
            built.proxy.is_some(),
            self.sidebar_min,
            self.sidebar_max,
        );

        self.items = built.items;
        self.pending_services = built.pending_services;
        self.proxy = built.proxy;
        self.tabs = tabs;
        self.proxy_tab_index = proxy_tab_index;
        self.config_path = config_path;
        // Stop the old watcher (it observes the flag within ~100ms) and replace
        // it with one owning the new stop flag we store.
        self.config_watcher_stop.store(true, Ordering::SeqCst);
        let (config_rx, config_watcher_stop) =
            config_watcher::spawn_config_watcher(self.config_path.clone());
        self.config_rx = config_rx;
        self.config_watcher_stop = config_watcher_stop;

        self.tabs.index = 0;
        self.scroll_offset = 0;
        self.mode = Mode::Normal;
        self.proxy_filter.clear();
        self.show_help = false;
        self.switch_popup = None;
        self.selecting = false;
        self.select_start = None;
        self.select_end = None;
        self.scrollbar_dragging = false;
        self.auto_scrolling = None;
        Ok(())
    }

    fn restart_current(&mut self) {
        if self.is_proxy_tab() {
            if let Some(ref mut p) = self.proxy {
                p.restart();
            }
            return;
        }
        if let Some(item) = self.service_tab_index().and_then(|i| self.items.get_mut(i))
            && !item.is_shell()
        {
            if let Err(e) = item.restart() {
                self.errors.push(format!("restart error: {}", e));
            }
            if let Some(e) = self.tabs.entries.get_mut(self.tabs.index) {
                e.stopped = false;
            }
        }
    }

    fn new_terminal(&mut self) {
        match Terminal::spawn_shell("bash".to_string(), self.scrollback) {
            Ok(term) => {
                let insertion_idx = self.tabs.entries.len();
                self.items.push(term);
                self.tabs
                    .insert_at(insertion_idx, "bash".to_string(), TabKind::Terminal);
                self.tabs.index = insertion_idx;
                self.scroll_offset = 0;
                self.mode = Mode::TerminalInput;
            }
            Err(e) => self
                .errors
                .push(format!("failed to create terminal: {}", e)),
        }
    }

    fn close_tab(&mut self) {
        if self.items.len() <= 1 {
            return;
        }
        if !self.is_shell_tab(self.tabs.index) {
            return;
        }
        let Some(item_idx) = self.service_tab_index() else {
            return;
        };
        self.items.remove(item_idx);
        self.tabs.remove(self.tabs.index);
        self.scroll_offset = 0;
        if self.is_shell_tab(self.tabs.index) {
            self.mode = Mode::TerminalInput;
        } else {
            self.mode = Mode::Normal;
        }
    }

    /// Refreshes process liveness, tab status, and the IPC snapshot. Runs on a
    /// slow cadence (and on health changes) instead of every frame.
    ///
    /// Returns `true` when a visible status changed, so the caller can redraw.
    fn refresh_runtime_state(&mut self) -> bool {
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

    fn update_shared_state(&self) {
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
    fn handle_control_request(&mut self) {
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
    fn execute_service_action(&mut self, req: &ipc::ServiceActionRequest) -> ipc::ControlResponse {
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
    fn check_pending(&mut self) {
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

            if let Some(item) = self.items.get_mut(tab_index) {
                item.log_dir = ps.log_dir.clone();
                item.injected_env = ps.injected_env.clone();
                if item.start(&ps.path, &ps.cmd).is_ok() {
                    item.health_checks = ps.health_checks;
                    item.shutdown_cmd = ps.shutdown_cmd;
                    item.dep_names = ps.dep_names.clone();
                    item.save_logs = ps.save_logs;
                    if !item.health_checks.is_empty() {
                        item.start_health_checks();
                    }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::render::panel_title_text;
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    fn make_app(
        items: Vec<Terminal>,
        proxy: Option<ProxyInstance>,
        tabs: ClickTab,
        mode: Mode,
        content_area: Rect,
    ) -> App {
        let proxy_tab_index = tabs.entries.iter().position(|e| e.kind == TabKind::Proxy);
        let (_tx, rx) = mpsc::channel();
        App {
            items,
            pending_services: vec![],
            proxy,
            sigint: Arc::new(AtomicBool::new(false)),
            theme: Theme::default(),
            scrollback: 0,
            tabs,
            mode,
            scroll_offset: 0,
            exit: false,
            selecting: false,
            select_start: None,
            select_end: None,
            content_area,
            show_help: false,
            errors: vec![],
            proxy_filter: String::new(),
            config_path: PathBuf::new(),
            config_rel: PathBuf::from("fog.json"),
            save_logs: false,
            config_rx: rx,
            config_watcher_stop: Arc::new(AtomicBool::new(false)),
            ipc_state: Arc::new(IpcState::new("test".to_string(), None, None, false)),
            title_branch: None,
            proxy_tab_index,
            sidebar_min: 10,
            sidebar_max: 30,
            scrollbar_dragging: false,
            auto_scrolling: None,
            auto_scroll_col: 0,
            content_layout: Vec::new(),
            switch_popup: None,
            no_share: false,
            verbose: false,
            startup_messages: vec![],
            show_startup_popup: false,
            force_redraw: true,
            last_drawn_gen: 0,
            last_drawn_proxy_fp: (0, 0),
            last_draw: Instant::now(),
        }
    }

    #[test]
    fn test_panel_title_text() {
        // Git repo: repo name plus branch.
        assert_eq!(
            panel_title_text(Some("/Users/alice/dev/fog/.git"), Some("main")),
            " fog (main) "
        );
        // Detached checkout in a git repo: repo name, no branch.
        assert_eq!(
            panel_title_text(Some("/Users/alice/dev/fog/.git"), None),
            " fog (detached) "
        );
        // Non-git fallback identity: dir name only, no "detached".
        assert_eq!(
            panel_title_text(Some("/tmp/my-project"), None),
            " my-project "
        );
        // No identity at all.
        assert_eq!(panel_title_text(None, Some("main")), "");
    }

    #[test]
    fn test_is_proxy_tab_false() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        assert!(!app.is_proxy_tab());
    }

    #[test]
    fn test_startup_popup_dismissed_by_any_key() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.startup_messages = vec!["⚠ docker hung".to_string()];
        app.show_startup_popup = true;

        app.handle_key(KeyEvent::from(KeyCode::Char('x')));

        assert!(!app.show_startup_popup);
        assert!(!app.exit);
    }

    #[test]
    fn test_startup_popup_quit_key_still_exits() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.startup_messages = vec!["⚠ docker hung".to_string()];
        app.show_startup_popup = true;

        app.handle_key(KeyEvent::from(KeyCode::Char('q')));

        assert!(!app.show_startup_popup);
        assert!(app.exit);
    }

    #[test]
    fn test_note_setup_message_shows_only_warnings() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());

        app.note_setup_message("  + port web -> 1".to_string());
        assert!(!app.show_startup_popup, "info lines never open the popup");
        assert_eq!(app.errors.len(), 1);

        app.note_setup_message("⚠ docker hung".to_string());
        assert!(app.show_startup_popup);
        assert_eq!(app.startup_messages.len(), 1);
        assert_eq!(app.errors.len(), 2);
    }

    #[test]
    fn test_startup_popup_renders_messages() {
        use ratatui::Terminal as RatatuiTerminal;
        use ratatui::backend::TestBackend;

        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.startup_messages = vec!["⚠ docker hung".to_string()];
        app.show_startup_popup = true;

        let mut terminal = RatatuiTerminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();

        let buf = terminal.backend().buffer();
        let content: String = (0..12)
            .flat_map(|y| (0..60).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(content.contains("Startup"), "overlay title missing");
        assert!(content.contains("docker hung"), "warning text missing");
    }

    #[test]
    fn test_is_proxy_tab_true() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert!(app.is_proxy_tab());
    }

    #[test]
    fn test_is_proxy_tab_not_selected() {
        let mut tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        tabs.index = 0;
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert!(!app.is_proxy_tab());
    }

    #[test]
    fn test_item_index_for_tab_with_proxy_at_zero() {
        // Mirrors build_tabs: proxy inserted at index 0, items have no proxy.
        let mut tabs = ClickTab::new(vec!["svc_a".into(), "svc_b".into()], 10, 30);
        tabs.insert_at(0, "proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        // proxy tab maps to no item
        assert_eq!(app.item_index_for_tab(0), None);
        // first service tab -> items[0]
        assert_eq!(app.item_index_for_tab(1), Some(0));
        // second service tab -> items[1]
        assert_eq!(app.item_index_for_tab(2), Some(1));
    }

    #[test]
    fn test_item_index_for_tab_without_proxy() {
        let tabs = ClickTab::new(vec!["svc_a".into(), "svc_b".into()], 10, 30);
        let app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        assert_eq!(app.item_index_for_tab(0), Some(0));
        assert_eq!(app.item_index_for_tab(1), Some(1));
    }

    #[test]
    fn test_content_height() {
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 20,
            },
        );
        assert_eq!(app.content_height(), 18);
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 5,
            },
        );
        assert_eq!(app.content_height(), 3);
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 1,
            },
        );
        assert_eq!(app.content_height(), 0);
    }

    #[test]
    fn test_current_total_lines_no_proxy_empty() {
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 0);
    }

    #[test]
    fn test_current_total_lines_with_proxy() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 3);
    }

    #[test]
    fn test_current_total_lines_with_proxy_filter_mode() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::ProxyFilter,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 4);
    }

    #[test]
    fn test_switch_popup_filter_by_branch() {
        let popup = SwitchPopup {
            worktrees: vec![
                Worktree {
                    path: PathBuf::from("/repo/fog"),
                    branch: Some("main".to_string()),
                },
                Worktree {
                    path: PathBuf::from("/repo/fog-feature"),
                    branch: Some("feature-x".to_string()),
                },
            ],
            filter: "feature".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        let matches = popup.matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].branch.as_deref(), Some("feature-x"));
    }

    #[test]
    fn test_switch_popup_filter_empty_matches_all() {
        let popup = SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        assert_eq!(popup.matches().len(), 1);
    }

    #[test]
    fn test_switch_popup_filter_matches_path() {
        let popup = SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog-detached"),
                branch: None,
            }],
            filter: "detached".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        assert_eq!(popup.matches().len(), 1);
    }

    #[test]
    fn test_switch_popup_filter_no_match() {
        let popup = SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: "zzz".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        assert!(popup.matches().is_empty());
    }

    #[test]
    fn test_switch_popup_fuzzy_subsequence() {
        let popup = SwitchPopup {
            worktrees: vec![
                Worktree {
                    path: PathBuf::from("/repo/fog"),
                    branch: Some("main".to_string()),
                },
                Worktree {
                    path: PathBuf::from("/repo/fog-feature"),
                    branch: Some("feature-x".to_string()),
                },
                Worktree {
                    path: PathBuf::from("/repo/fog-detached"),
                    branch: Some("detached-cleanup".to_string()),
                },
            ],
            filter: "ftx".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        let matches = popup.matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].branch.as_deref(), Some("feature-x"));
    }

    #[test]
    fn test_switch_popup_fuzzy_subsequence_case_insensitive() {
        let popup = SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog-feature"),
                branch: Some("feature-x".to_string()),
            }],
            filter: "FTX".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        assert_eq!(popup.matches().len(), 1);
    }

    #[test]
    fn test_switch_popup_search_mode_keys() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        });
        // 'f' enters search mode; typing appends to the filter.
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(!app.switch_popup.as_ref().unwrap().searching);
        assert!(app.switch_popup.as_ref().unwrap().filter.is_empty());
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('f')));
        assert!(app.switch_popup.as_ref().unwrap().searching);
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(app.switch_popup.as_ref().unwrap().filter, "a");
        // Backspace clears the filter while searching.
        app.handle_switch_key(KeyEvent::from(KeyCode::Backspace));
        assert!(app.switch_popup.as_ref().unwrap().filter.is_empty());
        // Esc exits search mode but keeps the popup open.
        app.handle_switch_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.switch_popup.is_some());
        assert!(!app.switch_popup.as_ref().unwrap().searching);
        // Typing outside search mode does nothing.
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.switch_popup.as_ref().unwrap().filter.is_empty());
        // Esc while browsing closes the popup.
        app.handle_switch_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.switch_popup.is_none());
    }

    #[test]
    fn test_switch_popup_d_while_searching_is_filter_input() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: true,
            running: Vec::new(),
            status: None,
        });
        // While searching `d` is filter input, never a terminate.
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('d')));
        let popup = app.switch_popup.as_ref().unwrap();
        assert_eq!(popup.filter, "d");
        assert!(popup.status.is_none());
    }

    #[test]
    fn test_switch_popup_status_cleared_on_next_key() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: Some("terminated 1 instance".to_string()),
        });
        // Any key press clears the transient status before handling itself.
        app.handle_switch_key(KeyEvent::from(KeyCode::Down));
        assert!(app.switch_popup.as_ref().unwrap().status.is_none());
    }

    #[test]
    fn test_switch_popup_d_reports_no_instances() {
        // make_app's IpcState has no project, so no instance can match: `d`
        // reports the zero-outcome status without touching any process.
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        });
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('d')));
        assert_eq!(
            app.switch_popup.as_ref().unwrap().status.as_deref(),
            Some("no running instances on this branch")
        );
    }

    #[test]
    fn test_switch_popup_d_on_detached_worktree() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog-detached"),
                branch: None,
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        });
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('d')));
        assert_eq!(
            app.switch_popup.as_ref().unwrap().status.as_deref(),
            Some("no running instances on this branch (detached)")
        );
    }

    #[test]
    fn test_switch_popup_d_on_current_branch_no_other_instances() {
        // `d` on the current branch should report "no other instances" rather
        // than "no running instances" to clarify that self is excluded.
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        // Make this app's config_dir be /repo/fog so the worktree counts as current.
        app.config_path = PathBuf::from("/repo/fog/fog.json");
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        });
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('d')));
        assert_eq!(
            app.switch_popup.as_ref().unwrap().status.as_deref(),
            Some("no other instances on this branch")
        );
    }
}
