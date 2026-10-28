//! Directory browser behind the launch picker: `GET /api/fs/list?dir=<abs|~>`.
//!
//! The launch form lets the user pick a config dir instead of typing one. The
//! listing is directories only (no files, no hidden dirs) and marks which
//! entries are git repositories and which hold a `fog.json`, so the picker can
//! show the same affordances the CLI user would look for.

use hyper::{Response, StatusCode};

use super::registry::expand_home;
use super::{RespBody, api_error, api_not_found, json_response};

/// Upper bound on entries returned for one directory.
const MAX_ENTRIES: usize = 500;

/// One subdirectory in a browse response.
#[derive(serde::Serialize, Debug, PartialEq, Eq)]
pub(super) struct BrowseEntry {
    /// Directory name (no path).
    pub(super) name: String,
    /// Absolute path of the directory.
    pub(super) path: String,
    /// The directory is a git worktree or repository (has `.git`).
    pub(super) git: bool,
    /// The directory holds a `fog.json`, so fog can launch it.
    pub(super) fog: bool,
}

/// Response body for `GET /api/fs/list`.
#[derive(serde::Serialize, Debug)]
pub(super) struct BrowseListing {
    /// Canonical path of the listed directory.
    pub(super) path: String,
    /// Parent directory, `None` at a filesystem root.
    pub(super) parent: Option<String>,
    /// The listed directory itself holds a `fog.json`.
    pub(super) fog: bool,
    /// Subdirectories, sorted by name.
    pub(super) entries: Vec<BrowseEntry>,
}

/// Lists the subdirectories of `raw` (`~` allowed). Returns a human-readable
/// error when the path is not absolute, not a directory, or unreadable.
pub(super) fn list_dirs(raw: &str) -> Result<BrowseListing, String> {
    let dir = expand_home(raw);
    if !dir.is_absolute() {
        return Err(format!("path must be absolute: {raw}"));
    }
    if !dir.is_dir() {
        return Err(format!("directory not found: {raw}"));
    }
    let canon = dir
        .canonicalize()
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?;

    let read = std::fs::read_dir(&canon)
        .map_err(|e| format!("could not read {}: {e}", canon.display()))?;
    let mut entries: Vec<BrowseEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        entries.push(BrowseEntry {
            name,
            git: path.join(".git").exists(),
            fog: path.join("fog.json").is_file(),
            path: path.to_string_lossy().into_owned(),
        });
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    entries.sort_by_key(|e| e.name.to_lowercase());

    Ok(BrowseListing {
        path: canon.to_string_lossy().into_owned(),
        parent: canon.parent().map(|p| p.to_string_lossy().into_owned()),
        fog: canon.join("fog.json").is_file(),
        entries,
    })
}

/// `GET /api/fs/list?dir=<abs|~>`: subdirectories of one directory.
///
/// Any method other than GET is treated like an unknown route (404), matching
/// the other read-only endpoints.
pub(super) fn api_fs_list_method(
    method: &hyper::Method,
    query: Option<&str>,
) -> Response<RespBody> {
    if method != hyper::Method::GET {
        return api_not_found();
    }
    // No `dir` (or an empty one) opens the home directory.
    let dir = super::query_param_decoded(query, "dir")
        .or_else(|| super::query_param_decoded(query, "path"))
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "~".to_string());
    match list_dirs(&dir) {
        Ok(listing) => json_response(&listing),
        Err(message) => api_error(StatusCode::BAD_REQUEST, &message),
    }
}
