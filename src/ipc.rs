use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem;
use std::os::unix::io::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const READ_TIMEOUT_SECS: u64 = 5;

/// Status snapshot of a single service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// Display name of the service.
    pub name: String,
    /// Whether the process is actively running.
    pub running: bool,
    /// Health check state: `pending`, `unknown`, `healthy`, or `unhealthy`.
    pub health: String,
}

/// Status snapshot of the reverse proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyStatus {
    /// Whether the proxy server is currently running.
    pub running: bool,
    /// The port the proxy listens on.
    pub port: u16,
}

/// A single service handed over to a replacing instance.
///
/// The PTY master fd is delivered separately via SCM_RIGHTS.
pub struct HandoffItem {
    /// Display name of the service.
    pub name: String,
    /// Process group leader of the running service.
    pub pid: u32,
    /// Scrollback text captured from the previous instance.
    pub scrollback: Vec<String>,
    /// A dup of the PTY master fd (now owned by the receiver).
    pub fd: RawFd,
}

/// Shared state published by the TUI app and served over the IPC socket.
pub struct IpcState {
    /// Current status of all services.
    pub services: Arc<Mutex<Vec<ServiceStatus>>>,
    /// Current proxy status, if a proxy is configured.
    pub proxy: Arc<Mutex<Option<ProxyStatus>>>,
    /// Name of the script currently running.
    pub script: String,
    /// Identity of the git project (worktree family) this instance belongs to.
    pub project: Option<String>,
    /// Set to `true` when a kill request is received.
    pub kill_flag: Arc<AtomicBool>,
    /// Names of services whose `shutdown_cmd` should be skipped on exit
    /// (set by a reclaim/reuse kill request).
    pub reuse_skip: Arc<Mutex<Vec<String>>>,
    /// Services requested for handover by a replacing instance.
    pub handoff_req: Arc<Mutex<Option<Vec<String>>>>,
    /// Handoff results filled in by the App after a handover request.
    pub handoff_results: Arc<Mutex<Vec<HandoffItem>>>,
    /// Set by the IPC thread once handoffs have been sent to the requester.
    pub handoff_done: Arc<AtomicBool>,
}

impl IpcState {
    /// Creates a new empty [`IpcState`] for the given script name.
    pub fn new(script: String, project: Option<String>) -> Self {
        Self {
            services: Arc::new(Mutex::new(Vec::new())),
            proxy: Arc::new(Mutex::new(None)),
            script,
            project,
            kill_flag: Arc::new(AtomicBool::new(false)),
            reuse_skip: Arc::new(Mutex::new(Vec::new())),
            handoff_req: Arc::new(Mutex::new(None)),
            handoff_results: Arc::new(Mutex::new(Vec::new())),
            handoff_done: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The status response sent back to a `status` request.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    /// PID of the fog process.
    pub pid: u32,
    /// Name of the script being run.
    pub script: String,
    /// Service status snapshots.
    pub services: Vec<ServiceStatus>,
    /// Proxy status, if a proxy is configured.
    pub proxy: Option<ProxyStatus>,
    /// Git project identity of the instance, if it is inside a repository.
    #[serde(default)]
    pub project: Option<String>,
}

/// A request received over the IPC socket.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Status,
    Kill {
        /// Names of services whose `shutdown_cmd` should be skipped, so a
        /// replacing instance can reuse them.
        #[serde(default)]
        reuse: Vec<String>,
    },
}

/// The response sent back to a `kill` request.
#[derive(Debug, Serialize)]
struct KillResponse {
    ok: bool,
}

/// Returns the socket path for a given PID: `$TMPDIR/fog-<pid>.sock`.
pub fn socket_path(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("fog-{pid}.sock"))
}

/// Returns the socket path for the current process.
pub fn current_socket_path() -> PathBuf {
    socket_path(std::process::id())
}

/// Spawns the IPC server thread that listens on the current process's socket.
///
/// Any pre-existing file at the socket path is removed first; since the socket
/// name is derived from this process's PID, any existing file must be stale.
///
/// # Errors
/// Returns an error if the socket could not be bound.
pub fn spawn_server(state: Arc<IpcState>) -> io::Result<()> {
    let path = current_socket_path();
    let _ = fs::remove_file(&path);

    let listener = UnixListener::bind(&path)?;

    thread::spawn(move || {
        for conn in listener.incoming() {
            let state = state.clone();
            match conn {
                Ok(stream) => {
                    thread::spawn(move || handle_connection(stream, state));
                }
                Err(_) => continue,
            }
        }
    });

    Ok(())
}

/// Removes the current process's socket file (e.g. on graceful exit).
pub fn cleanup_socket() {
    let _ = fs::remove_file(current_socket_path());
}

