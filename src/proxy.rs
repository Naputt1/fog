use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::ServerConfig;
use rustls_pemfile::{certs, private_key};
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

/// Response body type used by the proxy service.
type BoxedBody = UnsyncBoxBody<Bytes, hyper::Error>;

/// Parsed status code and headers of an HTTP/1.1 response head.
type ResponseHead = (u16, Vec<(String, Vec<u8>)>);

/// Wraps a fully-buffered byte body for the proxy service.
fn body_full(bytes: Bytes) -> BoxedBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// Upstream connect/handshake timeout for WebSocket and HTTP upstreams.
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const WS_HOP_BY_HOP: &[&str] = &[
    "host",
    "transfer-encoding",
    "te",
    "trailers",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
];

const HOP_BY_HOP: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Hop-by-hop headers a proxy must not forward to the client (RFC 7230 §6.1).
const RESPONSE_HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "proxy-authenticate",
    "proxy-authorization",
    "upgrade",
];

/// Returns `true` for headers that apply only to a single connection and must
/// be stripped when relaying an upstream response to the client.
fn is_hop_by_hop_response(name: &hyper::header::HeaderName) -> bool {
    RESPONSE_HOP_BY_HOP.contains(&name.as_str().to_ascii_lowercase().as_str())
}

trait IoBox: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> IoBox for T {}

fn load_tls_config(
    cert_path: &str,
    key_path: &str,
) -> Result<TlsAcceptor, Box<dyn std::error::Error>> {
    let cert_file = &mut std::io::BufReader::new(fs::File::open(cert_path)?);
    let key_file = &mut std::io::BufReader::new(fs::File::open(key_path)?);

    let cert_chain: Vec<rustls::pki_types::CertificateDer<'_>> =
        certs(cert_file).filter_map(Result::ok).collect();
    if cert_chain.is_empty() {
        return Err("no certificates found".into());
    }
    // `private_key` accepts PKCS#8, PKCS#1 (RSA), and SEC1 (EC) encodings.
    let key = private_key(key_file)?.ok_or_else(|| "no private keys found".to_string())?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// A single proxied request log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub method: String,
    pub path: String,
    pub upstream: String,
    pub status: u16,
    pub latency_ms: u64,
    pub ws: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteEntry {
    pub path: String,
    pub host: Option<String>,
    pub upstream: String,
    pub ws: bool,
}

struct HttpRequestContext<'a> {
    req: Request<hyper::body::Incoming>,
    client: Client<HttpConnector, Full<Bytes>>,
    route: &'a RouteEntry,
    suffix: &'a str,
    query: Option<&'a str>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    max_log_entries: usize,
    method: String,
    path: String,
    peer_ip: String,
    is_tls: bool,
}

pub struct ProxyInstance {
    pub port: u16,
    pub host: String,
    pub routes: Vec<RouteEntry>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    running: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    pub max_log_entries: usize,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

impl ProxyInstance {
    pub fn new(
        port: u16,
        host: Option<String>,
        routes: Vec<RouteEntry>,
        max_log_entries: usize,
        tls_cert: Option<String>,
        tls_key: Option<String>,
    ) -> Self {
        Self {
            port,
            host: host.unwrap_or_else(|| "0.0.0.0".to_string()),
            routes,
            logs: Arc::new(Mutex::new(VecDeque::with_capacity(max_log_entries))),
            running: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            handle: None,
            max_log_entries,
            tls_cert,
            tls_key,
        }
    }

    pub fn start(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            return;
        }

        let port = self.port;
        let host = self.host.clone();
        let routes = self.routes.clone();
        let logs = self.logs.clone();
        let max_entries = self.max_log_entries;
        let running = self.running.clone();
        let shutdown = self.shutdown.clone();

        // Load TLS config every start so cert rotation is picked up on
        // `restart()`. A TLS misconfiguration is fatal for the proxy: it must
        // not silently downgrade to plaintext on a TLS-configured port.
        let tls_acceptor = match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => match load_tls_config(cert, key) {
                Ok(a) => Some(a),
                Err(e) => {
                    let mut lk = logs.lock().expect("mutex poisoned");
                    lk.push_back(LogEntry {
                        method: "ERR".into(),
                        path: format!("TLS config failed: {}", e),
                        upstream: String::new(),
                        status: 0,
                        latency_ms: 0,
                        ws: false,
                    });
                    if lk.len() > max_entries {
                        lk.pop_front();
                    }
                    running.store(false, Ordering::SeqCst);
                    return;
                }
            },
            _ => None,
        };

        self.running.store(true, Ordering::SeqCst);
        self.shutdown.store(false, Ordering::SeqCst);

        let handle = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("failed to build tokio runtime");

