//! Log streaming: docker `logs --follow` as SSE, fog-instance logs over IPC,
//! and one-shot history windows.

use futures_core::Stream;
use http_body::Frame;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use super::{DEFAULT_LOG_TAIL, MAX_LOG_TAIL, RespBody, api_error, json_response, parse_query};

/// Streams one service's logs as Server-Sent Events.
///
/// Two sources are supported, selected by the query string:
///   - `service=<container>` — docker, via `docker logs
///     --timestamps [--tail N | --since S] --follow <container>`
///   - `pid=<fog-pid>&service=<name>` — a fog instance's captured log (or
///     `proxy` for its request log), proxied over the instance's Unix socket
///
/// Every docker event carries an `id:` of the line's unix timestamp, so when
/// the browser reconnects it sends `Last-Event-ID` and the stream resumes
/// with `--since` — no duplicated lines and no gap-replay of the backfill.
///
/// When the HTTP connection closes, the response body is dropped, which kills
/// the `docker logs` child (or closes the fog socket) so nothing lingers.
pub(super) async fn serve_logs_stream(req: &Request<Incoming>) -> Response<RespBody> {
    let params = parse_query(req.uri().query().unwrap_or(""));
    let service = params.get("service").map(String::as_str).unwrap_or("");
    let tail = params
        .get("tail")
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LOG_TAIL)
        .clamp(1, MAX_LOG_TAIL);

    if let Some(pid) = params.get("pid").and_then(|p| p.parse::<u32>().ok()) {
        return serve_fog_logs_stream(pid, service, tail).await;
    }

    let container = service;
    if !is_valid_container(container) {
        return sse_single("error: missing or invalid service (container) name");
    }
    // EventSource resumes with `Last-Event-ID` (unix seconds); a `since` query
    // parameter is also accepted so curl/CLI debugging can skip the backfill.
    let since = req
        .headers()
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or_else(|| params.get("since").and_then(|s| s.parse::<u64>().ok()));

    if !docker_container_exists(container).await {
        return sse_single(&format!("error: no such container '{container}'"));
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, Infallible>>(256);
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("logs").arg("--timestamps");
    match since {
        Some(secs) => {
            cmd.arg("--since").arg(secs.to_string());
        }
        None => {
            cmd.arg("--tail").arg(tail.to_string());
        }
    }
    cmd.arg("--follow")
        .arg(container)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return sse_single(&format!("error: could not start docker logs: {e}")),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    tokio::spawn(async move {
        stream_docker_lines(tx, stdout, stderr).await;
    });

    let body = StreamBody::new(LogStream { rx, child }).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("response builder failed")
}

