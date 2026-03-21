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
    #[arg(long)]
    model: Option<String>,
    #[arg(
        long,
        value_name = "MODE",
        help = "Agent mode: normal, plan, apply, yolo"
    )]
    mode: Option<String>,
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
    let (api_base, api_key, api_key_env, model, model_config) = {
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
            (base, key, key_env, r.model_name.clone(), r.config.clone())
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
                base,
                key,
                key_env,
                model,
                tui::config::ModelConfig::default(),
            )
        }
    };

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
    let restrict_to_workspace = cfg.settings.restrict_to_workspace.unwrap_or(true);

    // Parse reasoning effort: defaults.reasoning_effort if set, otherwise use saved state
    let reasoning_effort = cfg
        .defaults
        .reasoning_effort
        .as_deref()
        .map(|s| match s.to_lowercase().as_str() {
            "low" => ReasoningEffort::Low,
            "medium" => ReasoningEffort::Medium,
            "high" => ReasoningEffort::High,
            _ => ReasoningEffort::Off,
        })
        .unwrap_or(app_state.reasoning_effort);

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
            if !is_headless {
                if let Some(id) = session_id {
                    tui::session::print_resume_hint(&id);
                }
            }
            std::process::exit(0);
        });
    }

    // Assemble the system prompt
    let cwd = std::env::current_dir().unwrap_or_default();
    let instructions = tui::instructions::load();
    let initial_mode = mode_override.unwrap_or(protocol::Mode::Normal);
    let system_prompt = engine::build_system_prompt(initial_mode, &cwd, instructions.as_deref());

    // Start the engine
    let mut permissions = engine::Permissions::load();
    permissions.set_workspace(cwd.clone());
    permissions.set_restrict_to_workspace(restrict_to_workspace);
    let permissions = Arc::new(permissions);
    let initial_api_base = api_base.clone();
    let engine_handle = engine::start(engine::EngineConfig {
        api_base,
        api_key,
        model_config: engine::ModelConfig {
            name: model_config.name.clone(),
            temperature: model_config.temperature,
            top_p: model_config.top_p,
            top_k: model_config.top_k,
            min_p: model_config.min_p,
            repeat_penalty: model_config.repeat_penalty,
        },
        system_prompt,
        cwd,
        permissions: permissions.clone(),
    });

    // Fetch context window in background (before moving available_models)
    let ctx_rx = {
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
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let provider = engine::Provider::new(ctx_api_base, ctx_api_key, reqwest::Client::new());
            let _ = tx.send(provider.fetch_context_window(&ctx_model).await);
        });
        Some(rx)
    };

    // Build the TUI app
    let mut app = tui::app::App::new(
        model,
        initial_api_base,
        api_key_env,
        Arc::clone(&permissions),
        engine_handle,
        vim_enabled,
        auto_compact,
        show_speed,
        restrict_to_workspace,
        reasoning_effort,
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
