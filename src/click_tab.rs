use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::Style,
    widgets::{List, ListItem, ListState},
    text::{Line, Span},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TabKind {
    Service,
    Terminal,
}

#[derive(Debug, Clone)]
pub struct TabEntry {
    pub name: String,
    pub kind: TabKind,
    pub stopped: bool,
}

impl TabEntry {
    fn display_name(&self) -> String {
        match self.kind {
            TabKind::Terminal => format!("$ {}", self.name),
            TabKind::Service => {
                if self.stopped {
                    format!("{} [stopped]", self.name)
                } else {
                    self.name.clone()
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ClickTab {
    pub entries: Vec<TabEntry>,
    pub index: usize,
    area: Option<Rect>,
    list_state: ListState,
}

impl Default for ClickTab {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            index: 0,
            area: None,
            list_state: ListState::default(),
        }
    }
}

impl ClickTab {
    pub fn new(names: Vec<String>) -> Self {
        let entries = names
            .into_iter()
            .map(|name| TabEntry { name, kind: TabKind::Service, stopped: false })
            .collect();
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self { entries, index: 0, area: None, list_state }
    }

    pub fn add(&mut self, name: String, kind: TabKind) {
        self.entries.push(TabEntry { name, kind, stopped: false });
        self.index = self.entries.len() - 1;
        self.list_state.select(Some(self.index));
    }

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

    pub fn min_width(&self) -> u16 {
        let max_name = self
            .entries
            .iter()
            .map(|e| e.display_name().len())
            .max()
            .unwrap_or(0);
        (max_name + 5) as u16
    }

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

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = Some(area);

        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| {
                let name = e.display_name();
                let status = if e.stopped { "○" } else { "●" };
                let line = Line::from(Span::raw(format!("{} {}", status, name)));
                let item = ListItem::new(line);
                match e.kind {
                    TabKind::Terminal => item.style(Style::default().green()),
                    TabKind::Service => {
                        if e.stopped {
                            item.style(Style::default().red().dim())
                        } else {
                            item.style(Style::default())
                        }
                    }
                }
            })
            .collect();

        let list = List::new(items)
            .highlight_style(Style::default().magenta().on_black().bold())
            .highlight_symbol("▸ ");

        self.list_state.select(Some(self.index));
        frame.render_stateful_widget(list, area, &mut self.list_state);
    }
}
