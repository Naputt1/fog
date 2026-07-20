use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{
    fs,
    io::{self, Read, Write},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

const MAX_SCROLLBACK: usize = 2000;
const INITIAL_COLS: u16 = 256;

#[derive(Debug, Clone, PartialEq)]
pub enum Init {
    Shell,
    Command { path: String, cmd: String },
}

pub struct Terminal {
    pub init: Init,
    pub name: String,
    pub stopped: bool,
    pub save_logs: bool,
    parser: Arc<Mutex<vt100::Parser>>,
    handler: Option<JoinHandle<()>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    max_cols: u16,
    max_rows: u16,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("init", &self.init)
            .field("name", &self.name)
            .field("stopped", &self.stopped)
            .field("handler", &self.handler)
            .field("child", &self.child)
            .finish()
    }
}

fn scrollback_len(screen: &mut vt100::Screen) -> usize {
    let prev = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let n = screen.scrollback();
    screen.set_scrollback(prev);
    n
}

fn cell_style(cell: &vt100::Cell) -> Style {
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

fn spawn_reader(
    parser: Arc<Mutex<vt100::Parser>>,
    mut reader: Box<dyn Read + Send>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut p) = parser.lock() {
                        p.process(&buf[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

impl Terminal {
    pub fn spawn_shell(name: String) -> io::Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
        let cmd = CommandBuilder::new(shell);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, MAX_SCROLLBACK)));
        let handler = spawn_reader(parser.clone(), reader);

        Ok(Self {
            init: Init::Shell,
            name,
            stopped: false,
            save_logs: false,
            parser,
            handler: Some(handler),
            writer: Some(writer),
            child: Some(child),
            master: Some(pair.master),
            max_cols: 80,
            max_rows: 24,
        })
    }

    pub fn spawn_command(path: &str, cmd: &str, name: String) -> io::Result<Self> {
        let mut t = Self {
            init: Init::Command {
                path: path.to_string(),
                cmd: cmd.to_string(),
            },
            name,
            stopped: false,
            save_logs: false,
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, MAX_SCROLLBACK))),
            handler: None,
            writer: None,
            child: None,
            master: None,
            max_cols: 80,
            max_rows: 24,
        };
        t.spawn_into(path, cmd)?;
        Ok(t)
    }

    fn spawn_into(&mut self, path: &str, cmd: &str) -> io::Result<()> {
        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows: 24,
            cols: INITIAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
        let mut cmd_builder = CommandBuilder::new(&shell);
        cmd_builder.cwd(path);

        let child = pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let _ = writeln!(writer, "cd {} && {}", path, cmd);

        self.parser = Arc::new(Mutex::new(vt100::Parser::new(24, INITIAL_COLS, MAX_SCROLLBACK)));
        self.handler = Some(spawn_reader(self.parser.clone(), reader));
        self.writer = Some(writer);
        self.child = Some(child);
        self.master = Some(pair.master);
        self.max_cols = INITIAL_COLS;
        self.max_rows = 24;

        Ok(())
    }

    pub fn is_shell(&self) -> bool {
        matches!(self.init, Init::Shell)
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

        if rows_to_read == 0 {
            return (vec![], total);
        }

        let mut lines = Vec::with_capacity(rows_to_read);
        for row in 0..rows_to_read as u16 {
            let mut last_col = 0u16;
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col)
                    && !cell.contents().is_empty()
                {
                    last_col = col;
                }
            }

            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut buf = String::new();
            let mut cur = Style::default();

            for col in 0..=last_col {
                if let Some(cell) = screen.cell(row, col) {
                    let text = cell.contents();
                    if text.is_empty() {
                        if buf.is_empty()
                            || cur != Style::default()
                            || !buf.chars().all(|c| c == ' ')
                        {
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

    fn kill_inner(&mut self) {
        if let Some(ref child) = self.child {
            if let Some(pid) = child.process_id() {
                let pid = pid as libc::pid_t;
                let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
                kill_descendants(pid);
            }
        }

        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        if let Some(handler) = self.handler.take() {
            let _ = handler.join();
        }

        self.master = None;
        self.writer = None;
    }

    pub fn restart(&mut self) -> io::Result<()> {
        let (path, cmd) = match &self.init {
            Init::Command { path, cmd } => (path.clone(), cmd.clone()),
            Init::Shell => {
                return Err(io::Error::other("cannot restart a shell tab"));
            }
        };
        self.kill_inner();
        self.stopped = false;
        self.spawn_into(&path, &cmd)
    }

    pub fn refresh_status(&mut self) {
        if self.stopped {
            return;
        }
        if let Some(ref child) = self.child {
            if let Some(pid) = child.process_id() {
                let mut status: i32 = 0;
                let ret = unsafe {
                    libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG)
                };
                if ret != 0 {
                    self.stopped = true;
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn kill_descendants(pid: libc::pid_t) {
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(pid);

    while let Some(current_pid) = queue.pop_front() {
        unsafe {
            let byte_count = libc::proc_listchildpids(current_pid, std::ptr::null_mut(), 0);
            if byte_count > 0 {
                let pid_count = byte_count as usize / std::mem::size_of::<libc::pid_t>();
                let mut children: Vec<libc::pid_t> = vec![0; pid_count];
                libc::proc_listchildpids(
                    current_pid,
                    children.as_mut_ptr() as *mut libc::c_void,
                    byte_count,
                );
                for &child_pid in &children {
                    if child_pid > 0 {
                        libc::kill(child_pid, libc::SIGKILL);
                        queue.push_back(child_pid);
                    }
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn kill_descendants(_pid: libc::pid_t) {}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.save_logs {
            if let Init::Command { .. } = &self.init {
                let _ = fs::create_dir_all("temp");
                let text = self.get_all_lines().join("\n");
                let _ = fs::write(format!("temp/{}.txt", self.name), &text);
            }
        }
        self.kill_inner();
    }
}
