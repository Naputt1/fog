//! HTTP request routing for the embedded index server (`serve_index`).

use std::convert::Infallible;
use std::io;
use std::path::PathBuf;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};

use super::api::{api_config, api_health, api_services_with_query, api_status};
use super::assets::serve_spa_fallback;
use super::icons::{api_project_icon, parse_project_icon_route};
use super::launch::{api_launch_targets_method, handle_launch_request};
use super::logs::{serve_logs_history, serve_logs_stream};
use super::{
    CURRENT_INDEX_PORT, DEFAULT_INDEX_PORT, RespBody, api_error, api_not_found,
    discover_fog_instances, index_pid_path, json_response, load_runtime_config,
    maybe_terminate_if_no_instances, query_param, resolve_service_target, terminate_server_on_port,
};

/// Guards state-changing routes against cross-origin "simple" requests.
///
/// The embedded server binds loopback only, but a browser on any site the user
/// visits can still fire a cross-origin `POST` (a CORS "simple" request needs
/// no preflight) that launches or kills fog instances. A browser attaches
/// either `Sec-Fetch-Site` or `Origin` to such a request; a non-browser client
/// such as `curl` sends neither and is allowed, since it is not subject to the
/// browser's same-origin enforcement.
pub(super) fn write_request_allowed(headers: &hyper::HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && site != "same-origin"
        && site != "none"
    {
        return false;
    }
    if let Some(origin) = headers
        .get(hyper::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if origin.eq_ignore_ascii_case("null") {
            return false;
        }
        let Some((_scheme, origin_authority)) = origin.split_once("://") else {
            return false;
        };
        let Some(host) = headers
            .get(hyper::header::HOST)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        if crate::terminal_ws::normalize_authority(origin_authority)
            != crate::terminal_ws::normalize_authority(host)
        {
            return false;
        }
    }
    true
}

/// Defense-in-depth for the two body-parsing routes: a request that carries a
/// `Content-Type` must declare JSON. A missing header is allowed so non-browser
/// clients keep working; the origin guard is the primary protection.
pub(super) fn json_content_type_allowed(headers: &hyper::HeaderMap) -> bool {
    match headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        Some(ct) => ct
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("application/json"),
        None => true,
    }
}

/// Rebuilds a terminal-gateway response with this server's body type. The
/// gateway's responses are fully buffered (empty body on 101, short text on
/// 401/429), so collecting is cheap and lossless.
async fn rebuild_response<B>(resp: hyper::Response<B>) -> Response<RespBody>
where
    B: hyper::body::Body<Data = Bytes>,
{
    let (parts, body) = resp.into_parts();
    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => Bytes::new(),
    };
    Response::from_parts(parts, Full::new(bytes).boxed())
}

/// Handles the built-in `/ws/terminal` WebSocket upgrade.
///
/// `?service=<name>` attaches to a running service; `?live=1` requests live
/// emulation of the same PTY the TUI is showing (bidirectional, same process),
/// rather than a fresh shell in the service's workdir.
async fn handle_terminal_route(
    req: Request<Incoming>,
    terminal: std::sync::Arc<crate::config::TerminalConfig>,
    terminal_sessions: std::sync::Arc<crate::terminal_ws::SessionsRegistry>,
    peer_ip: String,
) -> Response<RespBody> {
    let service = query_param(req.uri().query(), "service");
    let live = query_param(req.uri().query(), "live")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    if live && service.is_some() {
        let svc = service.clone().unwrap();
        // Live attach must be a known running service, else 404.
        let running = discover_fog_instances()
            .iter()
            .any(|i| i.services.iter().any(|s| s.name == svc && s.running));
        if !running {
            return api_error(StatusCode::NOT_FOUND, "unknown or not running service");
        }
        let resp = crate::terminal_ws::handle_live_terminal_upgrade(
            req,
            terminal,
            terminal_sessions,
            peer_ip,
            svc,
            "127.0.0.1",
        )
        .await
        .expect("live terminal upgrade handler is infallible");
        return rebuild_response(resp).await;
    }
    let target = service.as_ref().and_then(|s| resolve_service_target(s));
    // An explicitly requested service that could not be resolved (not a
    // running service, or its workdir is unknown) is a 404, not a shell.
    if service.is_some() && target.is_none() {
        return api_error(StatusCode::NOT_FOUND, "unknown or not running service");
    }
    let resp = crate::terminal_ws::handle_terminal_upgrade(
        req,
        terminal,
        terminal_sessions,
        peer_ip,
        target,
        "127.0.0.1",
    )
    .await
    .expect("terminal upgrade handler is infallible");
    rebuild_response(resp).await
}

