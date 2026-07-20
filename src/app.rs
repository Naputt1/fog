use crate::click_tab::{ClickTab, TabKind};
use crate::keybinding;
use crate::proxy::ProxyInstance;
use crate::selection;
use crate::terminal::Terminal;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::{Style, Stylize},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{io, time::Duration};

enum Mode {
    Normal,
    TerminalInput,
}

/// Main application state managing terminals, the proxy, tabs, and input handling.
pub struct App {
    items: Vec<Terminal>,
    proxy: Option<ProxyInstance>,
    sigint: Arc<AtomicBool>,
    scrollback: usize,
    tabs: ClickTab,
    mode: Mode,
    scroll_offset: usize,
    exit: bool,
    selecting: bool,
    select_start: Option<(usize, usize)>,
    select_end: Option<(usize, usize)>,
    content_area: Rect,
    show_help: bool,
    errors: Vec<String>,
}

impl App {
    /// Creates a new [`App`] with the given terminals, optional proxy, and SIGINT flag.
    ///
    /// # Arguments
    /// * `items` - The list of terminal instances.
    /// * `proxy` - An optional reverse proxy instance.
    /// * `sigint` - An `AtomicBool` flag set to `true` when SIGINT (Ctrl+C) is received.
    pub fn new(items: Vec<Terminal>, proxy: Option<ProxyInstance>, sigint: Arc<AtomicBool>, scrollback: usize) -> Self {
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
            sigint,
            scrollback,
            tabs,
            mode: Mode::Normal,
            scroll_offset: 0,
            exit: false,
            selecting: false,
            select_start: None,
            select_end: None,
            content_area: Rect::default(),
            show_help: false,
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

    /// Runs the main event loop until exit is requested.
    ///
    /// Draws the UI on every tick and processes keyboard and mouse events.
    ///
    /// # Arguments
    /// * `terminal` - The ratatui terminal to render to.
    ///
    /// # Errors
    /// Returns an error if terminal rendering or event polling fails.
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            if self.sigint.load(Ordering::SeqCst) {
                self.exit = true;
                break;
            }
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
                    if let Some(pos) = selection::screen_to_content(
                        mouse.column,
                        mouse.row,
                        self.content_area,
                        self.scroll_offset,
                        self.current_total_lines(),
                    ) {
                        self.selecting = true;
                        self.select_start = Some(pos);
                        self.select_end = Some(pos);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if self.selecting
                        && let Some(pos) = selection::screen_to_content(
                            mouse.column,
                            mouse.row,
                            self.content_area,
                            self.scroll_offset,
                            self.current_total_lines(),
                        ) {
                            self.select_end = Some(pos);
                        }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if self.selecting {
                        self.selecting = false;
                        if let (Some(start), Some(end)) = (self.select_start, self.select_end) {
                            selection::copy_selection(start, end, &self.items, self.tabs.index);
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
        selection::clear_selection(&mut self.selecting, &mut self.select_start, &mut self.select_end);
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

    fn is_shell_tab(&self, idx: usize) -> bool {
        self.items.get(idx).map(|t| t.is_shell()).unwrap_or(false)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.show_help {
            match key.code {
                KeyCode::Char('?') => self.show_help = false,
                KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => self.exit = true,
                _ => self.show_help = false,
            }
            return;
        }

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
        if let Some(item) = self.items.get_mut(self.tabs.index)
            && let Some(bytes) = keybinding::key_to_bytes(key) {
                item.write(&bytes);
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
            KeyCode::Char('?') => self.show_help = !self.show_help,
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
        if let Some(item) = self.items.get_mut(self.tabs.index)
            && !item.is_shell() {
                if let Err(e) = item.restart() {
                    self.errors.push(format!("restart error: {}", e));
                }
                if let Some(e) = self.tabs.entries.get_mut(self.tabs.index) {
                    e.stopped = false;
                }
            }
    }

    fn new_terminal(&mut self) {
        match Terminal::spawn_shell("bash".to_string(), self.scrollback) {
            Ok(term) => {
                let id = self.items.len();
                self.items.push(term);
                self.tabs.add("bash".to_string(), TabKind::Terminal);
                self.tabs.index = id;
                self.scroll_offset = 0;
                self.mode = Mode::TerminalInput;
            }
            Err(e) => self.errors.push(format!("failed to create terminal: {}", e)),
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

        if let Some(ref mut p) = self.proxy
            && let Some(entry) = self
                .tabs
                .entries
                .iter_mut()
                .find(|e| e.kind == TabKind::Proxy)
            {
                entry.stopped = !p.is_running();
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

        if self.show_help {
            let help_text = vec![
                Line::from(vec![Span::raw("  q/Ctrl+q   Quit                ")]),
                Line::from(vec![Span::raw("  j/Right    Next tab            ")]),
                Line::from(vec![Span::raw("  k/Left     Previous tab        ")]),
                Line::from(vec![Span::raw("  i          Terminal input mode ")]),
                Line::from(vec![Span::raw("  Esc        Exit input mode     ")]),
                Line::from(vec![Span::raw("  R          Restart service     ")]),
                Line::from(vec![Span::raw("  t/Ctrl+t   New shell tab       ")]),
                Line::from(vec![Span::raw("  d          Close shell tab     ")]),
                Line::from(vec![Span::raw("  g/Home     Scroll to top       ")]),
                Line::from(vec![Span::raw("  G/End      Scroll to bottom    ")]),
                Line::from(vec![Span::raw("  Up/Down    Scroll output       ")]),
                Line::from(vec![Span::raw("  PgUp/Dn    Scroll by page      ")]),
                Line::from(vec![Span::raw("  ?          Toggle help         ")]),
            ];

            let overlay_width = 40u16.min(area.width.saturating_sub(4));
            let overlay_height = help_text.len() as u16 + 2;
            let overlay_x = (area.width.saturating_sub(overlay_width)) / 2;
            let overlay_y = (area.height.saturating_sub(overlay_height)) / 2;

            let overlay_area = Rect {
                x: overlay_x,
                y: overlay_y,
                width: overlay_width,
                height: overlay_height,
            };

            let block = Block::bordered()
                .title(" Help ")
                .style(Style::default());
            let help = Paragraph::new(Text::from(help_text))
                .block(block)
                .alignment(Alignment::Left);

            frame.render_widget(Clear, overlay_area);
            frame.render_widget(help, overlay_area);
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
        let offset = self.scroll_offset.min(total.saturating_sub(visible_height));

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
        let visible_height = inner.height;

        if let Some(item) = self.items.get_mut(self.tabs.index) {
            item.resize(inner.width, visible_height);
        }

        let (mut lines, _total) = match self.items.get_mut(self.tabs.index) {
            Some(item) => item.get_screen(visible_height as usize, self.scroll_offset),
            None => (vec![Line::from("no tab")], 0),
        };
        selection::apply_sel(&mut lines, self.select_start, self.select_end, self.scroll_offset, self.current_total_lines());

        if self.scroll_offset > 0 && !lines.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" ↑ scrolled up {} lines", self.scroll_offset),
                    Style::default().dim(),
                ),
            ]));
        }

        let widget = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, content_area);

        if in_terminal_input && self.scroll_offset == 0
            && let Some(item) = self.items.get(self.tabs.index)
                && let Some((row, col)) = item.cursor_position() {
                    let x = content_area.x + 1 + col;
                    let y = content_area.y + 1 + row;
                    if x < content_area.right() && y < content_area.bottom() {
                        frame.set_cursor_position(Position { x, y });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn test_truncate_under_max() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact_max() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_over_max() {
        assert_eq!(truncate("hello world", 8), "hello...");
    }

    #[test]
    fn test_truncate_multi_byte() {
        assert_eq!(truncate("héllo wörld", 6), "hél...");
    }

    #[test]
    fn test_truncate_max_zero() {
        assert_eq!(truncate("hello", 0), "...");
    }
}
