//! Project icon resolution and serving (`/api/projects/{name}/icon`).

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use super::{
    FogInstance, RespBody, api_error, discover_fog_instances, mime_for,
    project_name_from_common_dir,
};

/// A configured project icon: either a renderable URL/data URI, or a
/// filesystem path the index server reads and serves at
/// `/api/projects/{name}/icon`.
pub(super) enum IconSource {
    /// `http(s)://` URL or `data:image/…` URI, used verbatim.
    Url(String),
    /// Filesystem path. Relative paths resolve against the project's config dir.
    Path(String),
}

/// Classifies a configured `project.icon` value.
///
/// Returns `None` for blank values and for non-renderable URI schemes (e.g.
/// `javascript:`, `file:`), so a bad value falls back to the default glyph
/// rather than being treated as a path. Any other value is a filesystem path.
pub(super) fn resolve_project_icon(raw: &str) -> Option<IconSource> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:image/")
    {
        return Some(IconSource::Url(trimmed.to_string()));
    }
    // Reject values that carry some other URI scheme. A scheme is a leading
    // `[A-Za-z][A-Za-z0-9+.-]*` followed by `:`. Plain paths (including
    // absolute ones like `/x/y.svg`) have no such prefix.
    if let Some((scheme, _)) = trimmed.split_once(':') {
        let looks_like_scheme = scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if looks_like_scheme {
            return None;
        }
    }
    Some(IconSource::Path(trimmed.to_string()))
}

/// Percent-encodes a project name for use as a single URL path segment.
pub(super) fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Resolves a configured icon path to an existing file.
///
/// `~/…` expands against `$HOME`; absolute paths are used as-is; relative paths
/// are joined onto `config_dir`. Returns `None` when the path does not exist or
/// is not a regular file.
pub(super) fn resolve_icon_file(
    config_dir: &std::path::Path,
    raw: &str,
) -> Option<std::path::PathBuf> {
    let candidate = if let Some(rest) = raw.strip_prefix("~/") {
        std::path::PathBuf::from(std::env::var("HOME").ok()?).join(rest)
    } else {
        let path = std::path::PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            config_dir.join(path)
        }
    };
    let canonical = candidate.canonicalize().ok()?;
    canonical.is_file().then_some(canonical)
}

/// File extensions the icon endpoint will serve.
const ICON_EXTENSIONS: &[&str] = &["svg", "png", "jpg", "jpeg", "gif", "webp", "avif", "ico"];

/// Whether a resolved icon file has an allowed image extension.
pub(super) fn is_icon_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ICON_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Largest icon file the endpoint will serve (2 MiB).
pub(super) const MAX_ICON_BYTES: u64 = 2 * 1024 * 1024;

/// Maps project name → icon URL by reading each running instance's `fog.json`
/// (`project.icon`). URL/data icons are used verbatim; filesystem paths map to
/// this server's `/api/projects/{name}/icon` endpoint. Keys use the same name
/// derivation as the service list so docker and native entries agree.
pub(super) fn project_icons(
    instances: &[FogInstance],
) -> std::collections::HashMap<String, String> {
    let mut icons = std::collections::HashMap::new();
    for inst in instances {
        let Some(dir) = inst.config_dir.as_deref() else {
            continue;
        };
        let Ok(cfg) = crate::config::load(std::path::Path::new(dir).join("fog.json").as_path())
        else {
            continue;
        };
        let Some(raw) = cfg.project.and_then(|p| p.icon) else {
            continue;
        };
        let Some(source) = resolve_project_icon(&raw) else {
            continue;
        };
        let project = inst
            .project
            .as_deref()
            .map(project_name_from_common_dir)
            .unwrap_or_else(|| inst.script.clone());
        let value = match source {
            IconSource::Url(url) => url,
            IconSource::Path(_) => {
                format!("/api/projects/{}/icon", encode_path_segment(&project))
            }
        };
        icons.entry(project).or_insert(value);
    }
    icons
}

/// Serves a project's configured icon bytes with its content type and an
/// mtime-based `ETag`. Honors `If-None-Match` with a `304`.
pub(super) fn serve_icon_file(
    path: &std::path::Path,
    if_none_match: Option<&str>,
) -> Response<RespBody> {
    let Ok(meta) = std::fs::metadata(path) else {
        return api_error(StatusCode::NOT_FOUND, "icon file not found");
    };
    if meta.len() > MAX_ICON_BYTES {
        return api_error(StatusCode::PAYLOAD_TOO_LARGE, "icon file too large");
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("\"{}-{}\"", meta.len(), mtime);
    if if_none_match.is_some_and(|v| v.trim() == etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header("etag", etag)
            .header("cache-control", "no-cache")
            .body(Full::new(Bytes::new()).boxed())
            .expect("response builder failed");
    }
    let Ok(bytes) = std::fs::read(path) else {
        return api_error(StatusCode::NOT_FOUND, "icon file not found");
    };
    let mime = mime_for(&path.to_string_lossy());
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", mime)
        .header("content-length", bytes.len())
        .header("etag", etag)
        .header("cache-control", "no-cache")
        .body(Full::new(Bytes::from(bytes)).boxed())
        .expect("response builder failed")
}

/// `GET /api/projects/{name}/icon`: serves the owning project's configured icon.
///
/// Resolves the running instance whose derived project name matches `name`
/// (percent-encoded), re-reads its `fog.json`, and either redirects to a
/// configured URL/data URI or streams the configured image file. Only files
/// named in config are ever read; the request path selects a project, never a
/// file.
pub(super) fn api_project_icon(name: &str, if_none_match: Option<&str>) -> Response<RespBody> {
    for inst in discover_fog_instances() {
        let Some(dir) = inst.config_dir.as_deref() else {
            continue;
        };
        let project = inst
            .project
            .as_deref()
            .map(project_name_from_common_dir)
            .unwrap_or_else(|| inst.script.clone());
        if encode_path_segment(&project) != name {
            continue;
        }
        let config_dir = std::path::PathBuf::from(dir);
        let Ok(cfg) = crate::config::load(&config_dir.join("fog.json")) else {
            continue;
        };
        let Some(raw) = cfg.project.and_then(|p| p.icon) else {
            continue;
        };
        match resolve_project_icon(&raw) {
            Some(IconSource::Url(url)) => {
                return Response::builder()
                    .status(StatusCode::FOUND)
                    .header("location", url)
                    .header("cache-control", "no-cache")
                    .body(Full::new(Bytes::new()).boxed())
                    .expect("response builder failed");
            }
            Some(IconSource::Path(path)) => {
                let Some(file) = resolve_icon_file(&config_dir, &path) else {
                    continue;
                };
                if !is_icon_path(&file) {
                    return api_error(StatusCode::BAD_REQUEST, "icon is not a supported image");
                }
                return serve_icon_file(&file, if_none_match);
            }
            None => continue,
        }
    }
    api_error(
        StatusCode::NOT_FOUND,
        "unknown project or no icon configured",
    )
}

/// Parses `/api/projects/{name}/icon` into the raw (still percent-encoded)
/// project name segment.
pub(super) fn parse_project_icon_route(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/api/projects/")?;
    let name = rest.strip_suffix("/icon")?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(name)
}
