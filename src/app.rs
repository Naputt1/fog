use crate::click_tab::{ClickTab, TabKind};
use crate::config::HealthCheckConfig;
use crate::config_watcher;
use crate::ipc::{self, IpcState};
use crate::keybinding;
use crate::proxy::ProxyInstance;
use crate::render;
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
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph},
};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::{io, time::Duration};

enum Mode {
    Normal,
    TerminalInput,
    ProxyFilter,
}

/// An open worktree-switch popup: the repository's worktrees plus an
/// incremental fuzzy filter, a selected row, live-branch markers, and a
/// transient status line. `f`-search mode feeds the filter (Esc returns to
/// browsing); `d` terminates the selected branch's live instances.
struct SwitchPopup {
    worktrees: Vec<Worktree>,
    filter: String,
    selected: usize,
    searching: bool,
    /// Branches that currently have a live fog instance serving them,
    /// rendered with a green asterisk.
    running: Vec<String>,
    /// Transient status message (e.g. the terminate outcome), cleared by the
    /// next key press.
    status: Option<String>,
}

impl SwitchPopup {
    /// The worktrees matching the current filter, in original order.
    fn matches(&self) -> Vec<Worktree> {
        if self.filter.is_empty() {
            return self.worktrees.clone();
        }
        self.worktrees
            .iter()
            .filter(|w| {
                subsequence_match(&w.label(), &self.filter)
                    || subsequence_match(&w.path.to_string_lossy(), &self.filter)
            })
            .cloned()
            .collect()
    }
}

/// Case-insensitive subsequence test: every char of `needle` appears in
/// `haystack` in order, not necessarily contiguously.
fn subsequence_match(haystack: &str, needle: &str) -> bool {
    let mut needle = needle.chars().flat_map(char::to_lowercase);
    let mut expected = needle.next();
    for c in haystack.chars().flat_map(char::to_lowercase) {
        let Some(exp) = expected else { return true };
        if c == exp {
            expected = needle.next();
        }
    }
    expected.is_none()
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
}

