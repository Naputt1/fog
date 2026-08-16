use crate::config::RouterConfig;
use futures_core::Stream;
use http_body::Frame;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::task::{Context, Poll};
use std::thread;

/// Default port for the embedded service-directory index server.
const DEFAULT_INDEX_PORT: u16 = 18080;

/// Default number of trailing lines served by the live-log stream.
const DEFAULT_LOG_TAIL: usize = 200;
/// Upper bound for the `tail` query parameter, so a single request cannot
/// request an unbounded backfill.
const MAX_LOG_TAIL: usize = 10_000;

/// HTTP response body used by the embedded server. Static pages and the SSE
/// log stream both box into this type so a single service fn serves them all.
type RespBody = BoxBody<Bytes, Infallible>;

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
    /// Docker container name (e.g. `redfox-main-api-1`), used to stream that
    /// container's logs.
    container: String,
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
    // enable_all(): the SSE log stream spawns `docker logs -f` via
    // tokio::process, which needs the runtime's signal driver (SIGCHLD) as
    // well as the IO driver.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
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
  a.logs {{ font-size: .8rem; color: #57606a; }}
  a.logs:hover {{ color: #0969da; }}
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
    let logs = format!(
        " <a class=\"logs\" href=\"/logs?service={}\">logs</a>",
        e.container
    );
    if e.port.is_empty() {
        // Raw-TCP service (postgres, redis, ...): reachable via hostname + host port.
        let Some(raw_port) = &e.raw_port else {
            return format!(
                "<li><span class=\"name\">{}&nbsp;<code>{}</code></span>{logs}{published}</li>",
                e.service, e.hostname
            );
        };
        let url = format!("http://{}:{}/", e.hostname, raw_port);
        let host_port = format!("{}:{}", e.hostname, raw_port);
        return format!(
            "<li><span class=\"name\">{}&nbsp;<code>{}</code></span> \
             <button data-url=\"{url}\">copy</button> <a href=\"{url}\">{host_port}</a> \
             <span class=\"port\">→ host port {}</span>{logs}{published}</li>",
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
         <span class=\"port\">→ traefik port {}</span>{logs}{published}{raw}</li>",
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

    // Cache the git-derived project per working directory: containers from the
    // same compose dir (and different worktrees of the same repo) share it.
    let mut git_projects: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();

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
        let git_project = labels
            .get("com.docker.compose.project.working_dir")
            .and_then(|wd| {
                git_projects
                    .entry(wd.clone())
                    .or_insert_with(|| git_project_for(wd))
                    .clone()
            });
        let (project, worktree, shared) = derive_group(&labels, &name, git_project.as_deref());
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
                container: name.clone(),
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
                    container: name.clone(),
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
/// `git_project` — the git-common-dir-derived project name, when the compose
/// `working_dir` sits inside a repository — takes precedence over the
/// label-derived name. This groups all worktrees of the same repo (e.g.
/// `admin/` and `ui/` of red-fox) under one project, matching fog's own
/// instance identity.
fn derive_group(
    labels: &std::collections::HashMap<String, String>,
    container_name: &str,
    git_project: Option<&str>,
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
    let project = match git_project.filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => {
            if wd.is_empty() {
                // No working-dir label: fall back to the compose project name
                // with any trailing `-<worktree>` stripped.
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
            }
        }
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

/// Resolves the display project name from the git repository containing a
/// compose working directory, using the same identity fog uses for instances
/// (the git common dir — shared by every worktree of the repo). Returns
/// `None` when the directory isn't in a git repo or git is unavailable.
fn git_project_for(working_dir: &str) -> Option<String> {
    let common_dir = crate::project::detect(std::path::Path::new(working_dir))?;
    Some(project_name_from_common_dir(&common_dir))
}

/// Maps a git common-dir path (e.g. `/repo/.git`) to a display project name
/// (e.g. `repo`), lowercased to match the directory page's grouping.
fn project_name_from_common_dir(common_dir: &str) -> String {
    let path = std::path::Path::new(common_dir);
    if path.file_name().is_some_and(|n| n == ".git") {
        path.parent()
            .and_then(std::path::Path::file_name)
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| common_dir.to_string())
    } else {
        path.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| common_dir.to_string())
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

/// Routes embedded-server requests:
///   - `/`            → the service-directory page (existing behavior)
///   - `/logs`        → the live-log viewer page
///   - `/logs/stream` → SSE stream of a container's `docker logs -f`
///
/// Every other path falls back to the directory page.
///
/// The `Host` header (e.g. `100.86.26.45` on a phone, `127.0.0.1` on the
/// laptop) determines the raw-link base so those are always reachable from
/// the client.
async fn serve_index(
    _dir: &std::path::Path,
    network: &str,
    req: &Request<Incoming>,
) -> Result<Response<RespBody>, Infallible> {
    let base_host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_string());

    let cfg = RouterConfig {
        shared_network: network.to_string(),
        ..RouterConfig::default()
    };

    match req.uri().path() {
        "/logs" => Ok(serve_logs_page(&cfg, req)),
        "/logs/stream" => Ok(serve_logs_stream(req).await),
        _ => Ok(serve_directory(&cfg, base_host.as_deref())),
    }
}

/// The service-directory page: discovers the running services on `network`,
/// renders them with raw `http://{host}:{port}/` links derived from the
/// request's `Host` header, and returns the HTML.
fn serve_directory(cfg: &RouterConfig, base_host: Option<&str>) -> Response<RespBody> {
    let html = generate_index(cfg, base_host);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)).boxed())
        .expect("response builder failed")
}

/// The live-log viewer page: a dark terminal-style pane with a service picker
/// (grouped like the directory page) and inline JS that streams the selected
/// service's logs via `/logs/stream`. Picks from docker containers (router
/// discovery) plus running fog instances (their captured logs, proxy request
/// log, and daemon log).
fn serve_logs_page(cfg: &RouterConfig, req: &Request<Incoming>) -> Response<RespBody> {
    let entries = discover_entries(&cfg.shared_network);
    let instances = discover_fog_instances();
    let selected = req
        .uri()
        .query()
        .and_then(|q| parse_query(q).get("service").cloned())
        .unwrap_or_default();
    let html = logs_page_html(&entries, &instances, &selected);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)).boxed())
        .expect("response builder failed")
}