/// Routes embedded-server requests:
///   - `/ws/terminal` → the built-in terminal WebSocket gateway (live PTY)
///   - `/logs/stream` → SSE stream of a container's `docker logs -f`
///   - `/api/logs/history` → one-shot JSON window of older log lines
///     (`?service=&[pid=]&[tail=]&[offset=]`) for scroll-up backfill
///   - `/api/...`     → JSON API endpoints consumed by the SPA
///   - `/api/instances/{pid}/services/{name}/action` → `POST` a service action
///     to a running fog instance over its IPC socket
///   - any other path → an embedded SPA asset if present, else 404
///
/// The request is taken by value so the `action` route can consume its body;
/// every other route ignores it.
pub(super) async fn serve_index(
    network: &str,
    req: Request<Incoming>,
    terminal: std::sync::Arc<crate::config::TerminalConfig>,
    terminal_sessions: std::sync::Arc<crate::terminal_ws::SessionsRegistry>,
    peer_ip: String,
) -> Result<Response<RespBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // State-changing routes are dispatched before the SPA fallback. Loopback
    // binding alone does not stop a web page the user visits from firing a
    // cross-origin "simple" POST (no preflight), so reject any write route that
    // a browser marks as coming from another origin.
    let is_write_route = parse_action_route(&path).is_some()
        || parse_kill_route(&path).is_some()
        || path == "/api/launch"
        || path == "/api/server/kill"
        || path == "/api/server/restart"
        || parse_restart_route(&path).is_some();
    if is_write_route && !write_request_allowed(req.headers()) {
        return Ok(api_error(
            StatusCode::FORBIDDEN,
            "cross-origin request rejected",
        ));
    }

    // The terminal gateway is a built-in WebSocket endpoint, so it must be
    // served before any static/API routing. Without this the browser's
    // `/ws/terminal` upgrade would be answered with the SPA fallback and the
    // connection would fail immediately.
    if crate::terminal_ws::is_terminal_upgrade(&req) {
        return Ok(handle_terminal_route(req, terminal, terminal_sessions, peer_ip).await);
    }

    // The action route consumes the request body (and forwards to a blocking
    // IPC call), so handle it before the path-only dispatch below.
    if let Some((pid, name)) = parse_action_route(&path) {
        if !json_content_type_allowed(req.headers()) {
            return Ok(api_error(
                StatusCode::BAD_REQUEST,
                "unsupported content-type",
            ));
        }
        let (_, body) = req.into_parts();
        let bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => Bytes::new(),
        };
        return Ok(
            handle_action_request(&method, pid, name, &bytes, resolve_instance_socket).await,
        );
    }

    // The kill route is a no-body POST forwarded to the IPC socket.
    if let Some(pid) = parse_kill_route(&path) {
        return Ok(handle_kill_request(&method, pid).await);
    }

    // The launch route consumes the request body and can block for up to ~60s
    // while waiting for the daemon to become ready, so handle it before the
    // path-only dispatch below.
    if path == "/api/launch" {
        if !json_content_type_allowed(req.headers()) {
            return Ok(api_error(
                StatusCode::BAD_REQUEST,
                "unsupported content-type",
            ));
        }
        let (_, body) = req.into_parts();
        let bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => Bytes::new(),
        };
        return Ok(handle_launch_request(&method, &bytes).await);
    }

    if path == "/api/server/kill" {
        return Ok(handle_server_kill_request(&method).await);
    }
    if path == "/api/server/restart" {
        return Ok(handle_server_restart_request(&method).await);
    }
    if let Some(pid) = parse_restart_route(&path) {
        return Ok(handle_instance_restart_request(&method, pid).await);
    }

    // `/api/services` supports `?withInternal=1` to include non-Traefik compose services
    // (e.g. postgres) for the logs picker. Default is Traefik-only. Blocking docker
    // calls are offloaded so the current_thread runtime is not stalled.
    if path == "/api/services" {
        let query = req.uri().query().map(|s| s.to_string());
        let net = network.to_string();
        let resp =
            tokio::task::spawn_blocking(move || api_services_with_query(&net, query.as_deref()))
                .await
                .unwrap_or_else(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()));
        return Ok(resp);
    }

    // Per-project icon: reads the owning project's configured file (or
    // redirects to its configured URL). Filesystem access is offloaded so the
    // current_thread runtime is not stalled.
    if let Some(name) = parse_project_icon_route(&path) {
        let name = name.to_string();
        let if_none_match = req
            .headers()
            .get(hyper::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let resp =
            tokio::task::spawn_blocking(move || api_project_icon(&name, if_none_match.as_deref()))
                .await
                .unwrap_or_else(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()));
        return Ok(resp);
    }

    match path.as_str() {
        "/logs/stream" => Ok(serve_logs_stream(&req).await),
        "/api/logs/history" => Ok(serve_logs_history(&req).await),
        "/api/status" => Ok(api_status()),
        "/api/config" => Ok(api_config()),
        "/api/health" => Ok(api_health()),
        "/api/launch/targets" => Ok(api_launch_targets_method(&method)),
        _ if path.starts_with("/api/") => Ok(api_not_found()),
        _ => {
            let accept_encoding = req
                .headers()
                .get(hyper::header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            Ok(serve_spa_fallback(&path, accept_encoding))
        }
    }
}

