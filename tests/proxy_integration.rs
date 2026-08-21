use fog::proxy::{ProxyInstance, RouteEntry};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn find_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn test_proxy_starts_and_stops() {
    let routes = vec![RouteEntry {
        path: "/api".into(),
        host: None,
        upstream: "http://127.0.0.1:19999/api".into(),
        ws: false,
    }];
    let mut proxy = ProxyInstance::new(19998, None, routes, 1000, None, None);
    proxy.start();
    assert!(proxy.is_running());
    proxy.stop();
    assert!(!proxy.is_running());
}

#[test]
fn test_proxy_returns_404_for_unmatched_route() {
    let routes = vec![RouteEntry {
        path: "/api".into(),
        host: None,
        upstream: "http://127.0.0.1:19999".into(),
        ws: false,
    }];
    let mut proxy = ProxyInstance::new(19997, None, routes, 1000, None, None);
    proxy.start();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let resp = client.get("http://127.0.0.1:19997/other").send().unwrap();
    assert_eq!(resp.status(), 404);

    proxy.stop();
}

#[test]
fn test_proxy_returns_502_for_unreachable_upstream() {
    let routes = vec![RouteEntry {
        path: "/".into(),
        host: None,
        upstream: "http://127.0.0.1:1".into(),
        ws: false,
    }];
    let mut proxy = ProxyInstance::new(19996, None, routes, 1000, None, None);
    proxy.start();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();
    let resp = client.get("http://127.0.0.1:19996/test").send();
    if let Ok(r) = resp {
        assert_eq!(r.status(), 502);
    }

    proxy.stop();
}

#[test]
fn test_proxy_records_logs() {
    let routes = vec![RouteEntry {
        path: "/other".into(),
        host: None,
        upstream: "http://127.0.0.1:19999".into(),
        ws: false,
    }];
    let mut proxy = ProxyInstance::new(19995, None, routes, 1000, None, None);
    proxy.start();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let _ = client.get("http://127.0.0.1:19995/nonexistent").send();

    std::thread::sleep(std::time::Duration::from_millis(100));

    let logs = proxy.get_logs();
    assert!(
        !logs.is_empty(),
        "Proxy should have recorded at least one log entry"
    );

    proxy.stop();
}

#[test]
fn test_proxy_restart() {
    let routes = vec![RouteEntry {
        path: "/api".into(),
        host: None,
        upstream: "http://127.0.0.1:19999/api".into(),
        ws: false,
    }];
    let mut proxy = ProxyInstance::new(19994, None, routes, 1000, None, None);
    proxy.start();
    assert!(proxy.is_running());

    proxy.restart();
    assert!(proxy.is_running());

    proxy.stop();
    assert!(!proxy.is_running());
}

#[test]
fn test_proxy_strips_connection_listed_request_headers() {
    // Upstream that records the raw request headers.
    let upstream_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream_listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = upstream_listener.accept() {
            let mut buf = vec![0u8; 8192];
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let lower = req.to_ascii_lowercase();
            // Parse header lines to avoid matching token inside Connection value.
            let has_x_custom = lower
                .split("\r\n")
                .any(|l| l.starts_with("x-custom:"));
            let has_x_keep = lower
                .split("\r\n")
                .any(|l| l.starts_with("x-keep:"));
            let has_connection = lower
                .split("\r\n")
                .any(|l| l.starts_with("connection:"));
            let has_keep_alive_header = lower
                .split("\r\n")
                .any(|l| l.starts_with("keep-alive:"));
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = tx.send((has_x_custom, has_x_keep, has_connection, has_keep_alive_header, req));
        }
    });

    let proxy_port = find_free_port();
    let routes = vec![RouteEntry {
        path: "/".into(),
        host: None,
        upstream: format!("http://127.0.0.1:{upstream_port}"),
        ws: false,
    }];
    let mut proxy = ProxyInstance::new(proxy_port, None, routes, 100, None, None);
    proxy.start();
    // Wait for proxy to bind.
    thread::sleep(Duration::from_millis(200));
    assert!(proxy.is_running(), "proxy should be running");

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://127.0.0.1:{proxy_port}/test"))
        .header("Connection", "keep-alive, X-Custom")
        .header("X-Custom", "secret")
        .header("X-Keep", "pass")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);

    let (has_x_custom, has_x_keep, has_connection, has_keep_alive_header, req) =
        rx.recv_timeout(Duration::from_secs(3))
            .expect("upstream should have received request");

    assert!(
        !has_x_custom,
        "X-Custom listed in Connection must be stripped (RFC 7230), upstream got: {req}"
    );
    assert!(
        has_x_keep,
        "X-Keep (not listed) must be forwarded, upstream got: {req}"
    );
    assert!(
        !has_connection,
        "Connection header itself must be stripped (hop-by-hop), upstream got: {req}"
    );
    assert!(
        !has_keep_alive_header,
        "keep-alive is hop-by-hop and must be stripped, upstream got: {req}"
    );

    proxy.stop();
}

#[test]
fn test_proxy_strips_connection_listed_response_headers() {
    // Upstream that returns a response with Connection: X-Upstream-Custom
    // and a header X-Upstream-Custom that must be stripped, plus X-Normal that must pass.
    let upstream_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_port = upstream_listener.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = upstream_listener.accept() {
            let mut buf = vec![0u8; 4096];
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let _ = stream.read(&mut buf);
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Length: 2\r\n",
                "Connection: X-Upstream-Custom\r\n",
                "X-Upstream-Custom: secret\r\n",
                "X-Normal: keep\r\n",
                "Keep-Alive: timeout=5\r\n",
                "\r\n",
                "ok"
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let proxy_port = find_free_port();
    let routes = vec![RouteEntry {
        path: "/".into(),
        host: None,
        upstream: format!("http://127.0.0.1:{upstream_port}"),
        ws: false,
    }];
    let mut proxy = ProxyInstance::new(proxy_port, None, routes, 100, None, None);
    proxy.start();
    thread::sleep(Duration::from_millis(200));
    assert!(proxy.is_running());

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://127.0.0.1:{proxy_port}/"))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let headers = resp.headers();
    assert!(
        !headers.contains_key("x-upstream-custom"),
        "X-Upstream-Custom listed in upstream Connection must be stripped from response"
    );
    assert!(
        headers.contains_key("x-normal"),
        "X-Normal should be forwarded, got: {headers:?}"
    );
    assert!(
        !headers.contains_key("keep-alive"),
        "keep-alive is hop-by-hop response header and must be stripped"
    );
    assert!(
        !headers.contains_key("connection"),
        "Connection header must be stripped from response"
    );

    proxy.stop();
}
