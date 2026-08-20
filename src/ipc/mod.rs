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
mod server;
pub use server::{
    cleanup_socket, find_instances, query_status, send_kill, send_kill_with_reuse,
    send_service_action, spawn_server,
};
pub(crate) use server::{
    client_closed, handle_connection, handle_logs, proxy_running, read_tail_lines,
    sanitize_service_name, send_service_action_with_timeout, service_running, stream_proxy_log,
    stream_service_log, write_log_entry,
};

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