            rt.block_on(async move {
                let addr = format!("{}:{}", host, port);
                let listener = match TcpListener::bind(&addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        let mut lk = logs.lock().expect("mutex poisoned");
                        lk.push_back(LogEntry {
                            method: "ERR".into(),
                            path: format!("Failed to bind {}: {}", addr, e),
                            upstream: String::new(),
                            status: 0,
                            latency_ms: 0,
                            ws: false,
                        });
                        if lk.len() > max_entries {
                            lk.pop_front();
                        }
                        running.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                // Configure the upstream client with a connect timeout and a
                // pool idle timeout so hung upstreams cannot leak connections.
                let mut connector = HttpConnector::new();
                connector.set_connect_timeout(Some(UPSTREAM_TIMEOUT));
                let client: Client<HttpConnector, Full<Bytes>> =
                    Client::builder(TokioExecutor::new())
                        .timer(TokioTimer::new())
                        .pool_idle_timeout(std::time::Duration::from_secs(90))
                        .build(connector);

                loop {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }

                    let accept =
                        tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept())
                            .await;

                    match accept {
                        Ok(Ok((stream, _))) => {
                            let peer_ip = stream
                                .peer_addr()
                                .ok()
                                .map(|a| a.ip().to_string())
                                .unwrap_or_default();
                            let client = client.clone();
                            let logs_for_svc = logs.clone();
                            let routes_for_svc = routes.clone();
                            let acceptor = tls_acceptor.clone();
                            let is_tls = tls_acceptor.is_some();

                            tokio::spawn(async move {
                                let io = if let Some(ref acceptor) = acceptor {
                                    match acceptor.accept(stream).await {
                                        Ok(tls_stream) => {
                                            TokioIo::new(Box::new(tls_stream) as Box<dyn IoBox>)
                                        }
                                        Err(e) => {
                                            let mut lk =
                                                logs_for_svc.lock().expect("mutex poisoned");
                                            lk.push_back(LogEntry {
                                                method: "ERR".into(),
                                                path: format!("TLS accept failed: {}", e),
                                                upstream: String::new(),
                                                status: 0,
                                                latency_ms: 0,
                                                ws: false,
                                            });
                                            if lk.len() > max_entries {
                                                lk.pop_front();
                                            }
                                            return;
                                        }
                                    }
                                } else {
                                    TokioIo::new(Box::new(stream) as Box<dyn IoBox>)
                                };
                                let svc = service_fn(move |req| {
                                    handle_request(
                                        req,
                                        client.clone(),
                                        routes_for_svc.clone(),
                                        logs_for_svc.clone(),
                                        max_entries,
                                        peer_ip.clone(),
                                        is_tls,
                                    )
                                });
                                if let Err(e) = http1::Builder::new()
                                    .serve_connection(io, svc)
                                    .with_upgrades()
                                    .await
                                {
                                    let _ = e;
                                }
                            });
                        }
                        Ok(Err(_)) => {}
                        Err(_) => continue,
                    }
                }

                running.store(false, Ordering::SeqCst);
            });
        });

        self.handle = Some(handle);
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.running.store(false, Ordering::SeqCst);
        self.shutdown.store(false, Ordering::SeqCst);
    }

    pub fn restart(&mut self) {
        self.stop();
        self.start();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn get_logs(&self) -> Vec<LogEntry> {
        let lk = self.logs.lock().expect("mutex poisoned");
        lk.iter().cloned().collect()
    }

    /// Returns a clone of the live request-log handle so other threads (e.g.
    /// the IPC server) can tail the proxy log in real time. The handle is
    /// stable across `restart()`: config hot-reloads reuse the same queue.
    pub fn logs_handle(&self) -> Arc<Mutex<VecDeque<LogEntry>>> {
        self.logs.clone()
    }

    /// Number of log entries matching the given filter (empty filter = all),
    /// used to keep the scrollbar in sync with what the renderer displays.
    pub fn filtered_log_len(&self, filter: &str) -> usize {
        let lk = self.logs.lock().expect("mutex poisoned");
        if filter.is_empty() {
            return lk.len();
        }
        let filter_lower = filter.to_lowercase();
        lk.iter()
            .filter(|entry| {
                entry.method.to_lowercase().contains(&filter_lower)
                    || entry.path.to_lowercase().contains(&filter_lower)
                    || entry.status.to_string().contains(&filter_lower)
                    || entry.upstream.to_lowercase().contains(&filter_lower)
            })
            .count()
    }
}

