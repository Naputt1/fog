use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
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

/// Shared state published by the TUI app and served over the IPC socket.
pub struct IpcState {
    /// Current status of all services.
    pub services: Arc<Mutex<Vec<ServiceStatus>>>,
    /// Current proxy status, if a proxy is configured.
    pub proxy: Arc<Mutex<Option<ProxyStatus>>>,
    /// Name of the script currently running.
    pub script: String,
    /// Set to `true` when a kill request is received.
    pub kill_flag: Arc<AtomicBool>,
}

impl IpcState {
    /// Creates a new empty [`IpcState`] for the given script name.
    pub fn new(script: String) -> Self {
        Self {
            services: Arc::new(Mutex::new(Vec::new())),
            proxy: Arc::new(Mutex::new(None)),
            script,
            kill_flag: Arc::new(AtomicBool::new(false)),
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
}

/// A request received over the IPC socket.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Status,
    Kill,
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

    let resp: String = match req {
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
            serde_json::to_string(&StatusResponse {
                pid: std::process::id(),
                script: state.script.clone(),
                services,
                proxy,
            })
            .unwrap_or_default()
        }
        Request::Kill => {
            state.kill_flag.store(true, Ordering::SeqCst);
            serde_json::to_string(&KillResponse { ok: true }).unwrap_or_default()
        }
    };

    let mut writer = stream;
    let _ = writeln!(writer, "{resp}");
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
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(b"{\"type\":\"kill\"}\n")?;
    stream.flush()?;
    Ok(())
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
        let state = Arc::new(IpcState::new("dev".to_string()));
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
        let state = Arc::new(IpcState::new("dev".to_string()));
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
}
