//! Safe config file manipulation via proper YAML parsing.
//!
//! Reads, modifies, and writes `config.yaml` using `serde_yml::Value` so we
//! never corrupt the file with string hacking.

use crate::paths::config_dir;
use serde_yml::Value;
use std::path::{Path, PathBuf};

fn default_path() -> PathBuf {
    config_dir().join("config.yaml")
}

/// A provider entry to insert into the config.
pub struct NewProvider {
    pub name: String,
    pub provider_type: String,
    pub api_base: String,
    pub api_key_env: Option<String>,
    pub models: Vec<String>,
}

impl NewProvider {
    fn to_yaml(&self) -> serde_yml::Mapping {
        let mut entry = serde_yml::Mapping::new();
        entry.insert(val("name"), Value::String(self.name.clone()));
        entry.insert(val("type"), Value::String(self.provider_type.clone()));
        entry.insert(val("api_base"), Value::String(self.api_base.clone()));
        if let Some(ref key_env) = self.api_key_env {
            if !key_env.is_empty() {
                entry.insert(val("api_key_env"), Value::String(key_env.clone()));
            }
        }
        if !self.models.is_empty() {
            let models: Vec<Value> = self
                .models
                .iter()
                .map(|m| Value::String(m.clone()))
                .collect();
            entry.insert(val("models"), Value::Sequence(models));
        }
        entry
    }
}

fn val(s: &str) -> Value {
    Value::String(s.into())
}

fn read_config(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) if !contents.trim().is_empty() => {
            serde_yml::from_str(&contents).map_err(|e| format!("failed to parse config: {e}"))
        }
        _ => Ok(Value::Mapping(serde_yml::Mapping::new())),
    }
}

fn write_config(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if path.exists() {
        let backup = path.with_extension("yaml.old");
        let _ = std::fs::copy(path, &backup);
    }
    let yaml = serde_yml::to_string(value).map_err(|e| format!("failed to serialize: {e}"))?;
    std::fs::write(path, yaml).map_err(|e| e.to_string())
}

/// Add a provider to the config file. If the file doesn't exist, it is created.
/// If a provider with the same name already exists, it is replaced.
pub fn add_provider(provider: &NewProvider) -> Result<(), String> {
    add_provider_to(provider, &default_path())
}

pub fn add_provider_to(provider: &NewProvider, path: &Path) -> Result<(), String> {
    let mut root = read_config(path)?;
    let map = root
        .as_mapping_mut()
        .ok_or("config root is not a mapping")?;

    let providers_key = val("providers");
    if !map.contains_key(&providers_key) {
        map.insert(providers_key.clone(), Value::Sequence(Vec::new()));
    }
    let providers = map
        .get_mut(&providers_key)
        .and_then(|v| v.as_sequence_mut())
        .ok_or("providers is not a list")?;

    providers.retain(|p| {
        p.as_mapping()
            .and_then(|m| m.get(val("name")))
            .and_then(|v| v.as_str())
            != Some(&provider.name)
    });

    providers.push(Value::Mapping(provider.to_yaml()));
    write_config(path, &root)
}

/// Write a fresh config with a single provider (for first-time setup).
pub fn write_initial_config(path: &Path, provider: &NewProvider) -> Result<(), String> {
    let mut root = serde_yml::Mapping::new();

    root.insert(
        val("providers"),
        Value::Sequence(vec![Value::Mapping(provider.to_yaml())]),
    );

    if let Some(first_model) = provider.models.first() {
        let mut defaults = serde_yml::Mapping::new();
        defaults.insert(
            val("model"),
            Value::String(format!("{}/{first_model}", provider.name)),
        );
        root.insert(val("defaults"), Value::Mapping(defaults));
    }

    write_config(path, &Value::Mapping(root))
}
