use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum HealthCheckKind {
    Tcp,
    Http,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HealthCheckConfig {
    pub kind: HealthCheckKind,
    pub target: String,
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

/// A named script: a full set of services and optional proxy configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct ScriptConfig {
    /// Optional list of service entries to manage.
    pub service: Option<Vec<ConfigEntry>>,
    /// Optional reverse proxy configuration.
    pub proxy: Option<ProxyConfig>,
}

/// Top-level application configuration loaded from `fog.json`.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// Named scripts, each defining its own services and proxy.
    pub scripts: HashMap<String, ScriptConfig>,
    /// Maximum number of scrollback lines to retain per terminal (default: 2000).
    pub max_scrollback: Option<usize>,
    /// Optional sidebar width constraints.
    pub sidebar: Option<SidebarConfig>,
    /// Optional color theme overrides.
    pub theme: Option<ThemeConfig>,
}
