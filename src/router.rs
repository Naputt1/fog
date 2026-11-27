use crate::config::RouterConfig;
use crate::error::{FogError, FogResult};
use crate::process::run_with_timeout;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::sync::Once;
use std::time::Duration;

/// Wall-clock budget for the fast docker control-plane calls made during router
/// setup. A hung Docker Desktop would otherwise block startup forever.
const DOCKER_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Longer budget for `docker run`: a first run may need to pull the Traefik
/// image before the container starts.
const DOCKER_RUN_TIMEOUT: Duration = Duration::from_secs(180);

/// Warning used when a docker CLI call exceeds its budget.
fn docker_timeout_msg(cmd: &str, timeout: Duration) -> String {
    format!(
        "docker {cmd} did not respond within {}s; is Docker Desktop running or hung? \
         (check with `docker info`)",
        timeout.as_secs()
    )
}

/// Ensures the central reverse-proxy (Traefik) router exists on the host.
///
/// This is the router analog of [`crate::dnsmasq::ensure`]: it is a
/// host-global resource managed by fog once, independent of any project or
/// branch. App compose files opt into routing by attaching a service to
/// `shared_network` and declaring Traefik labels; fog only guarantees the
/// router and that network exist.
///
/// When [`RouterConfig::tls`] is enabled, fog also generates local-CA wildcard
/// certificates (via mkcert) for `domains` (from the `dnsmasq` section) plus
/// the router hostname and `localhost`, and Traefik terminates HTTPS on `:443`
/// (a `websecure` entrypoint) using them. HTTP on `:80` keeps working.
///
/// Best-effort: it never fails the run. Errors/warnings are returned as
/// messages for the caller to print.
pub fn ensure(cfg: &RouterConfig, domains: &[String]) -> Vec<String> {
    let mut messages = Vec::new();

    if !command_exists("docker") {
        messages.push(
            "⚠ docker is not installed; skipping central router setup. \
             Install docker and re-run for per-branch hostname routing."
                .to_string(),
        );
        return messages;
    }

    // 1. Create the shared network (idempotent). A timeout here means Docker
    //    is not answering at all, so bail before every later docker call hangs
    //    the daemon too.
    match network_exists(&cfg.shared_network) {
        Ok(true) => {}
        Ok(false) => match create_network(&cfg.shared_network) {
            Ok(()) => messages.push(format!(
                "  + created shared router network '{}'",
                cfg.shared_network
            )),
            Err(e) => {
                messages.push(format!(
                    "⚠ could not create network '{}' ({}); routing may not work.",
                    cfg.shared_network, e
                ));
                return messages;
            }
        },
        Err(e) => {
            messages.push(format!("⚠ {e}; skipping central router setup."));
            return messages;
        }
    }

    // 2. Write the service-directory catch-all router via the file provider so
    //    requests with no matching app host are served the fog index page. This
    //    runs before the docker provider's specific routers, which win by being
    //    more specific.
    if let Some(m) = ensure_index_router(cfg, &mut messages) {
        messages.push(m);
    }

    // 3. When TLS is enabled, ensure the CA + wildcard certificates and the
    //    per-domain file-provider config exist before (re)starting Traefik.
    //
    //    If TLS was requested but cert setup failed, we must NOT start a plain
    //    router: the router is host-global and shared, and silently downgrading
    //    would tear down HTTPS for every other project. We keep any existing
    //    router (TLS or not) and warn loudly instead.
    let mut tls_paths: Option<(PathBuf, PathBuf)> = None; // (cert_dir, dynamic_dir)
    if cfg.tls.enabled {
        match ensure_tls(cfg, domains, &[], &mut messages) {
            Ok((certs, dynamic)) => tls_paths = Some((certs, dynamic)),
            Err(e) => {
                messages.push(format!(
                    "⚠ TLS requested but certificate setup failed: {e}. HTTPS on :443 left \
                     as-is; run `fog <script>` interactively once after installing mkcert."
                ));
            }
        }
    }

    // 4. Start (or recreate) the router container.
    //
    //    Recreate only when:
    //      - no container exists, or
    //      - the running router has no TLS but TLS was requested (upgrade), or
    //      - TLS was requested but cert setup failed AND the running router
    //        also lacks TLS (nothing to preserve).
    //    We NEVER recreate purely because the current config's `tls.enabled` is
    //    false: the router is host-global and shared across projects, so a
    //    project that does not enable TLS must not tear down HTTPS that another
    //    project already brought up.
    let container = router_container_name(&cfg.image);
    let running = container_exists(&container);
    let running_tls = running && container_tls_enabled(&container);
    let recreate = should_recreate(running, running_tls, cfg.tls.enabled);
    if recreate {
        if running {
            let _ = remove_container(&container);
        }
        let tls = tls_paths.as_ref().map(|(c, d)| (c.as_path(), d.as_path()));
        match start_router(cfg, &container, tls) {
            Ok(()) => messages.push(format!(
                "  + started central router '{}' (Traefik dashboard: {:?}{})",
                container,
                dashboard_url(cfg),
                if cfg.tls.enabled {
                    ", HTTPS :443 enabled"
                } else {
                    ""
                }
            )),
            Err(e) => messages.push(format!(
                "⚠ could not start central router '{}' ({}); per-branch hostname routing \
                 may be unavailable. Check `docker ps -a` for conflicts.",
                container, e
            )),
        }
    }

    messages
}

/// Writes the file-provider catch-all router that serves the fog service index
/// for requests with no matching app host. The router must exist in the
/// `dynamic/` dir that Traefik watches; the embedded index server (see
/// [`crate::index`]) is reached over `host.docker.internal`.
fn ensure_index_router(cfg: &RouterConfig, messages: &mut Vec<String>) -> Option<String> {
    let dynamic_dir = default_dynamic_dir(cfg);
    let _ = fs::create_dir_all(&dynamic_dir);
    let index_port = cfg.index_port.unwrap_or(18080);
    let content = format!(
        r#"[http.routers]
  [http.routers.fog-index]
    rule = "Host(`*`)"
    priority = 1
    entryPoints = ["web"]
    service = "fog-index"

[http.services]
  [http.services.fog-index.loadBalancer]
    [[http.services.fog-index.loadBalancer.servers]]
      url = "http://host.docker.internal:{index_port}/"
"#
    );
    let file = dynamic_dir.join("index.toml");
    let changed = fs::read_to_string(&file)
        .map(|c| c != content)
        .unwrap_or(true);
    if changed {
        if let Err(e) = fs::write(&file, &content) {
            messages.push(format!(
                "⚠ could not write index router {}: {e}",
                file.display()
            ));
            return None;
        }
        Some(format!(
            "  + added service index router (unmatched hosts → :{index_port})"
        ))
    } else {
        None
    }
}

/// Directory holding Traefik file-provider dynamic config, mounted into the
/// router container at `/etc/fog/dynamic`.
fn default_dynamic_dir(cfg: &RouterConfig) -> PathBuf {
    PathBuf::from(&cfg.tls.cert_dir).join("dynamic")
}