fn wildcard_match(pattern: &str, input: &str) -> bool {
    let p = pattern.as_bytes();
    let i = input.as_bytes();
    let mut pi = 0;
    let mut ii = 0;
    let mut star_idx: Option<usize> = None;
    let mut match_idx: usize = 0;

    while ii < i.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_idx = Some(pi);
            match_idx = ii;
            pi += 1;
        } else if pi < p.len() && p[pi] == i[ii] {
            pi += 1;
            ii += 1;
        } else if let Some(si) = star_idx {
            pi = si + 1;
            match_idx += 1;
            ii = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

fn match_route(incoming: &str, route: &str) -> Option<String> {
    if route.contains('*') {
        let pat_segs: Vec<&str> = route.split('/').collect();
        let inc_segs: Vec<&str> = incoming.split('/').collect();

        if pat_segs.len() != inc_segs.len() {
            return None;
        }

        for (p_seg, i_seg) in pat_segs.iter().zip(inc_segs.iter()) {
            if !wildcard_match(p_seg, i_seg) {
                return None;
            }
        }

        return Some(incoming.to_string());
    }

    let prefix = route.trim_end_matches('/');
    if incoming == route || incoming == prefix {
        return Some("/".to_string());
    }
    if incoming.starts_with(prefix) && incoming.as_bytes().get(prefix.len()) == Some(&b'/') {
        let suffix = &incoming[prefix.len()..];
        return Some(if suffix.is_empty() {
            "/".to_string()
        } else {
            suffix.to_string()
        });
    }
    if prefix.is_empty() && route.starts_with('/') {
        return Some(incoming.to_string());
    }
    None
}

fn match_host(incoming_host: Option<&str>, route_host: &Option<String>) -> bool {
    match route_host {
        Some(pattern) => match incoming_host {
            Some(host) => {
                let host = host.split(':').next().unwrap_or(host);
                wildcard_match(pattern, host)
            }
            None => false,
        },
        None => true,
    }
}

fn build_upstream_uri(
    upstream: &str,
    suffix: &str,
    query: Option<&str>,
) -> Result<hyper::Uri, String> {
    let base = upstream.trim_end_matches('/');
    let path = format!("{}{}", base, suffix);
    let s = match query {
        Some(q) if !q.is_empty() => format!("{}?{}", path, q),
        _ => path,
    };
    s.parse()
        .map_err(|_| format!("invalid upstream URI: {}", s))
}

fn forward_headers(incoming: &hyper::HeaderMap) -> hyper::HeaderMap {
    // Headers listed in the Connection header are hop-by-hop per RFC 7230 §6.1
    // and must be stripped alongside the fixed HOP_BY_HOP set.
    let mut connection_tokens = std::collections::HashSet::new();
    for v in incoming.get_all(hyper::header::CONNECTION).iter() {
        if let Ok(s) = v.to_str() {
            for token in s.split(',') {
                let t = token.trim().to_ascii_lowercase();
                if !t.is_empty() {
                    connection_tokens.insert(t);
                }
            }
        }
    }
    let mut out = hyper::HeaderMap::new();
    for (key, value) in incoming.iter() {
        let k = key.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&k.as_str()) || connection_tokens.contains(&k) {
            continue;
        }
        // `append` preserves multi-value headers (e.g. duplicate cookies),
        // unlike `insert` which would collapse them to the last value.
        out.append(key.clone(), value.clone());
    }
    out
}

/// Adds X-Forwarded-* headers so upstreams can see the original host, scheme,
/// and client address. The forwarding Host header is rewritten to the upstream
/// host by the client, so these are the only way the original host survives.
fn add_forwarded_headers(
    headers: &mut hyper::HeaderMap,
    peer_ip: &str,
    incoming_host: Option<&str>,
    is_tls: bool,
) {
    const X_FORWARDED_PROTO: &str = "x-forwarded-proto";
    const X_FORWARDED_HOST: &str = "x-forwarded-host";
    const X_FORWARDED_FOR: &str = "x-forwarded-for";
    headers.append(
        hyper::header::HeaderName::from_static(X_FORWARDED_PROTO),
        hyper::header::HeaderValue::from_static(if is_tls { "https" } else { "http" }),
    );
    if let Some(host) = incoming_host
        && let Ok(v) = hyper::header::HeaderValue::from_str(host)
    {
        headers.append(hyper::header::HeaderName::from_static(X_FORWARDED_HOST), v);
    }
    if !peer_ip.is_empty()
        && let Ok(v) = hyper::header::HeaderValue::from_str(peer_ip)
    {
        headers.append(hyper::header::HeaderName::from_static(X_FORWARDED_FOR), v);
    }
}

fn is_ws_upgrade(req: &Request<impl hyper::body::Body>) -> bool {
    req.method() == hyper::Method::GET
        && req
            .headers()
            .get(hyper::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_uppercase() == "WEBSOCKET")
            .unwrap_or(false)
        && req
            .headers()
            .get(hyper::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_lowercase().contains("upgrade"))
            .unwrap_or(false)
}

