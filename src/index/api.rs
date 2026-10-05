//! `/api/*` JSON handlers: services, status, health, config.

use std::path::PathBuf;

use hyper::Response;

use super::{
    FogInstance, IndexEntry, RespBody, discover_compose_containers_filtered_scoped,
    discover_fog_instances, json_response, parse_query, project_icons,
    project_name_from_common_dir,
};

/// Builds a reachable URL for a service entry: HTTP services use
/// `{scheme}://{hostname}/`; raw-TCP services exposed via `fog.expose` use
/// `http://{hostname}:{raw_port}/`.
pub(super) fn entry_url(e: &IndexEntry) -> String {
    if e.port.is_empty() {
        match &e.raw_port {
            Some(rp) => format!("http://{}:{}/", e.hostname, rp),
            None => format!("http://{}/", e.hostname),
        }
    } else {
        let scheme = if e.tls { "https" } else { "http" };
        format!("{scheme}://{}/", e.hostname)
    }
}

/// One declared endpoint (endpoint) of an [`ApiService`].
#[derive(serde::Serialize, Clone)]
pub(super) struct ApiEndpoint {
    /// Endpoint display name.
    name: String,
    /// Externally reachable URL for this endpoint, empty when it declares no
    /// routable `host`.
    url: String,
    /// Host-published port, empty when not declared.
    port: String,
    /// Optional PathPrefix the route combines with the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    path_prefix: Option<String>,
    /// Per-endpoint health state (`healthy`/`unhealthy`/...).
    health: String,
}

/// A single service as reported by `GET /api/services`.
#[derive(serde::Serialize, Clone)]
pub(super) struct ApiService {
    project: String,
    worktree: String,
    service: String,
    /// Docker container name (e.g. `redfox-main-api-1`). `/logs/stream`
    /// streams a container's logs by this name, so the picker must pass it
    /// rather than the compose service name.
    container: String,
    status: String,
    url: String,
    ports: Vec<String>,
    health: String,
    /// Fog PID for native (non-docker) services. When present, logs are
    /// streamed via `?pid=<pid>&service=<name>` (fog IPC) instead of docker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    /// Optional project icon (image URL or data URI) from the owning project's
    /// `fog.json` (`project.icon`). Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    /// Declared endpoint endpoints of this service. Omitted when the
    /// service exposes a single implicit endpoint (the common case).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    endpoints: Vec<ApiEndpoint>,
}

/// Converts a discovered [`IndexEntry`] into its JSON shape. Discovery only sees
/// running containers, so `status` is `running`; per-service `health` is not
/// available from docker here (it is reported by fog instances via `/api/health`).
pub(super) fn api_service_from(e: IndexEntry) -> ApiService {
    let url = entry_url(&e);
    let ports = e.published;
    ApiService {
        project: e.project,
        worktree: e.worktree,
        service: e.service,
        container: e.container,
        status: "running".to_string(),
        url,
        ports,
        health: "unknown".to_string(),
        pid: None,
        icon: None,
        endpoints: Vec::new(),
    }
}