/// Streams a fog instance's captured log (or proxy request log) by proxying
/// its `logs` IPC request over `$TMPDIR/fog-<pid>.sock`. Each line the
/// instance emits is relayed as an SSE `data:` event.
async fn serve_fog_logs_stream(pid: u32, service: &str, tail: usize) -> Response<RespBody> {
    if !is_valid_service_name(service) {
        return sse_single("error: missing or invalid service name");
    }
    let sock_path = crate::ipc::socket_path(pid);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, Infallible>>(256);
    let (close_tx, close_rx) = tokio::sync::mpsc::channel::<()>(1);
    let service = service.to_string();
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut close_rx = close_rx;
        let Ok(mut sock) = crate::ipc::transport::connect_async(&sock_path).await else {
            let _ = tx
                .send(Ok(Frame::data(Bytes::from(
                    "data: [fog] no such fog instance\n\n",
                ))))
                .await;
            return;
        };
        let request = format!(
            "{{\"type\":\"logs\",\"service\":{},\"tail\":{tail},\"follow\":true}}\n",
            serde_json::to_string(&service).unwrap_or_else(|_| "\"\"".to_string())
        );
        if sock.write_all(request.as_bytes()).await.is_err() {
            return;
        }
        let mut reader = tokio::io::BufReader::new(sock);
        let mut line: Vec<u8> = Vec::with_capacity(1024);
        loop {
            line.clear();
            tokio::select! {
                r = reader.read_until(b'\n', &mut line) => {
                    match r {
                        Ok(0) => break,
                        Ok(_) => {
                            let text = String::from_utf8_lossy(&line);
                            let text = text.strip_suffix('\n').unwrap_or_else(|| &text);
                            let text = text.strip_suffix('\r').unwrap_or(text);
                            if tx.send(Ok(Frame::data(Bytes::from(sse_raw_line(text))))).await.is_err() {
                                return;
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Client disconnected: close the fog socket so the instance's
                // follow loop stops.
                _ = close_rx.recv() => return,
            }
        }
        let _ = tx
            .send(Ok(Frame::data(Bytes::from("data: [fog] stream ended\n\n"))))
            .await;
    });
    let body = StreamBody::new(FogLogStream {
        rx,
        close: close_tx,
    })
    .boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("response builder failed")
}

/// One-shot JSON history window for scroll-up backfill:
/// `GET /api/logs/history?service=<name>&[pid=<fog-pid>]&[tail=N]&[offset=M]`.
///
/// `offset` skips that many newest lines (already shown by the live stream);
/// `tail` is how many lines before that to return. Responds
/// `{"lines":[...],"has_more":bool}` where `has_more` tells the viewer an
/// older window probably exists. Bodies match the SSE stream (docker
/// timestamp prefixes stripped).
pub(super) async fn serve_logs_history(req: &Request<Incoming>) -> Response<RespBody> {
    let params = parse_query(req.uri().query().unwrap_or(""));
    let service = params.get("service").map(String::as_str).unwrap_or("");
    let tail = params
        .get("tail")
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(500)
        .clamp(1, MAX_LOG_TAIL);
    let offset = params
        .get("offset")
        .and_then(|o| o.parse::<usize>().ok())
        .unwrap_or(0)
        .clamp(0, MAX_LOG_TAIL);
    // +1 over-fetch detects whether an older window exists without a second
    // query (when the store returns exactly `need`, older lines remain).
    let need = (tail.saturating_add(offset).saturating_add(1)).clamp(1, MAX_LOG_TAIL);

    if let Some(pid) = params.get("pid").and_then(|p| p.parse::<u32>().ok()) {
        if !is_valid_service_name(service) {
            return api_error(StatusCode::BAD_REQUEST, "missing or invalid service name");
        }
        let sock = crate::ipc::socket_path(pid);
        let svc = service.to_string();
        let fetched =
            tokio::task::spawn_blocking(move || crate::ipc::query_logs(&sock, &svc, need)).await;
        match fetched {
            Ok(Ok(lines)) => {
                let (window, has_more) = slice_log_window(lines, tail, offset);
                return json_response(&serde_json::json!({
                    "lines": window,
                    "has_more": has_more,
                }));
            }
            Ok(Err(e)) => {
                return api_error(StatusCode::NOT_FOUND, &format!("no such fog instance: {e}"));
            }
            Err(e) => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("history lookup failed: {e}"),
                );
            }
        }
    }

    if !is_valid_container(service) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "missing or invalid service (container) name",
        );
    }
    if !docker_container_exists(service).await {
        return api_error(
            StatusCode::NOT_FOUND,
            &format!("no such container '{service}'"),
        );
    }
    match docker_log_history(service, need).await {
        Ok(lines) => {
            let (window, has_more) = slice_log_window(lines, tail, offset);
            json_response(&serde_json::json!({
                "lines": window,
                "has_more": has_more,
            }))
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not read docker logs: {e}"),
        ),
    }
}

