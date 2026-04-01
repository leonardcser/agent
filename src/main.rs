mod setup;

use clap::{Parser, Subcommand, ValueEnum};
use crossterm::ExecutableCommand;
use protocol::{Mode, ReasoningEffort};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "smelt", about = "Coding agent TUI", version)]
#[command(args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
    /// Initial message to send (auto-submits on startup)
    message: Option<String>,
    #[arg(long, value_name = "PATH", help = "Path to a custom config file")]
    config: Option<String>,
    #[arg(long)]
    api_base: Option<String>,
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(
        long,
        value_name = "TYPE",
        help = "Provider type: openai-compatible, openai, anthropic, codex"
    )]
    r#type: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(
        long,
        value_name = "MODE",
        help = "Agent mode: normal, plan, apply, yolo"
    )]
    mode: Option<String>,
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "MODES",
        help = "Modes available for cycling (comma-separated: normal,plan,apply,yolo)"
    )]
    mode_cycle: Option<Vec<String>>,
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
        help = "Reasoning effort levels for cycling (comma-separated: off,low,medium,high,max)"
    )]
    reasoning_cycle: Option<Vec<String>>,
    #[arg(long, value_name = "TEMP", help = "Sampling temperature")]
    temperature: Option<f64>,
    #[arg(long, value_name = "VALUE", help = "Top-p (nucleus) sampling")]
    top_p: Option<f64>,
    #[arg(long, value_name = "VALUE", help = "Top-k sampling")]
    top_k: Option<u32>,
    #[arg(long, help = "Disable tool calling (model becomes chat-only)")]
    no_tool_calling: bool,
    #[arg(
        long,
        conflicts_with = "no_system_prompt",
        help = "Override the system prompt (string or file path)"
    )]
    system_prompt: Option<String>,
    #[arg(
        long,
        conflicts_with = "system_prompt",
        help = "Disable system prompt and AGENTS.md instructions"
    )]
    no_system_prompt: bool,
    #[arg(long, default_value = "info", value_name = "LEVEL")]
    log_level: String,
    #[arg(long, help = "Print performance timing summary on exit")]
    bench: bool,
    #[arg(long, help = "Run headless (no TUI), requires a message argument")]
    headless: bool,
    #[arg(long, value_enum, default_value_t = OutputFormat::Text, help = "Headless output format")]
    format: OutputFormat,
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, help = "Color output")]
    color: ColorMode,
    #[arg(short, long, help = "Show tool output in headless mode")]
    verbose: bool,
    #[arg(long, help = "Run as a subagent (persistent headless with IPC)")]
    subagent: bool,
    #[arg(long, help = "Enable multi-agent mode (registry, socket, agent tools)")]
    multi_agent: bool,
    #[arg(long, help = "Disable multi-agent even if config enables it")]
    no_multi_agent: bool,
    #[arg(long, value_name = "PID", help = "Parent agent PID (for subagents)")]
    parent_pid: Option<u32>,
    #[arg(long, value_name = "N", help = "Agent depth in the spawn tree")]
    depth: Option<u8>,
    #[arg(
        long,
        value_name = "N",
        default_value = "1",
        help = "Maximum agent spawn depth"
    )]
    max_agent_depth: u8,
    #[arg(
        long,
        value_name = "N",
        default_value = "8",
        help = "Maximum concurrent agents per session"
    )]
    max_agents: u8,
    #[arg(short, long, num_args = 0..=1, default_missing_value = "", value_name = "SESSION_ID")]
    resume: Option<String>,
    #[arg(
        long,
        value_name = "KEY=VALUE",
        help = "Override a config setting (e.g. --set vim_mode=true)"
    )]
    set: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage provider authentication (add providers, Codex login/logout)
    Auth,
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

    // Handle subcommands before loading config.
    if let Some(Commands::Auth) = args.command {
        setup::run_auth_command().await;
        return;
    }

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
    let app_state = tui::state::State::load();
    let mut available_models = cfg.resolve_models();

    // For Codex providers, fetch models dynamically from the API (with cache).
    // Use cached models for instant startup; always refresh in background.
    if cfg.has_codex_provider() {
        let cached = engine::provider::codex::load_cached_models();
        if !cached.is_empty() {
            let slugs: Vec<String> = cached.into_iter().map(|m| m.slug).collect();
            cfg.inject_codex_models(&mut available_models, &slugs);
        }
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let _ = engine::provider::codex::refresh_models_cache(&client).await;
        });
    }

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
        } else if cfg.source == Some(tui::config::ConfigSource::NotFound) && args.api_base.is_none()
        {
            // No config at all — run the interactive setup wizard.
            if !setup::run_initial_setup(&cfg.path).await {
                std::process::exit(1);
            }
            cfg = tui::config::Config::load_from(&cfg.path);
            let models = cfg.resolve_models();
            if let Some(r) = models.first() {
                let key = std::env::var(&r.api_key_env).unwrap_or_default();
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
            let key = std::env::var(&key_env).unwrap_or_default();
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

    if args.subagent {
        if args.message.is_none() {
            eprintln!("error: --subagent requires a message argument");
            std::process::exit(1);
        }
        if args.parent_pid.is_none() || args.depth.is_none() {
            eprintln!("error: --subagent requires --parent-pid and --depth");
            std::process::exit(1);
        }
    }

    // Resolve multi-agent: CLI flags override config.
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

    let vim_enabled = cfg.settings.vim_mode.unwrap_or(false);
    let auto_compact = args.subagent || args.headless || cfg.settings.auto_compact.unwrap_or(false);
    let show_tps = cfg.settings.show_tps.unwrap_or(true);
    let show_tokens = cfg.settings.show_tokens.unwrap_or(true);
    let show_cost = cfg.settings.show_cost.unwrap_or(true);
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

    let mode_cycle = args
        .mode_cycle
        .as_deref()
        .or(cfg.defaults.mode_cycle.as_deref())
        .map(Mode::parse_list)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| Mode::ALL.to_vec());

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
    let headless_cancel = Arc::new(tokio::sync::Notify::new());

    // Signal handler for graceful shutdown
    {
        let shared = shared_session.clone();
        let is_headless = args.headless;
        let headless_cancel = headless_cancel.clone();
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
            if is_headless {
                // Notify run_headless to break out of the event loop so it
                // can print the token summary before exiting.
                headless_cancel.notify_one();
                return;
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
            if let Some(id) = session_id {
                tui::session::print_resume_hint(&id);
            }
            // Kill child agents on shutdown.
            if multi_agent {
                engine::registry::cleanup_self(std::process::id());
            }
            std::process::exit(0);
        });
    }

    // Load instructions and workspace
    let cwd = std::env::current_dir().unwrap_or_default();
    let instructions = if args.no_system_prompt {
        None
    } else {
        tui::instructions::load()
    };
    let system_prompt_override = if args.no_system_prompt {
        Some(String::new())
    } else {
        args.system_prompt.map(|s| {
            let path = std::path::Path::new(&s);
            if path.is_file() {
                std::fs::read_to_string(path).unwrap_or_else(|e| {
                    eprintln!(
                        "error: failed to read system prompt file {}: {e}",
                        path.display()
                    );
                    std::process::exit(1);
                })
            } else {
                s
            }
        })
    };

    // Start the engine
    let workspace = engine::paths::git_root(&cwd).unwrap_or_else(|| cwd.clone());
    let mut permissions = engine::Permissions::load();
    permissions.set_workspace(workspace);
    permissions.set_restrict_to_workspace(restrict_to_workspace);
    let permissions = Arc::new(permissions);
    let initial_api_base = api_base.clone();
    let initial_provider_type = provider_type.clone();
    let engine_injector;
    // Pick the interactive root agent ID once and share it across
    // engine tools + registry registration to avoid identity drift.
    let planned_agent_id = if multi_agent && !args.subagent {
        Some(engine::registry::next_agent_id())
    } else {
        None
    };

    // Create shared runtime approvals and load workspace rules.
    let runtime_approvals = {
        let cwd_str = cwd.to_string_lossy();
        let rules = tui::workspace_permissions::load(&cwd_str);
        let (ws_tools, ws_dirs) = tui::workspace_permissions::into_approvals(&rules);
        let mut rt = engine::permissions::RuntimeApprovals::new();
        rt.load_workspace(ws_tools, ws_dirs);
        Arc::new(std::sync::RwLock::new(rt))
    };

    let engine_handle = engine::start(engine::EngineConfig {
        api: engine::ApiConfig {
            base: api_base,
            key: api_key,
            key_env: api_key_env.clone(),
            provider_type,
            model_config: (&model_config).into(),
        },
        instructions,
        system_prompt_override,
        cwd: cwd.clone(),
        permissions: permissions.clone(),
        runtime_approvals: runtime_approvals.clone(),
        multi_agent: if multi_agent {
            Some(engine::MultiAgentConfig {
                depth: args.depth.unwrap_or(0),
                max_depth: args.max_agent_depth,
                max_agents: args.max_agents,
                parent_pid: args.parent_pid,
                agent_id: planned_agent_id.clone(),
            })
        } else {
            None
        },
        interactive: !args.headless && !args.subagent,
        mcp_servers: cfg.mcp.clone(),
        skills: {
            let extra_paths: Vec<std::path::PathBuf> = cfg
                .skills
                .paths
                .iter()
                .map(std::path::PathBuf::from)
                .collect();
            let loader = engine::SkillLoader::load(&extra_paths);
            Some(Arc::new(loader))
        },
        auto_compact,
        context_window: cfg.settings.context_window,
    });
    engine_injector = engine_handle.injector();

    // Fetch context window in background (only needed for interactive TUI display).
    // If the user set it in config, skip the fetch entirely.
    let ctx_rx = if !args.headless && cfg.settings.context_window.is_none() {
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
        show_tps,
        show_tokens,
        show_cost,
        input_prediction,
        task_slug,
        restrict_to_workspace,
        multi_agent,
        reasoning_effort,
        reasoning_cycle,
        mode_cycle,
        shared_session,
        available_models,
        args.model.is_some(),
        args.api_base.is_some(),
        args.api_key_env.is_some(),
    );
    app.model_config = (&model_config).into();
    if let Some(mode) = mode_override {
        app.mode = mode;
    }
    if !app.mode_cycle.contains(&app.mode) {
        app.mode_cycle.push(app.mode);
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

    if args.subagent {
        let parent_pid = args.parent_pid.unwrap();
        let depth = args.depth.unwrap();
        let my_pid = std::process::id();

        // Request SIGTERM when parent dies (Linux only).
        #[cfg(target_os = "linux")]
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            // Check if parent already died between our fork and prctl.
            if !engine::registry::is_pid_alive(parent_pid) {
                std::process::exit(1);
            }
        }

        // Start socket listener.
        let (socket_path, socket_rx) =
            engine::socket::start_listener(my_pid).expect("failed to start agent socket");

        // Detect scope for registry.
        let scope = engine::paths::git_root(&cwd)
            .unwrap_or_else(|| cwd.clone())
            .to_string_lossy()
            .into_owned();

        // Register in the agent registry (update the pre-registered entry).
        let branch = engine::paths::git_branch(&cwd);
        let agent_id = engine::registry::read_entry(my_pid)
            .ok()
            .map(|e| e.agent_id)
            .unwrap_or_else(|| format!("agent-{my_pid}"));
        engine::registry::register(&engine::registry::RegistryEntry {
            agent_id,
            pid: my_pid,
            parent_pid: Some(parent_pid),
            git_root: Some(scope.clone()),
            git_branch: branch,
            cwd: cwd.to_string_lossy().into_owned(),
            status: engine::registry::AgentStatus::Idle,
            task_slug: None,
            session_id: app.session.id.clone(),
            socket_path: socket_path.to_string_lossy().into_owned(),
            depth,
            started_at: timestamp_now(),
        })
        .expect("failed to register agent");

        app.run_subagent(args.message.unwrap(), parent_pid, socket_rx)
            .await;

        engine::registry::cleanup_self(my_pid);
    } else if args.headless {
        let output_format = match args.format {
            OutputFormat::Text => tui::app::OutputFormat::Text,
            OutputFormat::Json => tui::app::OutputFormat::Json,
        };
        let color_mode = match args.color {
            ColorMode::Auto => tui::app::ColorMode::Auto,
            ColorMode::Always => tui::app::ColorMode::Always,
            ColorMode::Never => tui::app::ColorMode::Never,
        };
        app.run_headless(
            args.message.unwrap(),
            output_format,
            color_mode,
            args.verbose,
            headless_cancel,
        )
        .await;
    } else {
        // Redirect stderr to a log file so stray output from system processes
        // (e.g. polkit, PAM) or libraries doesn't corrupt the TUI display.
        redirect_stderr();

        // Interactive mode: register if multi-agent is enabled.
        if multi_agent {
            let my_pid = std::process::id();
            let scope = engine::paths::git_root(&cwd)
                .unwrap_or_else(|| cwd.clone())
                .to_string_lossy()
                .into_owned();
            let branch = engine::paths::git_branch(&cwd);

            let (socket_path, socket_rx) =
                engine::socket::start_listener(my_pid).expect("failed to start agent socket");

            // Bridge socket messages to the engine + child permission channel.
            let (child_perm_tx, child_perm_rx) = tokio::sync::mpsc::unbounded_channel();
            spawn_socket_bridge(socket_rx, engine_injector.clone(), child_perm_tx);
            app.set_child_permission_rx(child_perm_rx);

            let my_agent_id = planned_agent_id
                .clone()
                .unwrap_or_else(engine::registry::next_agent_id);
            app.agent_id = my_agent_id.clone();
            if let Err(e) = engine::registry::register(&engine::registry::RegistryEntry {
                agent_id: my_agent_id,
                pid: my_pid,
                parent_pid: None,
                git_root: Some(scope),
                git_branch: branch,
                cwd: cwd.to_string_lossy().into_owned(),
                status: engine::registry::AgentStatus::Idle,
                task_slug: None,
                session_id: app.session.id.clone(),
                socket_path: socket_path.to_string_lossy().into_owned(),
                depth: 0,
                started_at: timestamp_now(),
            }) {
                eprintln!("warning: failed to register in agent registry: {e}");
            }

            // Prune dead entries on startup.
            engine::registry::prune_dead();

            // Watch for child agent deaths.
            spawn_child_watcher(my_pid, engine_injector.clone());
        }

        println!();
        app.run(ctx_rx, args.message).await;
        if !app.session.messages.is_empty() {
            tui::session::print_resume_hint(&app.session.id);
        }

        if multi_agent {
            engine::registry::cleanup_self(std::process::id());
        }
    }
    tui::perf::print_summary();
}

