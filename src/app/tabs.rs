use super::{App, Mode, PendingService};
use crate::click_tab::{ClickTab, TabKind};
use crate::selection;
use crate::terminal::Terminal;

impl App {
    /// Builds the sidebar tabs and proxy-tab index for a set of items.
    pub(crate) fn build_tabs(
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

    pub(crate) fn is_proxy_tab(&self) -> bool {
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
    pub(crate) fn item_index_for_tab(&self, tab_idx: usize) -> Option<usize> {
        match self.proxy_tab_index {
            Some(p) if tab_idx == p => None,
            Some(p) if tab_idx > p => Some(tab_idx - 1),
            _ => Some(tab_idx),
        }
    }

    /// Maps the currently selected tab to an index into `self.items`.
    pub(crate) fn service_tab_index(&self) -> Option<usize> {
        self.item_index_for_tab(self.tabs.index)
    }
}

impl App {
    pub(crate) fn on_tab_switch(&mut self) {
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

    pub(crate) fn is_shell_tab(&self, idx: usize) -> bool {
        self.item_index_for_tab(idx)
            .and_then(|i| self.items.get(i))
            .map(|t| t.is_shell())
            .unwrap_or(false)
    }

    pub(crate) fn new_terminal(&mut self) {
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
            Err(e) => self.note_error(format!("failed to create terminal: {}", e)),
        }
    }

    pub(crate) fn close_tab(&mut self) {
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

    pub(crate) fn restart_current(&mut self) {
        if self.is_proxy_tab() {
            if let Some(ref mut p) = self.proxy {
                p.restart();
            }
            return;
        }
        let mut restart_error = None;
        if let Some(item) = self.service_tab_index().and_then(|i| self.items.get_mut(i))
            && !item.is_shell()
        {
            if let Err(e) = item.restart() {
                restart_error = Some(format!("restart error: {}", e));
            }
            if let Some(e) = self.tabs.entries.get_mut(self.tabs.index) {
                e.stopped = false;
            }
        }
        if let Some(e) = restart_error {
            self.note_error(e);
        }
    }
}
