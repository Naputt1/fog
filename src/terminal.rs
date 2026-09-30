use crate::config::{EndpointConfig, HealthCheckConfig, HealthCheckSpec};
use crate::fds::Fd;
use crate::process::{self, Signal};
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

/// Default cadence used until a service's first successful health check.
const DEFAULT_START_INTERVAL: Duration = Duration::from_millis(500);
/// Default steady-state cadence, used once the service is healthy.
const DEFAULT_HEALTH_INTERVAL: Duration = Duration::from_millis(5000);
/// Default consecutive failures before a service is reported unhealthy.
const DEFAULT_HEALTH_RETRIES: u32 = 3;
/// Health check intervals are clamped to at least this (matches the schema).
const MIN_HEALTH_INTERVAL: Duration = Duration::from_millis(100);

/// How long a restart waits for `shutdown_cmd` to finish before spawning the
/// replacement command. After this the shutdown process is killed so a wedged
/// `docker compose down` cannot block the restart forever.
const SHUTDOWN_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Fires a wake-up ping to the app loop whenever any service's health status
/// changes, so dependents gated by `depends_on` start the moment a dependency
/// becomes ready instead of waiting for the next poll tick.
pub struct HealthSignal {
    subscribers: Mutex<Vec<std::sync::mpsc::Sender<()>>>,
}

impl HealthSignal {
    fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Subscribes to health-change pings. The loop wakes when it receives.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.subscribers
            .lock()
            .expect("health signal mutex poisoned")
            .push(tx);
        rx
    }

    /// Pings every subscriber, pruning any whose receiver has been dropped.
    pub fn notify(&self) {
        let mut subs = self
            .subscribers
            .lock()
            .expect("health signal mutex poisoned");
        subs.retain(|tx| tx.send(()).is_ok());
    }
}

/// Process-wide health-change signal shared by every health-check thread and the
/// app loop. A single orchestrator instance runs at a time, so a shared source
/// is sufficient and avoids threading an `Arc` through every terminal.
pub fn health_signal() -> &'static Arc<HealthSignal> {
    static SIGNAL: std::sync::OnceLock<Arc<HealthSignal>> = std::sync::OnceLock::new();
    SIGNAL.get_or_init(|| Arc::new(HealthSignal::new()))
}

fn clamp_interval(ms: u64) -> Duration {
    Duration::from_millis(ms).max(MIN_HEALTH_INTERVAL)
}

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
    /// Health checks have not passed yet, but the service is still within its
    /// startup grace window (`start_period_ms`) or below `retries`.
    Starting,
    Healthy,
    Unhealthy,
}

/// Flattens a `endpoint` health check spec into concrete configs.
pub(crate) fn endpoint_health_checks(spec: &Option<HealthCheckSpec>) -> Vec<HealthCheckConfig> {
    match spec {
        Some(HealthCheckSpec::Single(c)) => vec![c.clone()],
        Some(HealthCheckSpec::Multiple(v)) => v.clone(),
        None => Vec::new(),
    }
}

/// Runtime state of one declared endpoint: its resolved declaration plus an
/// independently-tracked health status (health threads are stopped on drop).
pub struct Endpoint {
    /// Resolved declaration (name/host/port/path_prefix/health_check).
    pub config: EndpointConfig,
    health_status: Arc<Mutex<HealthStatus>>,
    health_stop: Arc<AtomicBool>,
}

