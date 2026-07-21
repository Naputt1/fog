use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Converts a crossterm [`KeyEvent`] into a byte sequence for PTY input.
///
/// Mappings include:
///  * Enter → `\n`
///  * Backspace → `\x7f`
///  * Tab → `\t`
///  * Esc → `\x1b`
///  * Arrow keys → `\x1b[A` / `\x1b[B` / `\x1b[C` / `\x1b[D`
///  * Home → `\x1b[H`
///  * End → `\x1b[F`
///  * Delete → `\x1b[3~`
///  * PageUp/PageDown → `\x1b[5~` / `\x1b[6~`
///  * Control+letter → byte values 1–26
///  * Regular/shift characters → UTF-8 encoded bytes
///
/// Returns `None` for unmapped keys, function keys, or unsupported modifier combinations.
pub fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
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
            } else if key.modifiers == KeyModifiers::SHIFT || key.modifiers == KeyModifiers::NONE {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    #[test]
    fn test_key_to_enter() {
        let key = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![b'\n']));
    }

    #[test]
    fn test_key_to_backspace() {
        let key = KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![0x7f]));
    }

    #[test]
    fn test_key_to_tab() {
        let key = KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![b'\t']));
    }

    #[test]
    fn test_key_to_esc() {
        let key = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![0x1b]));
    }

    #[test]
    fn test_key_to_arrows() {
        let up = KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(up), Some(b"\x1b[A".to_vec()));

        let down = KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(down), Some(b"\x1b[B".to_vec()));

        let right = KeyEvent {
            code: KeyCode::Right,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(right), Some(b"\x1b[C".to_vec()));

        let left = KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(left), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn test_key_to_home_end_delete() {
        let home = KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(home), Some(b"\x1b[H".to_vec()));

        let end = KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(end), Some(b"\x1b[F".to_vec()));

        let del = KeyEvent {
            code: KeyCode::Delete,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(del), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn test_key_to_page_up_down() {
        let pu = KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(pu), Some(b"\x1b[5~".to_vec()));

        let pd = KeyEvent {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(pd), Some(b"\x1b[6~".to_vec()));
    }

    #[test]
    fn test_key_to_ctrl_a() {
        let key = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![1]));
    }

    #[test]
    fn test_key_to_ctrl_z() {
        let key = KeyEvent {
            code: KeyCode::Char('z'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![26]));
    }

    #[test]
    fn test_key_to_ctrl_upper() {
        let key = KeyEvent {
            code: KeyCode::Char('A'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![1]));
    }

    #[test]
    fn test_key_to_regular_char() {
        let key = KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![b'x']));
    }

    #[test]
    fn test_key_to_shift_char() {
        let key = KeyEvent {
            code: KeyCode::Char('X'),
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![b'X']));
    }

    #[test]
    fn test_key_to_unicode() {
        let key = KeyEvent {
            code: KeyCode::Char('é'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), Some(vec![0xc3, 0xa9]));
    }

    #[test]
    fn test_key_to_unknown() {
        let key = KeyEvent {
            code: KeyCode::F(1),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), None);
    }

    #[test]
    fn test_key_to_ctrl_unknown() {
        let key = KeyEvent {
            code: KeyCode::Char('!'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), None);
    }

    #[test]
    fn test_key_to_alt_modifier() {
        let key = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(key_to_bytes(key), None);
    }
}
