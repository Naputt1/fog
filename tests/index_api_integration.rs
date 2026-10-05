//! Integration tests for the embedded index server's HTTP API, driven over a
//! real TCP socket via `fog::index::serve_for_test`.
//!
//! One server instance is shared by every test in this binary (started lazily
//! on a free port) so the suite does not leak a thread and a port per test.
//! Assertions target status codes, content types, and JSON *shape* — never
//! container/service names — so they stay green without Docker or a git repo.

use std::net::TcpListener;
use std::sync::OnceLock;
use std::time::Duration;

/// Starts the index server once per test binary and returns its port.
fn server_port() -> u16 {
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::thread::spawn(move || {
            let _ = fog::index::serve_for_test(port);
        });
        // `serve_for_test` returns only after binding, but the thread is
        // spawned asynchronously, so poll until the listener answers.
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return port;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("index test server did not bind on port {port}");
    })
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn url(path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", server_port())
}

fn content_type(resp: &reqwest::blocking::Response) -> String {
    resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[test]
fn test_spa_fallback_root_serves_html() {
    let resp = client().get(url("/")).send().unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        content_type(&resp).contains("text/html"),
        "root must serve HTML, got {}",
        content_type(&resp)
    );
}

#[test]
fn test_spa_fallback_client_route_serves_html() {
    // Unknown non-API paths fall through to the SPA entry, not a 404.
    let resp = client().get(url("/projects/demo/main/dev")).send().unwrap();
    assert_eq!(resp.status(), 200);
    assert!(content_type(&resp).contains("text/html"));
}

#[test]
fn test_api_status_ok() {
    let resp = client().get(url("/api/status")).send().unwrap();
    assert_eq!(resp.status(), 200);
    assert!(content_type(&resp).contains("application/json"));
}

#[test]
fn test_api_config_ok() {
    let resp = client().get(url("/api/config")).send().unwrap();
    assert_eq!(resp.status(), 200);
    assert!(content_type(&resp).contains("application/json"));
}

#[test]
fn test_api_health_ok() {
    let resp = client().get(url("/api/health")).send().unwrap();
    assert_eq!(resp.status(), 200);
    assert!(content_type(&resp).contains("application/json"));
}

#[test]
fn test_api_launch_targets_get_returns_object() {
    let resp = client().get(url("/api/launch/targets")).send().unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().unwrap();
    assert!(
        body.trim_start().starts_with('{') && body.contains("\"projects\""),
        "launch targets must be a JSON object with `projects`, got: {body}"
    );
}

#[test]
fn test_api_launch_targets_rejects_non_get() {
    let resp = client().post(url("/api/launch/targets")).send().unwrap();
    assert_eq!(resp.status(), 404);
}

#[test]
fn test_api_services_returns_array() {
    // Works with or without Docker: discovery degrades to an empty list.
    let resp = client().get(url("/api/services")).send().unwrap();
    assert_eq!(resp.status(), 200);
    assert!(content_type(&resp).contains("application/json"));
    let body = resp.text().unwrap();
    assert!(body.trim_start().starts_with('['), "got: {body}");
}

#[test]
fn test_api_services_with_internal_returns_array() {
    let resp = client()
        .get(url("/api/services?withInternal=1"))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().unwrap();
    assert!(body.trim_start().starts_with('['), "got: {body}");
}

#[test]
fn test_unknown_api_route_is_404_json() {
    let resp = client().get(url("/api/nope")).send().unwrap();
    assert_eq!(resp.status(), 404);
    assert!(content_type(&resp).contains("application/json"));
}

#[test]
fn test_action_route_wrong_method_is_404() {
    let resp = client()
        .get(url("/api/instances/1/services/web/action"))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[test]
fn test_action_route_invalid_pid_is_400() {
    let resp = client()
        .post(url("/api/instances/notanint/services/web/action"))
        .body("{}")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[test]
fn test_kill_route_invalid_pid_is_400() {
    let resp = client()
        .post(url("/api/instances/notanint/kill"))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[test]
fn test_logs_history_invalid_service_is_400() {
    // With a `pid` the handler validates the service name before touching IPC;
    // an empty name must be rejected up front.
    let resp = client()
        .get(url("/api/logs/history?pid=1&service="))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
}
