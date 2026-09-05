use std::path::PathBuf;

pub use protocol::{ModelCatalogMetadata, ModelConfig};

pub fn config_dir() -> PathBuf {
    engine::config_dir()
}

pub fn state_dir() -> PathBuf {
    engine::state_dir()
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProviderConfig {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub api_base: Option<String>,
    pub api_key_env: Option<String>,
    pub models: Vec<ModelConfig>,
}

/// Value type of a settings slot. Drives parsing of `--set` overrides
/// and the Lua `__index`/`__newindex` dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Bool,
    Number,
    String,
}

/// Owned setting value. `set` accepts any of these; the schema decides
/// whether the assignment is type-compatible.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Number(f64),
    String(String),
}

impl SettingValue {
    pub fn kind(&self) -> SettingKind {
        match self {
            SettingValue::Bool(_) => SettingKind::Bool,
            SettingValue::Number(_) => SettingKind::Number,
            SettingValue::String(_) => SettingKind::String,
        }
    }
}

/// Schema entry for one settings key. Authored exclusively via the
/// `settings!` macro below - the macro is the only thing that ever
/// constructs `SettingDecl`, so its shape can evolve without touching
/// any call site.
pub struct SettingDecl {
    pub key: &'static str,
    pub kind: SettingKind,
    /// Concatenated doc lines from the `settings!` declaration. Empty
    /// when the macro entry had no `///` lines above it. Surfaced in
    /// generated config docs and the `customize` skill.
    pub doc: &'static str,
    /// Closed set of accepted values for `String` settings; `None`
    /// means free-form. Ignored for non-`String` kinds.
    pub choices: Option<&'static [&'static str]>,
    /// Runtime work required when the resolved value changes.
    pub effect: SettingEffect,
    /// Project the typed field to a polymorphic `SettingValue`.
    pub read: fn(&ResolvedSettings) -> SettingValue,
    /// Assign from a polymorphic `SettingValue`. Returns `false` on
    /// kind mismatch (the caller has already gated on kind, so this
    /// only fires for schema bugs).
    pub write: fn(&mut ResolvedSettings, &SettingValue) -> bool,
}

/// Runtime effect category for a resolved setting change. The settings macro
/// has no fallback arm, so adding a setting without classifying it fails to
/// compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingEffect {
    Input,
    Render,
    Prediction,
    FileIcons,
    TerminalTitle,
    Permissions,
    AutoReload,
    FutureRequests,
    Compaction,
    AutoContinue,
    WebSearch,
    Upgrade,
}

