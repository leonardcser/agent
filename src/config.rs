use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

const APP_NAME: &str = "agent";

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join(APP_NAME)
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("state"))
        .join(APP_NAME)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub api_base: Option<String>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SettingsConfig {
    pub vim_mode: Option<bool>,
    pub auto_compact: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub providers: HashMap<String, ProviderConfig>,
    pub default_provider: Option<String>,
    pub settings: SettingsConfig,
}

impl Config {
    pub fn load() -> Self {
        let path = config_dir().join("config.yaml");
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_yml::from_str(&contents) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("warning: failed to parse {}: {}", path.display(), e);
                Self::default()
            }
        }
    }
}
