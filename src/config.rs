use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ConfigEntry {
    pub name: Option<String>,
    pub path: String,
    pub cmd: String,
}

#[derive(Debug, Deserialize)]
pub struct ProxyRoute {
    pub path: String,
    pub upstream: String,
    pub ws: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ProxyConfig {
    pub port: u16,
    pub routes: Vec<ProxyRoute>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub service: Option<Vec<ConfigEntry>>,
    pub proxy: Option<ProxyConfig>,
}
