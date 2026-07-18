use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Style, Stylize},
    symbols,
    text::Line,
    widgets::Tabs,
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

#[derive(Debug, Default)]
pub struct ClickTab {
    pub entries: Vec<TabEntry>,
    pub index: usize,
    area: Option<Rect>,
}

impl ClickTab {
    pub fn new(names: Vec<String>) -> Self {
        let entries = names
            .into_iter()
            .map(|name| TabEntry { name, kind: TabKind::Service, stopped: false })
            .collect();
        Self { entries, index: 0, area: None }
    }

    pub fn add(&mut self, name: String, kind: TabKind) {
        self.entries.push(TabEntry { name, kind, stopped: false });
        self.index = self.entries.len() - 1;
    }

    pub fn remove(&mut self, idx: usize) {
        if idx >= self.entries.len() {
            return;
        }
        self.entries.remove(idx);
        if self.index >= self.entries.len() && !self.entries.is_empty() {
            self.index = self.entries.len() - 1;
        }
    }

    pub fn click(&mut self, x: u16, y: u16) {
        let tab_area = self.area.expect("missing tab area");

        if !tab_area.contains(Position { x, y }) {
            return;
        }

        let mut padding: usize = 0;
        for (i, entry) in self.entries.iter().enumerate() {
            let name_len = entry.display_name().len();
            if x as usize <= padding + name_len {
                self.index = i;
                return;
            }
            padding += name_len + 2;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = Some(area);
        let display_names: Vec<Line> = self
            .entries
            .iter()
            .map(|e| {
                let name = e.display_name();
                match e.kind {
                    TabKind::Terminal => Line::from(name).green(),
                    TabKind::Service => {
                        if e.stopped {
                            Line::from(name).red().dim()
                        } else {
                            Line::from(name)
                        }
                    }
                }
            })
            .collect();

        let tabs = Tabs::new(display_names)
            .style(Color::White)
            .highlight_style(Style::default().magenta().on_black().bold())
            .select(Some(self.index))
            .divider(symbols::DOT)
            .padding(" ", " ");

        frame.render_widget(tabs, area);
    }
}
