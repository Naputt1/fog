use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Style},
    symbols,
    widgets::{Tabs, Widget},
};

#[derive(Debug, Default)]
pub struct ClickTab {
    pub tabs: Vec<String>,
    pub index: usize,
    area: Option<Rect>,
}

impl ClickTab {
    pub fn new(tabs: Vec<String>) -> Self {
        Self {
            tabs,
            index: 0,
            area: None,
        }
    }

    pub fn click(&mut self, x: u16, y: u16) {
        let tab_area = self.area.expect("missing tab area");

        if !tab_area.contains(Position { x, y }) {
            return;
        }

        let mut padding: usize = 0;
        for (i, name) in self.tabs.iter().enumerate() {
            if x as usize <= padding + name.len() {
                self.index = i;
                return;
            }

            padding += name.len() + 2;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = Some(area);
        frame.render_widget(&*self, area);
    }
}

impl Widget for &ClickTab {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let tabs = Tabs::new(self.tabs.clone())
            .style(Color::White)
            .highlight_style(Style::default().magenta().on_black().bold())
            .select(Some(self.index))
            .divider(symbols::DOT)
            .padding(" ", " ");

        tabs.render(area, buf);
    }
}