/// Builds the declared endpoint list for one instance service.
///
/// Resolves each tagged endpoint route's host/port into a URL. When a
/// endpoint declares no route, the matching docker-discovered entry (matched
/// by project/worktree/name) supplies its URL and published ports — so a
/// compose service that routes itself via Traefik labels (or exposes raw TCP
/// via `fog.expose`) keeps its link after being nested under the parent.
///
/// Health comes from the instance's per-endpoint reading when it is known,
/// otherwise from the parent service's health (a endpoint without its own
/// `health_check` inherits the parent's).
fn api_endpoints_for(
    inst: &FogInstance,
    service: &str,
    docker: &[ApiService],
    project: &str,
    worktree: &str,
) -> Vec<ApiEndpoint> {
    let svc_status = inst.services.iter().find(|s| s.name == service);
    let parent_health = svc_status
        .map(|s| s.health.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let mut out = Vec::new();
    for route in &inst.native_routes {
        let Some(endpoint_name) = route.endpoint.as_deref() else {
            continue;
        };
        if route.service != service {
            continue;
        }
        // Resolve host/port templates with this instance's PortMap+branch. A
        // endpoint may declare neither (display + health only): url/port
        // stay empty.
        let host = crate::ports::resolve_template(&route.host, &inst.ports, inst.branch.as_deref())
            .map(|h| crate::ports::sanitize_hostname(&h))
            .unwrap_or_default();
        let mut port =
            crate::ports::resolve_template(&route.port, &inst.ports, inst.branch.as_deref())
                .unwrap_or_default();
        let mut url = if host.is_empty() {
            String::new()
        } else {
            format!("https://{}/", host)
        };
        // No fog-generated route: inherit the matching docker entry's URL/ports
        // (e.g. a compose service already routed by its own Traefik labels).
        // Prefer an exact worktree match; fall back to the sole entry for this
        // project+service (covers branch-agnostic/shared compose projects).
        if url.is_empty() {
            let matches: Vec<&ApiService> = docker
                .iter()
                .filter(|d| d.project.eq_ignore_ascii_case(project) && d.service == endpoint_name)
                .collect();
            let chosen = matches
                .iter()
                .find(|d| d.worktree == worktree)
                .copied()
                .or_else(|| (matches.len() == 1).then(|| matches[0]));
            if let Some(d) = chosen {
                url = d.url.clone();
                if port.is_empty() && !d.ports.is_empty() {
                    port = d.ports.join(", ");
                }
            }
        }
        let health = svc_status
            .and_then(|s| s.endpoints.iter().find(|ss| ss.name == endpoint_name))
            .map(|ss| ss.health.clone())
            .filter(|h| h != "unknown")
            .unwrap_or_else(|| parent_health.clone());
        out.push(ApiEndpoint {
            name: endpoint_name.to_string(),
            url,
            port,
            path_prefix: route.path_prefix.clone(),
            health,
        });
    }
    out
}

/// `GET /api/services`: every running compose service as JSON, so the SPA logs
/// picker can stream logs from any container. Native (non-docker) services
/// started via `ports` + `native_routes` are synthesized from fog instances so
/// they appear alongside docker services in the Services UI.
///
/// Query `?withInternal=1` (or `?internal=1`) opts into non-Traefik compose
/// services (e.g. `postgres`) so the logs picker can still see them. Default
/// is Traefik-only (`fog.expose` or `traefik.http.routers.*`) to avoid
/// unrelated `docker ps` entries like a stray `mongodb`.
/// Derives the `(project, worktree)` display group for an instance.
///
/// `project` keeps its original case; callers that compare against docker rows
/// lowercase it themselves. `worktree` is the branch sanitized down to a single
/// DNS label (`sanitize_hostname(...).split('.').next()`).
fn instance_group(inst: &FogInstance) -> (String, String) {
    let project = inst
        .project
        .as_deref()
        .map(project_name_from_common_dir)
        .unwrap_or_else(|| inst.script.clone());
    let raw_worktree = inst.branch.clone().unwrap_or_else(|| "default".to_string());
    let worktree = crate::ports::sanitize_hostname(&raw_worktree)
        .split('.')
        .next()
        .unwrap_or("default")
        .to_string();
    (project, worktree)
}

/// Declared-endpoint keys: `(project, worktree, endpoint)` for exact matches and
/// `(project, endpoint)` for branch-agnostic compose projects grouped as `shared`.
type DeclaredEndpoints = (
    std::collections::HashSet<(String, String, String)>,
    std::collections::HashSet<(String, String)>,
);

/// Collects the declared-endpoint keys for every instance, lowercasing the
/// project so it matches docker-derived `ApiService.project`.
fn declared_endpoint_keys(instances: &[FogInstance]) -> DeclaredEndpoints {
    let mut exact = std::collections::HashSet::new();
    let mut shared = std::collections::HashSet::new();
    for inst in instances {
        let (project, worktree) = instance_group(inst);
        let project = project.to_lowercase();
        for route in &inst.native_routes {
            if let Some(sub) = &route.endpoint {
                exact.insert((project.clone(), worktree.clone(), sub.clone()));
                shared.insert((project.clone(), sub.clone()));
            }
        }
    }
    (exact, shared)
}

/// Suppresses docker rows whose `(project, worktree, service)` is a declared
/// endpoint: those are nested under their parent instead of appearing as
/// separate top-level rows. A branch-agnostic compose project (e.g. a bare
/// `gems-infra` with no branch suffix) is grouped by docker under the `shared`
/// worktree even though its owning instance reports a real branch; for those,
/// match on project+service alone so they are still nested.
fn suppress_declared_docker(
    all_docker: &[ApiService],
    keys: &DeclaredEndpoints,
) -> Vec<ApiService> {
    let (exact, shared) = keys;
    all_docker
        .iter()
        .filter(|e| {
            let proj = e.project.to_lowercase();
            let exact_match =
                exact.contains(&(proj.clone(), e.worktree.clone(), e.service.clone()));
            let shared_match =
                e.worktree == "shared" && shared.contains(&(proj, e.service.clone()));
            !(exact_match || shared_match)
        })
        .cloned()
        .collect()
}

/// Adds top-level rows for an instance's running native routes. Endpoint routes
/// are skipped here — they are nested under their parent by
/// [`attach_declared_endpoints`].
fn synthesize_native_routes(
    list: &mut Vec<ApiService>,
    inst: &FogInstance,
    project: &str,
    worktree: &str,
) {
    // Build a map of service name -> health for quick lookup.
    let health_map: std::collections::HashMap<&str, &str> = inst
        .services
        .iter()
        .map(|s| (s.name.as_str(), s.health.as_str()))
        .collect();
    for route in &inst.native_routes {
        // Endpoint routes are nested under their parent (below), never listed
        // as separate top-level services.
        if route.endpoint.is_some() {
            continue;
        }
        let Some(svc_status) = inst.services.iter().find(|s| s.name == route.service) else {
            continue;
        };
        if !svc_status.running {
            continue;
        }
        // Resolve host and port templates with this instance's PortMap+branch.
        let host = match crate::ports::resolve_template(
            &route.host,
            &inst.ports,
            inst.branch.as_deref(),
        ) {
            Ok(h) => crate::ports::sanitize_hostname(&h),
            Err(_) => continue,
        };
        let port_str = match crate::ports::resolve_template(
            &route.port,
            &inst.ports,
            inst.branch.as_deref(),
        ) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let port: u16 = match port_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Native routes are always TLS-enabled (see router.rs), so use https.
        let url = format!("https://{}/", host);
        let health = health_map
            .get(route.service.as_str())
            .copied()
            .unwrap_or("unknown");
        // Avoid duplicating a docker entry that already covers this host (e.g. if a
        // service is both docker and native in different worktrees, keep both).
        if list
            .iter()
            .any(|e| e.service == route.service && e.worktree == worktree && e.project == project)
        {
            continue;
        }
        list.push(ApiService {
            project: project.to_string(),
            worktree: worktree.to_string(),
            service: route.service.clone(),
            container: format!("fog-{}-{}", inst.pid, route.service),
            status: "running".to_string(),
            url,
            ports: vec![format!("0.0.0.0:{}->{}/tcp", port, port)],
            health: health.to_string(),
            pid: Some(inst.pid),
            icon: None,
            endpoints: Vec::new(),
        });
    }
}

/// Attaches declared endpoints to their parent service's entry, creating the
/// parent entry when docker/native discovery did not list it (e.g. a
/// compose-stack launcher that is not itself a container). Also adds running
/// native services that have no native route so they still appear in the
/// directory.
fn attach_declared_endpoints(
    list: &mut Vec<ApiService>,
    inst: &FogInstance,
    project: &str,
    worktree: &str,
    all_docker: &[ApiService],
    docker_entries: &[IndexEntry],
) {
    for svc in &inst.services {
        if !svc.running {
            continue;
        }
        let subs = api_endpoints_for(inst, &svc.name, all_docker, project, worktree);
        if !subs.is_empty() {
            if let Some(existing) = list
                .iter_mut()
                .find(|e| e.service == svc.name && e.worktree == worktree && e.project == project)
            {
                if existing.endpoints.is_empty() {
                    existing.endpoints = subs;
                }
            } else {
                list.push(ApiService {
                    project: project.to_string(),
                    worktree: worktree.to_string(),
                    service: svc.name.clone(),
                    container: format!("fog-{}-{}", inst.pid, svc.name),
                    status: "running".to_string(),
                    url: String::new(),
                    ports: Vec::new(),
                    health: svc.health.clone(),
                    pid: Some(inst.pid),
                    icon: None,
                    endpoints: subs,
                });
            }
            continue;
        }
        // Native services with a route are already listed above.
        if inst.native_routes.iter().any(|r| r.service == svc.name) {
            continue;
        }
        // Show running native services not already listed.
        let already = list
            .iter()
            .any(|e| e.service == svc.name && e.worktree == worktree && e.project == project);
        if already {
            continue;
        }
        // A service that docker already reported is listed by docker, not here.
        let is_docker = docker_entries
            .iter()
            .any(|e| e.service == svc.name && e.worktree == worktree);
        if is_docker {
            continue;
        }
        list.push(ApiService {
            project: project.to_string(),
            worktree: worktree.to_string(),
            service: svc.name.clone(),
            container: format!("fog-{}-{}", inst.pid, svc.name),
            status: "running".to_string(),
            url: String::new(),
            ports: Vec::new(),
            health: svc.health.clone(),
            pid: Some(inst.pid),
            icon: None,
            endpoints: Vec::new(),
        });
    }
}

pub(super) fn api_services_with_query(_network: &str, query: Option<&str>) -> Response<RespBody> {
    let with_internal = query
        .map(parse_query)
        .map(|m| {
            m.get("withInternal")
                .or_else(|| m.get("with_internal"))
                .or_else(|| m.get("internal"))
                .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        })
        .unwrap_or(false);
    // Collect running fog roots once and reuse for docker filtering and native synthesis.
    let instances = discover_fog_instances();
    let icons = project_icons(&instances);
    let allowed_roots: std::collections::HashSet<PathBuf> = instances
        .iter()
        .filter_map(|i| i.config_dir.as_deref().map(PathBuf::from))
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect();
    let docker_entries = discover_compose_containers_filtered_scoped(with_internal, &allowed_roots);
    // Every docker-discovered service, before suppression. Used both for the
    // top-level list and to let a declared endpoint inherit the URL/ports of
    // a matching compose service that manages its own routing.
    let all_docker: Vec<ApiService> = docker_entries
        .iter()
        .cloned()
        .map(api_service_from)
        .collect();
    let declared = declared_endpoint_keys(&instances);
    let mut list = suppress_declared_docker(&all_docker, &declared);
    for inst in &instances {
        let (project, worktree) = instance_group(inst);
        synthesize_native_routes(&mut list, inst, &project, &worktree);
        attach_declared_endpoints(
            &mut list,
            inst,
            &project,
            &worktree,
            &all_docker,
            &docker_entries,
        );
    }
    // Attach each project's configured icon (if any) to every service in it, so
    // the Services UI can group and show it once per project card.
    for svc in &mut list {
        if svc.icon.is_none() {
            svc.icon = icons.get(&svc.project).cloned();
        }
    }
    json_response(&list)
}

/// One running fog instance as reported by `GET /api/status`.
#[derive(serde::Serialize)]
struct ApiInstance {
    pid: u32,
    script: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    services: Vec<crate::ipc::ServiceStatus>,
}

/// `GET /api/status`: running fog instances (pid, script, service statuses).
pub(super) fn api_status() -> Response<RespBody> {
    let list: Vec<ApiInstance> = discover_fog_instances()
        .into_iter()
        .map(|i| ApiInstance {
            pid: i.pid,
            script: i.script,
            project: i.project.map(|p| project_name_from_common_dir(&p)),
            branch: i.branch,
            services: i.services,
        })
        .collect();
    json_response(&serde_json::json!({ "instances": list }))
}

/// `GET /api/health`: per-service health across running fog instances.
pub(super) fn api_health() -> Response<RespBody> {
    let mut health = Vec::new();
    for inst in discover_fog_instances() {
        let proj = inst.project.as_deref().map(project_name_from_common_dir);
        for svc in inst.services {
            health.push(serde_json::json!({
                "pid": inst.pid,
                "script": inst.script,
                "project": proj,
                "branch": inst.branch,
                "service": svc.name,
                "running": svc.running,
                "health": svc.health,
            }));
        }
    }
    json_response(&serde_json::json!({ "health": health }))
}

/// `GET /api/config`: a summary of the loaded fog configuration.
pub(super) fn api_config() -> Response<RespBody> {
    let cfg = load_runtime_config();
    let mut script_names: Vec<&String> = cfg.scripts.keys().collect();
    script_names.sort();
    json_response(&serde_json::json!({
        "config": {
            "scripts": script_names,
            "max_scrollback": cfg.max_scrollback,
            "sidebar": cfg.sidebar.as_ref().map(|s| serde_json::json!({
                "min_width": s.min_width,
                "max_width": s.max_width,
            })),
            "theme": cfg.theme.is_some(),
            "dnsmasq": cfg.dnsmasq.as_ref().map(|d| serde_json::json!({
                "domains": d.domains,
                "address": d.address,
                "port": d.port,
            })),
            "router": cfg.router.as_ref().map(|r| serde_json::json!({
                "shared_network": r.shared_network,
                "index_port": r.index_port,
                "tls_enabled": r.tls.enabled,
            })),
        }
    }))
}

/// Loads the fog config for the `/api/config` and `/api/scripts` endpoints,
/// best-effort: tries `FOG_CONFIG_PATH`, then `./fog.json`, then
/// `~/.config/fog/fog.json`. Returns an empty config when none is readable.
pub fn load_runtime_config() -> crate::config::Config {
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        std::env::var("FOG_CONFIG_PATH")
            .ok()
            .map(std::path::PathBuf::from),
        Some(std::path::PathBuf::from("fog.json")),
        (!home.is_empty())
            .then(|| std::path::PathBuf::from(format!("{home}/.config/fog/fog.json"))),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_file()
            && let Ok(cfg) = crate::config::load(&candidate)
        {
            return cfg;
        }
    }
    crate::config::Config {
        scripts: std::collections::HashMap::new(),
        ports: None,
        native_routes: None,
        max_scrollback: None,
        sidebar: None,
        theme: None,
        dnsmasq: None,
        router: None,
        index: None,
        project: None,
    }
}
