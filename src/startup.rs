use crate::{setup, Args};
use protocol::{Mode, ReasoningEffort};

/// Read an API key from the given environment variable name.
/// An empty `key_env` yields an empty key (used by providers that don't need one).
pub fn resolve_api_key(key_env: &str) -> Result<String, String> {
    if key_env.is_empty() {
        return Ok(String::new());
    }
    match std::env::var(key_env) {
        Ok(key) => Ok(key),
        Err(std::env::VarError::NotPresent) => Err(format!(
            "environment variable '{key_env}' is not set but is required for API authentication"
        )),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "environment variable '{key_env}' contains non-Unicode data and cannot be used as an API key"
        )),
    }
}

/// Everything resolved from args + config + cached state before the engine
/// starts. Produced by [`resolve`] and consumed by the mode dispatch in `main`.
pub struct ResolvedStartup {
    pub cfg: tui::config::Config,
    pub available_models: Vec<tui::config::ResolvedModel>,
    pub auxiliary: engine::AuxiliaryModelConfig,
    pub api_base: String,
    pub api_key: String,
    pub api_key_env: String,
    pub provider_type: String,
    pub model: String,
    pub model_config: tui::config::ModelConfig,
    pub settings: tui::state::ResolvedSettings,
    pub multi_agent: bool,
    pub mode_override: Option<Mode>,
    pub mode_cycle: Vec<Mode>,
    pub reasoning_effort: ReasoningEffort,
    pub reasoning_cycle: Vec<ReasoningEffort>,
    pub startup_auth_error: Option<String>,
}

/// Resolve the four priority fallbacks for the active model reference:
/// CLI `--model` > config default > cached selection > first in config.
///
/// Returns `None` only when all sources are empty or when a CLI model is
/// not found in the resolved models and `allow_not_found_cli` is true (the
/// caller then falls back to `--api-base`-driven configuration).
fn resolve_model_reference(
    args: &Args,
    cfg: &tui::config::Config,
    available_models: &[tui::config::ResolvedModel],
    app_state: &tui::state::State,
) -> Option<tui::config::ResolvedModel> {
    let pick = |reference: &str, allow_not_found: bool| match tui::config::resolve_model_ref(
        available_models,
        reference,
    ) {
        Ok(model) => Some(model.clone()),
        Err(tui::config::ResolveModelRefError::NotFound { .. }) if allow_not_found => None,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };

    if let Some(ref cli_model) = args.model {
        // If the user passed --api-base alongside, a missing --model is allowed —
        // the caller will build a config from --api-base/--model directly.
        pick(cli_model, args.api_base.is_some())
    } else if let Some(default) = cfg.get_default_model() {
        // Config has a default: use it, ignore cached selection.
        pick(default, false)
    } else if let Some(ref cached) = app_state.selected_model {
        // No config default: prefer last-used, fall back to first if stale.
        tui::config::resolve_model_ref(available_models, cached)
            .ok()
            .cloned()
            .or_else(|| available_models.first().cloned())
    } else {
        available_models.first().cloned()
    }
}