fn build_ws_request(
    suffix: &str,
    query: Option<&str>,
    headers: &hyper::HeaderMap,
    upstream_host: &str,
) -> Vec<u8> {
    let path = match query {
        Some(q) if !q.is_empty() => format!("{}?{}", suffix, q),
        _ => suffix.to_string(),
    };

    let mut bytes = format!("GET {} HTTP/1.1\r\n", path).into_bytes();

    bytes.extend_from_slice(b"Host: ");
    bytes.extend_from_slice(upstream_host.as_bytes());
    bytes.extend_from_slice(b"\r\n");

    for (key, value) in headers.iter() {
        let k = key.as_str().to_lowercase();
        if WS_HOP_BY_HOP.contains(&k.as_str()) {
            continue;
        }
        bytes.extend_from_slice(key.as_str().as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }

    bytes.extend_from_slice(b"\r\n");
    bytes
}

/// Connects to the upstream and completes the WebSocket handshake.
///
/// Returns the live TCP stream plus the parsed status code and headers of the
/// upstream's response. The stream is handed back so bytes can be piped once
/// the client upgrade completes.
async fn connect_ws_upstream(
    upstream_host: &str,
    request_bytes: &[u8],
) -> Result<(tokio::net::TcpStream, ResponseHead), String> {
    let connect = tokio::time::timeout(UPSTREAM_TIMEOUT, TcpStream::connect(upstream_host)).await;
    let mut stream = match connect {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("connect: {}", e)),
        Err(_) => return Err("connect timed out".to_string()),
    };
    let write = tokio::time::timeout(UPSTREAM_TIMEOUT, stream.write_all(request_bytes)).await;
    match write {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(format!("write: {}", e)),
        Err(_) => return Err("write timed out".to_string()),
    }
    let read = tokio::time::timeout(UPSTREAM_TIMEOUT, read_ws_handshake(&mut stream)).await;
    match read {
        Ok(Ok(Some(head))) => Ok((stream, head)),
        Ok(Ok(None)) => Err("upstream closed the connection".to_string()),
        Ok(Err(e)) => Err(format!("read: {}", e)),
        Err(_) => Err("handshake timed out".to_string()),
    }
}

/// Reads an HTTP/1.1 response head (status line + headers) from a stream.
///
/// Returns `None` if the connection closed before the head was complete.
async fn read_ws_handshake(stream: &mut TcpStream) -> std::io::Result<Option<ResponseHead>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 8192 {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
    }
    Ok(parse_http_response_head(&buf))
}

/// Parses the status line and headers of an HTTP/1.1 response head.
fn parse_http_response_head(buf: &[u8]) -> Option<ResponseHead> {
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = &buf[..head_end];
    let mut lines = head.split(|b| *b == b'\n').filter(|l| !l.is_empty());
    let status_line = lines.next()?;
    let status: u16 = String::from_utf8_lossy(status_line)
        .split(' ')
        .nth(1)?
        .trim()
        .parse()
        .ok()?;
    let mut headers = Vec::new();
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(idx) = line.iter().position(|b| *b == b':') {
            let key = String::from_utf8_lossy(&line[..idx]).trim().to_lowercase();
            let value: Vec<u8> = line[idx + 1..]
                .iter()
                .copied()
                .skip_while(|b| *b == b' ')
                .collect();
            headers.push((key, value));
        }
    }
    Some((status, headers))
}

