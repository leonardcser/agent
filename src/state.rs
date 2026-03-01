use crate::config;
use crate::input::Mode;
use crate::provider::ReasoningEffort;
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
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
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

    pub fn vim_enabled(&self) -> bool {
        self.vim_enabled
    }
}

/// Read-modify-write helpers. Each loads state.json fresh, updates one field,
/// and saves back — preventing parallel instances from clobbering each other.
pub fn set_mode(mode: Mode) {
    let mut s = State::load();
    s.mode = mode.as_str().to_string();
    s.save();
}

pub fn set_vim_enabled(enabled: bool) {
    let mut s = State::load();
    s.vim_enabled = enabled;
    s.save();
}

pub fn set_selected_model(key: String) {
    let mut s = State::load();
    s.selected_model = Some(key);
    s.save();
}

pub fn set_reasoning_effort(effort: ReasoningEffort) {
    let mut s = State::load();
    s.reasoning_effort = effort;
    s.save();
}