/// Runs `docker logs --timestamps --tail <need>` once (no follow) and returns
/// the newest `need` message bodies in chronological order. Stdout and stderr
/// are merged by timestamp so services logging to both (e.g. postgres) keep
/// roughly the same order the SSE stream shows.
async fn docker_log_history(container: &str, need: usize) -> io::Result<Vec<String>> {
    let out = tokio::process::Command::new("docker")
        .arg("logs")
        .arg("--timestamps")
        .arg("--tail")
        .arg(need.to_string())
        .arg(container)
        .output()
        .await?;
    let mut merged: Vec<(Option<u64>, usize, String)> = Vec::new();
    let mut order = 0usize;
    let push = |bytes: &[u8], merged: &mut Vec<(Option<u64>, usize, String)>, order: &mut usize| {
        for raw in String::from_utf8_lossy(bytes).lines() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let (secs, body) = match split_docker_log_line(line) {
                Some((s, b)) => (Some(s), b.to_string()),
                None => (None, line.to_string()),
            };
            merged.push((secs, *order, body));
            *order += 1;
        }
    };
    push(&out.stdout, &mut merged, &mut order);
    push(&out.stderr, &mut merged, &mut order);
    merged.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(merged.into_iter().map(|(_, _, b)| b).collect())
}

/// Slices a newest-first `need = tail + offset + 1` fetch into the requested
/// window: skip the newest `offset` lines, return the previous `tail`.
/// `has_more` is true when the fetch hit its cap, meaning older lines remain.
pub(super) fn slice_log_window(
    all: Vec<String>,
    tail: usize,
    offset: usize,
) -> (Vec<String>, bool) {
    let total = all.len();
    let need = tail.saturating_add(offset).saturating_add(1);
    let has_more = total >= need && need < MAX_LOG_TAIL.saturating_add(1);
    // When capped exactly at `need`, index 0 is the +1 detection probe.
    let usable = if has_more { &all[1..] } else { &all[..] };
    let end = usable.len().saturating_sub(offset);
    let start = end.saturating_sub(tail);
    (usable[start..end].to_vec(), has_more)
}

/// Response body backing a fog-instance log stream. When dropped (client
/// disconnected), it signals the reader task to close the fog socket, which
/// makes the instance's follow loop stop.
struct FogLogStream {
    rx: tokio::sync::mpsc::Receiver<Result<Frame<Bytes>, Infallible>>,
    close: tokio::sync::mpsc::Sender<()>,
}

