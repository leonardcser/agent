use clap::Parser;
use crossterm::ExecutableCommand;
use protocol::{Mode, ReasoningEffort};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "agent", about = "Coding agent TUI")]
struct Args {
    /// Initial message to send (auto-submits on startup)
    message: Option<String>,
    #[arg(long)]
    api_base: Option<String>,
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(
        long,
        value_name = "TYPE",
        help = "Provider type: openai-compatible, openai, anthropic"
    )]
    r#type: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(
        long,
        value_name = "MODE",
        help = "Agent mode: normal, plan, apply, yolo"
    )]
    mode: Option<String>,
    #[arg(
        long,
        value_name = "EFFORT",
        help = "Starting reasoning effort (off/low/medium/high/max)"
    )]
    reasoning_effort: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "LEVELS",
        help = "Available reasoning levels for cycling (comma-separated: off,low,medium,high,max)"
    )]
    reasoning_efforts: Option<Vec<String>>,
    #[arg(long, value_name = "TEMP", help = "Sampling temperature")]
    temperature: Option<f64>,
    #[arg(long, value_name = "VALUE", help = "Top-p (nucleus) sampling")]
    top_p: Option<f64>,
    #[arg(long, value_name = "VALUE", help = "Top-k sampling")]
    top_k: Option<u32>,
    #[arg(long, help = "Disable tool calling (model becomes chat-only)")]
    no_tool_calling: bool,
    #[arg(long, default_value = "info", value_name = "LEVEL")]
    log_level: String,
    #[arg(long, help = "Print performance timing summary on exit")]
    bench: bool,
    #[arg(long, help = "Run headless (no TUI), requires a message argument")]
    headless: bool,
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "SESSION_ID")]
    resume: Option<String>,
}

