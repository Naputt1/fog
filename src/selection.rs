use crate::terminal::Terminal;
use base64::Engine as _;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::io::Write;

/// Converts screen coordinates to a content line/column position.
///
/// Accounts for the content area's inner padding (1-cell border) and the current
/// scroll offset.
///
/// # Arguments
/// * `x` - Screen x-coordinate.
/// * `y` - Screen y-coordinate.
/// * `content_area` - The content area rectangle (including border).
/// * `scroll_offset` - Current scroll offset from the bottom.
/// * `total_lines` - Total number of lines in the terminal.
///
/// # Returns
/// `Some((line_index, column))` if the coordinates fall within the content area,
/// or `None` if outside.
pub(crate) fn screen_to_content(
    x: u16,
    y: u16,
    content_area: Rect,
    scroll_offset: usize,
    total_lines: usize,
) -> Option<(usize, usize)> {
    let inner_x = content_area.x.saturating_add(1);
    let inner_y = content_area.y.saturating_add(1);
    let inner_w = content_area.width.saturating_sub(2);
    let inner_h = content_area.height.saturating_sub(2);
    if x < inner_x || x >= inner_x.saturating_add(inner_w) {
        return None;
    }
    if y < inner_y || y >= inner_y.saturating_add(inner_h) {
        return None;
    }
    let col = (x - inner_x) as usize;
    let row = (y - inner_y) as usize;
    let visible = inner_h as usize;
    let end = total_lines.saturating_sub(scroll_offset);
    let start = end.saturating_sub(visible);
    let line_idx = start.saturating_add(row);
    if line_idx >= total_lines {
        return None;
    }
    Some((line_idx, col))
}

/// Copies selected text to the system clipboard via the OSC 52 escape sequence.
///
/// # Arguments
/// * `start` - The start `(line, column)` of the selection.
/// * `end` - The end `(line, column)` of the selection.
/// * `items` - All terminal instances.
/// * `tab_index` - The index of the active terminal tab to copy from.
pub(crate) fn copy_selection(
    start: (usize, usize),
    end: (usize, usize),
    items: &[Terminal],
    tab_index: usize,
) {
    let (sel_start, sel_end) = if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
        (start, end)
    } else {
        (end, start)
    };
    let lines: Vec<String> = match items.get(tab_index) {
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
            selected.push_str(text);
        }
        if i != sel_end.0 {
            selected.push('\n');
        }
    }
    if !selected.is_empty() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&selected);
        let _ = write!(std::io::stdout(), "\x1b]52;c;{}\x07", encoded);
        let _ = std::io::stdout().flush();
    }
}

