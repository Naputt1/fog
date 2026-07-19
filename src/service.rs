use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde::Deserialize;
use std::{
    fmt, fs,
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

use crate::terminal::cell_style;

const MAX_SCROLLBACK: usize = 2000;
const COLS: u16 = 256;

fn scrollback_len(screen: &mut vt100::Screen) -> usize {
    let prev = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let n = screen.scrollback();
    screen.set_scrollback(prev);
    n
}

#[derive(Deserialize)]
pub struct Service {
    pub path: String,
    pub cmd: String,

    #[serde(skip)]
    child: Option<Box<dyn Child + Send + Sync>>,
    #[serde(skip)]
    handler: Option<JoinHandle<()>>,
    #[serde(skip)]
    pub parser: Arc<Mutex<vt100::Parser>>,
    #[serde(skip)]
    master: Option<Box<dyn MasterPty + Send>>,
    #[serde(skip)]
    pub stopped: bool,
}

impl fmt::Debug for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Service")
            .field("path", &self.path)
            .field("cmd", &self.cmd)
            .field("child", &self.child)
            .field("handler", &self.handler)
            .field("stopped", &self.stopped)
            .finish()
    }
}

impl Service {
    pub fn run(&mut self) -> Result<(), std::io::Error> {
        let part: Vec<&str> = self.cmd.split_whitespace().collect();

        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut cmd = CommandBuilder::new(part[0]);
        cmd.args(&part[1..]);
        cmd.cwd(&self.path);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        self.child = Some(child);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        self.master = Some(pair.master);

        self.parser = Arc::new(Mutex::new(vt100::Parser::new(24, COLS, MAX_SCROLLBACK)));
        let parser = self.parser.clone();

        let handler = thread::spawn(move || {
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
        });
        self.handler = Some(handler);

        Ok(())
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

        let rows_to_read = n.min(vis_rows as usize).min(total.saturating_sub(offset));

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

    pub fn total_lines(&self) -> usize {
        let mut parser = self.parser.lock().unwrap();
        let screen = parser.screen_mut();
        let (vis_rows, _) = screen.size();
        let sb = scrollback_len(screen);
        sb + vis_rows as usize
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
        p.screen_mut().set_size(rows, cols);
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
    }

    pub fn kill(&mut self) {
        if self.child.is_none() {
            return;
        }
        self.kill_inner();
        self.stopped = true;
    }

    pub fn restart(&mut self) -> Result<(), std::io::Error> {
        self.kill_inner();
        self.stopped = false;
        self.run()
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

impl Drop for Service {
    fn drop(&mut self) {
        let path = Path::new(&self.path);
        let name: String = path.file_name().unwrap().to_string_lossy().into_owned();

        let _ = fs::create_dir_all("temp");

        let text = self.get_all_lines().join("\n");
        _ = fs::write(format!("temp/{}.txt", name), &text);

        self.kill_inner();
    }
}
