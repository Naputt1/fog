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
    ///
    /// Only honored when the script has `concurrent: false` (single-instance
    /// mode, which is the worktree-switch reclaim path).
    #[serde(default)]
    pub reuse: bool,
    /// Share this service between multiple concurrent instances of the script.
    /// Only honored when the script has `concurrent: true`: when another
    /// instance of the same project+script is already running and this
    /// service's `health_check` passes, the `cmd` is not re-run (the service
    /// is borrowed instead of duplicated), and it is torn down only when the
    /// last instance of the project+script exits.
    #[serde(default)]
    pub share: bool,
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

fn default_concurrent() -> bool {
    true
}

fn default_true() -> bool {
    true
}

/// Standalone service-directory index server config.
///
/// Controls whether starting this project also serves the index (service
/// directory + web UI + JSON API) on `port`. Default `enabled: true` so every
/// `fog <script>` brings the index up unless explicitly disabled per-project.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct IndexConfig {
    /// Whether to serve the index server when this project starts.
    /// Default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Port the index server listens on (default 18080). Falls back to
    /// `router.index_port` for backward-compat when not set here.
    pub port: Option<u16>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: None,
        }
    }
}

/// A named script: a full set of services and optional proxy configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct ScriptConfig {
    /// Optional list of service entries to manage.
    pub service: Option<Vec<ConfigEntry>>,
    /// Optional reverse proxy configuration.
    pub proxy: Option<ProxyConfig>,
    /// Allow multiple concurrent instances of this script in the same
    /// project+branch. When true (default), running the script again starts
    /// alongside existing instances instead of killing them; services flagged
    /// `share: true` are shared between those instances. When false, only one
    /// instance runs and a new run kills the previous one first (with
    /// `reuse: true` services handed over).
    #[serde(default = "default_concurrent")]
    pub concurrent: bool,
}

fn default_router_image() -> String {
    "traefik:v3".to_string()
}

fn default_router_network() -> String {
    "fog-router".to_string()
}

fn default_router_cert_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.config/fog/certs")
}

/// Optional HTTPS (TLS) settings for the central router.
///
/// When enabled, fog generates local CA wildcard certificates (via mkcert) for
/// the configured `dnsmasq` domains plus the router hostname and `localhost`,
/// and Traefik terminates TLS on `:443` (a `websecure` entrypoint) using them.
/// HTTP on `:80` keeps working alongside HTTPS.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RouterTlsConfig {
    /// Enable HTTPS termination on the central router (default: false).
    pub enabled: bool,
    /// Directory where wildcard certificates are stored (default:
    /// `~/.config/fog/certs`). Generated per-domain, idempotently.
    pub cert_dir: String,
}

impl Default for RouterTlsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cert_dir: default_router_cert_dir(),
        }
    }
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
#[serde(default)]
pub struct RouterConfig {
    /// Traefik image to run (default: `traefik:v3`).
    pub image: String,
    /// Hostname for the Traefik dashboard (e.g. `router.red-fox`). The
    /// wildcard-DNS mapping must already cover it (via `dnsmasq.domains`).
    pub hostname: Option<String>,
    /// Host port on which the Traefik dashboard listens (default: 8080).
    pub dashboard_port: Option<u16>,
    /// Name of the external Docker network shared with app services.
    pub shared_network: String,
    /// Port the embedded service-directory index server listens on
    /// (default: 18080). Requests to the router with no matching app host are
    /// served the generated `index.html` from here.
    pub index_port: Option<u16>,
    /// Optional HTTPS (TLS) termination settings.
    pub tls: RouterTlsConfig,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            image: default_router_image(),
            hostname: None,
            dashboard_port: Some(8080),
            shared_network: default_router_network(),
            index_port: Some(18080),
            tls: RouterTlsConfig::default(),
        }
    }
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
    /// Standalone index server (service directory + web UI). Controls whether
    /// starting this project serves the index. Default `enabled: true`.
    #[serde(default)]
    pub index: Option<IndexConfig>,
}

impl Config {
    /// Whether this project should serve the index server when a script starts.
    /// Default `true` when `index` is absent or `enabled` is not set.
    pub fn should_serve_index(&self) -> bool {
        self.index.as_ref().map(|i| i.enabled).unwrap_or(true)
    }

    /// Effective index port for this project, preferring `index.port` then
    /// `router.index_port` then `18080`.
    pub fn index_port(&self) -> u16 {
        self.index
            .as_ref()
            .and_then(|i| i.port)
            .or_else(|| self.router.as_ref().and_then(|r| r.index_port))
            .unwrap_or(18080)
    }

    /// Shared network name for the index server, from `router.shared_network`
    /// or the default `fog-router`.
    pub fn index_network(&self) -> String {
        self.router
            .as_ref()
            .map(|r| r.shared_network.clone())
            .unwrap_or_else(default_router_network)
    }

    /// Effective should-serve for global + project: both must be true.
    /// Global config is `~/.config/fog/fog.json` (the fog config with themes).
    pub fn effective_should_serve_index(&self) -> bool {
        // Project-level check
        if !self.should_serve_index() {
            return false;
        }
        // Global fog config check (same schema, top-level `index` alongside `theme`)
        if let Some(global) = load_global_config()
            && !global.should_serve_index()
        {
            return false;
        }
        true
    }

    /// Effective index port, preferring project `index.port`, then project
    /// `router.index_port`, then global `index.port`/`router.index_port`, then 18080.
    pub fn effective_index_port(&self) -> u16 {
        if let Some(p) = self.index.as_ref().and_then(|i| i.port) {
            return p;
        }
        if let Some(p) = self.router.as_ref().and_then(|r| r.index_port) {
            return p;
        }
        if let Some(global) = load_global_config() {
            if let Some(p) = global.index.as_ref().and_then(|i| i.port) {
                return p;
            }
            if let Some(p) = global.router.as_ref().and_then(|r| r.index_port) {
                return p;
            }
        }
        18080
    }
}

/// Loads the global fog config (`~/.config/fog/fog.json`, the one with `theme`
/// etc) if it exists. Used for `index.enabled` global override.
pub fn load_global_config() -> Option<Config> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return None;
    }
    let path = std::path::PathBuf::from(format!("{home}/.config/fog/fog.json"));
    if !path.is_file() {
        return None;
    }
    load(&path).ok()
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
    fn test_load_concurrent_defaults_to_true() {
        let path =
            std::env::temp_dir().join(format!("fog-config-concurrent-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"scripts":{"dev":{}}}"#).unwrap();
        let config = load(&path).unwrap();
        assert!(
            config.scripts["dev"].concurrent,
            "scripts default to concurrent (multiple runs allowed)"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_concurrent_opt_out() {
        let path = std::env::temp_dir().join(format!(
            "fog-config-concurrent-false-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, r#"{"scripts":{"dev":{"concurrent":false}}}"#).unwrap();
        let config = load(&path).unwrap();
        assert!(!config.scripts["dev"].concurrent);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_share_defaults_to_false() {
        let path =
            std::env::temp_dir().join(format!("fog-config-share-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true"}]}}}"#,
        )
        .unwrap();
        let config = load(&path).unwrap();
        let entries = config.scripts["dev"].service.as_ref().unwrap();
        assert!(!entries[0].share, "share defaults to false");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_share_enabled() {
        let path =
            std::env::temp_dir().join(format!("fog-config-share-true-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"scripts":{"dev":{"service":[{"path":".","cmd":"true","share":true}]}}}"#,
        )
        .unwrap();
        let config = load(&path).unwrap();
        let entries = config.scripts["dev"].service.as_ref().unwrap();
        assert!(entries[0].share);
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
