use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ConfigEntry {
    pub path: String,
    pub cmd: String,
}

#[derive(Debug, Deserialize)]
pub struct ProxyRoute {
    pub path: String,
    pub upstream: String,
}

#[derive(Debug, Deserialize)]
pub struct ProxyConfig {
    pub port: u16,
    pub routes: Vec<ProxyRoute>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub service: Vec<ConfigEntry>,
    pub proxy: Option<ProxyConfig>,
}
