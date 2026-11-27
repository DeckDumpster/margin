//! The one TOML file `margin serve` reads. Every key is required; a missing
//! key is refused by name and never defaulted.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub base_url: String,
    pub database: PathBuf,
    pub agents_file: PathBuf,
    pub otlp_endpoint: String,
    /// Which of the deploy's servers this is (`production`, `scratch`); labels its metrics.
    pub instance: String,
    /// The most lines of whole documents one page of the reader shows.
    pub page_lines: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    Unparseable(String),
    Missing(&'static str),
    NotAString(&'static str),
    NotAPositiveInteger(&'static str),
    BadListen(String),
    UnspecifiedListen(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Unparseable(e) => write!(f, "config is not valid TOML: {e}"),
            ConfigError::Missing(k) => write!(f, "config key `{k}` is missing"),
            ConfigError::NotAString(k) => write!(f, "config key `{k}` must be a string"),
            ConfigError::NotAPositiveInteger(k) => {
                write!(f, "config key `{k}` must be a positive integer")
            }
            ConfigError::BadListen(v) => write!(f, "config key `listen` is not host:port: {v}"),
            ConfigError::UnspecifiedListen(v) => write!(
                f,
                "config key `listen` is an unspecified address ({v}); there is no sign-in, so listen on the tailnet address only"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ConfigError::Unparseable(e.to_string()))?;
        let get = |key: &'static str| -> Result<String, ConfigError> {
            match table.get(key) {
                None => Err(ConfigError::Missing(key)),
                Some(toml::Value::String(s)) => Ok(s.clone()),
                Some(_) => Err(ConfigError::NotAString(key)),
            }
        };
        let positive = |key: &'static str| -> Result<usize, ConfigError> {
            match table.get(key) {
                None => Err(ConfigError::Missing(key)),
                Some(v) => v
                    .as_integer()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n > 0)
                    .ok_or(ConfigError::NotAPositiveInteger(key)),
            }
        };
        let listen_text = get("listen")?;
        let base_url = get("base_url")?;
        let database = get("database")?;
        let agents_file = get("agents_file")?;
        let otlp_endpoint = get("otlp_endpoint")?;
        let instance = get("instance")?;
        let page_lines = positive("page_lines")?;
        let listen: SocketAddr = listen_text
            .parse()
            .map_err(|_| ConfigError::BadListen(listen_text.clone()))?;
        if listen.ip().is_unspecified() {
            return Err(ConfigError::UnspecifiedListen(listen_text));
        }
        Ok(Config {
            listen,
            base_url: base_url.trim_end_matches('/').to_string(),
            database: database.into(),
            agents_file: agents_file.into(),
            otlp_endpoint,
            instance,
            page_lines,
        })
    }

    pub fn load(path: &std::path::Path) -> Result<Config, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Ok(Config::parse(&text)?)
    }
}
