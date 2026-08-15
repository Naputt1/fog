use crate::terminal::Terminal;
use base64::Engine as _;
use ratatui::buffer::CellWidth;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, StyledGrapheme};
use std::io::Write;

/// For each physical render row of a wrapped pane, the content it shows:
/// `Some((absolute_line, column_offset))` for a wrapped chunk of a line, or
/// `None` for padding rows below short content.
pub(crate) type RowLayout = Vec<Option<(usize, usize)>>;

/// Wraps the logical lines of a pane into physical render rows of at most
/// `tw` cells, mirroring terminal behavior (wrap on any character, never lose
/// the tail of a long line). Rows are produced in order; each row is paired
/// with the `(absolute_line, column_offset)` it shows so mouse selection can
/// map a rendered row back to exact content coordinates.
///
/// `window_start` is the absolute line index of `lines[0]`. An empty line
/// becomes a single empty row.
pub(crate) fn build_layout(
    lines: &[Line<'static>],
    window_start: usize,
    tw: usize,
) -> (Vec<Line<'static>>, RowLayout) {
    let tw = tw.max(1);
    let mut rows = Vec::new();
    let mut layout = Vec::new();
    for (rel, line) in lines.iter().enumerate() {
        let line_idx = window_start + rel;
        let graphemes: Vec<StyledGrapheme<'_>> = line
            .spans
            .iter()
            .flat_map(|s| s.styled_graphemes(Style::default()))
            .collect();
        if graphemes.is_empty() {
            rows.push(Line::from(vec![Span::raw("")]));
            layout.push(Some((line_idx, 0)));
            continue;
        }
        let mut chunk = Vec::new();
        let mut width = 0usize;
        let mut col = 0usize;
        for grapheme in graphemes {
            let w = grapheme.symbol.cell_width() as usize;
            if width + w > tw && !chunk.is_empty() {
                rows.push(chunk_line(std::mem::take(&mut chunk)));
                layout.push(Some((line_idx, col)));
                col += width;
                width = 0;
            }
            width += w;
            chunk.push(grapheme);
        }
        if !chunk.is_empty() {
            rows.push(chunk_line(std::mem::take(&mut chunk)));
            layout.push(Some((line_idx, col)));
        }
    }
    (rows, layout)
}

/// Builds a styled `Line` from a wrapped chunk of graphemes, coalescing
/// adjacent graphemes that share a style into a single span.
fn chunk_line(chunk: Vec<StyledGrapheme<'_>>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for g in chunk {
        if let Some(last) = spans.last_mut()
            && last.style == g.style
        {
            last.content.to_mut().push_str(g.symbol);
        } else {
            spans.push(Span::styled(g.symbol.to_string(), g.style));
        }
    }
    Line::from(spans)
}

/// Converts screen coordinates to a content line/column position.
///
/// Accounts for the content area's inner padding (1-cell border). When a
/// [`RowLayout`] is supplied (the terminal pane, which wraps long rows) the
/// given row is mapped through the layout so the resulting coordinates always
/// match what is actually rendered on that row. Without a layout (the proxy
/// log pane) the previous logical-line window formula is used.
///
/// # Arguments
/// * `x` - Screen x-coordinate.
/// * `y` - Screen y-coordinate.
/// * `content_area` - The content area rectangle (including border).
/// * `scroll_offset` - Current scroll offset from the bottom.
/// * `total_lines` - Total number of lines in the terminal.
/// * `layout` - Physical-row layout of the last render, or `None`.
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
    layout: Option<&[Option<(usize, usize)>]>,
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
    if let Some(layout) = layout {
        let Some(Some((line, col_off))) = layout.get(row) else {
            return None;
        };
        if *line >= total_lines {
            return None;
        }
        return Some((*line, col_off + col));
    }
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