/// Handles a single IPC connection: reads one request line and writes a response.
fn handle_connection(stream: UnixStream, state: Arc<IpcState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)));
    let mut line = String::new();
    let mut reader = match stream.try_clone() {
        Ok(r) => BufReader::new(r),
        Err(_) => return,
    };
    if reader.read_line(&mut line).is_err() {
        return;
    }
    drop(reader);

    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(_) => return,
    };

    match req {
        Request::Status => {
            let services = state
                .services
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let proxy = state
                .proxy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let resp = serde_json::to_string(&StatusResponse {
                pid: std::process::id(),
                script: state.script.clone(),
                services,
                proxy,
                project: state.project.clone(),
            })
            .unwrap_or_default();
            let mut writer = stream;
            let _ = writeln!(writer, "{resp}");
        }
        Request::Kill { reuse } => {
            state.kill_flag.store(true, Ordering::SeqCst);
            if !reuse.is_empty() {
                *state.reuse_skip.lock().unwrap_or_else(|e| e.into_inner()) = reuse.clone();
                *state.handoff_req.lock().unwrap_or_else(|e| e.into_inner()) = Some(reuse);
            }
            send_handoffs(stream, state);
        }
    };
}

/// The handoff message sent over the wire before each transferred fd.
#[derive(Debug, Serialize)]
struct HandoffMsg {
    r#type: String,
    name: String,
    pid: u32,
    scrollback: Vec<String>,
}

/// Waits for the App to prepare handoffs, then sends each one (metadata line
/// followed by the fd via SCM_RIGHTS) and finally the kill response.
fn send_handoffs(mut stream: UnixStream, state: Arc<IpcState>) {
    // Give the App up to a few seconds to extract the handoffs.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ready = !state
            .handoff_results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if ready || std::time::Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let results = mem::take(
        &mut *state
            .handoff_results
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );

    let mut ok = true;
    for item in results {
        let msg = HandoffMsg {
            r#type: "handoff".to_string(),
            name: item.name,
            pid: item.pid,
            scrollback: item.scrollback,
        };
        let line = match serde_json::to_string(&msg) {
            Ok(l) => l,
            Err(_) => {
                // SAFETY: the fd was dupped for transfer; close it if unsent.
                unsafe { libc::close(item.fd) };
                continue;
            }
        };
        if writeln!(stream, "{line}")
            .and_then(|_| stream.flush())
            .is_err()
            || crate::fds::send_fd(&stream, item.fd).is_err()
        {
            ok = false;
            // SAFETY: the fd was dupped for transfer; close it if unsent.
            unsafe { libc::close(item.fd) };
            break;
        }
    }

    let _ = writeln!(
        stream,
        "{}",
        serde_json::to_string(&KillResponse { ok }).unwrap_or_default()
    );
    let _ = stream.flush();
    state.handoff_done.store(true, Ordering::SeqCst);
}

/// Finds all running fog instances by scanning `$TMPDIR` for `fog-<pid>.sock`.
///
/// Returns a sorted list of `(pid, socket_path)` pairs.
///
/// # Errors
/// Returns an error if the temp directory cannot be read.
pub fn find_instances() -> io::Result<Vec<(u32, PathBuf)>> {
    let mut instances = Vec::new();
    for entry in fs::read_dir(std::env::temp_dir())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(pid_str) = name
            .strip_prefix("fog-")
            .and_then(|s| s.strip_suffix(".sock"))
            && let Ok(pid) = pid_str.parse::<u32>()
        {
            instances.push((pid, entry.path()));
        }
    }
    instances.sort();
    Ok(instances)
}

/// Connects to a fog instance socket and requests its status snapshot.
///
/// # Errors
/// Returns an error if the connection fails (e.g. a stale socket) or the
/// response is malformed.
pub fn query_status(path: &Path) -> io::Result<StatusResponse> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
    stream.write_all(b"{\"type\":\"status\"}\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(line.trim()).map_err(|e| io::Error::other(e.to_string()))
}

/// Connects to a fog instance socket and requests a graceful shutdown.
///
/// # Errors
/// Returns an error if the connection fails.
pub fn send_kill(path: &Path) -> io::Result<()> {
    send_kill_with_reuse(path, &[])
}