impl Stream for FogLogStream {
    type Item = Result<Frame<Bytes>, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for FogLogStream {
    fn drop(&mut self) {
        let _ = self.close.try_send(());
    }
}

/// Response body backing the `/logs/stream` endpoint: yields SSE bytes pushed
/// by the reader task. Dropping it also drops the `docker logs` child, which
/// (with `kill_on_drop`) terminates the follow process and ends the reader.
struct LogStream {
    rx: tokio::sync::mpsc::Receiver<Result<Frame<Bytes>, Infallible>>,
    child: tokio::process::Child,
}

impl Stream for LogStream {
    type Item = Result<Frame<Bytes>, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for LogStream {
    fn drop(&mut self) {
        // The HTTP connection closed (or the body was abandoned): terminate
        // `docker logs --follow` so no orphan process keeps streaming. The
        // resulting stdout EOF makes the reader task exit and close the
        // channel. `kill_on_drop` on the spawn config is kept as a backstop.
        let _ = self.child.start_kill();
    }
}

/// Reads `docker logs` stdout *and* stderr (services like postgres write to
/// the container's stderr, which `docker logs` relays on its own stderr) line
/// by line and forwards each as an SSE event. Exits when both pipes hit EOF
/// or the receiver is dropped (client disconnected); the final
/// `[fog] stream ended` event lets the page stop reconnecting.
async fn stream_docker_lines(
    tx: tokio::sync::mpsc::Sender<Result<Frame<Bytes>, Infallible>>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
) {
    use tokio::io::AsyncBufReadExt;
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        return;
    };
    let mut out = tokio::io::BufReader::new(stdout);
    let mut err = tokio::io::BufReader::new(stderr);
    let mut out_line: Vec<u8> = Vec::with_capacity(1024);
    let mut err_line: Vec<u8> = Vec::with_capacity(1024);
    let mut out_done = false;
    let mut err_done = false;
    while !(out_done && err_done) {
        tokio::select! {
            r = { out_line.clear(); out.read_until(b'\n', &mut out_line) }, if !out_done => {
                match r {
                    Ok(0) => out_done = true,
                    Ok(_) => {
                        if send_log_line(&tx, &out_line).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => out_done = true,
                }
            }
            r = { err_line.clear(); err.read_until(b'\n', &mut err_line) }, if !err_done => {
                match r {
                    Ok(0) => err_done = true,
                    Ok(_) => {
                        if send_log_line(&tx, &err_line).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => err_done = true,
                }
            }
        }
    }
    let _ = tx
        .send(Ok(Frame::data(Bytes::from("data: [fog] stream ended\n\n"))))
        .await;
}

/// Converts one raw `docker logs` line into an SSE event and sends it.
/// Returns `Err` when the receiver is gone (client disconnected).
async fn send_log_line(
    tx: &tokio::sync::mpsc::Sender<Result<Frame<Bytes>, Infallible>>,
    line: &[u8],
) -> Result<(), ()> {
    let text = String::from_utf8_lossy(line);
    let text = text.strip_suffix('\n').unwrap_or_else(|| &text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    tx.send(Ok(Frame::data(Bytes::from(sse_event(text)))))
        .await
        .map_err(|_| ())
}

/// An SSE response carrying a single `[fog] ...` message then ending. Used for
/// errors (invalid container, spawn failure) so the page can surface them and
/// stop reconnecting.
fn sse_single(message: &str) -> Response<RespBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Full::new(Bytes::from(format!("data: [fog] {message}\n\n"))).boxed())
        .expect("response builder failed")
}

/// Formats a log line as an SSE event. `docker logs --timestamps` prefixes
/// each line with an RFC3339Nano UTC timestamp; that becomes the event `id`
/// (unix seconds) so reconnects resume via `--since`. Lines without a
/// parseable timestamp still stream, just without an id.
pub(super) fn sse_event(line: &str) -> String {
    match split_docker_log_line(line) {
        Some((secs, body)) => format!("id: {secs}\ndata: {body}\n\n"),
        None => format!("data: {line}\n\n"),
    }
}

/// Wraps a raw (non-docker) log line as an SSE event without an id — used for
/// fog-instance log streams, which carry no timestamp to resume from.
fn sse_raw_line(line: &str) -> String {
    format!("data: {line}\n\n")
}

/// Splits a `--timestamps` log line into its unix-seconds timestamp and the
/// message body (everything after the first space). Returns `None` when the
/// line has no leading timestamp.
pub(super) fn split_docker_log_line(line: &str) -> Option<(u64, &str)> {
    let (ts, body) = line.split_once(' ')?;
    let secs = parse_docker_timestamp(ts)?;
    Some((secs, body))
}

/// Parses a docker `--timestamps` prefix (`YYYY-MM-DDTHH:MM:SS[.fraction]Z`,
/// always UTC) into unix seconds. Returns `None` on malformed input.
pub(super) fn parse_docker_timestamp(ts: &str) -> Option<u64> {
    let ts = ts.strip_suffix('Z')?;
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let seconds: i64 = t.next()?.split('.').next()?.parse().ok()?;
    if hour > 23 || minute > 59 || seconds > 60 {
        return None;
    }
    // days_from_civil (Howard Hinnant): civil date to days since the Unix epoch.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3_600 + minute * 60 + seconds) as u64)
}

/// Accepts only well-formed docker container names (the charset docker itself
/// permits), which also blocks path traversal and flag injection via the URL.
pub(super) fn is_valid_container(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Accepts fog service names and the special `proxy`/`daemon` log names. The
/// allowlist blocks path traversal; the IPC handler sanitizes further.
pub(super) fn is_valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-' | ':' | '@'))
}

/// Whether a container with `name` currently exists (running or stopped).
async fn docker_container_exists(container: &str) -> bool {
    tokio::process::Command::new("docker")
        .args(["inspect", container])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
