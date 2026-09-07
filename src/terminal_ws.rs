//! WebSocket gateway that bridges a browser terminal (xterm.js) to a live PTY.
//!
//! A `GET /ws/terminal` request carrying a WebSocket upgrade is answered with
//! `101 Switching Protocols`. Once the upgrade completes, a fresh `bash`
//! (from `$SHELL`) session is spawned in a [`portable_pty`] pseudo-terminal and
//! raw ANSI bytes are streamed bidirectionally over the socket:
//!
//! * client -> server: raw input bytes are written to the PTY master. A text
//!   frame that parses as `{"type":"resize","cols":N,"rows":N}` resizes the
//!   PTY instead of being written as input.
//! * server -> client: PTY output is forwarded as raw binary frames.
//!
//! A `ping` frame is sent every 30s to keep the connection alive, and the
//! session is torn down after the configured idle timeout (default 900s).
//!
//! Hardening (configurable via [`crate::config::TerminalConfig`]):
//! * a bounded 64-frame output queue that drops the oldest frame when full,
//!   applying backpressure to a slow consumer without unbounded memory growth;
//! * a per-IP session cap (default 8) answered with `429`;
//! * a per-frame size cap (default 64 KiB) closing the socket with code `1009`;
//! * an optional `auth_token` query parameter rejecting requests with `401`.
//!
//! The full wire protocol is documented in `docs/terminal-protocol.md`.

use futures_util::{SinkExt, StreamExt};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::upgrade::Upgraded;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::config::TerminalConfig;

/// How often a keep-alive `ping` frame is sent to the client.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Default PTY dimensions used until the client sends its first resize.
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
/// Read buffer size for the PTY output pump.
const PTY_BUF: usize = 8192;
/// Minimum and maximum PTY dimensions accepted from a client resize.
const MIN_DIM: u32 = 1;
const MAX_DIM: u32 = 2000;
/// Bounded output queue capacity (number of buffered frames).
const OUTPUT_QUEUE_CAP: usize = 64;
/// Max size of a buffered output frame (drop-oldest threshold stays on count).
const INPUT_CHANNEL_CAP: usize = 256;

/// Response body type, matching the rest of the proxy service.
type BoxBody = UnsyncBoxBody<Bytes, hyper::Error>;

fn body_full(bytes: Bytes) -> BoxBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// Shared per-IP session accounting used to enforce
/// [`TerminalConfig::max_sessions_per_ip`].
#[derive(Default)]
pub struct SessionsRegistry {
    active: Mutex<HashMap<String, usize>>,
}

impl SessionsRegistry {
    /// Increments the active session count for `ip`. Returns `Ok` when the new
    /// count is within the configured cap, `Err` (with the current count) when
    /// the cap would be exceeded.
    fn acquire(&self, ip: &str, max_per_ip: usize) -> Result<(), usize> {
        let mut map = self.active.lock().expect("session registry poisoned");
        let count = map.entry(ip.to_string()).or_insert(0);
        if *count >= max_per_ip {
            return Err(*count);
        }
        *count += 1;
        Ok(())
    }

    fn release(&self, ip: &str) {
        let mut map = self.active.lock().expect("session registry poisoned");
        if let Some(count) = map.get_mut(ip) {
            if *count <= 1 {
                map.remove(ip);
            } else {
                *count -= 1;
            }
        }
    }
}

/// An optional "attach to service" target resolved by the caller (the index or
/// proxy server) before a WebSocket session is spawned.
///
/// Attaching is a *working-directory* attach, not a live PTY attach: the
/// managed service's PTY lives in its own fog daemon process and is only
/// reachable over IPC, so the web terminal cannot share that exact master.
/// Instead a fresh interactive shell is spawned with the service's working
/// directory and environment, giving the user a terminal that looks like they
/// are inside the running service. The service itself is untouched and keeps
/// running regardless of this session's lifetime.
#[derive(Clone, Debug)]
pub struct ServiceTarget {
    /// Absolute working directory for the spawned shell (the service's path).
    pub cwd: PathBuf,
    /// Environment variables (name -> value) to set on the spawned shell.
    /// `TERM` is always forced separately; these are applied on top.
    pub env: Vec<(String, String)>,
}