/// Applies visual selection highlighting (reversed style) to a slice of styled lines.
///
/// # Arguments
/// * `lines` - The styled lines to modify in place.
/// * `select_start` - The start of the selection, if any.
/// * `select_end` - The end of the selection, if any.
/// * `scroll_offset` - Current scroll offset from the bottom.
/// * `total_lines` - Total number of lines in the terminal.
pub(crate) fn apply_sel(
    lines: &mut [Line<'static>],
    select_start: Option<(usize, usize)>,
    select_end: Option<(usize, usize)>,
    scroll_offset: usize,
    total_lines: usize,
) {
    let Some(start) = select_start else { return };
    let Some(end) = select_end else { return };
    let (sel_start, sel_end) = if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
        (start, end)
    } else {
        (end, start)
    };
    let visible = lines.len();
    let end_idx = total_lines.saturating_sub(scroll_offset);
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

/// Resets all selection state to inactive.
///
/// # Arguments
/// * `selecting` - The selecting flag to set to `false`.
/// * `select_start` - The selection start to clear.
/// * `select_end` - The selection end to clear.
pub(crate) fn clear_selection(
    selecting: &mut bool,
    select_start: &mut Option<(usize, usize)>,
    select_end: &mut Option<(usize, usize)>,
) {
    *selecting = false;
    *select_start = None;
    *select_end = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn test_screen_to_content_inside() {
        let area = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 20,
        };
        let result = screen_to_content(11, 6, area, 0, 30);
        assert_eq!(result, Some((12, 0)));
    }

    #[test]
    fn test_screen_to_content_outside_left() {
        let area = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 20,
        };
        let result = screen_to_content(9, 6, area, 0, 30);
        assert_eq!(result, None);
    }

    #[test]
    fn test_screen_to_content_outside_right() {
        let area = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 20,
        };
        let result = screen_to_content(51, 6, area, 0, 30);
        assert_eq!(result, None);
    }

    #[test]
    fn test_screen_to_content_outside_top() {
        let area = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 20,
        };
        let result = screen_to_content(11, 4, area, 0, 30);
        assert_eq!(result, None);
    }

    #[test]
    fn test_screen_to_content_outside_bottom() {
        let area = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 20,
        };
        let result = screen_to_content(11, 26, area, 0, 30);
        assert_eq!(result, None);
    }

    #[test]
    fn test_screen_to_content_with_scroll_offset() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let result = screen_to_content(1, 1, area, 5, 25);
        assert_eq!(result, Some((2, 0)));
    }

    #[test]
    fn test_screen_to_content_beyond_total_lines() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        };
        let result = screen_to_content(1, 9, area, 0, 5);
        assert_eq!(result, None);
    }

    #[test]
    fn test_clear_selection_resets_state() {
        let mut selecting = true;
        let mut start = Some((1, 2));
        let mut end = Some((3, 4));
        clear_selection(&mut selecting, &mut start, &mut end);
        assert!(!selecting);
        assert_eq!(start, None);
        assert_eq!(end, None);
    }

    #[test]
    fn test_clear_selection_already_clear() {
        let mut selecting = false;
        let mut start: Option<(usize, usize)> = None;
        let mut end: Option<(usize, usize)> = None;
        clear_selection(&mut selecting, &mut start, &mut end);
        assert!(!selecting);
        assert_eq!(start, None);
        assert_eq!(end, None);
    }

    #[test]
    fn test_copy_selection_empty_items() {
        copy_selection((0, 0), (1, 1), &[], 0);
    }

    #[test]
    fn test_copy_selection_out_of_bounds_index() {
        let items: Vec<Terminal> = vec![];
        copy_selection((0, 0), (1, 1), &items, 5);
    }

    #[test]
    fn test_apply_sel_no_selection() {
        let mut lines = vec![Line::from("hello")];
        apply_sel(&mut lines, None, None, 0, 10);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_apply_sel_single_line() {
        let mut lines = vec![Line::from("hello world")];
        apply_sel(&mut lines, Some((0, 0)), Some((0, 5)), 0, 1);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.style == Style::new().reversed())
        );
    }

    #[test]
    fn test_apply_sel_reverse_order() {
        let mut lines = vec![Line::from("hello world")];
        apply_sel(&mut lines, Some((0, 5)), Some((0, 0)), 0, 1);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.style == Style::new().reversed())
        );
    }

    #[test]
    fn test_apply_sel_outside_visible_range() {
        let mut lines = vec![Line::from("hello")];
        apply_sel(&mut lines, Some((5, 0)), Some((5, 3)), 0, 10);
    }

    #[test]
    fn test_apply_sel_multi_line() {
        let mut lines = vec![Line::from("line1"), Line::from("line2")];
        apply_sel(&mut lines, Some((0, 2)), Some((1, 3)), 0, 2);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.style == Style::new().reversed())
        );
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.style == Style::new().reversed())
        );
    }

    #[test]
    fn test_screen_to_content_boundaries() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        assert_eq!(screen_to_content(0, 0, area, 0, 10), None);
        assert_eq!(screen_to_content(1, 1, area, 0, 10), Some((2, 0)));
        assert_eq!(screen_to_content(8, 8, area, 0, 10), Some((9, 7)));
        assert_eq!(screen_to_content(9, 9, area, 0, 10), None);
    }
}
