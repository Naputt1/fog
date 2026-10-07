//! Content-panel rendering: layout, scroll state, and the full `draw` pass.

use super::{ALERT_REPAINT, ALERT_WIDTH, AlertHit, App, Mode};
use crate::click_tab::TabKind;
use crate::render;
use crate::terminal::HealthStatus;
use crate::worktree;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect},
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

    /// Name shown in the content panel's top-left title.
    fn content_title(&self) -> String {
        self.tabs
            .entries
            .get(self.tabs.index)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "fog".to_string())
    }

    /// Top-right status suffix for the content panel: proxy address, shell
    /// marker, or the active service's health.
    fn content_status(&self) -> Line<'static> {
        let theme = &self.theme;
        let span = |text: String, color: Color| {
            Line::from(Span::styled(
                format!("{text} ─"),
                Style::default().fg(color).bold(),
            ))
        };
        if self.is_proxy_tab() {
            return match &self.proxy {
                Some(p) if p.is_running() => span(format!("● :{}", p.bound_port()), theme.proxy),
                Some(_) => span("○ stopped".to_string(), theme.stopped),
                None => span("○ not configured".to_string(), theme.text_muted),
            };
        }
        let entry = self.tabs.entries.get(self.tabs.index);
        if entry.map(|e| e.kind) == Some(TabKind::Terminal) {
            return span("$ shell".to_string(), theme.terminal);
        }
        match entry {
            Some(e) if e.pending => span("◌ waiting".to_string(), theme.status_300),
            Some(e) if e.stopped => span("○ stopped".to_string(), theme.stopped),
            Some(e) => match e.health_status {
                HealthStatus::Healthy => span("● healthy".to_string(), theme.status_200),
                HealthStatus::Starting => span("● starting".to_string(), theme.status_300),
                HealthStatus::Unhealthy => span("● unhealthy".to_string(), theme.stopped),
                HealthStatus::Pending => span("◌ pending".to_string(), theme.status_300),
                HealthStatus::Unknown => span("○ unknown".to_string(), theme.text_muted),
            },
            None => span("● up".to_string(), theme.text_muted),
        }
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Paint the base background, then compose header / body / status.
        frame.render_widget(Block::new().style(Style::default().bg(self.theme.bg)), area);

        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
        let header_area = rows[0];
        let body_area = rows[1];
        let status_area = rows[2];

        let sidebar_width = self.tabs.min_width();
        let cols = Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(1)])
            .split(body_area);
        let sidebar_area = cols[0];
        let content_area = cols[1];

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

        self.draw_header(frame, header_area);
        self.tabs.draw(frame, sidebar_area, &self.theme);

        self.content_area = content_area;

        let is_proxy = self.is_proxy_tab();
        let is_shell = self
            .service_tab_index()
            .and_then(|i| self.items.get(i))
            .map(|t| t.is_shell())
            .unwrap_or(false);
        let in_terminal_input = matches!(self.mode, Mode::TerminalInput);

        // Content is a bordered panel; the render fns draw the block and pad the
        // inner text by one cell. The border and title take the active tab's hue.
        let hue = self.theme.tab_color(self.tabs.index);
        let block = Block::bordered()
            .border_set(border::ROUNDED)
            .border_style(Style::default().fg(hue))
            .style(Style::default().bg(self.theme.bg))
            .title_top(
                Line::from(Span::styled(
                    format!("─┤ {} ├", self.content_title()),
                    Style::default().fg(hue).bold(),
                ))
                .alignment(Alignment::Left),
            )
            .title_top(self.content_status().alignment(Alignment::Right));

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

        self.draw_status(frame, status_area, is_proxy, is_shell, in_terminal_input);

        if self.show_help {
            let theme = &self.theme;
            let keycap = |key: &str| {
                Span::styled(
                    format!(" {key} "),
                    Style::default().fg(theme.key).bg(theme.surface_alt).bold(),
                )
            };
            let desc =
                |d: &str| Span::styled(format!("  {d}"), Style::default().fg(theme.text_muted));
            let help_text = vec![
                Line::from(vec![keycap("q / Ctrl+q"), desc("quit")]),
                Line::from(vec![keycap("j / Right"), desc("next tab")]),
                Line::from(vec![keycap("k / Left"), desc("previous tab")]),
                Line::from(vec![keycap("i"), desc("terminal input mode")]),
                Line::from(vec![keycap("Esc"), desc("exit input mode")]),
                Line::from(vec![keycap("R"), desc("restart service")]),
                Line::from(vec![keycap("t / Ctrl+t"), desc("new shell tab")]),
                Line::from(vec![keycap("x"), desc("close shell tab")]),
                Line::from(vec![keycap("d"), desc("detach (keep session running)")]),
                Line::from(vec![keycap("s"), desc("switch worktree")]),
                Line::from(vec![keycap("g / Home"), desc("scroll to top")]),
                Line::from(vec![keycap("G / End"), desc("scroll to bottom")]),
                Line::from(vec![keycap("Up / Down"), desc("scroll output")]),
                Line::from(vec![keycap("PgUp / PgDn"), desc("scroll by page")]),
                Line::from(vec![keycap("?"), desc("toggle help")]),
            ];

            let overlay_width = 44u16.min(area.width.saturating_sub(4));
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
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(self.theme.border))
                .style(Style::default().bg(self.theme.surface))
                .title(Span::styled(
                    " Help ",
                    Style::default().fg(self.theme.title).bold(),
                ));
            let help = Paragraph::new(Text::from(help_text))
                .block(block)
                .alignment(Alignment::Left);

            frame.render_widget(Clear, overlay_area);
            frame.render_widget(help, overlay_area);
        }

        self.draw_alerts(frame, body_area);

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
                    (p, Style::default().fg(self.theme.key).bold())
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
                        Style::default().fg(self.theme.key).bold(),
                    ));
                }
                if is_running {
                    spans.push(Span::styled(
                        " *",
                        Style::default().fg(self.theme.status_200).bold(),
                    ));
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
            let key = |k: &str| {
                Span::styled(format!(" {k} "), Style::default().fg(self.theme.key).bold())
            };
            let desc = |d: &str| {
                Span::styled(
                    format!(" {d}  "),
                    Style::default().fg(self.theme.text_muted),
                )
            };
            lines.push(Line::from(vec![
                key("f"),
                desc("search"),
                key("d"),
                desc("terminate"),
            ]));

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

            let block = Block::bordered()
                .border_set(border::ROUNDED)
                .border_style(Style::default().fg(self.theme.border))
                .style(Style::default().bg(self.theme.surface))
                .title(Span::styled(
                    " Switch worktree ",
                    Style::default().fg(self.theme.title).bold(),
                ));
            let widget = Paragraph::new(Text::from(lines))
                .block(block)
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: false });

            frame.render_widget(Clear, overlay_area);
            frame.render_widget(widget, overlay_area);
        }
    }

    /// One-line header: the `fog` wordmark and project (branch) on the left,
    /// proxy address and service health on the right.
    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let theme = &self.theme;
        frame.render_widget(Block::new().style(Style::default().bg(theme.surface)), area);
        let inner = area.inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let title = self.panel_title();
        let title = title.trim();
        let left = Line::from(vec![
            Span::styled("fog", Style::default().fg(theme.accent).bold()),
            Span::styled(
                if title.is_empty() {
                    String::new()
                } else {
                    format!("  {title}")
                },
                Style::default().fg(theme.text),
            ),
        ]);
        frame.render_widget(Paragraph::new(left), inner);
        frame.render_widget(
            Paragraph::new(self.header_status()).alignment(Alignment::Right),
            inner,
        );
    }

    /// Right-hand header content: proxy state and a per-service health meter.
    fn header_status(&self) -> Line<'static> {
        let theme = &self.theme;
        let mut spans: Vec<Span<'static>> = Vec::new();

        if let Some(p) = &self.proxy {
            let (label, color) = if p.is_running() {
                (format!("⬤ proxy :{}", p.bound_port()), theme.proxy)
            } else {
                ("○ proxy stopped".to_string(), theme.stopped)
            };
            spans.push(Span::styled(label, Style::default().fg(color).bold()));
        }

        let services: Vec<&crate::click_tab::TabEntry> = self
            .tabs
            .entries
            .iter()
            .filter(|e| e.kind == TabKind::Service)
            .collect();
        if !services.is_empty() {
            if !spans.is_empty() {
                spans.push(Span::styled("   ", Style::default()));
            }
            for e in &services {
                let color = if e.pending {
                    theme.status_300
                } else {
                    match e.health_status {
                        HealthStatus::Healthy => theme.status_200,
                        HealthStatus::Unhealthy => theme.stopped,
                        HealthStatus::Starting | HealthStatus::Pending => theme.status_300,
                        HealthStatus::Unknown => theme.text_muted,
                    }
                };
                spans.push(Span::styled(
                    if e.pending { "◌" } else { "●" },
                    Style::default().fg(color),
                ));
            }
            let healthy = services
                .iter()
                .filter(|e| e.health_status == HealthStatus::Healthy)
                .count();
            let color = if healthy == services.len() {
                theme.status_200
            } else {
                theme.status_300
            };
            spans.push(Span::styled(
                format!(" {healthy}/{} healthy", services.len()),
                Style::default().fg(color).bold(),
            ));
        }

        Line::from(spans)
    }

    /// One-line status bar: the current mode on the left, contextual key hints
    /// on the right.
    fn draw_status(
        &self,
        frame: &mut Frame,
        area: Rect,
        is_proxy: bool,
        is_shell: bool,
        in_terminal_input: bool,
    ) {
        let theme = &self.theme;
        frame.render_widget(Block::new().style(Style::default().bg(theme.surface)), area);
        let inner = area.inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let (label, color) = match self.mode {
            Mode::TerminalInput => ("INPUT", theme.accent),
            Mode::ProxyFilter => ("FILTER", theme.proxy),
            Mode::Normal => ("NORMAL", theme.text_muted),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(label, Style::default().fg(color).bold())),
            inner,
        );

        let hints = render::draw_instructions(is_proxy, is_shell, in_terminal_input, theme);
        frame.render_widget(Paragraph::new(hints).alignment(Alignment::Right), inner);
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
                .border_set(border::ROUNDED)
                .title(Span::styled(marker, Style::default().fg(color).bold()))
                .border_style(Style::default().fg(color))
                .style(Style::default().bg(self.theme.surface));
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