impl App {
    /// Creates a new [`App`] with the given terminals, optional proxy, and SIGINT flag.
    ///
    /// # Arguments
    /// * `items` - The list of terminal instances.
    /// * `proxy` - An optional reverse proxy instance.
    /// * `sigint` - An `AtomicBool` flag set to `true` when SIGINT (Ctrl+C) is received.
    /// * `scrollback` - Maximum number of scrollback lines.
    /// * `sidebar_min` - Minimum sidebar width in columns.
    /// * `sidebar_max` - Maximum sidebar width in columns.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        items: Vec<Terminal>,
        pending_services: Vec<PendingService>,
        proxy: Option<ProxyInstance>,
        sigint: Arc<AtomicBool>,
        scrollback: usize,
        sidebar_min: u16,
        sidebar_max: u16,
        theme: Theme,
        config_path: std::path::PathBuf,
        config_rx: std::sync::mpsc::Receiver<()>,
        ipc_state: Arc<IpcState>,
        config_rel: PathBuf,
        save_logs: bool,
    ) -> Self {
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
            errors: Vec::new(),
            proxy_filter: String::new(),
            config_path,
            config_rel,
            save_logs,
            config_rx,
            config_watcher_stop: Arc::new(AtomicBool::new(false)),
            ipc_state,
            proxy_tab_index,
            sidebar_min,
            sidebar_max,
            scrollbar_dragging: false,
            auto_scrolling: None,
            auto_scroll_col: 0,
            content_layout: Vec::new(),
            switch_popup: None,
        }
    }

    /// Builds the sidebar tabs and proxy-tab index for a set of items.
    fn build_tabs(
        items: &[Terminal],
        pending_services: &[PendingService],
        has_proxy: bool,
        sidebar_min: u16,
        sidebar_max: u16,
    ) -> (ClickTab, Option<usize>) {
        let names: Vec<String> = items.iter().map(|t| t.name.clone()).collect();
        let mut tabs = ClickTab::new(names, sidebar_min, sidebar_max);
        for (i, item) in items.iter().enumerate() {
            tabs.entries[i].kind = if item.is_shell() {
                TabKind::Terminal
            } else {
                TabKind::Service
            };
        }
        // Mark pending service tabs
        for ps in pending_services {
            if let Some(entry) = tabs.entries.get_mut(ps.tab_index) {
                entry.pending = true;
            }
        }
        let proxy_tab_index = if has_proxy {
            tabs.insert_at(0, "proxy".to_string(), TabKind::Proxy);
            Some(0)
        } else {
            None
        };
        (tabs, proxy_tab_index)
    }

    fn is_proxy_tab(&self) -> bool {
        self.tabs
            .entries
            .get(self.tabs.index)
            .map(|e| e.kind == TabKind::Proxy)
            .unwrap_or(false)
    }

    /// Maps a tab-bar index to an index into `self.items`, accounting for the
    /// proxy tab (which exists only in the tab bar, never in `items`).
    ///
    /// Returns `None` for the proxy tab itself or any out-of-range tab.
    fn item_index_for_tab(&self, tab_idx: usize) -> Option<usize> {
        match self.proxy_tab_index {
            Some(p) if tab_idx == p => None,
            Some(p) if tab_idx > p => Some(tab_idx - 1),
            _ => Some(tab_idx),
        }
    }

    /// Maps the currently selected tab to an index into `self.items`.
    fn service_tab_index(&self) -> Option<usize> {
        self.item_index_for_tab(self.tabs.index)
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
        config_watcher::reload_config(
            &self.config_path,
            &self.ipc_state.script,
            &mut self.proxy,
            &mut self.theme,
        );
    }

    /// Extracts live services requested for handover by a replacing instance
    /// and publishes them for the IPC thread to send over the socket.
    fn perform_handoff(&mut self) {
        let req = self
            .ipc_state
            .handoff_req
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(names) = req else {
            return;
        };
        let mut results = Vec::new();
        for item in &mut self.items {
            if names.contains(&item.name)
                && let Some(handoff) = item.extract_handoff()
            {
                results.push(handoff);
            }
        }
        *self
            .ipc_state
            .handoff_results
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = results;
        // Signal the IPC thread that the handoffs are ready to send, so it
        // never sends an empty set before we have prepared ours.
        self.ipc_state
            .handoff_prepared
            .store(true, Ordering::SeqCst);
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            if self.config_rx.try_recv().is_ok() {
                self.reload_config();
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
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))? {
                self.handle_events()?;
            }
            self.handle_auto_scroll();
        }
        self.clear_reuse_skip_shutdown_cmds();
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        let _ = event::poll(Duration::from_millis(20));
        while event::poll(Duration::from_millis(0)).unwrap_or(false) {
            let _ = event::read();
        }
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
            .unwrap_or_else(|e| e.into_inner())
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
                        .unwrap_or_else(|e| e.into_inner()),
                )
                .into_iter()
                .map(|h| h.fd)
                .collect();
                for fd in fds {
                    // SAFETY: these fds were dupped for transfer and
                    // are owned by this instance until sent.
                    unsafe {
                        libc::close(fd);
                    }
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
            self.check_pending();
            for item in &mut self.items {
                item.refresh_status();
            }
            self.update_shared_state();
            thread::sleep(Duration::from_millis(50));
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
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for item in &mut self.items {
            if reuse_skip.contains(&item.name) {
                item.shutdown_cmd = None;
            }
        }
    }

    fn handle_events(&mut self) -> io::Result<()> {
        match event::read()? {
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

    fn handle_key(&mut self, key: KeyEvent) {
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
                    self.switch_popup = None;
                    self.switch_worktree(&wt);
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
    /// a target: the instance scan excludes the current process.
    fn terminate_selected_branch(&mut self) {
        let branch = {
            let Some(popup) = &self.switch_popup else {
                return;
            };
            let matches = popup.matches();
            if matches.is_empty() {
                return;
            }
            matches[popup.selected.min(matches.len() - 1)]
                .branch
                .clone()
        };
        let Some(branch) = branch else {
            if let Some(popup) = &mut self.switch_popup {
                popup.status = Some("no branch to terminate (detached)".to_string());
            }
            return;
        };
        let project = self.ipc_state.project.clone();
        let script = self.ipc_state.script.clone();
        let instances = match project {
            Some(project) => {
                ipc::find_instances_with_status(&project, &script, Some(branch.as_str()))
                    .into_iter()
                    .map(|(pid, path, _)| (pid, path))
                    .collect::<Vec<_>>()
            }
            None => Vec::new(),
        };
        let terminated = ipc::terminate_instances(&instances);
        if let Some(popup) = &mut self.switch_popup {
            popup.status = Some(match terminated {
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
    fn switch_worktree(&mut self, wt: &Worktree) {
        // Resolve and validate the target worktree's config first, so a failure
        // leaves the current instance untouched.
        let config_path = if self.config_rel.is_absolute() {
            self.config_rel.clone()
        } else {
            wt.path.join(&self.config_rel)
        };
        let config = match crate::config::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                self.errors.push(format!("switch worktree: {e}"));
                return;
            }
        };
        let script_name = self.ipc_state.script.clone();
        let Some(script) = config.scripts.get(&script_name) else {
            self.errors.push(format!(
                "switch worktree: script '{}' not found in '{}'",
                script_name,
                config_path.display()
            ));
            return;
        };
        // Validate the dependency graph up front so a bad target script leaves
        // the current instance untouched instead of tearing it down first.
        let entries = script.service.clone().unwrap_or_default();
        if let Err(e) = runtime::resolve_dep_order(&entries) {
            self.errors.push(format!("switch worktree: {e}"));
            return;
        }
        let config_path = config_path.canonicalize().unwrap_or(config_path);
        let config_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        // Preserve shared services: mark reuse/share terminals handed off so
        // dropping the old set neither kills their processes nor runs their
        // shutdown_cmds. Adopted terminals transfer their live fd; borrowed
        // (assumed-up) terminals are marked handed off so the successor keeps
        // the resource.
        let mut adopted: HashMap<String, ipc::HandoffItem> = HashMap::new();
        for item in &mut self.items {
            if item.reused || item.shared {
                if let Some(handoff) = item.extract_handoff() {
                    adopted.insert(handoff.name.clone(), handoff);
                } else {
                    item.preserve_for_reuse();
                }
            }
        }

        // The project identity is the repo's git-common-dir, shared by every
        // worktree, so `ipc_state.project` stays unchanged across switches.

        // Tear down the old services and proxy now so their ports are free
        // before the new worktree's services start.
        self.proxy = None;
        self.items.clear();
        self.pending_services.clear();
        self.tabs = ClickTab::new(Vec::new(), self.sidebar_min, self.sidebar_max);
        self.proxy_tab_index = None;

        let built = match runtime::build(
            script,
            &script_name,
            &config_dir,
            self.ipc_state.project.clone(),
            self.save_logs,
            self.scrollback,
            None,
            &mut adopted,
        ) {
            Ok(b) => b,
            Err(e) => {
                self.errors.push(format!("switch worktree: {e}"));
                return;
            }
        };
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
        self.config_watcher_stop.store(true, Ordering::SeqCst);
        let config_watcher_stop = Arc::new(AtomicBool::new(false));
        self.config_watcher_stop = config_watcher_stop.clone();
        self.config_rx =
            config_watcher::spawn_config_watcher(self.config_path.clone(), config_watcher_stop);

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

    fn scroll_to(&mut self, target: usize) {
        let visible = self.content_height() as usize;
        let total = self.current_total_lines();
        let max = total.saturating_sub(visible);
        self.scroll_offset = target.min(max);
    }

    fn handle_scrollbar_click(&mut self, col: u16, row: u16) -> bool {
        let scrollbar_x = self.content_area.right().saturating_sub(2);
        let scrollbar_y = self.content_area.y + 1;
        let scrollbar_h = self.content_area.height.saturating_sub(2);

        if col != scrollbar_x || row < scrollbar_y || row >= scrollbar_y + scrollbar_h {
            return false;
        }

        let Some(offset) = self.scrollbar_row_to_offset(row) else {
            // No scrollbar is actually rendered (nothing to scroll): let the
            // click fall through so drag-select can start in this column.
            return false;
        };
        self.scroll_to(offset);
        true
    }

    fn handle_scrollbar_drag(&mut self, row: u16) {
        if let Some(offset) = self.scrollbar_row_to_offset(row) {
            self.scroll_to(offset);
        }
    }

    fn edge_content_pos(&self, col: u16, top: bool) -> Option<(usize, usize)> {
        let inner_x = self.content_area.x.saturating_add(1);
        let inner_w = self.content_area.width.saturating_sub(2);
        if col < inner_x || col >= inner_x.saturating_add(inner_w) {
            return None;
        }
        let col_idx = (col - inner_x) as usize;
        if let Some(layout) = self.active_layout() {
            // Wrap-aware edges: the first/last rendered row's content.
            let entry = if top {
                layout.iter().find_map(|e| *e)
            } else {
                layout.iter().rev().find_map(|e| *e)
            }?;
            let (line, col_off) = entry;
            if line >= self.current_total_lines() {
                return None;
            }
            return Some((line, col_off + col_idx));
        }
        let total = self.current_total_lines();
        let visible = self.content_height() as usize;
        let end = total.saturating_sub(self.scroll_offset);
        let start = end.saturating_sub(visible);
        let line = if top { start } else { end.saturating_sub(1) };
        if line >= total {
            None
        } else {
            Some((line, col_idx))
        }
    }

    /// Returns the physical-row layout of the last terminal-pane render, used
    /// to map mouse coordinates to exact content positions. The proxy pane has
    /// no layout (it does not wrap), so it falls back to the logical-line
    /// formula.
    fn active_layout(&self) -> Option<&[Option<(usize, usize)>]> {
        if self.is_proxy_tab() {
            None
        } else {
            Some(self.content_layout.as_slice())
        }
    }

    fn step_auto_scroll(&mut self) {
        let Some(scrolling_up) = self.auto_scrolling else {
            return;
        };
        let col = self.auto_scroll_col;
        if scrolling_up {
            self.scroll_to(self.scroll_offset.saturating_add(3));
            if let Some(pos) = self.edge_content_pos(col, true) {
                self.select_end = Some(pos);
            }
        } else {
            self.scroll_to(self.scroll_offset.saturating_sub(3));
            if let Some(pos) = self.edge_content_pos(col, false) {
                self.select_end = Some(pos);
            }
        }
    }

    fn handle_auto_scroll(&mut self) {
        if self.auto_scrolling.is_some() {
            self.step_auto_scroll();
        }
    }

    fn scrollbar_row_to_offset(&self, row: u16) -> Option<usize> {
        let scrollbar_y = self.content_area.y + 1;
        let scrollbar_h = self.content_area.height.saturating_sub(2);
        if scrollbar_h == 0 {
            return None;
        }

        let total = self.current_total_lines();
        let visible = self.content_height() as usize;
        let max_scroll = total.saturating_sub(visible);
        if max_scroll == 0 {
            return None;
        }

        let row_clamped = row.clamp(scrollbar_y, scrollbar_y + scrollbar_h - 1);
        let relative_y = (row_clamped - scrollbar_y) as usize;
        let target_position = relative_y.saturating_mul(max_scroll) / scrollbar_h as usize;
        Some(max_scroll.saturating_sub(target_position))
    }

    fn content_height(&self) -> u16 {
        self.content_area.height.saturating_sub(2)
    }

    fn current_total_lines(&self) -> usize {
        if self.is_proxy_tab() {
            let filter_lines = if matches!(self.mode, Mode::ProxyFilter) {
                1usize
            } else {
                0
            };
            match self.proxy {
                Some(ref p) => p.filtered_log_len(&self.proxy_filter) + 3 + filter_lines,
                None => 1,
            }
        } else {
            match self.service_tab_index().and_then(|i| self.items.get(i)) {
                Some(item) => item.total_lines(),
                None => 0,
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let sidebar_width = self.tabs.min_width();

        let main =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(sidebar_width)]).split(area);

        let content_area = main[0];
        let sidebar_area = main[1];

        self.check_pending();

        let proxy_offset = usize::from(self.proxy_tab_index.is_some());
        for (i, item) in self.items.iter_mut().enumerate() {
            item.refresh_status();
            if let Some(entry) = self.tabs.entries.get_mut(i + proxy_offset) {
                entry.stopped = item.stopped;
                entry.process_running = item.process_running;
                entry.pending = item.get_health_status() == HealthStatus::Pending;
                entry.health_status = item.get_health_status();
            }
        }

        self.update_shared_state();

        if let Some(ref mut p) = self.proxy
            && let Some(entry) = self
                .tabs
                .entries
                .iter_mut()
                .find(|e| e.kind == TabKind::Proxy)
        {
            entry.stopped = !p.is_running();
        }

        // Propagate unhealthy status through dependency chains
        let n = self.items.len();
        for _ in 0..n {
            let mut changed = false;
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
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        self.tabs.draw(frame, sidebar_area, &self.theme);

        self.content_area = content_area;

        let is_proxy = self.is_proxy_tab();
        let is_shell = self
            .service_tab_index()
            .and_then(|i| self.items.get(i))
            .map(|t| t.is_shell())
            .unwrap_or(false);
        let in_terminal_input = matches!(self.mode, Mode::TerminalInput);

        let instructions = render::draw_instructions(is_proxy, is_shell, in_terminal_input);

        let block = Block::bordered()
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        if is_proxy {
            self.content_layout.clear();
            render::draw_proxy_content(
                frame,
                content_area,
                block,
                &self.proxy,
                self.scroll_offset,
                &self.proxy_filter,
                matches!(self.mode, Mode::ProxyFilter),
                &self.theme,
            );
        } else {
            let total_lines = self.current_total_lines();
            let tab_index = self.service_tab_index().unwrap_or(self.tabs.index);
            self.content_layout = render::draw_terminal_content(
                frame,
                content_area,
                block,
                &mut self.items,
                tab_index,
                self.scroll_offset,
                self.select_start,
                self.select_end,
                in_terminal_input,
                total_lines,
                &self.theme,
            );
        }

        if self.show_help {
            let help_text = vec![
                Line::from(vec![Span::raw("  q/Ctrl+q   Quit                ")]),
                Line::from(vec![Span::raw("  j/Right    Next tab            ")]),
                Line::from(vec![Span::raw("  k/Left     Previous tab        ")]),
                Line::from(vec![Span::raw("  i          Terminal input mode ")]),
                Line::from(vec![Span::raw("  Esc        Exit input mode     ")]),
                Line::from(vec![Span::raw("  R          Restart service     ")]),
                Line::from(vec![Span::raw("  t/Ctrl+t   New shell tab       ")]),
                Line::from(vec![Span::raw("  d          Close shell tab     ")]),
                Line::from(vec![Span::raw("  s          Switch worktree     ")]),
                Line::from(vec![Span::raw("  g/Home     Scroll to top       ")]),
                Line::from(vec![Span::raw("  G/End      Scroll to bottom    ")]),
                Line::from(vec![Span::raw("  Up/Down    Scroll output       ")]),
                Line::from(vec![Span::raw("  PgUp/Dn    Scroll by page      ")]),
                Line::from(vec![Span::raw("  ?          Toggle help         ")]),
            ];

            let overlay_width = 40u16.min(area.width.saturating_sub(4));
            let overlay_height = help_text.len() as u16 + 2;
            let overlay_x = (area.width.saturating_sub(overlay_width)) / 2;
            let overlay_y = (area.height.saturating_sub(overlay_height)) / 2;

            let overlay_area = Rect {
                x: overlay_x,
                y: overlay_y,
                width: overlay_width,
                height: overlay_height,
            };

            let block = Block::bordered().title(" Help ").style(Style::default());
            let help = Paragraph::new(Text::from(help_text))
                .block(block)
                .alignment(Alignment::Left);

            frame.render_widget(Clear, overlay_area);
            frame.render_widget(help, overlay_area);
        }

        if let Some(popup) = &self.switch_popup {
            let config_dir = self.config_path.parent().unwrap_or_else(|| Path::new("."));
            let matches = popup.matches();

            let mut lines = vec![
                Line::from(vec![
                    Span::raw(" filter: "),
                    Span::styled(format!("{}▌", popup.filter), Style::default().bold()),
                ]),
                Line::from(""),
            ];
            for (i, wt) in matches.iter().enumerate() {
                let is_current = wt.contains(config_dir);
                let is_running = popup
                    .running
                    .iter()
                    .any(|b| wt.branch.as_deref() == Some(b.as_str()));
                let (prefix, label_style) = if i == popup.selected {
                    (" >", Style::default().fg(self.theme.highlight).bold())
                } else {
                    ("  ", Style::default())
                };
                // The current-worktree `*` and the live-branch green `*` are
                // distinct spans, so a selected/current running branch keeps
                // both readable.
                let mut spans = vec![Span::styled(
                    format!("{prefix} {}", wt.label()),
                    label_style,
                )];
                if is_current {
                    spans.push(Span::styled(
                        " *",
                        Style::default().fg(Color::Rgb(255, 176, 0)).bold(),
                    ));
                }
                if is_running {
                    spans.push(Span::styled(
                        " *",
                        Style::default().fg(Color::Blue).bold(),
                    ));
                }
                spans.push(Span::styled(
                    format!("  {}", wt.path.display()),
                    Style::default().dim(),
                ));
                lines.push(Line::from(spans));
            }
            if matches.is_empty() {
                lines.push(Line::from("  (no matching worktrees)"));
            }
            lines.push(Line::from(""));
            if let Some(status) = &popup.status {
                lines.push(Line::from(Span::styled(
                    format!(" {status}"),
                    Style::default().fg(Color::Yellow),
                )));
            }
            lines.push(Line::from(Span::styled(
                " f search   d terminate ",
                Style::default().dim(),
            )));

            let overlay_width = 64u16.min(area.width.saturating_sub(4));
            let overlay_height = ((lines.len() as u16) + 2).min(area.height);
            let overlay_x = (area.width.saturating_sub(overlay_width)) / 2;
            let overlay_y = (area.height.saturating_sub(overlay_height)) / 2;
            let overlay_area = Rect {
                x: overlay_x,
                y: overlay_y,
                width: overlay_width,
                height: overlay_height,
            };

            let block = Block::bordered().title(" Switch worktree ");
            let widget = Paragraph::new(Text::from(lines))
                .block(block)
                .alignment(Alignment::Left);

            frame.render_widget(Clear, overlay_area);
            frame.render_widget(widget, overlay_area);
        }
    }

    fn update_shared_state(&self) {
        let mut services = self
            .ipc_state
            .services
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        services.clear();
        for item in &self.items {
            services.push(ipc::ServiceStatus {
                name: item.name.clone(),
                running: !item.stopped && item.process_running,
                health: format!("{:?}", item.get_health_status()).to_lowercase(),
            });
        }
        let mut proxy = self
            .ipc_state
            .proxy
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *proxy = self.proxy.as_ref().map(|p| ipc::ProxyStatus {
            running: p.is_running(),
            port: p.port,
        });
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
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(req) = req else {
            return;
        };
        let resp = self.execute_service_action(&req);
        *self
            .ipc_state
            .control_result
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(resp);
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
            ipc_state: Arc::new(IpcState::new("test".to_string(), None, None)),
            proxy_tab_index,
            sidebar_min: 10,
            sidebar_max: 30,
            scrollbar_dragging: false,
            auto_scrolling: None,
            auto_scroll_col: 0,
            content_layout: Vec::new(),
            switch_popup: None,
        }
    }

    #[test]
    fn test_is_proxy_tab_false() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        assert!(!app.is_proxy_tab());
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
            Some("no branch to terminate (detached)")
        );
    }
}
