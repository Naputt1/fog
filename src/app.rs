use crate::click_tab::{ClickTab, TabKind};
use crate::proxy::ProxyInstance;
use crate::terminal::Terminal;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Style, Stylize},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Wrap},
};
use std::{io, time::Duration};
use std::io::Write;

enum Mode {
    Normal,
    TerminalInput,
}

pub struct App {
    items: Vec<Terminal>,
    proxy: Option<ProxyInstance>,
    tabs: ClickTab,
    mode: Mode,
    scroll_offset: usize,
    exit: bool,
    selecting: bool,
    select_start: Option<(usize, usize)>,
    select_end: Option<(usize, usize)>,
    content_area: Rect,
    errors: Vec<String>,
}

impl App {
    pub fn new(items: Vec<Terminal>, proxy: Option<ProxyInstance>) -> Self {
        let names: Vec<String> = items.iter().map(|t| t.name.clone()).collect();
        let mut tabs = ClickTab::new(names);
        for (i, item) in items.iter().enumerate() {
            tabs.entries[i].kind = if item.is_shell() {
                TabKind::Terminal
            } else {
                TabKind::Service
            };
        }

        if proxy.is_some() {
            tabs.add("proxy".to_string(), TabKind::Proxy);
        }

        Self {
            items,
            proxy,
            tabs,
            mode: Mode::Normal,
            scroll_offset: 0,
            exit: false,
            selecting: false,
            select_start: None,
            select_end: None,
            content_area: Rect::default(),
            errors: Vec::new(),
        }
    }

