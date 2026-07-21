use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::ServerConfig;
use rustls_pemfile::{certs, pkcs8_private_keys};
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

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

const MAX_LOG_ENTRIES: usize = 1000;

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
    let mut keys: Vec<rustls::pki_types::PrivateKeyDer<'_>> = pkcs8_private_keys(key_file)
        .filter_map(Result::ok)
        .map(|k| k.into())
        .collect();

    if keys.is_empty() {
        return Err("no private keys found".into());
    }

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, keys.remove(0))?;

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
    method: String,
    path: String,
}

pub struct ProxyInstance {
    pub port: u16,
    pub routes: Vec<RouteEntry>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    running: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    pub max_log_entries: usize,
    tls_acceptor: Option<TlsAcceptor>,
}

impl ProxyInstance {
    pub fn new(
        port: u16,
        routes: Vec<RouteEntry>,
        max_log_entries: usize,
        tls_cert: Option<String>,
        tls_key: Option<String>,
    ) -> Self {
        let tls_acceptor = match (tls_cert, tls_key) {
            (Some(cert), Some(key)) => match load_tls_config(&cert, &key) {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("warning: failed to load TLS config: {}", e);
                    None
                }
            },
            _ => None,
        };

        Self {
            port,
            routes,
            logs: Arc::new(Mutex::new(VecDeque::with_capacity(max_log_entries))),
            running: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            handle: None,
            max_log_entries,
            tls_acceptor,
        }
    }

    pub fn start(&mut self) {
        if self.running.load(Ordering::SeqCst) {
            return;
        }

        self.running.store(true, Ordering::SeqCst);
        self.shutdown.store(false, Ordering::SeqCst);

        let port = self.port;
        let routes = self.routes.clone();
        let logs = self.logs.clone();
        let running = self.running.clone();
        let shutdown = self.shutdown.clone();
        let tls_acceptor = self.tls_acceptor.clone();
        let handle = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("failed to build tokio runtime");

            rt.block_on(async move {
                let addr = format!("0.0.0.0:{}", port);
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
                        if lk.len() > MAX_LOG_ENTRIES {
                            lk.pop_front();
                        }
                        running.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let client: Client<HttpConnector, Full<Bytes>> =
                    Client::builder(TokioExecutor::new()).build_http();

                loop {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }

                    let accept =
                        tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept())
                            .await;

                    match accept {
                        Ok(Ok((stream, _))) => {
                            let client = client.clone();
                            let logs_for_svc = logs.clone();
                            let routes_for_svc = routes.clone();
                            let acceptor = tls_acceptor.clone();

                            tokio::spawn(async move {
                                let io = if let Some(ref acceptor) = acceptor {
                                    match acceptor.accept(stream).await {
                                        Ok(tls_stream) => {
                                            TokioIo::new(Box::new(tls_stream) as Box<dyn IoBox>)
                                        }
                                        Err(_) => return,
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
                                    )
                                });
                                if let Err(e) =
                                    http1::Builder::new().serve_connection(io, svc).await
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
}

fn match_route(incoming: &str, route: &str) -> Option<String> {
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

fn build_upstream_uri(upstream: &str, suffix: &str, query: Option<&str>) -> hyper::Uri {
    let base = upstream.trim_end_matches('/');
    let path = format!("{}{}", base, suffix);
    let s = match query {
        Some(q) if !q.is_empty() => format!("{}?{}", path, q),
        _ => path,
    };
    s.parse().unwrap_or_else(|_| {
        format!("{}/", base)
            .parse()
            .expect("built URI should parse")
    })
}

fn forward_headers(incoming: &hyper::HeaderMap) -> hyper::HeaderMap {
    let mut out = hyper::HeaderMap::new();
    for (key, value) in incoming.iter() {
        let k = key.as_str().to_lowercase();
        if !HOP_BY_HOP.contains(&k.as_str()) {
            out.insert(key.clone(), value.clone());
        }
    }
    out
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

async fn proxy_ws_pipe(
    mut client: TokioIo<Upgraded>,
    upstream_host: String,
    request_bytes: Vec<u8>,
    log_entry: LogEntry,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
) {
    let start = Instant::now();
    match tokio::net::TcpStream::connect(&upstream_host).await {
        Ok(mut upstream) => {
            if upstream.write_all(&request_bytes).await.is_err() {
                return;
            }

            let (mut cr, mut cw) = tokio::io::split(&mut client);
            let (mut ur, mut uw) = upstream.split();

            let c2u = tokio::io::copy(&mut cr, &mut uw);
            let u2c = tokio::io::copy(&mut ur, &mut cw);

            tokio::select! {
                _ = c2u => {},
                _ = u2c => {},
            }
        }
        Err(e) => {
            let mut lk = logs.lock().expect("mutex poisoned");
            lk.push_back(LogEntry {
                method: log_entry.method.clone(),
                path: log_entry.path.clone(),
                upstream: format!("ws connect error: {}", e),
                status: 502,
                latency_ms: start.elapsed().as_millis() as u64,
                ws: true,
            });
            if lk.len() > MAX_LOG_ENTRIES {
                lk.pop_front();
            }
            return;
        }
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
    if lk.len() > MAX_LOG_ENTRIES {
        lk.pop_front();
    }
}

async fn handle_ws(
    mut req: Request<hyper::body::Incoming>,
    route: &RouteEntry,
    suffix: &str,
    query: Option<&str>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
    method: String,
    path: String,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
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

    match hyper::upgrade::on(&mut req).await {
        Ok(upgraded) => {
            let l = logs.clone();
            let le = log_entry.clone();
            let uh = upstream_host.clone();

            tokio::spawn(async move {
                proxy_ws_pipe(TokioIo::new(upgraded), uh, request_bytes, le, l).await;
            });

            Ok(Response::new(Full::new(Bytes::new())))
        }
        Err(_) => {
            let mut lk = logs.lock().expect("mutex poisoned");
            lk.push_back(LogEntry {
                status: 502,
                ..log_entry
            });
            if lk.len() > MAX_LOG_ENTRIES {
                lk.pop_front();
            }

            Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::from("websocket upgrade failed")))
                .expect("response builder failed"))
        }
    }
}

async fn handle_http(
    ctx: HttpRequestContext<'_>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let start = Instant::now();
    let incoming_headers = ctx.req.headers().clone();

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
            if lk.len() > MAX_LOG_ENTRIES {
                lk.pop_front();
            }
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from("bad request")))
                .expect("response builder failed"));
        }
    };

    let upstream_str = format!("{}{}", ctx.route.upstream.trim_end_matches('/'), ctx.suffix);
    let upstream_uri = build_upstream_uri(&ctx.route.upstream, ctx.suffix, ctx.query);

    let forwarded_headers = forward_headers(&incoming_headers);

    let mut builder = Request::builder()
        .method(&ctx.method as &str)
        .uri(upstream_uri.clone());
    for (k, v) in &forwarded_headers {
        builder = builder.header(k, v);
    }
    let forward_req = builder
        .body(Full::new(body_bytes))
        .expect("request builder failed");

    match ctx.client.request(forward_req).await {
        Ok(upstream_resp) => {
            let (parts, body) = upstream_resp.into_parts();
            let resp_body = match body.collect().await {
                Ok(c) => c.to_bytes(),
                Err(_) => Bytes::new(),
            };
            let status = parts.status.as_u16();
            let ms = start.elapsed().as_millis() as u64;

            let mut resp_builder = Response::builder().status(parts.status);
            for (k, v) in &parts.headers {
                resp_builder = resp_builder.header(k, v);
            }
            let resp = resp_builder
                .body(Full::new(resp_body))
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
            if lk.len() > MAX_LOG_ENTRIES {
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
            if lk.len() > MAX_LOG_ENTRIES {
                lk.pop_front();
            }

            Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::from(format!("upstream error: {}", e))))
                .expect("response builder failed"))
        }
    }
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    client: Client<HttpConnector, Full<Bytes>>,
    routes: Vec<RouteEntry>,
    logs: Arc<Mutex<VecDeque<LogEntry>>>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());

    let matched = routes
        .iter()
        .find_map(|r| match_route(&path, &r.path).map(|suffix| (r, suffix.clone())));

    match matched {
        Some((route, suffix)) if route.ws || is_ws_upgrade(&req) => {
            handle_ws(req, route, &suffix, query.as_deref(), logs, method, path).await
        }
        Some((route, suffix)) => {
            let ctx = HttpRequestContext {
                req,
                client,
                route,
                suffix: &suffix,
                query: query.as_deref(),
                logs,
                method,
                path,
            };
            handle_http(ctx).await
        }
        None => {
            let mut lk = logs.lock().expect("mutex poisoned");
            lk.push_back(LogEntry {
                method,
                path,
                upstream: "-".into(),
                status: 404,
                latency_ms: 0,
                ws: false,
            });
            if lk.len() > MAX_LOG_ENTRIES {
                lk.pop_front();
            }

            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new(Bytes::from("no matching route")))
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
    fn test_is_ws_upgrade_with_headers() {
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::CONNECTION, "Upgrade")
            .body(Full::<Bytes>::new(Bytes::new()))
            .unwrap();
        assert!(is_ws_upgrade(&req));
    }

    #[test]
    fn test_is_ws_upgrade_without_headers() {
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Full::<Bytes>::new(Bytes::new()))
            .unwrap();
        assert!(!is_ws_upgrade(&req));
    }

    #[test]
    fn test_is_ws_upgrade_regular_get() {
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Full::<Bytes>::new(Bytes::new()))
            .unwrap();
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
            .unwrap();
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
            .unwrap();
        assert!(is_ws_upgrade(&req));
    }

    #[test]
    fn test_build_upstream_uri_no_query() {
        let uri = build_upstream_uri("http://localhost:8080", "/api/test", None);
        assert_eq!(uri.to_string(), "http://localhost:8080/api/test");
    }

    #[test]
    fn test_build_upstream_uri_with_query() {
        let uri = build_upstream_uri("http://localhost:8080", "/api/test", Some("key=value"));
        assert_eq!(uri.to_string(), "http://localhost:8080/api/test?key=value");
    }

    #[test]
    fn test_build_upstream_uri_trailing_slash() {
        let uri = build_upstream_uri("http://localhost:8080/", "/api/test", None);
        assert_eq!(uri.to_string(), "http://localhost:8080/api/test");
    }

    #[test]
    fn test_build_upstream_uri_empty_query() {
        let uri = build_upstream_uri("http://localhost:8080", "/api/test", Some(""));
        assert_eq!(uri.to_string(), "http://localhost:8080/api/test");
    }

    #[test]
    fn test_build_upstream_uri_suffix_root() {
        let uri = build_upstream_uri("http://localhost:8080", "/", None);
        assert_eq!(uri.to_string(), "http://localhost:8080/");
    }

    #[test]
    fn test_match_route_prefix_with_trailing_slash() {
        assert_eq!(
            match_route("/api/test/", "/api"),
            Some("/test/".to_string())
        );
    }
}
