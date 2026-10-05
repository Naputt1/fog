use crate::terminal::HealthStatus;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Margin, Position, Rect},
    style::Style,
    symbols::border,
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState},
};

/// The type of tab entry in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TabKind {
    /// A service process tab.
    Service,
    /// An interactive shell terminal tab.
    Terminal,
    /// The reverse proxy log tab.
    Proxy,
}

/// A single tab entry in the sidebar.
#[derive(Debug, Clone)]
pub struct TabEntry {
    /// Display name of the tab.
    pub name: String,
    /// The kind of tab this entry represents.
    pub kind: TabKind,
    /// Whether the underlying process has stopped.
    pub stopped: bool,
    /// Whether a command/process is actively running in this terminal.
    pub process_running: bool,
    /// Whether the service is waiting for dependencies.
    pub pending: bool,
    /// Health check status.
    pub health_status: HealthStatus,
}

impl TabEntry {
    fn display_name(&self) -> String {
        match self.kind {
            TabKind::Terminal => format!("$ {}", self.name),
            TabKind::Service => self.name.clone(),
            TabKind::Proxy => format!("▶ {}", self.name),
        }
    }
}

/// A clickable tab bar rendered as a sidebar list.
#[derive(Debug)]
pub struct ClickTab {
    /// The list of tab entries.
    pub entries: Vec<TabEntry>,
    /// The currently selected tab index.
    pub index: usize,
    area: Option<Rect>,
    list_state: ListState,
    /// Minimum sidebar width in columns.
    pub min_sidebar_width: u16,
    /// Maximum sidebar width in columns.
    pub max_sidebar_width: u16,
}