/// Parses `/api/instances/{pid}/services/{name}/action` from a path, returning
/// the raw `pid` and service `name` segments on a structural match.
pub(super) fn parse_action_route(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/api/instances/")?;
    let mut seg = rest.split('/');
    let pid = seg.next()?;
    if seg.next() != Some("services") {
        return None;
    }
    let name = seg.next()?;
    if seg.next() != Some("action") {
        return None;
    }
    // No trailing segments allowed.
    if seg.next().is_some() {
        return None;
    }
    Some((pid, name))
}

/// Resolves the IPC socket path for a fog instance PID, defaulting to the
/// standard `$TMPDIR/fog-<pid>.sock` location. Factored out so tests can point
/// the action handler at a custom socket.
fn resolve_instance_socket(pid: u32) -> PathBuf {
    crate::ipc::socket_path(pid)
}

/// Request body for `POST /api/instances/{pid}/services/{name}/action`.
///
/// The action uses [`crate::ipc::ServiceAction`] (serde `snake_case`), so only
/// `start` / `stop` / `restart` deserialize; anything else (or a missing
/// `action`) fails to parse and is rejected with a 400.
#[derive(serde::Deserialize)]
struct ServiceActionBody {
    action: crate::ipc::ServiceAction,
}

/// Parses `/api/instances/{pid}/kill` from a path, returning the pid string.
fn parse_kill_route(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/api/instances/")?;
    let (pid, tail) = rest.split_once('/')?;
    if tail != "kill" {
        return None;
    }
    Some(pid)
}

/// Parses `/api/instances/{pid}/restart` from a path, returning the pid string.
fn parse_restart_route(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/api/instances/")?;
    let (pid, tail) = rest.split_once('/')?;
    if tail != "restart" {
        return None;
    }
    Some(pid)
}

