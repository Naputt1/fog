//! Integration tests for the built-in terminal WebSocket gateway
//! (`/ws/terminal`), exercised through a real `ProxyInstance` over raw TCP.
//!
//! The handshake tests speak RFC 6455 by hand (no tungstenite client) so they
//! can assert on the raw HTTP response head. They never wait for PTY output,
//! so they stay fast and are not flaky: the `101` test tears the connection
//! down immediately after the upgrade response, and the auth tests reject
//! before any PTY is spawned.

use fog::config::TerminalConfig;
use fog::proxy::ProxyInstance;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Connects to the proxy, retrying briefly: `start()` flips `is_running()`
/// before the listener is actually bound, so a single immediate connect can
/// race it.
fn connect_retry(port: u16) -> TcpStream {
    let mut last_err = None;
    for _ in 0..50 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return stream,
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("could not connect to proxy on port {port}: {last_err:?}");
}

fn find_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn empty_proxy(port: u16) -> ProxyInstance {
    // The terminal gateway is a built-in endpoint served before route
    // matching, so no routes are required to reach `/ws/terminal`.
    ProxyInstance::new(port, None, vec![], 1000, None, None)
}

/// Builds a minimal RFC 6455 upgrade request for `/ws/terminal`.
fn ws_upgrade_request(host: &str, query: Option<&str>) -> Vec<u8> {
    let target = match query {
        Some(q) => format!("/ws/terminal?{q}"),
        None => "/ws/terminal".to_string(),
    };
    format!(
        "GET {target} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
    .into_bytes()
}

/// Reads an HTTP/1.1 response head (status line + headers) as raw bytes.
fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    while buf.len() < 16 * 1024 {
        let n = stream.read(&mut byte).unwrap();
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
    }
    buf
}

fn status_line(head: &[u8]) -> String {
    let end = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(head.len());
    String::from_utf8_lossy(&head[..end])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn test_terminal_ws_101_handshake() {
    let port = find_free_port();
    let mut proxy = empty_proxy(port);
    proxy.start();

    let mut stream = connect_retry(port);
    stream
        .write_all(&ws_upgrade_request(&format!("127.0.0.1:{port}"), None))
        .unwrap();
    let head = read_head(&mut stream);

    let status = status_line(&head);
    assert!(
        status.contains("101"),
        "expected 101 Switching Protocols, got: {status}"
    );
    let head_str = String::from_utf8_lossy(&head);
    assert!(
        head_str.contains("sec-websocket-accept:"),
        "101 response must include Sec-WebSocket-Accept for RFC 6455 compliance"
    );

    // Tear down immediately without waiting for (or writing to) the PTY so
    // the test does not depend on shell availability or timing.
    drop(stream);
    proxy.stop();
}

#[test]
fn test_terminal_ws_401_without_auth_token() {
    let port = find_free_port();
    let cfg = TerminalConfig {
        auth_token: Some("s3cret".to_string()),
        ..TerminalConfig::default()
    };
    let mut proxy = empty_proxy(port).with_terminal_config(cfg);
    proxy.start();

    // No `auth_token` query parameter at all.
    let mut stream = connect_retry(port);
    stream
        .write_all(&ws_upgrade_request(&format!("127.0.0.1:{port}"), None))
        .unwrap();
    let head = read_head(&mut stream);
    assert!(
        status_line(&head).contains("401"),
        "missing auth_token must be rejected with 401, got: {}",
        status_line(&head)
    );
    drop(stream);
    proxy.stop();
}

#[test]
fn test_terminal_ws_401_with_wrong_auth_token() {
    let port = find_free_port();
    let cfg = TerminalConfig {
        auth_token: Some("s3cret".to_string()),
        ..TerminalConfig::default()
    };
    let mut proxy = empty_proxy(port).with_terminal_config(cfg);
    proxy.start();

    let mut stream = connect_retry(port);
    stream
        .write_all(&ws_upgrade_request(
            &format!("127.0.0.1:{port}"),
            Some("auth_token=wrong"),
        ))
        .unwrap();
    let head = read_head(&mut stream);
    assert!(
        status_line(&head).contains("401"),
        "a wrong auth_token must be rejected with 401, got: {}",
        status_line(&head)
    );
    drop(stream);
    proxy.stop();
}

#[test]
fn test_terminal_ws_101_with_valid_auth_token() {
    let port = find_free_port();
    let cfg = TerminalConfig {
        auth_token: Some("s3cret".to_string()),
        ..TerminalConfig::default()
    };
    let mut proxy = empty_proxy(port).with_terminal_config(cfg);
    proxy.start();

    let mut stream = connect_retry(port);
    stream
        .write_all(&ws_upgrade_request(
            &format!("127.0.0.1:{port}"),
            Some("auth_token=s3cret"),
        ))
        .unwrap();
    let head = read_head(&mut stream);
    assert!(
        status_line(&head).contains("101"),
        "valid auth_token must proceed to 101, got: {}",
        status_line(&head)
    );
    drop(stream);
    proxy.stop();
}
