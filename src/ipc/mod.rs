use crate::proxy::LogEntry;
#[allow(unused_imports)]
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
#[allow(unused_imports)]
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const READ_TIMEOUT_SECS: u64 = 5;
/// How long the IPC thread waits for the App to execute a per-service
/// `start`/`stop`/`restart` request before answering with a timeout.
const CONTROL_TIMEOUT_SECS: u64 = 30;
/// Maximum accepted length for a single IPC request line.
const MAX_IPC_LINE_LEN: usize = 8192;
/// Upper bound for a `logs` request's `tail`, so a single request cannot
/// request an unbounded backfill.
const MAX_LOG_TAIL: usize = 10_000;
/// Poll interval while following a log file or the proxy queue.
const LOG_FOLLOW_POLL_MS: u64 = 150;
/// How long a follow loop keeps waiting for output after the underlying
/// service has stopped before ending the stream.
const LOG_FOLLOW_IDLE: Duration = Duration::from_secs(15);

mod types;
pub use types::*;
mod handoff;
pub use handoff::*;

/// Returns the socket path for a given PID: `$TMPDIR/fog-<pid>.sock`.
pub fn socket_path(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("fog-{pid}.sock"))
}

/// Returns the directory holding an instance's captured logs:
/// `$TMPDIR/fog-<pid>.logs/`. Every run (interactive or detached) tees each
/// service's raw PTY output into `<service>.log` here; detached runs also
/// write their own diagnostics to `daemon.log`.
pub fn instance_log_dir(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("fog-{pid}.logs"))
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
fn handle_connection(mut stream: UnixStream, state: Arc<IpcState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)));
    let mut reader = match stream.try_clone() {
        Ok(r) => BufReader::new(r),
        Err(_) => return,
    };
    // Cap the request line so a local client cannot inflate memory; a
    // malformed/oversized request is simply ignored.
    let mut line_buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while line_buf.len() < MAX_IPC_LINE_LEN {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line_buf.push(byte[0]);
            }
            Err(_) => return,
        }
    }
    drop(reader);
    let line = String::from_utf8_lossy(&line_buf);

    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(_) => return,
    };

    match req {
        Request::Status => {
            let services = state.services.lock().expect("mutex poisoned").clone();
            let proxy = state.proxy.lock().expect("mutex poisoned").clone();
            let ports = state.ports.lock().expect("mutex poisoned").clone();
            let native_routes = state.native_routes.lock().expect("mutex poisoned").clone();
            let resp = serde_json::to_string(&StatusResponse {
                pid: std::process::id(),
                script: state.script.clone(),
                services,
                proxy,
                project: state.project.clone(),
                branch: state.branch.clone(),
                config_dir: state.config_dir.clone(),
                started_at: state.started_at,
                ports,
                native_routes,
            })
            .unwrap_or_default();
            let mut writer = stream;
            let _ = writeln!(writer, "{resp}");
        }
        Request::Kill { reuse } => {
            if reuse.is_empty() {
                // Plain kill: acknowledge immediately and never touch handoff
                // state, so it cannot steal a handoff in flight.
                state.kill_flag.store(true, Ordering::SeqCst);
                let resp = serde_json::to_string(&KillResponse {
                    ok: true,
                    reason: String::new(),
                })
                .unwrap_or_default();
                let _ = writeln!(stream, "{resp}");
                return;
            }
            // A reclaim claims the handoff right exactly once; concurrent
            // reclaimers are refused instead of racing for the same live
            // processes.
            if state.handoff_claimed.swap(true, Ordering::SeqCst) {
                let resp = serde_json::to_string(&KillResponse {
                    ok: false,
                    reason: "instance is already being replaced".to_string(),
                })
                .unwrap_or_default();
                let _ = writeln!(stream, "{resp}");
                return;
            }
            // Publish the handoff request BEFORE raising the kill flag: the app
            // loop acts as soon as it sees `kill_flag`, so the request must
            // already be visible for it to prepare and send the handoffs.
            *state.reuse_skip.lock().expect("mutex poisoned") = reuse.clone();
            *state.handoff_req.lock().expect("mutex poisoned") = Some(reuse);
            state.kill_flag.store(true, Ordering::SeqCst);
            handoff::send_handoffs(stream, state);
        }
        Request::Logs {
            service,
            tail,
            follow,
        } => handle_logs(stream, state, &service, tail, follow),
        Request::ServiceAction { name, action } => {
            // Reset `control_done` BEFORE publishing the request so the App
            // can never complete this request before its completion signal is
            // cleared (the App is the only writer of the completion signal).
            state.control_done.store(false, Ordering::SeqCst);
            *state.control_req.lock().expect("mutex poisoned") =
                Some(ServiceActionRequest { name, action });
            // Wait for the App to execute the action, mirroring the wait loop
            // in `send_handoffs`. An empty result after the wait means the App
            // never took the request (or never finished it).
            let deadline = std::time::Instant::now() + Duration::from_secs(CONTROL_TIMEOUT_SECS);
            loop {
                if state.control_done.load(Ordering::SeqCst)
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let resp = state
                .control_result
                .lock()
                .expect("mutex poisoned")
                .take()
                .unwrap_or(ControlResponse {
                    ok: false,
                    reason: "timed out".to_string(),
                });
            let _ = writeln!(
                stream,
                "{}",
                serde_json::to_string(&resp).unwrap_or_default()
            );
        }
    };
}