/// Handles `POST /api/instances/{pid}/kill` — sends a graceful shutdown
/// over the instance's IPC socket.
///
///   - non-`POST` → 404
///   - non-numeric pid → 400
///   - no socket file → 404
///   - success → 200 `{"ok":true}`
///   - IPC error → 502
async fn handle_kill_request(method: &hyper::Method, pid_str: &str) -> Response<RespBody> {
    if method != hyper::Method::POST {
        return api_not_found();
    }

    let Ok(pid) = pid_str.parse::<u32>() else {
        return api_error(StatusCode::BAD_REQUEST, "invalid pid");
    };

    let socket = resolve_instance_socket(pid);
    if !socket.exists() {
        return api_error(StatusCode::NOT_FOUND, "instance not found");
    }

    let outcome = tokio::task::spawn_blocking(move || crate::ipc::send_kill(&socket))
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));

    match outcome {
        Ok(()) => {
            // The killed instance will run `maybe_terminate_if_no_instances`
            // on its own exit, but also schedule a server-side idle check
            // so the server exits even if that instance crashes before it.
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(800));
                maybe_terminate_if_no_instances(None);
            });
            json_response(&serde_json::json!({"ok":true}))
        }
        Err(e) => api_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

/// Handles `POST /api/server/kill` — terminates the index server itself.
///
/// The response is sent before the server exits, so the client sees success.
async fn handle_server_kill_request(method: &hyper::Method) -> Response<RespBody> {
    if method != hyper::Method::POST {
        return api_not_found();
    }
    let port = {
        let p = CURRENT_INDEX_PORT.load(std::sync::atomic::Ordering::SeqCst);
        if p != 0 { p } else { DEFAULT_INDEX_PORT }
    };
    tokio::task::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = terminate_server_on_port(port);
        std::process::exit(0);
    });
    json_response(&serde_json::json!({"ok":true}))
}

/// Handles `POST /api/server/restart` — kills then re-ensures the index server.
///
/// The restart is done by spawning a detached helper that revives the server
/// after this process exits, so the port is free when the new server binds.
async fn handle_server_restart_request(method: &hyper::Method) -> Response<RespBody> {
    if method != hyper::Method::POST {
        return api_not_found();
    }
    let current_port = CURRENT_INDEX_PORT.load(std::sync::atomic::Ordering::SeqCst);
    let port = if current_port != 0 {
        current_port
    } else {
        let cfg = load_runtime_config().router.unwrap_or_default();
        cfg.index_port.unwrap_or(DEFAULT_INDEX_PORT)
    };
    let network = std::env::var("FOG_INDEX_NETWORK").ok().unwrap_or_else(|| {
        load_runtime_config()
            .router
            .unwrap_or_default()
            .shared_network
            .clone()
    });
    tokio::task::spawn_blocking(move || {
        // Wait for the current server to release the port before relaunching.
        std::thread::sleep(std::time::Duration::from_millis(700));
        // Launch a detached `fog index serve --foreground`: the helper blocks
        // (plain `serve` would detach itself and double-fork).
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fog"));
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["index", "serve", "--foreground"])
            .env("FOG_INDEX_PORT", port.to_string())
            .env("FOG_INDEX_NETWORK", &network)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::process::detach_command(&mut cmd);
        let _ = cmd.spawn();
        let _ = std::fs::remove_file(index_pid_path(port));
        std::process::exit(0);
    });
    json_response(&serde_json::json!({"ok":true, "port": port}))
}

