//! Keyboard, mouse, and resize event handling.

use super::{ALERT_COPIED_TTL, App, Mode};
use crate::keybinding;
use crate::selection;
use crate::worktree;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::Position;
use std::io;
use std::path::Path;
use std::time::Instant;

impl App {
    pub(crate) fn handle_event(&mut self, ev: Event) -> io::Result<()> {
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // An alert under the pointer consumes the click before any
                    // tab/scrollbar/selection handling.
                    if self.handle_alert_click(mouse.column, mouse.row) {
                        return Ok(());
                    }
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
                    let layout = self.active_layout();
                    if let Some(pos) = selection::screen_to_content(
                        mouse.column,
                        mouse.row,
                        self.content_area,
                        self.scroll_offset,
                        self.current_total_lines(),
                        layout,
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
                            let layout = self.active_layout();
                            if let Some(pos) = selection::screen_to_content(
                                mouse.column,
                                mouse.row,
                                self.content_area,
                                self.scroll_offset,
                                self.current_total_lines(),
                                layout,
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
                        if let (Some(start), Some(end)) = (self.select_start, self.select_end)
                            && let Some(idx) = self.service_tab_index()
                        {
                            selection::copy_selection(start, end, &self.items, idx);
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

    /// Handles a click on the alert stack: the `✕` control dismisses the alert,
    /// anywhere else on its body copies the message and flashes it as copied.
    /// Returns `true` when the click hit an alert and was consumed.
    pub(crate) fn handle_alert_click(&mut self, x: u16, y: u16) -> bool {
        let point = Position { x, y };
        // Close controls take precedence and dismiss without copying.
        if let Some(index) = self
            .alert_areas
            .iter()
            .rev()
            .find(|hit| hit.close.contains(point))
            .map(|hit| hit.index)
            && index < self.alerts.len()
        {
            self.alerts.remove(index);
            self.force_redraw = true;
            return true;
        }
        let Some(index) = self
            .alert_areas
            .iter()
            .rev()
            .find(|hit| hit.body.contains(point))
            .map(|hit| hit.index)
        else {
            return false;
        };
        let Some(alert) = self.alerts.get_mut(index) else {
            return false;
        };
        selection::copy_text(&alert.message);
        alert.copied = true;
        alert.created = Instant::now();
        alert.ttl = ALERT_COPIED_TTL;
        self.force_redraw = true;
        true
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if self.show_help {
            match key.code {
                KeyCode::Char('?') => self.show_help = false,
                KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => self.exit = true,
                _ => self.show_help = false,
            }
            return;
        }

        if self.switch_popup.is_some() {
            self.handle_switch_key(key);
            return;
        }

        // With no tabs (empty script), tab navigation would divide by zero;
        // only quitting and opening a shell terminal remain useful.
        if self.tabs.entries.is_empty() {
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('t') => {}
                _ => return,
            }
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

    pub(crate) fn handle_terminal_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return;
        }
        if let Some(item) = self.service_tab_index().and_then(|i| self.items.get_mut(i))
            && let Some(bytes) = keybinding::key_to_bytes(key)
        {
            item.write(&bytes);
        }
    }

    pub(crate) fn handle_normal_key(&mut self, key: KeyEvent) {
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
            KeyCode::Char('s') => self.open_switch_popup(),
            KeyCode::Char('?') => self.show_help = !self.show_help,
            _ => {}
        }
    }

    /// Handles keys while the worktree-switch popup is open. In search mode
    /// (`f` toggled) typing filters the list; browsing accepts `f`, arrows,
    /// Enter (switch), `d` (terminate), and Esc.
    pub(crate) fn handle_switch_key(&mut self, key: KeyEvent) {
        // A status message lingers only until the next key press.
        if let Some(p) = &mut self.switch_popup {
            p.status = None;
        }
        let searching = self
            .switch_popup
            .as_ref()
            .map(|p| p.searching)
            .unwrap_or(false);
        match key.code {
            KeyCode::Char('f') if !searching => {
                if let Some(p) = &mut self.switch_popup {
                    p.searching = true;
                    p.selected = 0;
                }
            }
            KeyCode::Char('d') if !searching => self.terminate_selected_branch(),
            KeyCode::Esc if searching => {
                if let Some(p) = &mut self.switch_popup {
                    p.searching = false;
                }
            }
            KeyCode::Esc => {
                self.switch_popup = None;
            }
            KeyCode::Enter => {
                let selected = self.switch_popup.as_ref().and_then(|p| {
                    let matches = p.matches();
                    if matches.is_empty() {
                        None
                    } else {
                        Some(matches[p.selected.min(matches.len() - 1)].clone())
                    }
                });
                if let Some(wt) = selected {
                    let config_dir = self.config_path.parent().unwrap_or_else(|| Path::new("."));
                    let already_here = self
                        .switch_popup
                        .as_ref()
                        .map(|p| worktree::is_current_worktree(&wt, &p.worktrees, config_dir))
                        .unwrap_or_else(|| wt.contains(config_dir));
                    if already_here {
                        if let Some(p) = &mut self.switch_popup {
                            p.status = Some("already on this branch".to_string());
                        }
                        return;
                    }
                    let result = self.switch_worktree(&wt);
                    if let Err(e) = result {
                        if let Some(p) = &mut self.switch_popup {
                            p.status = Some(e);
                        } else {
                            self.note_error(e);
                        }
                    } else {
                        self.switch_popup = None;
                    }
                }
            }
            KeyCode::Up | KeyCode::Down => {
                if let Some(p) = &mut self.switch_popup {
                    let len = p.matches().len();
                    if len > 0 {
                        p.selected = if matches!(key.code, KeyCode::Up) {
                            (p.selected + len - 1) % len
                        } else {
                            (p.selected + 1) % len
                        };
                    }
                }
            }
            KeyCode::Backspace if searching => {
                if let Some(p) = &mut self.switch_popup {
                    p.filter.pop();
                    p.selected = 0;
                }
            }
            KeyCode::Char(c) if searching && !c.is_control() => {
                if let Some(p) = &mut self.switch_popup {
                    p.filter.push(c);
                    p.selected = 0;
                }
            }
            _ => {}
        }
    }
}
