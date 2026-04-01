use crate::config;
use protocol::{Mode, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Toggle settings persisted across sessions. Each field is `Option<bool>`:
/// `Some(v)` = user explicitly toggled it, `None` = use config/default.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PersistedSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vim_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_tps: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_tokens: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_cost: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_prediction: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_slug: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restrict_to_workspace: Option<bool>,
}

impl PersistedSettings {
    /// Resolve against config defaults: state wins, then config, then hardcoded default.
    pub fn resolve(&self, cfg: &crate::config::SettingsConfig) -> ResolvedSettings {
        ResolvedSettings {
            vim: self.vim_mode.or(cfg.vim_mode).unwrap_or(false),
            auto_compact: self.auto_compact.or(cfg.auto_compact).unwrap_or(false),
            show_tps: self.show_tps.or(cfg.show_tps).unwrap_or(true),
            show_tokens: self.show_tokens.or(cfg.show_tokens).unwrap_or(true),
            show_cost: self.show_cost.or(cfg.show_cost).unwrap_or(true),
            show_prediction: self
                .input_prediction
                .or(cfg.input_prediction)
                .unwrap_or(true),
            show_slug: self.task_slug.or(cfg.task_slug).unwrap_or(true),
            show_thinking: self.show_thinking.or(cfg.show_thinking).unwrap_or(true),
            restrict_to_workspace: self
                .restrict_to_workspace
                .or(cfg.restrict_to_workspace)
                .unwrap_or(true),
        }
    }
}

/// Fully resolved boolean settings (no more Options).
#[derive(Debug, Clone)]
pub struct ResolvedSettings {
    pub vim: bool,
    pub auto_compact: bool,
    pub show_tps: bool,
    pub show_tokens: bool,
    pub show_cost: bool,
    pub show_prediction: bool,
    pub show_slug: bool,
    pub show_thinking: bool,
    pub restrict_to_workspace: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub mode: String,
    // Legacy field — migrated into `settings.vim_mode` on load.
    #[serde(default)]
    pub vim_enabled: bool,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    #[serde(default)]
    pub accent_color: Option<u8>,
    // Legacy field — migrated into `settings.show_thinking` on load.
    #[serde(default)]
    pub show_thinking: Option<bool>,
    #[serde(default)]
    pub settings: PersistedSettings,
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
        let mut s: Self = serde_json::from_str(&contents).unwrap_or_default();
        // Migrate legacy fields into the settings struct.
        let mut migrated = false;
        if s.vim_enabled && s.settings.vim_mode.is_none() {
            s.settings.vim_mode = Some(true);
            s.vim_enabled = false;
            migrated = true;
        }
        if let Some(v) = s.show_thinking.take() {
            if s.settings.show_thinking.is_none() {
                s.settings.show_thinking = Some(v);
                migrated = true;
            }
        }
        if migrated {
            s.save();
        }
        s
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
}

// ── Read-modify-write helpers ─────────────────────────────────────────────

pub fn set_mode(mode: Mode) {
    let mut s = State::load();
    s.mode = mode.as_str().to_string();
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

pub fn set_accent(value: u8) {
    let mut s = State::load();
    s.accent_color = Some(value);
    s.save();
}

/// Persist all toggle settings from the resolved values.
pub fn save_settings(resolved: &ResolvedSettings) {
    let mut s = State::load();
    s.settings = PersistedSettings {
        vim_mode: Some(resolved.vim),
        auto_compact: Some(resolved.auto_compact),
        show_tps: Some(resolved.show_tps),
        show_tokens: Some(resolved.show_tokens),
        show_cost: Some(resolved.show_cost),
        input_prediction: Some(resolved.show_prediction),
        task_slug: Some(resolved.show_slug),
        show_thinking: Some(resolved.show_thinking),
        restrict_to_workspace: Some(resolved.restrict_to_workspace),
    };
    s.save();
}
