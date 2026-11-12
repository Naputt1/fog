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

    // `@` and `:` split cross-instance references, so a plain port name may not
    // contain them (a `ports` key like `api@repo` would otherwise be read as a
    // reference).
    if let Some(name) = names
        .iter()
        .find(|name| name.contains('@') || name.contains(':'))
    {
        return Err(format!(
            "port name '{name}' must not contain '@' or ':' (reserved for cross-instance \
             references like ${{ports.api@my-repo:main}})"
        ));
    }

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

/// Parses repeated `--port NAME=PORT` run-time overrides into a map.
///
/// Each entry must be `NAME=PORT` with a non-empty name and a `u16` port
/// (`0` re-randomizes). Duplicate names are rejected so the effective override
/// is unambiguous. Returns a human-readable error string on malformed input.
///
/// The result is merged onto the config's `ports` map before allocation; a
/// name absent from the config defines a new port for this run. A
/// cross-instance key (`api@repo:branch`) pins that reference.
pub fn parse_port_overrides(raw: &[String]) -> Result<HashMap<String, u16>, String> {
    let mut out = HashMap::new();
    for item in raw {
        let Some((name, value)) = item.split_once('=') else {
            return Err(format!(
                "invalid --port '{item}' (expected NAME=PORT, e.g. --port api=4000)"
            ));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("invalid --port '{item}': port name is empty"));
        }
        if let Some(parsed) = parse_external_port_key(name)
            && let Err(e) = parsed
        {
            return Err(format!("invalid --port '{item}': {e}"));
        }
        let value = value.trim();
        let port = value.parse::<u16>().map_err(|_| {
            format!("invalid --port '{item}': '{value}' is not a valid port (0-65535)")
        })?;
        // A parent cannot allocate or re-randomize a port it does not own: a
        // live target keeps its value and a cold-started target allocates its
        // own. Only a fixed value is meaningful for an external reference.
        if port == 0 && parse_external_port_key(name).is_some() {
            return Err(format!(
                "invalid --port '{item}': a cross-instance port needs a fixed value (1-65535); \
                 the owning instance allocates its own port"
            ));
        }
        if out.insert(name.to_string(), port).is_some() {
            return Err(format!("duplicate --port override for '{name}'"));
        }
    }
    Ok(out)
}

/// A cross-instance port reference:
/// `${ports.<name>@<repo>:<branch>[:<script>]}`.
///
/// `<repo>` is the repository directory name (the project name `fog ls` and
/// the index show), matched case-insensitively, or the exact git-common-dir
/// identity. `<branch>` matches a live instance's raw branch, its branch slug,
/// or the index's worktree segment, so `feat/login`, `feat-login` and the
/// truncated label the UI shows all resolve. Two tokens are special: `?` means
/// "the referencing branch when the target repo has that branch, otherwise its
/// main worktree", and `*` always means the target's main worktree. The
/// optional `<script>` narrows the reference to one script of that
/// project+branch; it also allows fog to start that script when no matching
/// instance is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalPortRef {
    /// Symbolic port name declared by the target instance.
    pub name: String,
    /// Repository (project) name to look up.
    pub project: String,
    /// Branch to look up (raw, slug, or index worktree segment).
    pub branch: String,
    /// Optional script name of the target instance.
    pub script: Option<String>,
}

impl ExternalPortRef {
    /// Map key the resolved port is stored under (the atom without `ports.`).
    pub fn key(&self) -> String {
        match &self.script {
            Some(script) => format!("{}@{}:{}:{}", self.name, self.project, self.branch, script),
            None => format!("{}@{}:{}", self.name, self.project, self.branch),
        }
    }

    /// Full template atom, for error messages.
    pub fn atom(&self) -> String {
        format!("ports.{}", self.key())
    }

    /// Human-readable target identity, e.g. `cinema_ticket:main:dev`.
    pub fn target(&self) -> String {
        match &self.script {
            Some(script) => format!("{}:{}:{}", self.project, self.branch, script),
            None => format!("{}:{}", self.project, self.branch),
        }
    }
}

/// Parses the part of a `${ports.…}` atom after `ports.`.
///
/// Returns `None` for a local name (`api`), `Some(Ok(..))` for a well-formed
/// cross-instance reference (`api@cinema_ticket:main` or
/// `api@cinema_ticket:main:dev`), and `Some(Err(..))` when the atom looks
/// cross-instance but is malformed.
pub fn parse_external_port_key(key: &str) -> Option<Result<ExternalPortRef, String>> {
    const EXPECTED: &str = "expected '${ports.<name>@<repo>:<branch>}' or \
                            '${ports.<name>@<repo>:<branch>:<script>}'";
    let (name, rest) = key.split_once('@')?;
    let parts: Vec<&str> = rest.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return Some(Err(format!(
            "invalid cross-instance port reference '${{ports.{key}}}' ({EXPECTED}, e.g. \
             '${{ports.api@my-repo:main}}' or '${{ports.api@my-repo:main:dev}}')"
        )));
    }
    let name = name.trim();
    let project = parts[0].trim();
    let branch = parts[1].trim();
    let script = parts.get(2).map(|part| part.trim());
    if name.is_empty()
        || project.is_empty()
        || branch.is_empty()
        || script.is_some_and(str::is_empty)
    {
        return Some(Err(format!(
            "invalid cross-instance port reference '${{ports.{key}}}': name, repo, branch and \
             script must all be non-empty ({EXPECTED})"
        )));
    }
    // `?` (same branch in the target, else its main worktree), `*` (the
    // target's main worktree) and branch atoms such as `${branch}` are resolved
    // later against the referencing instance, so they cannot be slug-checked
    // here.
    if branch != "?"
        && branch != "*"
        && !branch.contains("${")
        && let Err(e) = branch_slug(branch)
    {
        return Some(Err(format!(
            "invalid branch in cross-instance port reference '${{ports.{key}}}': {e}"
        )));
    }
    Some(Ok(ExternalPortRef {
        name: name.to_string(),
        project: project.to_string(),
        branch: branch.to_string(),
        script: script.map(str::to_string),
    }))
}

/// Whether a live branch matches a reference's branch token: the raw name, its
/// slug, or the index's worktree segment (`sanitize_hostname` truncated at the
/// first dot, e.g. `release/1.2` → `release-1`).
pub fn branch_matches(branch: &str, token: &str) -> bool {
    if branch.eq_ignore_ascii_case(token) {
        return true;
    }
    if let (Ok(a), Ok(b)) = (branch_slug(branch), branch_slug(token))
        && a == b
    {
        return true;
    }
    let ui = sanitize_hostname(branch);
    let ui = ui.split('.').next().unwrap_or("");
    !ui.is_empty() && token.eq_ignore_ascii_case(ui)
}

