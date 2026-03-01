use serde::de::{self, Deserializer};
use serde::Deserialize;
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

fn deserialize_models<'de, D>(deserializer: D) -> Result<Vec<ModelConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let values: Vec<serde_yml::Value> = Vec::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|v| match v {
            serde_yml::Value::String(s) => Ok(ModelConfig {
                name: Some(s),
                ..Default::default()
            }),
            other => serde_yml::from_value(other).map_err(de::Error::custom),
        })
        .collect()
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub provider_type: Option<String>,
    pub api_base: Option<String>,
    pub api_key_env: Option<String>,
    #[serde(deserialize_with = "deserialize_models", default)]
    pub models: Vec<ModelConfig>,
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
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    pub default_model: Option<String>,
    pub settings: SettingsConfig,
}

/// A resolved model entry combining provider connection info with model config.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// Display key: "provider_name/model_name"
    pub key: String,
    pub provider_name: String,
    pub model_name: String,
    pub api_base: String,
    pub api_key_env: String,
    pub config: ModelConfig,
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

    /// Flatten providers + models into a list of resolved model entries.
    pub fn resolve_models(&self) -> Vec<ResolvedModel> {
        let mut out = Vec::new();
        for provider in &self.providers {
            let provider_name = provider.name.clone().unwrap_or_default();
            let api_base = provider.api_base.clone().unwrap_or_default();
            let api_key_env = provider.api_key_env.clone().unwrap_or_default();
            for model in &provider.models {
                let model_name = model.name.clone().unwrap_or_default();
                if model_name.is_empty() {
                    continue;
                }
                let key = if provider_name.is_empty() {
                    model_name.clone()
                } else {
                    format!("{}/{}", provider_name, model_name)
                };
                out.push(ResolvedModel {
                    key,
                    provider_name: provider_name.clone(),
                    model_name,
                    api_base: api_base.clone(),
                    api_key_env: api_key_env.clone(),
                    config: model.clone(),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_models_from_config() {
        let yaml = r#"
providers:
  - name: zai
    type: openai-compatible
    api_base: https://api.z.ai/api/coding/paas/v4
    api_key_env: Z_AI_API_KEY
    models:
      - glm-4.7
  - name: box
    type: openai-compatible
    api_base: https://llm.box.home.arpa
    api_key_env: BOX_API_KEY
    models:
      - Qwen3.5-122B-A10B-Q4_0
      - Qwen3.5-27B-Q8_0
      - gpt-oss-120b-Q8_0
      - gpt-oss-20b-Q8_0
"#;
        let cfg: Config = serde_yml::from_str(yaml).unwrap();
        let resolved = cfg.resolve_models();

        assert_eq!(resolved.len(), 5);
        assert_eq!(resolved[0].key, "zai/glm-4.7");
        assert_eq!(resolved[0].api_base, "https://api.z.ai/api/coding/paas/v4");
        assert_eq!(resolved[1].key, "box/Qwen3.5-122B-A10B-Q4_0");
        assert_eq!(resolved[1].api_base, "https://llm.box.home.arpa");
    }
}