/// Dynamic dir derived from a `Config`'s router cert dir (or default).
fn default_dynamic_dir_for_config(cfg: &crate::config::Config) -> PathBuf {
    let cert_dir = cfg
        .router
        .as_ref()
        .map(|r| r.tls.cert_dir.clone())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}/.config/fog/certs")
        });
    PathBuf::from(cert_dir).join("dynamic")
}

/// Sanitizes a string for use as a Traefik router/service name and filename.
///
/// Router names are written as TOML bare keys, which only allow ASCII letters,
/// digits, `_` and `-`; a Unicode alphanumeric from a checkout, branch or
/// service name would make the whole route file unparseable.
fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Stable, filesystem-safe scope for one project checkout.
///
/// Native route files live in a host-global dynamic dir, so the file name must
/// identify the project that wrote it: two projects can share a branch (and
/// even a service name), and a shutdown must remove only its own routes. The
/// scope is the checkout path's basename plus a short hash of the absolute
/// path, so it is stable across restarts and distinct across worktrees.
fn route_scope(scope: &Path) -> String {
    let absolute = std::path::absolute(scope).unwrap_or_else(|_| scope.to_path_buf());
    let name = absolute
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let slug = sanitize_name(&name.to_ascii_lowercase());
    let slug = if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    };
    format!(
        "{slug}-{:08x}",
        fnv1a(absolute.to_string_lossy().as_bytes()) as u32
    )
}

/// FNV-1a, used only for short stable identifiers (route scopes, the mkcert CA
/// fingerprint). Not a cryptographic hash.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Ensures native (host) routes are written as Traefik file-provider config.
///
/// Each `native_routes` entry becomes a router+service that forwards
/// `Host(<host>)` to `http://host.docker.internal:<port>`. Explicit only:
/// the caller must have already validated that `port` resolves and `host`
/// is known. `branch` is used to resolve `${branch}` templates. Success
/// messages are only produced when `verbose` is set; warnings are always.
pub fn ensure_native_routes(
    routes: &[crate::config::NativeRouteConfig],
    ports: &crate::ports::PortMap,
    branch: Option<&str>,
    scope: &Path,
    cfg: &crate::config::Config,
    verbose: bool,
) -> Vec<String> {
    let mut messages = Vec::new();
    let dynamic_dir = default_dynamic_dir_for_config(cfg);
    let _ = fs::create_dir_all(&dynamic_dir);
    let scope_slug = route_scope(scope);
    let branch_slug = sanitize_name(branch.unwrap_or("default"));
    let owner = route_owner(&scope_slug, &branch_slug);
    let mut routed_hosts: Vec<String> = Vec::new();

    for r in routes {
        // Resolve host and port templates
        let host = match crate::ports::resolve_template(&r.host, ports, branch) {
            Ok(h) => h,
            Err(e) => {
                messages.push(format!(
                    "⚠ native route for service '{}' host error: {}",
                    r.service, e
                ));
                continue;
            }
        };
        let port_str = match crate::ports::resolve_template(&r.port, ports, branch) {
            Ok(p) => p,
            Err(e) => {
                messages.push(format!(
                    "⚠ native route for service '{}' port error: {}",
                    r.service, e
                ));
                continue;
            }
        };
        let port: u16 = match port_str.parse() {
            Ok(p) => p,
            Err(_) => {
                messages.push(format!(
                    "⚠ native route for service '{}' has non-numeric port '{}'",
                    r.service, port_str
                ));
                continue;
            }
        };
        let svc_slug = sanitize_name(&r.service);
        let host_slug = sanitize_name(&host);
        // Endpoint routes get a distinct `ep-` prefix (both in the router
        // name and the file name) so they never collide with a top-level
        // `native_routes` entry for the same service. The scope segment keeps
        // two projects on the same branch from overwriting (or cleaning up)
        // each other's routes.
        let (name, legacy) = match &r.endpoint {
            Some(endpoint) => (
                format!(
                    "ep-{}-{}-{}-{}-{}",
                    scope_slug,
                    branch_slug,
                    svc_slug,
                    sanitize_name(endpoint),
                    host_slug
                ),
                format!(
                    "ep-{}-{}-{}-{}",
                    branch_slug,
                    svc_slug,
                    sanitize_name(endpoint),
                    host_slug
                ),
            ),
            None => (
                format!(
                    "native-{}-{}-{}-{}",
                    scope_slug, branch_slug, svc_slug, host_slug
                ),
                format!("native-{}-{}-{}", branch_slug, svc_slug, host_slug),
            ),
        };
        // Migrate the pre-scope file for this exact route: it can only belong
        // to this branch+service+host, and leaving it would put two routers
        // with the same Host and priority in the dynamic dir.
        let _ = fs::remove_file(dynamic_dir.join(format!("{legacy}.toml")));
        if !routed_hosts.contains(&host) {
            routed_hosts.push(host.clone());
        }
        let rule = if let Some(p) = &r.path_prefix {
            format!("Host(`{}`) && PathPrefix(`{}`)", host, p)
        } else {
            format!("Host(`{}`)", host)
        };
        let content = format!(
            r#"{owner}

[http.routers]
  [http.routers.{name}]
    rule = "{rule}"
    entryPoints = ["web"]
    service = "{name}"
    priority = 100
  [http.routers.{name}-secure]
    rule = "{rule}"
    entryPoints = ["websecure"]
    service = "{name}"
    priority = 100
    [http.routers.{name}-secure.tls]

[http.services]
  [http.services.{name}.loadBalancer]
    [[http.services.{name}.loadBalancer.servers]]
      url = "http://host.docker.internal:{port}"
"#
        );
        let file = dynamic_dir.join(format!("{}.toml", name));
        let changed = fs::read_to_string(&file)
            .map(|c| c != content)
            .unwrap_or(true);
        if changed {
            if let Err(e) = fs::write(&file, &content) {
                messages.push(format!(
                    "⚠ could not write native route {}: {e}",
                    file.display()
                ));
                continue;
            }
            if verbose {
                let prefix = crate::log::INFO_PREFIX;
                match &r.endpoint {
                    Some(endpoint) => messages.push(format!(
                        "{prefix}endpoint route {}.{} -> host.docker.internal:{} (Host: {})",
                        r.service, endpoint, port, host
                    )),
                    None => messages.push(format!(
                        "{prefix}native route {} -> host.docker.internal:{} (Host: {})",
                        r.service, port, host
                    )),
                }
            }
        }
    }

    // Every routed host must appear in a certificate with an exact SAN: a bare
    // `*.<domain>` is invalid for the single-label domains fog uses, so a
    // verifier rejects it (`desired_sans`).
    //
    // Best-effort: a certificate problem must never fail route setup, and a
    // config without `dnsmasq.domains` has nothing to certify.
    let domains = cfg
        .dnsmasq
        .as_ref()
        .map(|d| d.domains.clone())
        .unwrap_or_default();
    if !domains.is_empty()
        && let Some(router) = tls_router(cfg)
        && let Err(e) = ensure_tls(&router, &domains, &routed_hosts, &mut messages)
    {
        messages.push(format!("⚠ could not refresh TLS certificates: {e}"));
    }

    messages
}

