use serde::Deserialize;

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

#[derive(Debug, Deserialize)]
pub struct ThemeConfig {
    pub proxy: Option<String>,
    pub terminal: Option<String>,
    pub stopped: Option<String>,
    pub highlight: Option<String>,
    pub status_200: Option<String>,
    pub status_300: Option<String>,
    pub status_400: Option<String>,
    pub status_500: Option<String>,
}

/// A single service entry in the config file.
#[derive(Debug, Deserialize)]
pub struct ConfigEntry {
    /// Optional display name for the service tab.
    pub name: Option<String>,
    /// Path to the service's working directory.
    pub path: String,
    /// Shell command to start the service.
    pub cmd: String,
    /// Optional health check configuration.
    pub health_check: Option<HealthCheckConfig>,
}

/// A route definition for the reverse proxy.
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
pub struct SidebarConfig {
    /// Minimum sidebar width in columns (default: 12).
    pub min_width: Option<u16>,
    /// Maximum sidebar width in columns (default: 30).
    pub max_width: Option<u16>,
}

/// Top-level application configuration loaded from `fog.json`.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Optional list of service entries to manage.
    pub service: Option<Vec<ConfigEntry>>,
    /// Optional reverse proxy configuration.
    pub proxy: Option<ProxyConfig>,
    /// Maximum number of scrollback lines to retain per terminal (default: 2000).
    pub max_scrollback: Option<usize>,
    /// Optional sidebar width constraints.
    pub sidebar: Option<SidebarConfig>,
    /// Optional color theme overrides.
    pub theme: Option<ThemeConfig>,
}
