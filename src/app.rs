use crate::click_tab::{ClickTab, TabKind};
use crate::service::Service;
use crate::terminal::TerminalSession;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Color, Style, Stylize},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Wrap},
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
    selecting: bool,
    select_start: Option<(usize, usize)>,
    select_end: Option<(usize, usize)>,
    content_area: Rect,
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
            selecting: false,
            select_start: None,
            select_end: None,
            content_area: Rect::default(),
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
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let idx_before = self.tabs.index;
                    self.tabs.click(mouse.column, mouse.row);
                    if self.tabs.index != idx_before {
                        self.on_tab_switch();
                        return Ok(());
                    }
                    if let Some(pos) = self.screen_to_content(mouse.column, mouse.row) {
                        self.selecting = true;
                        self.select_start = Some(pos);
                        self.select_end = Some(pos);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if self.selecting {
                        if let Some(pos) = self.screen_to_content(mouse.column, mouse.row) {
                            self.select_end = Some(pos);
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if self.selecting {
                        self.selecting = false;
                        if let (Some(start), Some(end)) = (self.select_start, self.select_end) {
                            self.copy_selection(start, end);
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
        self.clear_selection();
        if self.is_terminal_tab(self.tabs.index) {
            self.mode = Mode::TerminalInput;
        } else {
            self.mode = Mode::Normal;
        }
    }

    fn clear_selection(&mut self) {
        self.selecting = false;
        self.select_start = None;
        self.select_end = None;
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
            KeyCode::Char('j') | KeyCode::Char('h') | KeyCode::Right => {
                let prev = self.tabs.index;
                self.tabs.index = (self.tabs.index + 1) % self.items.len();
                if prev != self.tabs.index {
                    self.on_tab_switch();
                }
            }
            KeyCode::Char('k') | KeyCode::Char('l') | KeyCode::Left => {
                let prev = self.tabs.index;
                self.tabs.index = (self.tabs.index + self.items.len() - 1) % self.items.len();
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
        let visible = self.content_height() as usize;
        let total = self.current_total_lines();
        let max = total.saturating_sub(visible);
        self.scroll_offset = target.min(max);
    }

    fn content_height(&self) -> u16 {
        self.content_area.height.saturating_sub(2)
    }

    fn current_total_lines(&self) -> usize {
        match self.items.get(self.tabs.index) {
            Some(TabItem::Service(s)) => s.total_lines(),
            Some(TabItem::Terminal(t)) => t.total_lines(),
            None => 0,
        }
    }

    fn screen_to_content(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let inner_x = self.content_area.x.saturating_add(1);
        let inner_y = self.content_area.y.saturating_add(1);
        let inner_w = self.content_area.width.saturating_sub(2);
        let inner_h = self.content_area.height.saturating_sub(2);
        if x < inner_x || x >= inner_x.saturating_add(inner_w) {
            return None;
        }
        if y < inner_y || y >= inner_y.saturating_add(inner_h) {
            return None;
        }
        let col = (x - inner_x) as usize;
        let row = (y - inner_y) as usize;
        let total = self.current_total_lines();
        let visible = inner_h as usize;
        let offset = self.scroll_offset;
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(visible);
        let line_idx = start.saturating_add(row);
        if line_idx >= total {
            return None;
        }
        Some((line_idx, col))
    }

    fn copy_selection(&self, start: (usize, usize), end: (usize, usize)) {
        let (sel_start, sel_end) = if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1)
        {
            (start, end)
        } else {
            (end, start)
        };
        let lines: Vec<String> = match self.items.get(self.tabs.index) {
            Some(TabItem::Service(s)) => s.get_all_lines(),
            Some(TabItem::Terminal(t)) => t.get_all_lines(),
            None => return,
        };
        let mut selected = String::new();
        for i in sel_start.0..=sel_end.0 {
            let Some(text) = lines.get(i) else { continue };
            if sel_start.0 == sel_end.0 {
                let s: String = text.chars().skip(sel_start.1).take(sel_end.1 - sel_start.1).collect();
                selected.push_str(&s);
            } else if i == sel_start.0 {
                let s: String = text.chars().skip(sel_start.1).collect();
                selected.push_str(&s);
            } else if i == sel_end.0 {
                let s: String = text.chars().take(sel_end.1).collect();
                selected.push_str(&s);
            } else {
                selected.push_str(&text);
            }
            if i != sel_end.0 {
                selected.push('\n');
            }
        }
        if !selected.is_empty() {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&selected);
            use std::io::Write;
            let _ = write!(std::io::stdout(), "\x1b]52;c;{}\x07", encoded);
            let _ = std::io::stdout().flush();
        }
    }

    fn apply_sel(&self, lines: &mut [Line<'static>]) {
        let Some(start) = self.select_start else { return };
        let Some(end) = self.select_end else { return };
        let (sel_start, sel_end) = if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
            (start, end)
        } else {
            (end, start)
        };
        let total = self.current_total_lines();
        let visible = lines.len();
        let offset = self.scroll_offset;
        let end_idx = total.saturating_sub(offset);
        let start_idx = end_idx.saturating_sub(visible);
        for (i, line) in lines.iter_mut().enumerate() {
            let line_idx = start_idx + i;
            if line_idx < sel_start.0 || line_idx > sel_end.0 {
                continue;
            }
            let (sc, ec) = if sel_start.0 == sel_end.0 {
                (sel_start.1.min(sel_end.1), sel_start.1.max(sel_end.1))
            } else if line_idx == sel_start.0 {
                (sel_start.1, usize::MAX)
            } else if line_idx == sel_end.0 {
                (0, sel_end.1)
            } else {
                (0, usize::MAX)
            };
            let spans = std::mem::take(&mut line.spans);
            let mut new_spans = Vec::new();
            let mut char_off = 0;
            for span in spans {
                let span_len = span.content.chars().count();
                let span_start = char_off;
                let span_end = span_start + span_len;
                if span_end <= sc || span_start >= ec {
                    new_spans.push(span);
                } else {
                    let content = span.content.into_owned();
                    let orig_style = span.style;
                    let chars: Vec<char> = content.chars().collect();
                    let before_end = sc.saturating_sub(span_start).min(chars.len());
                    let after_start = ec.saturating_sub(span_start).min(chars.len());
                    if before_end > 0 {
                        let before: String = chars[..before_end].iter().collect();
                        new_spans.push(Span::styled(before, orig_style));
                    }
                    if before_end < after_start {
                        let sel: String = chars[before_end..after_start].iter().collect();
                        new_spans.push(Span::styled(sel, Style::new().reversed()));
                    }
                    if after_start < chars.len() {
                        let after: String = chars[after_start..].iter().collect();
                        new_spans.push(Span::styled(after, orig_style));
                    }
                }
                char_off += span_len;
            }
            line.spans = new_spans;
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let has_command = self.command_mode;
        let sidebar_width = self.tabs.min_width();

        let main = Layout::horizontal([
            Constraint::Min(1),
            Constraint::Length(sidebar_width),
        ])
        .split(area);

        let content_area = main[0];
        let sidebar_area = main[1];

        for (i, item) in self.items.iter_mut().enumerate() {
            if let TabItem::Service(s) = item {
                s.refresh_status();
                if let Some(entry) = self.tabs.entries.get_mut(i) {
                    entry.stopped = s.stopped;
                }
            }
        }

        self.tabs.draw(frame, sidebar_area);

        let content_layout = if has_command {
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
        } else {
            Layout::vertical([Constraint::Min(1)])
        };

        let content_chunks = content_layout.split(content_area);
        let inner_content = content_chunks[0];
        self.content_area = inner_content;

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

        if let Some(TabItem::Service(s)) = self.items.get_mut(self.tabs.index) {
            s.resize(inner.width, visible_height as u16);
        }

        let (mut lines, _total) = match self.items.get_mut(self.tabs.index) {
            Some(TabItem::Service(s)) => s.get_screen(visible_height, self.scroll_offset),
            Some(TabItem::Terminal(t)) => t.get_screen(visible_height, self.scroll_offset),
            None => (vec![Line::from("no tab")], 0),
        };
        self.apply_sel(&mut lines);

        let widget = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, content_area);

        if in_terminal_input && self.scroll_offset == 0 {
            if let Some(TabItem::Terminal(t)) = self.items.get_mut(self.tabs.index) {
                if let Some((row, col)) = t.cursor_position() {
                    let x = content_area.x + 1 + col;
                    let y = content_area.y + 1 + row;
                    if x < content_area.right() && y < content_area.bottom() {
                        frame.set_cursor_position(Position { x, y });
                    }
                }
            }
        }

        if has_command {
            let cmd_area = content_chunks[1];
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