async fn proxy_ws_pipe(
    mut client: TokioIo<Upgraded>,
    mut upstream: TcpStream,
    log_entry: LogEntry,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    max_log_entries: usize,
) {
    let start = Instant::now();
    let (mut cr, mut cw) = tokio::io::split(&mut client);
    let (mut ur, mut uw) = upstream.split();

    let c2u = tokio::io::copy(&mut cr, &mut uw);
    let u2c = tokio::io::copy(&mut ur, &mut cw);

    tokio::select! {
        _ = c2u => {},
        _ = u2c => {},
    }

    let ms = start.elapsed().as_millis() as u64;
    let mut lk = logs.lock().expect("mutex poisoned");
    lk.push_back(LogEntry {
        method: log_entry.method,
        path: log_entry.path,
        upstream: log_entry.upstream,
        status: 101,
        latency_ms: ms,
        ws: true,
    });
    if lk.len() > max_log_entries {
        lk.pop_front();
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_ws(
    mut req: Request<hyper::body::Incoming>,
    route: &RouteEntry,
    suffix: &str,
    query: Option<&str>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    max_log_entries: usize,
    method: String,
    path: String,
) -> Result<Response<BoxedBody>, std::convert::Infallible> {
    let upstream_host = route
        .upstream
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .to_string();
    let suffix = if suffix.is_empty() { "/" } else { suffix };

    let request_bytes = build_ws_request(suffix, query, req.headers(), &upstream_host);

    let upstream_str = format!("ws://{}{}", upstream_host, suffix);
    let log_entry = LogEntry {
        method,
        path,
        upstream: upstream_str,
        status: 0,
        latency_ms: 0,
        ws: true,
    };

    // Complete the upstream handshake before accepting the client's upgrade so
    // failures surface as a 502 instead of a half-open tunnel.
    match connect_ws_upstream(&upstream_host, &request_bytes).await {
        Ok((upstream, (status, headers))) => {
            if status != 101 {
                let mut lk = logs.lock().expect("mutex poisoned");
                lk.push_back(LogEntry {
                    status: 502,
                    ..log_entry
                });
                if lk.len() > max_log_entries {
                    lk.pop_front();
                }
                return Ok(Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(body_full(Bytes::from(format!(
                        "upstream rejected websocket upgrade (HTTP {status})"
                    ))))
                    .expect("response builder failed"));
            }

            // Relay the upstream's handshake headers (Sec-WebSocket-Accept,
            // subprotocol, extensions) back to the client.
            let mut builder = Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(hyper::header::CONNECTION, "Upgrade")
                .header(hyper::header::UPGRADE, "websocket");
            for (key, value) in headers {
                if key != "sec-websocket-accept"
                    && key != "sec-websocket-protocol"
                    && key != "sec-websocket-extensions"
                {
                    continue;
                }
                if let (Ok(k), Ok(v)) = (
                    hyper::header::HeaderName::from_bytes(key.as_bytes()),
                    hyper::header::HeaderValue::from_bytes(&value),
                ) {
                    builder = builder.header(k, v);
                }
            }

            // Request the connection upgrade first; it resolves once this
            // response has been written to the client.
            let on_upgrade = hyper::upgrade::on(&mut req);
            let le = log_entry.clone();
            let l = logs.clone();
            tokio::spawn(async move {
                match on_upgrade.await {
                    Ok(upgraded) => {
                        proxy_ws_pipe(TokioIo::new(upgraded), upstream, le, l, max_log_entries)
                            .await;
                    }
                    Err(_) => {
                        // Client disconnected before the upgrade completed.
                        let mut lk = l.lock().expect("mutex poisoned");
                        lk.push_back(LogEntry { status: 502, ..le });
                        if lk.len() > max_log_entries {
                            lk.pop_front();
                        }
                    }
                }
            });

            Ok(builder
                .body(body_full(Bytes::new()))
                .expect("response builder failed"))
        }
        Err(e) => {
            let mut lk = logs.lock().expect("mutex poisoned");
            lk.push_back(LogEntry {
                upstream: format!("ws connect error: {}", e),
                status: 502,
                ..log_entry
            });
            if lk.len() > max_log_entries {
                lk.pop_front();
            }
            Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(body_full(Bytes::from(format!(
                    "websocket connect failed: {e}"
                ))))
                .expect("response builder failed"))
        }
    }
}

async fn handle_http(
    ctx: HttpRequestContext<'_>,
) -> Result<Response<BoxedBody>, std::convert::Infallible> {
    let start = Instant::now();
    let incoming_headers = ctx.req.headers().clone();
    let incoming_host = ctx
        .req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let body_bytes = match ctx.req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            let ms = start.elapsed().as_millis() as u64;
            let mut lk = ctx.logs.lock().expect("mutex poisoned");
            lk.push_back(LogEntry {
                method: ctx.method,
                path: ctx.path,
                upstream: String::new(),
                status: 400,
                latency_ms: ms,
                ws: false,
            });
            if lk.len() > ctx.max_log_entries {
                lk.pop_front();
            }
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(body_full(Bytes::from("bad request")))
                .expect("response builder failed"));
        }
    };

    let upstream_str = format!("{}{}", ctx.route.upstream.trim_end_matches('/'), ctx.suffix);
    let upstream_uri = match build_upstream_uri(&ctx.route.upstream, ctx.suffix, ctx.query) {
        Ok(uri) => uri,
        Err(e) => {
            let mut lk = ctx.logs.lock().expect("mutex poisoned");
            lk.push_back(LogEntry {
                method: ctx.method,
                path: ctx.path,
                upstream: upstream_str.clone(),
                status: 502,
                latency_ms: start.elapsed().as_millis() as u64,
                ws: false,
            });
            if lk.len() > ctx.max_log_entries {
                lk.pop_front();
            }
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(body_full(Bytes::from(e)))
                .expect("response builder failed"));
        }
    };

    let mut forwarded_headers = forward_headers(&incoming_headers);
    add_forwarded_headers(
        &mut forwarded_headers,
        &ctx.peer_ip,
        incoming_host.as_deref(),
        ctx.is_tls,
    );

    let mut builder = Request::builder()
        .method(&ctx.method as &str)
        .uri(upstream_uri);
    for (k, v) in &forwarded_headers {
        builder = builder.header(k, v);
    }
    let forward_req = builder
        .body(Full::new(body_bytes))
        .expect("request builder failed");

    match ctx.client.request(forward_req).await {
        Ok(upstream_resp) => {
            let (parts, body) = upstream_resp.into_parts();
            let status = parts.status.as_u16();
            let ms = start.elapsed().as_millis() as u64;

            // Stream the upstream body through unchanged (SSE, long-polling and
            // large downloads work) and strip hop-by-hop response headers.
            // Also strip headers listed in the upstream's Connection header.
            let mut connection_tokens = std::collections::HashSet::new();
            for v in parts.headers.get_all(hyper::header::CONNECTION).iter() {
                if let Ok(s) = v.to_str() {
                    for token in s.split(',') {
                        let t = token.trim().to_ascii_lowercase();
                        if !t.is_empty() {
                            connection_tokens.insert(t);
                        }
                    }
                }
            }
            let mut resp_builder = Response::builder().status(parts.status);
            for (k, v) in &parts.headers {
                let k_lower = k.as_str().to_ascii_lowercase();
                if is_hop_by_hop_response(k) || connection_tokens.contains(&k_lower) {
                    continue;
                }
                resp_builder = resp_builder.header(k.clone(), v.clone());
            }
            let resp = resp_builder
                .body(body.boxed_unsync())
                .expect("response builder failed");

            let mut lk = ctx.logs.lock().expect("mutex poisoned");
            lk.push_back(LogEntry {
                method: ctx.method,
                path: ctx.path,
                upstream: upstream_str,
                status,
                latency_ms: ms,
                ws: false,
            });
            if lk.len() > ctx.max_log_entries {
                lk.pop_front();
            }

            Ok(resp)
        }
        Err(e) => {
            let ms = start.elapsed().as_millis() as u64;
            let mut lk = ctx.logs.lock().expect("mutex poisoned");
            lk.push_back(LogEntry {
                method: ctx.method,
                path: ctx.path,
                upstream: upstream_str,
                status: 502,
                latency_ms: ms,
                ws: false,
            });
            if lk.len() > ctx.max_log_entries {
                lk.pop_front();
            }

            Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(body_full(Bytes::from(format!("upstream error: {}", e))))
                .expect("response builder failed"))
        }
    }
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    client: Client<HttpConnector, Full<Bytes>>,
    routes: Vec<RouteEntry>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    max_log_entries: usize,
    peer_ip: String,
    is_tls: bool,
) -> Result<Response<BoxedBody>, std::convert::Infallible> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok());

    let matched = routes
        .iter()
        .filter(|r| match_host(host, &r.host))
        .find_map(|r| match_route(&path, &r.path).map(|suffix| (r, suffix.clone())));

    match matched {
        Some((route, suffix)) if is_ws_upgrade(&req) => {
            handle_ws(
                req,
                route,
                &suffix,
                query.as_deref(),
                logs,
                max_log_entries,
                method,
                path,
            )
            .await
        }
        Some((route, suffix)) => {
            let ctx = HttpRequestContext {
                req,
                client,
                route,
                suffix: &suffix,
                query: query.as_deref(),
                logs,
                max_log_entries,
                method,
                path,
                peer_ip,
                is_tls,
            };
            handle_http(ctx).await
        }
        None => {
            {
                let mut lk = logs.lock().expect("mutex poisoned");
                lk.push_back(LogEntry {
                    method,
                    path,
                    upstream: "-".into(),
                    status: 404,
                    latency_ms: 0,
                    ws: false,
                });
                if lk.len() > max_log_entries {
                    lk.pop_front();
                }
            }

            // Drain the request body so hyper can keep the connection alive for
            // the next request instead of dropping it.
            let _ = req.collect().await;

            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(body_full(Bytes::from("no matching route")))
                .expect("response builder failed"))
        }
    }
}

