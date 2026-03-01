pub struct SettingsMenu {
    pub vim_enabled: bool,
    pub auto_compact: bool,
    pub selected: usize, // 0 = vim mode, 1 = auto compact
}

pub struct ModelMenu {
    /// (key, model_name, provider_name) for each available model.
    pub models: Vec<(String, String, String)>,
    pub selected: usize,
}
