use serde::Deserialize;

/// A single service entry in the config file.
#[derive(Debug, Deserialize)]
pub struct ConfigEntry {
    /// Optional display name for the service tab.
    pub name: Option<String>,
    /// Path to the service's working directory.
    pub path: String,
    /// Shell command to start the service.
    pub cmd: String,
}

/// A route definition for the reverse proxy.
#[derive(Debug, Deserialize)]
pub struct ProxyRoute {
    /// The incoming path prefix to match against.
    pub path: String,
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
    /// The list of route definitions.
    pub routes: Vec<ProxyRoute>,
}

/// Top-level application configuration loaded from `fog.json`.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Optional list of service entries to manage.
    pub service: Option<Vec<ConfigEntry>>,
    /// Optional reverse proxy configuration.
    pub proxy: Option<ProxyConfig>,
}
