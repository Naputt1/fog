use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ConfigEntry {
    pub path: String,
    pub cmd: String,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub service: Vec<ConfigEntry>,
}