#[tokio::main]
async fn main() {
    std::panic::set_hook(Box::new(|info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
        let _ = std::io::stdout().execute(crossterm::cursor::Show);
        eprintln!("{info}");
    }));

    let args = Args::parse();
    let cfg = tui::config::Config::load();
    let app_state = tui::state::State::load();
    let available_models = cfg.resolve_models();

    // Resolve the active model: CLI flags > defaults.model (if set) > last_used (if no default) > first in config
    let (api_base, api_key, api_key_env, mut provider_type, model, mut model_config) = {
        let resolved = if let Some(ref cli_model) = args.model {
            available_models
                .iter()
                .find(|m| m.model_name == *cli_model || m.key == *cli_model)
        } else if let Some(default) = cfg.get_default_model() {
            // Config has a default: use it, ignore cached selection
            available_models
                .iter()
                .find(|m| m.key == default || m.model_name == default)
        } else if let Some(ref cached) = app_state.selected_model {
            // No config default: use last used model, fall back to first if stale
            available_models
                .iter()
                .find(|m| m.key == *cached)
                .or(available_models.first())
        } else {
            // Fallback to first model in config
            available_models.first()
        };

        if let Some(r) = resolved {
            let base = args.api_base.clone().unwrap_or_else(|| r.api_base.clone());
            let key_env = args
                .api_key_env
                .clone()
                .unwrap_or_else(|| r.api_key_env.clone());
            let key = std::env::var(&key_env).unwrap_or_default();
            (
                base,
                key,
                key_env,
                r.provider_type.clone(),
                r.model_name.clone(),
                r.config.clone(),
            )
        } else {
            let base = args
                .api_base
                .clone()
                .expect("api_base must be set via --api-base or config file");
            let key_env = args.api_key_env.clone().unwrap_or_default();
            let key = std::env::var(&key_env).unwrap_or_default();
            let model = args
                .model
                .clone()
                .expect("model must be set via --model or config file");
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

    if let Some(level) = engine::log::parse_level(&args.log_level) {
        engine::log::set_level(level);
    } else {
        eprintln!(
            "warning: invalid --log-level {}, defaulting to info",
            args.log_level
        );
    }

    if args.bench {
        tui::perf::enable();
    }

    if args.headless && args.message.is_none() {
        eprintln!("error: --headless requires a message argument");
        std::process::exit(1);
    }

    let mode_override = args.mode.as_deref().map(|s| {
        Mode::parse(s).unwrap_or_else(|| {
            eprintln!("warning: unknown --mode '{s}', defaulting to normal");
            Mode::Normal
        })
    });

    let vim_enabled = cfg.settings.vim_mode.unwrap_or(false);
    let auto_compact = cfg.settings.auto_compact.unwrap_or(false);
    let show_speed = cfg.settings.show_speed.unwrap_or(true);
    let input_prediction = cfg.settings.input_prediction.unwrap_or(true);
    let task_slug = cfg.settings.task_slug.unwrap_or(true);
    let restrict_to_workspace = cfg.settings.restrict_to_workspace.unwrap_or(true);

    // Apply CLI sampling overrides to model_config
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
    // Available reasoning efforts: CLI > config > provider-type defaults.
    let mut reasoning_efforts = args
        .reasoning_efforts
        .as_deref()
        .or(cfg.defaults.reasoning_efforts.as_deref())
        .map(ReasoningEffort::parse_list)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| provider_kind.default_reasoning_efforts().to_vec());
    // Ensure the starting effort is in the cycle list.
    if !reasoning_efforts.contains(&reasoning_effort) {
        reasoning_efforts.push(reasoning_effort);
    }

    // Parse theme accent from config
    if let Some(ref accent) = cfg.theme.accent {
        let theme_value = if let Ok(v) = accent.parse::<u8>() {
            v
        } else {
            // Try to find by name in presets
            tui::theme::PRESETS
                .iter()
                .find(|(name, _, _)| name.eq_ignore_ascii_case(accent))
                .map(|(_, _, value)| *value)
                .unwrap_or(tui::theme::DEFAULT_ACCENT)
        };
        tui::theme::set_accent(theme_value);
    }

    let shared_session: Arc<Mutex<Option<tui::session::Session>>> = Arc::new(Mutex::new(None));

    // Signal handler for graceful shutdown
    {
        let shared = shared_session.clone();
        let is_headless = args.headless;
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigint =
                    signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
                tokio::select! {
                    _ = sigint.recv() => {}
                    _ = sigterm.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await.ok();
            }
            if !is_headless {
                let session_id = if let Ok(guard) = shared.lock() {
                    if let Some(ref s) = *guard {
                        tui::session::save(s, &tui::attachment::AttachmentStore::new());
                        if !s.messages.is_empty() {
                            Some(s.id.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                let _ = crossterm::terminal::disable_raw_mode();
                let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
                println!();
                if let Some(id) = session_id {
                    tui::session::print_resume_hint(&id);
                }
            }
            std::process::exit(0);
        });
    }

    // Load instructions and workspace
    let cwd = std::env::current_dir().unwrap_or_default();
    let instructions = tui::instructions::load();

    // Start the engine
    let mut permissions = engine::Permissions::load();
    permissions.set_workspace(cwd.clone());
    permissions.set_restrict_to_workspace(restrict_to_workspace);
    let permissions = Arc::new(permissions);
    let initial_api_base = api_base.clone();
    let initial_provider_type = provider_type.clone();
    let engine_handle = engine::start(engine::EngineConfig {
        api_base,
        api_key,
        provider_type,
        model_config: engine::ModelConfig {
            name: model_config.name.clone(),
            temperature: model_config.temperature,
            top_p: model_config.top_p,
            top_k: model_config.top_k,
            min_p: model_config.min_p,
            repeat_penalty: model_config.repeat_penalty,
            tool_calling: model_config.tool_calling,
        },
        instructions,
        cwd,
        permissions: permissions.clone(),
    });

    // Fetch context window in background (only needed for interactive mode)
    let ctx_rx = if !args.headless {
        let ctx_api_base = args
            .api_base
            .clone()
            .or_else(|| available_models.first().map(|m| m.api_base.clone()))
            .unwrap_or_default();
        let ctx_api_key = args
            .api_key_env
            .as_deref()
            .or_else(|| available_models.first().map(|m| m.api_key_env.as_str()))
            .map(|env| std::env::var(env).unwrap_or_default())
            .unwrap_or_default();
        let ctx_model = model.clone();
        let ctx_provider_type = initial_provider_type.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let provider = engine::Provider::new(
                ctx_api_base,
                ctx_api_key,
                &ctx_provider_type,
                reqwest::Client::new(),
            );
            let _ = tx.send(provider.fetch_context_window(&ctx_model).await);
        });
        Some(rx)
    } else {
        None
    };

    // Build the TUI app
    let mut app = tui::app::App::new(
        model,
        initial_api_base,
        api_key_env,
        initial_provider_type,
        Arc::clone(&permissions),
        engine_handle,
        vim_enabled,
        auto_compact,
        show_speed,
        input_prediction,
        task_slug,
        restrict_to_workspace,
        reasoning_effort,
        reasoning_efforts,
        shared_session,
        available_models,
    );
    if let Some(mode) = mode_override {
        app.mode = mode;
    }

    if let Some(ref resume_val) = args.resume {
        if resume_val.is_empty() {
            app.resume_session_before_run();
        } else if let Some(loaded) = tui::session::load(resume_val) {
            app.load_session(loaded);
        } else {
            eprintln!("error: session '{}' not found", resume_val);
            std::process::exit(1);
        }
    }

    if args.headless {
        app.run_headless(args.message.unwrap()).await;
    } else {
        println!();
        app.run(ctx_rx, args.message).await;
        if !app.session.messages.is_empty() {
            tui::session::print_resume_hint(&app.session.id);
        }
    }
    tui::perf::print_summary();
}
