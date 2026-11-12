//! Detached-mode plumbing: the daemon child's log directory, its redirected
//! stdio, the append-only startup phase file, and the parent's checklist.
//!
//! The parent narrates startup from two independent channels — the phase file
//! (available before the ports settle) and IPC status (available after) — so
//! either one alone can make progress visible.

use crate::cli::presentation::{color_enabled, error_label};
use crate::cli::progress::StartProgress;
use crate::ipc;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long the parent of a detached run waits for the daemon to start serving
/// before reporting failure. Covers the owner-lock wait plus the reclaim.
pub(crate) const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a detached starter (`fog <script> -d`) waits for every service to
/// start once the daemon's socket is up. On timeout it warns rather than
/// failing: the daemon is already serving.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(120);
/// Append-only setup-phase file a detached daemon writes into its log dir for
/// the parent to narrate startup (the IPC socket, the other progress channel,
/// only exists once the ports are settled).
const STARTUP_PROGRESS_FILE: &str = "startup.progress";

/// Creates the per-instance log directory for the current process, which
/// every run (interactive or detached) tees each service's output into.
///
/// Stale directories from finished instances are pruned first, so the captured
/// logs of past runs cannot pile up in the temp directory.
pub(crate) fn create_log_dir() -> io::Result<PathBuf> {
    ipc::prune_stale_instance_logs();
    let dir = ipc::instance_log_dir(std::process::id());
    fs::create_dir_all(&dir)?;
    // Restrict to the owner, matching `ipc::ensure_instance_dir`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dir)?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&dir, perms)?;
    }
    Ok(dir)
}

/// Redirects this process's stdout/stderr into `daemon.log` inside `dir`,
/// so a detached daemon's own diagnostics are captured too. Only called for
/// detached runs — an interactive run keeps stdout/stderr for the TUI.
#[cfg(unix)]
pub(crate) fn redirect_daemon_output(dir: &Path) -> io::Result<()> {
    use std::os::unix::io::IntoRawFd;
    let log = fs::File::create(dir.join("daemon.log"))?;
    // SAFETY: dup2 onto the standard fds is always valid, and the original
    // fd is ours to close.
    let fd = log.into_raw_fd();
    unsafe {
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
        libc::close(fd);
    }
    Ok(())
}

/// Redirects this process's stdout/stderr into `daemon.log` inside `dir`.
///
/// Windows has no `dup2`; instead the standard handles are pointed at the log
/// file, which Rust's stdout/stderr resolve on each write.
#[cfg(windows)]
pub(crate) fn redirect_daemon_output(dir: &Path) -> io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    let log = fs::File::create(dir.join("daemon.log"))?;
    // Leak the handle so the log stays open for the process lifetime.
    let handle = log.into_raw_handle();
    // SAFETY: both are valid standard-handle selectors and the file handle
    // stays open for the process lifetime.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
    Ok(())
}

/// Append-only setup-phase log a detached daemon writes for its parent to tail.
/// Best-effort: failing to open or write it only costs live progress, never the
/// run.
pub(crate) struct ProgressLog {
    file: Option<fs::File>,
}

impl ProgressLog {
    /// Opens `<log_dir>/startup.progress`, replacing any stale file.
    pub(crate) fn create(log_dir: &Path) -> Self {
        Self {
            file: fs::File::create(log_dir.join(STARTUP_PROGRESS_FILE)).ok(),
        }
    }

    /// Records that `phase` has started. The parent treats every line but the
    /// last as complete.
    fn phase(&mut self, phase: &str) {
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{phase}");
            let _ = file.flush();
        }
    }
}

/// Records a setup phase when the caller runs detached.
pub(crate) fn note_phase(progress: &mut Option<ProgressLog>, phase: &str) {
    if let Some(p) = progress.as_mut() {
        p.phase(phase);
    }
}

/// Reads the setup phases a detached daemon has reported so far, oldest first.
fn read_startup_phases(pid: u32) -> Vec<String> {
    let path = ipc::instance_log_dir(pid).join(STARTUP_PROGRESS_FILE);
    match fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Prints the standard "started in background" block used by `-d` and detach.
pub(crate) fn print_daemon_started(script: &str, pid: u32) {
    println!("fog '{script}' started in background (pid {pid})");
    println!("  status: fog ls {pid}");
    println!("  stop:   fog kill {pid}");
    println!("  logs:   fog logs {pid}");
    println!("  log dir: {}", ipc::instance_log_dir(pid).display());
}

/// Detach mode entry point: re-executes `fog <script>` as a background daemon,
/// narrating setup with a docker-style checklist until every service has
/// started, then prints the PID.
pub(crate) fn daemonize(script: &str) -> io::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fog"));
    let args: Vec<OsString> = std::env::args_os()
        .skip(1)
        .filter(|a| {
            let s = a.to_string_lossy();
            s != "-d" && s != "--detach" && !s.starts_with("--detach=")
        })
        .collect();

    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .env("FOG_DAEMON_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::process::detach_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::other(format!("could not start detached fog: {e}")))?;
    let pid = child.id();

    // The daemon reports setup phases through an append-only file (the socket
    // does not exist until the ports are settled) and its per-service state
    // over IPC once it serves. Poll the socket first — a status reply means
    // `fog ls` / `fog kill` / `fog logs` already work — then keep going until
    // every service has started, so we report success when the environment is
    // actually up, not merely discoverable.
    let socket = ipc::socket_path(pid);
    let socket_deadline = std::time::Instant::now() + DAEMON_READY_TIMEOUT;
    let mut services_deadline: Option<std::time::Instant> = None;
    let mut socket_ready = false;
    let mut view = StartProgress::new(pid, script, stdout().is_terminal(), color_enabled());

    loop {
        let phases = read_startup_phases(pid);
        let status = ipc::query_status(&socket).ok();
        if status.is_some() && !socket_ready {
            socket_ready = true;
            services_deadline = Some(std::time::Instant::now() + DAEMON_START_TIMEOUT);
        }
        let all_started = status.as_ref().is_some_and(|s| s.started);
        let _ = view.render(&mut stdout(), &phases, status.as_ref(), all_started);
        if all_started {
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            let _ = view.finish(&mut stdout());
            eprintln!(
                "{} detached fog '{script}' (pid {pid}) exited during startup",
                error_label()
            );
            if let Some(hint) = ipc::daemon_error_hint(pid) {
                eprintln!("  {hint}");
            }
            eprintln!("  logs: {}", ipc::instance_log_dir(pid).display());
            std::process::exit(1);
        }
        if !socket_ready && std::time::Instant::now() >= socket_deadline {
            let _ = view.finish(&mut stdout());
            eprintln!(
                "{} detached fog '{script}' (pid {pid}) did not become ready within {}s",
                error_label(),
                DAEMON_READY_TIMEOUT.as_secs()
            );
            if let Some(hint) = ipc::daemon_error_hint(pid) {
                eprintln!("  {hint}");
            }
            eprintln!("  logs: {}", ipc::instance_log_dir(pid).display());
            std::process::exit(1);
        }
        if services_deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            let _ = view.finish(&mut stdout());
            eprintln!(
                "warning: fog '{script}' (pid {pid}) is serving, but not every service started within {}s",
                DAEMON_START_TIMEOUT.as_secs()
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = view.finish(&mut stdout());

    print_daemon_started(script, pid);
    Ok(())
}
