//! Docker/compose and running-instance discovery for the index server.

use std::path::PathBuf;
use std::process::Command;

use super::{FogInstance, IndexEntry};

/// Extracts every reachable entry for one container from its labels: raw
/// `fog.expose` entries (reached by hostname + published host port) and
/// Traefik router host/port/tls entries. Returns an empty vector when the
/// container neither exposes via `fog.expose` nor defines a reachable Traefik
/// router — the caller then falls back to a label-only entry.
///
/// Shared by [`discover_entries`] (the directory page) and
/// [`discover_compose_containers`] (`/api/services`) so both report the same
/// routed URLs. `project`/`worktree`/`shared` are the already-derived group;
/// `published` and `raw_port` are the container's docker host bindings.
#[allow(clippy::too_many_arguments)] // grouped container facts; explicit args keep it pure/testable
pub(super) fn reachable_entries_from(
    labels: &std::collections::HashMap<String, String>,
    container_name: &str,
    project: String,
    worktree: String,
    shared: bool,
    service: String,
    published: Vec<String>,
    raw_port: Option<String>,
) -> Vec<IndexEntry> {
    let mut entries = Vec::new();
    let has_traefik = labels
        .keys()
        .any(|k| k.starts_with("traefik.http.routers."));
    let expose = labels.get("fog.expose").is_some_and(|v| v == "true");

    // Raw-TCP services exposed via `fog.expose` have no Traefik HTTP router;
    // they are reached by hostname + published host port.
    if expose {
        let Some(hostname) = labels
            .get("fog.hostname")
            .cloned()
            .map(|h| crate::ports::sanitize_hostname(&h))
        else {
            return entries;
        };
        let Some(raw_port) = raw_port.clone() else {
            return entries;
        };
        entries.push(IndexEntry {
            project: project.clone(),
            worktree: worktree.clone(),
            shared,
            container: container_name.to_string(),
            service: service.clone(),
            hostname,
            port: String::new(),
            published: published.clone(),
            raw_port: Some(raw_port),
            tls: false,
        });
        if !has_traefik {
            return entries;
        }
    }

    // First pass: collect service ports.
    let mut service_port: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (k, v) in labels {
        if let Some(rest) = k
            .strip_prefix("traefik.http.services.")
            .and_then(|r| r.strip_suffix(".loadbalancer.server.port"))
        {
            service_port.insert(rest.to_string(), v.clone());
        }
    }
    // Second pass: routers -> host(s) + port + tls. The router may name a
    // different service explicitly; default to the same-name service.
    for (k, v) in labels {
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
        for host in extract_hosts(v) {
            entries.push(IndexEntry {
                project: project.clone(),
                worktree: worktree.clone(),
                shared,
                container: container_name.to_string(),
                service: service.clone(),
                hostname: host,
                port: port.clone(),
                published: published.clone(),
                raw_port: raw_port.clone(),
                tls,
            });
        }
    }
    entries
}

/// Splits a branch-specific infra compose project into its repo prefix and
/// branch suffix, e.g. `red-fox-infra-main` → `("red-fox", "main")` and
/// `red-fox-infra-feat-branch` → `("red-fox", "feat-branch")`.
///
/// Returns `None` for genuinely shared infra with no branch suffix (e.g.
/// `red-fox-infra`, `gems-infra`) or for non-infra names. Underscores are
/// treated as dashes because compose normalizes separators.
pub(super) fn infra_branch(project_name: &str) -> Option<(String, String)> {
    let norm = project_name.to_ascii_lowercase().replace('_', "-");
    let (prefix, suffix) = norm.split_once("-infra-")?;
    if suffix.is_empty() {
        return None;
    }
    Some((prefix.to_string(), suffix.to_string()))
}