/// Router config to use when refreshing the certificates for routed hosts.
///
/// The router is host-global, so a project can route hosts through a TLS router
/// it never declares itself. Prefer the project's `router`, then the global
/// config's, and finally the shared router when it is already serving
/// `websecure` — its certificates are what browsers see, whoever started it.
/// `None` means nothing terminates HTTPS, so there is no entrypoint that would
/// accept these certificates.
fn tls_router(cfg: &crate::config::Config) -> Option<RouterConfig> {
    let declared = cfg
        .router
        .clone()
        .or_else(|| crate::config::load_global_config().and_then(|global| global.router));
    if let Some(router) = &declared
        && router.tls.enabled
    {
        return Some(router.clone());
    }
    let router = declared.unwrap_or_default();
    let container = router_container_name(&router.image);
    (container_exists(&container) && container_tls_enabled(&container)).then_some(router)
}

/// First line written into every generated route file. The file name can only
/// encode a hyphen-delimited branch slug, so `main` and `main-2` share a
/// prefix; the exact owner line lets cleanup tell them apart.
const ROUTE_OWNER_PREFIX: &str = "# fog-route: ";

fn route_owner(scope_slug: &str, branch_slug: &str) -> String {
    format!("{ROUTE_OWNER_PREFIX}scope={scope_slug} branch={branch_slug}")
}