/// Returns `true` when `req` is a WebSocket upgrade request for the terminal
/// gateway at `/ws/terminal`.
pub fn is_terminal_upgrade(req: &Request<impl hyper::body::Body>) -> bool {
    req.method() == hyper::Method::GET
        && req.uri().path() == "/ws/terminal"
        && req
            .headers()
            .get(hyper::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
        && req
            .headers()
            .get(hyper::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_lowercase().contains("upgrade"))
            .unwrap_or(false)
}

/// Extracts a query parameter value by name from a URI query string.
fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let mut it = pair.splitn(2, '=');
        let key = it.next()?;
        if key == name {
            Some(it.next().unwrap_or("").to_string())
        } else {
            None
        }
    })
}

/// Checks whether the request satisfies the optional `auth_token` requirement.
/// Returns `Some(response)` when the request must be rejected, `None` to allow.
fn check_auth(
    req: &Request<hyper::body::Incoming>,
    config: &TerminalConfig,
) -> Option<Response<BoxBody>> {
    match &config.auth_token {
        Some(expected) => {
            let provided = query_param(req.uri().query(), "auth_token");
            if provided.as_deref() != Some(expected.as_str()) {
                return Some(
                    Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(body_full(Bytes::from(
                            "unauthorized: missing or invalid auth_token",
                        )))
                        .expect("response builder failed"),
                );
            }
            None
        }
        None => None,
    }
}

/// Answers a `GET /ws/terminal` WebSocket upgrade.
///
/// Applies the hardening checks (auth, per-IP cap) before answering. On
/// success returns a `101 Switching Protocols` response immediately; the actual
/// PTY session is spawned in a background task once the client's upgrade
/// handshake completes. On a rejected check returns `401`/`429` instead.
pub async fn handle_terminal_upgrade(
    mut req: Request<hyper::body::Incoming>,
    config: Arc<TerminalConfig>,
    registry: Arc<SessionsRegistry>,
    peer_ip: String,
    target: Option<ServiceTarget>,
) -> Result<Response<BoxBody>, std::convert::Infallible> {
    if let Some(resp) = check_auth(&req, &config) {
        return Ok(resp);
    }

    let session_key = registry_ip(&peer_ip);
    if let Err(count) = registry.acquire(&session_key, config.max_sessions_per_ip) {
        return Ok(Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(hyper::header::RETRY_AFTER, "1")
            .body(body_full(Bytes::from(format!(
                "too many terminal sessions from this IP (max {}; active {count})",
                config.max_sessions_per_ip
            ))))
            .expect("response builder failed"));
    }

    let on_upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        // Client disconnecting before the upgrade completes is not an error.
        if let Ok(upgraded) = on_upgrade.await {
            run_terminal_session(TokioIo::new(upgraded), config, target).await;
        }
        registry.release(&session_key);
    });

    // Per RFC 6455 §4.2.2 the server must answer with `Sec-WebSocket-Accept`,
    // derived from the client's `Sec-WebSocket-Key` plus the fixed GUID.
    let accept = req
        .headers()
        .get(hyper::header::SEC_WEBSOCKET_KEY)
        .and_then(|v| v.to_str().ok())
        .map(|key| derive_accept_key(key.as_bytes()))
        .unwrap_or_default();

    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(hyper::header::CONNECTION, "Upgrade")
        .header(hyper::header::UPGRADE, "websocket")
        .header(hyper::header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(body_full(Bytes::new()))
        .expect("response builder failed"))
}

