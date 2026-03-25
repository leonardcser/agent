use serde::de::{self, Deserializer};
use serde::Deserialize;
use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    engine::config_dir()
}

pub fn state_dir() -> PathBuf {
    engine::state_dir()
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
    pub tool_calling: Option<bool>,
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
    pub show_speed: Option<bool>,
    pub input_prediction: Option<bool>,
    pub task_slug: Option<bool>,
    pub restrict_to_workspace: Option<bool>,
}

impl SettingsConfig {
    /// Apply a `key=value` override. Returns an error message for unknown keys or bad values.
    pub fn apply(&mut self, key: &str, value: &str) -> Result<(), String> {
        let b = || match value {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(format!("invalid bool value '{value}' for {key}")),
        };
        match key {
            "vim_mode" => self.vim_mode = b()?,
            "auto_compact" => self.auto_compact = b()?,
            "show_speed" => self.show_speed = b()?,
            "input_prediction" => self.input_prediction = b()?,
            "task_slug" => self.task_slug = b()?,
            "restrict_to_workspace" => self.restrict_to_workspace = b()?,
            _ => return Err(format!("unknown setting '{key}'")),
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub accent: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DefaultsConfig {
    pub model: Option<String>,
    /// Starting mode: normal, plan, apply, yolo.
    pub mode: Option<String>,
    /// Modes available for Shift+Tab cycling. Defaults to all modes.
    pub mode_cycle: Option<Vec<String>>,
    pub reasoning_effort: Option<String>,
    /// Reasoning effort levels available for Ctrl+T cycling.
    pub reasoning_cycle: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    pub defaults: DefaultsConfig,
    pub settings: SettingsConfig,
    pub theme: ThemeConfig,
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
    /// Provider type from config: "openai", "anthropic", or "openai-compatible" (default).
    pub provider_type: String,
    pub config: ModelConfig,
}

impl Config {
    pub fn load() -> Self {
        Self::load_from(&config_dir().join("config.yaml"))
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        let Ok(contents) = std::fs::read_to_string(path) else {
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
                    provider_type: provider
                        .provider_type
                        .clone()
                        .unwrap_or_else(|| "openai-compatible".to_string()),
                    config: model.clone(),
                });
            }
        }
        out
    }

    /// Get the default model key from defaults.model
    pub fn get_default_model(&self) -> Option<&str> {
        self.defaults.model.as_deref()
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
