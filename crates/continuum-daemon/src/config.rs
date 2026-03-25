//! Daemon configuration — loaded from `~/.config/continuum/config.toml`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Directory for runtime data (sockets, sessions).
    pub data_dir: PathBuf,
    /// Path to the ingestion socket (shell hooks connect here).
    pub ingest_socket: PathBuf,
    /// Path to the query socket (UI clients connect here).
    pub query_socket: PathBuf,
    /// Maximum session age in days before cleanup.
    pub max_session_age_days: u64,
    /// Which side the rail appears on ("left" or "right").
    pub rail_side: String,
    /// Rail width in terminal columns.
    pub rail_width: u32,
    /// Hover trigger zone width in pixels.
    pub hover_zone_px: u32,
    /// Slide animation duration in milliseconds.
    pub animation_ms: u32,
    /// Enable debug logging.
    pub debug: bool,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = default_data_dir();
        Self {
            ingest_socket: data_dir.join("ingest.sock"),
            query_socket: data_dir.join("query.sock"),
            data_dir,
            max_session_age_days: 7,
            rail_side: "right".into(),
            rail_width: 16,
            hover_zone_px: 8,
            animation_ms: 200,
            debug: false,
        }
    }
}

impl Config {
    /// Load config from the default path, falling back to defaults.
    pub fn load() -> Self {
        let config_path = default_config_path();
        Self::load_from(&config_path)
    }

    /// Load config from a specific path.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str::<Config>(&contents) {
                Ok(config) => {
                    info!(path = %path.display(), "loaded config");
                    config
                }
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "invalid config, using defaults"
                    );
                    Config::default()
                }
            },
            Err(_) => {
                info!("no config file found, using defaults");
                Config::default()
            }
        }
    }
}

fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".continuum")
}

fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".config")
        })
        .join("continuum")
        .join("config.toml")
}
