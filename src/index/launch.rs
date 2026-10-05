//! Launch flow: `GET /api/launch/targets` discovery and `POST /api/launch`
//! spawning, plus the git-aware project/worktree grouping.

use hyper::{Response, StatusCode};
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::{
    RespBody, api_error, api_not_found, container_labels, discover_fog_instances, json_response,
    project_name_from_common_dir,
};

/// A launchable worktree (or the single non-git fallback) reported by
/// `GET /api/launch/targets`.
#[derive(serde::Serialize)]
pub(super) struct LaunchWorktree {
    pub(super) path: String,
    pub(super) branch: Option<String>,
    pub(super) scripts: Vec<String>,
}

/// A launchable project reported by `GET /api/launch/targets`.
#[derive(serde::Serialize)]
pub(super) struct LaunchProject {
    pub(super) path: String,
    pub(super) name: String,
    pub(super) worktrees: Vec<LaunchWorktree>,
}

/// The response body for `GET /api/launch/targets`.
#[derive(serde::Serialize)]
pub(super) struct LaunchTargets {
    pub(super) projects: Vec<LaunchProject>,
}

/// Collects the compose `working_dir` roots of every running compose
/// container (`com.docker.compose.project.working_dir` label). These are the
/// config dirs of projects currently up, so they are launchable even when no
/// `fog` instance is currently running for them.
fn compose_project_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let out = match Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return roots,
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        if let Some(labels) = container_labels(name)
            && let Some(wd) = labels.get("com.docker.compose.project.working_dir")
        {
            roots.push(PathBuf::from(wd));
        }
    }
    roots
}

/// Reads the sorted script names from a worktree's `fog.json`, best-effort.
/// Returns an empty array when the config is missing or unparseable, so one
/// bad project can never fail the whole discovery request.
fn worktree_scripts(path: &std::path::Path) -> Vec<String> {
    let cfg_path = path.join("fog.json");
    if !cfg_path.is_file() {
        return Vec::new();
    }
    let mut scripts: Vec<String> = match crate::config::load(&cfg_path) {
        Ok(cfg) => cfg.scripts.keys().cloned().collect(),
        Err(_) => return Vec::new(),
    };
    scripts.sort();
    scripts
}

/// Builds the launchable targets: one project per unique (canonicalized)
/// config dir, drawn from running fog instances and running compose
/// containers, each with its git worktrees (or a single non-git fallback).
fn discover_launch_targets() -> LaunchTargets {
    // Collect and canonicalize candidate roots, deduplicating by canonical
    // path so a project running both as a fog instance and a compose project
    // is listed once.
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |roots: &mut Vec<PathBuf>, p: &std::path::Path| {
        let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if seen.insert(canon.clone()) {
            roots.push(canon);
        }
    };

    for inst in discover_fog_instances() {
        if let Some(cfg) = inst.config_dir {
            push(&mut roots, std::path::Path::new(&cfg));
        }
    }
    for root in compose_project_roots() {
        push(&mut roots, &root);
    }

    roots.sort();
    build_launch_targets(&roots, crate::project::detect)
}

/// Groups candidate launch roots into projects by git repository identity:
/// every root inside the same repo collapses to one project (named after the
/// repo, e.g. `red-fox`) whose `worktrees` list all of that repo's worktrees.
/// Roots outside a git repository stay standalone single-worktree projects.
///
/// `detect` maps a root to its git common-dir (`None` for non-git roots), and
/// is injectable so the grouping logic is testable without a real git repo.
pub(super) fn build_launch_targets(
    roots: &[PathBuf],
    detect: impl Fn(&std::path::Path) -> Option<String>,
) -> LaunchTargets {
    let mut by_common: std::collections::BTreeMap<String, Vec<PathBuf>> =
        std::collections::BTreeMap::new();
    let mut non_git: Vec<PathBuf> = Vec::new();
    for root in roots {
        match detect(root) {
            Some(common_dir) => by_common.entry(common_dir).or_default().push(root.clone()),
            None => non_git.push(root.clone()),
        }
    }
    let mut projects: Vec<LaunchProject> = by_common
        .into_iter()
        .map(|(common_dir, group)| launch_project_for_repo(&common_dir, &group))
        .chain(non_git.iter().map(|root| launch_project_for(root)))
        .collect();
    // Sort projects by name for a stable, readable listing.
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    LaunchTargets { projects }
}

