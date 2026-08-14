use crate::config::RouterConfig;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::thread;

/// Default port for the embedded service-directory index server.
const DEFAULT_INDEX_PORT: u16 = 18080;

/// One rendered row of HTML for a single service.
type IndexRow = String;
/// A worktree (or `shared`) subgroup: (group name, rendered rows).
type IndexGroup = (String, Vec<IndexRow>);
/// A project section: (project name, worktree/shared groups).
type ProjectSection = (String, Vec<IndexGroup>);

/// One reachable service discovered from docker (Traefik labels or
/// `fog.expose` custom labels).
struct IndexEntry {
    /// Display project name (e.g. `gems`, `red-fox`), derived from the repo
    /// root of the compose project's working dir.
    project: String,
    /// Worktree/branch group (e.g. `main`, `feature-x`). Shared infra services
    /// report `shared`.
    worktree: String,
    /// Whether this is a shared infra service (under the `shared` subgroup).
    shared: bool,
    /// Compose service name (e.g. `frontend`, `postgres`).
    service: String,
    /// Hostname the router accepts (e.g. `main.red-fox`, `main.postgres.gems`).
    hostname: String,
    /// Internal container port Traefik forwards to. Empty for raw-TCP
    /// services exposed via `fog.expose` (they are reached by host port).
    port: String,
    /// Host-published docker port bindings of the container (e.g.
    /// `0.0.0.0:8080->8080/tcp`). Empty when nothing is published to the host.
    published: Vec<String>,
    /// The host port number docker published for this service, if any
    /// (e.g. `53012` for `0.0.0.0:53012->5173/tcp`).
    raw_port: Option<String>,
    /// Whether the router terminates TLS (`https`) or not (`http`).
    tls: bool,
}

/// Directory where the generated index is written (served by the embedded
/// server and hot-reloaded on refresh).
fn public_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.config/fog/public"))
}

/// Ensures the service-directory index exists: generates `index.html` from the
/// current docker routing state and starts the standalone index server if it is
/// not already running. Idempotent and best-effort, mirroring the router.
pub fn ensure(cfg: &RouterConfig) -> Vec<String> {
    let mut messages = Vec::new();
    if !command_exists("docker") {
        return messages;
    }
    messages.extend(refresh(cfg));
    let port = cfg.index_port.unwrap_or(DEFAULT_INDEX_PORT);
    if server_started(port) {
        return messages;
    }
    match spawn_server(cfg) {
        Ok(()) => messages.push(format!(
            "  + service index server on http://127.0.0.1:{port} (unmatched hosts)"
        )),
        Err(e) => messages.push(format!(
            "⚠ could not start index server on :{port} ({e}); unmatched hosts will 404."
        )),
    }
    messages
}

/// Regenerates `index.html` from live docker state without touching the server.
/// Safe to call multiple times; returns informational messages.
pub fn refresh(cfg: &RouterConfig) -> Vec<String> {
    let mut messages = Vec::new();
    let dir = public_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        messages.push(format!(
            "⚠ could not create index dir {}: {e}",
            dir.display()
        ));
        return messages;
    }
    let html = generate_index(cfg, None);
    if let Err(e) = fs::write(dir.join("index.html"), &html) {
        messages.push(format!("⚠ could not write index.html: {e}"));
        return messages;
    }
    messages.push(format!(
        "  + service index updated ({} entries)",
        html.matches("<li").count()
    ));
    messages
}

/// Regenerates the index a few times shortly after startup so freshly-started
/// app containers (which reconcile may briefly recreate) have joined the router
/// network. This is a bounded, one-shot "refresh on start" — not continuous
/// live-updating. Best-effort; failures only warn.
///
/// The returned stop flag lets teardown halt the loop so a stale refresh does
/// not overwrite the teardown regeneration.
pub fn refresh_after_startup(cfg: &RouterConfig) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cfg = cfg.clone();
    let stop_ref = stop.clone();
    let _ = thread::Builder::new()
        .name("fog-index-refresh".to_string())
        .spawn(move || {
            for _ in 0..4 {
                thread::sleep(std::time::Duration::from_secs(8));
                if stop_ref.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let _ = refresh(&cfg);
            }
        });
    stop
}

