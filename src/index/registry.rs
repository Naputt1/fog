//! Persistent registry of config dirs for the launch picker.
//!
//! Discovery alone only sees what is observable right now — running fog
//! instances and running compose containers — so the "known project" list
//! empties as soon as everything stops. The registry remembers config dirs the
//! user launched or picked by hand (the same idea as OpenChamber's project
//! registry) and discovery merges them back in as ordinary roots.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hyper::{Response, StatusCode};

use super::{RespBody, api_error, api_not_found, json_response};

/// Registry file name under the state dir.
const REGISTRY_FILE: &str = "projects.json";
/// On-disk shape version; an unknown version is treated as empty.
const REGISTRY_VERSION: u32 = 1;

/// One remembered config dir.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(super) struct RegisteredProject {
    /// Absolute canonical path of the config dir (the `fog.json` parent).
    pub(super) path: String,
    /// Unix seconds when the dir was first added.
    #[serde(default)]
    pub(super) added_at: u64,
}

/// Response body for `GET /api/launch/projects`.
#[derive(serde::Serialize, Debug)]
pub(super) struct RegisteredProjects {
    pub(super) projects: Vec<RegisteredProject>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RegistryFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    projects: Vec<RegisteredProject>,
}

/// Per-user state directory: `$FOG_STATE_DIR`, else `$XDG_STATE_HOME/fog`,
/// else `~/.local/state/fog`.
fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FOG_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("fog");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("state").join("fog")
}

/// Path of the registry file.
pub(super) fn registry_path() -> PathBuf {
    state_dir().join(REGISTRY_FILE)
}

/// Expands a leading `~` to `$HOME`; other paths are returned unchanged.
pub(super) fn expand_home(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" || trimmed == "~/" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(trimmed)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads the registry, treating a missing/corrupt/unknown-version file as empty
/// so the launch picker still works without one.
pub(super) fn load_from(path: &Path) -> Vec<RegisteredProject> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(file) = serde_json::from_slice::<RegistryFile>(&bytes) else {
        return Vec::new();
    };
    if file.version != REGISTRY_VERSION {
        return Vec::new();
    }
    file.projects
}