/// Builds a single launchable project from a canonicalized root path: its
/// git worktrees (or a single non-git fallback entry) and each worktree's
/// scripts. Factored out so tests can exercise it with a temp dir.
pub(super) fn launch_project_for(root: &std::path::Path) -> LaunchProject {
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());

    let worktrees: Vec<LaunchWorktree> = match crate::worktree::list(root) {
        Some(list) => list
            .into_iter()
            .map(|wt| LaunchWorktree {
                path: wt.path.to_string_lossy().into_owned(),
                branch: wt.branch,
                scripts: worktree_scripts(&wt.path),
            })
            .collect(),
        // Non-git (or git unavailable): a single entry pointing at the
        // root itself.
        None => vec![LaunchWorktree {
            path: root.to_string_lossy().into_owned(),
            branch: None,
            scripts: worktree_scripts(root),
        }],
    };

    LaunchProject {
        path: root.to_string_lossy().into_owned(),
        name,
        worktrees,
    }
}

/// Builds a single launchable project for one git repository, given its
/// common-dir identity and the config-dir roots inside it. Lists *all* of the
/// repo's worktrees (so `admin`/`ui`/`infra` appear as one project, not
/// three) and names the project after the repo. Falls back to a single
/// worktree at the representative root if the repo cannot be enumerated.
fn launch_project_for_repo(common_dir: &str, roots: &[PathBuf]) -> LaunchProject {
    let name = project_name_from_common_dir(common_dir);
    // Any root inside the repo can enumerate all of its worktrees.
    let rep = &roots[0];
    let worktrees: Vec<LaunchWorktree> = match crate::worktree::list(rep) {
        Some(list) => list
            .into_iter()
            .map(|wt| LaunchWorktree {
                path: wt.path.to_string_lossy().into_owned(),
                branch: wt.branch,
                scripts: worktree_scripts(&wt.path),
            })
            .collect(),
        None => vec![LaunchWorktree {
            path: rep.to_string_lossy().into_owned(),
            branch: None,
            scripts: worktree_scripts(rep),
        }],
    };
    // Prefer the main worktree's path as the project path, else the first
    // worktree, else the representative root.
    let path = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some("main"))
        .map(|w| w.path.clone())
        .or_else(|| worktrees.first().map(|w| w.path.clone()))
        .unwrap_or_else(|| rep.to_string_lossy().into_owned());
    LaunchProject {
        path,
        name,
        worktrees,
    }
}

/// `GET /api/launch/targets`: the launchable projects, worktrees, and scripts.
/// Any method other than GET is treated like an unknown route (404).
pub(super) fn api_launch_targets_method(method: &hyper::Method) -> Response<RespBody> {
    if method != hyper::Method::GET {
        return api_not_found();
    }
    json_response(&discover_launch_targets())
}

/// Request body for `POST /api/launch`.
#[derive(serde::Deserialize)]
struct LaunchBody {
    /// Absolute path to a config directory (or to a `fog.json` file).
    config_dir: String,
    /// Name of the script to run.
    script: String,
    /// Optional branch to launch; resolves to that branch's worktree.
    #[serde(default)]
    branch: Option<String>,
}

/// How long a launched daemon may take to become ready before we give up.
const LAUNCH_READY_TIMEOUT_SECS: u64 = 60;
/// How long we poll for readiness (ms).
const LAUNCH_READY_POLL_MS: u64 = 100;

