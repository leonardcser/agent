use serde::Deserialize;

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

impl ModelConfig {
    pub fn tool_calling(&self) -> bool {
        self.tool_calling.unwrap_or(true)
    }
}
