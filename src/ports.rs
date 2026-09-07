use std::collections::HashMap;
use std::net::TcpListener;

/// Map from symbolic port name to allocated host port.
pub type PortMap = HashMap<String, u16>;

/// Allocates host ports for `specs`.
///
/// `specs` is the top-level `ports` map: `0` means pick a free port via
/// `bind("127.0.0.1:0")`, `1-65535` is used verbatim (and warned if already
/// in use). Returns the resolved map or a human-readable error string.
pub fn allocate_ports(specs: &HashMap<String, u16>) -> Result<PortMap, String> {
    let mut out = HashMap::new();
    // Keep listeners alive until all allocations are done to avoid reusing same
    // free port when two names both request random.
    let mut _holders: Vec<TcpListener> = Vec::new();

    // Sort keys for deterministic allocation order.
    let mut names: Vec<&String> = specs.keys().collect();
    names.sort();

    for name in names {
        let spec = specs[name];
        if spec == 0 {
            let listener = TcpListener::bind("127.0.0.1:0")
                .map_err(|e| format!("could not allocate random port for '{}': {}", name, e))?;
            let port = listener
                .local_addr()
                .map_err(|e| format!("could not read port for '{}': {}", name, e))?
                .port();
            _holders.push(listener);
            out.insert(name.clone(), port);
        } else {
            // Validate fixed port not already in use (soft warning via error so
            // user fixes config deterministically).
            // We probe briefly; if bind fails we return error.
            // This also catches invalid 0 case already handled.
            out.insert(name.clone(), spec);
        }
    }

    // Verify fixed ports are not colliding with random allocations and warn if
    // a fixed port is already in use by another process.
    for (name, port) in out.iter() {
        if specs[name] != 0 {
            // Probe if already in use.
            if let Ok(listener) = TcpListener::bind(format!("127.0.0.1:{}", port)) {
                drop(listener);
            } else {
                return Err(format!(
                    "port {} for '{}' is already in use; use 0 for random or pick a free port",
                    port, name
                ));
            }
        }
    }

    // Check for duplicate fixed values (two names same port) — error, since
    // explicit ports are user-specified collisions.
    {
        let mut seen: HashMap<u16, String> = HashMap::new();
        for (name, port) in &out {
            if let Some(other) = seen.get(port) {
                return Err(format!(
                    "ports '{}' and '{}' both map to {} — duplicate port",
                    other, name, port
                ));
            }
            seen.insert(*port, name.clone());
        }
    }

    // Holders dropped here; ports may be reclaimed by OS until service binds.
    // This is the classic TOCTOU for dev tools. Mitigation: services should
    // bind to $PORT immediately on start and fail fast on EADDRINUSE so the
    // user can retry `fog` (which re-allocates). For strict guarantees,
    // services should accept the port via env and bind before any other work.
    drop(_holders);
    Ok(out)
}

/// Sanitizes a branch name for DNS/hostname use.
///
/// - lowercases
/// - replaces `/` and any non-`[a-z0-9-]` with `-`
/// - collapses consecutive `-` and trims leading/trailing `-`
/// - errors if empty or >63 chars (DNS label limit)
pub fn branch_slug(branch: &str) -> Result<String, String> {
    let lower = branch.to_ascii_lowercase();
    let mut sanitized = String::with_capacity(lower.len());
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            sanitized.push(c);
        } else {
            sanitized.push('-');
        }
    }
    // collapse consecutive '-' and trim
    let mut collapsed = String::with_capacity(sanitized.len());
    let mut prev_dash = false;
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push(c);
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    let trimmed = collapsed.trim_matches('-').to_string();
    if trimmed.is_empty() {
        return Err(format!(
            "branch '{}' sanitizes to empty (no alphanumeric characters)",
            branch
        ));
    }
    if trimmed.len() > 63 {
        return Err(format!(
            "branch slug '{}' too long for DNS label ({} > 63 chars); use a shorter branch name",
            trimmed,
            trimmed.len()
        ));
    }
    Ok(trimmed)
}