/// Applies visual selection highlighting (reversed style) to a slice of
/// physical rows, using the wrap-aware [`RowLayout`] to locate each row's
/// `(line, column_offset)`.
///
/// # Arguments
/// * `rows` - The wrapped physical rows to modify in place.
/// * `layout` - The parallel physical-row layout for `rows`.
/// * `select_start` - The start of the selection, if any.
/// * `select_end` - The end of the selection, if any.
pub(crate) fn apply_sel(
    rows: &mut [Line<'static>],
    layout: &[Option<(usize, usize)>],
    select_start: Option<(usize, usize)>,
    select_end: Option<(usize, usize)>,
) {
    let Some(start) = select_start else { return };
    let Some(end) = select_end else { return };
    let (sel_start, sel_end) = if start.0 < end.0 || (start.0 == end.0 && start.1 <= end.1) {
        (start, end)
    } else {
        (end, start)
    };
    for (row, line) in rows.iter_mut().enumerate() {
        let Some((line_idx, col_off)) = layout.get(row).copied().flatten() else {
            continue;
        };
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
        // This row shows columns [col_off, col_off + chunk width) of the line.
        let chunk_width = line.width();
        let lo = sc.saturating_sub(col_off).min(chunk_width);
        let hi = ec.saturating_sub(col_off).min(chunk_width);
        if lo < hi {
            apply_reversed(line, lo, hi);
        }
    }
}

/// Reverses the spans of a line over the character range `[lo, hi)`.
fn apply_reversed(line: &mut Line<'static>, lo: usize, hi: usize) {
    let spans = std::mem::take(&mut line.spans);
    let mut new_spans = Vec::new();
    let mut char_off = 0;
    for span in spans {
        let span_len = span.content.chars().count();
        let span_start = char_off;
        let span_end = span_start + span_len;
        if span_end <= lo || span_start >= hi {
            new_spans.push(span);
        } else {
            let content = span.content.into_owned();
            let orig_style = span.style;
            let chars: Vec<char> = content.chars().collect();
            let before_end = lo.saturating_sub(span_start).min(chars.len());
            let after_start = hi.saturating_sub(span_start).min(chars.len());
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
        let result = screen_to_content(11, 6, area, 0, 30, None);
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
        let result = screen_to_content(9, 6, area, 0, 30, None);
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
        let result = screen_to_content(51, 6, area, 0, 30, None);
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
        let result = screen_to_content(11, 4, area, 0, 30, None);
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
        let result = screen_to_content(11, 26, area, 0, 30, None);
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
        let result = screen_to_content(1, 1, area, 5, 25, None);
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
        let result = screen_to_content(1, 9, area, 0, 5, None);
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
        let lines = vec![Line::from("hello")];
        let (mut rows, layout) = build_layout(&lines, 0, 100);
        apply_sel(&mut rows, &layout, None, None);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_apply_sel_single_line() {
        let lines = vec![Line::from("hello world")];
        let (mut rows, layout) = build_layout(&lines, 0, 100);
        apply_sel(&mut rows, &layout, Some((0, 0)), Some((0, 5)));
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.style == Style::new().reversed())
        );
    }

    #[test]
    fn test_apply_sel_reverse_order() {
        let lines = vec![Line::from("hello world")];
        let (mut rows, layout) = build_layout(&lines, 0, 100);
        apply_sel(&mut rows, &layout, Some((0, 5)), Some((0, 0)));
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.style == Style::new().reversed())
        );
    }

    #[test]
    fn test_apply_sel_outside_visible_range() {
        let lines = vec![Line::from("hello")];
        let (mut rows, layout) = build_layout(&lines, 0, 100);
        apply_sel(&mut rows, &layout, Some((5, 0)), Some((5, 3)));
    }

    #[test]
    fn test_apply_sel_multi_line() {
        let lines = vec![Line::from("line1"), Line::from("line2")];
        let (mut rows, layout) = build_layout(&lines, 0, 100);
        apply_sel(&mut rows, &layout, Some((0, 2)), Some((1, 3)));
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.style == Style::new().reversed())
        );
        assert!(
            rows[1]
                .spans
                .iter()
                .any(|s| s.style == Style::new().reversed())
        );
    }

    #[test]
    fn test_apply_sel_wrapped_chunk_highlight() {
        // "abcdefghij" wraps at 4 into "abcd"(0-4), "efgh"(4-8), "ij"(8-10).
        // Selecting cols 5..9 highlights "fgh" in chunk 1 and "i" in chunk 2.
        let lines = vec![Line::from("abcdefghij")];
        let (mut rows, layout) = build_layout(&lines, 0, 4);
        apply_sel(&mut rows, &layout, Some((0, 5)), Some((0, 9)));
        assert_eq!(reversed_chunks(&rows[0]), Vec::<&str>::new());
        assert_eq!(reversed_chunks(&rows[1]), vec!["fgh"]);
        assert_eq!(reversed_chunks(&rows[2]), vec!["i"]);
    }

    fn reversed_chunks(line: &Line<'static>) -> Vec<String> {
        line.spans
            .iter()
            .filter(|s| s.style == Style::new().reversed())
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn test_build_layout_wraps_long_lines() {
        let lines = vec![Line::from("abcdefghij"), Line::from("xyz")];
        let (rows, layout) = build_layout(&lines, 5, 4);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].to_string(), "abcd");
        assert_eq!(rows[1].to_string(), "efgh");
        assert_eq!(rows[2].to_string(), "ij");
        assert_eq!(rows[3].to_string(), "xyz");
        assert_eq!(
            layout,
            vec![Some((5, 0)), Some((5, 4)), Some((5, 8)), Some((6, 0))]
        );
    }

    #[test]
    fn test_build_layout_empty_line() {
        let lines = vec![Line::from("")];
        let (rows, layout) = build_layout(&lines, 0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(layout, vec![Some((0, 0))]);
    }

    #[test]
    fn test_build_layout_wide_chars() {
        // "あ" is 2 cells wide, "b" is 1.
        let lines = vec![Line::from("あb")];
        let (rows, layout) = build_layout(&lines, 7, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].to_string(), "あ");
        assert_eq!(rows[1].to_string(), "b");
        assert_eq!(layout, vec![Some((7, 0)), Some((7, 2))]);
    }

    #[test]
    fn test_build_layout_exact_width_is_single_row() {
        let lines = vec![Line::from("abcd")];
        let (rows, layout) = build_layout(&lines, 3, 4);
        assert_eq!(rows.len(), 1);
        assert_eq!(layout, vec![Some((3, 0))]);
    }

    #[test]
    fn test_screen_to_content_with_layout() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 5, // inner region 8x3
        };
        let layout = vec![Some((5, 0)), Some((5, 4)), None];
        assert_eq!(
            screen_to_content(1, 1, area, 0, 30, Some(&layout)),
            Some((5, 0))
        );
        // Wrapped chunk starting at column 4; click column 2 -> line column 6.
        assert_eq!(
            screen_to_content(3, 2, area, 0, 30, Some(&layout)),
            Some((5, 6))
        );
        // Padded row below short content is not selectable.
        assert_eq!(screen_to_content(1, 3, area, 0, 30, Some(&layout)), None);
        // Stale layout pointing past the current total is rejected.
        assert_eq!(screen_to_content(1, 1, area, 0, 4, Some(&layout)), None);
    }

    #[test]
    fn test_screen_to_content_boundaries() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        assert_eq!(screen_to_content(0, 0, area, 0, 10, None), None);
        assert_eq!(screen_to_content(1, 1, area, 0, 10, None), Some((2, 0)));
        assert_eq!(screen_to_content(8, 8, area, 0, 10, None), Some((9, 7)));
        assert_eq!(screen_to_content(9, 9, area, 0, 10, None), None);
    }
}