    fn is_proxy_tab(&self) -> bool {
        self.tabs
            .entries
            .get(self.tabs.index)
            .map(|e| e.kind == TabKind::Proxy)
            .unwrap_or(false)
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))? {
                self.handle_events()?;
            }
        }
        if !self.errors.is_empty() {
            for err in &self.errors {
                let _ = writeln!(std::io::stderr(), "{}", err);
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
        if self.is_proxy_tab() {
            self.mode = Mode::Normal;
        } else if let Some(item) = self.items.get(self.tabs.index) {
            self.mode = if item.is_shell() {
                Mode::TerminalInput
            } else {
                Mode::Normal
            };
        }
    }

    fn clear_selection(&mut self) {
        self.selecting = false;
        self.select_start = None;
        self.select_end = None;
    }

    fn is_shell_tab(&self, idx: usize) -> bool {
        self.items.get(idx).map(|t| t.is_shell()).unwrap_or(false)
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

        match self.mode {
            Mode::TerminalInput => self.handle_terminal_key(key),
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_terminal_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return;
        }
        if let Some(item) = self.items.get_mut(self.tabs.index) {
            if let Some(bytes) = key_to_bytes(key) {
                item.write(&bytes);
            }
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
            _ => {}
        }
    }

    fn restart_current(&mut self) {
        if self.is_proxy_tab() {
            if let Some(ref mut p) = self.proxy {
                p.restart();
            }
            return;
        }
        if let Some(item) = self.items.get_mut(self.tabs.index) {
            if !item.is_shell() {
                if let Err(e) = item.restart() {
                    self.errors.push(format!("restart error: {}", e));
                }
                if let Some(e) = self.tabs.entries.get_mut(self.tabs.index) {
                    e.stopped = false;
                }
            }
        }
    }

    fn new_terminal(&mut self) {
        match Terminal::spawn_shell("bash".to_string()) {
            Ok(term) => {
                let id = self.items.len();
                self.items.push(term);
                self.tabs.add("bash".to_string(), TabKind::Terminal);
                self.tabs.index = id;
                self.scroll_offset = 0;
                self.mode = Mode::TerminalInput;
            }
            Err(e) => eprintln!("failed to create terminal: {}", e),
        }
    }

    fn close_tab(&mut self) {
        if self.items.len() <= 1 {
            return;
        }
        if !self.is_shell_tab(self.tabs.index) {
            return;
        }
        let idx = self.tabs.index;
        self.items.remove(idx);
        self.tabs.remove(idx);
        self.scroll_offset = 0;
        if self.tabs.index < self.items.len() && self.is_shell_tab(self.tabs.index) {
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

    fn content_height(&self) -> u16 {
        self.content_area.height.saturating_sub(2)
    }

    fn current_total_lines(&self) -> usize {
        if self.is_proxy_tab() {
            match self.proxy {
                Some(ref p) => p.get_logs().len() + 3,
                None => 1,
            }
        } else {
            match self.items.get(self.tabs.index) {
                Some(item) => item.total_lines(),
                None => 0,
            }
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
        let (sel_start, sel_end) =
            if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
                (start, end)
            } else {
                (end, start)
            };
        let lines: Vec<String> = match self.items.get(self.tabs.index) {
            Some(item) => item.get_all_lines(),
            None => return,
        };
        let mut selected = String::new();
        for i in sel_start.0..=sel_end.0 {
            let Some(text) = lines.get(i) else {
                continue;
            };
            if sel_start.0 == sel_end.0 {
                let s: String = text
                    .chars()
                    .skip(sel_start.1)
                    .take(sel_end.1 - sel_start.1)
                    .collect();
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
        let (sel_start, sel_end) =
            if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
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
        let sidebar_width = self.tabs.min_width();

        let main =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(sidebar_width)])
                .split(area);

        let content_area = main[0];
        let sidebar_area = main[1];

        for (i, item) in self.items.iter_mut().enumerate() {
            item.refresh_status();
            if let Some(entry) = self.tabs.entries.get_mut(i) {
                entry.stopped = item.stopped;
            }
        }

        if let Some(ref mut p) = self.proxy {
            if let Some(entry) = self
                .tabs
                .entries
                .iter_mut()
                .find(|e| e.kind == TabKind::Proxy)
            {
                entry.stopped = !p.is_running();
            }
        }

        self.tabs.draw(frame, sidebar_area);

        self.content_area = content_area;

        let is_proxy = self.is_proxy_tab();
        let is_shell = self
            .items
            .get(self.tabs.index)
            .map(|t| t.is_shell())
            .unwrap_or(false);
        let in_terminal_input = matches!(self.mode, Mode::TerminalInput);

        let instructions = if in_terminal_input {
            Line::from(vec![
                " Ctrl+Q ".into(),
                "quit".blue().bold(),
                " Esc ".into(),
                "scroll".blue().bold(),
            ])
        } else if is_proxy {
            Line::from(vec![
                " Q ".into(),
                "quit".blue().bold(),
                " R ".into(),
                "restart".blue().bold(),
            ])
        } else if is_shell {
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
                " R ".into(),
                "restart".blue().bold(),
                " I ".into(),
                "input".blue().bold(),
                " T ".into(),
                "new-term".blue().bold(),
            ])
        };

        let block = Block::bordered()
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        if is_proxy {
            self.draw_proxy_content(frame, content_area, block);
        } else {
            self.draw_terminal_content(frame, content_area, block, in_terminal_input);
        }
    }

    fn draw_proxy_content(&mut self, frame: &mut Frame, area: Rect, block: Block) {
        let inner = area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let visible_height = inner.height as usize;

        let logs = match self.proxy {
            Some(ref p) => p.get_logs(),
            None => vec![],
        };

        let total = logs.len() + 3;
        let offset = self.scroll_offset.min(total.saturating_sub(visible_height).max(0));

        let _start = offset.saturating_sub(3);

        let mut lines: Vec<Line<'static>> = Vec::new();

        let status_line = match self.proxy {
            Some(ref p) if p.is_running() => {
                format!(" Proxy listening on port {} (running)", p.port)
            }
            Some(_) => " Proxy (stopped)".to_string(),
            None => " Proxy (not configured)".to_string(),
        };
        lines.push(Line::from(Span::styled(
            status_line,
            Style::default().cyan().bold(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" {:<6} {:<35} {:<5} {:<8} {}", "METHOD", "PATH", "STATUS", "LATENCY", "UPSTREAM"),
            Style::default().dim(),
        )));

        for entry in logs.iter().rev().skip(offset.saturating_sub(3)).take(visible_height.saturating_sub(3)) {
            let status_style = match entry.status {
                0 => Style::default().dim(),
                200..=299 => Style::default().green(),
                300..=399 => Style::default().yellow(),
                400..=499 => Style::default().red(),
                _ => Style::default().red().bold(),
            };
            let status_str = if entry.status == 0 {
                "".to_string()
            } else {
                format!("{}", entry.status)
            };
            let latency_str = if entry.status == 0 {
                String::new()
            } else {
                format!("{}ms", entry.latency_ms)
            };
            let method_span = if entry.ws {
                Span::styled(format!(" {:<6}", "WS"), Style::default().cyan().bold())
            } else {
                Span::raw(format!(" {:<6}", entry.method))
            };
            lines.push(Line::from(vec![
                method_span,
                Span::raw(format!(" {:<35}", truncate(&entry.path, 35))),
                Span::styled(format!(" {:<5}", status_str), status_style),
                Span::raw(format!(" {:<8}", latency_str)),
                Span::raw(format!(" {}", truncate(&entry.upstream, 30))),
            ]));
        }

        if lines.is_empty() {
            lines.push(Line::from(" no requests yet"));
        }

        let widget = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, area);
    }

    fn draw_terminal_content(
        &mut self,
        frame: &mut Frame,
        content_area: Rect,
        block: Block,
        in_terminal_input: bool,
    ) {
        let inner = content_area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let visible_height = inner.height as u16;

        if let Some(item) = self.items.get_mut(self.tabs.index) {
            item.resize(inner.width, visible_height);
        }

        let (mut lines, _total) = match self.items.get_mut(self.tabs.index) {
            Some(item) => item.get_screen(visible_height as usize, self.scroll_offset),
            None => (vec![Line::from("no tab")], 0),
        };
        self.apply_sel(&mut lines);

        let widget = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, content_area);

        if in_terminal_input && self.scroll_offset == 0 {
            if let Some(item) = self.items.get(self.tabs.index) {
                if let Some((row, col)) = item.cursor_position() {
                    let x = content_area.x + 1 + col;
                    let y = content_area.y + 1 + row;
                    if x < content_area.right() && y < content_area.bottom() {
                        frame.set_cursor_position(Position { x, y });
                    }
                }
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{}...", truncated)
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
            } else if key.modifiers == KeyModifiers::SHIFT || key.modifiers == KeyModifiers::NONE
            {
                let mut s = [0u8; 4];
                let encoded = c.encode_utf8(&mut s);
                Some(encoded.as_bytes().to_vec())
            } else {
                None
            }
        }
        _ => None,
    }
}