/// Whether the standalone index server is already listening.
fn server_started(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// Launches the index server as a detached background process (`fog index
/// serve`), so it survives any individual fog instance exiting. If it is
/// already running this is a no-op.
fn spawn_server(cfg: &RouterConfig) -> Result<(), String> {
    let port = cfg.index_port.unwrap_or(DEFAULT_INDEX_PORT);
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("fog"));
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["index", "serve"])
        .env("FOG_INDEX_PORT", port.to_string())
        .env("FOG_INDEX_NETWORK", &cfg.shared_network)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not spawn index server: {e}"))?;
    let pid = child.id();
    // Wait briefly for it to bind before reporting success.
    for _ in 0..25 {
        if server_started(port) {
            return Ok(());
        }
        if child.try_wait().ok().flatten().is_some() {
            return Err(format!("index server (pid {pid}) exited during startup"));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err(format!("index server (pid {pid}) did not bind within 5s"))
}

/// Entry point for the `fog index serve` subcommand: runs the standalone index
/// server in the foreground until killed.
pub fn serve() -> io::Result<()> {
    let port: u16 = std::env::var("FOG_INDEX_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_INDEX_PORT);
    let network = std::env::var("FOG_INDEX_NETWORK").unwrap_or_else(|_| "fog-router".to_string());
    serve_blocking(port, public_dir(), network)
}

/// Blocks forever serving `index.html` from `dir` on loopback `port`, rendering
/// the page fresh on each request (so raw host ports and the request host are
/// always current).
fn serve_blocking(port: u16, dir: PathBuf, network: String) -> io::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|e| io::Error::other(e.to_string()))?;
    rt.block_on(async move {
        let addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| io::Error::other(format!("bind {addr}: {e}")))?;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let io = TokioIo::new(stream);
            let dir = dir.clone();
            let network = network.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let dir = dir.clone();
                    let network = network.clone();
                    async move { serve_index(&dir, &network, &req).await }
                });
                let _ = http1::Builder::new()
                    .serve_connection(io, svc)
                    .with_upgrades()
                    .await;
            });
        }
    })
}