/// Handles a `logs` request: streams a service's captured log (or the proxy
/// request log when `service == "proxy"`).
///
/// The response is newline-delimited raw text (ANSI intact): the last `tail`
/// lines are emitted first, then — with `follow` — new output until the
/// client disconnects. The `[fog] ...` prefix marks control messages the
/// page uses to stop reconnecting.
fn handle_logs(
    mut stream: UnixStream,
    state: Arc<IpcState>,
    service: &str,
    tail: usize,
    follow: bool,
) {
    let tail = tail.clamp(1, MAX_LOG_TAIL);
    if service == "proxy" {
        stream_proxy_log(&mut stream, &state, tail, follow);
        return;
    }
    let dir = instance_log_dir(std::process::id());
    stream_service_log(&mut stream, &state, &dir, service, tail, follow);
}

/// Whether a service is currently running, per the instance's live status.
/// Unknown services (e.g. shell tabs, `daemon`) count as running.
fn service_running(state: &IpcState, service: &str) -> bool {
    state
        .services
        .lock()
        .expect("mutex poisoned")
        .iter()
        .find(|s| s.name == service)
        .map(|s| s.running)
        .unwrap_or(true)
}

/// Whether the proxy is currently running, per the instance's live status.
fn proxy_running(state: &IpcState) -> bool {
    state
        .proxy
        .lock()
        .expect("mutex poisoned")
        .as_ref()
        .map(|p| p.running)
        .unwrap_or(true)
}

/// Streams a service's captured log file (`<sanitized>.log` inside `dir`).
/// See [`handle_logs`] for the wire format.
fn stream_service_log(
    stream: &mut UnixStream,
    state: &IpcState,
    dir: &Path,
    service: &str,
    tail: usize,
    follow: bool,
) {
    let safe = sanitize_service_name(service);
    let path = dir.join(format!("{safe}.log"));
    let Ok(file) = fs::File::open(&path) else {
        let _ = writeln!(stream, "[fog] no captured log for service '{service}'");
        return;
    };

    // Tail: read the last `tail` lines, and the byte offset just after them
    // so the follow loop resumes without a gap or duplication.
    let (lines, offset_after) = match file.try_clone() {
        Ok(mut f) => read_tail_lines(&mut f, tail),
        Err(_) => (Vec::new(), 0),
    };
    for line in &lines {
        // PTY output uses CRLF; strip the trailing `\r` so the page shows the
        // whole line (its JS treats `\r` as a line-overwrite marker).
        let line = line.trim_end_matches('\r');
        if writeln!(stream, "{line}").is_err() {
            return;
        }
    }
    if !follow {
        let _ = stream.flush();
        return;
    }

    // Follow: a BufReader positioned after the tail yields new bytes as the
    // file grows. Complete lines are emitted immediately; a partial line is
    // held until its terminating `\n` arrives.
    let mut reader = BufReader::new(file);
    if reader.seek(SeekFrom::Start(offset_after)).is_err() {
        return;
    }
    let mut pending: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let mut last_growth = std::time::Instant::now();
    loop {
        if client_closed(stream) {
            return;
        }
        // A stopped service that has gone quiet ends the stream, like
        // `docker logs -f` on a stopped container.
        if last_growth.elapsed() > LOG_FOLLOW_IDLE && !service_running(state, service) {
            break;
        }
        match reader.read(&mut chunk) {
            Ok(0) => {}
            Ok(n) => {
                last_growth = std::time::Instant::now();
                pending.extend_from_slice(&chunk[..n]);
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let text = String::from_utf8_lossy(&pending[..pos]);
                    let text = text.trim_end_matches('\r');
                    if writeln!(stream, "{text}").is_err() {
                        return;
                    }
                    pending.drain(..=pos);
                }
            }
            Err(_) => return,
        }
        thread::sleep(Duration::from_millis(LOG_FOLLOW_POLL_MS));
    }
}

