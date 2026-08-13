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
    io::{self, Write},
    net::ToSocketAddrs,
    os::unix::io::RawFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const INITIAL_COLS: u16 = 256;

/// How long a reused service may be unhealthy before fog starts it itself.
const DEFAULT_REUSE_GRACE: Duration = Duration::from_secs(10);

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
    /// Directory to tee this terminal's raw PTY output into (`<name>.log`),
    /// used by detached (`-d`) runs so an external agent can tail it.
    pub log_dir: Option<std::path::PathBuf>,
    /// Number of scrollback lines in the parser.
    pub scrollback: usize,
    /// Health check configurations (empty if none).
    pub health_checks: Vec<HealthCheckConfig>,
    /// Shell command to run on shutdown (e.g. "docker compose down").
    pub shutdown_cmd: Option<String>,
    /// Names of services this service depends on.
    pub dep_names: Vec<String>,
    /// Git branch of the worktree this service runs in, if any. Exposed to the
    /// spawned process as `FOG_BRANCH` so compose files can derive per-branch
    /// project names, hostnames, and ports.
    pub branch: Option<String>,
    /// Project identity (git-common-dir) of the instance this terminal serves,
    /// used to decide whether shared reuse infrastructure may be torn down.
    pub project: Option<String>,
    /// Name of the script this terminal is part of.
    pub script: String,
    /// Whether this service is borrowed from another instance (reuse mode):
    /// no process is spawned, health checks verify the resource, and it is
    /// not torn down on exit.
    pub reused: bool,
    /// Whether this service is a shared resource for a concurrent script
    /// (`share: true`). Even when it was started (not borrowed) here, its
    /// `shutdown_cmd` is skipped on teardown while any other instance still
    /// serves the same (project, script).
    pub shared: bool,
    /// When a reused service is adopted from another instance, the child PID
    /// to wait on / kill instead of a [`Child`] handle.
    owned_pid: Option<u32>,
    /// Raw master fd of an adopted PTY (used for resizing).
    raw_fd: Option<RawFd>,
    /// When the reused service was created, for the grace-period auto-start.
    reused_since: Option<Instant>,
    /// How long to wait for a reused resource to become healthy before
    /// starting it ourselves.
    reuse_grace: Duration,
    /// Write end of the pipe used to stop the reader thread.
    stop_w: Option<RawFd>,
    /// Set when this terminal's live process has been handed to another
    /// instance; `kill_inner` then releases resources without killing.
    handed_off: bool,
    /// Set once the child has been reaped (`waitpid`). A reaped PID must never
    /// be signaled again: the OS may have reused it for an unrelated process.
    child_reaped: bool,
    parser: Arc<Mutex<vt100::Parser>>,
    health_status: Arc<Mutex<HealthStatus>>,
    /// Set when this terminal is dropped, so its health-check thread exits.
    health_stop: Arc<AtomicBool>,
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
            .field("log_dir", &self.log_dir)
            .field("shutdown_cmd", &self.shutdown_cmd)
            .field("reused", &self.reused)
            .field("owned_pid", &self.owned_pid)
            .field("health_stop", &self.health_stop)
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

/// Probes a single health-check target. Both `tcp` and `http` kinds use a TCP
/// connect, so they share this implementation. The `docker` kind checks the
/// actual container from the configured compose file.
fn check_target(config: &HealthCheckConfig) -> bool {
    match config.kind {
        crate::config::HealthCheckKind::Docker => check_docker_target(config),
        _ => {
            let timeout = config.timeout_ms.unwrap_or(2000);
            let addr = config
                .target
                .trim_start_matches("tcp://")
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            addr.to_socket_addrs()
                .ok()
                .and_then(|mut addrs| addrs.next())
                .is_some_and(|sa| {
                    std::net::TcpStream::connect_timeout(
                        &sa,
                        std::time::Duration::from_millis(timeout),
                    )
                    .is_ok()
                })
        }
    }
}

/// Verifies a compose service is running (and, when the compose file defines a
/// healthcheck for it, reports `healthy`) by inspecting `docker compose ps`.
///
/// The compose file is resolved relative to the service's working directory at
/// build time, so `config.compose_file` is already absolute here.
fn check_docker_target(config: &HealthCheckConfig) -> bool {
    let timeout = config.timeout_ms.unwrap_or(2000);
    let compose_file = config
        .compose_file
        .clone()
        .unwrap_or_else(|| "docker-compose.yml".to_string());

    let mut child = match std::process::Command::new("docker")
        .args(["compose", "-f", &compose_file, "ps", "--format", "json"])
        .arg(&config.target)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let deadline = Instant::now() + Duration::from_millis(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break None;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let Some(status) = status else {
        return false;
    };
    if !status.success() {
        return false;
    }

    let Ok(out) = child.wait_with_output() else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    docker_ps_is_healthy(&stdout)
}

/// Parses the JSON output of `docker compose ps --format json <service>`.
/// Passes when the service is running; when a `Health` field is present and
/// non-empty it must equal `healthy`.
fn docker_ps_is_healthy(stdout: &str) -> bool {
    let value: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let entries: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(_) => vec![&value],
        _ => return false,
    };

    entries.into_iter().any(|entry| {
        let running = entry
            .get("State")
            .and_then(|s| s.as_str())
            .is_some_and(|s| s == "running");
        if !running {
            return false;
        }
        match entry.get("Health").and_then(|h| h.as_str()) {
            Some(h) if !h.is_empty() => h == "healthy",
            _ => true,
        }
    })
}

