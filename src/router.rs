use crate::config::RouterConfig;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    // 1. Create the shared network (idempotent).
    if !network_exists(&cfg.shared_network) {
        match create_network(&cfg.shared_network) {
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
        match ensure_tls(cfg, domains, &mut messages) {
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

/// Generates (idempotently) the mkcert wildcard certificates and the Traefik
/// file-provider dynamic config for every domain, returning the host paths of
/// the cert dir and the dynamic dir mounted into the container.
fn ensure_tls(
    cfg: &RouterConfig,
    domains: &[String],
    messages: &mut Vec<String>,
) -> Result<(PathBuf, PathBuf), String> {
    if !command_exists("mkcert") {
        return Err(
            "mkcert is not installed; install it with `brew install mkcert` and run \
             `mkcert -install` once"
                .to_string(),
        );
    }

    let cert_dir = PathBuf::from(&cfg.tls.cert_dir);
    fs::create_dir_all(&cert_dir)
        .map_err(|e| format!("could not create {}: {}", cert_dir.display(), e))?;
    // Traefik file-provider dynamic config, mounted read-only into the container.
    let dynamic_dir = cert_dir.join("dynamic");
    fs::create_dir_all(&dynamic_dir)
        .map_err(|e| format!("could not create {}: {}", dynamic_dir.display(), e))?;

    // Names covered by the certificates: every wildcard domain, the dashboard
    // hostname, and localhost.
    let mut names: Vec<String> = domains.to_vec();
    if let Some(h) = &cfg.hostname
        && !names.contains(h)
    {
        names.push(h.clone());
    }
    names.push("localhost".to_string());
    names.push("127.0.0.1".to_string());
    names.dedup();

    for domain in &names {
        // One certificate per name: `*.name` + `name` (mkcert generates a SAN
        // cert covering all the given hostnames).
        let wildcard = format!("*.{domain}");
        let hostnames = [domain.clone(), wildcard];
        let cert_file = cert_dir.join(format!("{domain}.pem"));
        let key_file = cert_dir.join(format!("{domain}-key.pem"));
        if cert_file.exists() && key_file.exists() {
            continue;
        }
        messages.push(format!(
            "  + generating wildcard certificate for *.{domain}"
        ));
        let status = Command::new("mkcert")
            .current_dir(&cert_dir)
            .arg("-cert-file")
            .arg(&cert_file)
            .arg("-key-file")
            .arg(&key_file)
            .args(&hostnames)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(_) | Err(_) => {
                return Err(format!("mkcert failed for {domain}"));
            }
        }

        // File-provider dynamic config so Traefik serves this cert over TLS.
        let dyn_file = dynamic_dir.join(format!("{domain}.toml"));
        let content = format!(
            "[[tls.certificates]]\n  certFile = \"/certs/{domain}.pem\"\n  \
             keyFile = \"/certs/{domain}-key.pem\"\n"
        );
        fs::write(&dyn_file, content)
            .map_err(|e| format!("could not write {}: {}", dyn_file.display(), e))?;
    }

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
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn network_exists(name: &str) -> bool {
    Command::new("docker")
        .args(["network", "inspect", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn create_network(name: &str) -> Result<(), String> {
    let status = Command::new("docker")
        .args(["network", "create", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("docker: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("docker network create exited with {}", status))
    }
}

/// Returns `true` if a container with `name` exists (running or not).
fn container_exists(name: &str) -> bool {
    let out = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name=^{name}$"),
            "--format",
            "{{.Names}}",
        ])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(name),
        Err(_) => false,
    }
}

/// Returns `true` if the running router container was started with TLS enabled
/// (i.e. it has the `websecure` entrypoint). Used to detect config drift.
fn container_tls_enabled(name: &str) -> bool {
    let out = Command::new("docker")
        .args(["inspect", "--format", "{{.Args}}", name])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("websecure"),
        Err(_) => false,
    }
}

fn remove_container(name: &str) -> Result<(), String> {
    let status = Command::new("docker")
        .args(["rm", "-f", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("docker rm: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("docker rm exited with {}", status))
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
fn start_router(
    cfg: &RouterConfig,
    container: &str,
    tls: Option<(&Path, &Path)>,
) -> Result<(), String> {
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
    let status = cmd.status().map_err(|e| format!("docker run: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("docker run exited with {}", status))
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
    fn test_ensure_tls_without_mkcert_warns_without_panic() {
        // ensure_tls must return Err (or generate certs if mkcert is present)
        // without panicking.
        let mut c = cfg();
        c.tls.enabled = true;
        let _ = ensure(&c, &["red-fox".to_string()]);
    }
}