/// Streams the proxy's live request-log queue. See [`handle_logs`] for the
/// wire format; each entry is rendered the way the TUI's proxy tab shows it.
fn stream_proxy_log(stream: &mut UnixStream, state: &IpcState, tail: usize, follow: bool) {
    let handle = state.proxy_logs.lock().expect("mutex poisoned").clone();
    let Some(queue) = handle else {
        let _ = writeln!(stream, "[fog] no proxy configured");
        return;
    };

    let (snapshot, total) = {
        let q = queue.lock().expect("mutex poisoned");
        let total = q.len();
        let skip = total.saturating_sub(tail);
        (q.iter().skip(skip).cloned().collect::<Vec<_>>(), total)
    };
    for entry in &snapshot {
        if write_log_entry(stream, entry).is_err() {
            return;
        }
    }
    if !follow {
        let _ = stream.flush();
        return;
    }

    let mut sent = total;
    let mut last_growth = std::time::Instant::now();
    loop {
        if client_closed(stream) {
            return;
        }
        // A stopped proxy that has gone quiet ends the stream.
        if last_growth.elapsed() > LOG_FOLLOW_IDLE && !proxy_running(state) {
            break;
        }
        let q = queue.lock().expect("mutex poisoned");
        let total = q.len();
        if total > sent {
            last_growth = std::time::Instant::now();
            for entry in q.iter().skip(sent) {
                if write_log_entry(stream, entry).is_err() {
                    return;
                }
            }
            sent = total;
        }
        drop(q);
        thread::sleep(Duration::from_millis(LOG_FOLLOW_POLL_MS));
    }
}

/// Formats a proxy log entry as a line matching the TUI proxy tab's layout.
fn write_log_entry(stream: &mut UnixStream, entry: &LogEntry) -> io::Result<()> {
    let method = if entry.ws {
        "WS".to_string()
    } else {
        entry.method.clone()
    };
    let status = if entry.status == 0 {
        String::new()
    } else {
        entry.status.to_string()
    };
    let latency = if entry.status == 0 {
        String::new()
    } else {
        format!("{}ms", entry.latency_ms)
    };
    writeln!(
        stream,
        "{method:<6} {:<35} {:<5} {:<8} {}",
        entry.path, status, latency, entry.upstream
    )
}

/// Whether the client closed the connection. Attempts a non-blocking read
/// with a short timeout: `Ok(0)` (EOF) or a hard error means closed.
fn client_closed(stream: &mut UnixStream) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(LOG_FOLLOW_POLL_MS)));
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            false
        }
        Err(_) => true,
    }
}

/// Reads the last `n` newline-terminated lines of `file`, returning them in
/// order plus the absolute byte offset just *after* them (so a follow reader
/// resumes with only new output — no duplication). The read window is capped
/// at 1 MiB, so an arbitrarily large log still tails quickly.
fn read_tail_lines(file: &mut File, n: usize) -> (Vec<String>, u64) {
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let window: u64 = 1 << 20;
    let start = len.saturating_sub(window);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), len);
    }
    let mut data = Vec::new();
    if file.read_to_end(&mut data).is_err() {
        return (Vec::new(), len);
    }

    // Split into lines, recording each line's absolute start and end offset.
    let mut lines: Vec<(String, u64, u64)> = Vec::new();
    let mut seg_start: u64 = start;
    for (i, b) in data.iter().enumerate() {
        if *b == b'\n' {
            let text = String::from_utf8_lossy(&data[(seg_start - start) as usize..i]).into_owned();
            let seg_end = seg_start + text.len() as u64 + 1;
            lines.push((text, seg_start, seg_end));
            seg_start = start + i as u64 + 1;
        }
    }

    // Drop the leading partial line when the window started mid-line (the
    // only complete line starting exactly at `start` when `start > 0`).
    if start > 0 && lines.first().is_some_and(|(_, off, _)| *off == start) {
        lines.remove(0);
    }
    let take = lines.len().saturating_sub(lines.len().saturating_sub(n));
    let first_kept = lines.len().saturating_sub(take);
    if first_kept >= lines.len() {
        return (Vec::new(), len);
    }
    let offset_after = lines[lines.len() - 1].2;
    let kept = lines
        .into_iter()
        .skip(first_kept)
        .map(|(text, _, _)| text)
        .collect::<Vec<_>>();
    (kept, offset_after)
}

