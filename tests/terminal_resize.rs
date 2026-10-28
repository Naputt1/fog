//! Resize semantics of the vt100-backed screen, exercised through the same
//! public surface the UI renders from.
//!
//! The 0.18 parser follows xterm: shrinking the height pushes rows into
//! scrollback and growing pulls them back, so output that scrolled off is not
//! destroyed by resizing the pane. These tests pin that contract; the 0.16
//! parser dropped those rows and grew back into blank lines.

#[cfg(unix)]
mod unix {
    use fog::terminal::Terminal;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn env() -> HashMap<String, String> {
        [("TERM".to_string(), "xterm-256color".to_string())]
            .into_iter()
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("fog-resize-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spawn(dir: &Path, cmd: &str, name: &str) -> Terminal {
        Terminal::spawn_command(
            dir.to_str().unwrap(),
            cmd,
            name.to_string(),
            2000,
            None,
            None,
            env(),
        )
        .unwrap()
    }

    fn text(t: &Terminal) -> String {
        t.get_all_lines().join("\n")
    }

    fn wait_for(t: &Terminal, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if t.get_all_lines().iter().any(|l| l.contains(needle)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(80));
        }
        false
    }

    /// Waits until the parser has stopped changing for a moment, so assertions
    /// do not race a redraw the resize just triggered.
    fn settle(t: &Terminal) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = t.screen_generation();
        let mut stable_since = Instant::now();
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(60));
            let generation = t.screen_generation();
            if generation == last {
                if stable_since.elapsed() > Duration::from_millis(300) {
                    return;
                }
            } else {
                last = generation;
                stable_since = Instant::now();
            }
        }
    }

    /// A pane that shrinks and grows again must hand back every line the
    /// service printed, with no blank padding where content used to be.
    #[test]
    fn shrink_then_grow_keeps_every_printed_line() {
        let dir = temp_dir("scrollback");
        let mut terminal = spawn(&dir, "seq 1 40", "seq");
        assert!(wait_for(&terminal, "40", 15), "seq output never arrived");

        for rows in [8, 30, 24] {
            terminal.resize(80, rows);
            settle(&terminal);
            assert_eq!(
                terminal.screen_size().0,
                rows,
                "parser height must track the visible area"
            );
        }

        let lines = terminal.get_all_lines();
        let missing: Vec<usize> = (1..=40)
            .filter(|n| !lines.iter().any(|line| line.trim() == n.to_string()))
            .collect();
        assert!(
            missing.is_empty(),
            "shrinking then growing dropped lines {missing:?}; the parser lost scrollback content"
        );

        let visible_blank = lines
            .iter()
            .rev()
            .take(24)
            .filter(|line| line.trim().is_empty())
            .count();
        assert!(
            visible_blank <= 1,
            "the grown screen came back with {visible_blank} blank rows out of 24"
        );

        terminal.stop().ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The alternate screen has no scrollback, so alt-screen apps (`vim`,
    /// `less`) must not accumulate rows here when the pane is resized.
    #[test]
    fn alternate_screen_keeps_no_scrollback_across_resizes() {
        let dir = temp_dir("altscreen");
        let mut terminal = spawn(
            &dir,
            "printf '\\033[?1049h'; seq 1 30; sleep 10",
            "altscreen",
        );
        assert!(wait_for(&terminal, "30", 15), "alt screen never filled");

        for rows in [10u16, 24, 40] {
            terminal.resize(80, rows);
            settle(&terminal);
            assert_eq!(terminal.screen_size().0, rows);
            let (visible, _) = terminal.screen_size();
            assert_eq!(
                terminal.total_lines(),
                usize::from(visible),
                "alt screen must not push rows into scrollback at {rows} rows"
            );
            let (row, _) = terminal.cursor_position().unwrap_or((0, 0));
            assert!(row < rows, "cursor escaped the {rows}-row screen");
        }

        terminal.stop().ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    fn have(binary: &str) -> bool {
        std::process::Command::new(binary)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// Real alt-screen apps must redraw into the resized screen instead of
    /// leaving a blank or stale one. Skipped where the binary is absent.
    #[test]
    fn full_screen_apps_redraw_after_resize() {
        let dir = temp_dir("apps");
        let body = (1..=200)
            .map(|i| format!("line {i:03} alpha bravo charlie delta"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("qa.txt"), body + "\n").unwrap();

        if have("less") {
            let mut less = spawn(&dir, "less -R qa.txt", "less");
            assert!(wait_for(&less, "line 001", 15), "less never drew the file");
            for rows in [12u16, 40, 24] {
                less.resize(80, rows);
                settle(&less);
                assert_eq!(less.screen_size().0, rows);
                let drawn = text(&less);
                assert!(
                    drawn.contains("line 0"),
                    "less left a blank screen after resizing to {rows} rows"
                );
            }
            less.write(b" ");
            settle(&less);
            assert!(
                text(&less).contains("line 0"),
                "less stopped drawing after SPACE"
            );
            less.stop().ok();
        }

        if have("vim") {
            let mut vim = spawn(&dir, "vim -u NONE -N -n qa.txt", "vim");
            assert!(wait_for(&vim, "line 001", 20), "vim never drew the file");
            for rows in [10u16, 30, 24] {
                vim.resize(80, rows);
                settle(&vim);
                assert_eq!(vim.screen_size().0, rows);
                assert!(
                    text(&vim).contains("line 0"),
                    "vim left a blank screen after resizing to {rows} rows"
                );
                if let Some((row, _)) = vim.cursor_position() {
                    assert!(row < rows, "cursor escaped the {rows}-row screen");
                }
            }
            vim.write(b"jj");
            settle(&vim);
            assert_eq!(
                vim.cursor_position().map(|(row, _)| row),
                Some(2),
                "vim did not accept input after the resizes"
            );
            vim.stop().ok();
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