/// Writes the registry atomically (temp file + rename) and creates the state
/// dir; callers ignore failures so a read-only home never breaks launching.
fn write_to(path: &Path, projects: &[RegisteredProject]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = RegistryFile {
        version: REGISTRY_VERSION,
        projects: projects.to_vec(),
    };
    let body = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Adds `dir` to the registry unless an entry for the same canonical path
/// already exists. Returns the stored entry (or `None` when the path cannot be
/// canonicalized).
pub(super) fn remember_in(path: &Path, dir: &Path) -> Option<RegisteredProject> {
    let canon = dir.canonicalize().ok()?;
    let canon = canon.to_string_lossy().into_owned();
    let mut projects = load_from(path);
    if let Some(existing) = projects.iter().find(|p| p.path == canon) {
        return Some(existing.clone());
    }
    let entry = RegisteredProject {
        path: canon,
        added_at: now_secs(),
    };
    projects.push(entry.clone());
    projects.sort_by(|a, b| a.path.cmp(&b.path));
    let _ = write_to(path, &projects);
    Some(entry)
}

/// Adds a user-picked config dir, rejecting paths that fog could not launch
/// (missing dir, or no `fog.json` in it).
pub(super) fn add_to(path: &Path, raw: &str) -> Result<RegisteredProject, String> {
    let dir = expand_home(raw);
    if !dir.is_absolute() {
        return Err(format!("path must be absolute: {raw}"));
    }
    if !dir.is_dir() {
        return Err(format!("config directory not found: {raw}"));
    }
    let canon = dir
        .canonicalize()
        .map_err(|_| format!("config directory not found: {raw}"))?;
    if !canon.join("fog.json").is_file() {
        return Err(format!("no fog.json in {}", canon.to_string_lossy()));
    }
    remember_in(path, &canon).ok_or_else(|| format!("could not add {}", canon.to_string_lossy()))
}

pub(super) fn forget_from(path: &Path, raw: &str) -> Result<(), String> {
    let target = expand_home(raw);
    if !target.is_absolute() {
        return Err(format!("path must be absolute: {raw}"));
    }
    // Compare canonically when the path exists, lexically otherwise, so a dir
    // that was deleted can still be forgotten.
    let wanted = target
        .canonicalize()
        .unwrap_or(target)
        .to_string_lossy()
        .into_owned();
    let projects = load_from(path);
    let kept: Vec<RegisteredProject> = projects
        .iter()
        .filter(|p| p.path != wanted)
        .cloned()
        .collect();
    if kept.len() == projects.len() {
        return Err(format!("project not registered: {raw}"));
    }
    write_to(path, &kept).map_err(|e| format!("could not update the project registry: {e}"))
}

/// The registered config dirs that still exist, in path order.
pub(super) fn load_existing_from(path: &Path) -> Vec<RegisteredProject> {
    load_from(path)
        .into_iter()
        .filter(|p| Path::new(&p.path).is_dir())
        .collect()
}

/// The registered config dirs that still exist, in path order.
///
/// Entries whose directory vanished are dropped from the answer (the file is
/// pruned on the next write).
pub(super) fn load() -> Vec<RegisteredProject> {
    load_existing_from(&registry_path())
}

/// Remembers a config dir after a successful launch. Best-effort: a launch
/// target may point at `fog.json` itself, in which case its parent is stored.
pub(super) fn remember(config_dir: &Path) {
    let Ok(canon) = config_dir.canonicalize() else {
        return;
    };
    let dir = if canon.is_file() {
        canon.parent().map(Path::to_path_buf).unwrap_or(canon)
    } else {
        canon
    };
    let _ = remember_in(&registry_path(), &dir);
}

/// Adds a user-picked config dir to the registry.
pub(super) fn add(raw: &str) -> Result<RegisteredProject, String> {
    add_to(&registry_path(), raw)
}

/// Forgets one registered config dir.
pub(super) fn forget(raw: &str) -> Result<(), String> {
    forget_from(&registry_path(), raw)
}

/// Request body for `POST /api/launch/projects`.
#[derive(serde::Deserialize)]
struct AddProjectBody {
    path: String,
}

/// `GET /api/launch/projects`: every remembered config dir.
pub(super) fn api_registered_projects_method(method: &hyper::Method) -> Response<RespBody> {
    if method != hyper::Method::GET {
        return api_not_found();
    }
    json_response(&RegisteredProjects { projects: load() })
}

/// `GET`/`POST`/`DELETE /api/launch/projects`: list, add, forget.
///
/// Status code contract:
///   - `GET` → 200 `{"projects":[...]}`
///   - `POST {"path":...}` → 200 `{"project":...}`, or 400 for a bad path
///   - `DELETE ?path=...` → 200 `{"ok":true}`, or 400 when unknown
pub(super) fn handle_launch_projects_request(
    method: &hyper::Method,
    query: Option<&str>,
    body: &[u8],
) -> Response<RespBody> {
    match *method {
        hyper::Method::GET => api_registered_projects_method(method),
        hyper::Method::POST => {
            let parsed: AddProjectBody = match serde_json::from_slice(body) {
                Ok(b) => b,
                Err(_) => return api_error(StatusCode::BAD_REQUEST, "path is required"),
            };
            if parsed.path.trim().is_empty() {
                return api_error(StatusCode::BAD_REQUEST, "path is required");
            }
            match add(&parsed.path) {
                Ok(project) => json_response(&serde_json::json!({ "project": project })),
                Err(message) => api_error(StatusCode::BAD_REQUEST, &message),
            }
        }
        hyper::Method::DELETE => {
            let path = super::query_param_decoded(query, "path").unwrap_or_default();
            if path.trim().is_empty() {
                return api_error(StatusCode::BAD_REQUEST, "path is required");
            }
            match forget(path.trim()) {
                Ok(()) => json_response(&serde_json::json!({ "ok": true })),
                Err(message) => api_error(StatusCode::BAD_REQUEST, &message),
            }
        }
        _ => api_not_found(),
    }
}
