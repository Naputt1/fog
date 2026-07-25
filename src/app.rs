use crate::click_tab::{ClickTab, TabKind};
use crate::config_watcher;
use crate::keybinding;
use crate::proxy::ProxyInstance;
use crate::render;
use crate::selection;
use crate::terminal::Terminal;
use crate::theme::Theme;
use crossterm::event::{
    self, DisableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph},
};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{io, time::Duration};

enum Mode {
    Normal,
    TerminalInput,
    ProxyFilter,
}

/// Main application state managing terminals, the proxy, tabs, and input handling.
pub struct App {
    items: Vec<Terminal>,
    proxy: Option<ProxyInstance>,
    sigint: Arc<AtomicBool>,
    theme: Theme,
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
    proxy_filter: String,
    config_path: std::path::PathBuf,
    config_rx: std::sync::mpsc::Receiver<()>,
    proxy_tab_index: Option<usize>,
    scrollbar_dragging: bool,
    auto_scrolling: Option<bool>,
    auto_scroll_col: u16,
}

impl App {
    /// Creates a new [`App`] with the given terminals, optional proxy, and SIGINT flag.
    ///
    /// # Arguments
    /// * `items` - The list of terminal instances.
    /// * `proxy` - An optional reverse proxy instance.
    /// * `sigint` - An `AtomicBool` flag set to `true` when SIGINT (Ctrl+C) is received.
    /// * `scrollback` - Maximum number of scrollback lines.
    /// * `sidebar_min` - Minimum sidebar width in columns.
    /// * `sidebar_max` - Maximum sidebar width in columns.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        items: Vec<Terminal>,
        proxy: Option<ProxyInstance>,
        sigint: Arc<AtomicBool>,
        scrollback: usize,
        sidebar_min: u16,
        sidebar_max: u16,
        theme: Theme,
        config_path: std::path::PathBuf,
        config_rx: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        let names: Vec<String> = items.iter().map(|t| t.name.clone()).collect();
        let mut tabs = ClickTab::new(names, sidebar_min, sidebar_max);
        for (i, item) in items.iter().enumerate() {
            tabs.entries[i].kind = if item.is_shell() {
                TabKind::Terminal
            } else {
                TabKind::Service
            };
        }

        let proxy_tab_index = if proxy.is_some() {
            let idx = items.len();
            tabs.add("proxy".to_string(), TabKind::Proxy);
            Some(idx)
        } else {
            None
        };

        Self {
            items,
            proxy,
            sigint,
            scrollback,
            theme,
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
            proxy_filter: String::new(),
            config_path,
            config_rx,
            proxy_tab_index,
            scrollbar_dragging: false,
            auto_scrolling: None,
            auto_scroll_col: 0,
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
    fn reload_config(&mut self) {
        config_watcher::reload_config(&self.config_path, &mut self.proxy, &mut self.theme);
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            if self.config_rx.try_recv().is_ok() {
                self.reload_config();
            }
            if self.sigint.load(Ordering::SeqCst) {
                self.exit = true;
                break;
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))? {
                self.handle_events()?;
            }
            self.handle_auto_scroll();
        }
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        let _ = event::poll(Duration::from_millis(20));
        while event::poll(Duration::from_millis(0)).unwrap_or(false) {
            let _ = event::read();
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
                    if self.handle_scrollbar_click(mouse.column, mouse.row) {
                        self.scrollbar_dragging = true;
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
                    if self.scrollbar_dragging {
                        self.handle_scrollbar_drag(mouse.row);
                    } else if self.selecting {
                        let inner_y = self.content_area.y.saturating_add(1);
                        let inner_h = self.content_area.height.saturating_sub(2);
                        if mouse.row < inner_y {
                            self.auto_scrolling = Some(true);
                            self.auto_scroll_col = mouse.column;
                            self.step_auto_scroll();
                        } else if mouse.row >= inner_y.saturating_add(inner_h) {
                            self.auto_scrolling = Some(false);
                            self.auto_scroll_col = mouse.column;
                            self.step_auto_scroll();
                        } else {
                            self.auto_scrolling = None;
                            if let Some(pos) = selection::screen_to_content(
                                mouse.column,
                                mouse.row,
                                self.content_area,
                                self.scroll_offset,
                                self.current_total_lines(),
                            ) {
                                self.select_end = Some(pos);
                            }
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.auto_scrolling = None;
                    if self.scrollbar_dragging {
                        self.scrollbar_dragging = false;
                    }
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
        self.scrollbar_dragging = false;
        self.auto_scrolling = None;
        selection::clear_selection(
            &mut self.selecting,
            &mut self.select_start,
            &mut self.select_end,
        );
        self.proxy_filter.clear();
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

        if matches!(self.mode, Mode::ProxyFilter) {
            match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.proxy_filter.clear();
                }
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                }
                KeyCode::Backspace => {
                    self.proxy_filter.pop();
                }
                KeyCode::Char(c) if !c.is_control() => {
                    self.proxy_filter.push(c);
                }
                _ => {}
            }
            return;
        }

        match self.mode {
            Mode::TerminalInput => self.handle_terminal_key(key),
            Mode::Normal => self.handle_normal_key(key),
            Mode::ProxyFilter => {}
        }
    }

