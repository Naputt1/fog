use crate::config::HealthCheckConfig;
use crate::process;
use libc::{SIGKILL, SIGTERM};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{
    cell::RefCell,
    fs,
    io::{self, Read, Write},
    net::ToSocketAddrs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};

const INITIAL_COLS: u16 = 256;

/// How a terminal was initialized.
#[derive(Debug, Clone, PartialEq)]
pub enum Init {
    /// An interactive shell session.
    Shell,
    /// A command spawned in the terminal.
    Command {
        /// Working directory of the command.
        path: String,
        /// The command string that was executed.
        cmd: String,
    },
}

/// Health check status for a terminal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HealthStatus {
    /// Service is waiting for dependencies to start.
    Pending,
    Unknown,
    Healthy,
    Unhealthy,
}

/// A pseudo-terminal managing a shell or command process.
pub struct Terminal {
    /// How this terminal was initialized.
    pub init: Init,
    /// Display name for the terminal tab.
    pub name: String,
    /// Whether the child process has exited.
    pub stopped: bool,
    /// Whether a command/process is actively running in this terminal.
    pub process_running: bool,
    /// Whether to save terminal output to a file on drop.
    pub save_logs: bool,
    /// Number of scrollback lines in the parser.
    pub scrollback: usize,
    /// Health check configurations (empty if none).
    pub health_checks: Vec<HealthCheckConfig>,
    /// Shell command to run on shutdown (e.g. "docker compose down").
    pub shutdown_cmd: Option<String>,
    /// Names of services this service depends on.
    pub dep_names: Vec<String>,
    parser: Arc<Mutex<vt100::Parser>>,
    health_status: Arc<Mutex<HealthStatus>>,
    screen_generation: Arc<AtomicUsize>,
    #[allow(clippy::type_complexity)]
    line_cache: RefCell<Option<(usize, usize, usize, Vec<Line<'static>>)>>,
    handler: Option<JoinHandle<()>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("init", &self.init)
            .field("name", &self.name)
            .field("stopped", &self.stopped)
            .field("process_running", &self.process_running)
            .field("scrollback", &self.scrollback)
            .field("health_checks", &self.health_checks)
            .field("shutdown_cmd", &self.shutdown_cmd)
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
    generation: Arc<AtomicUsize>,
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
                    generation.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
    })
}

