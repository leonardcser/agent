use serde::de::{self, Deserializer};
use serde::Deserialize;
use std::path::{Path, PathBuf};

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
    /// Cost per 1M input tokens in USD.
    pub input_cost: Option<f64>,
    /// Cost per 1M output tokens in USD.
    pub output_cost: Option<f64>,
    /// Cost per 1M cache-read tokens in USD.
    pub cache_read_cost: Option<f64>,
    /// Cost per 1M cache-write tokens in USD.
    pub cache_write_cost: Option<f64>,
}

impl From<&ModelConfig> for engine::ModelConfig {
    fn from(c: &ModelConfig) -> Self {
        Self {
            name: c.name.clone(),
            temperature: c.temperature,
            top_p: c.top_p,
            top_k: c.top_k,
            min_p: c.min_p,
            repeat_penalty: c.repeat_penalty,
            tool_calling: c.tool_calling,
            input_cost: c.input_cost,
            output_cost: c.output_cost,
            cache_read_cost: c.cache_read_cost,
            cache_write_cost: c.cache_write_cost,
        }
    }
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
    pub show_tps: Option<bool>,
    pub show_tokens: Option<bool>,
    pub show_cost: Option<bool>,
    pub input_prediction: Option<bool>,
    pub task_slug: Option<bool>,
    pub show_thinking: Option<bool>,
    pub restrict_to_workspace: Option<bool>,
    pub redact_secrets: Option<bool>,
    pub multi_agent: Option<bool>,
    /// Override the context window size (tokens). When unset, the engine
    /// fetches it from the provider API at startup.
    pub context_window: Option<u32>,
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
            "show_tps" => self.show_tps = b()?,
            "show_tokens" => self.show_tokens = b()?,
            "show_cost" => self.show_cost = b()?,
            "input_prediction" => self.input_prediction = b()?,
            "task_slug" => self.task_slug = b()?,
            "show_thinking" => self.show_thinking = b()?,
            "restrict_to_workspace" => self.restrict_to_workspace = b()?,
            "redact_secrets" => self.redact_secrets = b()?,
            "multi_agent" => self.multi_agent = b()?,
            _ => return Err(format!("unknown setting '{key}'")),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuxiliaryUseForConfig {
    #[serde(default = "default_true")]
    pub title: bool,
    #[serde(default = "default_true")]
    pub prediction: bool,
    #[serde(default = "default_true")]
    pub compaction: bool,
    #[serde(default = "default_true")]
    pub btw: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AuxiliaryUseForConfig {
    fn default() -> Self {
        Self {
            title: true,
            prediction: true,
            compaction: true,
            btw: true,
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct AuxiliaryConfig {
    pub model: Option<String>,
    pub use_for: AuxiliaryUseForConfig,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// Loaded and parsed from a file.
    Loaded,
    /// File exists but failed to parse (fell back to defaults).
    ParseError,
    /// File was not found (using defaults).
    NotFound,
}

/// Configuration for the skills system.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Extra directories to scan for skills.
    pub paths: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    pub defaults: DefaultsConfig,
    pub settings: SettingsConfig,
    pub auxiliary: AuxiliaryConfig,
    pub theme: ThemeConfig,
    /// MCP server configurations.
    #[serde(default)]
    pub mcp: std::collections::HashMap<String, engine::McpServerConfig>,
    /// Skills configuration.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Path the config was loaded from (not serialized).
    #[serde(skip)]
    pub path: PathBuf,
    /// How the config was resolved.
    #[serde(skip)]
    pub source: Option<ConfigSource>,
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
    /// Provider type from config: "openai", "anthropic", "codex", or "openai-compatible" (default).
    pub provider_type: String,
    pub config: ModelConfig,
}

pub use protocol::AuxiliaryTask;

#[derive(Debug, Clone)]
pub struct AuxiliaryRouting {
    pub model: Option<ResolvedModel>,
    pub use_for: AuxiliaryUseForConfig,
}

impl AuxiliaryRouting {
    pub fn is_enabled_for(&self, task: AuxiliaryTask) -> bool {
        match task {
            AuxiliaryTask::Title => self.use_for.title,
            AuxiliaryTask::Prediction => self.use_for.prediction,
            AuxiliaryTask::Compaction => self.use_for.compaction,
            AuxiliaryTask::Btw => self.use_for.btw,
        }
    }

    pub fn model_for(&self, task: AuxiliaryTask) -> Option<&ResolvedModel> {
        self.is_enabled_for(task)
            .then_some(self.model.as_ref())
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveModelRefError {
    NotFound {
        reference: String,
    },
    Ambiguous {
        reference: String,
        matches: Vec<String>,
    },
}

impl std::fmt::Display for ResolveModelRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { reference } => write!(f, "unknown model or provider: {reference}"),
            Self::Ambiguous { reference, matches } => write!(
                f,
                "ambiguous reference '{reference}' — use provider/model ({})",
                matches.join(", ")
            ),
        }
    }
}

impl std::error::Error for ResolveModelRefError {}

pub fn resolve_model_ref<'a>(
    models: &'a [ResolvedModel],
    reference: &str,
) -> Result<&'a ResolvedModel, ResolveModelRefError> {
    resolve_model_ref_with_provider(models, reference, None)
}

pub fn resolve_model_ref_with_provider<'a>(
    models: &'a [ResolvedModel],
    reference: &str,
    provider: Option<&str>,
) -> Result<&'a ResolvedModel, ResolveModelRefError> {
    if let Some(model) = models
        .iter()
        .find(|m| m.key == reference && provider.is_none_or(|p| m.provider_name == p))
    {
        return Ok(model);
    }

    let mut first_match: Option<&ResolvedModel> = None;
    let mut ambiguous_keys: Vec<String> = Vec::new();
    for model in models
        .iter()
        .filter(|m| m.model_name == reference && provider.is_none_or(|p| m.provider_name == p))
    {
        if let Some(first) = first_match {
            if ambiguous_keys.is_empty() {
                ambiguous_keys.push(first.key.clone());
            }
            ambiguous_keys.push(model.key.clone());
        } else {
            first_match = Some(model);
        }
    }

    if let Some(model) = first_match {
        if ambiguous_keys.is_empty() {
            Ok(model)
        } else {
            Err(ResolveModelRefError::Ambiguous {
                reference: reference.to_string(),
                matches: ambiguous_keys,
            })
        }
    } else {
        Err(ResolveModelRefError::NotFound {
            reference: reference.to_string(),
        })
    }
}