impl ClickTab {
    /// Creates a new [`ClickTab`] from a list of service tab names.
    ///
    /// All entries start with kind [`TabKind::Service`] and index 0 is selected.
    ///
    /// # Arguments
    /// * `names` - The display names for the initial service tabs.
    /// * `min_width` - Minimum sidebar width in columns.
    /// * `max_width` - Maximum sidebar width in columns.
    pub fn new(names: Vec<String>, min_width: u16, max_width: u16) -> Self {
        let entries = names
            .into_iter()
            .map(|name| TabEntry {
                name,
                kind: TabKind::Service,
                stopped: false,
                process_running: false,
                pending: false,
                health_status: HealthStatus::Unknown,
            })
            .collect();
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            entries,
            index: 0,
            area: None,
            list_state,
            min_sidebar_width: min_width,
            max_sidebar_width: max_width,
        }
    }

    /// Adds a new tab entry and selects it.
    ///
    /// # Arguments
    /// * `name` - The display name for the new tab.
    /// * `kind` - The kind of tab to add.
    pub fn add(&mut self, name: String, kind: TabKind) {
        self.entries.push(TabEntry {
            name,
            kind,
            stopped: false,
            process_running: false,
            pending: false,
            health_status: HealthStatus::Unknown,
        });
        self.index = self.entries.len() - 1;
        self.list_state.select(Some(self.index));
    }

    /// Inserts a new tab entry at the given index and selects it.
    ///
    /// # Arguments
    /// * `index` - The position to insert at.
    /// * `name` - The display name for the new tab.
    /// * `kind` - The kind of tab to add.
    pub fn insert_at(&mut self, index: usize, name: String, kind: TabKind) {
        self.entries.insert(
            index,
            TabEntry {
                name,
                kind,
                stopped: false,
                process_running: false,
                pending: false,
                health_status: HealthStatus::Unknown,
            },
        );
        self.index = index;
        self.list_state.select(Some(self.index));
    }

    /// Removes the tab at the given index.
    ///
    /// If the currently selected tab is removed, the selection adjusts to the last tab.
    ///
    /// # Arguments
    /// * `idx` - The index of the tab to remove.
    pub fn remove(&mut self, idx: usize) {
        if idx >= self.entries.len() {
            return;
        }
        self.entries.remove(idx);
        if self.index >= self.entries.len() && !self.entries.is_empty() {
            self.index = self.entries.len() - 1;
        }
        self.list_state.select(Some(self.index));
    }

    /// Returns the minimum sidebar width needed to display all tab names.
    pub fn min_width(&self) -> u16 {
        let max_name = self
            .entries
            .iter()
            .map(|e| e.display_name().len())
            .max()
            .unwrap_or(0);
        let computed = (max_name + 5) as u16;
        let lo = self.min_sidebar_width.min(self.max_sidebar_width);
        let hi = self.min_sidebar_width.max(self.max_sidebar_width);
        computed.clamp(lo, hi)
    }

    /// Handles a mouse click to select a tab.
    ///
    /// # Arguments
    /// * `x` - The screen x-coordinate of the click.
    /// * `y` - The screen y-coordinate of the click.
    ///
    /// # Panics
    /// Panics if the sidebar area has not been set by a prior call to [`draw`](Self::draw).
    pub fn click(&mut self, x: u16, y: u16) {
        let sidebar_area = self.area.expect("missing sidebar area");
        if !sidebar_area.contains(Position { x, y }) {
            return;
        }
        let row = y.saturating_sub(sidebar_area.y);
        if (row as usize) < self.entries.len() {
            self.index = row as usize;
            self.list_state.select(Some(self.index));
        }
    }

    /// Renders the tab sidebar as a titled, bordered panel.
    ///
    /// The selected row is filled with `selection_bg`; rows show a status dot
    /// followed by the tab name, and non-healthy states are labelled.
    ///
    /// # Arguments
    /// * `frame` - The ratatui frame to render into.
    /// * `area` - The rectangular area for the sidebar.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::bordered()
            .border_set(border::ROUNDED)
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.surface))
            .title_top(
                Line::from(Span::styled(
                    "─┤ services ├",
                    Style::default().fg(theme.title).bold(),
                ))
                .alignment(Alignment::Left),
            );
        frame.render_widget(block, area);

        // Inside the border; `inner` is the list/click region so hit-testing
        // maps 1:1.
        let inner = area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        self.area = Some(inner);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let items: Vec<ListItem> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let name = e.display_name();
                let is_selected = i == self.index;

                let (dot, dot_style) = if e.pending {
                    ("◌", Style::default().fg(theme.status_300).bold())
                } else if e.stopped {
                    ("○", Style::default().fg(theme.stopped))
                } else if e.kind == TabKind::Proxy {
                    ("●", Style::default().fg(theme.proxy))
                } else if e.health_status == HealthStatus::Unhealthy {
                    ("●", Style::default().fg(theme.stopped).bold())
                } else if e.health_status == HealthStatus::Starting {
                    ("●", Style::default().fg(theme.status_300))
                } else if e.process_running {
                    ("●", Style::default().fg(theme.status_200))
                } else {
                    ("●", Style::default().fg(theme.border))
                };

                let name_style = if is_selected {
                    Style::default().fg(theme.selection_fg).bold()
                } else {
                    match e.kind {
                        TabKind::Terminal => Style::default().fg(theme.terminal),
                        TabKind::Service if e.stopped => Style::default().fg(theme.stopped).dim(),
                        TabKind::Proxy if e.stopped => Style::default().fg(theme.stopped).dim(),
                        TabKind::Proxy => Style::default().fg(theme.proxy),
                        _ => Style::default().fg(theme.text),
                    }
                };

                // One column of padding inside the border.
                let mut spans = vec![
                    Span::raw(" "),
                    Span::styled(dot, dot_style),
                    Span::styled(format!(" {}", name), name_style),
                ];
                if let Some(label) = health_label(e) {
                    let label_style = if is_selected {
                        Style::default().fg(theme.selection_fg).dim()
                    } else {
                        Style::default().fg(theme.text_muted)
                    };
                    spans.push(Span::styled(format!("  {label}"), label_style));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let list = List::new(items)
            .highlight_style(Style::default().bg(theme.selection_bg))
            .highlight_symbol("");

        self.list_state.select(Some(self.index));
        frame.render_stateful_widget(list, inner, &mut self.list_state);
    }
}

