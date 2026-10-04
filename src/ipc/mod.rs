use std::io;
use std::path::PathBuf;
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
mod server;
pub use server::{
    cleanup_socket, find_instances, query_logs, query_status, query_terminal_snapshot,
    sanitize_service_name, send_kill, send_kill_with_reuse, send_service_action, spawn_server,
};
#[allow(unused_imports)]
pub(crate) use server::{
    client_closed, handle_connection, handle_logs, proxy_running, read_tail_lines,
    send_service_action_with_timeout, service_running, stream_proxy_log, stream_service_log,
    write_log_entry,
};
pub mod transport;
pub use transport::connect_async;

/// Returns the per-user directory holding this user's fog instance endpoints
/// and logs.
///
/// On Unix this is `$TMPDIR/fog-<uid>`, so the socket never lives in the
/// shared, world-writable temp directory where another local user could
/// connect to it. The directory is created and locked down to `0700` by
/// [`ensure_instance_dir`] when a listener binds. On Windows the per-user temp
/// directory already isolates it.
pub fn instance_dir() -> PathBuf {
    std::env::temp_dir().join(instance_dir_name())
}

/// Name of the per-user instance directory under the temp dir.
#[cfg(unix)]
fn instance_dir_name() -> String {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    format!("fog-{uid}")
}

/// Name of the per-user instance directory under the temp dir. Windows' temp
/// directory is already per-user, so no uid is needed.
#[cfg(windows)]
fn instance_dir_name() -> String {
    "fog".to_string()
}

/// Returns the socket path for a given PID: `$TMPDIR/fog-<uid>/fog-<pid>.sock`.
pub fn socket_path(pid: u32) -> PathBuf {
    instance_dir().join(format!("fog-{pid}.sock"))
}

/// Returns the directory holding an instance's captured logs:
/// `$TMPDIR/fog-<uid>/fog-<pid>.logs/`. Every run (interactive or detached)
/// tees each service's raw PTY output into `<service>.log` here; detached runs
/// also write their own diagnostics to `daemon.log`.
pub fn instance_log_dir(pid: u32) -> PathBuf {
    instance_dir().join(format!("fog-{pid}.logs"))
}

/// Returns the socket path for the current process.
pub fn current_socket_path() -> PathBuf {
    socket_path(std::process::id())
}

