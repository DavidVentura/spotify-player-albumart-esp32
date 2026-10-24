use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::spotify::{ClientCredentials, RefreshToken};

#[derive(Debug)]
pub struct Config {
    pub credentials: ClientCredentials,
    pub refresh_token: RefreshToken,
    pub device_listen: SocketAddr,
    pub web_listen: SocketAddr,
    pub presets_dir: PathBuf,
}

#[derive(Debug)]
pub enum ConfigError {
    Missing(&'static str),
    InvalidAddress {
        variable: &'static str,
        value: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Missing(variable) => write!(f, "{variable} is not set"),
            ConfigError::InvalidAddress { variable, value } => {
                write!(f, "{variable}={value} is not an ip:port address")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Config {
            credentials: ClientCredentials {
                id: required("SPOTIFY_CLIENT_ID")?,
                secret: required("SPOTIFY_CLIENT_SECRET")?,
            },
            refresh_token: RefreshToken::new(required("SPOTIFY_REFRESH_TOKEN")?),
            device_listen: address("DEVICE_LISTEN", "0.0.0.0:7878")?,
            web_listen: address("WEB_LISTEN", "0.0.0.0:8080")?,
            presets_dir: env::var_os("PRESETS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/presets"))),
        })
    }
}

fn required(variable: &'static str) -> Result<String, ConfigError> {
    env::var(variable).map_err(|_| ConfigError::Missing(variable))
}

fn address(variable: &'static str, default: &str) -> Result<SocketAddr, ConfigError> {
    let value = env::var(variable).unwrap_or_else(|_| default.to_string());
    value
        .parse()
        .map_err(|_| ConfigError::InvalidAddress { variable, value })
}
