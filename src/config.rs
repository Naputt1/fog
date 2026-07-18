use serde::Deserialize;

use crate::service::Service;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub service: Vec<Service>,
}