/// Connects to a fog instance socket and requests a graceful shutdown,
/// optionally listing services whose `shutdown_cmd` should be skipped.
///
/// # Errors
/// Returns an error if the connection fails.
pub fn send_kill_with_reuse(path: &Path, reuse: &[String]) -> io::Result<()> {
    let mut stream = UnixStream::connect(path)?;
    let mut payload = r#"{"type":"kill""#.to_string();
    if !reuse.is_empty() {
        let names = serde_json::to_string(reuse).unwrap_or_else(|_| "[]".to_string());
        payload.push_str(&format!(r#","reuse":{names}"#));
    }
    payload.push_str("}\n");
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// The handoff message as received by the reclaiming client.
#[derive(Debug, Deserialize)]
struct HandoffMsgReply {
    name: String,
    pid: u32,
    scrollback: Vec<String>,
}

/// The final response to a kill/reclaim request.
#[derive(Debug, Deserialize)]
struct KillReply {
    #[allow(dead_code)]
    ok: bool,
}

/// Reads a single newline-terminated line directly from the stream without
/// buffering, so fd-passing control messages are not swallowed by a reader.
fn read_line_nobuf(mut stream: &UnixStream) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before response",
            ));
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// Requests a graceful shutdown of an instance while reclaiming live services.
///
/// Sends a kill request carrying `reuse` names; the old instance responds with
/// one `handoff` message (plus an SCM_RIGHTS fd) per live service to keep
/// running, then a kill response. Returns the received handoffs.
///
/// # Errors
/// Returns an error if the connection fails or the response is malformed.
pub fn reclaim(path: &Path, reuse: &[String]) -> io::Result<Vec<HandoffItem>> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
    let mut payload = r#"{"type":"kill""#.to_string();
    if !reuse.is_empty() {
        let names = serde_json::to_string(reuse).unwrap_or_else(|_| "[]".to_string());
        payload.push_str(&format!(r#","reuse":{names}"#));
    }
    payload.push_str("}\n");
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;

    let mut handoffs = Vec::new();
    loop {
        let line = read_line_nobuf(&stream)?;
        if let Ok(reply) = serde_json::from_str::<HandoffMsgReply>(line.trim()) {
            let fd = crate::fds::recv_fd(&stream)?;
            handoffs.push(HandoffItem {
                name: reply.name,
                pid: reply.pid,
                scrollback: reply.scrollback,
                fd,
            });
            continue;
        }
        if let Ok(_reply) = serde_json::from_str::<KillReply>(line.trim()) {
            break;
        }
        return Err(io::Error::other("unexpected response from fog instance"));
    }
    Ok(handoffs)
}

/// Finds all running instances belonging to the given project that run the
/// given script, excluding this process.
///
/// Returns a sorted list of `(pid, socket_path)` pairs.
pub fn find_instances_for(project: &str, script: &str) -> Vec<(u32, PathBuf)> {
    let self_pid = std::process::id();
    let mut out = Vec::new();
    let Ok(instances) = find_instances() else {
        return out;
    };
    for (pid, path) in instances {
        if pid == self_pid {
            continue;
        }
        match query_status(&path) {
            Ok(status) => {
                if status.project.as_deref() == Some(project) && status.script == script {
                    out.push((pid, path));
                }
            }
            Err(_) => {
                let _ = fs::remove_file(&path);
            }
        }
    }
    out.sort();
    out
}

/// Waits until the given process has exited, up to `timeout`.
///
/// Returns `true` if the process exited within the timeout.
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !crate::process::is_pid_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    !crate::process::is_pid_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_path_format() {
        let path = socket_path(12345);
        assert_eq!(path, std::env::temp_dir().join("fog-12345.sock"));
    }

    #[test]
    fn test_find_instances_empty() {
        let instances = find_instances().unwrap();
        assert!(!instances.iter().any(|(_, p)| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with("fog-"))
                .unwrap_or(false)
        }));
    }

    #[test]
    fn test_server_and_client_roundtrip() {
        let state = Arc::new(IpcState::new("dev".to_string(), None));
        state.services.lock().unwrap().push(ServiceStatus {
            name: "web".into(),
            running: true,
            health: "healthy".into(),
        });
        state.proxy.lock().unwrap().replace(ProxyStatus {
            running: true,
            port: 8080,
        });

        let path = std::env::temp_dir().join("fog-test-roundtrip.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let resp = query_status(&path).unwrap();
        server.join().unwrap();

        assert_eq!(resp.script, "dev");
        assert_eq!(resp.pid, std::process::id());
        assert_eq!(resp.services.len(), 1);
        assert_eq!(resp.services[0].name, "web");
        assert_eq!(resp.services[0].health, "healthy");
        let proxy = resp.proxy.unwrap();
        assert_eq!(proxy.port, 8080);
        assert!(proxy.running);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_kill_sets_flag() {
        let state = Arc::new(IpcState::new("dev".to_string(), None));
        let path = std::env::temp_dir().join("fog-test-kill.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        send_kill(&path).unwrap();
        server.join().unwrap();

        assert!(state.kill_flag.load(Ordering::SeqCst));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_reclaim_receives_handoffs() {
        // Build a real PTY master fd to transfer.
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

        let state = Arc::new(IpcState::new("dev".to_string(), None));
        state.handoff_results.lock().unwrap().push(HandoffItem {
            name: "db".into(),
            pid: 99_999,
            scrollback: vec!["container db created".into()],
            fd: dup_fd,
        });

        let path = std::env::temp_dir().join("fog-test-reclaim.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let handoffs = reclaim(&path, &["db".to_string()]).unwrap();
        server.join().unwrap();
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].name, "db");
        assert_eq!(handoffs[0].scrollback, vec!["container db created"]);
        assert!(handoffs[0].fd >= 0);
        assert!(state.kill_flag.load(Ordering::SeqCst));
        assert_eq!(
            state.reuse_skip.lock().unwrap().clone(),
            vec!["db".to_string()]
        );
        assert!(state.handoff_done.load(Ordering::SeqCst));

        // SAFETY: the returned fd is owned by the test.
        unsafe { libc::close(handoffs[0].fd) };
        let _ = fs::remove_file(&path);
    }
}
