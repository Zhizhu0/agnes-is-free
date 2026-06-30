use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
}

impl Config {
    pub fn default_config() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://apihub.agnes-ai.com/v1".to_string(),
        }
    }

    /// Returns the path to the config file in the app data directory.
    fn config_path() -> PathBuf {
        let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("agnes-is-free");
        std::fs::create_dir_all(&path).ok();
        path.push("config.toml");
        path
    }

    /// Load config from disk. Returns default if file doesn't exist or is invalid.
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(config) = toml::from_str(&contents) {
                    return config;
                }
            }
        }
        Self::default_config()
    }

    /// Save config to disk.
    pub fn save(&self) {
        let path = Self::config_path();
        let contents = toml::to_string_pretty(self).unwrap_or_default();
        std::fs::write(&path, contents).ok();
    }
}
