use serde::de::{self, Deserializer};
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
pub struct ModelConfig {
    pub name: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
}

fn deserialize_model<'de, D>(deserializer: D) -> Result<Option<ModelConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_yml::Value> = Option::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(serde_yml::Value::String(s)) => Ok(Some(ModelConfig {
            name: Some(s),
            ..Default::default()
        })),
        Some(other) => {
            let cfg: ModelConfig = serde_yml::from_value(other).map_err(de::Error::custom)?;
            Ok(Some(cfg))
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub api_base: Option<String>,
    pub api_key_env: Option<String>,
    #[serde(deserialize_with = "deserialize_model", default)]
    pub model: Option<ModelConfig>,
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
