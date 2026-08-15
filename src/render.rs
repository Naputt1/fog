use crate::proxy::{LogEntry, ProxyInstance};
use crate::selection;
use crate::terminal::Terminal;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_proxy_content(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    proxy: &Option<ProxyInstance>,
    scroll_offset: usize,
    proxy_filter: &str,
    mode_filter_active: bool,
    theme: &Theme,
) {
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let visible_height = inner.height as usize;

    let logs = match proxy {
        Some(p) => p.get_logs(),
        None => vec![],
    };

    let filtered_logs: Vec<&LogEntry> = if proxy_filter.is_empty() {
        logs.iter().collect()
    } else {
        let filter_lower = proxy_filter.to_lowercase();
        logs.iter()
            .filter(|entry| {
                entry.method.to_lowercase().contains(&filter_lower)
                    || entry.path.to_lowercase().contains(&filter_lower)
                    || entry.status.to_string().contains(&filter_lower)
                    || entry.upstream.to_lowercase().contains(&filter_lower)
            })
            .collect()
    };

    let header_lines: usize = if mode_filter_active { 4 } else { 3 };
    let total = filtered_logs.len() + header_lines;
    let offset = scroll_offset.min(total.saturating_sub(visible_height));

    let scrollbar_shown = inner.width > 2 && total > visible_height;
    let (text_area, scrollbar_area) = if scrollbar_shown {
        let chunks = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        (chunks[0], chunks[1])
    } else {
        (inner, Rect::default())
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    if mode_filter_active {
        let filter_display = if proxy_filter.is_empty() {
            " Filter: (type to filter)".to_string()
        } else {
            format!(" Filter: {}", proxy_filter)
        };
        lines.push(Line::from(Span::styled(
            filter_display,
            Style::default().fg(theme.proxy),
        )));
    }

    let status_line = match proxy {
        Some(p) if p.is_running() => {
            format!(" Proxy listening on port {} (running)", p.port)
        }
        Some(_) => " Proxy (stopped)".to_string(),
        None => " Proxy (not configured)".to_string(),
    };
    lines.push(Line::from(Span::styled(
        status_line,
        Style::default().fg(theme.proxy).bold(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            " {:<6} {:<35} {:<5} {:<8} {}",
            "METHOD", "PATH", "STATUS", "LATENCY", "UPSTREAM"
        ),
        Style::default().dim(),
    )));

    for entry in filtered_logs
        .iter()
        .rev()
        .skip(offset.saturating_sub(header_lines))
        .take(visible_height.saturating_sub(header_lines))
    {
        let status_style = match entry.status {
            0 => Style::default().dim(),
            200..=299 => Style::default().fg(theme.status_200),
            300..=399 => Style::default().fg(theme.status_300),
            400..=499 => Style::default().fg(theme.status_400),
            _ => Style::default().fg(theme.status_500).bold(),
        };
        let status_str = if entry.status == 0 {
            String::new()
        } else {
            format!("{}", entry.status)
        };
        let latency_str = if entry.status == 0 {
            String::new()
        } else {
            format!("{}ms", entry.latency_ms)
        };
        let method_span = if entry.ws {
            Span::styled(
                format!(" {:<6}", "WS"),
                Style::default().fg(theme.proxy).bold(),
            )
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

    frame.render_widget(block, area);
    let widget = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(widget, text_area);

    if scrollbar_shown {
        render_scrollbar(
            frame,
            scrollbar_area,
            total,
            visible_height,
            scroll_offset,
            theme,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_terminal_content(
    frame: &mut Frame,
    content_area: Rect,
    block: Block<'static>,
    items: &mut [Terminal],
    tab_index: usize,
    scroll_offset: usize,
    select_start: Option<(usize, usize)>,
    select_end: Option<(usize, usize)>,
    in_terminal_input: bool,
    current_total_lines: usize,
    theme: &Theme,
) -> selection::RowLayout {
    let inner = content_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let visible_height = inner.height;

    let scrollbar_shown = inner.width > 2 && current_total_lines > visible_height as usize;
    let (text_area, scrollbar_area) = if scrollbar_shown {
        let chunks = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        (chunks[0], chunks[1])
    } else {
        (inner, Rect::default())
    };

    if let Some(item) = items.get_mut(tab_index) {
        item.resize(text_area.width, visible_height);
    }

    let (lines, _total) = match items.get_mut(tab_index) {
        Some(item) => item.get_screen(visible_height as usize, scroll_offset),
        None => (vec![Line::from("no tab")], 0),
    };
    // Wrap each logical line into physical rows of at most text_area.width, so
    // long lines stay fully visible (wrapped) instead of being truncated, and
    // so the row<->content mapping is exact. The layout is returned for mouse
    // selection to reuse.
    let window_start = current_total_lines
        .saturating_sub(scroll_offset)
        .saturating_sub(visible_height as usize);
    let (mut rows, mut layout) =
        selection::build_layout(&lines, window_start, text_area.width as usize);

    // Anchor to the bottom: when wrapped rows overflow the viewport the oldest
    // ones clip from the top so the newest output is always visible.
    let inner_h = visible_height as usize;
    let over = rows.len().saturating_sub(inner_h);
    if over > 0 {
        rows.drain(0..over);
        layout.drain(0..over);
    } else {
        while rows.len() < inner_h {
            rows.push(Line::from(vec![Span::raw("")]));
            layout.push(None);
        }
    }

    selection::apply_sel(&mut rows, &layout, select_start, select_end);

    frame.render_widget(block, content_area);
    // Rows are already wrapped to fit, so render them 1:1 (no Paragraph wrap).
    let widget = Paragraph::new(Text::from(rows));
    frame.render_widget(widget, text_area);

    if scrollbar_shown {
        render_scrollbar(
            frame,
            scrollbar_area,
            current_total_lines,
            visible_height as usize,
            scroll_offset,
            theme,
        );
    }

    if in_terminal_input
        && scroll_offset == 0
        && let Some(item) = items.get(tab_index)
        && let Some((row, col)) = item.cursor_position()
    {
        let cursor_line = window_start + row as usize;
        // Locate the physical row showing the cursor's logical line/column.
        let mut pos = None;
        for (pidx, entry) in layout.iter().enumerate() {
            if let Some((line, col_off)) = entry
                && *line == cursor_line
                && *col_off <= col as usize
            {
                pos = Some((pidx, *col_off));
            }
        }
        if let Some((pidx, col_off)) = pos {
            let x =
                content_area.x + 1 + (col - col_off as u16).min(text_area.width.saturating_sub(1));
            let y = content_area.y + 1 + pidx as u16;
            if x < content_area.right() && y < content_area.bottom() {
                frame.set_cursor_position(Position { x, y });
            }
        }
    }

    layout
}

pub(crate) fn draw_instructions(
    is_proxy: bool,
    is_shell: bool,
    in_terminal_input: bool,
) -> Line<'static> {
    if in_terminal_input {
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
            " / ".into(),
            "filter".blue().bold(),
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
            " S ".into(),
            "switch-wt".blue().bold(),
        ])
    }
}

fn render_scrollbar(
    frame: &mut Frame,
    scrollbar_area: Rect,
    total_lines: usize,
    visible_height: usize,
    scroll_offset: usize,
    theme: &Theme,
) {
    if scrollbar_area.width == 0 || total_lines <= visible_height {
        return;
    }
    let max_scroll = total_lines.saturating_sub(visible_height);
    let position = max_scroll.saturating_sub(scroll_offset);
    let mut state = ScrollbarState::new(max_scroll + 1).position(position);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol("█")
        .thumb_style(Style::default().fg(theme.scrollbar))
        .track_symbol(Some("░"))
        .track_style(Style::default().fg(theme.scrollbar).dim());
    frame.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
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
