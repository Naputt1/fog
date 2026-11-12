//! Terminal presentation: ANSI colour, table layout, width clamping and text
//! truncation.
//!
//! Everything here is pure: it returns strings and never writes them, so a
//! caller decides where output goes. Colour is chosen by the caller (`color`),
//! never detected here, so a test can render plain regardless of its stdout.

use crate::service_state::{ServiceState, Tone};
use std::io::{IsTerminal, stderr, stdout};

/// ANSI SGR codes for the checklist markers: a blue spinner for a service
/// still draining, a green tick for one that has stopped.
pub(crate) const MARKER_BLUE: &str = "\x1b[94m";
pub(crate) const MARKER_GREEN: &str = "\x1b[92m";
pub(crate) const MARKER_RED: &str = "\x1b[91m";
pub(crate) const ANSI_RESET: &str = "\x1b[0m";

/// Wraps `text` in `ansi` when `enable`, else returns it unchanged so piped
/// output (which is not a terminal) stays plain.
pub(crate) fn colored(text: &str, ansi: &str, enable: bool) -> String {
    if enable {
        format!("{ansi}{text}{ANSI_RESET}")
    } else {
        text.to_string()
    }
}

/// ANSI SGR dim, for de-emphasized table states (stopped/unknown).
pub(crate) const ANSI_DIM: &str = "\x1b[2m";

/// Whether styled stdout is wanted: it is a terminal and the user has not
/// opted out with `NO_COLOR`.
pub(crate) fn color_enabled() -> bool {
    stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Whether styled stderr is wanted: it is a terminal and the user has not
/// opted out with `NO_COLOR`.
pub(crate) fn stderr_color_enabled() -> bool {
    stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// The colored `error:` label that prefixes an error line on stderr.
pub(crate) fn error_label() -> String {
    colored("error:", MARKER_RED, stderr_color_enabled())
}

/// ANSI color for a status label, or `None` to leave it plain.
pub(crate) fn state_color(state: &str) -> Option<&'static str> {
    // `:down` is the proxy column's value rather than a service state, but it
    // reads as a failure, so it keeps the error colour.
    if state == ":down" {
        return Some(MARKER_RED);
    }
    match ServiceState::parse(state)?.tone() {
        Tone::Success => Some(MARKER_GREEN),
        // No state reports a warning tone today; if one does, it should stand
        // out like an error rather than fade into the dim neutral colour.
        Tone::Error | Tone::Warning => Some(MARKER_RED),
        Tone::Info => Some(MARKER_BLUE),
        Tone::Neutral => Some(ANSI_DIM),
    }
}

/// Renders a `service  status` table for `fog ls` and `fog logs`, coloring the
/// status column when `color`. The caller decides, so a test can render the
/// table plain regardless of whether stdout is a terminal.
pub(crate) fn service_table(rows: &[(String, String)], indent: &str, color: bool) -> Vec<String> {
    let width = rows
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0)
        .max("service".len());
    let mut lines = vec![format!("{indent}{:<width$}  status", "service")];
    for (name, state) in rows {
        let state = match state_color(state) {
            Some(ansi) => colored(state, ansi, color),
            None => state.clone(),
        };
        lines.push(format!("{indent}{:<width$}  {state}", name));
    }
    lines
}

/// Terminal width in columns, with a readable fallback when stdout is not a
/// terminal (or the size cannot be read).
pub(crate) fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(100)
}

/// Shortens `line` to `width` characters, ending a cut line with `…`.
pub(crate) fn truncate_to(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_string();
    }
    match width {
        0 => String::new(),
        1 => "…".to_string(),
        _ => {
            let mut out: String = line.chars().take(width - 1).collect();
            out.push('…');
            out
        }
    }
}

/// Strips ANSI escape sequences from `s`, producing plain text. Used to render
/// the raw PTY output captured in detached log files.
pub(crate) fn strip_ansi(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\x1b' => {
                if i + 1 >= chars.len() {
                    break;
                }
                match chars[i + 1] {
                    // CSI: consume until the final byte (0x40–0x7e).
                    '[' => {
                        i += 2;
                        while i < chars.len() && !('\u{40}'..='\u{7e}').contains(&chars[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    // OSC: consume until BEL or ST (`ESC \`).
                    ']' => {
                        i += 2;
                        loop {
                            if i >= chars.len() {
                                break;
                            }
                            if chars[i] == '\u{07}' {
                                i += 1;
                                break;
                            }
                            if chars[i] == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    // Two-character escape (e.g. ESC M): skip the second char.
                    _ => i += 2,
                }
            }
            // Drop lone carriage returns so `\r\n` renders as clean lines.
            '\r' => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Formats a byte count for one-line summaries (`512 B`, `24.1 MiB`).
pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(8 * 1024 * 1024), "8.0 MiB");
        assert_eq!(format_bytes(23 * 1024 * 1024 * 1024), "23.0 GiB");
    }

    #[test]
    fn test_strip_ansi_removes_escape_sequences() {
        let input = "\x1b[1;31merror\x1b[0m \x1b[38;2;255;128;0mok\r\nplain";
        let out = strip_ansi(input);
        assert_eq!(out, "error ok\nplain");
    }

    #[test]
    fn test_strip_ansi_handles_osc() {
        // OSC 52 clipboard / OSC title sequences must be consumed entirely.
        let input = "\x1b]52;c;QUJD\x07title\x1b]0;fog\x1b\\rest";
        let out = strip_ansi(input);
        assert_eq!(out, "titlerest");
    }

    #[test]
    fn test_strip_ansi_preserves_plain_text() {
        assert_eq!(strip_ansi("hello world"), "hello world");
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn test_truncate_to_keeps_short_lines() {
        assert_eq!(truncate_to("hello", 5), "hello");
        assert_eq!(truncate_to("hello", 6), "hello");
    }

    #[test]
    fn test_truncate_to_cuts_with_ellipsis() {
        assert_eq!(truncate_to("hello world", 5), "hell…");
        assert_eq!(truncate_to("hello", 1), "…");
        assert_eq!(truncate_to("hello", 0), "");
    }

    #[test]
    fn test_truncate_to_counts_chars_not_bytes() {
        assert_eq!(truncate_to("aé漢🙂", 3), "aé…");
    }

    #[test]
    fn test_state_color_maps_known_states() {
        assert_eq!(state_color("healthy"), Some(MARKER_GREEN));
        assert_eq!(state_color("unhealthy"), Some(MARKER_RED));
        assert_eq!(state_color(":down"), Some(MARKER_RED));
        assert_eq!(state_color("starting"), Some(MARKER_BLUE));
        assert_eq!(state_color("stopped"), Some(ANSI_DIM));
        assert_eq!(state_color(":3000"), None);
    }

    #[test]
    fn test_service_table_aligns_names() {
        let rows = vec![
            ("api".to_string(), "healthy".to_string()),
            ("frontend".to_string(), "stopped".to_string()),
        ];
        // Colour is chosen by the caller, so this renders plain even when the
        // test runs with stdout attached to a terminal.
        let lines = service_table(&rows, "  ", false);
        assert_eq!(lines[0], "  service   status");
        assert_eq!(lines[1], "  api       healthy");
        assert_eq!(lines[2], "  frontend  stopped");
    }
}