/// Removes the native route files this project+branch owns.
///
/// The dynamic dir is host-global, so cleanup must stay within one project
/// checkout and one branch: an unresolvable branch publishes under the
/// `default` slug (see [`ensure_native_routes`]), and must not delete routes
/// other projects still serve. Pre-scope files are migrated when their exact
/// route is rewritten, never deleted by a broad branch prefix here.
pub fn cleanup_native_routes(branch: Option<&str>, scope: &Path, cfg: &crate::config::Config) {
    let dynamic_dir = default_dynamic_dir_for_config(cfg);
    let branch_slug = sanitize_name(branch.unwrap_or("default"));
    let scope_slug = route_scope(scope);
    let owner = route_owner(&scope_slug, &branch_slug);
    let prefixes = [
        format!("native-{scope_slug}-{branch_slug}-"),
        format!("ep-{scope_slug}-{branch_slug}-"),
        // Legacy endpoint prefix from before the `ep-` rename.
        format!("sub-{scope_slug}-{branch_slug}-"),
    ];
    let Ok(entries) = fs::read_dir(&dynamic_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".toml") || !prefixes.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        // A generated file whose owner line names another branch only matched
        // because one branch slug prefixes the other (`main` vs `main-2`).
        // Files without an owner line predate the marker; the prefix is the
        // best available ownership test and matches the old cleanup behavior.
        let owned = match fs::read_to_string(entry.path()) {
            Ok(content) => match content.lines().next() {
                Some(line) if line.starts_with(ROUTE_OWNER_PREFIX) => line == owner,
                _ => true,
            },
            Err(_) => true,
        };
        if owned {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Extracts the hostnames from every `Host(...)` rule fragment.
///
/// Only `Host(...)` is matched (not `HostRegexp(...)`), and only its
/// backtick-quoted arguments, so unrelated content such as a dotted
/// `PathPrefix` is ignored. `Host(a, b)` is valid Traefik syntax and both hosts
/// must enter the certificate.
fn host_rule_hosts(rule: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut rest = rule;
    while let Some(start) = rest.find("Host(") {
        let after = &rest[start + "Host(".len()..];
        let Some(end) = after.find(')') else { break };
        let mut args = &after[..end];
        while let Some(open) = args.find('`') {
            let quoted = &args[open + 1..];
            let Some(close) = quoted.find('`') else { break };
            let host = quoted[..close].trim();
            if !host.is_empty() && !hosts.iter().any(|h| h == host) {
                hosts.push(host.to_string());
            }
            args = &quoted[close + 1..];
        }
        rest = &after[end + 1..];
    }
    hosts
}

/// Collects every hostname routed through the Traefik file provider by reading
/// the `*.toml` route files in `dynamic_dir`.
fn discover_route_hosts(dynamic_dir: &Path) -> Vec<String> {
    let mut hosts = Vec::new();
    let Ok(entries) = fs::read_dir(dynamic_dir) else {
        return hosts;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().ends_with(".toml") {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in content.lines() {
            if !line.contains("Host(`") {
                continue;
            }
            for host in host_rule_hosts(line) {
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        }
    }
    hosts
}

/// Computes the full SAN list for the certificate named `name`.
///
/// Every certificate covers the bare name. A wildcard `*.<name>` is added only
/// when `name` has more than one label, because verifiers reject a wildcard
/// whose suffix is a single label (e.g. `*.shutterbooth`). For a dnsmasq domain,
/// routed hostnames are added as exact SANs — a bare `*.<domain>` is invalid for
/// the single-label domains fog uses — while `<branch>.<service>.<domain>` hosts
/// also get `*.<service>.<domain>`, which is valid and covers future branches
/// under that service. Names deeper still are added verbatim.
fn desired_sans(name: &str, domains: &[String], route_hosts: &[String]) -> Vec<String> {
    let mut sans = vec![name.to_string()];
    if name.contains('.') {
        sans.push(format!("*.{name}"));
    }
    if domains.iter().any(|d| d == name) {
        let suffix = format!(".{name}");
        for host in route_hosts {
            let Some(prefix) = host.strip_suffix(&suffix) else {
                continue;
            };
            let labels: Vec<&str> = prefix.split('.').filter(|l| !l.is_empty()).collect();
            match labels.len() {
                // Bare `<domain>` is already covered.
                0 => {}
                // A one-label prefix (`<branch>.<domain>`) cannot use a
                // wildcard, so list the hostname exactly.
                1 => sans.push(host.clone()),
                2 => sans.push(format!("*.{}.{}", labels[1], name)),
                _ => sans.push(host.clone()),
            }
        }
    }
    sans.sort();
    sans.dedup();
    sans
}

/// Sidecar recording every hostname previously covered by a certificate.
///
/// Certificates must only ever grow: the dynamic dir is shared, and route files
/// disappear when an instance stops. Remembering hosts makes a stopped
/// sibling's hostname stay valid instead of quietly shrinking the certificate
/// back to a state where Chrome sees Traefik's default self-signed cert.
fn known_hosts_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join("known-hosts.txt")
}

/// Reads the persistent host registry, seeded from the `.sans` sidecars that
/// predate it so an upgrade does not drop an already-certified host.
fn read_known_hosts(cert_dir: &Path) -> Vec<String> {
    let mut hosts = Vec::new();
    if let Ok(text) = fs::read_to_string(known_hosts_path(cert_dir)) {
        for line in text.lines() {
            let host = line.trim();
            if is_plain_hostname(host) {
                push_unique(&mut hosts, host);
            }
        }
    }
    if let Ok(entries) = fs::read_dir(cert_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sans") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let host = line.trim();
                // Wildcards are recomputed from exact hosts; only remember
                // names a wildcard cannot express.
                if !host.starts_with("*.") && is_plain_hostname(host) {
                    push_unique(&mut hosts, host);
                }
            }
        }
    }
    hosts
}

fn push_unique(hosts: &mut Vec<String>, host: &str) {
    if !hosts.iter().any(|h| h == host) {
        hosts.push(host.to_string());
    }
}

/// Accepts only a plain hostname (letters, digits, `-`, `.`), never a wildcard
/// or a path fragment. The registry is data, not a filesystem path, but keeping
/// it well-formed avoids feeding junk into mkcert.
fn is_plain_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with('.')
        && !host.contains("..")
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

fn under_domains(host: &str, domains: &[String]) -> bool {
    domains
        .iter()
        .any(|domain| host == domain || host.ends_with(&format!(".{domain}")))
}

/// Merges newly routed hosts into the persistent registry and returns the full
/// set to certify. Best-effort: an unwritable cert dir still certifies this
/// run's hosts, it just cannot remember them for the next one.
fn remember_hosts(cert_dir: &Path, domains: &[String], new_hosts: &[String]) -> Vec<String> {
    let mut hosts = read_known_hosts(cert_dir);
    for host in new_hosts {
        if host.starts_with("*.") || !is_plain_hostname(host) || !under_domains(host, domains) {
            continue;
        }
        push_unique(&mut hosts, host);
    }
    // Bound the file so a long-lived machine cannot grow it forever; keep the
    // most recently appended hosts.
    const MAX_HOSTS: usize = 1000;
    if hosts.len() > MAX_HOSTS {
        hosts.drain(..hosts.len() - MAX_HOSTS);
    }
    let content = format!("{}\n", hosts.join("\n"));
    let path = known_hosts_path(cert_dir);
    if fs::read_to_string(&path).ok().as_deref() != Some(content.as_str()) {
        let _ = fs::write(&path, content);
    }
    hosts
}

/// Path to the mkcert root CA currently in use, if mkcert is installed.
fn mkcert_ca_root() -> Option<PathBuf> {
    let mut cmd = Command::new("mkcert");
    cmd.arg("-CAROOT");
    let out = run_with_timeout(cmd, Duration::from_secs(5))?;
    if !out.status.success() {
        return None;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("rootCA.pem"))
}

/// Identifies the mkcert root CA currently in use. Leaves signed by a replaced
/// CA are untrusted even though their SANs look right, so a change forces every
/// certificate to be regenerated and the user to re-run `mkcert -install`.
fn ca_fingerprint() -> Option<String> {
    let bytes = fs::read(mkcert_ca_root()?).ok()?;
    Some(format!("{:016x}", fnv1a(&bytes)))
}

/// Warns when the mkcert CA is not trusted by the OS. Without this, correct
/// certificates still surface as "self-signed" in Chrome. Checks the root CA
/// itself, once per process, so an already-generated untrusted cert is caught
/// too, not only a freshly generated one.
#[cfg(target_os = "macos")]
fn verify_ca_trust(messages: &mut Vec<String>) {
    static CHECKED: Once = Once::new();
    CHECKED.call_once(|| {
        let Some(root) = mkcert_ca_root() else {
            return;
        };
        // This now runs on every start, not only after generating a leaf, so
        // a wedged `security` (locked keychain, stuck trust evaluation) must
        // not be able to stall startup.
        let mut cmd = Command::new("security");
        cmd.args(["verify-cert", "-c"]).arg(&root);
        let Some(output) = run_with_timeout(cmd, Duration::from_secs(5)) else {
            return;
        };
        if !output.status.success() {
            messages.push(
                "⚠ the mkcert root CA is not trusted by macOS. Run `mkcert -install` once, \
                 then restart fog."
                    .to_string(),
            );
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn verify_ca_trust(_messages: &mut Vec<String>) {}

/// Reads the routers Traefik currently knows about, including those registered
/// by Docker labels. File-provider hosts are already discoverable from disk;
/// label-based routes exist only in Traefik, so this is the only way a compose
/// app's hostname can enter the certificate.
fn traefik_router_hosts(dashboard_port: u16) -> Vec<String> {
    let Some(body) = http_get(&format!("127.0.0.1:{dashboard_port}"), "/api/http/routers") else {
        return Vec::new();
    };
    hosts_from_traefik_routers(&body)
}

fn hosts_from_traefik_routers(body: &str) -> Vec<String> {
    let Ok(routers) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(routers) = routers.as_array() else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    for router in routers {
        if router.get("provider").and_then(|p| p.as_str()) != Some("docker") {
            continue;
        }
        let rule = router.get("rule").and_then(|r| r.as_str()).unwrap_or("");
        for host in host_rule_hosts(rule) {
            push_unique(&mut hosts, &host);
        }
    }
    hosts
}

/// Minimal HTTP GET for localhost diagnostics. The Traefik API runs with
/// `--api.insecure` on a local port, so no TLS or auth is involved.
fn http_get(address: &str, path: &str) -> Option<String> {
    /// A wedged or hostile local server must not make fog buffer unbounded
    /// data. Traefik's router list fits comfortably in a few MiB.
    const MAX_RESPONSE: u64 = 4 * 1024 * 1024;
    let mut addrs = address.to_socket_addrs().ok()?;
    let target = addrs.next()?;
    let mut stream = TcpStream::connect_timeout(&target, Duration::from_millis(500)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut bytes = Vec::new();
    (&mut stream)
        .take(MAX_RESPONSE)
        .read_to_end(&mut bytes)
        .ok()?;
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
    if !head.starts_with("http/1.1 200") && !head.starts_with("http/1.0 200") {
        return None;
    }
    let body = &bytes[split + 4..];
    if head.contains("transfer-encoding: chunked") {
        decode_chunked(body)
    } else {
        Some(String::from_utf8_lossy(body).into_owned())
    }
}

/// Decodes a chunked HTTP body without pulling in an HTTP client.
fn decode_chunked(body: &[u8]) -> Option<String> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let size =
            usize::from_str_radix(std::str::from_utf8(&rest[..line_end]).ok()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        if rest.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// Generates (idempotently) one certificate with the given SANs.
///
/// Regeneration is driven by a `.sans` sidecar recording the names baked into
/// the current cert: the certificate is rewritten whenever the required SAN set
/// changes, and the Traefik dynamic config is rewritten so the new cert is
/// picked up on hot reload.
fn ensure_named_cert(
    name: &str,
    sans: &[String],
    cert_dir: &Path,
    dynamic_dir: &Path,
    force: bool,
    messages: &mut Vec<String>,
) -> FogResult<bool> {
    let cert_file = cert_dir.join(format!("{name}.pem"));
    let key_file = cert_dir.join(format!("{name}-key.pem"));
    let sans_file = cert_dir.join(format!("{name}.sans"));
    let desired = sans.join("\n");

    let dyn_file = dynamic_dir.join(format!("{name}.toml"));
    let content = format!(
        "[[tls.certificates]]\n  certFile = \"/certs/{name}.pem\"\n  \
         keyFile = \"/certs/{name}-key.pem\"\n"
    );

    let up_to_date = !force
        && cert_file.exists()
        && key_file.exists()
        && fs::read_to_string(&sans_file).is_ok_and(|c| c == desired);
    if up_to_date {
        // Certificate already covers the required names; still make sure
        // Traefik has a config pointing at it (e.g. after a manual cleanup).
        if fs::read_to_string(&dyn_file)
            .map(|c| c != content)
            .unwrap_or(true)
        {
            fs::write(&dyn_file, &content).map_err(|e| {
                FogError::new(format!("could not write {}: {}", dyn_file.display(), e))
            })?;
        }
        return Ok(false);
    }

    messages.push(format!("  + generating certificate for {name}"));
    let status = Command::new("mkcert")
        .current_dir(cert_dir)
        .arg("-cert-file")
        .arg(&cert_file)
        .arg("-key-file")
        .arg(&key_file)
        .args(sans)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(_) | Err(_) => {
            return Err(FogError::new(format!("mkcert failed for {name}")));
        }
    }

    fs::write(&sans_file, &desired)
        .map_err(|e| FogError::new(format!("could not write {}: {}", sans_file.display(), e)))?;
    // Rewrite (touch) the dynamic config so Traefik hot-reloads the new cert.
    fs::write(&dyn_file, &content)
        .map_err(|e| FogError::new(format!("could not write {}: {}", dyn_file.display(), e)))?;
    Ok(true)
}

/// Generates (idempotently) the mkcert certificates and the Traefik
/// file-provider dynamic config for every domain, returning the host paths of
/// the cert dir and the dynamic dir mounted into the container.
///
/// Each certificate covers its name plus every hostname routed below it —
/// from this run's routes (`extra_hosts`), the dynamic dir, Traefik's Docker
/// routers, and a persistent registry of hosts seen before. The registry is
/// what keeps a shared certificate from shrinking when a sibling project
/// stops and its route file disappears.
fn ensure_tls(
    cfg: &RouterConfig,
    domains: &[String],
    extra_hosts: &[String],
    messages: &mut Vec<String>,
) -> FogResult<(PathBuf, PathBuf)> {
    if !command_exists("mkcert") {
        return Err(FogError::new(
            "mkcert is not installed; install it with `brew install mkcert` and run \
             `mkcert -install` once"
                .to_string(),
        ));
    }

    let cert_dir = PathBuf::from(&cfg.tls.cert_dir);
    fs::create_dir_all(&cert_dir)
        .map_err(|e| FogError::new(format!("could not create {}: {}", cert_dir.display(), e)))?;
    // Traefik file-provider dynamic config, mounted read-only into the container.
    let dynamic_dir = cert_dir.join("dynamic");
    fs::create_dir_all(&dynamic_dir)
        .map_err(|e| FogError::new(format!("could not create {}: {}", dynamic_dir.display(), e)))?;

    // Hostnames routed through the file provider, plus this run's routes and
    // any Docker-label routers Traefik already knows about. Union them with the
    // persistent registry so certificates never shrink.
    let mut discovered = discover_route_hosts(&dynamic_dir);
    discovered.extend_from_slice(extra_hosts);
    discovered.extend(traefik_router_hosts(cfg.dashboard_port.unwrap_or(8080)));
    let known_hosts = remember_hosts(&cert_dir, domains, &discovered);

    // A replaced mkcert CA invalidates every existing leaf even though its SANs
    // still match, so force one full regeneration when the CA changes.
    let current_ca = ca_fingerprint();
    let ca_marker = cert_dir.join("ca.fingerprint");
    let previous_ca = fs::read_to_string(&ca_marker)
        .ok()
        .map(|s| s.trim().to_string());
    let ca_changed = current_ca.is_some() && previous_ca.as_deref() != current_ca.as_deref();

    // Names covered by the certificates: every wildcard domain, the dashboard
    // hostname, and localhost. Skip anything that is not a plain hostname: the
    // name becomes a file name under cert_dir and an mkcert argument, so a
    // stray `/` or `..` in config must not escape that directory.
    let mut names: Vec<String> = Vec::new();
    for name in domains
        .iter()
        .map(String::as_str)
        .chain(cfg.hostname.as_deref())
        .filter(|name| is_plain_hostname(name))
    {
        push_unique(&mut names, name);
    }
    push_unique(&mut names, "localhost");
    push_unique(&mut names, "127.0.0.1");

    for name in &names {
        let sans = desired_sans(name, domains, &known_hosts);
        ensure_named_cert(name, &sans, &cert_dir, &dynamic_dir, ca_changed, messages)?;
    }
    if ca_changed && let Some(fingerprint) = &current_ca {
        let _ = fs::write(&ca_marker, fingerprint);
    }
    verify_ca_trust(messages);

    Ok((cert_dir, dynamic_dir))
}

/// Returns a container name for the router image. Keeps the internal name
/// stable (and name-prefixed) regardless of the image tag so idempotency
/// checks and cleanup are predictable.
fn router_container_name(image: &str) -> String {
    // Normalize the image's repo part to a sane container name, e.g.
    // `traefik:v3` -> `fog-router-traefik`.
    let repo = image
        .split(':')
        .next()
        .unwrap_or("router")
        .rsplit('/')
        .next()
        .unwrap_or("router");
    format!("fog-router-{repo}")
}

/// Human-readable dashboard URL for the configured router.
fn dashboard_url(cfg: &RouterConfig) -> String {
    match &cfg.hostname {
        Some(h) => format!("http://{h}:{}", cfg.dashboard_port.unwrap_or(8080)),
        None => format!("http://127.0.0.1:{}", cfg.dashboard_port.unwrap_or(8080)),
    }
}

fn command_exists(name: &str) -> bool {
    crate::process::command_exists(name)
}

fn network_exists(name: &str) -> FogResult<bool> {
    let mut cmd = Command::new("docker");
    cmd.args(["network", "inspect", name]);
    match run_with_timeout(cmd, DOCKER_CMD_TIMEOUT) {
        Some(out) => Ok(out.status.success()),
        None => Err(FogError::new(docker_timeout_msg(
            "network inspect",
            DOCKER_CMD_TIMEOUT,
        ))),
    }
}

fn create_network(name: &str) -> FogResult<()> {
    let mut cmd = Command::new("docker");
    cmd.args(["network", "create", name]);
    let out = run_with_timeout(cmd, DOCKER_CMD_TIMEOUT)
        .ok_or_else(|| FogError::new(docker_timeout_msg("network create", DOCKER_CMD_TIMEOUT)))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(FogError::new(format!(
            "docker network create exited with {}",
            out.status
        )))
    }
}

/// Returns `true` if a container with `name` exists (running or not).
///
/// A docker timeout reports "does not exist"; callers that need to distinguish
/// a hung Docker from a missing container check [`network_exists`] first.
fn container_exists(name: &str) -> bool {
    let mut cmd = Command::new("docker");
    cmd.args([
        "ps",
        "-a",
        "--filter",
        &format!("name=^{name}$"),
        "--format",
        "{{.Names}}",
    ]);
    match run_with_timeout(cmd, DOCKER_CMD_TIMEOUT) {
        Some(out) => String::from_utf8_lossy(&out.stdout).contains(name),
        None => false,
    }
}

/// Returns `true` if the running router container was started with TLS enabled
/// (i.e. it has the `websecure` entrypoint). Used to detect config drift.
fn container_tls_enabled(name: &str) -> bool {
    let mut cmd = Command::new("docker");
    cmd.args(["inspect", "--format", "{{.Args}}", name]);
    match run_with_timeout(cmd, DOCKER_CMD_TIMEOUT) {
        Some(out) => String::from_utf8_lossy(&out.stdout).contains("websecure"),
        None => false,
    }
}

fn remove_container(name: &str) -> FogResult<()> {
    let mut cmd = Command::new("docker");
    cmd.args(["rm", "-f", name]);
    let out = run_with_timeout(cmd, DOCKER_CMD_TIMEOUT)
        .ok_or_else(|| FogError::new(docker_timeout_msg("rm", DOCKER_CMD_TIMEOUT)))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(FogError::new(format!(
            "docker rm exited with {}",
            out.status
        )))
    }
}

/// Decides whether the router container must be (re)created.
///
/// The router is host-global and shared across every project, so TLS is
/// "sticky": a project whose config does not enable TLS must never tear down a
/// running TLS router (that would break HTTPS for everyone else). We only
/// recreate when:
///   - no container exists, or
///   - TLS was requested but the running router lacks it (an upgrade).
fn should_recreate(running: bool, running_tls: bool, tls_requested: bool) -> bool {
    !running || (tls_requested && !running_tls)
}

/// Starts the router container. When TLS is enabled, mounts the cert dir
/// (`/certs`) and the file-provider dynamic dir (`/etc/fog/dynamic`) read-only,
/// adds a `websecure` entrypoint on `:443`, and publishes `443:443`.
fn start_router(cfg: &RouterConfig, container: &str, tls: Option<(&Path, &Path)>) -> FogResult<()> {
    let dashboard_port = cfg.dashboard_port.unwrap_or(8080).to_string();
    // Docker CLI flags come before the image; everything after the image name
    // is Traefik's own command line, so the provider/entrypoint flags must be
    // appended after `image` (docker treats pre-image `--providers.*` as
    // unknown CLI flags).
    let dynamic_dir = default_dynamic_dir(cfg);
    let _ = fs::create_dir_all(&dynamic_dir);
    let mut cmd = Command::new("docker");
    cmd.args([
        "run",
        "-d",
        "--name",
        container,
        "--restart",
        "unless-stopped",
    ])
    .arg("--network")
    .arg(&cfg.shared_network)
    .args(["-p", "80:80", "-p", &format!("{dashboard_port}:8080")])
    // Allow the container to reach the host's embedded index server.
    .args(["--add-host", "host.docker.internal:host-gateway"])
    // The file provider is always mounted so the service-index catch-all
    // router (and, when enabled, the TLS certificates) are loaded and watched.
    .args([
        "-v",
        &format!("{}:/etc/fog/dynamic:ro", dynamic_dir.display()),
    ]);
    if let Some((cert_dir, _)) = tls {
        cmd.args([
            "-p",
            "443:443",
            "-v",
            &format!("{}:/certs:ro", cert_dir.display()),
        ]);
    }
    cmd.args(["-v", "/var/run/docker.sock:/var/run/docker.sock:ro"])
        .arg(&cfg.image)
        // --- Traefik arguments (container command) ---. Traefik auto-discovers
        // containers on its network via the docker provider; only
        // label-opted-in services are exposed.
        .args(["--providers.docker=true"])
        .args(["--providers.docker.exposedByDefault=false"])
        .args(["--providers.docker.network"])
        .arg(&cfg.shared_network)
        .args(["--entrypoints.web.address=:80"])
        // Serve the file-provider dynamic config (index router + TLS certs).
        .args(["--providers.file.directory=/etc/fog/dynamic"])
        .args(["--providers.file.watch=true"]);
    if let Some((_, dynamic_dir)) = tls {
        let _ = dynamic_dir;
        // TLS termination on :443.
        cmd.args(["--entrypoints.websecure.address=:443"]);
    }
    // The dashboard is served by `--api.insecure` on Traefik's default
    // `traefik` entrypoint, which listens on :8080 inside the container.
    cmd.args(["--api.dashboard=true"])
        .arg("--api.insecure")
        .arg("--log.level=ERROR");
    let out = run_with_timeout(cmd, DOCKER_RUN_TIMEOUT)
        .ok_or_else(|| FogError::new(docker_timeout_msg("run", DOCKER_RUN_TIMEOUT)))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(FogError::new(format!(
            "docker run exited with {}",
            out.status
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RouterConfig {
        RouterConfig {
            image: "traefik:v3".to_string(),
            hostname: Some("router.red-fox".to_string()),
            dashboard_port: Some(8080),
            shared_network: "fog-router".to_string(),
            index_port: Some(18080),
            tls: RouterConfig::default().tls,
        }
    }

    #[test]
    fn test_router_container_name_strips_tag_and_registry() {
        assert_eq!(router_container_name("traefik:v3"), "fog-router-traefik");
        assert_eq!(
            router_container_name("docker.io/library/traefik:v3.0"),
            "fog-router-traefik"
        );
    }

    #[test]
    fn test_dashboard_url_hostname() {
        assert_eq!(dashboard_url(&cfg()), "http://router.red-fox:8080");
    }

    #[test]
    fn test_dashboard_url_fallback() {
        let c = RouterConfig {
            hostname: None,
            ..cfg()
        };
        assert_eq!(dashboard_url(&c), "http://127.0.0.1:8080");
    }

    #[test]
    #[ignore = "calls the real router setup and can recreate the host Traefik container"]
    fn test_ensure_on_machine_without_docker_warns_without_panic() {
        // If docker is missing this warns and returns; if present it still
        // returns without panicking. Either way ensure() must not crash.
        let _msgs = ensure(&cfg(), &[]);
    }

    #[test]
    fn test_should_recreate_no_container() {
        assert!(
            should_recreate(false, false, false),
            "no container -> create"
        );
        assert!(
            should_recreate(false, false, true),
            "no container -> create"
        );
    }

    #[test]
    fn test_should_recreate_upgrades_plain_router_when_tls_requested() {
        assert!(
            should_recreate(true, false, true),
            "running plain router + tls requested -> upgrade"
        );
    }

    #[test]
    fn test_should_recreate_never_downgrades_tls_router() {
        assert!(
            !should_recreate(true, true, false),
            "running tls router + tls not requested -> keep (no downgrade)"
        );
        assert!(
            !should_recreate(true, true, true),
            "running tls router + tls requested -> keep"
        );
    }

    #[test]
    fn test_should_recreate_keeps_plain_router_when_tls_not_requested() {
        assert!(
            !should_recreate(true, false, false),
            "running plain router + no tls -> keep"
        );
    }

    #[test]
    #[ignore = "calls the real router setup and writes host mkcert certificates"]
    fn test_ensure_tls_without_mkcert_warns_without_panic() {
        // ensure_tls must return Err (or generate certs if mkcert is present)
        // without panicking.
        let mut c = cfg();
        c.tls.enabled = true;
        let _ = ensure(&c, &["red-fox".to_string()]);
    }

    fn native_cfg(cert_dir: &std::path::Path) -> crate::config::Config {
        crate::config::Config {
            schema: None,
            scripts: std::collections::HashMap::new(),
            ports: None,
            native_routes: None,
            max_scrollback: None,
            sidebar: None,
            theme: None,
            dnsmasq: None,
            router: Some(RouterConfig {
                tls: crate::config::RouterTlsConfig {
                    cert_dir: cert_dir.to_string_lossy().into_owned(),
                    ..Default::default()
                },
                ..RouterConfig::default()
            }),
            index: None,
            project: None,
        }
    }

    fn native_routes() -> Vec<crate::config::NativeRouteConfig> {
        vec![crate::config::NativeRouteConfig {
            host: "${branch}.red-fox".to_string(),
            service: "frontend".to_string(),
            port: "${ports.frontend}".to_string(),
            path_prefix: None,
            endpoint: None,
        }]
    }

    fn ports() -> crate::ports::PortMap {
        let mut m = crate::ports::PortMap::new();
        m.insert("frontend".to_string(), 53123);
        m
    }

    fn scope() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/fog-router-test-project")
    }

    #[test]
    fn test_native_routes_quiet_unless_verbose() {
        let base = std::env::temp_dir().join(format!("fog-native-quiet-{}", std::process::id()));
        let cfg = native_cfg(&base);
        let quiet = ensure_native_routes(
            &native_routes(),
            &ports(),
            Some("main"),
            &scope(),
            &cfg,
            false,
        );
        assert!(quiet.is_empty(), "expected no info lines: {quiet:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_native_routes_verbose_format() {
        let base = std::env::temp_dir().join(format!("fog-native-loud-{}", std::process::id()));
        let cfg = native_cfg(&base);
        let loud = ensure_native_routes(
            &native_routes(),
            &ports(),
            Some("main"),
            &scope(),
            &cfg,
            true,
        );
        assert_eq!(
            loud,
            vec![format!(
                "{}native route frontend -> host.docker.internal:53123 (Host: main.red-fox)",
                crate::log::INFO_PREFIX
            )]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_endpoint_route_uses_ep_prefix_and_cleans_up() {
        let base = std::env::temp_dir().join(format!("fog-ep-route-{}", std::process::id()));
        let cfg = native_cfg(&base);
        let mut routes = native_routes();
        routes[0].host = "${branch}.acme".to_string();
        routes[0].endpoint = Some("web".to_string());
        let msgs = ensure_native_routes(&routes, &ports(), Some("main"), &scope(), &cfg, true);
        assert!(msgs[0].contains("endpoint route frontend.web"), "{msgs:?}");
        let dir = default_dynamic_dir_for_config(&cfg);
        let files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            files.iter().any(|f| f.starts_with("ep-")),
            "expected an ep- route file, got {files:?}"
        );
        cleanup_native_routes(Some("main"), &scope(), &cfg);
        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(remaining.is_empty(), "cleanup left {remaining:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_host_rule_hosts_extracts_only_host_arguments() {
        assert_eq!(
            host_rule_hosts("Host(`main.red-fox`) && PathPrefix(`/v1.0`)"),
            vec!["main.red-fox".to_string()]
        );
        assert_eq!(
            host_rule_hosts("Host(`a.gems`) || Host(`b.gems`)"),
            vec!["a.gems".to_string(), "b.gems".to_string()]
        );
        assert_eq!(
            host_rule_hosts("Host(`a.gems`, `b.gems`) && PathPrefix(`/`)"),
            vec!["a.gems".to_string(), "b.gems".to_string()],
            "every argument of one Host() call is a route"
        );
        assert!(
            host_rule_hosts("HostRegexp(`{host:.+}`)").is_empty(),
            "HostRegexp must not be treated as a Host rule"
        );
    }

    #[test]
    fn test_sanitize_name_is_a_valid_toml_bare_key() {
        assert_eq!(sanitize_name("Prüfung ✓"), "Pr_fung__");
        let scope = route_scope(std::path::Path::new("/tmp/prüfung"));
        assert!(scope.starts_with("pr_fung-"), "{scope}");
        assert!(
            scope
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "route scope must be ASCII so it is a valid TOML bare key: {scope}"
        );
    }

    #[test]
    fn test_desired_sans_cover_nested_service_subdomains() {
        let domains = vec!["shutterbooth".to_string()];
        let hosts = vec![
            "fix.shutterbooth".to_string(),
            "fix.admin.shutterbooth".to_string(),
            "fix.api.shutterbooth".to_string(),
            "fix.api.other.gems".to_string(),
        ];
        let sans = desired_sans("shutterbooth", &domains, &hosts);
        for expected in [
            "shutterbooth",
            "fix.shutterbooth",
            "*.admin.shutterbooth",
            "*.api.shutterbooth",
        ] {
            assert!(
                sans.contains(&expected.to_string()),
                "missing {expected} in {sans:?}"
            );
        }
        assert!(
            !sans.contains(&"*.shutterbooth".to_string()),
            "single-label domains must not rely on an invalid wildcard: {sans:?}"
        );
    }

    #[test]
    fn test_desired_sans_non_domain_names_stay_single_wildcard() {
        let hosts = vec!["fix.admin.builds".to_string()];
        let sans = desired_sans("router.builds", &["builds".to_string()], &hosts);
        assert_eq!(sans, vec!["*.router.builds", "router.builds"]);
    }

    #[test]
    fn test_route_scope_is_stable_and_distinct_per_checkout() {
        let a = route_scope(std::path::Path::new("/tmp/alpha"));
        assert_eq!(a, route_scope(std::path::Path::new("/tmp/alpha")));
        assert!(a.starts_with("alpha-"), "{a}");
        assert_ne!(a, route_scope(std::path::Path::new("/tmp/beta")));
        assert_ne!(
            a,
            route_scope(std::path::Path::new("/tmp/alpha-worktree")),
            "sibling worktrees must not share a scope"
        );
    }

    #[test]
    fn test_remember_hosts_persists_and_seeds_from_sans_sidecars() {
        let base = std::env::temp_dir().join(format!("fog-hosts-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // A pre-registry certificate sidecar: exact hosts are remembered,
        // wildcards are recomputed from them instead.
        std::fs::write(
            base.join("red-fox.sans"),
            "red-fox\nmain.red-fox\n*.api.red-fox\n",
        )
        .unwrap();

        let domains = vec!["red-fox".to_string()];
        let hosts = remember_hosts(
            &base,
            &domains,
            &[
                "feat.red-fox".to_string(),
                "evil.gems".to_string(),
                "*.other.red-fox".to_string(),
            ],
        );
        for expected in ["red-fox", "main.red-fox", "feat.red-fox"] {
            assert!(hosts.contains(&expected.to_string()), "missing {expected}");
        }
        assert!(!hosts.contains(&"evil.gems".to_string()));
        assert!(!hosts.contains(&"*.other.red-fox".to_string()));

        // A second run keeps the union instead of shrinking it.
        let again = remember_hosts(&base, &domains, &[]);
        assert_eq!(hosts, again);
        let on_disk = std::fs::read_to_string(base.join("known-hosts.txt")).unwrap();
        assert!(on_disk.contains("main.red-fox"));
        assert!(on_disk.contains("feat.red-fox"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_plain_hostname_rejects_paths_and_wildcards() {
        assert!(is_plain_hostname("api.red-fox"));
        assert!(is_plain_hostname("127.0.0.1"));
        assert!(!is_plain_hostname("../certs"));
        assert!(!is_plain_hostname("a/b"));
        assert!(!is_plain_hostname("*.red-fox"));
        assert!(!is_plain_hostname(""));
    }

    #[test]
    fn test_hosts_from_traefik_routers_reads_only_docker_rules() {
        let body = r#"[
            {"provider":"docker","rule":"Host(`api.main.red-fox`) && PathPrefix(`/`)","entryPoints":["websecure"]},
            {"provider":"file","rule":"Host(`file.red-fox`)"},
            {"provider":"docker","rule":"HostRegexp(`{host:.+}`)"}
        ]"#;
        assert_eq!(
            hosts_from_traefik_routers(body),
            vec!["api.main.red-fox".to_string()]
        );
        assert!(hosts_from_traefik_routers("not json").is_empty());
    }

    #[test]
    fn test_decode_chunked_body() {
        assert_eq!(
            decode_chunked(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
            Some("hello world".to_string())
        );
        assert_eq!(decode_chunked(b"zz\r\n"), None);
    }

    #[test]
    fn test_cleanup_leaves_other_projects_and_branches_alone() {
        let base = std::env::temp_dir().join(format!("fog-cleanup-none-{}", std::process::id()));
        let cfg = native_cfg(&base);
        let dir = default_dynamic_dir_for_config(&cfg);
        std::fs::create_dir_all(&dir).unwrap();

        let scope_a = base.join("project-a");
        let scope_b = base.join("project-b");
        let slug_a = route_scope(&scope_a);
        let slug_b = route_scope(&scope_b);
        let own = format!("native-{slug_a}-default-web-retail");
        let other_branch = format!("native-{slug_a}-main-web-retail");
        let other_project = format!("native-{slug_b}-default-web-retail");
        for name in [
            own.as_str(),
            other_branch.as_str(),
            other_project.as_str(),
            "native-default-web-retail",
            "deepvision",
        ] {
            std::fs::write(dir.join(format!("{name}.toml")), "").unwrap();
        }

        cleanup_native_routes(None, &scope_a, &cfg);

        let mut remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();
        let mut expected = vec![
            "deepvision.toml".to_string(),
            format!("{other_branch}.toml"),
            format!("{other_project}.toml"),
            "native-default-web-retail.toml".to_string(),
        ];
        expected.sort();
        assert_eq!(
            remaining, expected,
            "cleanup must remove only this checkout's default-branch routes"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_cleanup_without_branch_removes_the_default_slug_route() {
        let base = std::env::temp_dir().join(format!("fog-cleanup-default-{}", std::process::id()));
        let cfg = native_cfg(&base);
        let mut routes = native_routes();
        routes[0].host = "retail.red-fox".to_string();

        // No branch: the route is still published, under the `default` slug.
        let msgs = ensure_native_routes(&routes, &ports(), None, &scope(), &cfg, true);
        assert!(
            msgs.iter().all(|m| !m.starts_with('⚠')),
            "expected no warnings: {msgs:?}"
        );
        let dir = default_dynamic_dir_for_config(&cfg);
        let file = dir.join(format!(
            "native-{}-default-frontend-retail_red-fox.toml",
            route_scope(&scope())
        ));
        assert!(file.is_file(), "expected {} to exist", file.display());

        cleanup_native_routes(None, &scope(), &cfg);
        assert!(!file.exists(), "cleanup must remove the default-slug route");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_cleanup_does_not_delete_a_branch_that_prefixes_another() {
        let base = std::env::temp_dir().join(format!("fog-cleanup-prefix-{}", std::process::id()));
        let cfg = native_cfg(&base);
        let dir = default_dynamic_dir_for_config(&cfg);
        std::fs::create_dir_all(&dir).unwrap();
        let slug = route_scope(&scope());
        let main_2 = dir.join(format!("native-{slug}-main-2-frontend-retail_red-fox.toml"));
        std::fs::write(&main_2, format!("{}\n", route_owner(&slug, "main-2"))).unwrap();
        // Files from before the owner marker still fall back to the prefix.
        let legacy_main = dir.join(format!("native-{slug}-main-frontend-retail_red-fox.toml"));
        std::fs::write(&legacy_main, "").unwrap();

        cleanup_native_routes(Some("main"), &scope(), &cfg);
        assert!(
            main_2.is_file(),
            "cleanup for `main` must not delete `main-2` routes"
        );
        assert!(!legacy_main.exists(), "the legacy same-branch file is ours");

        cleanup_native_routes(Some("main-2"), &scope(), &cfg);
        assert!(
            !main_2.exists(),
            "cleanup for `main-2` removes its own route"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_rewriting_a_route_removes_its_legacy_branch_file() {
        let base = std::env::temp_dir().join(format!("fog-migrate-{}", std::process::id()));
        let cfg = native_cfg(&base);
        let dir = default_dynamic_dir_for_config(&cfg);
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("native-main-frontend-main_red-fox.toml");
        std::fs::write(&legacy, "stale").unwrap();

        let _ = ensure_native_routes(
            &native_routes(),
            &ports(),
            Some("main"),
            &scope(),
            &cfg,
            false,
        );
        assert!(!legacy.exists(), "the pre-scope file must be migrated");
        let _ = std::fs::remove_dir_all(&base);
    }
}
