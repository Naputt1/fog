use fog::proxy::{ProxyInstance, RouteEntry};

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
    match resp {
        Ok(r) => assert_eq!(r.status(), 502),
        Err(_) => {}
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
