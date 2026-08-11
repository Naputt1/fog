use crate::config::RouterConfig;
use std::process::Command;

/// Ensures the central reverse-proxy (Traefik) router exists on the host.
///
/// This is the router analog of [`crate::dnsmasq::ensure`]: it is a
/// host-global resource managed by fog once, independent of any project or
/// branch. App compose files opt into routing by attaching a service to
/// `shared_network` and declaring Traefik labels; fog only guarantees the
/// router and that network exist.
///
/// Best-effort: it never fails the run. Errors/warnings are returned as
/// messages for the caller to print.
pub fn ensure(cfg: &RouterConfig) -> Vec<String> {
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

    // 2. Start the router container (idempotent — only starts if not running).
    let container = router_container_name(&cfg.image);
    if !container_running(&container) {
        match start_router(cfg, &container) {
            Ok(()) => messages.push(format!(
                "  + started central router '{}' (Traefik dashboard: {:?})",
                container,
                dashboard_url(cfg)
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

fn container_running(name: &str) -> bool {
    let out = Command::new("docker")
        .args([
            "ps",
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

fn start_router(cfg: &RouterConfig, container: &str) -> Result<(), String> {
    let dashboard_port = cfg.dashboard_port.unwrap_or(8080).to_string();
    // Docker CLI flags come before the image; everything after the image name
    // is Traefik's own command line, so the provider/entrypoint flags must be
    // appended after `image` (docker treats pre-image `--providers.*` as
    // unknown CLI flags).
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
    .args(["-v", "/var/run/docker.sock:/var/run/docker.sock:ro"])
    .arg(&cfg.image)
    // --- Traefik arguments (container command) ---. Traefik auto-discovers
    // containers on its network via the docker provider; only label-opted-in
    // services are exposed.
    .args(["--providers.docker=true"])
    .args(["--providers.docker.exposedByDefault=false"])
    .args(["--providers.docker.network"])
    .arg(&cfg.shared_network)
    .args(["--entrypoints.web.address=:80"])
    // The dashboard is served by `--api.insecure` on Traefik's default
    // `traefik` entrypoint, which listens on :8080 inside the container.
    .args(["--api.dashboard=true"])
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
        let _msgs = ensure(&cfg());
    }
}