/// Evaluates health checks, returning `true` only when ALL of them pass.
///
/// Checks run concurrently so the result is bounded by the slowest check
/// rather than the sum — important for the synchronous startup probe of reused
/// services, which would otherwise add `timeout_ms` per check to startup.
pub fn health_checks_pass(configs: &[HealthCheckConfig]) -> bool {
    thread::scope(|s| {
        configs
            .iter()
            .map(|c| {
                let c = c.clone();
                s.spawn(move || check_target(&c))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .all(|h| h.join().unwrap_or(false))
    })
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

/// Opens a tee file for a service's raw PTY output inside `log_dir`, if set.
/// The service name is sanitized so a name containing a `/` cannot escape the
/// directory.
fn open_log_file(log_dir: &std::path::Path, name: &str) -> io::Result<fs::File> {
    let safe_name: String = name
        .chars()
        .map(|c| if c == '/' { '_' } else { c })
        .collect();
    fs::File::create(log_dir.join(format!("{safe_name}.log")))
}

/// Creates a pipe used to signal a reader thread to stop. Returns
/// `(read_end, write_end)`.
fn make_stop_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [-1i32, -1];
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((fds[0], fds[1]))
    }
}

/// Spawns a thread that reads PTY output from `fd` and feeds the parser,
/// stopping when the PTY reaches EOF or the `stop` pipe becomes readable.
///
/// When `tee` is `Some`, each raw chunk read is also appended to it (used by
/// detached runs to capture service output to `<log_dir>/<name>.log`).
///
/// The thread owns `fd`, `stop`, and `tee` and closes them on exit.
fn spawn_reader(
    parser: Arc<Mutex<vt100::Parser>>,
    generation: Arc<AtomicUsize>,
    fd: RawFd,
    stop: RawFd,
    mut tee: Option<fs::File>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut pfds = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: pfds is a valid array of pollfd structs.
            let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
            if r < 0 {
                break;
            }
            if pfds[1].revents != 0 {
                break;
            }
            if pfds[0].revents & libc::POLLIN != 0 {
                // SAFETY: fd is a valid, owned descriptor opened for reading.
                let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break;
                }
                if let Ok(mut p) = parser.lock() {
                    p.process(&buf[..n as usize]);
                }
                generation.fetch_add(1, Ordering::Relaxed);
                if let Some(file) = tee.as_mut() {
                    let _ = file.write_all(&buf[..n as usize]);
                }
            } else if pfds[0].revents != 0 {
                break;
            }
        }
        // SAFETY: this thread owns these fds.
        unsafe {
            libc::close(fd);
            libc::close(stop);
        }
    })
}

/// Polls `waitpid(pid, WNOHANG)` until the child is reaped or `timeout` elapses.
///
/// Returns `true` if the child was reaped, `false` if it was still running (or
/// was already reaped elsewhere) when the timeout expired. Never blocks longer
/// than `timeout`, so a process stuck in an uninterruptible exit state cannot
/// freeze teardown.
fn wait_reaped(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match process::waitpid_nohang(pid) {
            Ok(Some(_)) => return true,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(50));
            }
            // ECHILD (already reaped) or another error: nothing left to wait on.
            Err(_) => return false,
        }
    }
}

/// A write-only wrapper around a raw fd (e.g. a PTY master received from
/// another instance). Owns the fd and closes it on drop.
struct FdWriter {
    fd: RawFd,
}