impl Terminal {
    /// Creates a new interactive shell terminal using the user's `$SHELL` (defaults to `bash`).
    ///
    /// # Arguments
    /// * `name` - The display name for the terminal tab.
    ///
    /// # Returns
    /// A new [`Terminal`] connected to a shell PTY.
    ///
    /// # Errors
    /// Returns an error if the PTY could not be opened or the shell could not be spawned.
    pub fn spawn_shell(name: String, scrollback: usize) -> io::Result<Self> {
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

        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback)));
        let screen_generation = Arc::new(AtomicUsize::new(0));
        let handler = spawn_reader(parser.clone(), screen_generation.clone(), reader);

        Ok(Self {
            init: Init::Shell,
            name,
            stopped: false,
            process_running: false,
            save_logs: false,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: vec![],
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            handler: Some(handler),
            writer: Some(writer),
            child: Some(child),
            master: Some(pair.master),
        })
    }

    /// Spawns a command in a new terminal within the given working directory.
    ///
    /// # Arguments
    /// * `path` - The working directory for the command.
    /// * `cmd` - The shell command to execute.
    /// * `name` - The display name for the terminal tab.
    ///
    /// # Returns
    /// A new [`Terminal`] with the command running inside.
    ///
    /// # Errors
    /// Returns an error if the PTY could not be opened or the shell could not be spawned.
    pub fn spawn_command(
        path: &str,
        cmd: &str,
        name: String,
        scrollback: usize,
    ) -> io::Result<Self> {
        let mut t = Self {
            init: Init::Command {
                path: String::new(),
                cmd: cmd.to_string(),
            },
            name,
            stopped: false,
            process_running: true,
            save_logs: false,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: vec![],
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback))),
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            handler: None,
            writer: None,
            child: None,
            master: None,
        };
        t.spawn_into(path, cmd)?;
        Ok(t)
    }

    /// Creates a terminal that displays an error message instead of a running process.
    ///
    /// # Arguments
    /// * `name` - The display name for the terminal tab.
    /// * `error` - The error message to display in the terminal.
    pub fn spawn_error(name: String, error: String, scrollback: usize) -> Self {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback)));
        {
            let mut p = parser.lock().unwrap_or_else(|e| e.into_inner());
            p.screen_mut().set_size(24, 80);
            p.process(error.as_bytes());
        }

        Self {
            init: Init::Command {
                path: String::new(),
                cmd: String::new(),
            },
            name,
            stopped: true,
            process_running: false,
            save_logs: false,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: vec![],
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Unhealthy)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            handler: None,
            writer: None,
            child: None,
            master: None,
        }
    }

    /// Creates a pending terminal that displays a "waiting for dependencies" message.
    /// No process is spawned — the terminal is upgraded later via [`start`](Self::start).
    ///
    /// # Arguments
    /// * `name` - The display name for the terminal tab.
    /// * `scrollback` - Number of scrollback lines.
    /// * `deps` - Names of the dependencies this service is waiting for.
    pub fn spawn_pending(name: String, scrollback: usize, deps: &[String]) -> Self {
        let message = format!("⏳ waiting for: {}", deps.join(", "));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback)));
        {
            let mut p = parser.lock().unwrap_or_else(|e| e.into_inner());
            p.screen_mut().set_size(24, 80);
            p.process(message.as_bytes());
        }

        Self {
            init: Init::Command {
                path: String::new(),
                cmd: String::new(),
            },
            name,
            stopped: false,
            process_running: false,
            save_logs: false,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: deps.to_vec(),
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Pending)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            handler: None,
            writer: None,
            child: None,
            master: None,
        }
    }

    /// Starts a command in this terminal, upgrading it from a pending state.
    ///
    /// # Arguments
    /// * `path` - The working directory for the command.
    /// * `cmd` - The shell command to execute.
    ///
    /// # Errors
    /// Returns an error if the PTY could not be opened or the shell could not be spawned.
    pub fn start(&mut self, path: &str, cmd: &str) -> io::Result<()> {
        *self.health_status.lock().unwrap_or_else(|e| e.into_inner()) = HealthStatus::Unknown;
        self.spawn_into(path, cmd)
    }

    /// Returns `true` if the service is running and (if health checks are configured) healthy.
    pub fn is_ready(&self) -> bool {
        if self.health_checks.is_empty() {
            return !self.stopped && self.process_running;
        }
        *self.health_status.lock().unwrap_or_else(|e| e.into_inner()) == HealthStatus::Healthy
    }

    fn spawn_into(&mut self, path: &str, cmd: &str) -> io::Result<()> {
        self.init = Init::Command {
            path: path.to_string(),
            cmd: cmd.to_string(),
        };

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

        self.parser = Arc::new(Mutex::new(vt100::Parser::new(
            24,
            INITIAL_COLS,
            self.scrollback,
        )));
        *self.line_cache.borrow_mut() = None;
        self.screen_generation.store(0, Ordering::Relaxed);
        self.handler = Some(spawn_reader(
            self.parser.clone(),
            self.screen_generation.clone(),
            reader,
        ));
        self.writer = Some(writer);
        self.child = Some(child);
        self.master = Some(pair.master);
        self.process_running = true;

        Ok(())
    }

    /// Returns `true` if this terminal is an interactive shell.
    pub fn is_shell(&self) -> bool {
        matches!(self.init, Init::Shell)
    }

    /// Writes raw bytes to the terminal's PTY input.
    pub fn write(&mut self, data: &[u8]) {
        if let Some(ref mut w) = self.writer {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    /// Returns the total number of lines in both scrollback and visible area.
    pub fn total_lines(&self) -> usize {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen_mut();
        let (vis_rows, _) = screen.size();
        let sb = scrollback_len(screen);
        sb + vis_rows as usize
    }

    /// Returns a screenful of styled lines and the total line count.
    ///
    /// # Arguments
    /// * `n` - The number of visible rows to return.
    /// * `offset` - The scroll offset from the bottom of the content.
    ///
    /// # Returns
    /// A tuple of styled lines for rendering and the total number of available lines.
    pub fn get_screen(&self, n: usize, offset: usize) -> (Vec<Line<'static>>, usize) {
        let generation = self.screen_generation.load(Ordering::Relaxed);

        if let Some((cached_offset, cached_n, cached_gen, ref cached_lines)) =
            *self.line_cache.borrow()
            && cached_offset == offset
            && cached_n == n
            && cached_gen == generation
        {
            return (cached_lines.clone(), self.total_lines());
        }

        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen_mut();
        let (vis_rows, cols) = screen.size();
        let sb = scrollback_len(screen);
        let total = sb + vis_rows as usize;

        if offset >= total.saturating_sub(1) {
            screen.set_scrollback(0);
            let lines = vec![Line::from("(top)")];
            *self.line_cache.borrow_mut() = Some((offset, n, generation, lines.clone()));
            return (lines, total);
        }

        let scroll_off = offset.min(sb);
        screen.set_scrollback(scroll_off);

        let rows_to_read = n.min(vis_rows as usize).min(total.saturating_sub(offset));

        if rows_to_read == 0 {
            *self.line_cache.borrow_mut() = Some((offset, n, generation, vec![]));
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

        *self.line_cache.borrow_mut() = Some((offset, n, generation, lines.clone()));
        (lines, total)
    }

    /// Returns all lines (scrollback + visible) as plain text strings.
    pub fn get_all_lines(&self) -> Vec<String> {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen_mut();
        let (vis_rows, cols) = screen.size();
        let sb = scrollback_len(screen);
        let vis = vis_rows as usize;

        let mut result = Vec::with_capacity(sb + vis);

        // Read scrollback in chunks of vis_rows to avoid O(sb²) from
        // repeated visible_rows() iterator creation. Each call to
        // cell() or rows() goes through visible_rows().skip(sb - offset),
        // which is O(sb) — iterating one row at a time costs O(sb²).
        let mut remaining = sb;
        while remaining > 0 {
            screen.set_scrollback(remaining);
            let chunk_size = remaining.min(vis);
            for line in screen.rows(0, cols).take(chunk_size) {
                result.push(line);
            }
            remaining -= chunk_size;
        }

        screen.set_scrollback(0);
        result.extend(screen.rows(0, cols));

        result
    }

    /// Returns the cursor position `(row, col)` if the cursor is visible.
    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        let parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Resizes the PTY and internal parser screen dimensions.
    ///
    /// # Arguments
    /// * `cols` - The new number of columns.
    /// * `rows` - The new number of rows.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let mut changed = false;
        if let Some(ref m) = self.master {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        let mut p = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        let (cur_rows, cur_cols) = p.screen().size();
        if cols != cur_cols || rows != cur_rows {
            p.screen_mut().set_size(rows, cols);
            changed = true;
        }
        drop(p);
        if changed {
            *self.line_cache.borrow_mut() = None;
        }
    }

    fn kill_inner(&mut self) {
        if let Some(ref child) = self.child
            && let Some(pid) = child.process_id()
        {
            process::try_kill_process_group(pid, SIGTERM);
            thread::sleep(std::time::Duration::from_millis(500));
            if let Ok(None) = process::waitpid_nohang(pid) {
                process::try_kill_process_group(pid, SIGKILL);
            }
            process::kill_descendants(pid);
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
        self.process_running = false;
    }

    /// Restarts the command process in this terminal.
    ///
    /// # Errors
    /// Returns an error if this is a shell tab (shells cannot be restarted).
    ///
    /// # Panics
    /// Panics if the PTY could not be re-opened or the command re-spawned.
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

    /// Returns the current health status.
    pub fn get_health_status(&self) -> HealthStatus {
        *self.health_status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts a background thread that periodically runs all configured health checks.
    /// The service is considered healthy only when ALL checks pass.
    pub fn start_health_checks(&self) {
        let configs: Vec<HealthCheckConfig> = self.health_checks.clone();
        if configs.is_empty() {
            return;
        }
        let status = self.health_status.clone();

        thread::spawn(move || {
            let min_interval = configs
                .iter()
                .map(|c| c.interval_ms.unwrap_or(5000))
                .min()
                .unwrap_or(5000);
            loop {
                thread::sleep(std::time::Duration::from_millis(min_interval));
                let mut all_healthy = true;
                for config in &configs {
                    let target = config.target.clone();
                    let timeout = config.timeout_ms.unwrap_or(2000);
                    let healthy = match config.kind {
                        crate::config::HealthCheckKind::Tcp
                        | crate::config::HealthCheckKind::Http => {
                            let addr = target
                                .trim_start_matches("tcp://")
                                .trim_start_matches("http://")
                                .trim_start_matches("https://");
                            match addr.to_socket_addrs() {
                                Ok(mut addrs) => addrs.next().is_some_and(|sa| {
                                    std::net::TcpStream::connect_timeout(
                                        &sa,
                                        std::time::Duration::from_millis(timeout),
                                    )
                                    .is_ok()
                                }),
                                Err(_) => false,
                            }
                        }
                    };
                    if !healthy {
                        all_healthy = false;
                    }
                }
                let mut s = status.lock().expect("health status mutex poisoned");
                *s = if all_healthy {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Unhealthy
                };
            }
        });
    }

    /// Returns `true` if the child process is still running.
    pub fn is_running(&self) -> bool {
        !self.stopped
    }

    /// Checks if the child process has exited and updates `stopped` accordingly.
    pub fn refresh_status(&mut self) {
        if self.stopped {
            return;
        }
        if *self.health_status.lock().unwrap_or_else(|e| e.into_inner()) == HealthStatus::Pending {
            return;
        }
        if let Some(ref handler) = self.handler
            && handler.is_finished()
        {
            self.stopped = true;
            self.process_running = false;
            return;
        }
        if let Some(ref child) = self.child
            && let Some(pid) = child.process_id()
            && process::waitpid_nohang(pid).unwrap_or(Some(0)).is_some()
        {
            self.stopped = true;
            self.process_running = false;
            return;
        }
        self.update_process_running();
    }

    #[cfg(unix)]
    fn update_process_running(&mut self) {
        if let (Some(master), Some(child)) = (self.master.as_ref(), self.child.as_ref())
            && let Some(fg_pgid) = master.process_group_leader()
            && let Some(shell_pid) = child.process_id()
        {
            self.process_running = fg_pgid != shell_pid as libc::pid_t;
            return;
        }
        // Fallback: check if shell has child processes
        if let Some(ref child) = self.child
            && let Some(pid) = child.process_id()
        {
            self.process_running = process::has_child_processes(pid);
            return;
        }
        self.process_running = false;
    }

    #[cfg(not(unix))]
    fn update_process_running(&mut self) {
        if let Some(ref child) = self.child
            && let Some(pid) = child.process_id()
        {
            self.process_running = process::has_child_processes(pid);
            return;
        }
        self.process_running = !self.stopped;
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.save_logs
            && let Init::Command { .. } = &self.init
        {
            let _ = fs::create_dir_all("temp");
            let text = self.get_all_lines().join("\n");
            let _ = fs::write(format!("temp/{}.txt", self.name), &text);
        }
        self.kill_inner();
        if let Some(ref shutdown_cmd) = self.shutdown_cmd {
            let cwd = match &self.init {
                Init::Command { path, .. } if !path.is_empty() => Some(path.as_str()),
                _ => None,
            };
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", shutdown_cmd])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            #[cfg(unix)]
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            let _ = cmd.spawn();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn test_cell_style_default() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"X");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert_eq!(style, Style::default());
    }

    #[test]
    fn test_cell_style_fg_color() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[31mX");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert_eq!(style.fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn test_cell_style_bg_color() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[42mX");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert_eq!(style.bg, Some(Color::Indexed(2)));
    }

    #[test]
    fn test_cell_style_bold() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[1mB");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_cell_style_italic() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[3mI");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert!(style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn test_cell_style_underline() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[4mU");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_cell_style_inverse() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[7mV");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn test_cell_style_dim() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[2mD");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn test_cell_style_rgb() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[38;2;255;128;0mO");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert_eq!(style.fg, Some(Color::Rgb(255, 128, 0)));
    }

    #[test]
    fn test_cell_style_indexed() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[38;5;42mC");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert_eq!(style.fg, Some(Color::Indexed(42)));
    }

    #[test]
    fn test_cell_style_combined() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[1;31;43mX");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(&cell);
        assert_eq!(style.fg, Some(Color::Indexed(1)));
        assert_eq!(style.bg, Some(Color::Indexed(3)));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_cell_style_reset() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[31;1mX\x1b[0mY");
        let screen = parser.screen();
        let cell = screen.cell(0, 1).expect("cell should exist at (0,1)");
        let style = cell_style(&cell);
        assert_eq!(style, Style::default());
    }
}