/// One declaration per setting; the macro emits the `ResolvedSettings`
/// struct, its `Default` impl, and the `SETTINGS` schema table. Adding
/// a setting means appending one line below - nothing else fans out.
///
/// Grammar:
///   key: Kind = default[, choices: [..]];      -- one or more docs above
///
/// `Kind` is one of `Bool` / `Number` / `String`. `choices` is only
/// valid for `String` settings; an unknown choice in a `set` call
/// rejects at the call site rather than at the consumer.
macro_rules! settings {
    (
        $(
            $(#[doc = $doc:literal])*
            $key:ident : $kind:ident = $default:expr
            $(, choices: [$($choice:literal),* $(,)?])?
        );* $(;)?
    ) => {
        /// Fully resolved settings. Lives on `RuntimeState` so runtime
        /// reads hit one authoritative snapshot; persistence is not a
        /// concern of this type - config is `init.lua`, not a JSON
        /// registry.
        #[derive(Debug, Clone, PartialEq)]
        pub struct ResolvedSettings {
            $(
                $(#[doc = $doc])*
                pub $key: settings!(@ty $kind),
            )*
        }

        impl Default for ResolvedSettings {
            fn default() -> Self {
                Self {
                    $($key: settings!(@default $kind, $default),)*
                }
            }
        }

        pub const SETTINGS: &[SettingDecl] = &[
            $(SettingDecl {
                key: stringify!($key),
                kind: SettingKind::$kind,
                doc: concat!($($doc, " "),*),
                choices: settings!(@choices $($($choice),*)?),
                effect: settings!(@effect $key),
                read: |s| settings!(@read $kind, s.$key),
                write: |s, v| settings!(@write $kind, s.$key, v),
            },)*
        ];
    };
    (@ty Bool)   => { bool };
    (@ty Number) => { f64 };
    (@ty String) => { String };
    (@default Bool,   $e:expr) => { $e };
    (@default Number, $e:expr) => { $e };
    (@default String, $e:expr) => { String::from($e) };
    (@choices) => { None };
    (@choices $($choice:literal),+) => { Some(&[$($choice),+]) };
    (@read Bool,   $f:expr) => { SettingValue::Bool($f) };
    (@read Number, $f:expr) => { SettingValue::Number($f) };
    (@read String, $f:expr) => { SettingValue::String($f.clone()) };
    (@write Bool,   $f:expr, $v:expr) => {
        if let SettingValue::Bool(b)   = $v { $f = *b;        true } else { false }
    };
    (@write Number, $f:expr, $v:expr) => {
        if let SettingValue::Number(n) = $v { $f = *n;        true } else { false }
    };
    (@write String, $f:expr, $v:expr) => {
        if let SettingValue::String(t) = $v { $f = t.clone(); true } else { false }
    };
    (@effect vim) => { SettingEffect::Input };
    (@effect system_clipboard) => { SettingEffect::Input };
    (@effect show_tps) => { SettingEffect::Render };
    (@effect show_tokens) => { SettingEffect::Render };
    (@effect show_cost) => { SettingEffect::Render };
    (@effect show_slug) => { SettingEffect::Render };
    (@effect show_tips) => { SettingEffect::Render };
    (@effect show_prediction) => { SettingEffect::Prediction };
    (@effect file_icons) => { SettingEffect::FileIcons };
    (@effect file_icon_colors) => { SettingEffect::FileIcons };
    (@effect terminal_title) => { SettingEffect::TerminalTitle };
    (@effect restrict_to_workspace) => { SettingEffect::Permissions };
    (@effect worktree_root) => { SettingEffect::Permissions };
    (@effect auto_reload) => { SettingEffect::AutoReload };
    (@effect redact_secrets) => { SettingEffect::FutureRequests };
    (@effect fast_mode) => { SettingEffect::FutureRequests };
    (@effect cache_ttl_long) => { SettingEffect::FutureRequests };
    (@effect request_audit) => { SettingEffect::FutureRequests };
    (@effect auto_compact) => { SettingEffect::Compaction };
    (@effect compact_threshold) => { SettingEffect::Compaction };
    (@effect compact_keep_recent_groups) => { SettingEffect::Compaction };
    (@effect auto_continue) => { SettingEffect::AutoContinue };
    (@effect web_search_provider) => { SettingEffect::WebSearch };
    (@effect brave_search_api_key_env) => { SettingEffect::WebSearch };
    (@effect web_fetch_render) => { SettingEffect::FutureRequests };
    (@effect web_fetch_renderer_command) => { SettingEffect::FutureRequests };
    (@effect autoupgrade) => { SettingEffect::Upgrade };
    (@effect autoupgrade_channel) => { SettingEffect::Upgrade };
    (@effect autoupgrade_interval) => { SettingEffect::Upgrade };
}

settings! {
    /// Vi keybindings in the prompt.
    vim:                   Bool   = false;
    /// Sync prompt kills and yanks with the OS clipboard. Disable to keep `C-w`/`C-k`/`C-u`/`C-y` and vim `y`/`p` internal when OSC 52 clipboard writes are unreliable. Bracketed terminal paste still works.
    system_clipboard:      Bool   = true;
    /// Auto-summarize when request context usage crosses `compact_threshold` (forced on in headless).
    auto_compact:          Bool   = true;
    /// Idle auto-continue policy: `off` disables it, `goal` continues active auto goals, and `always` continues any idle session.
    auto_continue:         String = "goal", choices: ["off", "goal", "always"];
    /// Tokens/sec in status bar.
    show_tps:              Bool   = true;
    /// Context token count in status bar.
    show_tokens:           Bool   = true;
    /// Session cost in status bar.
    show_cost:             Bool   = true;
    /// Ghost-text input predictions in the prompt.
    show_prediction:       Bool   = true;
    /// Curated discovery tips in the start banner and prompt chrome.
    show_tips:             Bool   = true;
    /// Show Nerd Font file-type icons before inline-code paths that point at existing files.
    file_icons:            Bool   = false;
    /// Color inline-code file icons with nvim-web-devicons colors when `file_icons` is enabled.
    file_icon_colors:      Bool   = true;
    /// Task-slug label in status bar.
    show_slug:             Bool   = true;
    /// Keep the terminal window/tab title in sync with the current session title.
    terminal_title:        Bool   = true;
    /// Downgrade `Allow` to `Ask` for paths outside the workspace.
    restrict_to_workspace: Bool   = true;
    /// Scrub detected secrets from user input and tool results before they reach the LLM.
    redact_secrets:        Bool   = false;
    /// Watch Lua config inputs (init.lua, plugins/, commands/,
    /// completers/, tools/, dialogs/, runtime overrides) and dispatch
    /// `/reload` when any of them changes. Prompt inputs such as
    /// AGENTS.md, SKILL.md, and `--system-prompt` stay manual via `/reload`.
    auto_reload:           Bool   = true;
    /// Fraction of the configured context window (0, 1] at which the
    /// bundled compact plugin auto-triggers before oversized requests.
    compact_threshold:     Number = 0.80;
    /// Minimum number of trailing message groups kept verbatim after
    /// compaction. A group is a user message, a plain assistant message,
    /// or an assistant tool-use step together with its tool outputs.
    compact_keep_recent_groups: Number = 1.0;
    /// Request audit storage mode. `summary` keeps timing, token, cost, and size
    /// metadata only; `full` stores reconstructable provider payloads; `off`
    /// disables request audit writes.
    request_audit:         String = "summary", choices: ["off", "summary", "full"];
    /// Request the provider's accelerated inference mode when supported.
    fast_mode:             Bool   = false;
    /// Anthropic prompt cache TTL. `false` uses the 5-minute ephemeral
    /// TTL; `true` opts into the 1-hour TTL. Has no effect on
    /// non-Anthropic providers.
    cache_ttl_long:        Bool   = false;
    /// Search provider used by `web_search`; `auto` prefers Brave when its API key is available and otherwise uses DuckDuckGo.
    web_search_provider:   String = "auto", choices: ["auto", "duckduckgo", "brave"];
    /// Environment variable containing the Brave Search API key.
    brave_search_api_key_env: String = "BRAVE_SEARCH_API_KEY";
    /// JavaScript rendering policy for `web_fetch`: `http` never launches a browser, `auto` renders challenge and SPA-shell responses, and `browser` always renders.
    web_fetch_render:       String = "auto", choices: ["http", "auto", "browser"];
    /// Renderer executable used by `web_fetch`. It reads a JSON request from stdin and writes rendered HTML, status, and final URL as JSON to stdout.
    web_fetch_renderer_command: String = "";
    /// Root directory for managed git worktrees. Relative paths are resolved
    /// inside the git root and contain worktrees directly; absolute paths are
    /// external roots and get a per-repository bucket. Supports leading `~`,
    /// `$VAR`, and `${VAR}` expansion; relative roots may not escape the repo.
    worktree_root:         String = ".worktrees";
    /// Autoupgrade behavior. `"off"` skips checks; `"notify"` shows a
    /// pill when an update is available; `"auto"` installs in
    /// background on detection.
    autoupgrade:           String = "notify", choices: ["off", "notify", "auto"];
    /// Release channel autoupgrade tracks: `"stable"` (published GitHub
    /// releases) or `"unstable"` (`main` HEAD).
    autoupgrade_channel:   String = "stable", choices: ["stable", "unstable"];
    /// Seconds between background autoupgrade checks. The upgrade
    /// plugin clamps to a 60-second minimum to avoid hammering GitHub.
    autoupgrade_interval:  Number = 3600.0;
}

pub fn setting_decl(key: &str) -> Option<&'static SettingDecl> {
    SETTINGS.iter().find(|d| d.key == key)
}

pub fn setting_kind(key: &str) -> Option<SettingKind> {
    setting_decl(key).map(|d| d.kind)
}

impl ResolvedSettings {
    /// Apply an override by key. Returns an error message on unknown
    /// keys, kind mismatches, or string values outside the schema's
    /// allowed-choice list.
    pub fn set(&mut self, key: &str, value: &SettingValue) -> Result<(), String> {
        let decl = setting_decl(key).ok_or_else(|| format!("unknown setting '{key}'"))?;
        if matches!(value, SettingValue::Number(number) if !number.is_finite()) {
            return Err(format!("setting '{key}' must be finite"));
        }
        if value.kind() != decl.kind {
            return Err(format!(
                "setting '{key}' expects {:?}, got {:?}",
                decl.kind,
                value.kind()
            ));
        }
        if let (SettingValue::String(s), Some(choices)) = (value, decl.choices) {
            if !choices.contains(&s.as_str()) {
                return Err(format!("setting '{key}': '{s}' is not one of {choices:?}"));
            }
        }
        if !(decl.write)(self, value) {
            return Err(format!("setting '{key}': internal kind mismatch"));
        }
        Ok(())
    }

    /// Projection helper for Lua / status / introspection. Returns
    /// `None` for unknown keys.
    pub fn get(&self, key: &str) -> Option<SettingValue> {
        setting_decl(key).map(|d| (d.read)(self))
    }
}

/// Startup defaults for new sessions, set from Lua via `smelt.defaults{...}`.
/// Each field is the "no recent memory" fallback: the last-used pick
/// (when `smelt.remember[k]` is on) and CLI flags both win.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DefaultsConfig {
    /// Starting model reference (`"provider/model"` or bare model name).
    pub model: Option<String>,
    /// Starting agent mode. Must name a registered mode.
    pub mode: Option<String>,
    /// Starting reasoning effort. Known labels are `"off"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`, and `"ultra"`; provider-defined labels are also accepted.
    pub reasoning_effort: Option<String>,
}