/// Redirect stderr (fd 2) to a file in the logs directory so that any stray
/// output from system daemons, libraries, or child processes doesn't pollute
/// the TUI display.
fn redirect_stderr() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let dir = engine::log::logs_dir();
        let path = dir.join("stderr.log");
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let file_fd = file.as_raw_fd();
            // dup2 the log file onto fd 2 (stderr).
            // SAFETY: both fds are valid open file descriptors.
            unsafe {
                libc::dup2(file_fd, 2);
            }
            // `file` is dropped here but fd 2 now points to the same open file
            // description, so it stays open.
        }
    }
}

fn spawn_socket_bridge(
    mut socket_rx: tokio::sync::mpsc::UnboundedReceiver<engine::socket::IncomingMessage>,
    injector: engine::EventInjector,
    child_perm_tx: tokio::sync::mpsc::UnboundedSender<engine::socket::IncomingMessage>,
) {
    tokio::spawn(async move {
        while let Some(msg) = socket_rx.recv().await {
            match msg {
                engine::socket::IncomingMessage::Message {
                    from_id,
                    from_slug,
                    message,
                } => {
                    injector.inject_agent_message(from_id, from_slug, message);
                }
                engine::socket::IncomingMessage::Query { reply_tx, .. } => {
                    let _ = reply_tx.send(
                        "agent is in interactive mode and cannot serve queries at this time".into(),
                    );
                }
                perm @ engine::socket::IncomingMessage::PermissionCheck { .. } => {
                    let _ = child_perm_tx.send(perm);
                }
            }
        }
    });
}

fn spawn_child_watcher(parent_pid: u32, injector: engine::EventInjector) {
    tokio::spawn(async move {
        let mut known: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let children = engine::registry::children_of(parent_pid);
            let current: std::collections::HashSet<u32> = children.iter().map(|c| c.pid).collect();

            for (pid, agent_id) in &known {
                if !current.contains(pid) {
                    injector.inject_agent_exited(agent_id.clone(), None);
                }
            }

            known = children.into_iter().map(|c| (c.pid, c.agent_id)).collect();
        }
    });
}

fn timestamp_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}