/// Best-effort sanitization for hostnames already stored in Docker labels /
/// API output. Unlike `branch_slug`, this never errors — it is used at the
/// read path (`extract_hosts`, `reachable_entries_from`) where a hard error
/// would blank the whole `/api/services` list. Malformed labels like
/// `feat/book.red-fox` are leniently mapped to `feat-book.red-fox`.
///
/// Each dot-separated label is slugified independently; labels that already
/// look DNS-valid are left unchanged. Over-long labels (`>63`) are
/// truncated rather than erroring.
pub fn sanitize_hostname(host: &str) -> String {
    // Preserve empty / wildcard patterns like `{host:.+}` verbatim.
    if host.starts_with('{') {
        return host.to_string();
    }
    let mut out_parts: Vec<String> = Vec::new();
    for label in host.split('.') {
        if label.is_empty() {
            out_parts.push(String::new());
            continue;
        }
        // Fast-path: already valid DNS label.
        let is_valid = !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if is_valid && !label.contains('/') {
            out_parts.push(label.to_ascii_lowercase());
            continue;
        }
        match branch_slug(label) {
            Ok(slug) => out_parts.push(slug),
            Err(_) => {
                // Lenient fallback: same transform but truncate to 63 instead
                // of erroring, so the API stays usable for stale containers.
                let lower = label.to_ascii_lowercase();
                let mut sanitized = String::with_capacity(lower.len());
                for c in lower.chars() {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        sanitized.push(c);
                    } else {
                        sanitized.push('-');
                    }
                }
                let mut collapsed = String::with_capacity(sanitized.len());
                let mut prev_dash = false;
                for c in sanitized.chars() {
                    if c == '-' {
                        if !prev_dash {
                            collapsed.push(c);
                        }
                        prev_dash = true;
                    } else {
                        collapsed.push(c);
                        prev_dash = false;
                    }
                }
                let mut trimmed = collapsed.trim_matches('-').to_string();
                if trimmed.is_empty() {
                    trimmed = "branch".to_string();
                }
                if trimmed.len() > 63 {
                    trimmed.truncate(63);
                    // avoid trailing '-'
                    trimmed = trimmed.trim_end_matches('-').to_string();
                }
                out_parts.push(trimmed);
            }
        }
    }
    out_parts.join(".")
}

