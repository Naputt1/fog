//! Content-panel rendering: layout, scroll state, and the full `draw` pass.

use super::{ALERT_REPAINT, ALERT_WIDTH, AlertHit, App, Mode};
use crate::render;
use crate::worktree;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use std::path::Path;
use std::time::{Duration, Instant};

impl App {
    pub(crate) fn scroll_to(&mut self, target: usize) {
        let visible = self.content_height() as usize;
        let total = self.current_total_lines();
        let max = total.saturating_sub(visible);
        self.scroll_offset = target.min(max);
    }

    pub(crate) fn handle_scrollbar_click(&mut self, col: u16, row: u16) -> bool {
        let scrollbar_x = self.content_area.right().saturating_sub(2);
        let scrollbar_y = self.content_area.y + 1;
        let scrollbar_h = self.content_area.height.saturating_sub(2);

        if col != scrollbar_x || row < scrollbar_y || row >= scrollbar_y + scrollbar_h {
            return false;
        }

        let Some(offset) = self.scrollbar_row_to_offset(row) else {
            // No scrollbar is actually rendered (nothing to scroll): let the
            // click fall through so drag-select can start in this column.
            return false;
        };
        self.scroll_to(offset);
        true
    }

    pub(crate) fn handle_scrollbar_drag(&mut self, row: u16) {
        if let Some(offset) = self.scrollbar_row_to_offset(row) {
            self.scroll_to(offset);
        }
    }

