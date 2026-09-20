use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use codexec_judge::{IsolateConfig, JudgeConfig};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub sandbox: IsolateConfig,
    #[serde(default)]
    pub judge: JudgeConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub database: PathBuf,
    pub problems_dir: PathBuf,
    pub languages_file: PathBuf,
    /// A job whose worker stops heart-beating for this long is re-queued.
    pub lease_seconds: u64,
    /// After this many failed attempts a submission gets the verdict IE.
    pub max_attempts: u32,
    pub max_source_bytes: usize,
    pub run_queue_capacity: usize,
    pub run_ttl_seconds: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".parse().unwrap(),
            database: "data/codexec.db".into(),
            problems_dir: "problems".into(),
            languages_file: "languages.toml".into(),
            lease_seconds: 60,
            max_attempts: 3,
            max_source_bytes: 64 * 1024,
            run_queue_capacity: 256,
            run_ttl_seconds: 600,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }
}
