use std::collections::HashMap;

use serde::Deserialize;
use toml;

#[derive(Deserialize)]
pub struct Entry {
    pub forward: String,
}

#[derive(Deserialize)]
pub struct Config {
    pub listen: u16,
    pub hosts: HashMap<String, Entry>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content.to_string())?;
        Ok(config)
    }
}
