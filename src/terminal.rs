use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{
    io::Read,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

const MAX_SCROLLBACK: usize = 2000;

pub fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    style = match cell.fgcolor() {
        vt100::Color::Default => style,
        vt100::Color::Idx(i) => style.fg(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => style.fg(Color::Rgb(r, g, b)),
    };
    style = match cell.bgcolor() {
        vt100::Color::Default => style,
        vt100::Color::Idx(i) => style.bg(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => style.bg(Color::Rgb(r, g, b)),
    };
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if cell.dim() {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

pub struct TerminalSession {
    parser: Arc<Mutex<vt100::Parser>>,
    handler: Option<JoinHandle<()>>,
    writer: Option<Box<dyn std::io::Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    max_cols: u16,
    max_rows: u16,
}

fn scrollback_len(screen: &mut vt100::Screen) -> usize {
    let prev = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let n = screen.scrollback();
    screen.set_scrollback(prev);
    n
}

impl TerminalSession {
    pub fn new() -> std::io::Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system.openpty(size).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
        let cmd = CommandBuilder::new(shell);
        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;

        let mut reader: Box<dyn Read + Send> = pair.master.try_clone_reader().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        let writer: Box<dyn std::io::Write + Send> =
            pair.master.take_writer().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
            })?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, MAX_SCROLLBACK)));
        let parser_clone = parser.clone();

        let handler = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut p = parser_clone.lock().unwrap();
                        p.process(&buf[..n]);
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            parser,
            handler: Some(handler),
            writer: Some(writer),
            child: Some(child),
            master: Some(pair.master),
            max_cols: 80,
            max_rows: 24,
        })
    }

    pub fn write(&mut self, data: &[u8]) {
        if let Some(ref mut w) = self.writer {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    pub fn total_lines(&self) -> usize {
        let mut parser = self.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let (vis_rows, _) = screen.size();
        let sb = scrollback_len(screen);
        sb + vis_rows as usize
    }

    pub fn get_screen(&self, n: usize, offset: usize) -> (Vec<Line<'static>>, usize) {
        let mut parser = self.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let (vis_rows, cols) = screen.size();
        let sb = scrollback_len(screen);
        let total = sb + vis_rows as usize;

        if offset >= total.saturating_sub(1) {
            screen.set_scrollback(0);
            return (vec![Line::from("(top)")], total);
        }

        let scroll_off = offset.min(sb);
        screen.set_scrollback(scroll_off);

        let rows_to_read = n
            .min(vis_rows as usize)
            .min(total.saturating_sub(offset));

        let mut lines = Vec::with_capacity(rows_to_read);
        for row in 0..rows_to_read as u16 {
            let mut last_col = 0u16;
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col) {
                    if !cell.contents().is_empty() {
                        last_col = col;
                    }
                }
            }

            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut buf = String::new();
            let mut cur = Style::default();

            for col in 0..=last_col {
                if let Some(cell) = screen.cell(row, col) {
                    let text = cell.contents();
                    if text.is_empty() {
                        if buf.is_empty() || cur != Style::default() || !buf.chars().all(|c| c == ' ') {
                            if !buf.is_empty() {
                                spans.push(Span::styled(std::mem::take(&mut buf), cur));
                            }
                            cur = Style::default();
                        }
                        buf.push(' ');
                    } else {
                        let s = cell_style(cell);
                        if buf.is_empty() {
                            cur = s;
                            buf.push_str(text);
                        } else if s == cur {
                            buf.push_str(text);
                        } else {
                            spans.push(Span::styled(std::mem::take(&mut buf), cur));
                            cur = s;
                            buf.push_str(text);
                        }
                    }
                }
            }
            if !buf.is_empty() {
                spans.push(Span::styled(buf, cur));
            }
            if spans.is_empty() {
                spans.push(Span::raw(""));
            }
            lines.push(Line::from(spans));
        }

        (lines, total)
    }

    pub fn get_all_lines(&self) -> Vec<String> {
        let mut parser = self.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let (vis_rows, cols) = screen.size();
        let sb = scrollback_len(screen);

        let mut result = Vec::with_capacity(sb + vis_rows as usize);

        for n in (1..=sb).rev() {
            screen.set_scrollback(n);
            let mut line = String::with_capacity(cols as usize);
            for c in 0..cols {
                if let Some(cell) = screen.cell(0, c) {
                    line.push_str(cell.contents());
                }
            }
            result.push(line);
        }

        screen.set_scrollback(0);
        for r in 0..vis_rows {
            let mut line = String::with_capacity(cols as usize);
            for c in 0..cols {
                if let Some(cell) = screen.cell(r, c) {
                    line.push_str(cell.contents());
                }
            }
            result.push(line);
        }

        result
    }

    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        if screen.hide_cursor() {
            return None;
        }
        let (row, col) = screen.cursor_position();
        let (rows, _) = screen.size();
        if row >= rows {
            return None;
        }
        Some((row, col))
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if let Some(ref m) = self.master {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        let mut p = self.parser.lock().unwrap();
        if cols > self.max_cols || rows > self.max_rows {
            let new_cols = cols.max(self.max_cols);
            let new_rows = rows.max(self.max_rows);
            self.max_cols = new_cols;
            self.max_rows = new_rows;
            p.screen_mut().set_size(new_rows, new_cols);
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handler) = self.handler.take() {
            let _ = handler.join();
        }
    }
}