/// Derives the display project name, worktree group and shared-infra flag for a
/// container from its compose labels.
///
/// `git_project` — the git-common-dir-derived project name, when the compose
/// `working_dir` sits inside a repository — takes precedence over the
/// label-derived name. This groups all worktrees of the same repo (e.g.
/// `admin/` and `ui/` of red-fox) under one project, matching fog's own
/// instance identity.
///
/// Infra (`working_dir` ends in `infra/`) with a branch suffix in its compose
/// project (e.g. `red-fox-infra-feat-branch`) runs a dedicated container per
/// branch, so it groups under that branch (`shared == false`). Only a bare
/// `red-fox-infra` with no suffix stays under the `shared` group.
pub(super) fn derive_group(
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
    let wd_trimmed = wd.trim_end_matches(['/', '\\']);
    let is_infra = wd_trimmed.ends_with("/infra")
        || wd_trimmed.ends_with("\\infra")
        || wd_trimmed.eq_ignore_ascii_case("infra");
    // Branch-specific infra runs its own container per branch, e.g.
    // `red-fox-infra-main` or `red-fox-infra-feat-branch`. Only a bare
    // `red-fox-infra` (no branch suffix) is truly shared.
    let infra_branch_suffix = is_infra.then(|| infra_branch(project_name)).flatten();
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
                if let Some((prefix, _)) = infra_branch_suffix.as_ref()
                    && !prefix.is_empty()
                {
                    // The worktree checkout lives outside its git repo (e.g. an
                    // opencode worktree cache), so git resolution failed. The
                    // compose prefix before `-infra-` is the repo name — the
                    // worktree dir basename would be the branch (wrong).
                    prefix.clone()
                } else {
                    // Repo root is the parent of `infra/`.
                    let repo = std::path::Path::new(wd_trimmed)
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| project_name.to_string());
                    repo.to_lowercase()
                }
            } else {
                std::path::Path::new(wd)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_lowercase().to_owned())
                    .unwrap_or_else(|| project_name.to_string())
            }
        }
    };
    if is_infra {
        if let Some((_, raw_branch)) = infra_branch_suffix {
            let worktree = crate::ports::sanitize_hostname(&raw_branch)
                .split('.')
                .next()
                .unwrap_or("main")
                .to_string();
            (project, worktree, false)
        } else {
            (project, "shared".to_string(), true)
        }
    } else {
        let raw_worktree = project_name
            .split_once('-')
            .map(|(_, w)| w.to_string())
            .unwrap_or_else(|| "main".to_string());
        let worktree = crate::ports::sanitize_hostname(&raw_worktree)
            .split('.')
            .next()
            .unwrap_or("main")
            .to_string();
        (project, worktree, false)
    }
}

/// Resolves the display project name from the git repository containing a
/// compose working directory, using the same identity fog uses for instances
/// (the git common dir — shared by every worktree of the repo). Returns
/// `None` when the directory isn't in a git repo or git is unavailable.
pub(super) fn git_project_for(working_dir: &str) -> Option<String> {
    let common_dir = crate::project::detect(std::path::Path::new(working_dir))?;
    Some(project_name_from_common_dir(&common_dir))
}