/// Handles a live service attach: proxies WS bytes to the daemon's PTY via IPC
/// (TerminalInput/Resize) and streams snapshots back. This is the "same process"
/// emulation requested by the user.
pub async fn handle_live_terminal_upgrade(
    mut req: Request<hyper::body::Incoming>,
    config: Arc<TerminalConfig>,
    registry: Arc<SessionsRegistry>,
    peer_ip: String,
    service: String,
) -> Result<Response<BoxBody>, std::convert::Infallible> {
    if let Some(resp) = check_auth(&req, &config) {
        return Ok(resp);
    }
    let session_key = registry_ip(&peer_ip);
    if let Err(count) = registry.acquire(&session_key, config.max_sessions_per_ip) {
        return Ok(Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(hyper::header::RETRY_AFTER, "1")
            .body(body_full(Bytes::from(format!(
                "too many terminal sessions from this IP (max {}; active {count})",
                config.max_sessions_per_ip
            ))))
            .expect("response builder failed"));
    }
    let on_upgrade = hyper::upgrade::on(&mut req);
    let svc = service.clone();
    tokio::spawn(async move {
        if let Ok(upgraded) = on_upgrade.await {
            run_live_terminal_session(TokioIo::new(upgraded), svc, config).await;
        }
        registry.release(&session_key);
    });
    let accept = req
        .headers()
        .get(hyper::header::SEC_WEBSOCKET_KEY)
        .and_then(|v| v.to_str().ok())
        .map(|key| derive_accept_key(key.as_bytes()))
        .unwrap_or_default();
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(hyper::header::CONNECTION, "Upgrade")
        .header(hyper::header::UPGRADE, "websocket")
        .header(hyper::header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(body_full(Bytes::new()))
        .expect("response builder failed"))
}

async fn run_live_terminal_session(io: TokioIo<Upgraded>, service: String, config: Arc<TerminalConfig>) {
    let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
    let (mut ws_sink, mut ws_stream) = ws.split();
    // Find daemon socket that owns this service (first instance where service running).
    let daemon_path = crate::ipc::find_instances()
        .ok()
        .and_then(|instances| {
            for (_, path) in instances {
                if let Ok(status) = crate::ipc::query_status(&path) {
                    if status.services.iter().any(|s| s.name == service && s.running) {
                        return Some(path);
                    }
                }
            }
            None
        });
    let Some(daemon_path) = daemon_path else {
        let _ = ws_sink
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Away,
                reason: format!("service {service} not running").into(),
            })))
            .await;
        return;
    };
    // Send initial snapshot immediately, then poll every 100ms.
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let idle_timeout = Duration::from_secs(config.idle_timeout_secs);
    let max_bytes = config.max_message_bytes;
    let mut last_activity = Instant::now();
    let mut seen_total = 0;
    if let Ok((chunks, total)) = crate::ipc::query_terminal_snapshot(&daemon_path, &service, 500, 0) {
        for bytes in &chunks {
            let _ = ws_sink.send(Message::binary(bytes.clone())).await;
        }
        seen_total = total;
    }
    loop {
        tokio::select! {
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_activity = Instant::now();
                        let s = text.to_string();
                        if s.len() > max_bytes {
                            let _ = ws_sink.send(Message::Close(Some(CloseFrame{ code: CloseCode::Size, reason: "frame too large".into()}))).await;
                            break;
                        }
                        if is_live_resize(&s, &daemon_path, &service) {
                            // handled via IPC resize
                        } else {
                            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, s.as_bytes());
                            let _ = crate::ipc::send_service_action(&daemon_path, &service, crate::ipc::ServiceAction::TerminalInput{ data: b64 });
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        last_activity = Instant::now();
                        if data.len() > max_bytes {
                            let _ = ws_sink.send(Message::Close(Some(CloseFrame{ code: CloseCode::Size, reason: "frame too large".into()}))).await;
                            break;
                        }
                        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
                        let _ = crate::ipc::send_service_action(&daemon_path, &service, crate::ipc::ServiceAction::TerminalInput{ data: b64 });
                    }
                    Some(Ok(Message::Ping(_))) => { let _ = ws_sink.send(Message::Pong(Bytes::new())).await; }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if let Ok((chunks, total)) = crate::ipc::query_terminal_snapshot(&daemon_path, &service, 500, 0) {
                    if total > seen_total {
                        let new_count = (total - seen_total).min(chunks.len());
                        let start = chunks.len().saturating_sub(new_count);
                        for bytes in &chunks[start..] {
                            let _ = ws_sink.send(Message::binary(bytes.clone())).await;
                            last_activity = Instant::now();
                        }
                        seen_total = total;
                    }
                }
            }
            _ = ping.tick() => {
                if last_activity.elapsed() >= idle_timeout {
                    let _ = ws_sink.send(Message::Close(None)).await;
                    break;
                }
                let _ = ws_sink.send(Message::Ping(Bytes::new())).await;
            }
        }
    }
}