/// Per-key opt-in to last-used recall on startup. All true by default;
/// flip any to `false` from init.lua via `smelt.remember.set({...})`
/// to make that key always start from `smelt.defaults` regardless of
/// what the user picked in the previous session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberConfig {
    pub model: bool,
    pub mode: bool,
    pub reasoning_effort: bool,
}

impl Default for RememberConfig {
    fn default() -> Self {
        Self {
            model: true,
            mode: true,
            reasoning_effort: true,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub providers: Vec<ProviderConfig>,
    pub settings: ResolvedSettings,
    /// MCP server configurations.
    pub mcp: std::collections::HashMap<String, crate::mcp::McpServerConfig>,
    /// LSP server configurations.
    pub lsp: crate::lsp::LspConfig,
    pub defaults: DefaultsConfig,
    pub remember: RememberConfig,
}

/// A resolved model entry combining provider connection info with model config.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    /// Display key: "provider_name/model_name"
    pub key: String,
    pub provider_name: String,
    pub model_name: String,
    pub display_name: Option<String>,
    pub api_base: String,
    pub api_key_env: String,
    /// Provider type from config: "openai-compatible" (default), "openai", "codex", "anthropic-compatible", "anthropic", or "copilot".
    pub provider_type: String,
    pub config: ModelConfig,
    pub catalog: ModelCatalogMetadata,
}

impl ResolvedModel {
    pub fn reasoning_catalog(&self) -> ModelCatalogMetadata {
        smelt_provider::reasoning_catalog(
            &self.provider_type,
            &self.api_base,
            &self.model_name,
            &self.config,
            &self.catalog,
        )
    }