/// Load config (honouring `--config` + `--set`), fetch dynamic model lists,
/// resolve the active model, auxiliary routing, API keys, and all pure
/// defaults merges (mode, reasoning, settings, multi-agent).
pub async fn resolve(args: &Args) -> ResolvedStartup {
    let mut cfg = match args.config {
        Some(ref path) => {
            let c = tui::config::Config::load_from(std::path::Path::new(path));
            match c.source {
                Some(tui::config::ConfigSource::NotFound) => {
                    eprintln!("error: config file not found: {path}");
                    std::process::exit(1);
                }
                Some(tui::config::ConfigSource::ParseError) => {
                    // warning already printed by load_from
                    std::process::exit(1);
                }
                _ => c,
            }
        }
        None => tui::config::Config::load(),
    };

    for pair in &args.set {
        let Some((key, value)) = pair.split_once('=') else {
            eprintln!("error: --set requires KEY=VALUE format, got '{pair}'");
            std::process::exit(1);
        };
        if let Err(e) = cfg.settings.apply(key, value) {
            eprintln!("error: --set {pair}: {e}");
            std::process::exit(1);
        }
    }

    cfg.inject_oauth_providers();

    let app_state = tui::state::State::load();
    let mut available_models = cfg.resolve_models();

    // For Codex providers, fetch models dynamically from the API (with cache).
    // Use cached models for instant startup; always refresh in background.
    // Done before auxiliary validation so cached codex slugs are in scope.
    if cfg.has_codex_provider() {
        let ids = engine::auth::cached_models(engine::auth::AuthProvider::Codex);
        if !ids.is_empty() {
            cfg.inject_codex_models(&mut available_models, &ids);
        }
        tokio::spawn(async {
            let client = reqwest::Client::new();
            let _ = engine::auth::refresh_models_cache(engine::auth::AuthProvider::Codex, &client)
                .await;
        });
    }

    // Same pattern for GitHub Copilot.
    if cfg.has_copilot_provider() {
        let ids = engine::auth::cached_models(engine::auth::AuthProvider::Copilot);
        if !ids.is_empty() {
            cfg.inject_copilot_models(&mut available_models, &ids);
        }
        tokio::spawn(async {
            let client = reqwest::Client::new();
            let _ =
                engine::auth::refresh_models_cache(engine::auth::AuthProvider::Copilot, &client)
                    .await;
        });
    }

    let auxiliary_routing = match cfg.resolve_auxiliary_routing(&available_models) {
        Ok(routing) => routing,
        Err(err) => {
            eprintln!("error: auxiliary.model: {err}");
            std::process::exit(1);
        }
    };

    let mut startup_auth_error: Option<String> = None;

    // Resolve the active model and the connection details derived from it.
    let (api_base, api_key, api_key_env, mut provider_type, model, mut model_config) = {
        let resolved = resolve_model_reference(args, &cfg, &available_models, &app_state);

        if let Some(r) = resolved {
            let base = args.api_base.clone().unwrap_or_else(|| r.api_base.clone());
            let key_env = args
                .api_key_env
                .clone()
                .unwrap_or_else(|| r.api_key_env.clone());
            let key = match resolve_api_key(&key_env) {
                Ok(key) => key,
                Err(err) => {
                    startup_auth_error = Some(err);
                    String::new()
                }
            };
            (
                base,
                key,
                key_env,
                r.provider_type.clone(),
                r.model_name.clone(),
                r.config.clone(),
            )
        } else if cfg.source == Some(tui::config::ConfigSource::NotFound) && args.api_base.is_none()
        {
            // No config at all — run the interactive setup wizard.
            if !setup::run_initial_setup(&cfg.path).await {
                std::process::exit(1);
            }
            cfg = tui::config::Config::load_from(&cfg.path);
            cfg.inject_oauth_providers();
            available_models = cfg.resolve_models();
            // Inject cached models for OAuth providers discovered after the wizard.
            if cfg.has_codex_provider() {
                let ids = engine::auth::cached_models(engine::auth::AuthProvider::Codex);
                if !ids.is_empty() {
                    cfg.inject_codex_models(&mut available_models, &ids);
                }
            }
            if cfg.has_copilot_provider() {
                let ids = engine::auth::cached_models(engine::auth::AuthProvider::Copilot);
                if !ids.is_empty() {
                    cfg.inject_copilot_models(&mut available_models, &ids);
                }
            }
            if let Some(r) = available_models.first() {
                let key = match resolve_api_key(&r.api_key_env) {
                    Ok(key) => key,
                    Err(err) => {
                        startup_auth_error = Some(err);
                        String::new()
                    }
                };
                (
                    r.api_base.clone(),
                    key,
                    r.api_key_env.clone(),
                    r.provider_type.clone(),
                    r.model_name.clone(),
                    r.config.clone(),
                )
            } else {
                eprintln!("error: setup completed but no models found in config");
                std::process::exit(1);
            }
        } else if let Some(base) = args.api_base.clone() {
            let key_env = args.api_key_env.clone().unwrap_or_default();
            let key = match resolve_api_key(&key_env) {
                Ok(key) => key,
                Err(err) => {
                    startup_auth_error = Some(err);
                    String::new()
                }
            };
            let Some(model) = args.model.clone() else {
                eprintln!("error: --model is required when using --api-base without a config file");
                std::process::exit(1);
            };
            (
                base.clone(),
                key,
                key_env,
                engine::ProviderKind::detect_from_url(&base)
                    .as_config_str()
                    .to_string(),
                model,
                tui::config::ModelConfig::default(),
            )
        } else {
            match cfg.source {
                Some(tui::config::ConfigSource::ParseError) => {
                    eprintln!(
                        "error: config file at {} failed to parse (see warning above)\n\
                         Fix the config or provide --api-base and --model.",
                        cfg.path.display()
                    );
                }
                _ => {
                    eprintln!(
                        "error: no providers with models found in {}\n\
                         Add a provider with models, or provide --api-base and --model.",
                        cfg.path.display()
                    );
                }
            }
            std::process::exit(1);
        }
    };

    // CLI --type overrides config/auto-detected provider type.
    // CLI --api-base re-triggers auto-detect when no --type is given.
    if let Some(ref t) = args.r#type {
        provider_type = t.clone();
    } else if args.api_base.is_some() {
        provider_type = engine::ProviderKind::detect_from_url(&api_base)
            .as_config_str()
            .to_string();
    }

    // Apply CLI sampling overrides to model_config.
    if let Some(v) = args.temperature {
        model_config.temperature = Some(v);
    }
    if let Some(v) = args.top_p {
        model_config.top_p = Some(v);
    }
    if let Some(v) = args.top_k {
        model_config.top_k = Some(v);
    }
    if args.no_tool_calling {
        model_config.tool_calling = Some(false);
    }

    // Resolve auxiliary request configs; auth errors are captured but non-fatal
    // here so interactive sessions can still render their "set your API key"
    // hint without aborting.
    let auxiliary = {
        let mut build = |task: tui::config::AuxiliaryTask| {
            auxiliary_routing.model_for(task).map(|resolved| {
                let key = resolve_api_key(&resolved.api_key_env).unwrap_or_else(|err| {
                    startup_auth_error.get_or_insert(err);
                    String::new()
                });
                engine::RequestModelConfig {
                    model: resolved.model_name.clone(),
                    api: engine::ApiConfig {
                        base: resolved.api_base.clone(),
                        key,
                        key_env: resolved.api_key_env.clone(),
                        provider_type: resolved.provider_type.clone(),
                        model_config: (&resolved.config).into(),
                    },
                }
            })
        };
        engine::AuxiliaryModelConfig {
            title: build(tui::config::AuxiliaryTask::Title),
            prediction: build(tui::config::AuxiliaryTask::Prediction),
            compaction: build(tui::config::AuxiliaryTask::Compaction),
            btw: build(tui::config::AuxiliaryTask::Btw),
        }
    };

    // Multi-agent: CLI flags override config.
    let multi_agent = if args.no_multi_agent {
        false
    } else if args.multi_agent || args.subagent {
        true
    } else {
        cfg.settings.multi_agent.unwrap_or(false)
    };

    let mode_override = args
        .mode
        .as_deref()
        .or(cfg.defaults.mode.as_deref())
        .map(|s| {
            Mode::parse(s).unwrap_or_else(|| {
                eprintln!("warning: unknown mode '{s}', defaulting to normal");
                Mode::Normal
            })
        });

    let mode_cycle = args
        .mode_cycle
        .as_deref()
        .or(cfg.defaults.mode_cycle.as_deref())
        .map(Mode::parse_list)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| Mode::ALL.to_vec());

    // Reasoning effort: CLI --reasoning-effort > config defaults > saved state.
    let reasoning_effort = args
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
        .or_else(|| {
            cfg.defaults
                .reasoning_effort
                .as_deref()
                .and_then(ReasoningEffort::parse)
        })
        .unwrap_or(app_state.reasoning_effort);

    let provider_kind = engine::ProviderKind::from_config(&provider_type);
    let mut reasoning_cycle = args
        .reasoning_cycle
        .as_deref()
        .or(cfg.defaults.reasoning_cycle.as_deref())
        .map(ReasoningEffort::parse_list)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| provider_kind.default_reasoning_cycle().to_vec());
    if !reasoning_cycle.contains(&reasoning_effort) {
        reasoning_cycle.push(reasoning_effort);
    }

    let mut settings = app_state.settings.resolve(&cfg.settings);
    // Force auto_compact on for subagent/headless mode.
    if args.subagent || args.headless {
        settings.auto_compact = true;
    }

    ResolvedStartup {
        cfg,
        available_models,
        auxiliary,
        api_base,
        api_key,
        api_key_env,
        provider_type,
        model,
        model_config,
        settings,
        multi_agent,
        mode_override,
        mode_cycle,
        reasoning_effort,
        reasoning_cycle,
        startup_auth_error,
    }
}