/// Resolves a launch request's effective config path and validates the script
/// against the config's `scripts`, returning a human-readable error message on
/// failure. Mirrors `run_script`'s config resolution so the web UI launches
/// exactly what the CLI would.
fn resolve_launch_target(body: &LaunchBody) -> Result<PathBuf, String> {
    let config_dir = std::path::Path::new(&body.config_dir);
    // Accept a directory (config dir) or a direct path to `fog.json`.
    let config_path = if config_dir.is_dir() {
        config_dir.join("fog.json")
    } else {
        config_dir.to_path_buf()
    };
    // The config dir must exist (or the `<dir>/fog.json` path must exist).
    if !config_path.exists() {
        return Err(format!("config directory not found: {}", body.config_dir));
    }

    // If a branch is requested, resolve it to a worktree and point the config
    // at that worktree's `fog.json` (the branch may live in another worktree
    // whose config differs).
    let effective_config =
        if let Some(branch) = body.branch.as_deref().filter(|b| !b.trim().is_empty()) {
            match crate::worktree::resolve(config_dir, branch) {
                Some(wt) => wt.path.join("fog.json"),
                None => return Err(format!("no worktree is checked out on branch '{branch}'")),
            }
        } else {
            config_path
        };

    let cfg = crate::config::load(&effective_config)
        .map_err(|_| format!("could not read config '{}'", effective_config.display()))?;
    if !cfg.scripts.contains_key(&body.script) {
        return Err(format!("unknown script '{}'", body.script));
    }
    Ok(effective_config)
}

/// Spawns `fog <script>` detached (mirroring `daemonize`), waits until its IPC
/// socket serves a status reply, and returns its PID. Returns `io::Error` on
/// spawn failure or if the child exits / never becomes ready during startup.
fn spawn_detached(config_path: &std::path::Path, script: &str) -> io::Result<u32> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fog"));
    let mut cmd = Command::new(&exe);
    cmd.arg("--config")
        .arg(config_path)
        .arg(script)
        .env("FOG_DAEMON_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Detach from the controlling terminal / session, exactly like
    // `daemonize`.
    crate::process::detach_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::other(format!("could not start detached fog: {e}")))?;
    let pid = child.id();

    // Wait until the daemon's socket serves a status reply.
    let socket = crate::ipc::socket_path(pid);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(LAUNCH_READY_TIMEOUT_SECS);
    loop {
        if crate::ipc::query_status(&socket).is_ok() {
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            return Err(io::Error::other(format!(
                "detached fog '{script}' (pid {pid}) exited during startup; logs: {}",
                crate::ipc::instance_log_dir(pid).display()
            )));
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "detached fog '{script}' (pid {pid}) did not become ready within {}s; logs: {}",
                LAUNCH_READY_TIMEOUT_SECS,
                crate::ipc::instance_log_dir(pid).display()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(LAUNCH_READY_POLL_MS));
    }
    Ok(pid)
}

/// Handles `POST /api/launch`.
///
/// Status code contract:
///   - non-`POST` method → 404 (unknown route)
///   - missing `config_dir`/`script` → 400 `{"error":"config_dir and script are required"}`
///   - nonexistent config dir → 400 `{"error":"config directory not found: <path>"}`
///   - unknown branch → 400 `{"error":"no worktree is checked out on branch '<branch>'"}`
///   - unreadable config / unknown script → 400
///   - spawn failure → 500 `{"error":"could not start detached fog: <err>"}`
///   - child exits during startup → 500
///   - readiness timeout → 500
///   - success → 200 `{"ok":true,"pid":<pid>}`
pub(super) async fn handle_launch_request(
    method: &hyper::Method,
    body: &[u8],
) -> Response<RespBody> {
    if method != hyper::Method::POST {
        return api_not_found();
    }

    let parsed: LaunchBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(_) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "config_dir and script are required",
            );
        }
    };
    if parsed.config_dir.trim().is_empty() || parsed.script.trim().is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "config_dir and script are required",
        );
    }

    let config_path = match resolve_launch_target(&parsed) {
        Ok(p) => p,
        Err(msg) => return api_error(StatusCode::BAD_REQUEST, &msg),
    };

    let script = parsed.script.clone();
    let config_task = config_path.clone();
    // The spawn + readiness wait can block for up to ~60s; move it onto a
    // blocking thread so the HTTP task is never blocked.
    let outcome = tokio::task::spawn_blocking(move || spawn_detached(&config_task, &script))
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));

    match outcome {
        Ok(pid) => json_response(&serde_json::json!({ "ok": true, "pid": pid })),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