impl Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: fd is a valid, owned descriptor opened for writing.
        let n = unsafe { libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for FdWriter {
    fn drop(&mut self) {
        // SAFETY: this struct owns the fd.
        unsafe { libc::close(self.fd) };
    }
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

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let master_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("pty master has no fd"))?;
        let (stop_r, stop_w) = make_stop_pipe()?;
        // SAFETY: dup creates a new independent descriptor for the thread.
        let reader_fd = unsafe { libc::dup(master_fd) };
        if reader_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback)));
        let screen_generation = Arc::new(AtomicUsize::new(0));
        let handler = spawn_reader(
            parser.clone(),
            screen_generation.clone(),
            reader_fd,
            stop_r,
            None,
        );

        Ok(Self {
            init: Init::Shell,
            name,
            stopped: false,
            process_running: false,
            save_logs: false,
            log_dir: None,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: vec![],
            branch: None,
            project: None,
            script: String::new(),
            reused: false,
            shared: false,
            owned_pid: None,
            raw_fd: None,
            reused_since: None,
            reuse_grace: DEFAULT_REUSE_GRACE,
            stop_w: Some(stop_w),
            handed_off: false,
            child_reaped: false,
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            health_stop: Arc::new(AtomicBool::new(false)),
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
    /// * `scrollback` - Number of scrollback lines.
    /// * `log_dir` - If set, raw PTY output is teed into
    ///   `<log_dir>/<name>.log` while the service runs.
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
        log_dir: Option<std::path::PathBuf>,
        branch: Option<String>,
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
            log_dir,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: vec![],
            branch,
            project: None,
            script: String::new(),
            reused: false,
            shared: false,
            owned_pid: None,
            raw_fd: None,
            reused_since: None,
            reuse_grace: DEFAULT_REUSE_GRACE,
            stop_w: None,
            handed_off: false,
            child_reaped: false,
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback))),
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            health_stop: Arc::new(AtomicBool::new(false)),
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
            log_dir: None,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: vec![],
            branch: None,
            project: None,
            script: String::new(),
            reused: false,
            shared: false,
            owned_pid: None,
            raw_fd: None,
            reused_since: None,
            reuse_grace: DEFAULT_REUSE_GRACE,
            stop_w: None,
            handed_off: false,
            child_reaped: false,
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Unhealthy)),
            health_stop: Arc::new(AtomicBool::new(false)),
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
            log_dir: None,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: deps.to_vec(),
            branch: None,
            project: None,
            script: String::new(),
            reused: false,
            shared: false,
            owned_pid: None,
            raw_fd: None,
            reused_since: None,
            reuse_grace: DEFAULT_REUSE_GRACE,
            stop_w: None,
            handed_off: false,
            child_reaped: false,
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Pending)),
            health_stop: Arc::new(AtomicBool::new(false)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            handler: None,
            writer: None,
            child: None,
            master: None,
        }
    }

    /// Creates a terminal for a reused service that is borrowed from another
    /// instance: no process is spawned and the resource is verified via health
    /// checks instead. If the resource does not come up within the grace
    /// period, [`maybe_auto_start`](Self::maybe_auto_start) starts it.
    ///
    /// # Arguments
    /// * `name` - The display name for the terminal tab.
    /// * `path` - The working directory the command would run in.
    /// * `cmd` - The command that would start the service.
    /// * `scrollback` - Number of scrollback lines.
    pub fn spawn_reused(name: String, path: String, cmd: String, scrollback: usize) -> Self {
        let message =
            format!("♻ reusing already-running '{name}'; start skipped (press R to take over)");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback)));
        {
            let mut p = parser.lock().unwrap_or_else(|e| e.into_inner());
            p.screen_mut().set_size(24, 80);
            p.process(message.as_bytes());
        }

        Self {
            init: Init::Command { path, cmd },
            name,
            stopped: false,
            process_running: true,
            save_logs: false,
            log_dir: None,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: vec![],
            branch: None,
            project: None,
            script: String::new(),
            reused: true,
            shared: false,
            owned_pid: None,
            raw_fd: None,
            reused_since: Some(Instant::now()),
            reuse_grace: DEFAULT_REUSE_GRACE,
            stop_w: None,
            handed_off: false,
            child_reaped: false,
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            health_stop: Arc::new(AtomicBool::new(false)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            handler: None,
            writer: None,
            child: None,
            master: None,
        }
    }

    /// Adopts a live PTY handed over from another fog instance.
    ///
    /// The process keeps running; its output is streamed into this terminal
    /// via the received master `fd`. Ownership is taken lazily: pressing
    /// `R` (restart) kills the borrowed process and starts the command fresh.
    ///
    /// # Arguments
    /// * `path` - The working directory the command would run in.
    /// * `cmd` - The command that started the service.
    /// * `name` - The display name for the terminal tab.
    /// * `scrollback` - Number of scrollback lines.
    /// * `fd` - The PTY master fd received via SCM_RIGHTS (now owned by us).
    /// * `pid` - The process group leader of the running service.
    /// * `log_dir` - If set, raw PTY output is teed into
    ///   `<log_dir>/<name>.log` while the service runs.
    pub fn adopt(
        path: String,
        cmd: String,
        name: String,
        scrollback: usize,
        fd: RawFd,
        pid: u32,
        log_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let tee = log_dir
            .as_deref()
            .and_then(|dir| open_log_file(dir, &name).ok());
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, INITIAL_COLS, scrollback)));
        {
            let mut p = parser.lock().unwrap_or_else(|e| e.into_inner());
            p.process(
                format!("\x1b[36m♻ adopted from instance {pid} — streaming live output\x1b[0m\r\n")
                    .as_bytes(),
            );
        }
        let screen_generation = Arc::new(AtomicUsize::new(0));
        let (stop_r, stop_w) = make_stop_pipe().unwrap_or((-1, -1));
        // SAFETY: dup creates an independent descriptor for the reader thread.
        let reader_fd = unsafe { libc::dup(fd) };
        let handler = if reader_fd >= 0 && stop_r >= 0 {
            Some(spawn_reader(
                parser.clone(),
                screen_generation.clone(),
                reader_fd,
                stop_r,
                tee,
            ))
        } else {
            if reader_fd >= 0 {
                // SAFETY: this descriptor is owned by us.
                unsafe { libc::close(reader_fd) };
            }
            if stop_r >= 0 {
                // SAFETY: this descriptor is owned by us.
                unsafe { libc::close(stop_r) };
            }
            None
        };
        // SAFETY: dup creates an independent descriptor for the writer.
        let writer_fd = unsafe { libc::dup(fd) };
        let writer = if writer_fd >= 0 {
            Some(Box::new(FdWriter { fd: writer_fd }) as Box<dyn Write + Send>)
        } else {
            None
        };

        Self {
            init: Init::Command { path, cmd },
            name,
            stopped: false,
            process_running: true,
            save_logs: false,
            log_dir,
            scrollback,
            health_checks: vec![],
            shutdown_cmd: None,
            dep_names: vec![],
            branch: None,
            project: None,
            script: String::new(),
            reused: true,
            shared: false,
            owned_pid: Some(pid),
            raw_fd: Some(fd),
            reused_since: None,
            reuse_grace: DEFAULT_REUSE_GRACE,
            stop_w: if stop_w >= 0 { Some(stop_w) } else { None },
            handed_off: false,
            child_reaped: false,
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            health_stop: Arc::new(AtomicBool::new(false)),
            screen_generation,
            line_cache: RefCell::new(None),
            handler,
            writer,
            child: None,
            master: None,
        }
    }

    /// Marks this terminal as handed over to a successor without transferring
    /// a live process. Used for borrowed reuse services during in-place
    /// worktree switches: `Drop` then neither kills the resource nor runs its
    /// `shutdown_cmd`, keeping it alive for the successor instance.
    pub fn preserve_for_reuse(&mut self) {
        self.handed_off = true;
    }

    /// Extracts this terminal's live process for transfer to another instance.
    ///
    /// Dups the PTY master fd and stops this terminal's reader so output is
    /// not consumed after handoff. Returns `None` if there is no live process
    /// to hand over.
    pub fn extract_handoff(&mut self) -> Option<crate::ipc::HandoffItem> {
        let pid = if let Some(pid) = self.owned_pid {
            pid
        } else {
            self.child.as_ref()?.process_id()?
        };
        let fd = if let Some(fd) = self.raw_fd {
            fd
        } else {
            self.master.as_ref()?.as_raw_fd()?
        };
        // SAFETY: dup creates an independent descriptor for the receiver.
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            return None;
        }

        if let Some(stop) = self.stop_w.take() {
            // SAFETY: stop is a valid pipe write end owned by this terminal.
            unsafe {
                libc::write(stop, c"".as_ptr().cast(), 1);
                libc::close(stop);
            }
        }
        if let Some(handler) = self.handler.take() {
            let _ = handler.join();
        }
        // Leak the writer instead of dropping it: portable-pty's UnixMasterWriter
        // writes `\n` + EOT (Ctrl-D) into the PTY on drop, which would terminate
        // the very process being handed over to the successor. The master stays
        // alive on the successor's dup'd fd; the OS reclaims this one on exit.
        if let Some(w) = self.writer.take() {
            std::mem::forget(w);
        }
        self.reused = true;
        self.handed_off = true;
        Some(crate::ipc::HandoffItem {
            name: self.name.clone(),
            pid,
            fd: dup_fd,
        })
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
        if let Some(branch) = &self.branch {
            cmd_builder.env("FOG_BRANCH", branch);
        }

        let child = pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let master_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| io::Error::other("pty master has no fd"))?;
        let (stop_r, stop_w) = make_stop_pipe()?;
        // SAFETY: dup creates a new independent descriptor for the thread.
        let reader_fd = unsafe { libc::dup(master_fd) };
        if reader_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let _ = writeln!(writer, "cd {} && {}", path, cmd);

        let tee = self
            .log_dir
            .as_deref()
            .and_then(|dir| open_log_file(dir, &self.name).ok());

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
            reader_fd,
            stop_r,
            tee,
        ));
        self.stop_w = Some(stop_w);
        self.writer = Some(writer);
        self.child = Some(child);
        self.child_reaped = false;
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
        // A zero-size screen makes the vt100 parser overflow; clamp to sane
        // minimums (also avoids PTY ioctls that would fail).
        let rows = rows.max(1);
        let cols = cols.max(1);
        if let Some(fd) = self.raw_fd {
            // Adopted PTY: resize directly via ioctl.
            // SAFETY: ws is a valid, fully-initialized winsize struct.
            unsafe {
                let mut ws: libc::winsize = std::mem::zeroed();
                ws.ws_row = rows;
                ws.ws_col = cols;
                libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
            }
        }
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
        // Grow-only width: shrinking the vt100 screen truncates every visible
        // and scrollback row irreversibly, so text cut at a narrow width never
        // comes back on re-expand. Keep the widest width seen. Height still
        // tracks the visible area so bottom-anchored output stays on screen.
        let new_cols = cur_cols.max(cols);
        if new_cols != cur_cols || rows != cur_rows {
            p.screen_mut().set_size(rows, new_cols);
            changed = true;
        }
        drop(p);
        if changed {
            *self.line_cache.borrow_mut() = None;
        }
    }

    fn kill_inner(&mut self) {
        // Signal the reader thread to stop reading so it releases its fd.
        if let Some(stop) = self.stop_w.take() {
            // SAFETY: stop is a valid pipe write end owned by this terminal.
            unsafe {
                libc::write(stop, c"".as_ptr().cast(), 1);
                libc::close(stop);
            }
        }

        if self.handed_off {
            // The live process was transferred to another instance: release
            // our resources without killing or reaping the child.
            let _ = self.child.take();
            self.master = None;
            self.writer = None;
            self.raw_fd = None;
            self.owned_pid = None;
            if let Some(handler) = self.handler.take() {
                let _ = handler.join();
            }
            self.process_running = false;
            return;
        }

        // An adopted terminal: kill the process group by the stored PID. The
        // adopted PID is not a child, so probe liveness with `kill(pid, 0)`.
        if let Some(pid) = self.owned_pid {
            if process::is_pid_alive(pid) {
                process::try_kill_process_group(pid, SIGTERM);
                thread::sleep(Duration::from_millis(500));
                if process::is_pid_alive(pid) {
                    process::try_kill_process_group(pid, SIGKILL);
                }
                process::kill_descendants(pid);
            }
            self.owned_pid = None;
        }

        // A real child. Once it has been reaped (or is known to be dead), its
        // PID must not be signaled again: the OS may have reused it.
        if !self.child_reaped
            && let Some(ref child) = self.child
            && let Some(pid) = child.process_id()
        {
            process::try_kill_process_group(pid, SIGTERM);
            thread::sleep(Duration::from_millis(500));
            match process::waitpid_nohang(pid) {
                Ok(Some(_)) => self.child_reaped = true,
                Ok(None) => process::try_kill_process_group(pid, SIGKILL),
                Err(_) => self.child_reaped = true,
            }
            process::kill_descendants(pid);
        }

        if let Some(mut child) = self.child.take()
            && !self.child_reaped
        {
            let pid = child.process_id();
            let _ = child.kill();
            // Reap with a bounded wait. A process stuck in an uninterruptible
            // exit state can defer even SIGKILL, so a bare blocking `wait()`
            // would freeze fog's teardown on quit or worktree switch. If it
            // is not reaped in time, leave the zombie for the OS to reap.
            if let Some(pid) = pid
                && !wait_reaped(pid, Duration::from_secs(2))
            {
                process::try_kill_process_group(pid, SIGKILL);
            }
        }

        if let Some(fd) = self.raw_fd {
            // SAFETY: fd was received via SCM_RIGHTS and is owned by us.
            unsafe { libc::close(fd) };
            self.raw_fd = None;
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
    /// For a reused service this takes ownership: any borrowed process is
    /// killed and the command is spawned fresh in this terminal.
    ///
    /// # Errors
    /// Returns an error if this is a shell tab (shells cannot be restarted).
    pub fn restart(&mut self) -> io::Result<()> {
        let (path, cmd) = match &self.init {
            Init::Command { path, cmd } => (path.clone(), cmd.clone()),
            Init::Shell => {
                return Err(io::Error::other("cannot restart a shell tab"));
            }
        };
        self.kill_inner();
        // Tear down the previous incarnation (e.g. `docker compose down`) so
        // the fresh start does not silently attach to leftover containers.
        self.run_shutdown_cmd();
        // A health-checked reuse service (e.g. a one-shot `docker compose up -d`)
        // stays health-driven after restart: its command exits after bringing
        // the resource up, so liveness comes from the health checks. Without
        // health checks the terminal owns the process and stays process-driven.
        if self.health_checks.is_empty() {
            self.reused = false;
        }
        self.reused_since = None;
        self.stopped = false;
        self.set_health_status(HealthStatus::Unknown);
        self.spawn_into(&path, &cmd)
    }

    /// Returns the current health status.
    pub fn get_health_status(&self) -> HealthStatus {
        *self.health_status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets the current health status. Used to seed a reused terminal with
    /// `Healthy` right after a successful startup probe so it does not flicker
    /// as stopped until the first background check runs.
    pub fn set_health_status(&self, s: HealthStatus) {
        *self.health_status.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// Runs the configured health checks once, immediately, and updates the
    /// current health status. Used right after adopting a service so its tab
    /// (and anything depending on it) knows right away whether the borrowed
    /// resource is actually up, instead of waiting for the first periodic
    /// check to run. No-op when no health checks are configured.
    pub fn probe_health(&self) {
        if self.health_checks.is_empty() {
            return;
        }
        let healthy = health_checks_pass(&self.health_checks);
        self.set_health_status(if healthy {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
        });
    }

    /// Appends a plain status message into this terminal's own screen buffer so
    /// state changes are visible in the tab without writing to stderr (which
    /// would corrupt the raw-mode TUI) or to the process PTY.
    pub fn notice(&self, message: &str) {
        self.write_to_screen(message);
    }

    /// Appends a plain status message into this terminal's own screen buffer so
    /// state changes are visible in the tab without writing to stderr (which
    /// would corrupt the raw-mode TUI) or to the process PTY.
    fn write_to_screen(&self, message: &str) {
        let mut parser = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        parser.process(message.as_bytes());
        *self.line_cache.borrow_mut() = None;
    }

    /// Starts a background thread that periodically runs all configured health checks.
    /// The service is considered healthy only when ALL checks pass.
    pub fn start_health_checks(&self) {
        let configs: Vec<HealthCheckConfig> = self.health_checks.clone();
        if configs.is_empty() {
            return;
        }
        let status = self.health_status.clone();
        let stop = self.health_stop.clone();

        thread::spawn(move || {
            let min_interval = configs
                .iter()
                .map(|c| c.interval_ms.unwrap_or(5000))
                .min()
                .unwrap_or(5000);
            loop {
                thread::sleep(std::time::Duration::from_millis(min_interval));
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let all_healthy = health_checks_pass(&configs);
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
        // An adopted terminal whose transferred process has already exited
        // (e.g. a one-shot `docker compose up`) falls back to plain reuse:
        // drop the dead fd and rely on health checks. The adopted PID is not a
        // child of this process, so `waitpid` would return ECHILD; use
        // `kill(pid, 0)` to probe liveness instead.
        if let Some(pid) = self.owned_pid
            && !process::is_pid_alive(pid)
        {
            self.owned_pid = None;
            if let Some(fd) = self.raw_fd.take() {
                // SAFETY: fd was received via SCM_RIGHTS and is owned by us.
                unsafe { libc::close(fd) };
            }
            self.process_running = false;
        }
        // Reused services have no owned process; their state is driven by
        // health checks (or assumed up when none are configured).
        if self.reused {
            let healthy = *self.health_status.lock().unwrap_or_else(|e| e.into_inner())
                == HealthStatus::Healthy;
            let up = self.health_checks.is_empty() || healthy;
            self.process_running = up;
            self.stopped = !up;
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
            // Reap the exited child so it does not linger as a zombie.
            if let Some(ref child) = self.child
                && let Some(pid) = child.process_id()
                && process::waitpid_nohang(pid).is_ok_and(|r| r.is_some())
            {
                self.child_reaped = true;
            }
            return;
        }
        if let Some(ref child) = self.child
            && let Some(pid) = child.process_id()
            && process::waitpid_nohang(pid).is_ok_and(|r| r.is_some())
        {
            self.stopped = true;
            self.process_running = false;
            self.child_reaped = true;
            return;
        }
        self.update_process_running();
    }

    /// Starts a reused service if it has not become healthy within the grace
    /// period.
    ///
    /// The terminal stays health-driven: `reused` is left set so `refresh_status`
    /// keeps deriving liveness from the health checks. A service started by a
    /// one-shot command (e.g. `docker compose up -d`) would otherwise be marked
    /// stopped as soon as the command exits.
    ///
    /// # Errors
    /// Returns an error if the process could not be spawned.
    pub fn maybe_auto_start(&mut self) -> io::Result<()> {
        if !self.reused {
            return Ok(());
        }
        if self.health_checks.is_empty() {
            return Ok(());
        }
        let Some(since) = self.reused_since else {
            return Ok(());
        };
        if since.elapsed() < self.reuse_grace {
            return Ok(());
        }
        let healthy =
            *self.health_status.lock().unwrap_or_else(|e| e.into_inner()) == HealthStatus::Healthy;
        if healthy {
            return Ok(());
        }
        let (path, cmd) = match &self.init {
            Init::Command { path, cmd } => (path.clone(), cmd.clone()),
            Init::Shell => return Ok(()),
        };
        // Clear the grace timer so this only fires once. `reused` stays set to
        // keep the terminal health-driven (see doc comment above).
        self.reused_since = None;
        self.write_to_screen(&format!(
            "♻ '{}' was not reachable, starting it now\n",
            self.name
        ));
        self.start(&path, &cmd)
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

impl Terminal {
    /// Runs the service's `shutdown_cmd` in a fresh session, in the service's
    /// working directory. Used both at teardown (`Drop`) and on restart so a
    /// restarted compose-style service does not leave its old containers up.
    fn run_shutdown_cmd(&self) {
        let Some(ref shutdown_cmd) = self.shutdown_cmd else {
            return;
        };
        let cwd = match &self.init {
            Init::Command { path, .. } if !path.is_empty() => Some(path.as_str()),
            _ => None,
        };
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", shutdown_cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(branch) = &self.branch {
            cmd.env("FOG_BRANCH", branch);
        }
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

impl Drop for Terminal {
    fn drop(&mut self) {
        // Stop the health-check thread so it does not outlive this terminal
        // (relevant when services are replaced by an in-place worktree switch).
        self.health_stop.store(true, Ordering::SeqCst);
        if self.save_logs
            && let Init::Command { .. } = &self.init
        {
            let _ = fs::create_dir_all("temp");
            let text = self.get_all_lines().join("\n");
            let _ = fs::write(format!("temp/{}.txt", self.name), &text);
        }
        self.kill_inner();
        // Run the shutdown command unless the live process was handed off to a
        // live successor (handover in a reclaim/worktree switch). A borrowed or
        // assumed-up reuse service with no successor must still be torn down,
        // so the gate is `handed_off`, not `reused`.
        //
        // Multi-branch: a shared (reuse/share) service is only torn down when
        // this is the last instance serving the same (project, script) —
        // another branch, or a concurrent same-branch instance, may still be
        // using it, so its `shutdown_cmd` (e.g. `docker compose down`) must
        // not run while a sibling is alive. A shared service that was started
        // (not borrowed) here is covered by `shared`, not just `reused`.
        let is_shared = self.reused || self.shared;
        let last_instance = !is_shared
            || self.project.is_none()
            || self.script.is_empty()
            || crate::ipc::find_instances_any_branch(
                self.project.as_deref().unwrap_or_default(),
                &self.script,
            )
            .is_empty();
        if !self.handed_off && last_instance {
            self.run_shutdown_cmd();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn test_spawn_reused_is_ready_without_health_checks() {
        let mut t =
            Terminal::spawn_reused("db".into(), ".".into(), "docker compose up -d".into(), 100);
        assert!(t.reused);
        assert!(t.process_running);
        assert!(!t.stopped);
        assert!(t.is_ready());
        t.refresh_status();
        assert!(!t.stopped);
    }

    #[test]
    fn test_reused_with_unreachable_health_auto_starts() {
        let mut t = Terminal::spawn_reused("db".into(), ".".into(), "true".into(), 100);
        t.health_checks.push(HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target: "127.0.0.1:1".into(),
            compose_file: None,
            interval_ms: Some(50),
            timeout_ms: Some(50),
        });
        t.reuse_grace = Duration::ZERO;
        t.start_health_checks();
        // health starts Unknown (not Healthy), so auto-start should take over.
        t.maybe_auto_start().unwrap();
        // The terminal stays health-driven so a one-shot start command (e.g.
        // `docker compose up -d`) does not mark the resource stopped as soon
        // as the command exits.
        assert!(t.reused, "auto-started reuse service stays health-driven");
        assert!(
            t.reused_since.is_none(),
            "grace timer cleared so auto-start fires only once"
        );
        assert!(t.process_running);
        t.kill_inner();
    }

    #[test]
    fn test_health_checks_pass_all() {
        // A closed port never passes.
        let closed = vec![HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target: "127.0.0.1:1".into(),
            compose_file: None,
            interval_ms: None,
            timeout_ms: Some(100),
        }];
        assert!(!health_checks_pass(&closed));

        // A live listener passes, and stays passing next to a reachable check.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let open = vec![HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target: addr.to_string(),
            compose_file: None,
            interval_ms: None,
            timeout_ms: Some(200),
        }];
        assert!(health_checks_pass(&open));

        // All checks must pass: one reachable + one closed fails.
        let mixed = vec![
            HealthCheckConfig {
                kind: crate::config::HealthCheckKind::Http,
                target: format!("http://{}", addr),
                compose_file: None,
                interval_ms: None,
                timeout_ms: Some(200),
            },
            HealthCheckConfig {
                kind: crate::config::HealthCheckKind::Tcp,
                target: "127.0.0.1:1".into(),
                compose_file: None,
                interval_ms: None,
                timeout_ms: Some(100),
            },
        ];
        assert!(!health_checks_pass(&mixed));
    }

    #[test]
    fn test_docker_ps_is_healthy_running() {
        let out = r#"[{"Service":"postgres","State":"running","Health":"healthy"}]"#;
        assert!(docker_ps_is_healthy(out), "running + healthy passes");
    }

    #[test]
    fn test_docker_ps_is_healthy_running_without_healthcheck() {
        let out = r#"[{"Service":"postgres","State":"running","Health":""}]"#;
        assert!(
            docker_ps_is_healthy(out),
            "running with no healthcheck passes (health ignored when empty)"
        );
    }

    #[test]
    fn test_docker_ps_is_healthy_running_absent_health_field() {
        let out = r#"[{"Service":"postgres","State":"running"}]"#;
        assert!(
            docker_ps_is_healthy(out),
            "running with no Health field passes"
        );
    }

    #[test]
    fn test_docker_ps_is_healthy_unhealthy() {
        let out = r#"[{"Service":"postgres","State":"running","Health":"unhealthy"}]"#;
        assert!(
            !docker_ps_is_healthy(out),
            "running but unhealthy must fail"
        );
    }

    #[test]
    fn test_docker_ps_is_healthy_exited() {
        let out = r#"[{"Service":"postgres","State":"exited","Health":""}]"#;
        assert!(!docker_ps_is_healthy(out), "exited service must fail");
    }

    #[test]
    fn test_docker_ps_is_healthy_empty_output() {
        assert!(!docker_ps_is_healthy(""), "empty output must fail");
    }

    #[test]
    fn test_docker_ps_is_healthy_not_json() {
        assert!(!docker_ps_is_healthy("garbage"), "non-JSON must fail");
    }

    #[test]
    fn test_adopted_probe_health_runs_immediately() {
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let master_fd = pty.master.as_raw_fd().expect("pty master fd");
        let dup_fd = unsafe { libc::dup(master_fd) };
        assert!(dup_fd >= 0);
        let mut t = Terminal::adopt(
            ".".into(),
            "true".into(),
            "db".into(),
            100,
            dup_fd,
            99_999,
            None,
        );

        // A live listener flips an adopted terminal to Healthy immediately.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        t.health_checks.push(HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target: addr.to_string(),
            compose_file: None,
            interval_ms: None,
            timeout_ms: Some(200),
        });
        t.probe_health();
        assert_eq!(t.get_health_status(), HealthStatus::Healthy);

        // A closed port flips it to Unhealthy immediately.
        t.health_checks.push(HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target: "127.0.0.1:1".into(),
            compose_file: None,
            interval_ms: None,
            timeout_ms: Some(100),
        });
        t.probe_health();
        assert_eq!(t.get_health_status(), HealthStatus::Unhealthy);

        t.kill_inner();
    }

    /// Runs `shutdown_cmd` (a `touch`) and waits up to `timeout` for `marker`.
    fn wait_for_marker(marker: &std::path::Path, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if marker.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn test_drop_runs_shutdown_cmd_for_reused_without_handoff() {
        let marker = std::env::temp_dir().join(format!(
            "fog-test-drop-reused-{}.marker",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        let mut t = Terminal::spawn_reused("db".into(), ".".into(), "true".into(), 100);
        assert!(t.reused);
        t.shutdown_cmd = Some(format!("touch {}", marker.display()));
        drop(t);

        let seen = wait_for_marker(&marker, Duration::from_secs(5));
        let _ = fs::remove_file(&marker);
        assert!(
            seen,
            "a reused service with no successor must run its shutdown_cmd on drop"
        );
    }

    #[test]
    fn test_drop_runs_shutdown_cmd_for_adopted_without_handoff() {
        // A borrowed (adopted) terminal is the reported bug case: it must run
        // shutdown_cmd when no successor takes the resource over.
        let marker = std::env::temp_dir().join(format!(
            "fog-test-drop-adopted-{}.marker",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let master_fd = pty.master.as_raw_fd().expect("pty master fd");
        let dup_fd = unsafe { libc::dup(master_fd) };
        assert!(dup_fd >= 0);

        let mut t = Terminal::adopt(
            ".".into(),
            "true".into(),
            "db".into(),
            100,
            dup_fd,
            99_999,
            None,
        );
        assert!(t.reused);
        t.shutdown_cmd = Some(format!("touch {}", marker.display()));
        drop(t);

        let seen = wait_for_marker(&marker, Duration::from_secs(5));
        let _ = fs::remove_file(&marker);
        assert!(
            seen,
            "a borrowed service with no successor must run its shutdown_cmd on drop"
        );
    }

    #[test]
    fn test_drop_skips_shutdown_cmd_when_handed_off() {
        let marker = std::env::temp_dir().join(format!(
            "fog-test-drop-handed-off-{}.marker",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        let mut t = Terminal::spawn_reused("db".into(), ".".into(), "true".into(), 100);
        t.handed_off = true;
        t.shutdown_cmd = Some(format!("touch {}", marker.display()));
        drop(t);

        std::thread::sleep(Duration::from_millis(500));
        let seen = marker.exists();
        let _ = fs::remove_file(&marker);
        assert!(
            !seen,
            "a service handed off to a successor must not run its shutdown_cmd"
        );
    }

    #[test]
    fn test_extract_handoff_live_process() {
        let mut t =
            Terminal::spawn_command(".", "echo hello-fog", "svc".into(), 100, None, None).unwrap();
        let handoff = t.extract_handoff().expect("live process should hand off");
        assert_eq!(handoff.name, "svc");
        assert!(handoff.pid > 0);
        assert!(handoff.fd >= 0);
        assert!(t.handed_off);
        // Releasing must not kill the (already-extracted) process.
        t.kill_inner();
        // SAFETY: handoff.fd is owned by the test after extraction.
        unsafe { libc::close(handoff.fd) };
    }

    #[test]
    fn test_wait_reaped_exited_child() {
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        let pid = child.id();
        thread::sleep(Duration::from_millis(200));
        assert!(
            wait_reaped(pid, Duration::from_secs(5)),
            "an exited child must be reaped"
        );
        drop(child);
    }

    #[test]
    fn test_wait_reaped_still_running_then_killed() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(
            !wait_reaped(pid, Duration::from_millis(150)),
            "a running child must not be reaped within a short timeout"
        );
        let _ = child.kill();
        assert!(
            wait_reaped(pid, Duration::from_secs(5)),
            "a killed child must be reaped"
        );
        drop(child);
    }

    #[test]
    fn test_adopt_starts_clean_with_header() {
        // Build a live PTY to adopt.
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let master_fd = pty.master.as_raw_fd().unwrap();
        let fd = unsafe { libc::dup(master_fd) };
        assert!(fd >= 0);

        let mut t = Terminal::adopt(
            "/repo/infra".into(),
            "docker compose up -d".into(),
            "infra".into(),
            100,
            fd,
            99_999,
            None,
        );
        let lines: Vec<String> = t
            .get_all_lines()
            .into_iter()
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "adopt should start clean with only the header, got: {:?}",
            lines
        );
        assert!(lines[0].contains("adopted from instance 99999"));

        // Pid is owned; a dead pid (not a child of this test) makes
        // refresh_status fall back to reuse.
        t.refresh_status();
        assert!(t.reused);
        t.kill_inner();
        // SAFETY: fd is owned by the test.
        unsafe { libc::close(fd) };
    }

    #[test]
    fn test_cell_style_default() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"X");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert_eq!(style, Style::default());
    }

    #[test]
    fn test_cell_style_fg_color() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[31mX");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert_eq!(style.fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn test_cell_style_bg_color() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[42mX");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert_eq!(style.bg, Some(Color::Indexed(2)));
    }

    #[test]
    fn test_cell_style_bold() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[1mB");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_cell_style_italic() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[3mI");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn test_cell_style_underline() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[4mU");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_cell_style_inverse() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[7mV");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn test_cell_style_dim() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[2mD");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn test_cell_style_rgb() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[38;2;255;128;0mO");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert_eq!(style.fg, Some(Color::Rgb(255, 128, 0)));
    }

    #[test]
    fn test_cell_style_indexed() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[38;5;42mC");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
        assert_eq!(style.fg, Some(Color::Indexed(42)));
    }

    #[test]
    fn test_cell_style_combined() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[1;31;43mX");
        let screen = parser.screen();
        let cell = screen.cell(0, 0).expect("cell should exist at (0,0)");
        let style = cell_style(cell);
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
        let style = cell_style(cell);
        assert_eq!(style, Style::default());
    }

    #[test]
    fn test_resize_shrink_does_not_truncate_content() {
        let mut t = Terminal::spawn_reused("svc".into(), ".".into(), "true".into(), 100);
        // Fits on one parser row (initial width 80) but exceeds the 60-col
        // shrink target, so it would be truncated by a shrink-to-fit resize.
        let long_line = format!("ERROR {}", "x".repeat(70));
        {
            let mut p = t.parser.lock().unwrap_or_else(|e| e.into_inner());
            // Clear the header spawn_reused wrote so the line sits alone on row 0.
            p.process(b"\x1b[2J\x1b[H");
            p.process(long_line.as_bytes());
        }

        t.resize(60, 24);
        let after: Vec<String> = t.get_all_lines();
        assert!(
            after.iter().any(|l| l.contains(&long_line)),
            "shrink must not truncate text (resize width is grow-only), got: {:?}",
            after
        );

        t.resize(120, 24);
        assert!(
            t.get_all_lines().iter().any(|l| l.contains(&long_line)),
            "re-expand must still show the full text"
        );
    }

    #[test]
    fn test_resize_height_tracks_visible_area() {
        let mut t = Terminal::spawn_reused("svc".into(), ".".into(), "true".into(), 100);
        t.resize(80, 30);
        let (rows, cols) = t
            .parser
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .screen()
            .size();
        assert_eq!(rows, 30);
        assert_eq!(cols, 80);

        t.resize(50, 20);
        let (rows, cols) = t
            .parser
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .screen()
            .size();
        assert_eq!(rows, 20, "height must track the visible area");
        assert_eq!(cols, 80, "width must never shrink");
    }

    #[test]
    fn test_log_dir_tees_output_to_file() {
        let dir = std::env::temp_dir().join(format!(
            "fog-test-log-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let mut t = Terminal::spawn_command(
            ".",
            "echo fog-tee-marker",
            "svc".into(),
            100,
            Some(dir.clone()),
            None,
        )
        .unwrap();

        // The reader thread tees output asynchronously; poll for the marker.
        let path = dir.join("svc.log");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut content = String::new();
        while std::time::Instant::now() < deadline {
            if let Ok(c) = fs::read_to_string(&path)
                && c.contains("fog-tee-marker")
            {
                content = c;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        t.kill_inner();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            content.contains("fog-tee-marker"),
            "raw PTY output must be teed into the service log file, got: {:?}",
            content
        );
    }

    #[test]
    fn test_log_file_name_sanitizes_slashes() {
        let dir = std::env::temp_dir().join(format!(
            "fog-test-log-sanitize-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let master_fd = pty.master.as_raw_fd().unwrap();
        let fd = unsafe { libc::dup(master_fd) };
        assert!(fd >= 0);
        let mut t = Terminal::adopt(
            ".".into(),
            "true".into(),
            "a/b".into(),
            100,
            fd,
            99_999,
            Some(dir.clone()),
        );
        assert!(dir.join("a_b.log").exists());
        assert!(!dir.join("a/b.log").exists());
        t.kill_inner();
        // SAFETY: fd is owned by the test.
        unsafe { libc::close(fd) };
        let _ = fs::remove_dir_all(&dir);
    }
}
