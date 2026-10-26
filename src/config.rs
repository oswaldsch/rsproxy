use log::error;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    process::exit,
};

use crate::helpers;

pub enum InvalidConfig {
    DuplicateHost(String, String),
    InvalidAuthority(helpers::InvalidAuthority),
}

impl From<helpers::InvalidAuthority> for InvalidConfig {
    fn from(err: helpers::InvalidAuthority) -> InvalidConfig {
        InvalidConfig::InvalidAuthority(err)
    }
}

impl Display for InvalidConfig {
    fn fmt(&self, formatter: &mut Formatter) -> Result<(), std::fmt::Error> {
        match self {
            InvalidConfig::DuplicateHost(host, original_host) => {
                write!(
                    formatter,
                    "duplicate host entry '{}' normalized from '{original_host}'",
                    host
                )
            }
            InvalidConfig::InvalidAuthority(err) => {
                write!(formatter, "invalid authority entry: {}", err)
            }
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub forward: String,
    pub port: u16,
    #[serde(default = "default_external_flag")]
    pub external: bool,
    pub tls: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_http_listen_port")]
    pub listen_http: u16,
    #[serde(default = "default_https_listen_port")]
    pub listen_https: u16,
    pub hosts: HashMap<String, Entry>,
}

impl Config {
    pub fn load(path: &str, to_stderr: bool) -> Self {
        let read_result = std::fs::read_to_string(path);
        if let Err(err) = read_result {
            if to_stderr {
                eprintln!("could not read config at {}: {}", path, err)
            } else {
                error!("could not read config at {}: {}", path, err);
            }
            exit(1)
        }
        let content = read_result.unwrap();
        let parse_result: Result<Config, toml::de::Error> = toml::from_str(&content.to_string());
        if let Err(err) = parse_result {
            if to_stderr {
                eprintln!("could not parse config at {}:\n{}", path, err);
            } else {
                error!("could not parse config at {}:\n{}", path, err);
            }
            exit(1)
        }
        let mut parse_result = parse_result.unwrap();
        if let Err(err) = parse_result.clean_up() {
            if to_stderr {
                eprintln!("config at {} is not valid: {}", path, err)
            } else {
                error!("config at {} is not valid: {}", path, err)
            }
            exit(1)
        };
        parse_result
    }

    fn clean_up(&mut self) -> Result<(), InvalidConfig> {
        let mut cleaned_hosts: HashMap<String, Entry> = HashMap::new();
        for (host, entry) in &self.hosts {
            let canon_host = helpers::canonical_host(&host, true)?;
            let mut cleaned_entry = entry.clone();
            cleaned_entry.forward = helpers::canonical_host(&entry.forward, true)?;
            let insert_result = cleaned_hosts.insert(canon_host.clone(), cleaned_entry);
            if !insert_result.is_none() {
                return Err(InvalidConfig::DuplicateHost(canon_host, host.to_string()));
            }
        }
        self.hosts = cleaned_hosts;
        Ok(())
    }
}

fn default_external_flag() -> bool {
    false
}

fn default_http_listen_port() -> u16 {
    8080
}

fn default_https_listen_port() -> u16 {
    8443
}