impl Drop for ProxyInstance {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use hyper::Request;
    use hyper::body::Bytes;

    // --- wildcard_match ---

    #[test]
    fn test_wildcard_match_exact() {
        assert!(wildcard_match("hello", "hello"));
    }

    #[test]
    fn test_wildcard_match_star() {
        assert!(wildcard_match("he*lo", "hello"));
    }

    #[test]
    fn test_wildcard_match_star_prefix() {
        assert!(wildcard_match("*lo", "hello"));
    }

    #[test]
    fn test_wildcard_match_star_suffix() {
        assert!(wildcard_match("he*", "hello"));
    }

    #[test]
    fn test_wildcard_match_star_all() {
        assert!(wildcard_match("*", "anything"));
    }

    #[test]
    fn test_wildcard_match_no_match() {
        assert!(!wildcard_match("hello", "world"));
    }

    #[test]
    fn test_wildcard_match_empty() {
        assert!(wildcard_match("", ""));
    }

    #[test]
    fn test_wildcard_match_star_empty() {
        assert!(wildcard_match("*", ""));
    }

    #[test]
    fn test_wildcard_match_multi_star() {
        assert!(wildcard_match("a*b*c", "axbyc"));
    }

    // --- match_route (prefix) ---

    #[test]
    fn test_match_route_exact() {
        assert_eq!(match_route("/api", "/api"), Some("/".to_string()));
    }

    #[test]
    fn test_match_route_prefix() {
        assert_eq!(match_route("/api/test", "/api"), Some("/test".to_string()));
    }