/// Expands branch atoms inside a cross-instance branch token using the
/// referencing instance's branch: `${branch}` / `${FOG_BRANCH}` /
/// `${FOG_BRANCH_SLUG}` become its slug, `${branch_raw}` / `${FOG_BRANCH_RAW}`
/// its raw name. Returns `None` when the token needs a branch and the calling
/// instance has none (detached HEAD).
pub fn expand_branch_token(token: &str, owner_branch: Option<&str>) -> Option<String> {
    const SLUG_ATOMS: [&str; 3] = ["${branch}", "${FOG_BRANCH}", "${FOG_BRANCH_SLUG}"];
    const RAW_ATOMS: [&str; 2] = ["${branch_raw}", "${FOG_BRANCH_RAW}"];
    let uses_slug = SLUG_ATOMS.iter().any(|atom| token.contains(atom));
    let uses_raw = RAW_ATOMS.iter().any(|atom| token.contains(atom));
    if !uses_slug && !uses_raw {
        return Some(token.to_string());
    }
    let branch = owner_branch?;
    let mut out = token.to_string();
    if uses_slug {
        let slug = branch_slug(branch).ok()?;
        for atom in SLUG_ATOMS {
            out = out.replace(atom, &slug);
        }
    }
    if uses_raw {
        for atom in RAW_ATOMS {
            out = out.replace(atom, branch);
        }
    }
    Some(out)
}

/// Whether a project identity (`git rev-parse --git-common-dir`) or its
/// directory name matches a reference's project token. The directory-name
/// comparison is case-insensitive so the lowercased name the index and UI show
/// resolves too.
pub fn project_matches(identity: &str, token: &str) -> bool {
    identity == token || crate::project::display_name(identity).eq_ignore_ascii_case(token)
}

