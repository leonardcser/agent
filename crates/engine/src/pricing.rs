use protocol::TokenUsage;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

/// Per-model pricing in USD per 1M tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelPricing {
    /// Calculate the cost in USD for the given token usage.
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
        let input = usage.prompt_tokens.unwrap_or(0) as f64;
        let output = usage.completion_tokens.unwrap_or(0) as f64;
        let cache_read = usage.cache_read_tokens.unwrap_or(0) as f64;
        let cache_write = usage.cache_write_tokens.unwrap_or(0) as f64;
        // Reasoning tokens are billed at the output rate.
        let reasoning = usage.reasoning_tokens.unwrap_or(0) as f64;

        (self.input * input
            + self.output * output
            + self.output * reasoning
            + self.cache_read * cache_read
            + self.cache_write * cache_write)
            / 1_000_000.0
    }

    pub fn is_zero(&self) -> bool {
        self.input == 0.0 && self.output == 0.0
    }
}

const ZERO: ModelPricing = ModelPricing {
    input: 0.0,
    output: 0.0,
    cache_read: 0.0,
    cache_write: 0.0,
};

/// Where the resolved pricing came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingSource {
    /// User-supplied cost overrides in model config.
    Config,
    /// Matched from the models.dev remote catalog.
    Catalog,
    /// No pricing data available (local/unknown model).
    None,
}

impl PricingSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "config override",
            Self::Catalog => "models.dev",
            Self::None => "none",
        }
    }
}

/// Resolved pricing plus its source.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedPricing {
    pub pricing: ModelPricing,
    pub source: PricingSource,
}

// ── Remote catalog (models.dev) ──────────────────────────────────────────

const MODELS_API_URL: &str = "https://models.dev/api.json";
const CACHE_KEY: &str = "models_dev_pricing";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60); // 1 hour

/// Global catalog keyed by (provider, model_id).
static CATALOG: OnceLock<HashMap<(String, String), ModelPricing>> = OnceLock::new();

/// Fetch pricing from models.dev in the background. Call once at startup.
/// Safe to call multiple times — only the first call populates the catalog.
pub fn spawn_catalog_fetch(client: reqwest::Client) {
    if CATALOG.get().is_some() {
        return;
    }
    tokio::spawn(async move {
        let map = load_or_fetch(&client).await;
        let _ = CATALOG.set(map);
    });
}

async fn load_or_fetch(client: &reqwest::Client) -> HashMap<(String, String), ModelPricing> {
    // Try disk cache first.
    if let Some(json) = crate::tools::web_cache::get(CACHE_KEY) {
        if let Some(map) = parse_catalog(&json) {
            return map;
        }
    }
    // Fetch from API.
    let json = match client.get(MODELS_API_URL).send().await {
        Ok(resp) => match resp.text().await {
            Ok(t) => t,
            Err(_) => return HashMap::new(),
        },
        Err(_) => return HashMap::new(),
    };
    let map = parse_catalog(&json).unwrap_or_default();
    if !map.is_empty() {
        crate::tools::web_cache::put_with_ttl(CACHE_KEY, &json, CACHE_TTL);
    }
    map
}

/// Parse the models.dev JSON into a (provider, model_id) → pricing map.
fn parse_catalog(json: &str) -> Option<HashMap<(String, String), ModelPricing>> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = root.as_object()?;
    let mut map = HashMap::new();
    for (provider, provider_val) in obj {
        let models = provider_val.get("models").and_then(|m| m.as_object());
        let models = match models {
            Some(m) => m,
            None => continue,
        };
        for (model_id, model_val) in models {
            let cost = match model_val.get("cost") {
                Some(c) => c,
                None => continue,
            };
            let input = cost["input"].as_f64().unwrap_or(0.0);
            let output = cost["output"].as_f64().unwrap_or(0.0);
            if input == 0.0 && output == 0.0 {
                continue;
            }
            map.insert(
                (provider.clone(), model_id.clone()),
                ModelPricing {
                    input,
                    output,
                    cache_read: cost["cache_read"].as_f64().unwrap_or(0.0),
                    cache_write: cost["cache_write"].as_f64().unwrap_or(0.0),
                },
            );
        }
    }
    Some(map)
}

/// Look up pricing for a (provider, model) pair from the remote catalog.
/// Returns `None` when the provider/model combination isn't found.
fn lookup(provider_type: &str, model: &str) -> Option<ModelPricing> {
    let catalog = CATALOG.get()?;
    let key = catalog_key(provider_type)?;
    catalog.get(&(key.to_string(), model.to_string())).copied()
}

/// Map a provider_type string to the corresponding models.dev provider key.
/// Known first-party providers map directly; "openai-compatible" gets no
/// lookup. Other values are tried verbatim as catalog keys.
fn catalog_key(provider_type: &str) -> Option<&str> {
    match provider_type {
        "openai" | "codex" => Some("openai"),
        "anthropic" => Some("anthropic"),
        "copilot" | "github-copilot" => Some("github-copilot"),
        "openai-compatible" => None,
        other => Some(other),
    }
}

/// Resolve pricing for a model, returning both the rates and where they came from.
pub fn resolve(
    model: &str,
    provider_type: &str,
    config: &crate::config::ModelConfig,
) -> ResolvedPricing {
    let has_config_override = config.input_cost.is_some() || config.output_cost.is_some();

    if has_config_override {
        let catalog = lookup(provider_type, model).unwrap_or(ZERO);
        return ResolvedPricing {
            pricing: ModelPricing {
                input: config.input_cost.unwrap_or(catalog.input),
                output: config.output_cost.unwrap_or(catalog.output),
                cache_read: config.cache_read_cost.unwrap_or(catalog.cache_read),
                cache_write: config.cache_write_cost.unwrap_or(catalog.cache_write),
            },
            source: PricingSource::Config,
        };
    }

    if let Some(catalog) = lookup(provider_type, model) {
        return ResolvedPricing {
            pricing: catalog,
            source: PricingSource::Catalog,
        };
    }

    ResolvedPricing {
        pricing: ZERO,
        source: PricingSource::None,
    }
}