/// Handles `POST /api/instances/{pid}/restart` — kills the instance and
/// relaunches it detached with the same script/config/branch.
async fn handle_instance_restart_request(
    method: &hyper::Method,
    pid_str: &str,
) -> Response<RespBody> {
    if method != hyper::Method::POST {
        return api_not_found();
    }
    let Ok(pid) = pid_str.parse::<u32>() else {
        return api_error(StatusCode::BAD_REQUEST, "invalid pid");
    };
    let socket = resolve_instance_socket(pid);
    if !socket.exists() {
        return api_error(StatusCode::NOT_FOUND, "instance not found");
    }
    let outcome = tokio::task::spawn_blocking(move || {
        // Query before killing to capture relaunch params.
        let status = crate::ipc::query_status(&socket)
            .map_err(|e| format!("could not query instance {pid}: {e}"))?;
        let script = status.script.clone();
        let config_dir = status.config_dir.clone().unwrap_or_else(|| ".".to_string());
        let config_path = std::path::PathBuf::from(&config_dir).join("fog.json");
        let config_path = if config_path.exists() {
            config_path
        } else {
            std::path::PathBuf::from(&config_dir)
        };
        let socket = resolve_instance_socket(pid);
        crate::ipc::send_kill(&socket).map_err(|e| e.to_string())?;
        // Wait for old instance to exit.
        for _ in 0..50 {
            if !crate::process::is_pid_alive(pid) && crate::ipc::query_status(&socket).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Relaunch detached. config_path already points at the correct
        // worktree's fog.json, so no --branch is needed.
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fog"));
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--config").arg(&config_path).arg(&script);
        cmd.env("FOG_DAEMON_CHILD", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::process::detach_command(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("could not restart: {e}"))?;
        let new_pid = child.id();
        let new_socket = crate::ipc::socket_path(new_pid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if crate::ipc::query_status(&new_socket).is_ok() {
                return Ok::<u32, String>(new_pid);
            }
            if child.try_wait().ok().flatten().is_some() {
                return Err(format!(
                    "restarted instance {new_pid} exited during startup"
                ));
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("restarted instance {new_pid} did not become ready"));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    })
    .await
    .unwrap_or(Err("spawn_blocking failed".to_string()));
    match outcome {
        Ok(new_pid) => json_response(&serde_json::json!({"ok":true, "pid": new_pid})),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Handles `POST /api/instances/{pid}/services/{name}/action`.
///
/// Forward the action to the fog instance owning `pid` over its IPC socket.
/// Status code contract:
///   - non-`POST` method → 404 (treated like an unknown route, matching the
///     other path-only handlers)
///   - non-numeric `pid` → 400 `{"error":"invalid pid"}`
///   - invalid/missing `action` in the body → 400 `{"error":"invalid action"}`
///   - no socket file for the pid → 404 `{"error":"instance not found"}`
///   - success (even with `ok:false`) → 200 `{"ok":..,"reason":".."}`
///   - `send_service_action` io error (socket exists but the instance is
///     unreachable / connect fails / times out) → 502 `{"error":"<err>"}`
///
/// The 502 choice mirrors a reverse-proxy `BAD_GATEWAY`: the request reached a
/// real endpoint but the upstream fog instance did not answer — consistent with
/// the proxy treating an unreachable upstream as a gateway error. The 404 above
/// is reserved for the unambiguous "no instance socket at all" case.
pub(super) async fn handle_action_request<F>(
    method: &hyper::Method,
    pid: &str,
    name: &str,
    body: &[u8],
    resolve: F,
) -> Response<RespBody>
where
    F: Fn(u32) -> PathBuf,
{
    // The action route only fires on POST; any other method is treated like an
    // unknown API route (404), matching the existing path-only handlers.
    if method != hyper::Method::POST {
        return api_not_found();
    }

    let Ok(pid) = pid.parse::<u32>() else {
        return api_error(StatusCode::BAD_REQUEST, "invalid pid");
    };

    // Parse the body into a typed request so the action must be a real
    // `ServiceAction` (`start`/`stop`/`restart`).
    let req: ServiceActionBody = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid action"),
    };

    let socket = resolve(pid);
    if !socket.exists() {
        return api_error(StatusCode::NOT_FOUND, "instance not found");
    }

    // send_service_action blocks for up to ~35s (IPC 30s control window +
    // margin); move it onto a blocking thread so the HTTP task is never
    // blocked. A panic inside is surfaced as an io error, never an unwrap.
    let socket_task = socket.clone();
    let name_task = name.to_string();
    let action = req.action;
    let outcome = tokio::task::spawn_blocking(move || {
        crate::ipc::send_service_action(&socket_task, &name_task, action)
    })
    .await
    .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));

    match outcome {
        // The instance's verdict is reported verbatim: 200 even when the action
        // was refused (`ok:false`) with a human-readable reason.
        Ok(resp) => json_response(&serde_json::json!({
            "ok": resp.ok,
            "reason": resp.reason,
        })),
        Err(e) => api_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}