    #[test]
    fn test_match_route_no_match() {
        assert_eq!(match_route("/other", "/api"), None);
    }

    #[test]
    fn test_match_route_root() {
        assert_eq!(match_route("/", "/"), Some("/".to_string()));
    }

    #[test]
    fn test_match_route_trailing_slash_incoming() {
        assert_eq!(match_route("/api/", "/api"), Some("/".to_string()));
    }

    #[test]
    fn test_match_route_trailing_slash_route() {
        assert_eq!(match_route("/api", "/api/"), Some("/".to_string()));
    }

    #[test]
    fn test_match_route_empty_prefix() {
        let result = match_route("/anything", "");
        assert!(result.is_some());
    }

    #[test]
    fn test_match_route_empty_prefix_no_slash() {
        let result = match_route("test", "");
        assert_eq!(result, None);
    }

    #[test]
    fn test_match_route_prefix_with_trailing_slash() {
        assert_eq!(
            match_route("/api/test/", "/api"),
            Some("/test/".to_string())
        );
    }

    // --- match_route (wildcard) ---

    #[test]
    fn test_match_route_wildcard_single_segment() {
        assert_eq!(
            match_route("/api/foo", "/api/*"),
            Some("/api/foo".to_string())
        );
    }

    #[test]
    fn test_match_route_wildcard_mid_segment() {
        assert_eq!(
            match_route("/api/foo/bar", "/api/*/bar"),
            Some("/api/foo/bar".to_string())
        );
    }

    #[test]
    fn test_match_route_wildcard_no_match_length() {
        assert_eq!(match_route("/api/foo/bar", "/api/*"), None);
    }

    #[test]
    fn test_match_route_wildcard_no_match_segment() {
        assert_eq!(match_route("/api/foo", "/api/bar"), None);
    }

    #[test]
    fn test_match_route_wildcard_partial_segment() {
        assert_eq!(
            match_route("/api/v2/users", "/api/v*/users"),
            Some("/api/v2/users".to_string())
        );
    }

    #[test]
    fn test_match_route_wildcard_segment_star() {
        assert_eq!(
            match_route("/static/js/main.js", "/*/js/main.js"),
            Some("/static/js/main.js".to_string())
        );
    }

    // --- match_host ---

    #[test]
    fn test_match_host_none_route_no_host() {
        assert!(match_host(None, &None));
    }

    #[test]
    fn test_match_host_with_host_matches() {
        assert!(match_host(
            Some("custom.com"),
            &Some("custom.*".to_string())
        ));
    }

    #[test]
    fn test_match_host_with_host_no_match() {
        assert!(!match_host(
            Some("other.com"),
            &Some("custom.*".to_string())
        ));
    }

    #[test]
    fn test_match_host_with_host_and_port() {
        assert!(match_host(
            Some("custom.com:8080"),
            &Some("custom.*".to_string())
        ));
    }

    #[test]
    fn test_match_host_no_incoming_with_pattern() {
        assert!(!match_host(None, &Some("custom.*".to_string())));
    }

    #[test]
    fn test_match_host_subdomain_wildcard() {
        assert!(match_host(
            Some("api.example.com"),
            &Some("*.example.com".to_string())
        ));
    }

    #[test]
    fn test_match_host_subdomain_no_match() {
        assert!(!match_host(
            Some("other.com"),
            &Some("*.example.com".to_string())
        ));
    }

    // --- is_ws_upgrade ---