fn is_live_resize(text: &str, daemon_path: &std::path::Path, service: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return false };
    if v.get("type").and_then(|t| t.as_str()) != Some("resize") { return false; }
    let cols = v.get("cols").and_then(|c| c.as_u64()).unwrap_or(DEFAULT_COLS as u64) as u16;
    let rows = v.get("rows").and_then(|r| r.as_u64()).unwrap_or(DEFAULT_ROWS as u64) as u16;
    let _ = crate::ipc::send_service_action(daemon_path, service, crate::ipc::ServiceAction::TerminalResize{ cols, rows });
    true
}

/// Normalizes a peer IP into a stable session-counting key.
fn registry_ip(ip: &str) -> String {
    ip.to_string()
}

/// Spawns the PTY-backed shell and pumps frames between the WebSocket and the
/// PTY until the client disconnects, the session idles out, or the shell exits.
async fn run_terminal_session(
    io: TokioIo<Upgraded>,
    config: Arc<TerminalConfig>,
    target: Option<ServiceTarget>,
) {
    let session = match TerminalSession::spawn(target) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("fog: terminal: failed to spawn PTY: {e}");
            return;
        }
    };
    if let Err(e) = session.run(io, &config).await {
        eprintln!("fog: terminal: session error: {e}");
    }
}

/// A live PTY shell wired to a WebSocket via two pump threads.
///
/// The PTY's blocking `Read`/`Write` handles are moved onto dedicated OS
/// threads, bridged by `tokio` channels, so the async session loop never blocks
/// on synchronous I/O. The session owns the [`MasterPty`] so it can resize the
/// PTY on demand, and the [`portable_pty::Child`] so the shell is reaped when
/// the session ends.
struct TerminalSession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    input_tx: mpsc::Sender<Vec<u8>>,
    output_q: Arc<OutputQueue>,
    last_activity: Instant,
}

/// Bounded, drop-oldest frame queue bridging the PTY read thread and the async
/// WebSocket pump. When the queue is full, the oldest buffered frame is
/// discarded to make room, bounding memory while keeping the most recent output
/// (so the client never sees stale, superseded output).
struct OutputQueue {
    inner: Mutex<VecDeque<Vec<u8>>>,
    notify: tokio::sync::Notify,
    closed: AtomicBool,
}