/// A running fog instance, listed in the log viewer's picker.
struct FogInstance {
    /// The fog process PID (its socket is `$TMPDIR/fog-<pid>.sock`).
    pid: u32,
    /// The script the instance is running (e.g. `dev`).
    script: String,
    /// Live service status snapshots.
    services: Vec<crate::ipc::ServiceStatus>,
}

/// Discovers running fog instances by scanning their IPC sockets.
fn discover_fog_instances() -> Vec<FogInstance> {
    let mut out = Vec::new();
    if let Ok(instances) = crate::ipc::find_instances() {
        for (pid, path) in instances {
            if let Ok(status) = crate::ipc::query_status(&path) {
                out.push(FogInstance {
                    pid,
                    script: status.script,
                    services: status.services,
                });
            }
        }
    }
    out
}

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
async fn serve_logs_stream(req: &Request<Incoming>) -> Response<RespBody> {
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
        let Ok(mut sock) = tokio::net::UnixStream::connect(&sock_path).await else {
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
fn sse_event(line: &str) -> String {
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
fn split_docker_log_line(line: &str) -> Option<(u64, &str)> {
    let (ts, body) = line.split_once(' ')?;
    let secs = parse_docker_timestamp(ts)?;
    Some((secs, body))
}

/// Parses a docker `--timestamps` prefix (`YYYY-MM-DDTHH:MM:SS[.fraction]Z`,
/// always UTC) into unix seconds. Returns `None` on malformed input.
fn parse_docker_timestamp(ts: &str) -> Option<u64> {
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
fn is_valid_container(name: &str) -> bool {
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
fn is_valid_service_name(name: &str) -> bool {
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

/// Parses a URL query string into a map of unquoted key/value pairs.
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(k.to_string(), v.to_string());
    }
    out
}

/// Renders the live-log viewer page with a picker of every reachable service,
/// grouped by project then worktree (shared infra last), matching the
/// directory page's grouping, plus a `fog` section listing running instances.
fn logs_page_html(entries: &[IndexEntry], instances: &[FogInstance], selected: &str) -> String {
    let groups = group_entries(entries);
    let mut aside = String::new();

    if !instances.is_empty() {
        aside.push_str("<div class=\"proj\">fog</div>\n");
        for inst in instances {
            let label = format!("{} · pid {}", inst.script, inst.pid);
            aside.push_str(&format!(
                "<div class=\"group\">{}</div>\n",
                html_escape(&label)
            ));
            for svc in &inst.services {
                aside.push_str(&format!(
                    "<button class=\"svc\" data-pid=\"{}\" data-service=\"{}\">{} \
                     <span class=\"host\">fog {}</span></button>\n",
                    inst.pid,
                    svc.name,
                    html_escape(&svc.name),
                    inst.pid
                ));
            }
            // A detached instance captures its own diagnostics in daemon.log.
            if crate::ipc::instance_log_dir(inst.pid)
                .join("daemon.log")
                .is_file()
            {
                aside.push_str(&format!(
                    "<button class=\"svc\" data-pid=\"{}\" data-service=\"daemon\">daemon \
                     <span class=\"host\">fog {} · logs</span></button>\n",
                    inst.pid, inst.pid
                ));
            }
            aside.push_str(&format!(
                "<button class=\"svc\" data-pid=\"{}\" data-service=\"proxy\">proxy \
                 <span class=\"host\">fog {} · request log</span></button>\n",
                inst.pid, inst.pid
            ));
        }
    }

    aside.push_str(
        &groups
            .into_iter()
            .map(|(project, groups)| {
                let groups_html = groups
                    .into_iter()
                    .map(|(group, list)| {
                        let buttons = list
                            .iter()
                            .map(|e| {
                                let active = if e.container == selected {
                                    " active"
                                } else {
                                    ""
                                };
                                format!(
                                    "<button class=\"svc{active}\" data-service=\"{}\">{}\
                                 <span class=\"host\">{}</span></button>",
                                    e.container, e.service, e.hostname
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("<div class=\"group\">{group}</div>\n{buttons}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("<div class=\"proj\">{project}</div>\n{groups_html}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );

    LOGS_PAGE.replace("__GROUPS__", &aside)
}

/// HTML-escapes text for safe interpolation into the picker markup.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One worktree (or `shared`) subgroup of the live-log picker.
type LogGroup<'a> = (String, Vec<&'a IndexEntry>);
/// A project section of the live-log picker: (project name, groups).
type ProjectLogGroups<'a> = Vec<(String, Vec<LogGroup<'a>>)>;

/// Groups entries by project then worktree (shared subgroup last), matching
/// [`generate_index`]'s grouping so the picker mirrors the directory page.
fn group_entries(entries: &[IndexEntry]) -> ProjectLogGroups<'_> {
    let mut projects: ProjectLogGroups<'_> = Vec::new();
    for e in entries {
        let group = if e.shared {
            "shared".to_string()
        } else {
            e.worktree.clone()
        };
        match projects.iter_mut().find(|(p, _)| *p == e.project) {
            Some((_, groups)) => match groups.iter_mut().find(|(g, _)| *g == group) {
                Some((_, list)) => list.push(e),
                None => groups.push((group, vec![e])),
            },
            None => projects.push((e.project.clone(), vec![(group, vec![e])])),
        }
    }
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
        for (_, list) in groups {
            list.sort_by(|a, b| a.service.cmp(&b.service));
        }
    }
    projects
}

/// The live-log viewer page (static shell; the service picker is injected
/// where `__GROUPS__` appears). Dependency-free: all streaming and ANSI
/// rendering happens in the inline script.
const LOGS_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>fog — live logs</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  html, body { height: 100%; }
  body { margin: 0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; color: #e6edf3; background: #0d1117; }
  .pane { display: flex; height: 100vh; }
  aside { width: 250px; min-width: 250px; overflow: auto; border-right: 1px solid #21262d; padding: .75rem .5rem 2rem; background: #010409; }
  aside h1 { font-size: 1rem; margin: 0 .25rem .15rem; }
  aside .back { font-size: .72rem; margin: 0 .25rem .4rem; }
  aside .back a { color: #58a6ff; text-decoration: none; }
  aside .hint { font-size: .75rem; color: #8b949e; margin: 0 .25rem .6rem; }
  .proj { font-size: .8rem; font-weight: 600; margin: .9rem .25rem .15rem; }
  .group { font-size: .68rem; color: #8b949e; text-transform: uppercase; letter-spacing: .06em; margin: .7rem .25rem .15rem; }
  .svc { display: block; width: 100%; text-align: left; padding: .35rem .5rem; margin: .1rem 0; background: none; border: none; border-radius: 6px; color: #c9d1d9; font: inherit; font-size: .85rem; cursor: pointer; }
  .svc:hover { background: #161b22; }
  .svc.active { background: #1f6feb; color: #fff; }
  .svc .host { display: block; font-size: .68rem; color: #8b949e; }
  .svc.active .host { color: #d0e0f7; }
  main { flex: 1; display: flex; flex-direction: column; min-width: 0; }
  .toolbar { display: flex; gap: .5rem; align-items: center; padding: .4rem .75rem; border-bottom: 1px solid #21262d; font-size: .75rem; color: #8b949e; }
  .toolbar button { font: inherit; color: #c9d1d9; background: #21262d; border: 1px solid #30363d; border-radius: 6px; padding: .2rem .55rem; cursor: pointer; }
  .toolbar button:hover { background: #30363d; }
  .toolbar button.off { color: #8b949e; }
  .toolbar .spacer { flex: 1; }
  .log { flex: 1; overflow: auto; background: #010409; padding: .6rem .9rem 2rem; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 12.5px; line-height: 1.45; white-space: pre-wrap; word-break: break-word; }
  .log div { min-height: 1.2em; }
  .log .meta { color: #8b949e; font-style: italic; }
</style>
</head>
<body>
<div class="pane">
  <aside>
    <h1>fog logs</h1>
    <div class="back"><a href="/">← running services</a></div>
    <p class="hint">pick a service to stream its container logs</p>
__GROUPS__
  </aside>
  <main>
    <div class="toolbar">
      <button id="follow">follow</button>
      <button id="copy">copy</button>
      <span class="spacer"></span>
      <span id="status"></span>
    </div>
    <div class="log" id="log"></div>
  </main>
</div>
<script>
(function () {
  'use strict';
  var logEl = document.getElementById('log');
  var followBtn = document.getElementById('follow');
  var copyBtn = document.getElementById('copy');
  var statusEl = document.getElementById('status');
  var TAIL = 200;
  var MAX_LINES = 2000;
  var service = null;
  var servicePid = null;
  var es = null;
  var follow = true;
  var stopped = false;

  var FG = {30:'#c9d1d9',31:'#ff7b72',32:'#3fb950',33:'#d29922',34:'#58a6ff',35:'#bc8cff',36:'#39c5cf',37:'#c9d1d9',90:'#f0f6fc',91:'#ffa198',92:'#56d364',93:'#e3b341',94:'#79c0ff',95:'#d2a8ff',96:'#56d4dd',97:'#f0f6fc'};
  var BG = {40:'#161b22',41:'#ff7b72',42:'#3fb950',43:'#d29922',44:'#58a6ff',45:'#bc8cff',46:'#39c5cf',47:'#c9d1d9',100:'#161b22',101:'#ff7b72',102:'#3fb950',103:'#d29922',104:'#58a6ff',105:'#bc8cff',106:'#39c5cf',107:'#c9d1d9'};

  function emptyStyle() { return {bold:false,dim:false,italic:false,underline:false,inverse:false,fg:'',bg:''}; }
  function css(s) {
    var out = '';
    if (s.fg) out += 'color:' + s.fg + ';';
    if (s.bg) out += 'background-color:' + s.bg + ';';
    if (s.bold) out += 'font-weight:700;';
    if (s.dim) out += 'opacity:.7;';
    if (s.italic) out += 'font-style:italic;';
    if (s.underline) out += 'text-decoration:underline;';
    if (s.inverse) out += 'filter:invert(1);';
    return out;
  }
  function hex(n) { return n.toString(16).padStart(2, '0'); }
  function xterm(n) {
    var basic = ['#000000','#cc0000','#4e9a06','#c4a000','#3465a4','#75507b','#06989a','#d3d7cf','#555753','#ef2929','#8ae234','#fce94f','#729fcf','#ad7fa8','#34e2e2','#eeeeec'];
    if (n < 16) return basic[n];
    if (n < 232) {
      var ramp = [0,95,135,175,215,255];
      var nn = n - 16;
      return '#' + hex(ramp[Math.floor(nn / 36)]) + hex(ramp[Math.floor((nn % 36) / 6)]) + hex(ramp[nn % 6]);
    }
    var v = 8 + (n - 232) * 10;
    return '#' + hex(v) + hex(v) + hex(v);
  }

  function applySGR(seq, cur) {
    var params = seq.split(';');
    if (params.length === 1 && params[0] === '') params = ['0'];
    var next = {bold:cur.bold,dim:cur.dim,italic:cur.italic,underline:cur.underline,inverse:cur.inverse,fg:cur.fg,bg:cur.bg};
    var i = 0;
    while (i < params.length) {
      var p = params[i];
      if (p === '0') return emptyStyle();
      if (p === '1') next.bold = true;
      else if (p === '2') next.dim = true;
      else if (p === '3') next.italic = true;
      else if (p === '4') next.underline = true;
      else if (p === '7') next.inverse = true;
      else if (p === '22') { next.bold = false; next.dim = false; }
      else if (p === '23') next.italic = false;
      else if (p === '24') next.underline = false;
      else if (p === '27') next.inverse = false;
      else if (p === '39') next.fg = '';
      else if (p === '49') next.bg = '';
      else if (p >= '30' && p <= '37') next.fg = FG[p];
      else if (p >= '90' && p <= '97') next.fg = FG[p];
      else if (p >= '40' && p <= '47') next.bg = BG[p];
      else if (p >= '100' && p <= '107') next.bg = BG[p];
      else if (p === '38' || p === '48') {
        var mode = params[++i];
        if (mode === '5') { var c = xterm(parseInt(params[++i], 10)); if (p === '38') next.fg = c; else next.bg = c; }
        else if (mode === '2') {
          var rgb = 'rgb(' + params[++i] + ',' + params[++i] + ',' + params[++i] + ')';
          if (p === '38') next.fg = rgb; else next.bg = rgb;
        }
      }
      i++;
    }
    return next;
  }

  function esc(c) {
    if (c === '<') return '&lt;';
    if (c === '>') return '&gt;';
    if (c === '&') return '&amp;';
    return c;
  }

  function parseLine(line) {
    var html = '';
    var text = '';
    var cur = emptyStyle();
    var open = false;
    var i = 0;
    var n = line.length;
    function closeSpan() { if (open) { html += '</span>'; open = false; } }
    while (i < n) {
      var c = line[i];
      if (c === '\x1b') {
        if (line[i + 1] === '[') {
          var j = i + 2;
          while (j < n && !(line.charCodeAt(j) >= 0x40 && line.charCodeAt(j) <= 0x7e)) j++;
          var finalByte = j < n ? line[j] : '';
          var seq = line.slice(i + 2, j);
          i = j < n ? j + 1 : n;
          if (finalByte === 'm') {
            var next = applySGR(seq, cur);
            var ns = css(next), os = css(cur);
            if (ns !== os) {
              closeSpan();
              cur = next;
              if (ns) { html += '<span style="' + ns + '">'; open = true; }
            }
          }
          continue;
        }
        i += 2;
        continue;
      }
      html += esc(c);
      text += c;
      i++;
    }
    closeSpan();
    return {html: html, text: text};
  }

  function scrollToBottom() { logEl.scrollTop = logEl.scrollHeight; }

  function trimPane() {
    while (logEl.childElementCount > MAX_LINES) logEl.removeChild(logEl.firstChild);
  }

  function appendLine(data, cls) {
    // Carriage returns overwrite the line (progress bars, spinners): keep only
    // the final segment, mirroring what a terminal shows.
    var parts = data.split('\r');
    var keep = parts[parts.length - 1];
    if (keep.indexOf('[fog] ') === 0) { stopped = true; }
    var line = document.createElement('div');
    if (cls) line.className = cls;
    var parsed = parseLine(keep);
    line.innerHTML = parsed.html || '&nbsp;';
    line.setAttribute('data-text', parsed.text);
    logEl.appendChild(line);
    trimPane();
    if (follow) scrollToBottom();
  }

  function clearPane() { logEl.innerHTML = ''; }

  function setStatus(t) { statusEl.textContent = t || ''; }

  function closeStream() { if (es) { es.close(); es = null; } }

  function openStream() {
    if (!service) return;
    stopped = false;
    setStatus('connecting…');
    var base = servicePid ? '/logs/stream?pid=' + encodeURIComponent(servicePid) + '&service=' + encodeURIComponent(service)
                          : '/logs/stream?service=' + encodeURIComponent(service);
    var url = base + '&tail=' + TAIL;
    es = new EventSource(url);
    es.onmessage = function (ev) {
      var d = ev.data;
      if (d.indexOf('[fog] stream ended') === 0) setStatus('stream ended');
      else if (d.indexOf('[fog] ') === 0) setStatus(d.slice(6));
      else setStatus('');
      appendLine(d, 'line');
    };
    es.onerror = function () {
      if (stopped) { closeStream(); return; }
      setStatus('reconnecting…');
    };
  }

  function switchService(name, pid) {
    service = name;
    servicePid = pid || null;
    closeStream();
    clearPane();
    var els = document.querySelectorAll('.svc');
    for (var i = 0; i < els.length; i++) {
      var same = els[i].getAttribute('data-service') === name && (els[i].getAttribute('data-pid') || null) === servicePid;
      els[i].classList.toggle('active', same);
    }
    if (!name) { setStatus(''); appendLine('[fog] select a service to stream its logs.', 'meta'); return; }
    appendLine('[fog] streaming ' + name + '…', 'meta');
    openStream();
  }

  var svcEls = document.querySelectorAll('.svc');
  for (var i = 0; i < svcEls.length; i++) {
    (function (el) {
      el.addEventListener('click', function () {
        switchService(el.getAttribute('data-service'), el.getAttribute('data-pid'));
      });
    })(svcEls[i]);
  }

  followBtn.addEventListener('click', function () {
    follow = !follow;
    followBtn.classList.toggle('off', !follow);
    if (follow) scrollToBottom();
  });

  copyBtn.addEventListener('click', function () {
    var lines = [];
    var children = logEl.children;
    for (var i = 0; i < children.length; i++) {
      var t = children[i].getAttribute('data-text');
      lines.push(t == null ? children[i].textContent : t);
    }
    var text = lines.join('\n');
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(function () {
        copyBtn.textContent = 'copied';
        setTimeout(function () { copyBtn.textContent = 'copy'; }, 1500);
      });
    } else {
      var ta = document.createElement('textarea');
      ta.value = text; document.body.appendChild(ta); ta.select();
      try { document.execCommand('copy'); } catch (e) {}
      document.body.removeChild(ta);
    }
  });

  logEl.addEventListener('scroll', function () {
    follow = (logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight) < 8;
    followBtn.classList.toggle('off', !follow);
  });

  var qs = new URLSearchParams(window.location.search);
  var initial = qs.get('service');
  if (initial) switchService(initial, null);
  else { setStatus(''); appendLine('[fog] select a service on the left to start streaming.', 'meta'); }
})();
</script>
</body>
</html>
"#;

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
            derive_group(&l, "gems-main-api-1", None),
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
            derive_group(&l, "gems-feature-x-api-1", None),
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
            derive_group(&l, "gems-postgres", None),
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
            derive_group(&app, "redfox-main-api-1", None),
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
            derive_group(&infra, "red-fox-infra-postgres-1", None),
            ("red-fox".into(), "shared".into(), true)
        );
    }

    #[test]
    fn test_derive_group_no_labels_falls_back_to_name() {
        let l = labels(&[]);
        let (p, w, s) = derive_group(&l, "barecontainer", None);
        assert_eq!(p, "barecontainer");
        assert_eq!(w, "main");
        assert!(!s);

        // A dashed name without a working-dir falls back to the base (prefix
        // before the first `-`).
        let (p, w, _) = derive_group(&l, "gems-main", None);
        assert_eq!(p, "gems");
        assert_eq!(w, "main");
    }

    #[test]
    fn test_derive_group_git_project_unifies_worktrees() {
        // admin/ and ui/ are separate worktrees of the same repo; the
        // git-derived project groups both under red-fox.
        let app = labels(&[
            (
                "com.docker.compose.project.working_dir",
                "/Users/naputt/code/red-fox/ui",
            ),
            ("com.docker.compose.project", "redfox-ui"),
        ]);
        let (p, w, s) = derive_group(&app, "redfox-ui-frontend-1", Some("red-fox"));
        assert_eq!((p.as_str(), w.as_str(), s), ("red-fox", "ui", false));

        // Shared infra of any worktree stays under the same project.
        let infra = labels(&[
            (
                "com.docker.compose.project.working_dir",
                "/Users/naputt/code/red-fox/ui/infra",
            ),
            ("com.docker.compose.project", "red-fox-infra"),
        ]);
        let (p, w, s) = derive_group(&infra, "red-fox-infra-postgres-1", Some("red-fox"));
        assert_eq!((p.as_str(), w.as_str(), s), ("red-fox", "shared", true));

        // Without the git override, the working-dir basename wins (old
        // behavior) — the override is what fixes the multi-worktree case.
        let (p, w, s) = derive_group(&app, "redfox-ui-frontend-1", None);
        assert_eq!((p.as_str(), w.as_str(), s), ("ui", "ui", false));
    }

    #[test]
    fn test_project_name_from_common_dir() {
        assert_eq!(
            project_name_from_common_dir("/Users/naputt/code/red-fox/.git"),
            "red-fox"
        );
        assert_eq!(
            project_name_from_common_dir("/Users/naputt/git/GEMS/.git"),
            "gems"
        );
        // A non-`.git` path uses its own basename.
        assert_eq!(
            project_name_from_common_dir("/repo/my-project"),
            "my-project"
        );
    }

    #[test]
    fn test_render_entry_http_vs_raw() {
        let http = IndexEntry {
            project: "gems".into(),
            worktree: "main".into(),
            shared: false,
            container: "gems-main-api-1".into(),
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
        assert!(row.contains("/logs?service=gems-main-api-1"));

        let raw = IndexEntry {
            project: "gems".into(),
            worktree: "shared".into(),
            shared: true,
            container: "gems-postgres".into(),
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
        assert!(row.contains("/logs?service=gems-postgres"));
    }

    #[test]
    fn test_parse_docker_timestamp() {
        // Known epoch.
        assert_eq!(parse_docker_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_docker_timestamp("2021-01-01T00:00:00Z"),
            Some(1_609_459_200)
        );
        assert_eq!(
            parse_docker_timestamp("2026-08-15T05:12:52Z"),
            Some(1_786_770_772)
        );
        // Fractional seconds are ignored (epoch seconds).
        assert_eq!(
            parse_docker_timestamp("2026-08-15T05:12:52.383339801Z"),
            Some(1_786_770_772)
        );
        // Malformed input.
        assert_eq!(parse_docker_timestamp(""), None);
        assert_eq!(parse_docker_timestamp("2026-08-15"), None);
        assert_eq!(parse_docker_timestamp("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_docker_timestamp("2026-08-15T24:00:00Z"), None);
        assert_eq!(parse_docker_timestamp("hello"), None);
    }

    #[test]
    fn test_split_docker_log_line_and_sse_event() {
        let (secs, body) =
            split_docker_log_line("2026-08-15T05:12:52.383339801Z hello world").unwrap();
        assert_eq!(secs, 1_786_770_772);
        assert_eq!(body, "hello world");

        let evt = sse_event("2026-08-15T05:12:52.383339801Z hello world");
        assert_eq!(evt, "id: 1786770772\ndata: hello world\n\n");

        // A line without a timestamp still streams, just without an id.
        let evt = sse_event("plain log line");
        assert_eq!(evt, "data: plain log line\n\n");
    }

    #[test]
    fn test_is_valid_container() {
        assert!(is_valid_container("redfox-main-api-1"));
        assert!(is_valid_container("fog-router-traefik"));
        assert!(is_valid_container("my.container_01-x"));
        assert!(!is_valid_container(""));
        assert!(!is_valid_container("../../etc/passwd"));
        assert!(!is_valid_container("-leading-dash"));
        assert!(!is_valid_container("a b"));
        assert!(!is_valid_container("a;rm -rf /"));
        assert!(!is_valid_container("a".repeat(256).as_str()));
    }

    #[test]
    fn test_parse_query() {
        let q = parse_query("service=abc&tail=50");
        assert_eq!(q.get("service").map(String::as_str), Some("abc"));
        assert_eq!(q.get("tail").map(String::as_str), Some("50"));
        assert_eq!(parse_query("").len(), 0);
    }

    #[test]
    fn test_logs_page_html() {
        let entries = vec![
            IndexEntry {
                project: "red-fox".into(),
                worktree: "main".into(),
                shared: false,
                container: "redfox-main-api-1".into(),
                service: "api".into(),
                hostname: "main.red-fox".into(),
                port: "8082".into(),
                published: vec![],
                raw_port: None,
                tls: true,
            },
            IndexEntry {
                project: "red-fox".into(),
                worktree: "shared".into(),
                shared: true,
                container: "red-fox-infra-postgres-1".into(),
                service: "postgres".into(),
                hostname: "main.postgres.red-fox".into(),
                port: String::new(),
                published: vec![],
                raw_port: None,
                tls: false,
            },
        ];
        let instances = vec![FogInstance {
            pid: 1234,
            script: "dev".into(),
            services: vec![
                crate::ipc::ServiceStatus {
                    name: "web".into(),
                    running: true,
                    health: "healthy".into(),
                },
                crate::ipc::ServiceStatus {
                    name: "worker".into(),
                    running: false,
                    health: "stopped".into(),
                },
            ],
        }];
        let html = logs_page_html(&entries, &instances, "redfox-main-api-1");
        assert!(html.contains("red-fox"));
        assert!(html.contains("data-service=\"redfox-main-api-1\""));
        assert!(html.contains("class=\"svc active\""));
        assert!(html.contains("data-service=\"red-fox-infra-postgres-1\""));
        // Fog instances appear first, with pid-addressable picker buttons.
        assert!(html.find("fog").unwrap() < html.find("red-fox").unwrap());
        assert!(html.contains("data-pid=\"1234\" data-service=\"web\""));
        assert!(html.contains("data-pid=\"1234\" data-service=\"proxy\""));
        // Shared infra sinks below the app group.
        assert!(html.find("main").unwrap() < html.find("shared").unwrap());
        // The page is self-contained JS/CSS.
        assert!(html.contains("new EventSource"));
        assert!(html.contains("</html>"));
    }

    #[test]
    fn test_is_valid_service_name() {
        assert!(is_valid_service_name("web"));
        assert!(is_valid_service_name("proxy"));
        assert!(is_valid_service_name("daemon"));
        assert!(is_valid_service_name("my service-v2"));
        assert!(!is_valid_service_name(""));
        assert!(!is_valid_service_name("../etc/passwd"));
        assert!(!is_valid_service_name("a/b"));
        assert!(!is_valid_service_name("a".repeat(256).as_str()));
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }
}