    #[test]
    fn test_is_ws_upgrade_with_headers() {
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::CONNECTION, "Upgrade")
            .body(Full::<Bytes>::new(Bytes::new()))
            .expect("request builder should succeed");
        assert!(is_ws_upgrade(&req));
    }

    #[test]
    fn test_is_ws_upgrade_without_headers() {
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Full::<Bytes>::new(Bytes::new()))
            .expect("request builder should succeed");
        assert!(!is_ws_upgrade(&req));
    }

    #[test]
    fn test_is_ws_upgrade_regular_get() {
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Full::<Bytes>::new(Bytes::new()))
            .expect("request builder should succeed");
        assert!(!is_ws_upgrade(&req));
    }

    #[test]
    fn test_is_ws_upgrade_post() {
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::CONNECTION, "Upgrade")
            .body(Full::<Bytes>::new(Bytes::new()))
            .expect("request builder should succeed");
        assert!(!is_ws_upgrade(&req));
    }

    #[test]
    fn test_is_ws_upgrade_case_insensitive() {
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .header(hyper::header::UPGRADE, "WebSocket")
            .header(hyper::header::CONNECTION, "keep-alive, Upgrade")
            .body(Full::<Bytes>::new(Bytes::new()))
            .expect("request builder should succeed");
        assert!(is_ws_upgrade(&req));
    }

    // --- build_upstream_uri ---

    #[test]
    fn test_build_upstream_uri_no_query() {
        let uri = build_upstream_uri("http://localhost:8080", "/api/test", None).unwrap();
        assert_eq!(uri.to_string(), "http://localhost:8080/api/test");
    }

    #[test]
    fn test_build_upstream_uri_with_query() {
        let uri =
            build_upstream_uri("http://localhost:8080", "/api/test", Some("key=value")).unwrap();
        assert_eq!(uri.to_string(), "http://localhost:8080/api/test?key=value");
    }

    #[test]
    fn test_build_upstream_uri_trailing_slash() {
        let uri = build_upstream_uri("http://localhost:8080/", "/api/test", None).unwrap();
        assert_eq!(uri.to_string(), "http://localhost:8080/api/test");
    }

    #[test]
    fn test_build_upstream_uri_empty_query() {
        let uri = build_upstream_uri("http://localhost:8080", "/api/test", Some("")).unwrap();
        assert_eq!(uri.to_string(), "http://localhost:8080/api/test");
    }

    #[test]
    fn test_build_upstream_uri_suffix_root() {
        let uri = build_upstream_uri("http://localhost:8080", "/", None).unwrap();
        assert_eq!(uri.to_string(), "http://localhost:8080/");
    }

    #[test]
    fn test_build_upstream_uri_invalid_fails() {
        assert!(build_upstream_uri("http://", "/x", None).is_err());
    }

    // --- forward_headers (RFC 7230 §6.1) ---

    #[test]
    fn test_forward_headers_strips_connection_listed() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::CONNECTION,
            "keep-alive, X-Custom".parse().unwrap(),
        );
        headers.insert("x-custom", "secret".parse().unwrap());
        headers.insert("x-keep", "should-pass".parse().unwrap());
        headers.insert(hyper::header::HOST, "example.com".parse().unwrap());
        let out = forward_headers(&headers);
        assert!(!out.contains_key("x-custom"), "X-Custom listed in Connection must be stripped");
        assert!(!out.contains_key(hyper::header::CONNECTION));
        assert!(!out.contains_key("keep-alive"), "keep-alive is hop-by-hop");
        assert!(out.contains_key("x-keep"), "unlisted header must be forwarded");
        assert!(!out.contains_key(hyper::header::HOST), "host is hop-by-hop");
    }

    #[test]
    fn test_forward_headers_case_insensitive_connection_tokens() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::CONNECTION,
            "X-Custom".parse().unwrap(),
        );
        headers.insert("x-custom", "secret".parse().unwrap());
        headers.insert("x-other", "ok".parse().unwrap());
        let out = forward_headers(&headers);
        assert!(!out.contains_key("x-custom"));
        assert!(out.contains_key("x-other"));
    }

    #[test]
    fn test_forward_headers_multiple_connection_headers() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::CONNECTION,
            "keep-alive".parse().unwrap(),
        );
        headers.append(
            hyper::header::CONNECTION,
            "X-Custom".parse().unwrap(),
        );
        headers.insert("x-custom", "secret".parse().unwrap());
        headers.insert("x-keep", "pass".parse().unwrap());
        let out = forward_headers(&headers);
        assert!(!out.contains_key("x-custom"));
        assert!(out.contains_key("x-keep"));
        assert!(!out.contains_key(hyper::header::CONNECTION));
    }

    #[test]
    fn test_forward_headers_preserves_non_hop_headers() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("x-normal", "value".parse().unwrap());
        headers.insert(hyper::header::ACCEPT, "text/html".parse().unwrap());
        let out = forward_headers(&headers);
        assert!(out.contains_key("x-normal"));
        assert!(out.contains_key(hyper::header::ACCEPT));
    }

    #[test]
    fn test_forward_headers_strips_response_connection_tokens() {
        // Simulate upstream response header filtering done in handle_http.
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::CONNECTION,
            "X-Upstream-Custom".parse().unwrap(),
        );
        headers.insert("x-upstream-custom", "secret".parse().unwrap());
        headers.insert("x-normal", "keep".parse().unwrap());
        // Mirror the logic in handle_http: build token set from Connection header.
        let mut connection_tokens = std::collections::HashSet::new();
        for v in headers.get_all(hyper::header::CONNECTION).iter() {
            if let Ok(s) = v.to_str() {
                for token in s.split(',') {
                    let t = token.trim().to_ascii_lowercase();
                    if !t.is_empty() {
                        connection_tokens.insert(t);
                    }
                }
            }
        }
        let mut filtered = hyper::HeaderMap::new();
        for (k, v) in &headers {
            let k_lower = k.as_str().to_ascii_lowercase();
            if is_hop_by_hop_response(k) || connection_tokens.contains(&k_lower) {
                continue;
            }
            filtered.append(k.clone(), v.clone());
        }
        assert!(!filtered.contains_key("x-upstream-custom"), "listed in Connection must be stripped from response");
        assert!(filtered.contains_key("x-normal"));
        assert!(!filtered.contains_key(hyper::header::CONNECTION));
    }
}