    /// Construct a dispatch-ready target after the caller resolves its key env.
    pub fn target(&self, api_key: String) -> protocol::ModelTarget {
        protocol::ModelTarget {
            model: self.model_name.clone(),
            api_base: self.api_base.clone(),
            api_key,
            provider_type: self.provider_type.clone(),
            config: self.config.clone(),
        }
    }
}

pub type DynamicModel = protocol::ModelMetadata;

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
                "ambiguous reference '{reference}'. Use provider/model ({})",
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

fn is_kimi_code_provider(provider: &ProviderConfig) -> bool {
    provider.name.as_deref() == Some("kimi-code")
        || provider
            .api_base
            .as_deref()
            .is_some_and(smelt_provider::is_kimi_code_api_base)
}

pub(crate) fn is_managed_provider_kind(provider: &ProviderConfig, kind: &str) -> bool {
    match kind {
        "codex" | "copilot" => provider.provider_type.as_deref() == Some(kind),
        "kimi-code" => is_kimi_code_provider(provider),
        _ => false,
    }
}

impl Config {
    /// Flatten providers + models into a list of resolved model entries.
    pub fn resolve_models(&self) -> Vec<ResolvedModel> {
        let mut out = Vec::new();
        for provider in &self.providers {
            let provider_name = provider.name.clone().unwrap_or_default();
            let provider_type = provider
                .provider_type
                .clone()
                .unwrap_or_else(|| "openai-compatible".to_string());
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
                    display_name: None,
                    api_base: api_base.clone(),
                    api_key_env: api_key_env.clone(),
                    provider_type: provider_type.clone(),
                    config: model.clone(),
                    catalog: ModelCatalogMetadata::default(),
                });
            }
        }
        out
    }

    fn inject_dynamic_models(
        &self,
        resolved: &mut Vec<ResolvedModel>,
        provider: &ProviderConfig,
        provider_type: &str,
        models: &[DynamicModel],
    ) {
        let provider_name = provider.name.clone().unwrap_or_default();
        let api_base = provider.api_base.clone().unwrap_or_default();
        let api_key_env = provider.api_key_env.clone().unwrap_or_default();

        for model in models {
            for resolved_model in resolved.iter_mut().filter(|entry| {
                entry.provider_name == provider_name && model.matches_name(&entry.model_name)
            }) {
                if resolved_model.display_name.is_none() {
                    resolved_model.display_name = model.display_name.clone();
                }
                resolved_model.config.context_window = resolved_model
                    .config
                    .context_window
                    .or(model.context_window);
                resolved_model.config.max_tokens =
                    resolved_model.config.max_tokens.or(model.max_output_tokens);
                resolved_model.config.supports_reasoning = resolved_model
                    .config
                    .supports_reasoning
                    .or(model.supports_reasoning);
                resolved_model.config.supports_fast_mode = resolved_model
                    .config
                    .supports_fast_mode
                    .or(model.supports_fast_mode);
                if resolved_model.catalog.description.is_none() {
                    resolved_model.catalog.description = model.catalog.description.clone();
                }
                if resolved_model.catalog.default_reasoning_effort.is_none() {
                    resolved_model.catalog.default_reasoning_effort =
                        model.catalog.default_reasoning_effort.clone();
                }
                if resolved_model
                    .catalog
                    .supported_reasoning_efforts
                    .is_empty()
                {
                    resolved_model.catalog.supported_reasoning_efforts =
                        model.catalog.supported_reasoning_efforts.clone();
                }
                if resolved_model.config.input_modalities.is_none() {
                    resolved_model.config.input_modalities = model.input_modalities.clone();
                }
            }
        }

        // Managed providers return models in recommendation order. Preserve it so
        // catalog fallback follows provider priority instead of model spelling.
        for model in models {
            if resolved
                .iter()
                .any(|entry| entry.provider_name == provider_name && entry.model_name == model.id)
            {
                continue;
            }
            resolved.push(ResolvedModel {
                key: format!("{provider_name}/{}", model.id),
                provider_name: provider_name.clone(),
                model_name: model.id.clone(),
                display_name: model.display_name.clone(),
                api_base: api_base.clone(),
                api_key_env: api_key_env.clone(),
                provider_type: provider_type.to_string(),
                config: ModelConfig {
                    name: Some(model.id.clone()),
                    max_tokens: model.max_output_tokens,
                    context_window: model.context_window,
                    supports_reasoning: model.supports_reasoning,
                    supports_fast_mode: model.supports_fast_mode,
                    input_modalities: model.input_modalities.clone(),
                    ..ModelConfig::default()
                },
                catalog: model.catalog.clone(),
            });
        }
    }

    /// Replace codex placeholders with dynamically fetched model metadata.
    pub fn inject_codex_models(&self, resolved: &mut Vec<ResolvedModel>, models: &[DynamicModel]) {
        let Some(provider) = self
            .providers
            .iter()
            .find(|p| p.provider_type.as_deref() == Some("codex"))
        else {
            return;
        };
        self.inject_dynamic_models(resolved, provider, "codex", models);
    }

    /// Returns true if the config has a codex provider.
    pub fn has_codex_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.provider_type.as_deref() == Some("codex"))
    }

    /// Replace copilot placeholders with dynamically fetched model metadata.
    pub fn inject_copilot_models(
        &self,
        resolved: &mut Vec<ResolvedModel>,
        models: &[DynamicModel],
    ) {
        let Some(provider) = self
            .providers
            .iter()
            .find(|p| p.provider_type.as_deref() == Some("copilot"))
        else {
            return;
        };
        self.inject_dynamic_models(resolved, provider, "copilot", models);
    }

    pub fn has_copilot_provider(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.provider_type.as_deref() == Some("copilot"))
    }

    /// Add Kimi Code models fetched from the provider without clobbering
    /// statically configured aliases.
    pub fn inject_kimi_code_models(
        &self,
        resolved: &mut Vec<ResolvedModel>,
        models: &[DynamicModel],
    ) {
        let Some(provider) = self.providers.iter().find(|p| is_kimi_code_provider(p)) else {
            return;
        };
        self.inject_dynamic_models(resolved, provider, "kimi-code", models);
    }

    /// Returns true if the config has a Kimi Code provider.
    pub fn has_kimi_code_provider(&self) -> bool {
        self.providers.iter().any(is_kimi_code_provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_models_from_config() {
        let cfg = Config {
            providers: vec![
                ProviderConfig {
                    name: Some("zai".to_string()),
                    provider_type: Some("openai-compatible".to_string()),
                    api_base: Some("https://api.z.ai/api/coding/paas/v4".to_string()),
                    api_key_env: Some("Z_AI_API_KEY".to_string()),
                    models: vec![ModelConfig {
                        name: Some("glm-4.7".to_string()),
                        ..Default::default()
                    }],
                },
                ProviderConfig {
                    name: Some("box".to_string()),
                    provider_type: Some("openai-compatible".to_string()),
                    api_base: Some("https://llm.box.home.arpa".to_string()),
                    api_key_env: Some("BOX_API_KEY".to_string()),
                    models: vec![
                        ModelConfig {
                            name: Some("Qwen3.5-122B-A10B-Q4_0".to_string()),
                            ..Default::default()
                        },
                        ModelConfig {
                            name: Some("Qwen3.5-27B-Q8_0".to_string()),
                            ..Default::default()
                        },
                        ModelConfig {
                            name: Some("gpt-oss-120b-Q8_0".to_string()),
                            ..Default::default()
                        },
                        ModelConfig {
                            name: Some("gpt-oss-20b-Q8_0".to_string()),
                            ..Default::default()
                        },
                    ],
                },
            ],
            ..Default::default()
        };
        let resolved = cfg.resolve_models();

        assert_eq!(resolved.len(), 5);
        assert_eq!(resolved[0].key, "zai/glm-4.7");
        assert_eq!(resolved[0].api_base, "https://api.z.ai/api/coding/paas/v4");
        assert_eq!(resolved[1].key, "box/Qwen3.5-122B-A10B-Q4_0");
        assert_eq!(resolved[1].api_base, "https://llm.box.home.arpa");
    }

    #[test]
    fn dynamic_metadata_preserves_aliases_fields_and_provider_order() {
        let cfg = Config {
            providers: vec![ProviderConfig {
                name: Some("copilot".into()),
                provider_type: Some("copilot".into()),
                api_base: Some("https://copilot.example".into()),
                api_key_env: Some("COPILOT_KEY".into()),
                models: vec![ModelConfig {
                    name: Some("friendly-alias".into()),
                    context_window: Some(8_192),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let mut resolved = cfg.resolve_models();
        cfg.inject_copilot_models(
            &mut resolved,
            &[
                DynamicModel {
                    id: "z-model".into(),
                    display_name: Some("Zed".into()),
                    context_window: Some(64_000),
                    max_output_tokens: Some(4_096),
                    supports_reasoning: Some(true),
                    supports_fast_mode: Some(true),
                    input_modalities: Some(vec!["text".into()]),
                    catalog: ModelCatalogMetadata::default(),
                },
                DynamicModel {
                    id: "canonical".into(),
                    display_name: Some("friendly-alias".into()),
                    context_window: Some(128_000),
                    max_output_tokens: Some(8_192),
                    supports_reasoning: Some(true),
                    supports_fast_mode: Some(true),
                    input_modalities: Some(vec!["text".into(), "image".into()]),
                    catalog: ModelCatalogMetadata {
                        description: Some("provider metadata".into()),
                        show_in_picker: false,
                        default_reasoning_effort: Some(protocol::ReasoningEffort::Low),
                        supported_reasoning_efforts: vec![
                            protocol::ReasoningEffort::Low,
                            protocol::ReasoningEffort::High,
                        ],
                    },
                },
            ],
        );

        assert_eq!(resolved[0].key, "copilot/friendly-alias");
        assert_eq!(resolved[0].config.context_window, Some(8_192));
        assert_eq!(resolved[0].config.max_tokens, Some(8_192));
        assert_eq!(resolved[0].display_name.as_deref(), Some("friendly-alias"));
        assert_eq!(
            resolved[0].catalog.description.as_deref(),
            Some("provider metadata")
        );
        assert_eq!(
            resolved[0].catalog.supported_reasoning_efforts,
            vec![
                protocol::ReasoningEffort::Low,
                protocol::ReasoningEffort::High,
            ]
        );
        assert_eq!(resolved[1].key, "copilot/z-model");
        assert_eq!(resolved[2].key, "copilot/canonical");
        assert!(resolved[0].catalog.show_in_picker);
        assert!(!resolved[2].catalog.show_in_picker);
        assert!(resolved[1..]
            .iter()
            .all(|model| model.api_key_env == "COPILOT_KEY"));
    }

    #[test]
    fn hidden_dynamic_models_remain_explicitly_resolvable() {
        let cfg = Config {
            providers: vec![ProviderConfig {
                name: Some("codex".into()),
                provider_type: Some("codex".into()),
                api_base: Some("https://chatgpt.com/backend-api/codex/responses".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut resolved = cfg.resolve_models();
        cfg.inject_codex_models(
            &mut resolved,
            &[
                DynamicModel {
                    id: "gpt-6-astra".into(),
                    display_name: Some("GPT-6-Astra".into()),
                    context_window: Some(272_000),
                    max_output_tokens: None,
                    supports_reasoning: Some(true),
                    supports_fast_mode: Some(false),
                    input_modalities: None,
                    catalog: ModelCatalogMetadata {
                        description: None,
                        show_in_picker: false,
                        default_reasoning_effort: Some(protocol::ReasoningEffort::Low),
                        supported_reasoning_efforts: vec![
                            protocol::ReasoningEffort::Low,
                            protocol::ReasoningEffort::XHigh,
                            protocol::ReasoningEffort::Max,
                            protocol::ReasoningEffort::Ultra,
                        ],
                    },
                },
                DynamicModel {
                    id: "visible".into(),
                    display_name: Some("Visible".into()),
                    context_window: None,
                    max_output_tokens: None,
                    supports_reasoning: None,
                    supports_fast_mode: None,
                    input_modalities: None,
                    catalog: ModelCatalogMetadata::default(),
                },
            ],
        );

        let astra = resolve_model_ref(&resolved, "codex/gpt-6-astra").unwrap();
        assert!(!astra.catalog.show_in_picker);
        assert_eq!(
            astra.catalog.supported_reasoning_efforts,
            vec![
                protocol::ReasoningEffort::Low,
                protocol::ReasoningEffort::XHigh,
                protocol::ReasoningEffort::Max,
                protocol::ReasoningEffort::Ultra,
            ]
        );
    }

    #[test]
    fn every_setting_has_the_expected_runtime_effect_classification() {
        let classified: std::collections::HashMap<_, _> = SETTINGS
            .iter()
            .map(|setting| (setting.key, setting.effect))
            .collect();
        let expected = [
            ("vim", SettingEffect::Input),
            ("system_clipboard", SettingEffect::Input),
            ("show_tps", SettingEffect::Render),
            ("show_tokens", SettingEffect::Render),
            ("show_cost", SettingEffect::Render),
            ("show_slug", SettingEffect::Render),
            ("show_tips", SettingEffect::Render),
            ("show_prediction", SettingEffect::Prediction),
            ("file_icons", SettingEffect::FileIcons),
            ("file_icon_colors", SettingEffect::FileIcons),
            ("terminal_title", SettingEffect::TerminalTitle),
            ("restrict_to_workspace", SettingEffect::Permissions),
            ("worktree_root", SettingEffect::Permissions),
            ("auto_reload", SettingEffect::AutoReload),
            ("redact_secrets", SettingEffect::FutureRequests),
            ("fast_mode", SettingEffect::FutureRequests),
            ("cache_ttl_long", SettingEffect::FutureRequests),
            ("request_audit", SettingEffect::FutureRequests),
            ("auto_compact", SettingEffect::Compaction),
            ("compact_threshold", SettingEffect::Compaction),
            ("compact_keep_recent_groups", SettingEffect::Compaction),
            ("auto_continue", SettingEffect::AutoContinue),
            ("web_search_provider", SettingEffect::WebSearch),
            ("brave_search_api_key_env", SettingEffect::WebSearch),
            ("web_fetch_render", SettingEffect::FutureRequests),
            ("web_fetch_renderer_command", SettingEffect::FutureRequests),
            ("autoupgrade", SettingEffect::Upgrade),
            ("autoupgrade_channel", SettingEffect::Upgrade),
            ("autoupgrade_interval", SettingEffect::Upgrade),
        ];

        assert_eq!(classified.len(), expected.len());
        for (key, effect) in expected {
            assert_eq!(classified.get(key), Some(&effect), "{key}");
        }
    }

    #[test]
    fn numeric_settings_reject_non_finite_values() {
        let mut settings = ResolvedSettings::default();
        assert_eq!(
            settings.set("compact_threshold", &SettingValue::Number(f64::NAN)),
            Err("setting 'compact_threshold' must be finite".into())
        );
    }
}