/// Generates the `index.html` listing every running service grouped by project
/// then worktree, by parsing the Traefik labels (and `fog.expose` custom labels)
/// of containers. `base_host` is the host the request came in on (e.g.
/// `100.86.26.45`); when set, each service that publishes a host port gets a
/// clickable raw `http://{base_host}:{port}/` link too.
fn generate_index(cfg: &RouterConfig, base_host: Option<&str>) -> String {
    let entries = discover_entries(&cfg.shared_network);
    let _tls_default = cfg.tls.enabled;

    // Group entries by project, then by worktree (shared infra last). Preserve
    // insertion order within a group.
    let mut projects: Vec<ProjectSection> = Vec::new();
    for e in entries {
        let row = render_entry(&e, base_host);
        let group = if e.shared {
            "shared".to_string()
        } else {
            e.worktree.clone()
        };
        // Find or insert the project bucket.
        match projects.iter_mut().find(|(p, _)| *p == e.project) {
            Some((_, groups)) => match groups.iter_mut().find(|(g, _)| *g == group) {
                Some((_, rows)) => rows.push(row),
                None => groups.push((group, vec![row])),
            },
            None => projects.push((e.project.clone(), vec![(group, vec![row])])),
        }
    }

    // Sort projects and groups; shared subgroup sinks to the bottom.
    projects.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, groups) in &mut projects {
        groups.sort_by(|a, b| {
            let ak = if a.0 == "shared" {
                "zzz".to_string()
            } else {
                a.0.clone()
            };
            let bk = if b.0 == "shared" {
                "zzz".to_string()
            } else {
                b.0.clone()
            };
            ak.cmp(&bk)
        });
    }

    let sections = projects
        .into_iter()
        .map(|(project, groups)| {
            let groups_html = groups
                .into_iter()
                .map(|(group, rows)| {
                    let rows_html = rows.join("\n");
                    if group == "shared" {
                        format!(
                            "<div class=\"group\"><h3>shared</h3><ul>\n{rows_html}\n</ul></div>"
                        )
                    } else {
                        format!(
                            "<div class=\"group\"><h3>{group}</h3><ul>\n{rows_html}\n</ul></div>"
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("<section><h2>{project}</h2>\n{groups_html}\n</section>")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let empty = if sections.is_empty() {
        "<p class=\"empty\">No services are currently running on the router.</p>"
    } else {
        ""
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>fog — running services</title>
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 2rem auto; max-width: 720px; padding: 0 1rem; color: #1f2328; }}
  h1 {{ font-size: 1.3rem; }}
  h2 {{ font-size: 1.1rem; margin: 1.5rem 0 .4rem; border-bottom: 2px solid #0969da; padding-bottom: .2rem; }}
  h3 {{ font-size: .95rem; margin: .8rem 0 .2rem; color: #57606a; }}
  ul {{ list-style: none; padding: 0; margin: 0; }}
  li {{ display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; padding: .5rem .8rem; border-bottom: 1px solid #e5e7eb; }}
  code {{ background: #f3f4f6; padding: .1rem .3rem; border-radius: 4px; }}
  button {{ font: inherit; cursor: pointer; border: 1px solid #d0d7de; background: #f6f8fa; border-radius: 6px; padding: .2rem .6rem; }}
  button.copied {{ background: #dafbe1; border-color: #4ac26b; }}
  a {{ color: #0969da; text-decoration: none; }}
  .port {{ color: #57606a; font-size: .85rem; }}
  .empty {{ color: #57606a; }}
</style>
</head>
<body>
<h1>fog — running services</h1>
<p>Tap <b>copy</b> to copy a URL, or open it directly.</p>
{sections}
{empty}
<script>
document.addEventListener('click', function (ev) {{
  var b = ev.target.closest('button');
  if (!b) return;
  var url = b.getAttribute('data-url');
  function ok() {{ b.textContent = 'copied'; b.classList.add('copied'); setTimeout(function () {{ b.textContent = 'copy'; b.classList.remove('copied'); }}, 1500); }}
  if (navigator.clipboard && navigator.clipboard.writeText) {{
    navigator.clipboard.writeText(url).then(ok, function () {{ fallback(url, ok); }});
  }} else {{ fallback(url, ok); }}
}});
function fallback(url, ok) {{
  var ta = document.createElement('textarea');
  ta.value = url; document.body.appendChild(ta); ta.select();
  try {{ document.execCommand('copy'); ok(); }} catch (e) {{ prompt('Copy this URL:', url); }}
  document.body.removeChild(ta);
}}
</script>
</body>
</html>
"#
    )
}

/// Renders a single index entry as a `<li>` row. HTTP services (via Traefik)
/// link to `{scheme}://{hostname}/`; raw-TCP services exposed via `fog.expose`
/// have no traefik port and link to `{hostname}:{raw_port}` instead.
fn render_entry(e: &IndexEntry, base_host: Option<&str>) -> String {
    let published = if e.published.is_empty() {
        String::new()
    } else {
        format!(
            " <span class=\"port\">docker published: {}</span>",
            e.published.join(", ")
        )
    };
    if e.port.is_empty() {
        // Raw-TCP service (postgres, redis, ...): reachable via hostname + host port.
        let Some(raw_port) = &e.raw_port else {
            return format!(
                "<li><span class=\"name\">{}&nbsp;<code>{}</code></span>{published}</li>",
                e.service, e.hostname
            );
        };
        let url = format!("http://{}:{}/", e.hostname, raw_port);
        let host_port = format!("{}:{}", e.hostname, raw_port);
        return format!(
            "<li><span class=\"name\">{}&nbsp;<code>{}</code></span> \
             <button data-url=\"{url}\">copy</button> <a href=\"{url}\">{host_port}</a> \
             <span class=\"port\">→ host port {}</span>{published}</li>",
            e.service, e.hostname, raw_port
        );
    }
    let scheme = if e.tls { "https" } else { "http" };
    let url = format!("{scheme}://{}/", e.hostname);
    let raw = match (&base_host, &e.raw_port) {
        (Some(host), Some(port)) => {
            let raw_url = format!("http://{host}:{port}/");
            format!(
                " <button data-url=\"{raw_url}\">copy raw</button> \
                 <a href=\"{raw_url}\">{raw_url}</a>"
            )
        }
        _ => String::new(),
    };
    format!(
        "<li><span class=\"name\">{}&nbsp;<code>{}</code></span> \
         <button data-url=\"{url}\">copy</button> <a href=\"{url}\">{url}</a> \
         <span class=\"port\">→ traefik port {}</span>{published}{raw}</li>",
        e.service, e.hostname, e.port
    )
}

/// Queries docker for containers on the router's shared network and extracts,
/// per reachable router: hostname(s), the internal port, and whether TLS.
fn discover_entries(shared_network: &str) -> Vec<IndexEntry> {
    let mut entries = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for query in [
        format!("network={shared_network}"),
        "label=fog.expose=true".to_string(),
    ] {
        let out = match Command::new("docker")
            .args(["ps", "--filter", &query, "--format", "{{.Names}}"])
            .output()
        {
            Ok(o) => o,
            Err(_) => continue,
        };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim();
            if !line.is_empty() && !names.contains(&line.to_string()) {
                names.push(line.to_string());
            }
        }
    }

    for name in names {
        // Skip the router itself.
        let labels = match container_labels(&name) {
            Some(l) => l,
            None => continue,
        };
        let has_traefik = labels
            .keys()
            .any(|k| k.starts_with("traefik.http.routers."));
        let expose = labels.get("fog.expose").is_some_and(|v| v == "true");
        if !has_traefik && !expose {
            continue;
        }
        let (project, worktree, shared) = derive_group(&labels, &name);
        let service = labels
            .get("com.docker.compose.service")
            .cloned()
            .unwrap_or_else(|| "app".to_string());
        let published = docker_ports(&name);

        // Raw-TCP services exposed via `fog.expose` have no Traefik HTTP
        // router; they are reached by hostname + published host port.
        if expose {
            let Some(hostname) = labels.get("fog.hostname").cloned() else {
                continue;
            };
            let Some(raw_port) = docker_host_port(&name) else {
                continue;
            };
            entries.push(IndexEntry {
                project: project.clone(),
                worktree: worktree.clone(),
                shared,
                service: service.clone(),
                hostname,
                port: String::new(),
                published: published.clone(),
                raw_port: Some(raw_port),
                tls: false,
            });
            if !has_traefik {
                continue;
            }
        }

        // First pass: collect service ports.
        let mut service_port: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (k, v) in &labels {
            if let Some(rest) = k
                .strip_prefix("traefik.http.services.")
                .and_then(|r| r.strip_suffix(".loadbalancer.server.port"))
            {
                service_port.insert(rest.to_string(), v.clone());
            }
        }
        // Second pass: routers -> host(s) + port + tls. The router may name a
        // different service explicitly; default to the same-name service.
        for (k, v) in &labels {
            let Some(rest) = k.strip_prefix("traefik.http.routers.") else {
                continue;
            };
            let Some((router, "rule")) = rest.split_once('.') else {
                continue;
            };
            let svc = labels
                .get(&format!("traefik.http.routers.{router}.service"))
                .map(String::as_str)
                .unwrap_or(router);
            // Routers whose service has no load balancer port (e.g. an
            // HTTP->HTTPS redirect router) are skipped: they don't terminate at
            // a backend we can open.
            let Some(port) = service_port.get(svc) else {
                continue;
            };
            let tls = labels
                .get(&format!("traefik.http.routers.{router}.tls"))
                .is_some_and(|t| t == "true");
            let raw_port = docker_host_port(&name);
            for host in extract_hosts(v) {
                entries.push(IndexEntry {
                    project: project.clone(),
                    worktree: worktree.clone(),
                    shared,
                    service: service.clone(),
                    hostname: host,
                    port: port.clone(),
                    published: published.clone(),
                    raw_port: raw_port.clone(),
                    tls,
                });
            }
        }
    }
    entries
}

/// Derives the display project name, worktree group and shared-infra flag for a
/// container from its compose labels.
///
/// Project = the repo root basename of `com.docker.compose.project.working_dir`
/// (e.g. `/Users/.../GEMS` -> `gems`). Infra compose files live in an `infra/`
/// subdirectory, so those are flagged `shared` and grouped under the repo root's
/// project. Worktree = the compose project name after the first `-` (e.g.
/// `gems-main` -> `main`, `gems-feature-x` -> `feature-x`), ignored for shared.
fn derive_group(
    labels: &std::collections::HashMap<String, String>,
    container_name: &str,
) -> (String, String, bool) {
    let wd = labels
        .get("com.docker.compose.project.working_dir")
        .map(String::as_str)
        .unwrap_or("");
    let project_name = labels
        .get("com.docker.compose.project")
        .map(String::as_str)
        .unwrap_or(container_name);
    let is_infra = wd.ends_with("/infra") || wd.ends_with("\\infra");
    let project = if wd.is_empty() {
        // No working-dir label: fall back to the compose project name with any
        // trailing `-<worktree>` stripped.
        project_name
            .split_once('-')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| project_name.to_string())
    } else if is_infra {
        // Repo root is the parent of `infra/`.
        let repo = std::path::Path::new(wd)
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_name.to_string());
        repo.to_lowercase()
    } else {
        std::path::Path::new(wd)
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase().to_owned())
            .unwrap_or_else(|| project_name.to_string())
    };
    if is_infra {
        (project, "shared".to_string(), true)
    } else {
        let worktree = project_name
            .split_once('-')
            .map(|(_, w)| w.to_string())
            .unwrap_or_else(|| "main".to_string());
        (project, worktree, false)
    }
}

/// Extracts hostnames from a Traefik `Host(...)` / `HostRegexp(...)` rule.
///
/// Hostnames appear inside backticks: `Host(\`main.gems\`)` and
/// `HostRegexp(\`{host:.+}\`)` both place their pattern between backticks.
fn extract_hosts(rule: &str) -> Vec<String> {
    rule.split('`')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| p.contains('.') || p.starts_with('{'))
        .map(String::from)
        .collect()
}

/// Extracts a container's host-published port bindings, e.g.
/// `0.0.0.0:8080->8080/tcp`. Containers that publish no host ports (only
/// EXPOSE) return an empty list — the container's internal exposed port is not
/// a reachable host port, so it is intentionally not shown.
fn docker_ports(name: &str) -> Vec<String> {
    let Ok(out) = Command::new("docker").args(["port", name]).output() else {
        return Vec::new();
    };
    let mut ports = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Format as `host->container` so the reachable side is clear, e.g.
        // `0.0.0.0:8080->8080/tcp`.
        let pretty = line
            .split_once(" -> ")
            .map(|(host, container)| format!("{host}->{container}"))
            .unwrap_or_else(|| line.to_string());
        if !ports.contains(&pretty) {
            ports.push(pretty);
        }
    }
    ports
}

/// Returns the first host port docker published for a container, if any (the
/// number after the host `:` in `docker port` output, e.g. `53012` from
/// `5173/tcp -> 0.0.0.0:53012`).
fn docker_host_port(name: &str) -> Option<String> {
    let out = Command::new("docker").args(["port", name]).output().ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if let Some((_, host)) = line.split_once(" -> ") {
            let host = host.trim();
            // Prefer an IPv4/IPv6 binding; take its port (last `:` segment).
            if let Some(port) = host.rsplit(':').next()
                && port.chars().all(|c| c.is_ascii_digit())
                && !port.is_empty()
            {
                return Some(port.to_string());
            }
        }
    }
    None
}

/// Returns the container's labels as a map, or `None` if the container is the
/// router itself or cannot be inspected.
fn container_labels(name: &str) -> Option<std::collections::HashMap<String, String>> {
    if name.starts_with("fog-router") {
        return None;
    }
    let out = Command::new("docker")
        .args(["inspect", "--format", "{{json .Config.Labels}}", name])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let obj = value.as_object()?;
    let mut labels = std::collections::HashMap::new();
    for (k, v) in obj {
        if let Some(v) = v.as_str() {
            labels.insert(k.clone(), v.to_string());
        }
    }
    Some(labels)
}

/// Starts the embedded static HTTP server serving `index.html` from `dir` on
/// loopback `port`. Reads the file fresh on each request so manual refreshes
/// pick up regenerated content.
/// Serves the directory page fresh: discovers the running services on
/// `network`, renders them with raw `http://{host}:{port}/` links derived from
/// the request's `Host` header, and returns the HTML.
async fn serve_index(
    _dir: &std::path::Path,
    network: &str,
    req: &Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    // Host header (e.g. `100.86.26.45` on a phone, `127.0.0.1` on the laptop)
    // determines the raw link base so it is always reachable from the client.
    let base_host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_string());

    let cfg = RouterConfig {
        shared_network: network.to_string(),
        ..RouterConfig::default()
    };
    let html = generate_index(&cfg, base_host.as_deref());
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)))
        .expect("response builder failed"))
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Test-only helper to force the default index port constant.
#[cfg(test)]
fn default_port() -> u16 {
    DEFAULT_INDEX_PORT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_port_is_18080() {
        assert_eq!(default_port(), 18080);
    }

    #[test]
    fn test_extract_hosts_simple() {
        let hosts = extract_hosts("Host(`main.gems`)");
        assert_eq!(hosts, vec!["main.gems"]);
    }

    #[test]
    fn test_extract_hosts_multi() {
        let hosts = extract_hosts("Host(`main.gems`) || Host(`feature-x.gems`)");
        assert_eq!(hosts, vec!["main.gems", "feature-x.gems"]);
    }

    #[test]
    fn test_extract_hosts_regexp() {
        let hosts = extract_hosts("HostRegexp(`{host:.+}`)");
        assert_eq!(hosts, vec!["{host:.+}"]);
    }

    #[test]
    fn test_public_dir_under_home() {
        let p = public_dir();
        assert!(p.to_string_lossy().contains(".config/fog/public"));
    }

    #[test]
    fn test_generate_index_empty() {
        let html = generate_index(
            &RouterConfig {
                shared_network: "fog-router".to_string(),
                ..RouterConfig::default()
            },
            Some("100.86.26.45"),
        );
        // No live docker query in tests is guaranteed to have services, but the
        // page must always render valid markup.
        assert!(html.contains("running services"));
        assert!(html.contains("</html>"));
    }

    #[test]
    fn test_docker_host_port_extracts_number() {
        // The helper shells out to docker, so no live assertion is reliable;
        // just confirm it returns a value (or None) without panicking.
        let _ = docker_host_port("fog-router-traefik");
    }

    fn labels(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_derive_group_app_project() {
        let l = labels(&[
            (
                "com.docker.compose.project.working_dir",
                "/Users/naputt/git/GEMS",
            ),
            ("com.docker.compose.project", "gems-main"),
        ]);
        assert_eq!(
            derive_group(&l, "gems-main-api-1"),
            ("gems".into(), "main".into(), false)
        );
    }

    #[test]
    fn test_derive_group_app_feature_worktree() {
        let l = labels(&[
            (
                "com.docker.compose.project.working_dir",
                "/Users/naputt/git/GEMS",
            ),
            ("com.docker.compose.project", "gems-feature-x"),
        ]);
        assert_eq!(
            derive_group(&l, "gems-feature-x-api-1"),
            ("gems".into(), "feature-x".into(), false)
        );
    }

    #[test]
    fn test_derive_group_infra_is_shared() {
        let l = labels(&[
            (
                "com.docker.compose.project.working_dir",
                "/Users/naputt/git/GEMS/infra",
            ),
            ("com.docker.compose.project", "gems-infra"),
        ]);
        assert_eq!(
            derive_group(&l, "gems-postgres"),
            ("gems".into(), "shared".into(), true)
        );
    }

    #[test]
    fn test_derive_group_redfox_naming() {
        // red-fox app project is `redfox-main` (no dash in the base) and infra
        // is `red-fox-infra` under `infra/`; both must resolve to `red-fox`.
        let app = labels(&[
            (
                "com.docker.compose.project.working_dir",
                "/Users/naputt/code/red-fox",
            ),
            ("com.docker.compose.project", "redfox-main"),
        ]);
        assert_eq!(
            derive_group(&app, "redfox-main-api-1"),
            ("red-fox".into(), "main".into(), false)
        );

        let infra = labels(&[
            (
                "com.docker.compose.project.working_dir",
                "/Users/naputt/code/red-fox/infra",
            ),
            ("com.docker.compose.project", "red-fox-infra"),
        ]);
        assert_eq!(
            derive_group(&infra, "red-fox-infra-postgres-1"),
            ("red-fox".into(), "shared".into(), true)
        );
    }

    #[test]
    fn test_derive_group_no_labels_falls_back_to_name() {
        let l = labels(&[]);
        let (p, w, s) = derive_group(&l, "barecontainer");
        assert_eq!(p, "barecontainer");
        assert_eq!(w, "main");
        assert!(!s);

        // A dashed name without a working-dir falls back to the base (prefix
        // before the first `-`).
        let (p, w, _) = derive_group(&l, "gems-main");
        assert_eq!(p, "gems");
        assert_eq!(w, "main");
    }

    #[test]
    fn test_render_entry_http_vs_raw() {
        let http = IndexEntry {
            project: "gems".into(),
            worktree: "main".into(),
            shared: false,
            service: "api".into(),
            hostname: "main.api.gems".into(),
            port: "8082".into(),
            published: vec![],
            raw_port: None,
            tls: true,
        };
        let row = render_entry(&http, None);
        assert!(row.contains("https://main.api.gems/"));
        assert!(row.contains("traefik port 8082"));

        let raw = IndexEntry {
            project: "gems".into(),
            worktree: "shared".into(),
            shared: true,
            service: "postgres".into(),
            hostname: "main.postgres.gems".into(),
            port: String::new(),
            published: vec!["0.0.0.0:55274->5432/tcp".into()],
            raw_port: Some("55274".into()),
            tls: false,
        };
        let row = render_entry(&raw, None);
        assert!(row.contains("main.postgres.gems:55274"));
        assert!(row.contains("host port 55274"));
    }
}
