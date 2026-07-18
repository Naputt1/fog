use crate::click_tab::{ClickTab, TabKind};
use crate::service::Service;
use crate::terminal::TerminalSession;
use ansi_to_tui::IntoText;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin, Position},
    style::{Color, Style, Stylize},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Paragraph},
};
use std::{io, path::Path, time::Duration};

enum TabItem {
    Service(Service),
    Terminal(TerminalSession),
}

enum Mode {
    Normal,
    TerminalInput,
}

pub struct App {
    items: Vec<TabItem>,
    tabs: ClickTab,
    mode: Mode,
    scroll_offset: usize,
    command_buf: String,
    command_mode: bool,
    exit: bool,
}

impl App {
    pub fn new(services: Vec<Service>) -> Self {
        let mut names = Vec::new();
        for service in services.iter() {
            let dir = Path::new(&service.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            names.push(dir);
        }

        let items = services.into_iter().map(TabItem::Service).collect();

        Self {
            items,
            tabs: ClickTab::new(names),
            mode: Mode::Normal,
            scroll_offset: 0,
            command_buf: String::new(),
            command_mode: false,
            exit: false,
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        for item in self.items.iter_mut() {
            if let TabItem::Service(s) = item {
                if let Err(e) = s.run() {
                    eprintln!("error: {}", e);
                }
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
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_)) => {
                let idx_before = self.tabs.index;
                self.tabs.click(mouse.column, mouse.row);
                if self.tabs.index != idx_before {
                    self.on_tab_switch();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_tab_switch(&mut self) {
        self.scroll_offset = 0;
        if self.is_terminal_tab(self.tabs.index) {
            self.mode = Mode::TerminalInput;
        } else {
            self.mode = Mode::Normal;
        }
    }

    fn is_terminal_tab(&self, idx: usize) -> bool {
        matches!(self.items.get(idx), Some(TabItem::Terminal(_)))
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('q') => {
                    self.exit = true;
                    return;
                }
                KeyCode::Char('n') => {
                    let prev = self.tabs.index;
                    self.tabs.index = (self.tabs.index + 1) % self.items.len();
                    if prev != self.tabs.index {
                        self.on_tab_switch();
                    }
                    return;
                }
                KeyCode::Char('p') => {
                    let prev = self.tabs.index;
                    self.tabs.index = (self.tabs.index + self.items.len() - 1) % self.items.len();
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

        if self.command_mode {
            self.handle_command_key(key);
            return;
        }

        match self.mode {
            Mode::TerminalInput => self.handle_terminal_key(key),
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.command_mode = false;
                self.command_buf.clear();
            }
            KeyCode::Enter => {
                let cmd = std::mem::take(&mut self.command_buf);
                self.command_mode = false;
                self.execute_command(&cmd);
            }
            KeyCode::Backspace => {
                self.command_buf.pop();
            }
            KeyCode::Char(c) => {
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT {
                    self.command_buf.push(c);
                }
            }
            _ => {}
        }
    }

    fn execute_command(&mut self, cmd: &str) {
        match cmd {
            "q" | "quit" => self.exit = true,
            "kill" => self.kill_current(),
            "restart" => self.restart_current(),
            _ => {}
        }
    }

    fn handle_terminal_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return;
        }

        if let Some(TabItem::Terminal(t)) = self.items.get_mut(self.tabs.index) {
            if let Some(bytes) = key_to_bytes(key) {
                t.write(&bytes);
            }
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.exit = true,
            KeyCode::Esc => {}
            KeyCode::Char('h') | KeyCode::Right => {
                let prev = self.tabs.index;
                self.tabs.index = (self.tabs.index + 1) % self.items.len();
                if prev != self.tabs.index {
                    self.on_tab_switch();
                }
            }
            KeyCode::Char('l') | KeyCode::Left => {
                let prev = self.tabs.index;
                self.tabs.index = (self.tabs.index + self.items.len() - 1) % self.items.len();
                if prev != self.tabs.index {
                    self.on_tab_switch();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll_to(self.scroll_offset.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::PageUp => {
                let h = self.content_height();
                self.scroll_offset = self.scroll_offset.saturating_add(h as usize);
            }
            KeyCode::PageDown => {
                let h = self.content_height();
                self.scroll_to(self.scroll_offset.saturating_sub(h as usize));
            }
            KeyCode::Home | KeyCode::Char('g') => {
                let total = self.current_total_lines();
                self.scroll_offset = total.saturating_sub(1);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll_offset = 0;
            }
            KeyCode::Char('i') => {
                if self.is_terminal_tab(self.tabs.index) {
                    self.mode = Mode::TerminalInput;
                }
            }
            KeyCode::Char('x') => self.kill_current(),
            KeyCode::Char('R') => self.restart_current(),
            KeyCode::Char('t') => self.new_terminal(),
            KeyCode::Char('d') => self.close_tab(),
            KeyCode::Char(':') => {
                self.command_mode = true;
                self.command_buf.clear();
            }
            _ => {}
        }
    }

    fn kill_current(&mut self) {
        if !self.is_terminal_tab(self.tabs.index) {
            if let Some(TabItem::Service(s)) = self.items.get_mut(self.tabs.index) {
                s.kill();
                if let Some(e) = self.tabs.entries.get_mut(self.tabs.index) {
                    e.stopped = true;
                }
            }
        }
    }

    fn restart_current(&mut self) {
        if !self.is_terminal_tab(self.tabs.index) {
            if let Some(TabItem::Service(s)) = self.items.get_mut(self.tabs.index) {
                if let Err(e) = s.restart() {
                    eprintln!("restart error: {}", e);
                }
                if let Some(e) = self.tabs.entries.get_mut(self.tabs.index) {
                    e.stopped = false;
                }
            }
        }
    }

    fn new_terminal(&mut self) {
        match TerminalSession::new() {
            Ok(term) => {
                let id = self.items.len();
                self.items.push(TabItem::Terminal(term));
                self.tabs.add("bash".to_string(), TabKind::Terminal);
                self.tabs.index = id;
                self.scroll_offset = 0;
                self.mode = Mode::TerminalInput;
            }
            Err(e) => eprintln!("failed to create terminal: {}", e),
        }
    }

    fn close_tab(&mut self) {
        if self.is_terminal_tab(self.tabs.index) && self.items.len() > 1 {
            let idx = self.tabs.index;
            self.items.remove(idx);
            self.tabs.remove(idx);
            self.scroll_offset = 0;
            if self.tabs.index < self.items.len() && self.is_terminal_tab(self.tabs.index) {
                self.mode = Mode::TerminalInput;
            } else {
                self.mode = Mode::Normal;
            }
        }
    }

    fn scroll_to(&mut self, target: usize) {
        let max = self
            .current_total_lines()
            .saturating_sub(self.content_height() as usize + 1);
        self.scroll_offset = target.min(max);
    }

    fn content_height(&self) -> u16 {
        match self.items.get(self.tabs.index) {
            Some(TabItem::Service(_)) | Some(TabItem::Terminal(_)) => 20,
            None => 0,
        }
    }

    fn current_total_lines(&self) -> usize {
        match self.items.get(self.tabs.index) {
            Some(TabItem::Service(s)) => s.total_lines(),
            Some(TabItem::Terminal(t)) => t.total_lines(),
            None => 0,
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let has_command = self.command_mode;

        let layout = if has_command {
            Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
        } else {
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)])
        };

        let chunks = layout.split(area);
        self.tabs.draw(frame, chunks[0]);

        let content_area = chunks[1];

        if let Some(TabItem::Terminal(t)) = self.items.get_mut(self.tabs.index) {
            let w = content_area.width.saturating_sub(2).max(10);
            let h = content_area.height.saturating_sub(2).max(3);
            t.resize(w, h);
        }

        let is_terminal = self.is_terminal_tab(self.tabs.index);
        let in_terminal_input = is_terminal && matches!(self.mode, Mode::TerminalInput);

        let instructions = if in_terminal_input {
            Line::from(vec![
                " Ctrl+Q ".into(),
                "quit".blue().bold(),
                " Esc ".into(),
                "scroll".blue().bold(),
            ])
        } else if is_terminal {
            Line::from(vec![
                " Q ".into(),
                "quit".blue().bold(),
                " T ".into(),
                "new-term".blue().bold(),
                " I ".into(),
                "input".blue().bold(),
                " D ".into(),
                "close".blue().bold(),
            ])
        } else {
            Line::from(vec![
                " Q ".into(),
                "quit".blue().bold(),
                " X ".into(),
                "kill".blue().bold(),
                " R ".into(),
                "restart".blue().bold(),
                " T ".into(),
                "new-term".blue().bold(),
                " : ".into(),
                "cmd".blue().bold(),
            ])
        };

        let block = Block::bordered()
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        let inner = content_area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let visible_height = inner.height as usize;

        let (lines, _total): (Vec<Line>, usize) = match self.items.get_mut(self.tabs.index) {
            Some(TabItem::Service(s)) => {
                let total = s.total_lines();
                let raw = s.tail(visible_height, self.scroll_offset);
                let lines: Vec<Line> = raw.into_iter().map(Line::from).collect();
                (lines, total)
            }
            Some(TabItem::Terminal(t)) => {
                let total = t.total_lines();
                let raw = t.tail(visible_height, self.scroll_offset);
                let lines: Vec<Line> = raw
                    .into_iter()
                    .map(|l| match l.into_text() {
                        Ok(text) => {
                            let spans: Vec<Span<'static>> = text
                                .lines
                                .into_iter()
                                .flat_map(|line| line.spans.into_iter())
                                .collect();
                            Line::from(spans)
                        }
                        Err(_) => Line::from(l),
                    })
                    .collect();
                (lines, total)
            }
            None => (vec![Line::from("no tab")], 0),
        };

        let widget = Paragraph::new(Text::from(lines)).block(block);
        frame.render_widget(widget, content_area);

        if in_terminal_input {
            let cursor_y = content_area.bottom().saturating_sub(2);
            if cursor_y > content_area.y {
                frame.set_cursor_position(Position {
                    x: content_area.x + 1,
                    y: cursor_y,
                });
            }
        }

        if has_command {
            let cmd_area = chunks[2];
            let prompt = format!(":{}", self.command_buf);
            let cmd_block = Block::bordered()
                .border_set(border::THICK)
                .style(Style::default().bg(Color::DarkGray).fg(Color::White));
            frame.render_widget(
                Paragraph::new(Line::from(Span::raw(prompt))).block(cmd_block),
                cmd_area,
            );
            let x = cmd_area.x + 1 + self.command_buf.len() as u16;
            let y = cmd_area.y + 1;
            if x < cmd_area.right() && y < cmd_area.bottom() {
                frame.set_cursor_position(Position { x, y });
            }
        }
    }
}

fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Enter => Some(vec![b'\n']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Char(c) => {
            if key.modifiers == KeyModifiers::CONTROL {
                let byte = match c {
                    'a'..='z' => c as u8 - b'a' + 1,
                    'A'..='Z' => c as u8 - b'A' + 1,
                    _ => return None,
                };
                Some(vec![byte])
            } else if key.modifiers == KeyModifiers::SHIFT || key.modifiers == KeyModifiers::NONE {
                let mut s = vec![0u8; 4];
                let encoded = c.encode_utf8(&mut s);
                Some(encoded.as_bytes().to_vec())
            } else {
                None
            }
        }
        _ => None,
    }
}