/// Lowercases `input`, replaces every non-`[a-z0-9-]` character with `-`,
/// collapses consecutive `-`, and trims leading/trailing `-`. The shared core
/// of [`branch_slug`] (which then validates length/emptiness) and the lenient
/// [`sanitize_hostname`] fallback (which truncates instead of erroring).
fn slugify(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_dash = false;
    for c in lower.chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if c == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

/// Sanitizes a branch name for DNS/hostname use.
///
/// - lowercases
/// - replaces `/` and any non-`[a-z0-9-]` with `-`
/// - collapses consecutive `-` and trims leading/trailing `-`
/// - errors if empty or >63 chars (DNS label limit)
pub fn branch_slug(branch: &str) -> Result<String, String> {
    let trimmed = slugify(branch);
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
                let mut trimmed = slugify(label);
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
/// - `ports.<name>@<repo>:<branch>` → a port owned by another live instance
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
            let end =
                template_end(s, start).ok_or_else(|| format!("unclosed template in '{}'", s))?;
            let key = &s[start..start + end];
            let repl = resolve_atom(key.trim(), ports, branch)?;
            out.push_str(&repl);
            i = start + end + 1;
        } else {
            // Advance by a whole char, not a byte: `bytes[i] as char` would
            // corrupt any UTF-8 continuation byte (e.g. `é` → `Ã©`).
            let ch = s[i..]
                .chars()
                .next()
                .expect("i is a char boundary inside s");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

fn resolve_atom(atom: &str, ports: &PortMap, branch: Option<&str>) -> Result<String, String> {
    if let Some(name) = atom.strip_prefix("ports.") {
        if name.is_empty() {
            return Err(format!("empty port name in '${{{}}}'", atom));
        }
        if let Some(parsed) = parse_external_port_key(name) {
            let reference = parsed?;
            let port = ports.get(&reference.key()).ok_or_else(|| {
                let hint = if reference.script.is_some() {
                    format!(
                        "start the '{}' instance directly, or pass --port {}=PORT",
                        reference.target(),
                        reference.key()
                    )
                } else {
                    format!(
                        "start it first, add ':script' (e.g. '${{ports.{}@{}:{}:<script>}}') so fog \
                         can start it, or pass --port {}=PORT",
                        reference.name,
                        reference.project,
                        reference.branch,
                        reference.key()
                    )
                };
                format!(
                    "cross-instance port '${{{}}}' was not resolved ({hint})",
                    atom
                )
            })?;
            return Ok(port.to_string());
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

/// Returns `true` if `s` contains one of fog's template prefixes.
///
/// Only fog's own atoms count; a shell expansion such as `${HOME}` is left to
/// the shell, so it is not a template.
pub fn has_template(s: &str) -> bool {
    const PREFIXES: [&str; 5] = [
        "${ports.",
        "${branch}",
        "${FOG_BRANCH}",
        "${branch_raw}",
        "${FOG_BRANCH_RAW}",
    ];
    PREFIXES.iter().any(|p| s.contains(p))
}

/// Whether `s` contains a *local* `${ports.<name>}` atom, i.e. one that needs a
/// top-level `ports` map. Cross-instance references
/// (`${ports.<name>@<repo>:<branch>}`) are resolved from live instances and do
/// not require one.
pub fn has_local_port_template(s: &str) -> bool {
    let mut found = false;
    let _ = scan_port_atoms(s, &mut |atom| {
        if parse_external_port_key(atom).is_none() {
            found = true;
        }
        Ok(())
    });
    found
}

/// Calls `f` with every `${ports.<atom>}` atom in `s` (the part after
/// `ports.`), so callers can classify local names and cross-instance
/// references identically.
fn scan_port_atoms(s: &str, f: &mut dyn FnMut(&str) -> Result<(), String>) -> Result<(), String> {
    const PREFIX: &str = "${ports.";
    let mut rest = s;
    while let Some(start) = rest.find(PREFIX) {
        let atom_start = start + PREFIX.len();
        let Some(end) = template_end(rest, atom_start) else {
            break;
        };
        let atom = rest[atom_start..atom_start + end].trim();
        if !atom.is_empty() {
            f(atom)?;
        }
        rest = &rest[atom_start + end + 1..];
    }
    Ok(())
}

/// Offset of the `}` closing the template whose content starts at `atom_start`,
/// skipping nested atoms such as `${branch}` inside a cross-instance port key.
fn template_end(s: &str, atom_start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = atom_start;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'}' {
            if depth == 0 {
                return Some(i - atom_start);
            }
            depth -= 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    None
}

/// Collects every local `${ports.<name>}` name referenced by `s` into `out`.
///
/// Used to decide which allocated ports a borrowing instance must adopt from
/// the sibling that already owns a shared resource, so dependent services point
/// at the live container instead of a freshly allocated port. Cross-instance
/// references are skipped: they are not allocated by this instance.
pub fn referenced_port_names(s: &str, out: &mut std::collections::BTreeSet<String>) {
    let _ = scan_port_atoms(s, &mut |name| {
        if parse_external_port_key(name).is_none() {
            out.insert(name.to_string());
        }
        Ok(())
    });
}

/// Collects every `${ports.<name>}` referenced by a service entry: its `cmd`,
/// `shutdown_cmd`, `env` values, health-check targets, and declared endpoints.
pub fn entry_referenced_ports(
    entry: &crate::config::ConfigEntry,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    referenced_port_names(&entry.cmd, &mut out);
    if let Some(cmd) = &entry.shutdown_cmd {
        referenced_port_names(cmd, &mut out);
    }
    if let Some(env) = &entry.env {
        for value in env.values() {
            referenced_port_names(value, &mut out);
        }
    }
    collect_health_check_ports(entry.health_check.as_ref(), &mut out);
    if let Some(endpoints) = &entry.endpoint {
        for endpoint in endpoints {
            if let Some(h) = &endpoint.host {
                referenced_port_names(h, &mut out);
            }
            if let Some(p) = &endpoint.port {
                referenced_port_names(p, &mut out);
            }
            if let Some(p) = &endpoint.path_prefix {
                referenced_port_names(p, &mut out);
            }
            collect_health_check_ports(endpoint.health_check.as_ref(), &mut out);
        }
    }
    out
}

fn collect_health_check_ports(
    spec: Option<&crate::config::HealthCheckSpec>,
    out: &mut std::collections::BTreeSet<String>,
) {
    match spec {
        Some(crate::config::HealthCheckSpec::Single(c)) => referenced_port_names(&c.target, out),
        Some(crate::config::HealthCheckSpec::Multiple(v)) => {
            for c in v {
                referenced_port_names(&c.target, out);
            }
        }
        None => {}
    }
}

/// Calls `f` with every template-bearing string in `script` and its native
/// routes: service `cmd`/`shutdown_cmd`/`env`, health checks (including
/// endpoint health checks and compose files), endpoint host/port/path, proxy
/// upstream/host, and native route host/port.
fn for_each_script_string(
    script: &crate::config::ScriptConfig,
    native_routes: Option<&Vec<crate::config::NativeRouteConfig>>,
    f: &mut dyn FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(entries) = &script.service {
        for entry in entries {
            f(&entry.cmd)?;
            if let Some(cmd) = &entry.shutdown_cmd {
                f(cmd)?;
            }
            if let Some(env) = &entry.env {
                for value in env.values() {
                    f(value)?;
                }
            }
            for_each_health_check_string(entry.health_check.as_ref(), f)?;
            if let Some(endpoints) = &entry.endpoint {
                for endpoint in endpoints {
                    for value in [
                        endpoint.host.as_deref(),
                        endpoint.port.as_deref(),
                        endpoint.path_prefix.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        f(value)?;
                    }
                    for_each_health_check_string(endpoint.health_check.as_ref(), f)?;
                }
            }
        }
    }
    if let Some(proxy) = &script.proxy {
        for route in &proxy.routes {
            f(&route.upstream)?;
            if let Some(host) = &route.host {
                f(host)?;
            }
        }
    }
    if let Some(routes) = native_routes {
        for route in routes {
            f(&route.host)?;
            f(&route.port)?;
        }
    }
    Ok(())
}

fn for_each_health_check_string(
    spec: Option<&crate::config::HealthCheckSpec>,
    f: &mut dyn FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    let mut one = |check: &crate::config::HealthCheckConfig| -> Result<(), String> {
        f(&check.target)?;
        if let Some(file) = &check.compose_file {
            f(file)?;
        }
        Ok(())
    };
    match spec {
        Some(crate::config::HealthCheckSpec::Single(c)) => one(c),
        Some(crate::config::HealthCheckSpec::Multiple(v)) => {
            for c in v {
                one(c)?;
            }
            Ok(())
        }
        None => Ok(()),
    }
}

/// Collects every cross-instance `${ports.<name>@<repo>:<branch>[:<script>]}`
/// reference in the script and its native routes, keyed by
/// [`ExternalPortRef::key`].
pub fn collect_external_port_refs(
    script: &crate::config::ScriptConfig,
    native_routes: Option<&Vec<crate::config::NativeRouteConfig>>,
) -> Result<Vec<ExternalPortRef>, String> {
    let mut refs: std::collections::BTreeMap<String, ExternalPortRef> =
        std::collections::BTreeMap::new();
    for_each_script_string(script, native_routes, &mut |s| {
        scan_port_atoms(s, &mut |atom| {
            if let Some(parsed) = parse_external_port_key(atom) {
                let reference = parsed?;
                refs.entry(reference.key()).or_insert(reference);
            }
            Ok(())
        })
    })?;
    Ok(refs.into_values().collect())
}

/// Local `${ports.<name>}` names referenced by `script` and its native routes.
/// Cross-instance references are excluded; they resolve to their own map keys.
pub fn referenced_local_port_names(
    script: &crate::config::ScriptConfig,
    native_routes: Option<&Vec<crate::config::NativeRouteConfig>>,
) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    let mut collect = |s: &str| -> Result<(), String> {
        scan_port_atoms(s, &mut |atom| {
            if parse_external_port_key(atom).is_none() {
                names.insert(atom.to_string());
            }
            Ok(())
        })
    };
    let _ = for_each_script_string(script, native_routes, &mut collect);
    names
}

/// Port snapshot of one live fog instance, used to resolve cross-instance
/// port references.
#[derive(Debug, Clone)]
pub struct LiveInstance {
    pub pid: u32,
    pub script: String,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub ports: PortMap,
}

/// Queries every live fog instance except this process.
pub fn live_instances() -> Vec<LiveInstance> {
    let self_pid = std::process::id();
    let Ok(paths) = crate::ipc::find_instances() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (pid, path) in paths {
        if pid == self_pid {
            continue;
        }
        match crate::ipc::query_status(&path) {
            Ok(status) => out.push(LiveInstance {
                pid,
                script: status.script,
                project: status.project,
                branch: status.branch,
                ports: status.ports,
            }),
            Err(_) => {
                if !crate::process::is_pid_alive(pid) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    out
}

fn instance_matches(reference: &ExternalPortRef, instance: &LiveInstance) -> bool {
    let Some(project) = instance.project.as_deref() else {
        return false;
    };
    if !project_matches(project, &reference.project) {
        return false;
    }
    if let Some(script) = &reference.script
        && instance.script != *script
    {
        return false;
    }
    let Some(branch) = instance.branch.as_deref() else {
        return false;
    };
    branch_matches(branch, &reference.branch)
}

/// Whether any live instance matches the reference's project, branch and
/// (when given) script, regardless of whether it publishes the port.
pub fn any_instance_matches(reference: &ExternalPortRef, instances: &[LiveInstance]) -> bool {
    instances
        .iter()
        .any(|instance| instance_matches(reference, instance))
}

/// Why a cross-instance reference could not be resolved from live instances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickFailure {
    /// No running instance matches project, branch (and script, when given).
    NoInstance,
    /// A matching instance is running but publishes no port of that name.
    /// Carries the sorted list of names it does publish.
    MissingPort(String),
    /// Several matching instances disagree on the value. Carries the details.
    Ambiguous(String),
}

/// Resolves one cross-instance reference against `instances`, reporting the
/// kind of failure so callers can try a cold start only for [`PickFailure::NoInstance`].
pub fn pick_external_port_detailed(
    reference: &ExternalPortRef,
    instances: &[LiveInstance],
) -> Result<u16, PickFailure> {
    pick_external_port_source(reference, instances).map(|(port, _)| port)
}

/// Like [`pick_external_port_detailed`], also returning the PID of the instance
/// that published the value (for the startup log).
pub fn pick_external_port_source(
    reference: &ExternalPortRef,
    instances: &[LiveInstance],
) -> Result<(u16, u32), PickFailure> {
    let candidates: Vec<&LiveInstance> = instances
        .iter()
        .filter(|instance| instance_matches(reference, instance))
        .collect();
    if candidates.is_empty() {
        return Err(PickFailure::NoInstance);
    }
    let mut values: Vec<(&LiveInstance, u16)> = candidates
        .iter()
        .filter_map(|instance| {
            instance
                .ports
                .get(&reference.name)
                .map(|port| (*instance, *port))
        })
        .collect();
    if values.is_empty() {
        let mut names: Vec<String> = candidates
            .iter()
            .flat_map(|instance| instance.ports.keys().cloned())
            .collect();
        names.sort_unstable();
        names.dedup();
        let available = if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        };
        return Err(PickFailure::MissingPort(available));
    }
    let first = values[0].1;
    if values.iter().any(|(_, port)| *port != first) {
        values.sort_by_key(|(instance, _)| instance.pid);
        let detail = values
            .iter()
            .map(|(instance, port)| {
                format!(
                    "pid {} (script '{}') -> {}",
                    instance.pid, instance.script, port
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(PickFailure::Ambiguous(detail));
    }
    Ok((first, values[0].0.pid))
}

/// Renders a [`PickFailure`] for one reference as the user-facing error.
pub fn format_pick_failure(reference: &ExternalPortRef, failure: &PickFailure) -> String {
    let atom = reference.atom();
    match failure {
        PickFailure::NoInstance => {
            let hint = if reference.script.is_some() {
                format!("start it first or pass --port {}=PORT", reference.key())
            } else {
                format!(
                    "start it first, add ':script' (e.g. '${{ports.{}@{}:{}:<script>}}') so fog can \
                     start it, or pass --port {}=PORT",
                    reference.name,
                    reference.project,
                    reference.branch,
                    reference.key()
                )
            };
            format!(
                "'${{{atom}}}' needs a running fog instance for '{}' — {hint}",
                reference.target()
            )
        }
        PickFailure::MissingPort(available) => format!(
            "'${{{atom}}}': '{}' is running but declares no '{}' port (available: {available})",
            reference.target(),
            reference.name
        ),
        PickFailure::Ambiguous(detail) => format!(
            "'${{{atom}}}' is ambiguous: {detail}; pass --port {}=PORT to choose",
            reference.key()
        ),
    }
}

/// Resolves one cross-instance reference against `instances`, failing when no
/// matching instance is running, when it does not declare the port, or when
/// two matching instances disagree on the value.
pub fn pick_external_port(
    reference: &ExternalPortRef,
    instances: &[LiveInstance],
) -> Result<u16, String> {
    pick_external_port_detailed(reference, instances)
        .map_err(|failure| format_pick_failure(reference, &failure))
}

/// Inserts resolved cross-instance ports into `ports`. A key already present
/// (an explicit `--port` override or a takeover) is left untouched.
pub fn apply_external_refs(
    refs: &[ExternalPortRef],
    instances: &[LiveInstance],
    ports: &mut PortMap,
) -> Result<usize, String> {
    let mut resolved = 0;
    for reference in refs {
        if ports.contains_key(&reference.key()) {
            continue;
        }
        let port = pick_external_port(reference, instances)?;
        ports.insert(reference.key(), port);
        resolved += 1;
    }
    Ok(resolved)
}

/// Resolves every cross-instance reference in `script` against live instances
/// and stores the values in `ports`.
pub fn resolve_external_refs(
    script: &crate::config::ScriptConfig,
    native_routes: Option<&Vec<crate::config::NativeRouteConfig>>,
    ports: &mut PortMap,
) -> Result<usize, String> {
    let refs = collect_external_port_refs(script, native_routes)?;
    if refs.is_empty() {
        return Ok(0);
    }
    apply_external_refs(&refs, &live_instances(), ports)
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
        } else if r.port.trim().parse::<u16>().is_ok() {
            // literal port (whitespace-padded numerics included) — ok
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
        } else {
            return Err(format!(
                "native_routes for service '{}' has invalid port '{}'",
                r.service, r.port
            ));
        }
    }
    Ok(())
}

/// Whether a declared endpoint uses any local `${ports.<name>}` template.
fn endpoint_has_template(s: &crate::config::EndpointConfig) -> bool {
    s.host.as_ref().is_some_and(|h| has_local_port_template(h))
        || s.port.as_ref().is_some_and(|p| has_local_port_template(p))
        || s.path_prefix
            .as_ref()
            .is_some_and(|p| has_local_port_template(p))
        || s.health_check.as_ref().is_some_and(|hc| match hc {
            crate::config::HealthCheckSpec::Single(c) => has_local_port_template(&c.target),
            crate::config::HealthCheckSpec::Multiple(v) => {
                v.iter().any(|c| has_local_port_template(&c.target))
            }
        })
}

/// Validates declared `endpoint` entries: names must be non-empty and
/// unique within their owning service, and `port` (when present) must be a
/// resolvable `${ports.<name>}` template or a literal port. Mirrors
/// [`validate_native_routes`] for the nested form.
pub fn validate_endpoints(
    script: &crate::config::ScriptConfig,
    port_map: &PortMap,
    branch: Option<&str>,
) -> Result<(), String> {
    let Some(entries) = script.service.as_ref() else {
        return Ok(());
    };
    for entry in entries {
        let Some(subs) = entry.endpoint.as_ref() else {
            continue;
        };
        let service = entry.name.as_deref().unwrap_or("?");
        let mut seen = std::collections::HashSet::new();
        for sub in subs {
            if sub.name.trim().is_empty() {
                return Err(format!(
                    "service '{service}' has a endpoint with an empty name"
                ));
            }
            if !seen.insert(sub.name.clone()) {
                return Err(format!(
                    "service '{service}' has duplicate endpoint name '{}'",
                    sub.name
                ));
            }
            let Some(port) = sub.port.as_deref() else {
                continue;
            };
            if has_template(port) {
                resolve_template(port, port_map, branch).map_err(|e| {
                    format!(
                        "service '{service}' endpoint '{}' port template error: {e}",
                        sub.name
                    )
                })?;
            } else if port.parse::<u16>().is_err() {
                return Err(format!(
                    "service '{service}' endpoint '{}' port '{}' must be a '${{ports.<name>}}' template or literal port",
                    sub.name, port
                ));
            }
        }
    }
    Ok(())
}

/// Ensures `ports` top-level is defined when any template references a local
/// `${ports.<name>}`. Checks service `cmd`/`shutdown_cmd`/`env`/`health_check`,
/// proxy routes, and native routes. Cross-instance references
/// (`${ports.<name>@<repo>:<branch>}`) are resolved from live instances, so
/// they do not require a `ports` map. Returns `Err` with the same message
/// previously duplicated in `main.rs` and `runtime.rs`.
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
            has_local_port_template(&e.cmd)
                || e.shutdown_cmd
                    .as_ref()
                    .is_some_and(|s| has_local_port_template(s))
                || e.env
                    .as_ref()
                    .is_some_and(|m| m.values().any(|v| has_local_port_template(v)))
                || e.health_check.as_ref().is_some_and(|hc| match hc {
                    crate::config::HealthCheckSpec::Single(c) => has_local_port_template(&c.target),
                    crate::config::HealthCheckSpec::Multiple(v) => {
                        v.iter().any(|c| has_local_port_template(&c.target))
                    }
                })
                || e.endpoint
                    .as_ref()
                    .is_some_and(|subs| subs.iter().any(endpoint_has_template))
        })
    }) || script.proxy.as_ref().is_some_and(|p| {
        p.routes.iter().any(|r| {
            has_local_port_template(&r.upstream)
                || r.host.as_ref().is_some_and(|h| has_local_port_template(h))
        })
    }) || native_routes.is_some_and(|routes| {
        routes
            .iter()
            .any(|r| has_local_port_template(&r.port) || has_local_port_template(&r.host))
    });
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
    fn test_parse_port_overrides_ok() {
        let raw = vec!["api=4000".to_string(), "web=0".to_string()];
        let m = parse_port_overrides(&raw).unwrap();
        assert_eq!(m["api"], 4000);
        assert_eq!(m["web"], 0);
    }

    #[test]
    fn test_parse_port_overrides_trims() {
        let raw = vec![" api = 4000 ".to_string()];
        let m = parse_port_overrides(&raw).unwrap();
        assert_eq!(m["api"], 4000);
    }

    #[test]
    fn test_parse_port_overrides_missing_equals_errors() {
        let err = parse_port_overrides(&["api".to_string()]).unwrap_err();
        assert!(err.contains("NAME=PORT"), "{err}");
    }

    #[test]
    fn test_parse_port_overrides_empty_name_errors() {
        let err = parse_port_overrides(&["=4000".to_string()]).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn test_parse_port_overrides_bad_port_errors() {
        for value in ["abc", "-1", "65536", ""] {
            let item = format!("api={value}");
            let err = parse_port_overrides(std::slice::from_ref(&item)).unwrap_err();
            assert!(err.contains("not a valid port"), "{value}: {err}");
        }
    }

    #[test]
    fn test_parse_port_overrides_duplicate_name_errors() {
        let raw = vec!["api=4000".to_string(), "api=4001".to_string()];
        let err = parse_port_overrides(&raw).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn test_parse_port_overrides_empty_input() {
        assert!(parse_port_overrides(&[]).unwrap().is_empty());
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

    fn script_with_subs(subs: Vec<crate::config::EndpointConfig>) -> crate::config::ScriptConfig {
        crate::config::ScriptConfig {
            service: Some(vec![crate::config::ConfigEntry {
                name: Some("infra".to_string()),
                path: ".".to_string(),
                cmd: "true".to_string(),
                health_check: None,
                endpoint: Some(subs),
                depends_on: None,
                shutdown_cmd: None,
                env: None,
                reuse: false,
                share: false,
            }]),
            proxy: None,
            terminal: None,
            concurrent: true,
        }
    }

    fn sub(name: &str, host: Option<&str>, port: Option<&str>) -> crate::config::EndpointConfig {
        crate::config::EndpointConfig {
            name: name.to_string(),
            host: host.map(String::from),
            port: port.map(String::from),
            path_prefix: None,
            health_check: None,
        }
    }

    #[test]
    fn test_validate_endpoints_ok() {
        let script = script_with_subs(vec![sub(
            "web",
            Some("web.${branch}.acme"),
            Some("${ports.web}"),
        )]);
        let pm = pm(&[("web", 8080)]);
        assert!(validate_endpoints(&script, &pm, Some("main")).is_ok());
    }

    #[test]
    fn test_validate_endpoints_duplicate_name_errors() {
        let script = script_with_subs(vec![sub("web", None, None), sub("web", None, None)]);
        let err = validate_endpoints(&script, &pm(&[]), None).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn test_validate_endpoints_bad_port_errors() {
        let script = script_with_subs(vec![sub("web", Some("web.acme"), Some("nope"))]);
        let err = validate_endpoints(&script, &pm(&[]), None).unwrap_err();
        assert!(err.contains("literal port"), "{err}");
    }

    #[test]
    fn test_validate_endpoints_unknown_port_template_errors() {
        let script = script_with_subs(vec![sub("web", None, Some("${ports.missing}"))]);
        let err = validate_endpoints(&script, &pm(&[("web", 1)]), None).unwrap_err();
        assert!(err.contains("unknown port"), "{err}");
    }

    #[test]
    fn test_ensure_ports_defined_detects_endpoint_template() {
        let script = script_with_subs(vec![sub("web", None, Some("${ports.web}"))]);
        let err = ensure_ports_defined(None, &script, None).unwrap_err();
        assert!(err.contains("'ports' is not defined"), "{err}");
        // With a ports map present it passes.
        let ports = std::collections::HashMap::from([("web".to_string(), 8080u16)]);
        assert!(ensure_ports_defined(Some(&ports), &script, None).is_ok());
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

    #[test]
    fn test_resolve_preserves_non_ascii() {
        let m = pm(&[("api", 3000)]);
        assert_eq!(
            resolve_template("café-${ports.api}", &m, None).unwrap(),
            "café-3000"
        );
        // Non-ASCII on both sides of a template.
        assert_eq!(
            resolve_template("héllo-${ports.api}-wörld", &m, None).unwrap(),
            "héllo-3000-wörld"
        );
    }

    #[test]
    fn test_has_template_only_fog_atoms() {
        // Shell expansions are not fog templates.
        assert!(!has_template("${HOME}/bin"));
        assert!(!has_template("$${not_a_template}"));
        assert!(!has_template("plain"));
        // Real fog atoms are.
        assert!(has_template("${ports.api}"));
        assert!(has_template("${branch}"));
        assert!(has_template("${FOG_BRANCH}"));
        assert!(has_template("${branch_raw}"));
        assert!(has_template("${FOG_BRANCH_RAW}"));
    }

    fn route(service: &str, port: &str) -> crate::config::NativeRouteConfig {
        crate::config::NativeRouteConfig {
            host: "api.acme".to_string(),
            service: service.to_string(),
            port: port.to_string(),
            path_prefix: None,
            endpoint: None,
        }
    }

    #[test]
    fn test_validate_native_routes_accepts_literal_and_template() {
        let m = pm(&[("api", 8080)]);
        assert!(validate_native_routes(&[route("api", "8080")], &m, None).is_ok());
        assert!(validate_native_routes(&[route("api", "${ports.api}")], &m, None).is_ok());
        // Whitespace-padded literal port still resolves as a number.
        assert!(validate_native_routes(&[route("api", " 8080 ")], &m, None).is_ok());
    }

    #[test]
    fn test_validate_native_routes_rejects_invalid_literal_ports() {
        let m = pm(&[]);
        for port in ["nope", "80x", "80 < 90", "  "] {
            let err = validate_native_routes(&[route("api", port)], &m, None).unwrap_err();
            assert!(err.contains("invalid port"), "{port}: {err}");
        }
    }

    #[test]
    fn test_parse_external_port_key_ok() {
        let reference = parse_external_port_key("api@cinema_ticket:main")
            .unwrap()
            .unwrap();
        assert_eq!(
            reference,
            ExternalPortRef {
                name: "api".to_string(),
                project: "cinema_ticket".to_string(),
                branch: "main".to_string(),
                script: None,
            }
        );
        assert_eq!(reference.key(), "api@cinema_ticket:main");
        assert_eq!(reference.atom(), "ports.api@cinema_ticket:main");
        assert_eq!(reference.target(), "cinema_ticket:main");
        // The optional script segment authorizes a cold start.
        let script = parse_external_port_key("api@cinema_ticket:main:dev")
            .unwrap()
            .unwrap();
        assert_eq!(script.key(), "api@cinema_ticket:main:dev");
        assert_eq!(script.target(), "cinema_ticket:main:dev");
        // A raw branch name is accepted; matching happens by slug.
        assert!(
            parse_external_port_key("api@repo:feat/login")
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn test_parse_external_port_key_local_and_malformed() {
        assert!(parse_external_port_key("api").is_none());
        let err = parse_external_port_key("api@repo").unwrap().unwrap_err();
        assert!(err.contains("expected"), "{err}");
        for key in [
            "@repo:main",
            "api@:main",
            "api@repo:",
            "api@repo:main:",
            "api@repo:main:dev:extra",
        ] {
            let err = parse_external_port_key(key).unwrap().unwrap_err();
            assert!(
                err.contains("non-empty") || err.contains("expected"),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn test_parse_external_port_key_accepts_dynamic_branch_tokens() {
        for branch in ["?", "*", "${branch}", "${FOG_BRANCH}", "${branch_raw}"] {
            let key = format!("api@repo:{branch}");
            let reference = parse_external_port_key(&key).unwrap().unwrap();
            assert_eq!(reference.branch, branch);
            assert_eq!(reference.key(), key);
        }
    }

    #[test]
    fn test_parse_port_overrides_external_key() {
        let raw = vec![
            "api@repo:main=4000".to_string(),
            "api@repo:main:dev=4001".to_string(),
        ];
        let m = parse_port_overrides(&raw).unwrap();
        assert_eq!(m["api@repo:main"], 4000);
        assert_eq!(m["api@repo:main:dev"], 4001);
        let err = parse_port_overrides(&["api@repo=4000".to_string()]).unwrap_err();
        assert!(err.contains("cross-instance"), "{err}");
        let err = parse_port_overrides(&["api@repo:main:=4000".to_string()]).unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
    }

    #[test]
    fn test_parse_port_overrides_rejects_zero_for_external_key() {
        let err = parse_port_overrides(&["api@repo:main=0".to_string()]).unwrap_err();
        assert!(err.contains("fixed value"), "{err}");
        let err = parse_port_overrides(&["api@repo:main:dev=0".to_string()]).unwrap_err();
        assert!(err.contains("fixed value"), "{err}");
        // A local name may still be re-randomized for the run.
        let m = parse_port_overrides(&["api=0".to_string()]).unwrap();
        assert_eq!(m["api"], 0);
    }

    #[test]
    fn test_allocate_ports_rejects_reserved_name_characters() {
        for name in ["api@repo", "api:main"] {
            let specs = HashMap::from([(name.to_string(), 0)]);
            let err = allocate_ports(&specs).unwrap_err();
            assert!(err.contains("reserved"), "{name}: {err}");
        }
    }

    #[test]
    fn test_expand_branch_token() {
        assert_eq!(
            expand_branch_token("${branch}", Some("feat/login")).as_deref(),
            Some("feat-login")
        );
        assert_eq!(
            expand_branch_token("${FOG_BRANCH}", Some("feat/login")).as_deref(),
            Some("feat-login")
        );
        assert_eq!(
            expand_branch_token("${branch_raw}", Some("feat/login")).as_deref(),
            Some("feat/login")
        );
        assert_eq!(
            expand_branch_token("${branch}", None),
            None,
            "a detached HEAD cannot resolve the atom"
        );
        assert_eq!(
            expand_branch_token("main", Some("feat/login")).as_deref(),
            Some("main")
        );
        assert_eq!(expand_branch_token("main", None).as_deref(), Some("main"));
    }

    #[test]
    fn test_resolve_external_atom_uses_resolved_key() {
        let m = pm(&[
            ("api@cinema_ticket:main", 8080),
            ("api@cinema_ticket:main:dev", 8081),
        ]);
        assert_eq!(
            resolve_template("http://localhost:${ports.api@cinema_ticket:main}", &m, None).unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            resolve_template(
                "http://localhost:${ports.api@cinema_ticket:main:dev}",
                &m,
                None
            )
            .unwrap(),
            "http://localhost:8081"
        );
    }

    #[test]
    fn test_resolve_template_external_with_nested_branch_atom() {
        let m = pm(&[("api@cinema_ticket:${branch}:serve", 8080)]);
        assert_eq!(
            resolve_template(
                "http://localhost:${ports.api@cinema_ticket:${branch}:serve}",
                &m,
                Some("feat/login")
            )
            .unwrap(),
            "http://localhost:8080"
        );
    }

    #[test]
    fn test_scan_port_atoms_keeps_nested_branch_atom() {
        let mut refs = std::collections::BTreeSet::new();
        referenced_port_names(
            "x ${ports.api@cinema_ticket:${branch}:serve} y ${ports.web}",
            &mut refs,
        );
        assert_eq!(
            refs.into_iter().collect::<Vec<_>>(),
            vec!["web".to_string()]
        );
    }

    #[test]
    fn test_resolve_external_atom_unresolved_errors() {
        let m = pm(&[]);
        let err = resolve_template("${ports.api@cinema_ticket:main}", &m, None).unwrap_err();
        assert!(err.contains("not resolved"), "{err}");
        assert!(err.contains("cinema_ticket"), "{err}");
        assert!(err.contains(":script"), "{err}");
        let err = resolve_template("${ports.api@cinema_ticket:main:dev}", &m, None).unwrap_err();
        assert!(err.contains("cinema_ticket:main:dev"), "{err}");
    }

    #[test]
    fn test_resolve_external_atom_malformed_errors() {
        let m = pm(&[("api@repo", 1)]);
        let err = resolve_template("${ports.api@repo}", &m, None).unwrap_err();
        assert!(err.contains("invalid cross-instance"), "{err}");
    }

    #[test]
    fn test_referenced_port_names_skips_external_refs() {
        let mut out = std::collections::BTreeSet::new();
        referenced_port_names(
            "${ports.api} ${ports.db@repo:main} ${ports.web@repo:feat/x} \
             ${ports.cache@repo:main:dev}",
            &mut out,
        );
        assert_eq!(out, std::collections::BTreeSet::from(["api".to_string()]));
    }

    #[test]
    fn test_has_local_port_template() {
        assert!(has_local_port_template("${ports.api}"));
        assert!(!has_local_port_template("${ports.api@repo:main}"));
        assert!(!has_local_port_template("${ports.api@repo:main:dev}"));
        assert!(!has_local_port_template("${branch}"));
        assert!(!has_local_port_template("plain"));
    }

    #[test]
    fn test_referenced_local_port_names_walks_script() {
        let mut script = script_with_env(&[("A", "${ports.api} ${ports.db@repo:main}")]);
        script.proxy = Some(crate::config::ProxyConfig {
            port: 3000,
            host: None,
            routes: vec![crate::config::ProxyRoute {
                path: "/".to_string(),
                host: Some("${ports.ui}.acme".to_string()),
                upstream: "http://localhost:${ports.web@repo:main}".to_string(),
                ws: None,
            }],
            tls_cert: None,
            tls_key: None,
            max_log_entries: None,
        });
        let routes = vec![route("api", "${ports.api}")];
        let names = referenced_local_port_names(&script, Some(&routes));
        assert_eq!(
            names,
            std::collections::BTreeSet::from(["api".to_string(), "ui".to_string(),])
        );
    }

    fn script_with_env(values: &[(&str, &str)]) -> crate::config::ScriptConfig {
        let env = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        crate::config::ScriptConfig {
            service: Some(vec![crate::config::ConfigEntry {
                name: Some("web".to_string()),
                path: ".".to_string(),
                cmd: "true".to_string(),
                health_check: None,
                endpoint: None,
                depends_on: None,
                shutdown_cmd: None,
                env: Some(env),
                reuse: false,
                share: false,
            }]),
            proxy: None,
            terminal: None,
            concurrent: true,
        }
    }

    #[test]
    fn test_ensure_ports_defined_external_and_branch_only() {
        let external = script_with_env(&[("API", "http://localhost:${ports.api@repo:main}")]);
        assert!(ensure_ports_defined(None, &external, None).is_ok());
        let branch_only = script_with_env(&[("HOST", "http://${branch}.acme")]);
        assert!(ensure_ports_defined(None, &branch_only, None).is_ok());
        let local = script_with_env(&[("API", "http://localhost:${ports.api}")]);
        let err = ensure_ports_defined(None, &local, None).unwrap_err();
        assert!(err.contains("'ports' is not defined"), "{err}");
    }

    #[test]
    fn test_collect_external_port_refs_scans_fields_and_dedupes() {
        let mut script = script_with_env(&[("A", "${ports.api@repo:main}")]);
        script.proxy = Some(crate::config::ProxyConfig {
            port: 3000,
            host: None,
            routes: vec![crate::config::ProxyRoute {
                path: "/".to_string(),
                host: Some("${ports.api@repo:feat/login}.acme".to_string()),
                upstream: "http://localhost:${ports.web@repo:main}".to_string(),
                ws: None,
            }],
            tls_cert: None,
            tls_key: None,
            max_log_entries: None,
        });
        let routes = vec![route("api", "${ports.api@repo:main}")];
        let refs = collect_external_port_refs(&script, Some(&routes)).unwrap();
        let keys: Vec<String> = refs.iter().map(ExternalPortRef::key).collect();
        // BTreeMap order: api@repo:feat/login, api@repo:main, web@repo:main
        assert_eq!(
            keys,
            vec!["api@repo:feat/login", "api@repo:main", "web@repo:main"]
        );
        // A script segment is part of the key, so it is a distinct reference.
        let with_script = script_with_env(&[("A", "${ports.cache@repo:main:dev}")]);
        let refs = collect_external_port_refs(&with_script, None).unwrap();
        assert_eq!(refs[0].key(), "cache@repo:main:dev");
        // A malformed reference surfaces as an error.
        let bad = script_with_env(&[("A", "${ports.api@repo}")]);
        let err = collect_external_port_refs(&bad, None).unwrap_err();
        assert!(err.contains("expected"), "{err}");
    }

    fn instance(
        pid: u32,
        script: &str,
        project: &str,
        branch: Option<&str>,
        pairs: &[(&str, u16)],
    ) -> LiveInstance {
        LiveInstance {
            pid,
            script: script.to_string(),
            project: Some(project.to_string()),
            branch: branch.map(String::from),
            ports: pm(pairs),
        }
    }

    #[test]
    fn test_pick_external_port_matches_display_name_and_slug() {
        let reference = parse_external_port_key("api@repo:feat-login")
            .unwrap()
            .unwrap();
        let instances = vec![instance(
            1,
            "dev",
            "/tmp/repo/.git",
            Some("feat/login"),
            &[("api", 8080)],
        )];
        assert_eq!(pick_external_port(&reference, &instances).unwrap(), 8080);
    }

    #[test]
    fn test_pick_external_port_matches_index_worktree_segment() {
        // The index truncates `release/1.2` to the first DNS label,
        // `release-1`; a reference copied from the UI URL uses that label.
        let reference = parse_external_port_key("api@repo:release-1")
            .unwrap()
            .unwrap();
        let instances = vec![instance(
            1,
            "dev",
            "/tmp/repo/.git",
            Some("release/1.2"),
            &[("api", 8080)],
        )];
        assert_eq!(pick_external_port(&reference, &instances).unwrap(), 8080);
    }

    #[test]
    fn test_pick_external_port_matches_project_name_case_insensitively() {
        let reference = parse_external_port_key("api@Cinema_Ticket:main")
            .unwrap()
            .unwrap();
        let instances = vec![instance(
            1,
            "dev",
            "/tmp/cinema_ticket/.git",
            Some("main"),
            &[("api", 8080)],
        )];
        assert_eq!(pick_external_port(&reference, &instances).unwrap(), 8080);
    }

    #[test]
    fn test_pick_external_port_filters_by_script() {
        let reference = parse_external_port_key("api@repo:main:worker")
            .unwrap()
            .unwrap();
        let instances = vec![
            instance(1, "dev", "/tmp/repo/.git", Some("main"), &[("api", 8080)]),
            instance(
                2,
                "worker",
                "/tmp/repo/.git",
                Some("main"),
                &[("api", 8081)],
            ),
        ];
        assert_eq!(pick_external_port(&reference, &instances).unwrap(), 8081);
        let reference = parse_external_port_key("api@repo:main:api")
            .unwrap()
            .unwrap();
        assert_eq!(
            pick_external_port_detailed(&reference, &instances),
            Err(PickFailure::NoInstance)
        );
    }

    #[test]
    fn test_pick_external_port_no_instance_errors() {
        let reference = parse_external_port_key("api@repo:main").unwrap().unwrap();
        let err = pick_external_port(&reference, &[]).unwrap_err();
        assert!(err.contains("running fog instance"), "{err}");
        assert!(err.contains("--port api@repo:main=PORT"), "{err}");
        assert!(err.contains(":script"), "{err}");
        assert_eq!(
            pick_external_port_detailed(&reference, &[]),
            Err(PickFailure::NoInstance)
        );
        let script = parse_external_port_key("api@repo:main:dev")
            .unwrap()
            .unwrap();
        let err = pick_external_port(&script, &[]).unwrap_err();
        assert!(err.contains("repo:main:dev"), "{err}");
    }

    #[test]
    fn test_pick_external_port_missing_name_lists_available() {
        let reference = parse_external_port_key("db@repo:main").unwrap().unwrap();
        let instances = vec![instance(
            1,
            "dev",
            "/tmp/repo/.git",
            Some("main"),
            &[("api", 8080), ("web", 5173)],
        )];
        let err = pick_external_port(&reference, &instances).unwrap_err();
        assert!(err.contains("api, web"), "{err}");
    }

    #[test]
    fn test_pick_external_port_disagreement_errors() {
        let reference = parse_external_port_key("api@repo:main").unwrap().unwrap();
        let instances = vec![
            instance(1, "dev", "/tmp/repo/.git", Some("main"), &[("api", 8080)]),
            instance(2, "alt", "/tmp/repo/.git", Some("main"), &[("api", 8081)]),
        ];
        let err = pick_external_port(&reference, &instances).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("8080") && err.contains("8081"), "{err}");
    }

    #[test]
    fn test_pick_external_port_same_value_ok() {
        let reference = parse_external_port_key("api@repo:main").unwrap().unwrap();
        let instances = vec![
            instance(1, "dev", "/tmp/repo/.git", Some("main"), &[("api", 8080)]),
            instance(2, "alt", "/tmp/repo/.git", Some("main"), &[("api", 8080)]),
        ];
        assert_eq!(pick_external_port(&reference, &instances).unwrap(), 8080);
    }

    #[test]
    fn test_apply_external_refs_keeps_override() {
        let reference = parse_external_port_key("api@repo:main").unwrap().unwrap();
        let instances = vec![instance(
            1,
            "dev",
            "/tmp/repo/.git",
            Some("main"),
            &[("api", 8080)],
        )];
        let mut ports = pm(&[("api@repo:main", 9999)]);
        assert_eq!(
            apply_external_refs(std::slice::from_ref(&reference), &instances, &mut ports).unwrap(),
            0
        );
        assert_eq!(ports["api@repo:main"], 9999);
        let mut empty = pm(&[]);
        assert_eq!(
            apply_external_refs(&[reference], &instances, &mut empty).unwrap(),
            1
        );
        assert_eq!(empty["api@repo:main"], 8080);
    }
}
