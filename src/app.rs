use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::Stylize,
    symbols::border,
    text::{Line, Text},
    widgets::{Block, Paragraph, Widget},
};
use std::{io, path::Path, time::Duration};

use crate::{click_tab::ClickTab, service::Service};

#[derive(Debug, Default)]
pub struct App {
    services: Vec<Service>,
    exit: bool,
    tabs: ClickTab,
}

impl App {
    pub fn new(services: Vec<Service>) -> Self {
        let mut names = Vec::new();
        for service in services.iter() {
            let path = Path::new(&service.path);
            let dir: String = path.file_name().unwrap().to_string_lossy().into_owned();
            names.push(dir);
        }

        Self {
            services,
            exit: false,
            tabs: ClickTab::new(names),
        }
    }

    /// runs the application's main loop until the user quits
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        for service in self.services.iter_mut() {
            match service.run() {
                Ok(_) => {}
                Err(e) => println!("error: {}", e),
            }
        }

        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))? {
                self.handle_events()?;
            }
        }
        Ok(())
    }

    fn handle_events(&mut self) -> io::Result<()> {
        match event::read()? {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                self.handle_key_event(key_event)
            }
            Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_)) => {
                let x = mouse.column;
                let y = mouse.row;

                self.tabs.click(x, y);
            }
            _ => {}
        };
        Ok(())
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char('q') | KeyCode::Esc => self.exit(),
            KeyCode::Char('h') => self.tabs.index = (self.tabs.index + 1) % self.services.len(),
            KeyCode::Char('l') => {
                self.tabs.index = (self.tabs.index + self.services.len() - 1) % self.services.len()
            }
            _ => {}
        }
    }

    fn exit(&mut self) {
        self.exit = true;
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(frame.area());

        // let mut names = Vec::new();
        // for service in self.services.iter() {
        //     let path = Path::new(&service.path);
        //     let dir: String = path.file_name().unwrap().to_string_lossy().into_owned();
        //     names.push(dir);
        // }

        // let tabs = Tabs::new(names)
        //     .style(Color::White)
        //     .highlight_style(Style::default().magenta().on_black().bold())
        //     .select(Some(self.tab.into()))
        //     .divider(symbols::DOT)
        //     .padding(" ", " ");
        // self.tab_area = Some(chunks[0]);
        // frame.render_widget(tabs, chunks[0]);
        self.tabs.draw(frame, chunks[0]);

        frame.render_widget(self, chunks[1]);
    }
}

impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let instructions = Line::from(vec![
            " Decrement ".into(),
            "<Left>".blue().bold(),
            " Increment ".into(),
            "<Right>".blue().bold(),
            " Quit ".into(),
            "<Q> ".blue().bold(),
        ]);
        let block = Block::bordered()
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        let visible_height = area.height.saturating_sub(2);

        let service = self.services.get(self.tabs.index).expect("missing service");
        let lines: Vec<Line> = service
            .tail(visible_height as usize)
            .into_iter()
            .map(Line::from)
            .collect();

        Paragraph::new(Text::from(lines))
            .block(block)
            .render(area, buf);
    }
}