/// Resolves a single template string.
///
/// Supported atoms inside `${...}`:
/// - `ports.<name>` → allocated port number
/// - `branch` / `FOG_BRANCH` → sanitized branch name (DNS-safe, `/` → `-`)
/// - `branch_raw` / `FOG_BRANCH_RAW` → raw branch name (when Some)
///
/// Any `${ports.X}` where `X` not in `port_map` returns `Err`.
/// Unknown atoms also error. Literal `${` without closing `}` errors.
pub fn resolve_template(s: &str, ports: &PortMap, branch: Option<&str>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let end = s[start..]
                .find('}')
                .ok_or_else(|| format!("unclosed template in '{}'", s))?;
            let key = &s[start..start + end];
            let repl = resolve_atom(key.trim(), ports, branch)?;
            out.push_str(&repl);
            i = start + end + 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

fn resolve_atom(atom: &str, ports: &PortMap, branch: Option<&str>) -> Result<String, String> {
    if let Some(name) = atom.strip_prefix("ports.") {
        if name.is_empty() {
            return Err(format!("empty port name in '${{{}}}'", atom));
        }
        let port = ports.get(name).ok_or_else(|| {
            format!(
                "unknown port '{}' in '${{{}}}' (available: {})",
                name,
                atom,
                port_keys(ports)
            )
        })?;
        return Ok(port.to_string());
    }
    if atom == "branch" || atom == "FOG_BRANCH" {
        return branch
            .map(branch_slug)
            .ok_or_else(|| format!("'${{{}}}' requires a git branch (not in a worktree)", atom))?;
    }
    if atom == "branch_raw" || atom == "FOG_BRANCH_RAW" {
        return branch
            .map(|b| b.to_string())
            .ok_or_else(|| format!("'${{{}}}' requires a git branch (not in a worktree)", atom));
    }
    Err(format!(
        "unknown template '${{{}}}' (expected '${{ports.<name}}', '${{branch}}' or '${{branch_raw}}')",
        atom
    ))
}

fn port_keys(ports: &PortMap) -> String {
    let mut keys: Vec<&String> = ports.keys().collect();
    keys.sort();
    if keys.is_empty() {
        "(none)".to_string()
    } else {
        keys.into_iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// Returns `true` if `s` contains any `${...}` template.
pub fn has_template(s: &str) -> bool {
    s.contains("${")
}

/// Validates native route ports against the allocated port map and branch.
/// Mirrors the checks previously duplicated in `main.rs` and `runtime.rs`.
pub fn validate_native_routes(
    routes: &[crate::config::NativeRouteConfig],
    port_map: &PortMap,
    branch: Option<&str>,
) -> Result<(), String> {
    for r in routes {
        if r.port.contains("${ports.") && r.port.contains('}') {
            resolve_template(&r.port, port_map, branch).map_err(|e| {
                format!(
                    "native_routes port template error for service '{}': {}",
                    r.service, e
                )
            })?;
        } else if r.port.parse::<u16>().is_ok() {
            // literal port — ok
        } else if r.port.trim().is_empty() {
            return Err(format!(
                "native_routes for service '{}' has invalid port '{}'",
                r.service, r.port
            ));
        } else if r.port.contains("${") {
            return Err(format!(
                "native_routes for service '{}' port '{}' must be '${{ports.<name>}}' or literal port",
                r.service, r.port
            ));
        }
    }
    Ok(())
}

/// Ensures `ports` top-level is defined when any template references `${ports.*}`.
/// Checks service `cmd`/`shutdown_cmd`/`env`/`health_check`, proxy routes, and
/// native routes. Returns `Err` with the same message previously duplicated in
/// `main.rs` and `runtime.rs`.
pub fn ensure_ports_defined(
    config_ports: Option<&std::collections::HashMap<String, u16>>,
    script: &crate::config::ScriptConfig,
    native_routes: Option<&Vec<crate::config::NativeRouteConfig>>,
) -> Result<(), String> {
    if config_ports.is_some() {
        return Ok(());
    }
    let has_template = script.service.as_ref().is_some_and(|entries| {
        entries.iter().any(|e| {
            has_template(&e.cmd)
                || e.shutdown_cmd.as_ref().is_some_and(|s| has_template(s))
                || e.env
                    .as_ref()
                    .is_some_and(|m| m.values().any(|v| has_template(v)))
                || e.health_check.as_ref().is_some_and(|hc| match hc {
                    crate::config::HealthCheckSpec::Single(c) => has_template(&c.target),
                    crate::config::HealthCheckSpec::Multiple(v) => {
                        v.iter().any(|c| has_template(&c.target))
                    }
                })
        })
    }) || script.proxy.as_ref().is_some_and(|p| {
        p.routes
            .iter()
            .any(|r| has_template(&r.upstream) || r.host.as_ref().is_some_and(|h| has_template(h)))
    }) || native_routes
        .is_some_and(|routes| routes.iter().any(|r| has_template(&r.port)));
    if has_template {
        return Err("config uses ${ports.*} but top-level 'ports' is not defined".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pm(pairs: &[(&str, u16)]) -> PortMap {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn test_allocate_random_and_fixed() {
        let mut specs = HashMap::new();
        specs.insert("api".into(), 0);
        specs.insert("web".into(), 0);
        let m = allocate_ports(&specs).unwrap();
        assert_ne!(m["api"], 0);
        assert_ne!(m["web"], 0);
        assert_ne!(m["api"], m["web"]);
    }

    #[test]
    fn test_allocate_fixed_conflict() {
        let mut specs = HashMap::new();
        // Bind a port then try to allocate same fixed port
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        specs.insert("a".into(), p);
        // While listener alive, allocation should error
        let err = allocate_ports(&specs).unwrap_err();
        assert!(err.contains("already in use"), "{err}");
    }

    #[test]
    fn test_allocate_duplicate_fixed() {
        let mut specs = HashMap::new();
        specs.insert("a".into(), 41234);
        specs.insert("b".into(), 41234);
        let err = allocate_ports(&specs).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn test_resolve_simple() {
        let m = pm(&[("api", 1234), ("web", 5678)]);
        assert_eq!(
            resolve_template("x=${ports.api}/y", &m, None).unwrap(),
            "x=1234/y"
        );
        assert_eq!(
            resolve_template("http://localhost:${ports.web}", &m, None).unwrap(),
            "http://localhost:5678"
        );
    }

    #[test]
    fn test_resolve_branch() {
        let m = pm(&[]);
        assert_eq!(
            resolve_template("${branch}.acme", &m, Some("main")).unwrap(),
            "main.acme"
        );
        assert_eq!(
            resolve_template("${FOG_BRANCH}", &m, Some("feat")).unwrap(),
            "feat"
        );
    }

    #[test]
    fn test_resolve_unknown_port_errors() {
        let m = pm(&[("api", 1234)]);
        let err = resolve_template("${ports.missing}", &m, None).unwrap_err();
        assert!(err.contains("unknown port"), "{err}");
        // Must list available
        assert!(err.contains("api"), "{err}");
    }

    #[test]
    fn test_resolve_unclosed_errors() {
        let m = pm(&[]);
        assert!(resolve_template("a ${ports.api", &m, None).is_err());
    }

    #[test]
    fn test_resolve_unknown_atom_errors() {
        let m = pm(&[]);
        let err = resolve_template("${unknown}", &m, None).unwrap_err();
        assert!(err.contains("unknown template"), "{err}");
    }

    #[test]
    fn test_resolve_no_template_passthrough() {
        let m = pm(&[("api", 1)]);
        assert_eq!(resolve_template("plain", &m, None).unwrap(), "plain");
        assert_eq!(resolve_template("", &m, None).unwrap(), "");
    }

    #[test]
    fn test_resolve_branch_missing_errors() {
        let m = pm(&[]);
        assert!(resolve_template("${branch}", &m, None).is_err());
    }

    #[test]
    fn test_resolve_concat() {
        let m = pm(&[("db", 5432), ("api", 3000)]);
        assert_eq!(
            resolve_template("postgres://localhost:${ports.db}/app", &m, None).unwrap(),
            "postgres://localhost:5432/app"
        );
        assert_eq!(
            resolve_template("a${ports.api}b${ports.db}c", &m, None).unwrap(),
            "a3000b5432c"
        );
    }
}