    pub(crate) fn edge_content_pos(&self, col: u16, top: bool) -> Option<(usize, usize)> {
        let inner_x = self.content_area.x.saturating_add(1);
        let inner_w = self.content_area.width.saturating_sub(2);
        if col < inner_x || col >= inner_x.saturating_add(inner_w) {
            return None;
        }
        let col_idx = (col - inner_x) as usize;
        if let Some(layout) = self.active_layout() {
            // Wrap-aware edges: the first/last rendered row's content.
            let entry = if top {
                layout.iter().find_map(|e| *e)
            } else {
                layout.iter().rev().find_map(|e| *e)
            }?;
            let (line, col_off) = entry;
            if line >= self.current_total_lines() {
                return None;
            }
            return Some((line, col_off + col_idx));
        }
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

    /// Returns the physical-row layout of the last terminal-pane render, used
    /// to map mouse coordinates to exact content positions. The proxy pane has
    /// no layout (it does not wrap), so it falls back to the logical-line
    /// formula.
    pub(crate) fn active_layout(&self) -> Option<&[Option<(usize, usize)>]> {
        if self.is_proxy_tab() {
            None
        } else {
            Some(self.content_layout.as_slice())
        }
    }

    pub(crate) fn step_auto_scroll(&mut self) {
        let Some(scrolling_up) = self.auto_scrolling else {
            return;
        };
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

    pub(crate) fn handle_auto_scroll(&mut self) {
        if self.auto_scrolling.is_some() {
            self.step_auto_scroll();
        }
    }

    pub(crate) fn scrollbar_row_to_offset(&self, row: u16) -> Option<usize> {
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

    pub(crate) fn content_height(&self) -> u16 {
        self.content_area.height.saturating_sub(2)
    }

    pub(crate) fn current_total_lines(&self) -> usize {
        if self.is_proxy_tab() {
            let filter_lines = if matches!(self.mode, Mode::ProxyFilter) {
                1usize
            } else {
                0
            };
            match self.proxy {
                Some(ref p) => p.filtered_log_len(&self.proxy_filter) + 3 + filter_lines,
                None => 1,
            }
        } else {
            match self.service_tab_index().and_then(|i| self.items.get(i)) {
                Some(item) => item.total_lines(),
                None => 0,
            }
        }
    }

    /// Short project-and-branch label shown on the content panel's top border.
    pub(crate) fn panel_title(&self) -> String {
        panel_title_text(
            self.ipc_state.project.as_deref(),
            self.title_branch.as_deref(),
        )
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let sidebar_width = self.tabs.min_width();

        let main =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(sidebar_width)]).split(area);

        let content_area = main[0];
        let sidebar_area = main[1];

        // Clamp scroll_offset before rendering so a resize or new output that
        // shrinks visible height can never leave offset stranded past the top
        // or make bottom (0) unreachable via clamping drift.
        {
            let max = self
                .current_total_lines()
                .saturating_sub(content_area.height.saturating_sub(2) as usize);
            if self.scroll_offset > max {
                self.scroll_offset = max;
            }
        }

        self.tabs.draw(frame, sidebar_area, &self.theme);

        self.content_area = content_area;

        let is_proxy = self.is_proxy_tab();
        let is_shell = self
            .service_tab_index()
            .and_then(|i| self.items.get(i))
            .map(|t| t.is_shell())
            .unwrap_or(false);
        let in_terminal_input = matches!(self.mode, Mode::TerminalInput);

        let instructions = render::draw_instructions(is_proxy, is_shell, in_terminal_input);

        let block = Block::bordered()
            .title_top(
                Line::from(Span::styled(
                    self.panel_title(),
                    Style::default().fg(self.theme.highlight).bold(),
                ))
                .centered(),
            )
            .title_bottom(instructions.centered())
            .border_set(border::THICK);

        if is_proxy {
            self.content_layout.clear();
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
            let tab_index = self.service_tab_index().unwrap_or(self.tabs.index);
            self.content_layout = render::draw_terminal_content(
                frame,
                content_area,
                block,
                &mut self.items,
                tab_index,
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
                Line::from(vec![Span::raw("  s          Switch worktree     ")]),
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

        self.draw_alerts(frame, area);

        if let Some(popup) = &self.switch_popup {
            let config_dir = self.config_path.parent().unwrap_or_else(|| Path::new("."));
            let matches = popup.matches();

            let overlay_width = 80u16.min(area.width.saturating_sub(4));
            let inner_width = overlay_width.saturating_sub(2) as usize;
            let mut lines = vec![
                Line::from(vec![
                    Span::raw(" filter: "),
                    Span::styled(format!("{}▌", popup.filter), Style::default().bold()),
                ]),
                Line::from(""),
            ];
            for (i, wt) in matches.iter().enumerate() {
                let is_current = worktree::is_current_worktree(wt, &popup.worktrees, config_dir);
                let is_running = popup
                    .running
                    .iter()
                    .any(|b| wt.branch.as_deref() == Some(b.as_str()));
                let (prefix, label_style) = if is_current {
                    let p = if i == popup.selected { " >" } else { "  " };
                    (p, Style::default().fg(Color::Rgb(255, 176, 0)).bold())
                } else if i == popup.selected {
                    (" >", Style::default().fg(self.theme.highlight).bold())
                } else {
                    ("  ", Style::default())
                };
                // The current-worktree `*` and the live-branch green `*` are
                // distinct spans, so a selected/current running branch keeps
                // both readable.
                let mut spans = vec![Span::styled(
                    format!("{prefix} {}", wt.label()),
                    label_style,
                )];
                if is_current {
                    spans.push(Span::styled(
                        " *",
                        Style::default().fg(Color::Rgb(255, 176, 0)).bold(),
                    ));
                }
                if is_running {
                    spans.push(Span::styled(" *", Style::default().fg(Color::Blue).bold()));
                }
                // Truncate path to single line to avoid wrap-induced overlay overflow.
                // Reserve space for stars + two spaces + label/prefix already in spans.
                let prefix_len = 3usize; // " >" + space
                let stars_len = (is_current as usize) * 2 + (is_running as usize) * 2;
                let label_len = wt.label().len();
                let used = prefix_len + label_len + stars_len + 2; // 2 = "  " before path
                let avail = inner_width.saturating_sub(used);
                let path_str = wt.path.display().to_string();
                let truncated = if path_str.len() > avail && avail > 3 {
                    // show tail with ellipsis, keeps worktree id visible
                    format!("...{}", &path_str[path_str.len() - (avail - 3)..])
                } else {
                    path_str
                };
                spans.push(Span::styled(
                    format!("  {truncated}"),
                    Style::default().dim(),
                ));
                lines.push(Line::from(spans));
            }
            if matches.is_empty() {
                lines.push(Line::from("  (no matching worktrees)"));
            }
            lines.push(Line::from(""));
            if let Some(status) = &popup.status {
                lines.push(Line::from(Span::styled(
                    format!(" {status}"),
                    Style::default().fg(Color::Yellow),
                )));
            }
            lines.push(Line::from(Span::styled(
                " f search   d terminate ",
                Style::default().dim(),
            )));

            let status_extra = popup
                .status
                .as_ref()
                .map(|s| s.len() as u16 / overlay_width.max(1))
                .unwrap_or(0);
            let overlay_height = ((lines.len() as u16) + 2 + status_extra).min(area.height);
            let overlay_x = (area.width.saturating_sub(overlay_width)) / 2;
            let overlay_y = (area.height.saturating_sub(overlay_height)) / 2;
            let overlay_area = Rect {
                x: overlay_x,
                y: overlay_y,
                width: overlay_width,
                height: overlay_height,
            };

            let block = Block::bordered().title(" Switch worktree ");
            let widget = Paragraph::new(Text::from(lines))
                .block(block)
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: false });

            frame.render_widget(Clear, overlay_area);
            frame.render_widget(widget, overlay_area);
        }
    }

    /// Returns `true` when the visible content changed since the last frame.
    pub(crate) fn visible_content_changed(&self) -> bool {
        if self.is_proxy_tab() {
            let fp = self
                .proxy
                .as_ref()
                .map(|p| p.log_fingerprint())
                .unwrap_or((0, 0));
            return fp != self.last_drawn_proxy_fp;
        }
        let generation = self
            .service_tab_index()
            .and_then(|i| self.items.get(i))
            .map(|t| t.screen_generation())
            .unwrap_or(0);
        generation != self.last_drawn_gen
    }

    /// Returns `true` when a frame should be drawn. Idle ticks do no work: every
    /// event that changes the visible state sets `force_redraw`, and new output
    /// is detected from the content generation.
    pub(crate) fn needs_redraw(&self) -> bool {
        self.force_redraw
            || self.auto_scrolling.is_some()
            || self.visible_content_changed()
            // The worktree popup is the only transient overlay whose contents
            // can outlive a key press; keep it fresh without repainting an idle
            // screen.
            || (self.switch_popup.is_some()
                && self.last_draw.elapsed() >= Duration::from_millis(500))
            // Alerts auto-dismiss, so keep repainting while any are visible.
            || (!self.alerts.is_empty() && self.last_draw.elapsed() >= ALERT_REPAINT)
    }

    /// Records that a frame was drawn, clearing the dirty flag and capturing the
    /// content generation so the next tick can detect new output.
    pub(crate) fn record_drawn(&mut self) {
        self.force_redraw = false;
        self.last_draw = Instant::now();
        self.last_drawn_gen = self
            .service_tab_index()
            .and_then(|i| self.items.get(i))
            .map(|t| t.screen_generation())
            .unwrap_or(0);
        self.last_drawn_proxy_fp = self
            .proxy
            .as_ref()
            .map(|p| p.log_fingerprint())
            .unwrap_or((0, 0));
    }

    /// Renders the bottom-right alert stack and records each box's regions for
    /// click hit-testing. Newer alerts sit nearest the corner.
    fn draw_alerts(&mut self, frame: &mut Frame, area: Rect) {
        self.alert_areas.clear();
        if self.alerts.is_empty() || area.width < 12 || area.height < 4 {
            return;
        }
        let width = ALERT_WIDTH.min(area.width.saturating_sub(2)).max(12);
        let inner = width.saturating_sub(2) as usize;
        let x = area.right().saturating_sub(width).saturating_sub(1);
        let mut cursor = area.bottom().saturating_sub(1);
        // Walk newest -> oldest, stacking upward from the corner.
        for (i, alert) in self.alerts.iter().enumerate().rev() {
            // The box title already carries the warning marker, so strip the
            // message's own leading `⚠ ` from the display only (copy and the
            // exit-time stderr reprint keep the full text).
            let text = alert
                .message
                .strip_prefix("⚠ ")
                .unwrap_or(alert.message.as_str());
            let wrapped = (text.chars().count() / inner.max(1)) as u16 + 1;
            let height = wrapped + 2;
            if height > cursor.saturating_sub(area.y) {
                break;
            }
            cursor = cursor.saturating_sub(height);
            let rect = Rect {
                x,
                y: cursor,
                width,
                height,
            };
            let color = if alert.copied {
                Color::Green
            } else {
                Color::Rgb(255, 176, 0)
            };
            let marker = if alert.copied { " ✓ " } else { " ⚠ " };
            let block = Block::bordered()
                .title(Span::styled(marker, Style::default().fg(color).bold()))
                .border_style(Style::default().fg(color));
            let para = Paragraph::new(Text::from(Span::styled(
                text.to_string(),
                Style::default().fg(color),
            )))
            .block(block)
            .wrap(Wrap { trim: false });
            frame.render_widget(Clear, rect);
            frame.render_widget(para, rect);
            // `✕` close control on the top border, right-aligned.
            let close = Rect {
                x: rect.right().saturating_sub(2),
                y: rect.y,
                width: 1,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled("✕", Style::default().fg(color).bold())),
                close,
            );
            self.alert_areas.push(AlertHit {
                body: rect,
                close,
                index: i,
            });
        }
    }
}

/// Builds the content panel's top title from a project identity and branch.
///
/// Produces `" name (branch) "`, `" name (detached) "` for a detached checkout
/// in a git repo (detected by the `.git` common-dir identity), or `" name "`
/// outside a git repo. Returns an empty string when there is no identity.
pub(crate) fn panel_title_text(project: Option<&str>, branch: Option<&str>) -> String {
    let Some(project) = project else {
        return String::new();
    };
    let name = crate::project::display_name(project);
    if let Some(branch) = branch {
        return format!(" {name} ({branch}) ");
    }
    let is_git = Path::new(project).file_name().is_some_and(|n| n == ".git");
    if is_git {
        format!(" {name} (detached) ")
    } else {
        format!(" {name} ")
    }
}
