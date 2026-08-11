use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum HealthCheckKind {
    Tcp,
    Http,
    /// Verifies a container/service from a docker compose file is running and
    /// (when the compose defines a healthcheck) reports "healthy".
    Docker,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HealthCheckConfig {
    pub kind: HealthCheckKind,
    /// For `tcp`/`http`: the address to check (e.g. `localhost:8080`).
    /// For `docker`: the compose service name to check.
    pub target: String,
    /// For `docker`: compose file to use, relative to the service `path`.
    /// Defaults to `docker-compose.yml`. Ignored by `tcp`/`http`.
    pub compose_file: Option<String>,
    pub interval_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
}
#[derive(Debug, Deserialize, Clone)]
pub struct ThemeConfig {
    pub proxy: Option<String>,
    pub terminal: Option<String>,
    pub stopped: Option<String>,
    pub highlight: Option<String>,
    pub status_200: Option<String>,
    pub status_300: Option<String>,
    pub status_400: Option<String>,
    pub status_500: Option<String>,
    pub scrollbar: Option<String>,
}

/// Accepts either a single health check object or an array of health checks.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum HealthCheckSpec {
    Single(HealthCheckConfig),
    Multiple(Vec<HealthCheckConfig>),
}

/// A single service entry in the config file.
#[derive(Debug, Deserialize, Clone)]
pub struct ConfigEntry {
    /// Optional display name for the service tab.
    pub name: Option<String>,
    /// Path to the service's working directory.
    pub path: String,
    /// Shell command to start the service.
    pub cmd: String,
    /// Optional health check configuration (single object or array).
    pub health_check: Option<HealthCheckSpec>,
    /// Names of services this service depends on.
    pub depends_on: Option<Vec<String>>,
    /// Shell command to run when fog shuts down (e.g. "docker compose down").
    pub shutdown_cmd: Option<String>,
    /// When another instance of the same project+script starts, this service's
    /// `shutdown_cmd` is skipped (and its live process handed over) instead of
    /// being torn down, so the resource can be reused across worktrees.
    #[serde(default)]
    pub reuse: bool,
}

/// A route definition for the reverse proxy.
#[derive(Debug, Deserialize, Clone)]
pub struct ProxyRoute {
    /// The incoming path prefix to match against.
    /// Supports `*` wildcards (e.g. `/api/*` matches `/api/foo`).
    pub path: String,
    /// Optional host pattern to match against the `Host` header.
    /// Supports `*` wildcards (e.g. `custom.*` matches `custom.com`).
    /// If omitted, matches any host.
    pub host: Option<String>,
    /// The upstream URL to forward matching requests to.
    pub upstream: String,
    /// Whether this route should use WebSocket proxying.
    pub ws: Option<bool>,
}

/// Reverse proxy configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct ProxyConfig {
    /// The port the reverse proxy listens on.
    pub port: u16,
    /// Optional host address to bind to (default: 0.0.0.0).
    pub host: Option<String>,
    /// The list of route definitions.
    pub routes: Vec<ProxyRoute>,
    /// Optional path to a TLS certificate file (PEM-encoded).
    pub tls_cert: Option<String>,
    /// Optional path to a TLS private key file (PEM-encoded, PKCS8).
    pub tls_key: Option<String>,
    /// Maximum number of log entries to retain (default: 1000).
    pub max_log_entries: Option<usize>,
}

/// Sidebar width configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct SidebarConfig {
    /// Minimum sidebar width in columns (default: 12).
    pub min_width: Option<u16>,
    /// Maximum sidebar width in columns (default: 30).
    pub max_width: Option<u16>,
}

fn default_dnsmasq_address() -> String {
    "127.0.0.1".to_string()
}

fn default_dnsmasq_port() -> u16 {
    53
}

/// Wildcard DNS routing set up through dnsmasq automatically on startup.
///
/// Each domain in `domains` is mapped so that `*.<domain>` resolves to
/// `address` (e.g. `["red-fox"]` with the default address makes
/// `main.red-fox`, `feature-x.red-fox`, ... resolve to `127.0.0.1`).
///
/// The port defaults to 53 (the DNS standard). On macOS the daemon is run as
/// a root LaunchDaemon (via `sudo brew services start`) so it can bind the
/// privileged port; the per-zone `/etc/resolver/<domain>` file then only
/// needs a plain `nameserver` line.
#[derive(Debug, Deserialize, Clone)]
pub struct DnsmasqConfig {
    /// Domains to wildcard-map to `address`.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Address that `*.<domain>` resolves to (default: 127.0.0.1).
    #[serde(default = "default_dnsmasq_address")]
    pub address: String,
    /// Port dnsmasq listens on (default: 53). Run as root on macOS so the
    /// privileged port can be bound.
    #[serde(default = "default_dnsmasq_port")]
    pub port: u16,
}

