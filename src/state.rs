use crate::config;
use crate::input::Mode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub vim_enabled: bool,
    #[serde(default)]
    pub selected_model: Option<String>,
}

fn state_path() -> PathBuf {
    config::state_dir().join("state.json")
}

impl State {
    pub fn load() -> Self {
        let path = state_path();
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&contents).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    pub fn mode(&self) -> Mode {
        Mode::parse(&self.mode).unwrap_or(Mode::Normal)
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode.as_str().to_string();
        self.save();
    }

    pub fn vim_enabled(&self) -> bool {
        self.vim_enabled
    }

    pub fn set_vim_enabled(&mut self, enabled: bool) {
        self.vim_enabled = enabled;
        self.save();
    }

    pub fn set_selected_model(&mut self, key: String) {
        self.selected_model = Some(key);
        self.save();
    }
}