/// Creates the per-user instance directory (if needed) and restricts it to the
/// owner (`0700` on Unix). Callers that bind an instance socket should invoke
/// this first so the endpoint is unreachable by other local users.
///
/// # Errors
/// Returns an error if the directory cannot be created or secured.
pub fn ensure_instance_dir() -> io::Result<PathBuf> {
    let dir = instance_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dir)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&dir, perms)?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::LogEntry;
    use std::collections::VecDeque;
    use std::fs::{self};
    use std::io::{BufReader, Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_socket_path_format() {
        let path = socket_path(12345);
        assert_eq!(path, instance_dir().join("fog-12345.sock"));
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
        let mut state = IpcState::new("dev".to_string(), None, None, false);
        state.services.lock().unwrap().push(ServiceStatus {
            name: "web".into(),
            running: true,
            health: "healthy".into(),
            endpoints: Vec::new(),
        });
        state.proxy.lock().unwrap().replace(ProxyStatus {
            running: true,
            port: 8080,
        });
        state.config_dir = Some("/srv/example".to_string());
        let state = Arc::new(state);

        let path = instance_dir().join("fog-test-roundtrip.sock");
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
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
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        let path = instance_dir().join("fog-test-kill.sock");
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
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
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        let path = instance_dir().join("fog-test-terminate.sock");
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let n = terminate_instances(&[(999_999_u32, path.clone())]);
        server.join().unwrap();

        assert!(state.kill_flag.load(Ordering::SeqCst));
        assert_eq!(n, 1);

        let _ = fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[cfg(unix)]
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

        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        state.handoff_results.lock().unwrap().push(HandoffItem {
            name: "db".into(),
            pid: 99_999,
            fd: dup_fd,
        });
        state.handoff_prepared.store(true, Ordering::SeqCst);

        let path = instance_dir().join("fog-test-reclaim.sock");
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
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

    #[cfg(unix)]
    #[cfg(unix)]
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

        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        state.handoff_results.lock().unwrap().push(HandoffItem {
            name: "db".into(),
            pid: 99_999,
            fd: dup_fd,
        });
        state.handoff_prepared.store(true, Ordering::SeqCst);

        let path = instance_dir().join(format!(
            "fog-test-single-winner-{}.sock",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();

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

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn test_plain_kill_does_not_consume_handoffs() {
        // A plain kill arriving while a handoff is pending must not take the
        // prepared results away from the reclaiming client.
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
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

        let path = instance_dir().join(format!("fog-test-plain-kill-{}.sock", std::process::id()));
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();
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
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        let path = unique("svcaction.sock");
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
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
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        let path = unique("svcaction-timeout.sock");
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
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
        instance_dir().join(format!("fog-{name}-{}", std::process::id()))
    }

    #[test]
    fn test_instance_log_dir_naming() {
        assert_eq!(instance_log_dir(1234), instance_dir().join("fog-1234.logs"));
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

        let listener = super::transport::Listener::bind(&sock).unwrap();
        let dir_clone = dir.clone();
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            stream_service_log(&mut stream, &server_state, &dir_clone, "web", 2, false);
        });

        let mut client = super::transport::connect(&sock).unwrap();
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

        let listener = super::transport::Listener::bind(&sock).unwrap();
        let dir_clone = dir.clone();
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            stream_service_log(&mut stream, &server_state, &dir_clone, "web", 5, false);
        });

        let mut client = super::transport::connect(&sock).unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        server.join().unwrap();

        assert!(out.contains("[fog] no captured log"));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&sock);
    }

    #[test]
    fn test_stream_proxy_log_tail() {
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
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
        let listener = super::transport::Listener::bind(&sock).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            stream_proxy_log(&mut stream, &server_state, 10, false);
        });

        let mut client = super::transport::connect(&sock).unwrap();
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
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        let sock = unique("logsreq.sock");
        let _ = fs::remove_file(&sock);
        let listener = super::transport::Listener::bind(&sock).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let mut client = super::transport::connect(&sock).unwrap();
        client
            .write_all(b"{\"type\":\"logs\",\"service\":\"nonexistent\",\"follow\":false}\n")
            .unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        server.join().unwrap();

        assert!(out.contains("[fog] no captured log"));
        let _ = fs::remove_file(&sock);
    }

    #[test]
    fn test_query_logs_proxy_roundtrip() {
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
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
        }
        *state.proxy_logs.lock().unwrap() = Some(q);

        let sock = unique("querylogs.sock");
        let _ = fs::remove_file(&sock);
        let listener = super::transport::Listener::bind(&sock).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let lines = super::server::query_logs(&sock, "proxy", 10).unwrap();
        server.join().unwrap();

        assert!(lines.iter().any(|l| l.contains("/api/bookings")));
        let _ = fs::remove_file(&sock);
    }

    #[test]
    fn test_query_logs_missing_service_roundtrip() {
        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        let sock = unique("querylogs-miss.sock");
        let _ = fs::remove_file(&sock);
        let listener = super::transport::Listener::bind(&sock).unwrap();
        let server_state = state.clone();
        let server = thread::spawn(move || {
            let stream = listener.accept().unwrap();
            handle_connection(stream, server_state);
        });

        let lines = super::server::query_logs(&sock, "nonexistent", 10).unwrap();
        server.join().unwrap();

        assert!(lines.iter().any(|l| l.contains("[fog] no captured log")));
        let _ = fs::remove_file(&sock);
    }

    #[cfg(unix)]
    #[test]
    fn test_socket_and_dir_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = ensure_instance_dir().unwrap();
        let path = dir.join(format!("fog-perm-{}.sock", std::process::id()));
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();

        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "socket file mode: {file_mode:o}");
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "instance dir mode: {dir_mode:o}");

        drop(listener);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_find_instances_discovers_bound_socket() {
        // Use a PID that will not collide with sockets other tests create.
        let pid = 4_000_000_000_u32;
        let dir = instance_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("fog-{pid}.sock"));
        let _ = fs::remove_file(&path);
        let listener = super::transport::Listener::bind(&path).unwrap();

        let found = find_instances().unwrap();
        assert!(
            found.iter().any(|(p, s)| *p == pid && s == &path),
            "bound socket must be discovered, got {found:?}"
        );

        drop(listener);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_spawn_server_stale_and_live_handling() {
        let path = current_socket_path();
        let _ = fs::remove_file(&path);

        // A bound-then-dropped listener leaves a stale socket file behind.
        {
            let listener = super::transport::Listener::bind(&path).unwrap();
            drop(listener);
        }
        assert!(path.exists(), "stale socket file should remain after drop");

        let state = Arc::new(IpcState::new("dev".to_string(), None, None, false));
        // A stale socket must be replaced, not refused.
        spawn_server(state.clone()).unwrap();
        assert!(
            query_status(&path).is_ok(),
            "replaced endpoint must answer status"
        );

        // A second server must refuse to steal the live endpoint.
        let err = spawn_server(state).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        cleanup_socket();
    }
}