/// A short label for a row that is not simply "running healthy", so problems
/// stand out at a glance. Healthy rows stay unlabelled.
fn health_label(e: &TabEntry) -> Option<&'static str> {
    if e.pending {
        return Some("waiting");
    }
    if e.stopped {
        return Some("stopped");
    }
    match e.health_status {
        HealthStatus::Unhealthy => Some("unhealthy"),
        HealthStatus::Starting => Some("starting"),
        HealthStatus::Pending => Some("pending"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_creates_entries() {
        let names = vec!["tab1".into(), "tab2".into(), "tab3".into()];
        let ct = ClickTab::new(names, 12, 30);
        assert_eq!(ct.entries.len(), 3);
        assert_eq!(ct.index, 0);
        for e in &ct.entries {
            assert_eq!(e.kind, TabKind::Service);
        }
    }

    #[test]
    fn test_new_empty() {
        let ct = ClickTab::new(vec![], 12, 30);
        assert_eq!(ct.entries.len(), 0);
        assert_eq!(ct.index, 0);
    }

    #[test]
    fn test_add_entry() {
        let mut ct = ClickTab::new(vec!["first".into()], 12, 30);
        ct.add("second".into(), TabKind::Terminal);
        assert_eq!(ct.entries.len(), 2);
        assert_eq!(ct.index, 1);
        assert_eq!(ct.entries[1].kind, TabKind::Terminal);
        assert_eq!(ct.entries[1].name, "second");
    }

    #[test]
    fn test_add_updates_index() {
        let mut ct = ClickTab::new(vec!["a".into(), "b".into()], 12, 30);
        ct.add("c".into(), TabKind::Proxy);
        assert_eq!(ct.index, 2);
    }

    #[test]
    fn test_remove_entry() {
        let mut ct = ClickTab::new(vec!["a".into(), "b".into(), "c".into()], 12, 30);
        ct.remove(0);
        assert_eq!(ct.entries.len(), 2);
        assert_eq!(ct.entries[0].name, "b");
        assert_eq!(ct.entries[1].name, "c");
    }

    #[test]
    fn test_remove_adjusts_index() {
        let mut ct = ClickTab::new(vec!["a".into(), "b".into(), "c".into()], 12, 30);
        ct.index = 2;
        ct.remove(2);
        assert_eq!(ct.index, 1);
    }

    #[test]
    fn test_remove_out_of_bounds() {
        let mut ct = ClickTab::new(vec!["a".into()], 12, 30);
        ct.remove(5);
        assert_eq!(ct.entries.len(), 1);
    }

    #[test]
    fn test_remove_last_adjusts_index() {
        let mut ct = ClickTab::new(vec!["a".into(), "b".into()], 12, 30);
        ct.index = 1;
        ct.remove(1);
        assert_eq!(ct.index, 0);
        assert_eq!(ct.entries.len(), 1);
    }

    #[test]
    fn test_min_width() {
        let ct = ClickTab::new(vec!["short".into(), "very long name".into()], 12, 30);
        assert_eq!(ct.min_width(), 19); // max_name (14) + 5 = 19
    }

    #[test]
    fn test_min_width_empty() {
        let ct = ClickTab::new(vec![], 12, 30);
        assert_eq!(ct.min_width(), 12); // clamped to min
    }

    #[test]
    fn test_min_width_single() {
        let ct = ClickTab::new(vec!["hi".into()], 12, 30);
        assert_eq!(ct.min_width(), 12); // 2 + 5 = 7, clamped to min
    }

    #[test]
    fn test_display_name_service() {
        let e = TabEntry {
            name: "myservice".into(),
            kind: TabKind::Service,
            stopped: false,
            process_running: false,
            pending: false,
            health_status: HealthStatus::Unknown,
        };
        assert_eq!(e.display_name(), "myservice");
    }

    #[test]
    fn test_display_name_terminal() {
        let e = TabEntry {
            name: "bash".into(),
            kind: TabKind::Terminal,
            stopped: false,
            process_running: false,
            pending: false,
            health_status: HealthStatus::Unknown,
        };
        assert_eq!(e.display_name(), "$ bash");
    }

    #[test]
    fn test_display_name_proxy() {
        let e = TabEntry {
            name: "proxy".into(),
            kind: TabKind::Proxy,
            stopped: false,
            process_running: false,
            pending: false,
            health_status: HealthStatus::Unknown,
        };
        assert_eq!(e.display_name(), "▶ proxy");
    }

    #[test]
    fn test_click_hit_inside() {
        let mut ct = ClickTab::new(vec!["a".into(), "b".into(), "c".into()], 12, 30);
        ct.area = Some(Rect {
            x: 80,
            y: 0,
            width: 20,
            height: 10,
        });
        ct.click(80, 1);
        assert_eq!(ct.index, 1);
    }

    #[test]
    fn test_click_miss_outside() {
        let mut ct = ClickTab::new(vec!["a".into(), "b".into()], 12, 30);
        ct.area = Some(Rect {
            x: 80,
            y: 0,
            width: 20,
            height: 10,
        });
        ct.index = 1;
        ct.click(10, 10);
        assert_eq!(ct.index, 1);
    }

    #[test]
    fn test_click_hit_different_row() {
        let mut ct = ClickTab::new(vec!["a".into(), "b".into(), "c".into()], 12, 30);
        ct.area = Some(Rect {
            x: 80,
            y: 0,
            width: 20,
            height: 10,
        });
        ct.click(80, 2);
        assert_eq!(ct.index, 2);
    }

    #[test]
    fn test_click_beyond_entries() {
        let mut ct = ClickTab::new(vec!["a".into(), "b".into()], 12, 30);
        ct.area = Some(Rect {
            x: 80,
            y: 0,
            width: 20,
            height: 10,
        });
        ct.click(80, 5);
        assert_eq!(ct.index, 0);
    }

    #[test]
    #[should_panic(expected = "missing sidebar area")]
    fn test_click_without_area_set() {
        let mut ct = ClickTab::new(vec!["a".into()], 12, 30);
        ct.click(0, 0);
    }
}