/// Resolve an `auxiliary.model` reference. Falls back to a provider-name
/// lookup when the reference doesn't match any registered model AND the
/// provider uses dynamic model discovery (e.g. `codex`), where statically
/// listing every model under `providers[].models` is impractical.
pub fn resolve_aux_model_ref<'a>(
    models: &'a [ResolvedModel],
    reference: &str,
) -> Result<&'a ResolvedModel, ResolveModelRefError> {
    match resolve_model_ref(models, reference) {
        Ok(model) => Ok(model),
        Err(ResolveModelRefError::NotFound { .. }) => {
            let resolved = resolve_provider_ref(models, reference)?;
            if resolved.provider_type == "codex" || resolved.provider_type == "copilot" {
                Ok(resolved)
            } else {
                Err(ResolveModelRefError::NotFound {
                    reference: reference.to_string(),
                })
            }
        }
        Err(other) => Err(other),
    }
}

pub fn resolve_provider_ref<'a>(
    models: &'a [ResolvedModel],
    provider: &str,
) -> Result<&'a ResolvedModel, ResolveModelRefError> {
    let mut first_match: Option<&ResolvedModel> = None;
    let mut ambiguous_keys: Vec<String> = Vec::new();
    for model in models.iter().filter(|m| m.provider_name == provider) {
        if let Some(first) = first_match {
            if ambiguous_keys.is_empty() {
                ambiguous_keys.push(first.key.clone());
            }
            ambiguous_keys.push(model.key.clone());
        } else {
            first_match = Some(model);
        }
    }

    if let Some(model) = first_match {
        if ambiguous_keys.is_empty() {
            Ok(model)
        } else {
            Err(ResolveModelRefError::Ambiguous {
                reference: provider.to_string(),
                matches: ambiguous_keys,
            })
        }
    } else {
        Err(ResolveModelRefError::NotFound {
            reference: provider.to_string(),
        })
    }
}

impl Config {
    pub fn load() -> Self {
        Self::load_from(&config_dir().join("config.yaml"))
    }

