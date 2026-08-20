use super::{App, PendingService};
use crate::click_tab::{ClickTab, TabKind};
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