    fn handle_terminal_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return;
        }
        if let Some(item) = self.items.get_mut(self.tabs.index)
            && let Some(bytes) = keybinding::key_to_bytes(key)
        {
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
            KeyCode::Char('/') => {
                if self.is_proxy_tab() {
                    self.mode = Mode::ProxyFilter;
                }
            }
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
            && !item.is_shell()
        {
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
                let insertion_idx = self.proxy_tab_index.unwrap_or(self.items.len());
                self.items.insert(insertion_idx, term);
                self.tabs
                    .insert_at(insertion_idx, "bash".to_string(), TabKind::Terminal);
                self.tabs.index = insertion_idx;
                self.scroll_offset = 0;
                self.mode = Mode::TerminalInput;
            }
            Err(e) => self
                .errors
                .push(format!("failed to create terminal: {}", e)),
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
        if let Some(proxy_idx) = self.proxy_tab_index.as_mut()
            && idx < *proxy_idx
        {
            *proxy_idx -= 1;
        }
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

    fn handle_scrollbar_click(&mut self, col: u16, row: u16) -> bool {
        let scrollbar_x = self.content_area.right().saturating_sub(2);
        let scrollbar_y = self.content_area.y + 1;
        let scrollbar_h = self.content_area.height.saturating_sub(2);

        if col != scrollbar_x || row < scrollbar_y || row >= scrollbar_y + scrollbar_h {
            return false;
        }

        if let Some(offset) = self.scrollbar_row_to_offset(row) {
            self.scroll_to(offset);
        }
        true
    }

    fn handle_scrollbar_drag(&mut self, row: u16) {
        if let Some(offset) = self.scrollbar_row_to_offset(row) {
            self.scroll_to(offset);
        }
    }

    fn edge_content_pos(&self, col: u16, top: bool) -> Option<(usize, usize)> {
        let inner_x = self.content_area.x.saturating_add(1);
        let inner_w = self.content_area.width.saturating_sub(2);
        if col < inner_x || col >= inner_x.saturating_add(inner_w) {
            return None;
        }
        let col_idx = (col - inner_x) as usize;
        let total = self.current_total_lines();
        let visible = self.content_height() as usize;
        let end = total.saturating_sub(self.scroll_offset);
        let start = end.saturating_sub(visible);
        let line = if top { start } else { end.saturating_sub(1) };
        if line >= total {
            None
        } else {
            Some((line, col_idx))
        }
    }

    fn step_auto_scroll(&mut self) {
        let Some(scrolling_up) = self.auto_scrolling else { return };
        let col = self.auto_scroll_col;
        if scrolling_up {
            self.scroll_to(self.scroll_offset.saturating_add(3));
            if let Some(pos) = self.edge_content_pos(col, true) {
                self.select_end = Some(pos);
            }
        } else {
            self.scroll_to(self.scroll_offset.saturating_sub(3));
            if let Some(pos) = self.edge_content_pos(col, false) {
                self.select_end = Some(pos);
            }
        }
    }

    fn handle_auto_scroll(&mut self) {
        if self.auto_scrolling.is_some() {
            self.step_auto_scroll();
        }
    }

    fn scrollbar_row_to_offset(&self, row: u16) -> Option<usize> {
        let scrollbar_y = self.content_area.y + 1;
        let scrollbar_h = self.content_area.height.saturating_sub(2);
        if scrollbar_h == 0 {
            return None;
        }

        let total = self.current_total_lines();
        let visible = self.content_height() as usize;
        let max_scroll = total.saturating_sub(visible);
        if max_scroll == 0 {
            return None;
        }

        let row_clamped = row.clamp(scrollbar_y, scrollbar_y + scrollbar_h - 1);
        let relative_y = (row_clamped - scrollbar_y) as usize;
        let target_position = relative_y.saturating_mul(max_scroll) / scrollbar_h as usize;
        Some(max_scroll.saturating_sub(target_position))
    }

    fn content_height(&self) -> u16 {
        self.content_area.height.saturating_sub(2)
    }

    fn current_total_lines(&self) -> usize {
        if self.is_proxy_tab() {
            let filter_lines = if matches!(self.mode, Mode::ProxyFilter) {
                1usize
            } else {
                0
            };
            match self.proxy {
                Some(ref p) => p.get_logs().len() + 3 + filter_lines,
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
            Layout::horizontal([Constraint::Min(1), Constraint::Length(sidebar_width)]).split(area);

        let content_area = main[0];
        let sidebar_area = main[1];

        for (i, item) in self.items.iter_mut().enumerate() {
            item.refresh_status();
            if let Some(entry) = self.tabs.entries.get_mut(i) {
                entry.stopped = item.stopped;
                entry.process_running = item.process_running;
                entry.health_status = item.get_health_status();
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

        self.tabs.draw(frame, sidebar_area, &self.theme);

        self.content_area = content_area;

        let is_proxy = self.is_proxy_tab();
        let is_shell = self
            .items
            .get(self.tabs.index)
            .map(|t| t.is_shell())
            .unwrap_or(false);
        let in_terminal_input = matches!(self.mode, Mode::TerminalInput);

        let instructions = render::draw_instructions(is_proxy, is_shell, in_terminal_input);

        let block = Block::bordered()
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        if is_proxy {
            render::draw_proxy_content(
                frame,
                content_area,
                block,
                &self.proxy,
                self.scroll_offset,
                &self.proxy_filter,
                matches!(self.mode, Mode::ProxyFilter),
                &self.theme,
            );
        } else {
            let total_lines = self.current_total_lines();
            render::draw_terminal_content(
                frame,
                content_area,
                block,
                &mut self.items,
                self.tabs.index,
                self.scroll_offset,
                self.select_start,
                self.select_end,
                in_terminal_input,
                total_lines,
                &self.theme,
            );
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

            let block = Block::bordered().title(" Help ").style(Style::default());
            let help = Paragraph::new(Text::from(help_text))
                .block(block)
                .alignment(Alignment::Left);

            frame.render_widget(Clear, overlay_area);
            frame.render_widget(help, overlay_area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    fn make_app(
        items: Vec<Terminal>,
        proxy: Option<ProxyInstance>,
        tabs: ClickTab,
        mode: Mode,
        content_area: Rect,
    ) -> App {
        let proxy_tab_index = tabs.entries.iter().position(|e| e.kind == TabKind::Proxy);
        let (_tx, rx) = mpsc::channel();
        App {
            items,
            proxy,
            sigint: Arc::new(AtomicBool::new(false)),
            theme: Theme::default(),
            scrollback: 0,
            tabs,
            mode,
            scroll_offset: 0,
            exit: false,
            selecting: false,
            select_start: None,
            select_end: None,
            content_area,
            show_help: false,
            errors: vec![],
            proxy_filter: String::new(),
            config_path: std::path::PathBuf::new(),
            config_rx: rx,
            proxy_tab_index,
            scrollbar_dragging: false,
            auto_scrolling: None,
            auto_scroll_col: 0,
        }
    }

    #[test]
    fn test_is_proxy_tab_false() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        assert!(!app.is_proxy_tab());
    }

    #[test]
    fn test_is_proxy_tab_true() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert!(app.is_proxy_tab());
    }

    #[test]
    fn test_is_proxy_tab_not_selected() {
        let mut tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        tabs.index = 0;
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert!(!app.is_proxy_tab());
    }

    #[test]
    fn test_content_height() {
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 20,
            },
        );
        assert_eq!(app.content_height(), 18);
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 5,
            },
        );
        assert_eq!(app.content_height(), 3);
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 1,
            },
        );
        assert_eq!(app.content_height(), 0);
    }

    #[test]
    fn test_current_total_lines_no_proxy_empty() {
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 0);
    }

    #[test]
    fn test_current_total_lines_with_proxy() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 3);
    }

    #[test]
    fn test_current_total_lines_with_proxy_filter_mode() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::ProxyFilter,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 4);
    }
}