/// A named script: a full set of services and optional proxy configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct ScriptConfig {
    /// Optional list of service entries to manage.
    pub service: Option<Vec<ConfigEntry>>,
    /// Optional reverse proxy configuration.
    pub proxy: Option<ProxyConfig>,
}

fn default_router_image() -> String {
    "traefik:v3".to_string()
}

fn default_router_network() -> String {
    "fog-router".to_string()
}

/// Host-global reverse proxy (Traefik) setup applied automatically on startup.
///
/// This mirrors [`DnsmasqConfig`]: the router is a shared, host-level resource
/// that must exist exactly once across every project and branch, so fog manages
/// its lifecycle directly rather than letting each app's compose file run its
/// own instance (which would collide on the published port).
///
/// Apps opt into routing by placing Traefik container labels on their services
/// and attaching them to the shared network named by `shared_network`.
#[derive(Debug, Deserialize, Clone)]
pub struct RouterConfig {
    /// Traefik image to run (default: `traefik:v3`).
    #[serde(default = "default_router_image")]
    pub image: String,
    /// Hostname for the Traefik dashboard (e.g. `router.red-fox`). The
    /// wildcard-DNS mapping must already cover it (via `dnsmasq.domains`).
    pub hostname: Option<String>,
    /// Host port on which the Traefik dashboard listens (default: 8080).
    pub dashboard_port: Option<u16>,
    /// Name of the external Docker network shared with app services.
    #[serde(default = "default_router_network")]
    pub shared_network: String,
}

/// Top-level application configuration loaded from `fog.json`.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// Named scripts, each defining its own services and proxy.
    #[serde(default)]
    pub scripts: HashMap<String, ScriptConfig>,
    /// Maximum number of scrollback lines to retain per terminal (default: 2000).
    pub max_scrollback: Option<usize>,
    /// Optional sidebar width constraints.
    pub sidebar: Option<SidebarConfig>,
    /// Optional color theme overrides.
    pub theme: Option<ThemeConfig>,
    /// Optional dnsmasq wildcard-DNS setup applied automatically on startup.
    pub dnsmasq: Option<DnsmasqConfig>,
    /// Optional central reverse-proxy (Traefik) setup applied on startup.
    pub router: Option<RouterConfig>,
}

/// Loads and parses a config file, returning a human-readable error on failure.
pub fn load(path: &Path) -> Result<Config, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read config '{}': {}", path.display(), e))?;
    serde_json::from_str(&contents)
        .map_err(|e| format!("invalid config '{}': {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_missing_file_errors() {
        let path =
            std::env::temp_dir().join(format!("fog-config-missing-{}.json", std::process::id()));
        let err = load(&path).unwrap_err();
        assert!(err.contains("could not read config"), "{err}");
    }

    #[test]
    fn test_load_invalid_json_errors() {
        let path =
            std::env::temp_dir().join(format!("fog-config-invalid-{}.json", std::process::id()));
        std::fs::write(&path, "{ not json").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.contains("invalid config"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_valid() {
        let path =
            std::env::temp_dir().join(format!("fog-config-valid-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"scripts":{"dev":{}}}"#).unwrap();
        let config = load(&path).unwrap();
        assert!(config.scripts.contains_key("dev"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_dnsmasq_config() {
        let path =
            std::env::temp_dir().join(format!("fog-config-dnsmasq-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"scripts":{},"dnsmasq":{"domains":["red-fox","dev"],"address":"127.0.0.1"}}"#,
        )
        .unwrap();
        let config = load(&path).unwrap();
        let d = config.dnsmasq.expect("dnsmasq section present");
        assert_eq!(d.domains, vec!["red-fox", "dev"]);
        assert_eq!(d.address, "127.0.0.1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_dnsmasq_config_defaults() {
        let path = std::env::temp_dir().join(format!(
            "fog-config-dnsmasq-defaults-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, r#"{"scripts":{},"dnsmasq":{"domains":["red-fox"]}}"#).unwrap();
        let config = load(&path).unwrap();
        let d = config.dnsmasq.expect("dnsmasq section present");
        assert_eq!(d.domains, vec!["red-fox"]);
        assert_eq!(d.address, "127.0.0.1", "address defaults to loopback");
        assert_eq!(
            d.port, 53,
            "port defaults to 53 (DNS standard; root daemon on macOS)"
        );
        let _ = std::fs::remove_file(&path);
    }
}
