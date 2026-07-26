use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{io, thread};

/// Shared state between the IPC server and the TUI App.
pub struct SharedState {
    pub services: Mutex<Vec<ServiceStatus>>,
    pub proxy: Mutex<Option<ProxyStatus>>,
    pub exit: AtomicBool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub stopped: bool,
    pub process_running: bool,
    pub health_status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProxyStatus {
    pub running: bool,
    pub port: u16,
}

#[derive(serde::Deserialize)]
struct IpcRequest {
    #[serde(rename = "type")]
    type_: String,
}

#[derive(Serialize)]
struct IpcResponse {
    #[serde(rename = "type")]
    type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            services: Mutex::new(Vec::new()),
            proxy: Mutex::new(None),
            exit: AtomicBool::new(false),
        }
    }
}

fn socket_path(pid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/fog-{}.sock", pid))
}

/// Handle that removes the socket file on drop.
pub struct SocketHandle {
    path: PathBuf,
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Binds a Unix domain socket for a given PID.
/// Cleans up any stale socket file first.
pub fn bind(pid: u32) -> io::Result<(SocketHandle, UnixListener)> {
    let path = socket_path(pid);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    Ok((SocketHandle { path }, listener))
}

/// Spawns the IPC server thread that handles incoming client requests.
pub fn spawn_server(listener: UnixListener, state: Arc<SharedState>) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_client(stream, &state),
                Err(_) => break,
            }
        }
    });
}

fn handle_client(stream: UnixStream, state: &SharedState) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }

    let request: IpcRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(_) => return,
    };

    let response = match request.type_.as_str() {
        "status" => status_response(state),
        "kill" => kill_response(state),
        _ => IpcResponse {
            type_: "error".into(),
            data: None,
            error: Some(format!("unknown command: {}", request.type_)),
        },
    };

    let mut writer = stream;
    if let Ok(json) = serde_json::to_string(&response) {
        let _ = writeln!(writer, "{}", json);
    }
}

fn status_response(state: &SharedState) -> IpcResponse {
    let services = state.services.lock().unwrap_or_else(|e| e.into_inner());
    let proxy = state.proxy.lock().unwrap_or_else(|e| e.into_inner());

    let data = serde_json::json!({
        "services": *services,
        "proxy": *proxy,
    });

    IpcResponse {
        type_: "ok".into(),
        data: Some(data),
        error: None,
    }
}

fn kill_response(state: &SharedState) -> IpcResponse {
    state.exit.store(true, Ordering::SeqCst);
    IpcResponse {
        type_: "ok".into(),
        data: Some(serde_json::json!({"status": "shutting_down"})),
        error: None,
    }
}

/// Finds all fog instance sockets in /tmp.
pub fn find_sockets() -> io::Result<Vec<(u32, PathBuf)>> {
    let dir = match std::fs::read_dir("/tmp") {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()),
    };

    let mut sockets: Vec<_> = dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix("fog-")?.strip_suffix(".sock")?;
            let pid = rest.parse::<u32>().ok()?;
            Some((pid, entry.path()))
        })
        .collect();
    sockets.sort_by_key(|(pid, _)| *pid);
    Ok(sockets)
}

/// Sends a JSON request to a fog instance and returns the response string.
pub fn send_request(socket_path: &Path, request_type: &str) -> io::Result<String> {
    let mut stream = UnixStream::connect(socket_path)?;
    let request = serde_json::to_string(&serde_json::json!({ "type": request_type }))
        .map_err(io::Error::other)?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;

    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_string())
}

/// Formats and prints a status response from a fog instance.
pub fn print_status(json_str: &str) {
    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => {
            println!("  error: invalid response from fog instance");
            return;
        }
    };

    if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
        println!("  error: {}", error);
        return;
    }

    let data = match value.get("data") {
        Some(d) => d,
        None => {
            println!("  error: unexpected response format");
            return;
        }
    };

    if let Some(services) = data.get("services").and_then(|s| s.as_array()) {
        if services.is_empty() {
            println!("  (no services)");
        } else {
            println!("  {:<16} {:<10} Health", "Service", "Status");
            println!("  {}", "-".repeat(38));
            for svc in services {
                let name = svc.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                let status = if svc.get("stopped").and_then(|s| s.as_bool()).unwrap_or(true) {
                    "stopped"
                } else {
                    "running"
                };
                let health = svc
                    .get("health_status")
                    .and_then(|h| h.as_str())
                    .unwrap_or("unknown");
                println!("  {:<16} {:<10} {}", name, status, health);
            }
        }
    }

    if let Some(proxy) = data.get("proxy")
        && let Some(port) = proxy.get("port").and_then(|p| p.as_u64())
    {
        let running = proxy
            .get("running")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);
        if running {
            println!("\n  Proxy: running on port {}", port);
        } else {
            println!("\n  Proxy: stopped");
        }
    }
}