impl OutputQueue {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            notify: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
        })
    }

    /// Appends a frame, dropping the oldest when at capacity.
    fn push(&self, frame: Vec<u8>) {
        let mut q = self.inner.lock().expect("output queue poisoned");
        if q.len() >= OUTPUT_QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(frame);
        drop(q);
        self.notify.notify_one();
    }

    /// Marks the queue closed (PTY EOF / session teardown) and wakes the pump.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    /// Waits for and returns the next frame, or `None` when the queue is closed
    /// and drained.
    async fn recv(&self) -> Option<Vec<u8>> {
        loop {
            {
                let mut q = self.inner.lock().expect("output queue poisoned");
                if let Some(f) = q.pop_front() {
                    return Some(f);
                }
                if self.closed.load(Ordering::SeqCst) {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }
}

impl TerminalSession {
    /// Opens a PTY and spawns the user's login shell (`$SHELL`, defaulting to
    /// `bash`) in it. When `target` is set, the shell is spawned with the
    /// service's working directory and environment (a working-directory
    /// "attach"); otherwise it uses the daemon's current directory.
    fn spawn(target: Option<ServiceTarget>) -> std::io::Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
        let mut cmd = CommandBuilder::new(shell);
        // Pin a stable working directory (the daemon's cwd may be deleted or
        // unrelated to the project) and force a proper TERM so the shell — and
        // any full-screen TUI it launches — renders color/control sequences
        // instead of degrading to a dumb terminal.
        let (cwd, env) = target
            .map(|t| (Some(t.cwd), t.env))
            .unwrap_or_else(|| (None, Vec::new()));
        let cwd = cwd.unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        });
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let master = pair.master;

        // Client -> PTY pump.
        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(INPUT_CHANNEL_CAP);
        std::thread::spawn(move || {
            while let Some(bytes) = input_rx.blocking_recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        // PTY -> client pump: reads into a bounded, drop-oldest queue.
        let output_q = OutputQueue::new(OUTPUT_QUEUE_CAP);
        let reader_q = Arc::clone(&output_q);
        std::thread::spawn(move || {
            let mut buf = [0u8; PTY_BUF];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // shell exited
                    Ok(n) => reader_q.push(buf[..n].to_vec()),
                    Err(_) => break,
                }
            }
            reader_q.close();
        });

        Ok(TerminalSession {
            master,
            child,
            input_tx,
            output_q,
            last_activity: Instant::now(),
        })
    }

    /// Runs the frame pump until the session ends.
    ///
    /// The HTTP upgrade (and the RFC 6455 `Sec-WebSocket-Accept` exchange) was
    /// already completed by hyper in [`handle_terminal_upgrade`], so the
    /// upgraded stream is wrapped as a raw socket rather than re-doing the
    /// handshake with `accept_async`.
    async fn run(self, io: TokioIo<Upgraded>, config: &TerminalConfig) -> std::io::Result<()> {
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
        let (mut ws_sink, mut ws_stream) = ws.split();

        let TerminalSession {
            master,
            mut child,
            input_tx,
            output_q,
            mut last_activity,
        } = self;

        let idle_timeout = Duration::from_secs(config.idle_timeout_secs);
        let max_message_bytes = config.max_message_bytes;

        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                msg = ws_stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            last_activity = Instant::now();
                            let s = text.to_string();
                            if s.len() > max_message_bytes {
                                let _ = ws_sink.send(Message::Close(Some(
                                    CloseFrame {
                                        code: CloseCode::Size,
                                        reason: "frame too large".into(),
                                    }
                                ))).await;
                                break;
                            }
                            if is_resize(&s, master.as_ref()) {
                                // Resized via JSON; not forwarded as input.
                            } else if input_tx.send(s.into_bytes()).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Binary(data))) => {
                            last_activity = Instant::now();
                            if data.len() > max_message_bytes {
                                let _ = ws_sink.send(Message::Close(Some(
                                    CloseFrame {
                                        code: CloseCode::Size,
                                        reason: "frame too large".into(),
                                    }
                                ))).await;
                                break;
                            }
                            if input_tx.send(data.to_vec()).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Ping(_))) => {
                            if ws_sink.send(Message::Pong(Bytes::new())).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Pong(_))) => { /* keep-alive reply */ }
                        Some(Ok(Message::Frame(_))) => { /* passthrough, ignore */ }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    }
                }
                out = output_q.recv() => {
                    match out {
                        Some(bytes) => {
                            last_activity = Instant::now();
                            if ws_sink.send(Message::binary(bytes)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping.tick() => {
                    if last_activity.elapsed() >= idle_timeout {
                        let _ = ws_sink.send(Message::Close(None)).await;
                        break;
                    }
                    if ws_sink.send(Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
            }
        }

        // Reap the shell and stop the pump threads by dropping the channels.
        let _ = child.kill();
        drop(input_tx);
        output_q.close();
        Ok(())
    }
}

/// Handles a `{"type":"resize","cols":N,"rows":N}` control frame.
///
/// Returns `true` when the text was a resize command (and therefore should not
/// be forwarded to the PTY as input). Dimensions are clamped to `1..=2000`.
fn is_resize(text: &str, master: &dyn MasterPty) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    if value.get("type").and_then(|t| t.as_str()) != Some("resize") {
        return false;
    }
    let clamp_dim = |v: u64| -> u16 { v.clamp(MIN_DIM as u64, MAX_DIM as u64) as u16 };
    let cols = value
        .get("cols")
        .and_then(|c| c.as_u64())
        .map(clamp_dim)
        .unwrap_or(DEFAULT_COLS);
    let rows = value
        .get("rows")
        .and_then(|r| r.as_u64())
        .map(clamp_dim)
        .unwrap_or(DEFAULT_ROWS);
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let _ = master.resize(size);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    fn upgrade_request(path: &str) -> Request<Full<Bytes>> {
        Request::builder()
            .method("GET")
            .uri(path)
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::CONNECTION, "Upgrade")
            .header(hyper::header::HOST, "localhost")
            .body(Full::new(Bytes::new()))
            .expect("request builder failed")
    }

    #[test]
    fn test_is_terminal_upgrade_matches() {
        assert!(is_terminal_upgrade(&upgrade_request("/ws/terminal")));
    }

    #[test]
    fn test_is_terminal_upgrade_rejects_other_path() {
        assert!(!is_terminal_upgrade(&upgrade_request("/api")));
    }

    #[test]
    fn test_is_terminal_upgrade_rejects_wrong_method() {
        let mut req = upgrade_request("/ws/terminal");
        *req.method_mut() = hyper::Method::POST;
        assert!(!is_terminal_upgrade(&req));
    }

    #[test]
    fn test_query_param_extracts_value() {
        assert_eq!(
            query_param(Some("auth_token=abc&x=1"), "auth_token"),
            Some("abc".into())
        );
        assert_eq!(
            query_param(Some("auth_token=abc&x=1"), "x"),
            Some("1".into())
        );
        assert_eq!(query_param(Some("auth_token=abc"), "missing"), None);
        assert_eq!(query_param(None, "auth_token"), None);
    }

    #[test]
    fn test_is_resize_applies_dimensions() {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let master = pty.master;
        assert!(is_resize(
            r#"{"type":"resize","cols":132,"rows":43}"#,
            master.as_ref()
        ));
        assert_eq!(master.get_size().unwrap().cols, 132);
        assert_eq!(master.get_size().unwrap().rows, 43);
    }

    #[test]
    fn test_is_resize_clamps_dimensions() {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let master = pty.master;
        // Oversized values clamp to MAX_DIM (2000).
        assert!(is_resize(
            r#"{"type":"resize","cols":99999,"rows":99999}"#,
            master.as_ref()
        ));
        assert_eq!(master.get_size().unwrap().cols, MAX_DIM as u16);
        assert_eq!(master.get_size().unwrap().rows, MAX_DIM as u16);
        // Zero values clamp up to MIN_DIM (1).
        assert!(is_resize(
            r#"{"type":"resize","cols":0,"rows":0}"#,
            master.as_ref()
        ));
        assert_eq!(master.get_size().unwrap().cols, MIN_DIM as u16);
        assert_eq!(master.get_size().unwrap().rows, MIN_DIM as u16);
    }

    #[test]
    fn test_is_resize_ignores_non_resize() {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        assert!(!is_resize("plain input", pty.master.as_ref()));
        assert!(!is_resize(
            r#"{"type":"something-else"}"#,
            pty.master.as_ref()
        ));
    }

    #[test]
    fn test_registry_acquire_enforces_cap() {
        let reg = SessionsRegistry::default();
        assert!(reg.acquire("1.2.3.4", 2).is_ok());
        assert!(reg.acquire("1.2.3.4", 2).is_ok());
        assert!(reg.acquire("1.2.3.4", 2).is_err());
        reg.release("1.2.3.4");
        assert!(reg.acquire("1.2.3.4", 2).is_ok());
    }

    #[test]
    fn test_output_queue_drops_oldest() {
        let q = OutputQueue::new(OUTPUT_QUEUE_CAP);
        for i in 0..(OUTPUT_QUEUE_CAP + 5) {
            q.push(format!("frame-{i}").into_bytes());
        }
        // The oldest 5 frames were dropped; the newest frame survives.
        let mut popped = Vec::new();
        while let Some(f) = q.inner.lock().unwrap().pop_front() {
            popped.push(String::from_utf8(f).unwrap());
        }
        assert_eq!(popped.len(), OUTPUT_QUEUE_CAP);
        assert_eq!(popped[0], "frame-5");
        assert_eq!(
            popped.last().unwrap(),
            &format!("frame-{}", OUTPUT_QUEUE_CAP + 4)
        );
    }
}