/// Maps a git common-dir path (e.g. `/repo/.git`) to a display project name
/// (e.g. `repo`), lowercased to match the directory page's grouping.
pub(super) fn project_name_from_common_dir(common_dir: &str) -> String {
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
/// Hostnames are sanitized so stale containers with `feat/book.red-fox`
/// are returned as `feat-book.red-fox` rather than an invalid DNS name.
pub(super) fn extract_hosts(rule: &str) -> Vec<String> {
    rule.split('`')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| p.contains('.') || p.starts_with('{'))
        .map(crate::ports::sanitize_hostname)
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

/// Derives the first host port from already-fetched `docker port` output
/// (`published` from [`docker_ports`]) to avoid a second `docker port` subprocess.
fn raw_port_from_published(published: &[String]) -> Option<String> {
    for entry in published {
        // `published` entries are `host->container` e.g. `0.0.0.0:53012->5173/tcp`
        let host_part = entry.split("->").next().unwrap_or(entry);
        if let Some(port) = host_part.rsplit(':').next()
            && port.chars().all(|c| c.is_ascii_digit())
            && !port.is_empty()
        {
            return Some(port.to_string());
        }
    }
    None
}

/// Returns the first host port docker published for a container, if any (the
/// number after the host `:` in `docker port` output, e.g. `53012` from
/// `5173/tcp -> 0.0.0.0:53012`).
#[allow(dead_code)]
pub(super) fn docker_host_port(name: &str) -> Option<String> {
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
pub(super) fn container_labels(name: &str) -> Option<std::collections::HashMap<String, String>> {
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

/// Discovers every running compose-managed container. `/api/services` uses this
/// so the logs picker can stream from *any* service (e.g. `api`, `minio`) even
/// when it exposes no router route and sits off the router network.
///
/// Enumerates running compose containers. By default only Traefik-exposed
/// containers (via `fog.expose` or `traefik.http.routers.*`) are returned so
/// unrelated `docker ps` entries (e.g. a stray `mongodb`) do not pollute
/// `/api/services`. Pass `with_internal=true` to also include non-routed
/// compose services (useful for the logs picker to see `postgres` etc).
#[allow(dead_code)]
fn discover_compose_containers() -> Vec<IndexEntry> {
    discover_compose_containers_filtered(false)
}

fn is_running_fog_project(working_dir: &str, allowed: &std::collections::HashSet<PathBuf>) -> bool {
    if working_dir.is_empty() || allowed.is_empty() {
        return false;
    }
    // Canonicalize wd when possible; fall back to raw path comparison.
    let wd_path = PathBuf::from(working_dir);
    let wd_canon = wd_path.canonicalize().unwrap_or(wd_path.clone());
    // Direct match or allowed is prefix of wd (worktree subdir) or vice versa.
    for root in allowed {
        if wd_canon == *root {
            return true;
        }
        // Worktree subdir: wd inside allowed root
        if wd_canon.starts_with(root) {
            return true;
        }
        // Config dir may be subdir of wd (e.g. wd=/repo, config_dir=/repo/app with fog.json)
        // Check via try_canonical; cheap.
        if root.starts_with(&wd_canon) {
            return true;
        }
    }
    // Also allow when wd's fog.json lives under an allowed root via git common dir
    // grouping: check parent chain.
    false
}

fn discover_compose_containers_filtered(with_internal: bool) -> Vec<IndexEntry> {
    // Default entrypoint without running-instance scoping (used by tests). Delegates
    // to the scoped variant with an allowlist derived from live instances when called
    // via api_services_with_query.
    let allowed = {
        let instances = discover_fog_instances();
        let mut set = std::collections::HashSet::new();
        for inst in instances {
            if let Some(dir) = inst.config_dir {
                let p = PathBuf::from(dir);
                set.insert(p.canonicalize().unwrap_or(p));
            }
        }
        set
    };
    discover_compose_containers_filtered_scoped(with_internal, &allowed)
}

pub(super) fn discover_compose_containers_filtered_scoped(
    with_internal: bool,
    allowed_roots: &std::collections::HashSet<PathBuf>,
) -> Vec<IndexEntry> {
    let mut names: Vec<String> = Vec::new();
    let out = match Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() && !names.contains(&line.to_string()) {
            names.push(line.to_string());
        }
    }

    // Cache the git-derived project per working directory (shared by every
    // worktree of the same repo), matching `discover_entries`.
    let mut git_projects: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();

    let mut entries = Vec::new();
    for name in names {
        let Some(labels) = container_labels(&name) else {
            continue;
        };
        let Some(service) = labels.get("com.docker.compose.service").cloned() else {
            // Not a compose-managed container (e.g. the router, or an
            // unrelated docker container): not a selectable service.
            continue;
        };
        // Only containers whose compose working_dir belongs to a currently
        // running fog project are considered. This hides stray compose projects
        // like `neo-backend` when no fog instance is running for that dir,
        // even with ?withInternal=1 (which otherwise would include all non-Traefik
        // containers such as postgres).
        if let Some(wd) = labels.get("com.docker.compose.project.working_dir") {
            if !is_running_fog_project(wd, allowed_roots) {
                continue;
            }
        } else if !allowed_roots.is_empty() {
            // No working_dir label but we have running fog projects: treat as unrelated.
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
        let published = docker_ports(&name);
        let raw_port = raw_port_from_published(&published);
        let reachable = reachable_entries_from(
            &labels,
            &name,
            project.clone(),
            worktree.clone(),
            shared,
            service.clone(),
            published.clone(),
            raw_port.clone(),
        );
        if reachable.is_empty() {
            if !with_internal {
                // Traefik-only by default: skip containers with no router/fog.expose
                // (e.g. unrelated mongo). Logs picker can opt-in via ?withInternal=1.
                continue;
            }
            // withInternal: keep non-routed services so logs picker can see postgres etc.
            let hostname = labels
                .get("fog.hostname")
                .cloned()
                .unwrap_or_else(|| service.clone());
            entries.push(IndexEntry {
                project,
                worktree,
                shared,
                container: name.clone(),
                service,
                hostname,
                port: String::new(),
                published,
                raw_port,
                tls: false,
            });
        } else {
            entries.extend(reachable);
        }
    }
    entries
}

/// Discovers running fog instances by scanning their IPC sockets.
pub(super) fn discover_fog_instances() -> Vec<FogInstance> {
    let mut out = Vec::new();
    if let Ok(instances) = crate::ipc::find_instances() {
        for (pid, path) in instances {
            if let Ok(status) = crate::ipc::query_status(&path) {
                out.push(FogInstance {
                    pid,
                    script: status.script,
                    services: status.services,
                    config_dir: status.config_dir,
                    project: status.project,
                    branch: status.branch,
                    ports: status.ports,
                    native_routes: status.native_routes,
                });
            }
        }
    }
    out
}