    pub fn load_from(path: &Path) -> Self {
        let path = path.to_path_buf();
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Self {
                path,
                source: Some(ConfigSource::NotFound),
                ..Self::default()
            };
        };
        match serde_yml::from_str(&contents) {
            Ok(cfg) => Self {
                path,
                source: Some(ConfigSource::Loaded),
                ..cfg
            },
            Err(e) => {
                eprintln!("warning: failed to parse {}: {}", path.display(), e);
                Self {
                    path,
                    source: Some(ConfigSource::ParseError),
                    ..Self::default()
                }
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
            let provider_type = provider
                .provider_type
                .clone()
                .unwrap_or_else(|| "openai-compatible".to_string());

            // Codex and Copilot models are fetched dynamically — emit a
            // placeholder so the provider is detected even when no models are
            // listed in config.
            if (provider_type == "codex" || provider_type == "copilot")
                && provider.models.is_empty()
            {
                out.push(ResolvedModel {
                    key: format!("{}/{}", provider_name, provider_type),
                    provider_name: provider_name.clone(),
                    model_name: String::new(),
                    api_base: api_base.clone(),
                    api_key_env: api_key_env.clone(),
                    provider_type: provider_type.clone(),
                    config: ModelConfig::default(),
                });
                continue;
            }

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
                    provider_type: provider_type.clone(),
                    config: model.clone(),
                });
            }
        }
        out
    }

    /// Replace codex placeholders with dynamically fetched model slugs.
    pub fn inject_codex_models(&self, resolved: &mut Vec<ResolvedModel>, slugs: &[String]) {
        let Some(codex_provider) = self
            .providers
            .iter()
            .find(|p| p.provider_type.as_deref() == Some("codex"))
        else {
            return;
        };

        let provider_name = codex_provider.name.clone().unwrap_or_default();
        let api_base = codex_provider.api_base.clone().unwrap_or_default();

        resolved.retain(|m| m.provider_type != "codex");

        for slug in slugs {
            resolved.push(ResolvedModel {
                key: format!("{provider_name}/{slug}"),
                provider_name: provider_name.clone(),
                model_name: slug.clone(),
                api_base: api_base.clone(),
                api_key_env: String::new(),
                provider_type: "codex".to_string(),
                config: ModelConfig {
                    name: Some(slug.clone()),
                    ..ModelConfig::default()
                },
            });
        }
    }

    /// Returns true if the config has a codex provider.
    pub fn has_codex_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.provider_type.as_deref() == Some("codex"))
    }

    /// Replace copilot placeholders with dynamically fetched model IDs.
    pub fn inject_copilot_models(&self, resolved: &mut Vec<ResolvedModel>, ids: &[String]) {
        let Some(copilot_provider) = self
            .providers
            .iter()
            .find(|p| p.provider_type.as_deref() == Some("copilot"))
        else {
            return;
        };

        let provider_name = copilot_provider.name.clone().unwrap_or_default();
        let api_base = copilot_provider.api_base.clone().unwrap_or_default();

        resolved.retain(|m| m.provider_type != "copilot");

        for id in ids {
            resolved.push(ResolvedModel {
                key: format!("{provider_name}/{id}"),
                provider_name: provider_name.clone(),
                model_name: id.clone(),
                api_base: api_base.clone(),
                api_key_env: String::new(),
                provider_type: "copilot".to_string(),
                config: ModelConfig {
                    name: Some(id.clone()),
                    ..ModelConfig::default()
                },
            });
        }
    }

    /// Returns true if the config has a Copilot provider.
    pub fn has_copilot_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.provider_type.as_deref() == Some("copilot"))
    }

    /// Auto-inject OAuth providers (Codex, Copilot) when the user has stored
    /// credentials but no explicit config entry. This eliminates the need for
    /// `smelt auth` to mutate the user's config file.
    pub fn inject_oauth_providers(&mut self) {
        if !self.has_codex_provider()
            && engine::auth::is_logged_in(engine::auth::AuthProvider::Codex)
        {
            self.providers.push(ProviderConfig {
                name: Some("codex".to_string()),
                provider_type: Some("codex".to_string()),
                api_base: Some(engine::provider::codex::CODEX_API_ENDPOINT.to_string()),
                api_key_env: None,
                models: vec![],
            });
        }
        if !self.has_copilot_provider()
            && engine::auth::is_logged_in(engine::auth::AuthProvider::Copilot)
        {
            self.providers.push(ProviderConfig {
                name: Some("copilot".to_string()),
                provider_type: Some("copilot".to_string()),
                api_base: Some(engine::provider::copilot::DEFAULT_COPILOT_API_BASE.to_string()),
                api_key_env: None,
                models: vec![],
            });
        }
    }

    /// Get the default model key from defaults.model
    pub fn get_default_model(&self) -> Option<&str> {
        self.defaults.model.as_deref()
    }

    pub fn resolve_auxiliary_routing(
        &self,
        models: &[ResolvedModel],
    ) -> Result<AuxiliaryRouting, ResolveModelRefError> {
        let model = self
            .auxiliary
            .model
            .as_deref()
            .map(|reference| resolve_aux_model_ref(models, reference).cloned())
            .transpose()?;
        Ok(AuxiliaryRouting {
            model,
            use_for: self.auxiliary.use_for.clone(),
        })
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