impl Endpoint {
    fn new(config: EndpointConfig) -> Self {
        Self {
            config,
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            health_stop: Arc::new(AtomicBool::new(false)),
        }
    }
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
    /// Declared endpoints of this service, each with its own health tracking.
    pub endpoints: Vec<Endpoint>,
    /// Shell command to run on shutdown (e.g. "docker compose down").
    pub shutdown_cmd: Option<String>,
    /// Names of services this service depends on.
    pub dep_names: Vec<String>,
    /// Extra env vars resolved from `service.env` templates (`${ports.*}` etc).
    pub injected_env: std::collections::HashMap<String, String>,
    /// Git branch of the worktree this service runs in, if any. Exposed to the
    /// spawned process as `FOG_BRANCH` (DNS-safe slug, `/` → `-`) and
    /// `FOG_BRANCH_RAW` (original `feat/book`). `FOG_BRANCH_SLUG` mirrors
    /// `FOG_BRANCH` for explicitness.
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
    /// `shutdown_cmd` is skipped on teardown while another instance on the
    /// same (project, script, branch) still serves it.
    pub shared: bool,
    /// When a reused service is adopted from another instance, the child PID
    /// to wait on / kill instead of a [`Child`] handle.
    owned_pid: Option<u32>,
    /// Raw master fd of an adopted PTY (used for resizing). Unix-only; always
    /// `None` on Windows, where live handoff is unsupported.
    raw_fd: Option<Fd>,
    /// When the reused service was created, for the grace-period auto-start.
    reused_since: Option<Instant>,
    /// How long to wait for a reused resource to become healthy before
    /// starting it ourselves.
    reuse_grace: Duration,
    /// Write end of the pipe used to stop the reader thread. Unix-only.
    #[cfg_attr(not(unix), allow(dead_code))]
    stop_w: Option<Fd>,
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
    raw_output: Arc<Mutex<std::collections::VecDeque<Vec<u8>>>>,
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
            .field("endpoints", &self.endpoints.len())
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
fn check_target(config: &HealthCheckConfig, branch: Option<&str>) -> bool {
    match config.kind {
        crate::config::HealthCheckKind::Docker => check_docker_target(config, branch),
        _ => {
            let timeout = config.timeout_ms.unwrap_or(2000);
            let addr = config
                .target
                .trim_start_matches("tcp://")
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            addr.to_socket_addrs()
                .ok()
                .map(|addrs| {
                    addrs.into_iter().any(|sa| {
                        std::net::TcpStream::connect_timeout(
                            &sa,
                            std::time::Duration::from_millis(timeout),
                        )
                        .is_ok()
                    })
                })
                .unwrap_or(false)
        }
    }
}

/// Verifies a compose service is running (and, when the compose file defines a
/// healthcheck for it, reports `healthy`) by inspecting `docker compose ps`.
///
/// The compose file is resolved relative to the service's working directory at
/// build time, so `config.compose_file` is already absolute here.
///
/// `branch` (the worktree branch, exported to services as `FOG_BRANCH` (slug)
/// and `FOG_BRANCH_RAW` (raw)) is forwarded to the subprocess so
/// branch-suffixed compose project names (e.g. `redfox-${FOG_BRANCH:-main}`)
/// resolve to the running project instead of the `main` default.
fn check_docker_target(config: &HealthCheckConfig, branch: Option<&str>) -> bool {
    let timeout = config.timeout_ms.unwrap_or(2000);
    let compose_file = config
        .compose_file
        .clone()
        .unwrap_or_else(|| "docker-compose.yml".to_string());

    let mut cmd = std::process::Command::new("docker");
    cmd.args(["compose", "-f", &compose_file, "ps", "--format", "json"])
        .arg(&config.target)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(branch) = branch {
        if let Ok(slug) = crate::ports::branch_slug(branch) {
            cmd.env("FOG_BRANCH", slug.clone());
            cmd.env("FOG_BRANCH_SLUG", slug);
        }
        cmd.env("FOG_BRANCH_RAW", branch);
    }

    let mut child = match cmd.spawn() {
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
///
/// `branch` is forwarded to `docker`-kind checks so they resolve the
/// branch-suffixed compose project (see [`check_docker_target`]).
pub fn health_checks_pass(configs: &[HealthCheckConfig], branch: Option<&str>) -> bool {
    thread::scope(|s| {
        configs
            .iter()
            .map(|c| {
                let c = c.clone();
                s.spawn(move || check_target(&c, branch))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .all(|h| h.join().unwrap_or(false))
    })
}

/// Runs the adaptive health-check loop for `configs` on a background thread,
/// updating `status` until `stop` is set. Shared by a terminal's own health
/// checks and each of its declared endpoints.
fn spawn_health_loop(
    configs: Vec<HealthCheckConfig>,
    branch: Option<String>,
    status: Arc<Mutex<HealthStatus>>,
    stop: Arc<AtomicBool>,
) {
    if configs.is_empty() {
        return;
    }
    thread::spawn(move || {
        let start_interval = clamp_interval(
            configs
                .iter()
                .filter_map(|c| c.start_interval_ms)
                .min()
                .unwrap_or(DEFAULT_START_INTERVAL.as_millis() as u64),
        );
        let interval = clamp_interval(
            configs
                .iter()
                .filter_map(|c| c.interval_ms)
                .min()
                .unwrap_or(DEFAULT_HEALTH_INTERVAL.as_millis() as u64),
        );
        let start_period = Duration::from_millis(
            configs
                .iter()
                .filter_map(|c| c.start_period_ms)
                .max()
                .unwrap_or(0),
        );
        let retries = configs
            .iter()
            .map(|c| c.retries.unwrap_or(DEFAULT_HEALTH_RETRIES))
            .max()
            .unwrap_or(DEFAULT_HEALTH_RETRIES)
            .max(1);

        let started = Instant::now();
        let mut failures: u32 = 0;
        let mut ever_healthy = false;
        let mut last = HealthStatus::Unknown;

        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let pass = health_checks_pass(&configs, branch.as_deref());
            let next = if pass {
                failures = 0;
                ever_healthy = true;
                HealthStatus::Healthy
            } else {
                failures = failures.saturating_add(1);
                if !ever_healthy && started.elapsed() < start_period {
                    HealthStatus::Starting
                } else if failures < retries {
                    if last == HealthStatus::Healthy {
                        HealthStatus::Healthy
                    } else {
                        HealthStatus::Starting
                    }
                } else {
                    HealthStatus::Unhealthy
                }
            };

            if next != last {
                *status.lock().expect("health status mutex poisoned") = next;
                last = next;
                health_signal().notify();
            }

            let sleep_for = if ever_healthy {
                interval
            } else {
                start_interval
            };
            let deadline = Instant::now() + sleep_for;
            while Instant::now() < deadline {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(Duration::from_millis(50)));
            }
        }
    });
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

/// Notice shown for a borrowed (shared/reused) service, both in its tab and
/// in its log file.
fn borrow_notice(name: &str) -> String {
    format!("♻ reusing already-running '{name}'; start skipped (press R to take over)")
}

/// Creates a pipe used to signal a reader thread to stop. Returns
/// `(read_end, write_end)`.
#[cfg(unix)]
fn make_stop_pipe() -> io::Result<(Fd, Fd)> {
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
#[cfg(unix)]
fn spawn_reader(
    parser: Arc<Mutex<vt100::Parser>>,
    generation: Arc<AtomicUsize>,
    raw_output: Arc<Mutex<std::collections::VecDeque<Vec<u8>>>>,
    fd: Fd,
    stop: Fd,
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
                {
                    let mut q = raw_output.lock().expect("mutex poisoned");
                    if q.len() >= 500 {
                        q.pop_front();
                    }
                    q.push_back(buf[..n as usize].to_vec());
                }
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

/// Spawns a thread that reads PTY output from a portable-pty reader and feeds
/// the parser, until the PTY reaches EOF or the master is closed.
///
/// Windows uses ConPTY, whose output handle cannot be polled alongside a stop
/// pipe, so there is no explicit cancellation: dropping the master (which
/// closes the pseudoconsole) ends the read. Every teardown path kills the
/// child and drops the master, so the thread always terminates.
#[cfg(windows)]
fn spawn_reader_pty(
    parser: Arc<Mutex<vt100::Parser>>,
    generation: Arc<AtomicUsize>,
    raw_output: Arc<Mutex<std::collections::VecDeque<Vec<u8>>>>,
    mut reader: Box<dyn std::io::Read + Send>,
    mut tee: Option<fs::File>,
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
                    {
                        let mut q = raw_output.lock().expect("mutex poisoned");
                        if q.len() >= 500 {
                            q.pop_front();
                        }
                        q.push_back(buf[..n].to_vec());
                    }
                    if let Some(file) = tee.as_mut() {
                        let _ = file.write_all(&buf[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// Returns the user's shell, or a sensible platform default.
pub(crate) fn default_shell() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string())
    }
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
}

/// Builds a `Command` that runs `cmd` through the platform shell.
fn shell_command(cmd: &str) -> std::process::Command {
    #[cfg(unix)]
    {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    }
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new(default_shell());
        c.args(["/C", cmd]);
        c
    }
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
#[cfg(unix)]
struct FdWriter {
    fd: Fd,
}

#[cfg(unix)]
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

#[cfg(unix)]
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

        let shell = default_shell();
        let cmd = CommandBuilder::new(shell);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback)));
        let screen_generation = Arc::new(AtomicUsize::new(0));
        let raw_output = Arc::new(Mutex::new(std::collections::VecDeque::new()));

        #[cfg(unix)]
        let (handler, stop_w) = {
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
            let handler = spawn_reader(
                parser.clone(),
                screen_generation.clone(),
                raw_output.clone(),
                reader_fd,
                stop_r,
                None,
            );
            (Some(handler), Some(stop_w))
        };

        #[cfg(windows)]
        let (handler, stop_w) = {
            let reader = pair
                .master
                .try_clone_reader()
                .map_err(|e| io::Error::other(e.to_string()))?;
            let handler = spawn_reader_pty(
                parser.clone(),
                screen_generation.clone(),
                raw_output.clone(),
                reader,
                None,
            );
            (Some(handler), None)
        };

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
            injected_env: Default::default(),
            branch: None,
            project: None,
            script: String::new(),
            reused: false,
            shared: false,
            owned_pid: None,
            raw_fd: None,
            reused_since: None,
            reuse_grace: DEFAULT_REUSE_GRACE,
            endpoints: Vec::new(),
            stop_w,
            handed_off: false,
            child_reaped: false,
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            health_stop: Arc::new(AtomicBool::new(false)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            raw_output,
            handler,
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
        injected_env: std::collections::HashMap<String, String>,
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
            injected_env,
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
            raw_output: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            handler: None,
            writer: None,
            child: None,
            master: None,
            endpoints: Vec::new(),
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
            let mut p = parser.lock().expect("mutex poisoned");
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
            injected_env: Default::default(),
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
            endpoints: Vec::new(),
            health_status: Arc::new(Mutex::new(HealthStatus::Unhealthy)),
            health_stop: Arc::new(AtomicBool::new(false)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            raw_output: Arc::new(Mutex::new(std::collections::VecDeque::new())),
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
            let mut p = parser.lock().expect("mutex poisoned");
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
            injected_env: Default::default(),
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
            endpoints: Vec::new(),
            health_status: Arc::new(Mutex::new(HealthStatus::Pending)),
            health_stop: Arc::new(AtomicBool::new(false)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            raw_output: Arc::new(Mutex::new(std::collections::VecDeque::new())),
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
        let message = borrow_notice(&name);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, scrollback)));
        {
            let mut p = parser.lock().expect("mutex poisoned");
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
            injected_env: Default::default(),
            branch: None,
            project: None,
            script: String::new(),
            reused: true,
            shared: false,
            owned_pid: None,
            raw_fd: None,
            reused_since: Some(Instant::now()),
            endpoints: Vec::new(),
            reuse_grace: DEFAULT_REUSE_GRACE,
            stop_w: None,
            handed_off: false,
            child_reaped: false,
            parser,
            health_status: Arc::new(Mutex::new(HealthStatus::Unknown)),
            health_stop: Arc::new(AtomicBool::new(false)),
            screen_generation: Arc::new(AtomicUsize::new(0)),
            line_cache: RefCell::new(None),
            raw_output: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            handler: None,
            writer: None,
            child: None,
            master: None,
        }
    }

    /// Persists the borrow notice into this terminal's log file so `fog logs`
    /// shows borrowed services too. Borrowed terminals have no PTY reader
    /// thread, so nothing would otherwise land in the file. No-op when this
    /// terminal has no log dir (e.g. interactive TUI runs).
    pub(crate) fn persist_borrow_notice(&self) {
        let Some(dir) = self.log_dir.as_deref() else {
            return;
        };
        if let Ok(mut f) = open_log_file(dir, &self.name) {
            use std::io::Write;
            let _ = writeln!(f, "{}", borrow_notice(&self.name));
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
    #[cfg(unix)]
    pub fn adopt(
        path: String,
        cmd: String,
        name: String,
        scrollback: usize,
        fd: Fd,
        pid: u32,
        log_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let tee = log_dir
            .as_deref()
            .and_then(|dir| open_log_file(dir, &name).ok());
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, INITIAL_COLS, scrollback)));
        {
            let mut p = parser.lock().expect("mutex poisoned");
            p.process(
                format!("\x1b[36m♻ adopted from instance {pid} — streaming live output\x1b[0m\r\n")
                    .as_bytes(),
            );
        }
        let screen_generation = Arc::new(AtomicUsize::new(0));
        let raw_output = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let (stop_r, stop_w) = make_stop_pipe().unwrap_or((-1, -1));
        // SAFETY: dup creates an independent descriptor for the reader thread.
        let reader_fd = unsafe { libc::dup(fd) };
        let handler = if reader_fd >= 0 && stop_r >= 0 {
            Some(spawn_reader(
                parser.clone(),
                screen_generation.clone(),
                raw_output.clone(),
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
            injected_env: Default::default(),
            branch: None,
            project: None,
            script: String::new(),
            reused: true,
            shared: false,
            owned_pid: Some(pid),
            endpoints: Vec::new(),
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
            raw_output,
            handler,
            writer,
            child: None,
            master: None,
        }
    }

    /// Adopts a live PTY handed over from another fog instance.
    ///
    /// Live handoff is unsupported on Windows (ConPTY handles cannot be
    /// transferred), so no handoffs are ever produced and this is never
    /// reached. It falls back to a borrowed-reuse placeholder for
    /// completeness.
    #[cfg(windows)]
    pub fn adopt(
        path: String,
        cmd: String,
        name: String,
        scrollback: usize,
        _fd: Fd,
        _pid: u32,
        log_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let mut t = Self::spawn_reused(name, path, cmd, scrollback);
        t.log_dir = log_dir;
        t
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
    #[cfg(unix)]
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

    /// Extracts this terminal's live process for transfer to another instance.
    ///
    /// Live handoff is unsupported on Windows, so this always returns `None`,
    /// causing the successor instance to start the service fresh.
    #[cfg(windows)]
    pub fn extract_handoff(&mut self) -> Option<crate::ipc::HandoffItem> {
        None
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
        *self.health_status.lock().expect("mutex poisoned") = HealthStatus::Unknown;
        self.spawn_into(path, cmd)?;
        // A fresh spawn means running: clear any stopped flag so
        // refresh_status resumes deriving liveness. This matters for reused
        // terminals revived by maybe_auto_start after the grace period —
        // without it they report "stopped" forever despite a live process.
        self.stopped = false;
        self.process_running = true;
        Ok(())
    }

    /// Returns `true` if the service is running and (if health checks are configured) healthy.
    pub fn is_ready(&self) -> bool {
        if self.health_checks.is_empty() {
            return !self.stopped && self.process_running;
        }
        *self.health_status.lock().expect("mutex poisoned") == HealthStatus::Healthy
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

        let shell = default_shell();
        let mut cmd_builder = CommandBuilder::new(&shell);
        cmd_builder.cwd(path);
        if let Some(branch) = &self.branch {
            if let Ok(slug) = crate::ports::branch_slug(branch) {
                cmd_builder.env("FOG_BRANCH", slug.clone());
                cmd_builder.env("FOG_BRANCH_SLUG", slug);
            }
            cmd_builder.env("FOG_BRANCH_RAW", branch);
        }
        for (k, v) in &self.injected_env {
            cmd_builder.env(k.clone(), v.clone());
        }

        let child = pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))?;

        let _ = writeln!(writer, "{}", cmd);

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
        let raw_output = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        self.raw_output = raw_output.clone();

        #[cfg(unix)]
        {
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
            self.handler = Some(spawn_reader(
                self.parser.clone(),
                self.screen_generation.clone(),
                raw_output,
                reader_fd,
                stop_r,
                tee,
            ));
            self.stop_w = Some(stop_w);
        }

        #[cfg(windows)]
        {
            let reader = pair
                .master
                .try_clone_reader()
                .map_err(|e| io::Error::other(e.to_string()))?;
            self.handler = Some(spawn_reader_pty(
                self.parser.clone(),
                self.screen_generation.clone(),
                raw_output,
                reader,
                tee,
            ));
        }

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
        let mut parser = self.parser.lock().expect("mutex poisoned");
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

        let mut parser = self.parser.lock().expect("mutex poisoned");
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
        let mut parser = self.parser.lock().expect("mutex poisoned");
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

    pub fn drain_raw_output(&self) -> Vec<Vec<u8>> {
        let mut q = self.raw_output.lock().expect("mutex poisoned");
        q.drain(..).collect()
    }

    /// Returns the cursor position `(row, col)` if the cursor is visible.
    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        let parser = self.parser.lock().expect("mutex poisoned");
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
        #[cfg(unix)]
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
        let mut p = self.parser.lock().expect("mutex poisoned");
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
        #[cfg(unix)]
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
                // Snapshot the tree before signaling the leader: backgrounded
                // grandchildren in their own pgid (shell job control) survive
                // kill(-pgid), and orphans reparent once the leader exits, so
                // a post-mortem scan finds nothing.
                process::signal_tree(pid, Signal::Term);
                thread::sleep(Duration::from_millis(500));
                if process::is_pid_alive(pid) {
                    process::signal_tree(pid, Signal::Kill);
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
            process::signal_tree(pid, Signal::Term);
            thread::sleep(Duration::from_millis(500));
            match process::waitpid_nohang(pid) {
                Ok(Some(_)) => self.child_reaped = true,
                Ok(None) => process::signal_tree(pid, Signal::Kill),
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
                process::signal_tree(pid, Signal::Kill);
            }
        }

        #[cfg(unix)]
        if let Some(fd) = self.raw_fd {
            // SAFETY: fd was received via SCM_RIGHTS and is owned by us.
            unsafe { libc::close(fd) };
            self.raw_fd = None;
        }

        // Drop the PTY before joining the reader: on Windows the reader blocks
        // on a ConPTY read until the pseudoconsole is closed.
        self.master = None;
        self.writer = None;

        if let Some(handler) = self.handler.take() {
            let _ = handler.join();
        }

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
        // Tear down the previous incarnation (e.g. `docker compose down`) and
        // wait for it to finish so the fresh start does not race it or silently
        // attach to leftover containers.
        self.run_shutdown_cmd_blocking(SHUTDOWN_CMD_TIMEOUT);
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

    /// Stops the command process in this terminal without respawning it:
    /// kills the running process and runs the `shutdown_cmd`, then marks the
    /// terminal stopped so its status reports not-running.
    ///
    /// This is what [`restart`](Self::restart) does minus the fresh spawn.
    ///
    /// # Errors
    /// Always succeeds; the signature mirrors [`restart`](Self::restart) so the
    /// two can be handled uniformly.
    pub fn stop(&mut self) -> io::Result<()> {
        self.kill_inner();
        // Tear down the previous incarnation (e.g. `docker compose down`) just
        // like restart does, so a stopped compose-style service does not leave
        // its containers running.
        self.run_shutdown_cmd();
        self.stopped = true;
        Ok(())
    }

    /// Returns the current health status.
    pub fn get_health_status(&self) -> HealthStatus {
        *self.health_status.lock().expect("mutex poisoned")
    }

    /// Sets the current health status. Used to seed a reused terminal with
    /// `Healthy` right after a successful startup probe so it does not flicker
    /// as stopped until the first background check runs.
    pub fn set_health_status(&self, s: HealthStatus) {
        *self.health_status.lock().expect("mutex poisoned") = s;
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
        let healthy = health_checks_pass(&self.health_checks, self.branch.as_deref());
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
        let mut parser = self.parser.lock().expect("mutex poisoned");
        parser.process(message.as_bytes());
        *self.line_cache.borrow_mut() = None;
    }

    /// Starts a background thread that periodically runs all configured health checks.
    /// The service is considered healthy only when ALL checks pass.
    pub fn start_health_checks(&self) {
        spawn_health_loop(
            self.health_checks.clone(),
            self.branch.clone(),
            self.health_status.clone(),
            self.health_stop.clone(),
        );
    }

    /// Replaces this terminal's declared endpoints (endpoints) with
    /// `subs`, seeding each at `Unknown`.
    ///
    /// Endpoints carry their own health checks; call
    /// [`start_endpoint_health_checks`](Self::start_endpoint_health_checks)
    /// afterwards to begin polling them.
    pub fn set_endpoints(&mut self, subs: Vec<EndpointConfig>) {
        self.endpoints = subs.into_iter().map(Endpoint::new).collect();
    }

    /// Starts one background health thread per declared endpoint that has
    /// health checks configured. Idempotent-ish: call once per terminal build.
    pub fn start_endpoint_health_checks(&self) {
        let branch = self.branch.clone();
        for sub in &self.endpoints {
            let checks = endpoint_health_checks(&sub.config.health_check);
            spawn_health_loop(
                checks,
                branch.clone(),
                sub.health_status.clone(),
                sub.health_stop.clone(),
            );
        }
    }

    /// Current health reading for each declared endpoint, in declared order.
    pub fn endpoint_statuses(&self) -> Vec<crate::ipc::EndpointStatus> {
        self.endpoints
            .iter()
            .map(|s| crate::ipc::EndpointStatus {
                name: s.config.name.clone(),
                health: format!("{:?}", s.health_status.lock().expect("mutex poisoned"))
                    .to_lowercase(),
            })
            .collect()
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
                // The handle was received via SCM_RIGHTS and is owned by us.
                crate::fds::close(fd);
            }
            self.process_running = false;
        }
        // Reused services have no owned process; their state is driven by
        // health checks (or assumed up when none are configured).
        if self.reused {
            let healthy =
                *self.health_status.lock().expect("mutex poisoned") == HealthStatus::Healthy;
            let up = self.health_checks.is_empty() || healthy;
            self.process_running = up;
            self.stopped = !up;
            return;
        }
        if matches!(
            *self.health_status.lock().expect("mutex poisoned"),
            HealthStatus::Pending | HealthStatus::Starting
        ) {
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
        let healthy = *self.health_status.lock().expect("mutex poisoned") == HealthStatus::Healthy;
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
    /// Builds the service's `shutdown_cmd` as a process in a fresh session, in
    /// the service's working directory. Returns `None` when no `shutdown_cmd`
    /// is configured.
    fn shutdown_cmd_process(&self) -> Option<std::process::Command> {
        let shutdown_cmd = self.shutdown_cmd.as_ref()?;
        let cwd = match &self.init {
            Init::Command { path, .. } if !path.is_empty() => Some(path.as_str()),
            _ => None,
        };
        let mut cmd = shell_command(shutdown_cmd);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(branch) = &self.branch {
            if let Ok(slug) = crate::ports::branch_slug(branch) {
                cmd.env("FOG_BRANCH", slug.clone());
                cmd.env("FOG_BRANCH_SLUG", slug);
            }
            cmd.env("FOG_BRANCH_RAW", branch);
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
        Some(cmd)
    }

    /// Runs the service's `shutdown_cmd` without waiting for it. Used at
    /// teardown (`Drop`) and on `stop`, where blocking would delay quit or a
    /// worktree switch; `restart` uses `run_shutdown_cmd_blocking`.
    fn run_shutdown_cmd(&self) {
        if let Some(mut cmd) = self.shutdown_cmd_process() {
            let _ = cmd.spawn();
        }
    }

    /// Runs the service's `shutdown_cmd` and waits up to `timeout` for it to
    /// exit, escalating to SIGKILL on the process tree if it does not. Returns
    /// `true` if the command exited within the timeout, `false` if it was
    /// killed (or there was nothing to run).
    ///
    /// Used by restart so the replacement command starts only after the old
    /// incarnation is actually torn down (e.g. `docker compose down` finished),
    /// instead of racing it.
    fn run_shutdown_cmd_blocking(&self, timeout: Duration) -> bool {
        let Some(mut cmd) = self.shutdown_cmd_process() else {
            return false;
        };
        self.notice("running shutdown command…\n");
        let Ok(child) = cmd.spawn() else {
            return false;
        };
        let pid = child.id();
        let exited = wait_reaped(pid, timeout);
        if !exited {
            process::signal_tree(pid, Signal::Kill);
            wait_reaped(pid, Duration::from_secs(2));
        }
        exited
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Stop the health-check thread so it does not outlive this terminal
        // (relevant when services are replaced by an in-place worktree switch).
        self.health_stop.store(true, Ordering::SeqCst);
        for sub in &self.endpoints {
            sub.health_stop.store(true, Ordering::SeqCst);
        }
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
        // A shared (reuse/share) service is only torn down when this is the
        // last instance serving the same (project, script, branch) — a
        // concurrent same-branch instance may still be using it, so its
        // `shutdown_cmd` (e.g. `docker compose down`) must not run while a
        // sibling on the same branch is alive. Other branches run their own
        // (branch-suffixed) resources and must not keep this one alive: e.g.
        // `red-fox-infra-${FOG_BRANCH}` gives every branch its own containers.
        // A shared service that was started (not borrowed) here is covered by
        // `shared`, not just `reused`.
        let is_shared = self.reused || self.shared;
        let last_instance = !is_shared
            || self.project.is_none()
            || self.script.is_empty()
            || crate::ipc::find_instances_for(
                self.project.as_deref().unwrap_or_default(),
                &self.script,
                self.branch.as_deref(),
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
    #[cfg(unix)]
    static DOCKER_STUB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn test_endpoint_statuses_seed_unknown() {
        let mut t = Terminal::spawn_reused("infra".into(), ".".into(), "true".into(), 10);
        t.set_endpoints(vec![EndpointConfig {
            name: "web".into(),
            host: None,
            port: None,
            path_prefix: None,
            health_check: None,
        }]);
        let statuses = t.endpoint_statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].name, "web");
        assert_eq!(statuses[0].health, "unknown");
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_kills_backgrounded_grandchildren() {
        // Regression test for orphaned listeners: a service that backgrounds
        // a long-lived child (its own pgid under shell job control, like an
        // `exec socat` listener) must take it down on stop.
        let dir = std::env::temp_dir().join(format!("fog-test-stop-tree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("bg.pid");
        let cmd = format!("sleep 60 & echo $! > {}; sleep 60", pidfile.display());
        let mut t =
            Terminal::spawn_command(".", &cmd, "svc".into(), 100, None, None, Default::default())
                .unwrap();
        // Wait for the background child to appear (up to ~5s).
        let mut bg = 0;
        for _ in 0..50 {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                bg = pid;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(bg != 0, "background child never started");
        assert!(crate::process::is_pid_alive(bg));
        t.stop().unwrap();
        // The whole tree must be gone (up to ~5s for SIGTERM grace + KILL).
        let mut dead = false;
        for _ in 0..50 {
            if !crate::process::is_pid_alive(bg) {
                dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(dead, "background child {bg} survived stop()");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_persist_borrow_notice_writes_log_file() {
        let dir = std::env::temp_dir().join(format!("fog-test-borrow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut t = Terminal::spawn_reused("db".into(), ".".into(), "true".into(), 100);
        // No log dir: no-op, no file created.
        t.persist_borrow_notice();
        assert!(!dir.join("db.log").exists());
        t.log_dir = Some(dir.clone());
        t.persist_borrow_notice();
        let content = std::fs::read_to_string(dir.join("db.log")).unwrap();
        assert!(content.contains("reusing already-running 'db'"));
        std::fs::remove_dir_all(&dir).ok();
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
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
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
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        }];
        assert!(!health_checks_pass(&closed, None));

        // A live listener passes, and stays passing next to a reachable check.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let open = vec![HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target: addr.to_string(),
            compose_file: None,
            interval_ms: None,
            timeout_ms: Some(200),
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        }];
        assert!(health_checks_pass(&open, None));

        // All checks must pass: one reachable + one closed fails.
        let mixed = vec![
            HealthCheckConfig {
                kind: crate::config::HealthCheckKind::Http,
                target: format!("http://{}", addr),
                compose_file: None,
                interval_ms: None,
                timeout_ms: Some(200),
                start_interval_ms: None,
                start_period_ms: None,
                retries: None,
            },
            HealthCheckConfig {
                kind: crate::config::HealthCheckKind::Tcp,
                target: "127.0.0.1:1".into(),
                compose_file: None,
                interval_ms: None,
                timeout_ms: Some(100),
                start_interval_ms: None,
                start_period_ms: None,
                retries: None,
            },
        ];
        assert!(!health_checks_pass(&mixed, None));
    }

    /// Polls a terminal's health status until it matches `want` or times out.
    fn wait_for_health(t: &Terminal, want: HealthStatus, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if t.get_health_status() == want {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        t.get_health_status() == want
    }

    fn tcp_check(target: String) -> HealthCheckConfig {
        HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Tcp,
            target,
            compose_file: None,
            interval_ms: None,
            timeout_ms: Some(200),
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        }
    }

    #[test]
    fn test_health_probe_is_immediate_not_interval_delayed() {
        // A reachable service must be marked Healthy by the immediate startup
        // probe, not after the (long) start interval elapses.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut t = Terminal::spawn_reused("svc".into(), ".".into(), "true".into(), 100);
        let mut check = tcp_check(addr.to_string());
        check.start_interval_ms = Some(5000);
        check.interval_ms = Some(5000);
        t.health_checks.push(check);
        t.start_health_checks();
        assert!(
            wait_for_health(&t, HealthStatus::Healthy, Duration::from_millis(2000)),
            "immediate probe should mark the service healthy without waiting the interval"
        );
    }

    #[test]
    fn test_health_retries_starting_then_unhealthy() {
        let mut t = Terminal::spawn_reused("svc".into(), ".".into(), "true".into(), 100);
        let mut check = tcp_check("127.0.0.1:1".into());
        check.start_interval_ms = Some(20);
        check.retries = Some(3);
        t.health_checks.push(check);
        t.start_health_checks();
        assert!(
            wait_for_health(&t, HealthStatus::Starting, Duration::from_millis(1000)),
            "below the retries threshold the service reports Starting"
        );
        assert!(!t.is_ready(), "Starting is not ready");
        assert!(
            wait_for_health(&t, HealthStatus::Unhealthy, Duration::from_millis(1000)),
            "after `retries` consecutive failures the service reports Unhealthy"
        );
    }

    #[test]
    fn test_health_start_period_holds_starting() {
        let mut t = Terminal::spawn_reused("svc".into(), ".".into(), "true".into(), 100);
        let mut check = tcp_check("127.0.0.1:1".into());
        check.start_interval_ms = Some(20);
        check.start_period_ms = Some(10_000);
        check.retries = Some(1);
        t.health_checks.push(check);
        t.start_health_checks();
        assert!(wait_for_health(
            &t,
            HealthStatus::Starting,
            Duration::from_millis(1000)
        ));
        // Inside the grace window, failures never flip the status to Unhealthy.
        thread::sleep(Duration::from_millis(200));
        assert_eq!(t.get_health_status(), HealthStatus::Starting);
    }

    #[test]
    fn test_health_interval_is_clamped_to_minimum() {
        assert_eq!(clamp_interval(1), MIN_HEALTH_INTERVAL);
        assert_eq!(clamp_interval(100), MIN_HEALTH_INTERVAL);
        assert_eq!(clamp_interval(5000), Duration::from_millis(5000));
    }

    #[test]
    fn test_health_signal_pings_subscribers() {
        let signal = Arc::new(HealthSignal::new());
        let rx = signal.subscribe();
        assert!(rx.try_recv().is_err(), "no ping before notify");
        signal.notify();
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_ok(),
            "subscriber should be woken by notify"
        );
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

    #[cfg(unix)]
    #[test]
    fn test_check_docker_target_exports_fog_branch() {
        let _lock = DOCKER_STUB_LOCK.lock().unwrap();
        // A stub `docker` on PATH that only reports the api healthy when the
        // probe exports `FOG_BRANCH` — regression test for branch-suffixed
        // compose projects (e.g. `redfox-${FOG_BRANCH:-main}`) resolving to
        // the wrong (main) project during the health check.
        let dir = std::env::temp_dir().join(format!("fog-stub-docker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("docker");
        std::fs::write(
            &stub,
            "#!/bin/sh\n\
             [ \"$FOG_BRANCH\" = \"ui\" ] || { echo '[]'; exit 0; }\n\
             echo '[{\"Service\":\"api\",\"State\":\"running\",\"Health\":\"healthy\"}]'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let prev_path = std::env::var("PATH").unwrap_or_default();
        let stub_path = format!("{}:{}", dir.display(), prev_path);
        // SAFETY: the stub only shadows the `docker` binary, and no other test
        // in this binary spawns `docker`, so the PATH mutation cannot affect a
        // concurrently running test.
        unsafe { std::env::set_var("PATH", &stub_path) };

        let config = HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Docker,
            target: "api".into(),
            compose_file: Some("compose.yml".into()),
            interval_ms: None,
            timeout_ms: Some(2000),
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        };
        assert!(
            check_docker_target(&config, Some("ui")),
            "branch must be exported to the docker compose ps probe"
        );
        assert!(
            !check_docker_target(&config, None),
            "without FOG_BRANCH the probe must fail (resolves the main project)"
        );

        // SAFETY: restores the original PATH for any later test.
        unsafe { std::env::set_var("PATH", &prev_path) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_docker_target_exports_slug_for_slashed_branch() {
        let _lock = DOCKER_STUB_LOCK.lock().unwrap();
        // Regression for `feat/barber` style branches: `spawn_into` exports
        // FOG_BRANCH as the slug (`feat-barber`) so compose project
        // `red-fox-infra-${FOG_BRANCH:-main}` matches. The health probe must
        // do the same – raw `feat/barber` would resolve to a non-existent
        // `red-fox-infra-feat/barber` project and incorrectly report unhealthy.
        let dir =
            std::env::temp_dir().join(format!("fog-stub-docker-slash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("docker");
        std::fs::write(
            &stub,
            "#!/bin/sh\n\
             [ \"$FOG_BRANCH\" = \"feat-barber\" ] || { echo '[]'; exit 0; }\n\
             [ \"$FOG_BRANCH_RAW\" = \"feat/barber\" ] || { echo '[]'; exit 0; }\n\
             [ \"$FOG_BRANCH_SLUG\" = \"feat-barber\" ] || { echo '[]'; exit 0; }\n\
             echo '[{\"Service\":\"api\",\"State\":\"running\",\"Health\":\"healthy\"}]'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let prev_path = std::env::var("PATH").unwrap_or_default();
        let stub_path = format!("{}:{}", dir.display(), prev_path);
        // SAFETY: stub only shadows `docker`, no other test spawns `docker` concurrently.
        unsafe { std::env::set_var("PATH", &stub_path) };

        let config = HealthCheckConfig {
            kind: crate::config::HealthCheckKind::Docker,
            target: "api".into(),
            compose_file: Some("compose.yml".into()),
            interval_ms: None,
            timeout_ms: Some(2000),
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        };
        assert!(
            check_docker_target(&config, Some("feat/barber")),
            "slashed branch must be slugified for FOG_BRANCH (feat/barber -> feat-barber)"
        );

        // SAFETY: restores original PATH.
        unsafe { std::env::set_var("PATH", &prev_path) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
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
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
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
            start_interval_ms: None,
            start_period_ms: None,
            retries: None,
        });
        t.probe_health();
        assert_eq!(t.get_health_status(), HealthStatus::Unhealthy);

        t.kill_inner();
    }

    /// Runs `shutdown_cmd` (a `touch`) and waits up to `timeout` for `marker`.
    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn test_shutdown_cmd_blocking_waits_for_completion() {
        let marker = std::env::temp_dir().join(format!(
            "fog-test-shutdown-blocking-{}.marker",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        let mut t = Terminal::spawn_reused("db".into(), ".".into(), "true".into(), 100);
        t.shutdown_cmd = Some(format!("sleep 0.3 && touch {}", marker.display()));

        let start = std::time::Instant::now();
        let exited = t.run_shutdown_cmd_blocking(Duration::from_secs(5));
        let elapsed = start.elapsed();

        let seen = marker.exists();
        let _ = fs::remove_file(&marker);
        assert!(exited, "shutdown_cmd should exit within the timeout");
        assert!(seen, "restart must wait for shutdown_cmd to finish");
        assert!(
            elapsed >= Duration::from_millis(300),
            "blocking shutdown must not return before the command exits (took {elapsed:?})"
        );
    }

    #[test]
    fn test_shutdown_cmd_blocking_noop_when_unset() {
        let t = Terminal::spawn_reused("db".into(), ".".into(), "true".into(), 100);
        let start = std::time::Instant::now();
        let exited = t.run_shutdown_cmd_blocking(Duration::from_secs(5));
        assert!(!exited);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a service without shutdown_cmd must not block"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_shutdown_cmd_blocking_kills_on_timeout() {
        let mut t = Terminal::spawn_reused("db".into(), ".".into(), "true".into(), 100);
        t.shutdown_cmd = Some("sleep 30".to_string());

        let start = std::time::Instant::now();
        let exited = t.run_shutdown_cmd_blocking(Duration::from_millis(300));
        assert!(
            !exited,
            "a shutdown_cmd that outlives the timeout is killed"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the timeout must bound the wait, not the command's own duration"
        );
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    /// Binds a fake live instance socket (`$TMPDIR/fog-<pid>.sock`) that answers
    /// status requests, so it is discovered as a sibling instance.
    #[cfg(unix)]
    fn spawn_status_instance(
        pid: u32,
        project: &str,
        script: &str,
        branch: Option<&str>,
    ) -> std::path::PathBuf {
        let state = std::sync::Arc::new(crate::ipc::IpcState::new(
            script.to_string(),
            Some(project.to_string()),
            branch.map(str::to_string),
            false,
        ));
        let path = std::env::temp_dir().join(format!("fog-{pid}.sock"));
        let _ = fs::remove_file(&path);
        let listener = crate::ipc::transport::Listener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                crate::ipc::handle_connection(stream, state.clone());
            }
        });
        path
    }

    #[cfg(unix)]
    #[test]
    fn test_drop_tears_down_shared_with_sibling_on_other_branch() {
        // Branch-scoped shared resources (e.g. `red-fox-infra-${FOG_BRANCH}`):
        // a sibling on another branch must not keep this branch's resource up.
        let project = format!("fog-test-drop-cross-{}", std::process::id());
        let sibling_pid = std::process::id().wrapping_add(11);
        let sibling = spawn_status_instance(sibling_pid, &project, "dev", Some("other"));
        let marker =
            std::env::temp_dir().join(format!("fog-test-drop-cross-{}.marker", std::process::id()));
        let _ = fs::remove_file(&marker);

        let mut t = Terminal::spawn_reused("infra".into(), ".".into(), "true".into(), 100);
        t.shared = true;
        t.project = Some(project);
        t.script = "dev".into();
        t.branch = Some("mine".into());
        t.shutdown_cmd = Some(format!("touch {}", marker.display()));
        drop(t);

        let seen = wait_for_marker(&marker, Duration::from_secs(5));
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&sibling);
        assert!(
            seen,
            "a shared resource must be torn down while only another branch runs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_drop_skips_shared_teardown_with_sibling_on_same_branch() {
        let project = format!("fog-test-drop-same-{}", std::process::id());
        let sibling_pid = std::process::id().wrapping_add(12);
        let sibling = spawn_status_instance(sibling_pid, &project, "dev", Some("mine"));
        let marker =
            std::env::temp_dir().join(format!("fog-test-drop-same-{}.marker", std::process::id()));
        let _ = fs::remove_file(&marker);

        let mut t = Terminal::spawn_reused("infra".into(), ".".into(), "true".into(), 100);
        t.shared = true;
        t.project = Some(project);
        t.script = "dev".into();
        t.branch = Some("mine".into());
        t.shutdown_cmd = Some(format!("touch {}", marker.display()));
        drop(t);

        std::thread::sleep(Duration::from_millis(500));
        let seen = marker.exists();
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&sibling);
        assert!(
            !seen,
            "a shared resource must stay up while a same-branch sibling runs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_handoff_live_process() {
        let mut t = Terminal::spawn_command(
            ".",
            "echo hello-fog",
            "svc".into(),
            100,
            None,
            None,
            Default::default(),
        )
        .unwrap();
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
            let mut p = t.parser.lock().expect("mutex poisoned");
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
        let (rows, cols) = t.parser.lock().expect("mutex poisoned").screen().size();
        assert_eq!(rows, 30);
        assert_eq!(cols, 80);

        t.resize(50, 20);
        let (rows, cols) = t.parser.lock().expect("mutex poisoned").screen().size();
        assert_eq!(rows, 20, "height must track the visible area");
        assert_eq!(cols, 80, "width must never shrink");
    }

    /// Regression: when the terminal screen is wider than the render area (a
    /// vertical scrollbar steals a column, or the window shrank and width is
    /// grow-only), a full-width row wraps onto a second row. The last line must
    /// stay visible (bottom-anchored) and its selection highlight must land on
    /// the row that actually shows it — both were broken when wrapping pushed
    /// the last line below the visible clip.
    #[test]
    fn test_last_line_selectable_when_screen_wider_than_area() {
        use crate::render;
        use crate::selection::RowLayout;
        use crate::theme::Theme;
        use ratatui::Terminal as RatatuiTerminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::widgets::Block;

        let mut t = Terminal::spawn_reused("svc".into(), ".".into(), "true".into(), 100);
        {
            let mut p = t.parser.lock().expect("mutex poisoned");
            // Screen is 21 cols; the render area is only 20 wide (scrollbar).
            p.screen_mut().set_size(3, 21);
            p.process(b"\x1b[2J\x1b[H");
            // Line 0 fills the screen (wraps over 2 rows); the last line is
            // short so it is the whole content of the bottom row.
            p.process(b"AAAAAAAAAAAAAAAAAAAAA\r\nBBBB\r\nCCCC");
        }

        let content_area = Rect::new(0, 0, 22, 5); // inner region: 20x3
        let backend = TestBackend::new(22, 5);
        let mut tt = RatatuiTerminal::new(backend).unwrap();
        let mut layout: RowLayout = Vec::new();
        tt.draw(|f| {
            layout = render::draw_terminal_content(
                f,
                content_area,
                Block::bordered(),
                std::slice::from_mut(&mut t),
                0,
                0,
                Some((2, 0)),
                Some((2, 20)),
                false,
                3,
                &Theme::default(),
            );
        })
        .unwrap();

        let buf = tt.backend().buffer();
        // Bottom content row = buffer row 3 (border at rows 0 and 4).
        let bottom: String = (1..21).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert!(
            bottom.contains("CCCC"),
            "last line must render on the bottom content row, got: {bottom:?}"
        );
        assert!(
            buf[(1, 3)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "last line selection must be highlighted on the bottom content row"
        );
        // The layout must map the bottom content row to the last line.
        assert_eq!(layout.last(), Some(&Some((2, 0))));
    }

    #[cfg(unix)]
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
            Default::default(),
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

    #[cfg(unix)]
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
