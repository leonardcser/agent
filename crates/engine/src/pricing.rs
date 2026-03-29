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
}

const ZERO: ModelPricing = ModelPricing {
    input: 0.0,
    output: 0.0,
    cache_read: 0.0,
    cache_write: 0.0,
};

// ── Remote catalog (models.dev) ──────────────────────────────────────────

const MODELS_API_URL: &str = "https://models.dev/api.json";
const CACHE_KEY: &str = "models_dev_pricing";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60); // 1 hour

/// Global catalog populated by `fetch_catalog()`.
static CATALOG: OnceLock<HashMap<String, ModelPricing>> = OnceLock::new();

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

async fn load_or_fetch(client: &reqwest::Client) -> HashMap<String, ModelPricing> {
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

/// Parse the models.dev JSON into a flat model_id → pricing map.
fn parse_catalog(json: &str) -> Option<HashMap<String, ModelPricing>> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = root.as_object()?;
    let mut map = HashMap::new();
    for (_provider, provider_val) in obj {
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
                model_id.clone(),
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

/// Look up pricing for a model from the remote catalog.
/// Returns `None` for unknown/local models (cost = 0).
pub fn lookup(model: &str) -> Option<ModelPricing> {
    CATALOG.get()?.get(model).copied()
}

/// Build a `ModelPricing` from config overrides, falling back to the
/// built-in table, then to zero for unknown models.
pub fn resolve(model: &str, config: &crate::config::ModelConfig) -> ModelPricing {
    let builtin = lookup(model).unwrap_or(ZERO);
    ModelPricing {
        input: config.input_cost.unwrap_or(builtin.input),
        output: config.output_cost.unwrap_or(builtin.output),
        cache_read: config.cache_read_cost.unwrap_or(builtin.cache_read),
        cache_write: config.cache_write_cost.unwrap_or(builtin.cache_write),
    }
}