/// Sanitizes a service name into a safe log filename stem: `/` (and anything
/// else outside an allowlist) becomes `_`, so a hostile name can never escape
/// the log directory or reference another file.
fn sanitize_service_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
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

/// Connects to a fog instance socket and asks it to `start`, `stop`, or
/// `restart` a single service, returning the instance's verdict.
///
/// The read timeout covers the App's whole `CONTROL_TIMEOUT_SECS` execution
/// window (the server answers "timed out" itself when the App never takes the
/// request), so a slow-but-legitimate action is not cut short.
///
/// # Errors
/// Returns an error if the connection fails, the response is malformed, or
/// the instance does not answer within its control window.
pub fn send_service_action(
    path: &Path,
    name: &str,
    action: ServiceAction,
) -> io::Result<ControlResponse> {
    send_service_action_with_timeout(
        path,
        name,
        action,
        Duration::from_secs(CONTROL_TIMEOUT_SECS + 5),
    )
}

/// Like [`send_service_action`], but with an explicit client read timeout
/// (used by tests to fail fast on the timeout path).
fn send_service_action_with_timeout(
    path: &Path,
    name: &str,
    action: ServiceAction,
    read_timeout: Duration,
) -> io::Result<ControlResponse> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(read_timeout))?;
    // Serialize both fields through serde_json so a hostile name or a
    // non-default action can never break out of the JSON line.
    let name = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string());
    let action = serde_json::to_string(&action).unwrap_or_else(|_| "\"start\"".to_string());
    let line = format!(r#"{{"type":"service_action","name":{name},"action":{action}}}"#);
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(line.trim()).map_err(|e| io::Error::other(e.to_string()))
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
    fn test_find_instances_sorted() {
        // Not asserting the temp dir is empty: other running `fog` instances
        // may legitimately have sockets there. Just verify the scan returns a
        // sorted list (and does not choke on unrelated files).
        let instances = find_instances().unwrap();
        for w in instances.windows(2) {
            assert!(w[0].0 <= w[1].0, "instances must be sorted by pid");
        }
    }

    #[test]
    fn test_server_and_client_roundtrip() {
        // Build the state before wrapping it in an Arc so the plain `config_dir`
        // field (which is set once, before the server shares the state) can be
        // populated, mirroring `run_script`.
        let mut state = IpcState::new("dev".to_string(), None, None);
        state.services.lock().unwrap().push(ServiceStatus {
            name: "web".into(),
            running: true,
            health: "healthy".into(),
        });
        state.proxy.lock().unwrap().replace(ProxyStatus {
            running: true,
            port: 8080,
        });
        state.config_dir = Some("/srv/example".to_string());
        let state = Arc::new(state);

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
        assert!(resp.started_at > 0);
        assert_eq!(resp.config_dir.as_deref(), Some("/srv/example"));
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
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
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
    fn test_terminate_instances_sends_kill_request() {
        // A live socket server must receive the kill request even though the
        // instance PID is long gone; the nonexistent PID also exercises the
        // signal fallback path without signalling anything real.
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        let path = std::env::temp_dir().join("fog-test-terminate.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let n = terminate_instances(&[(999_999_u32, path.clone())]);
        server.join().unwrap();

        assert!(state.kill_flag.load(Ordering::SeqCst));
        assert_eq!(n, 1);

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

        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        state.handoff_results.lock().unwrap().push(HandoffItem {
            name: "db".into(),
            pid: 99_999,
            fd: dup_fd,
        });
        state.handoff_prepared.store(true, Ordering::SeqCst);

        let path = std::env::temp_dir().join("fog-test-reclaim.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let outcome = reclaim(&path, &["db".to_string()]);
        server.join().unwrap();
        assert!(
            outcome.error.is_none(),
            "reclaim error: {:?}",
            outcome.error
        );
        assert!(!outcome.incomplete);
        assert_eq!(outcome.handoffs.len(), 1);
        assert_eq!(outcome.handoffs[0].name, "db");
        assert!(outcome.handoffs[0].fd >= 0);
        assert!(state.kill_flag.load(Ordering::SeqCst));
        assert_eq!(
            state.reuse_skip.lock().unwrap().clone(),
            vec!["db".to_string()]
        );
        assert!(state.handoff_done.load(Ordering::SeqCst));

        // SAFETY: the returned fd is owned by the test.
        unsafe { libc::close(outcome.handoffs[0].fd) };
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_reclaim_single_winner() {
        // Two concurrent reclaims: exactly one gets the handoff, the other is
        // refused with ok:false and must not consume the handoff results.
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

        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        state.handoff_results.lock().unwrap().push(HandoffItem {
            name: "db".into(),
            pid: 99_999,
            fd: dup_fd,
        });
        state.handoff_prepared.store(true, Ordering::SeqCst);

        let path = std::env::temp_dir().join(format!(
            "fog-test-single-winner-{}.sock",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let server_state = state.clone();
        let server = thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let st = server_state.clone();
                thread::spawn(move || handle_connection(stream, st));
            }
        });

        let outcome_a = reclaim(&path, &["db".to_string()]);
        let outcome_b = reclaim(&path, &["db".to_string()]);

        drop(server);
        let _ = fs::remove_file(&path);

        for outcome in [&outcome_a, &outcome_b] {
            for item in &outcome.handoffs {
                // SAFETY: the returned fd is owned by the test.
                unsafe { libc::close(item.fd) };
            }
        }
        let winners = [&outcome_a, &outcome_b]
            .iter()
            .filter(|o| o.error.is_none() && o.handoffs.len() == 1)
            .count();
        let refusals = [&outcome_a, &outcome_b]
            .iter()
            .filter(|o| {
                o.error
                    .as_deref()
                    .is_some_and(|e| e.contains("already being replaced"))
            })
            .count();
        assert_eq!(winners, 1, "exactly one reclaim must win");
        assert_eq!(refusals, 1, "the other reclaim must be refused");
    }

    #[test]
    fn test_plain_kill_does_not_consume_handoffs() {
        // A plain kill arriving while a handoff is pending must not take the
        // prepared results away from the reclaiming client.
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
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
        state.handoff_results.lock().unwrap().push(HandoffItem {
            name: "db".into(),
            pid: 99_998,
            fd: dup_fd,
        });
        state.handoff_prepared.store(true, Ordering::SeqCst);

        let path =
            std::env::temp_dir().join(format!("fog-test-plain-kill-{}.sock", std::process::id()));
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let st = server_state.clone();
                thread::spawn(move || handle_connection(stream, st));
            }
        });

        // Plain kill first, then the reclaiming client.
        let kill_res = send_kill(&path);
        assert!(kill_res.is_ok());
        let outcome = reclaim(&path, &["db".to_string()]);

        drop(server);
        let _ = fs::remove_file(&path);

        assert!(
            outcome.error.is_none(),
            "reclaim error: {:?}",
            outcome.error
        );
        assert_eq!(
            outcome.handoffs.len(),
            1,
            "plain kill must not steal handoffs"
        );
        assert_eq!(outcome.handoffs[0].name, "db");
        // SAFETY: the returned fd is owned by the test.
        unsafe { libc::close(outcome.handoffs[0].fd) };
    }

    #[test]
    fn test_service_action_roundtrip() {
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        let path = unique("svcaction.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let client_path = path.clone();
        let client = thread::spawn(move || {
            send_service_action(&client_path, "web", ServiceAction::Restart).unwrap()
        });

        // The test main thread acts as the App loop: wait for the request to
        // be published, then answer it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let req = loop {
            if let Some(req) = state.control_req.lock().expect("mutex poisoned").clone() {
                break req;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "control request was never published"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(req.name, "web");
        assert_eq!(req.action, ServiceAction::Restart);
        *state.control_result.lock().expect("mutex poisoned") = Some(ControlResponse {
            ok: true,
            reason: String::new(),
        });
        state.control_done.store(true, Ordering::SeqCst);

        let resp = client.join().unwrap();
        server.join().unwrap();
        assert!(resp.ok);
        assert!(resp.reason.is_empty());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_service_action_timeout() {
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        let path = unique("svcaction-timeout.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        // The App loop never sets `control_done`: the server would answer
        // "timed out" only after the full CONTROL_TIMEOUT_SECS. Give the
        // client a short read timeout so the test fails fast instead of
        // blocking the whole window when the wait misbehaves.
        let res = send_service_action_with_timeout(
            &path,
            "web",
            ServiceAction::Stop,
            Duration::from_millis(300),
        );
        assert!(
            res.is_err(),
            "client must give up when the App never answers, got: {res:?}"
        );
        // The request must still have been published for the App loop.
        assert!(
            state.control_req.lock().expect("mutex poisoned").is_some(),
            "control request must be published before the wait"
        );

        drop(server);
        let _ = fs::remove_file(&path);
    }

    fn unique(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fog-{name}-{}", std::process::id()))
    }

    #[test]
    fn test_instance_log_dir_naming() {
        assert_eq!(
            instance_log_dir(1234),
            std::env::temp_dir().join("fog-1234.logs")
        );
    }

    #[test]
    fn test_sanitize_service_name() {
        assert_eq!(sanitize_service_name("web"), "web");
        assert_eq!(sanitize_service_name("my service"), "my service");
        assert_eq!(sanitize_service_name("a/b"), "a_b");
        assert_eq!(sanitize_service_name("../../etc"), ".._.._etc");
        assert_eq!(sanitize_service_name("a;rm -rf"), "a_rm -rf");
        assert_eq!(sanitize_service_name(""), "");
    }

    #[test]
    fn test_read_tail_lines() {
        let dir = unique("readtail");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.log");
        fs::write(&path, "l1\nl2\nl3\nl4\nl5\n").unwrap();

        let mut f = fs::File::open(&path).unwrap();
        let (lines, offset_after) = read_tail_lines(&mut f, 3);
        assert_eq!(lines, vec!["l3", "l4", "l5"]);
        // Offset points past the last tail line, so a follow reader gets only
        // new output — no duplication.
        assert_eq!(offset_after, 15);
        let mut reader = BufReader::new(fs::File::open(&path).unwrap());
        reader.seek(SeekFrom::Start(offset_after)).unwrap();
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "");

        // n larger than the file returns everything.
        let mut f = fs::File::open(&path).unwrap();
        let (lines, _) = read_tail_lines(&mut f, 10);
        assert_eq!(lines, vec!["l1", "l2", "l3", "l4", "l5"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_stream_service_log_tail() {
        let dir = unique("svclog");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("web.log"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let sock = unique("svclog.sock");
        let _ = fs::remove_file(&sock);

        let listener = UnixListener::bind(&sock).unwrap();
        let dir_clone = dir.clone();
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream_service_log(&mut stream, &server_state, &dir_clone, "web", 2, false);
        });

        let mut client = UnixStream::connect(&sock).unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        server.join().unwrap();

        assert_eq!(out, "l4\nl5\n");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&sock);
    }

    #[test]
    fn test_stream_service_log_missing() {
        let dir = unique("misslog");
        fs::create_dir_all(&dir).unwrap();
        let sock = unique("misslog.sock");
        let _ = fs::remove_file(&sock);

        let listener = UnixListener::bind(&sock).unwrap();
        let dir_clone = dir.clone();
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream_service_log(&mut stream, &server_state, &dir_clone, "web", 5, false);
        });

        let mut client = UnixStream::connect(&sock).unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        server.join().unwrap();

        assert!(out.contains("[fog] no captured log"));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&sock);
    }

    #[test]
    fn test_stream_proxy_log_tail() {
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        let q = Arc::new(Mutex::new(VecDeque::new()));
        {
            let mut lk = q.lock().unwrap();
            lk.push_back(LogEntry {
                method: "GET".into(),
                path: "/api/bookings".into(),
                upstream: "127.0.0.1:8000".into(),
                status: 200,
                latency_ms: 3,
                ws: false,
            });
            lk.push_back(LogEntry {
                method: "WS".into(),
                path: "/ws".into(),
                upstream: "127.0.0.1:8000".into(),
                status: 101,
                latency_ms: 1,
                ws: true,
            });
        }
        *state.proxy_logs.lock().unwrap() = Some(q);

        let sock = unique("proxylog.sock");
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream_proxy_log(&mut stream, &server_state, 10, false);
        });

        let mut client = UnixStream::connect(&sock).unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        server.join().unwrap();

        assert!(out.contains("GET"));
        assert!(out.contains("/api/bookings"));
        assert!(out.contains("200"));
        assert!(out.contains("3ms"));
        assert!(out.contains("WS"));
        assert!(out.contains("/ws"));

        let _ = fs::remove_file(&sock);
    }

    #[test]
    fn test_logs_request_missing_file_roundtrip() {
        let state = Arc::new(IpcState::new("dev".to_string(), None, None));
        let sock = unique("logsreq.sock");
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let mut client = UnixStream::connect(&sock).unwrap();
        client
            .write_all(b"{\"type\":\"logs\",\"service\":\"nonexistent\",\"follow\":false}\n")
            .unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        server.join().unwrap();

        assert!(out.contains("[fog] no captured log"));
        let _ = fs::remove_file(&sock);
    }
}
