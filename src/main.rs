mod setup;
mod startup;

use clap::{Parser, Subcommand, ValueEnum};
use crossterm::ExecutableCommand;
use startup::resolve_api_key;
use std::sync::{Arc, Mutex};

#[global_allocator]
static ALLOCATOR: tui::alloc::Counting = tui::alloc::Counting;

#[derive(Parser)]
#[command(name = "smelt", about = "Coding agent TUI", version)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
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
        help = "Provider type: openai-compatible, openai, anthropic, codex, copilot"
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
    /// Manage provider authentication (add providers, Codex or Copilot login/logout)
    Auth,
}

#[tokio::main]
async fn main() {
    std::panic::set_hook(Box::new(|info| {
        let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
        let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
        let _ = std::io::stdout().execute(crossterm::event::DisableFocusChange);
        let _ = std::io::stdout().execute(crossterm::cursor::Show);
        eprintln!("{info}");
    }));

    let mut args = Args::parse();

    // Handle subcommands before loading config.
    if let Some(Commands::Auth) = args.command {
        setup::run_auth_command().await;
        return;
    }

    let s = startup::resolve(&args).await;
    let startup::ResolvedStartup {
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
        mut startup_auth_error,
    } = s;

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
        tui::alloc::enable();
    }

    // Eager-load syntect's syntax and theme sets in the background so the
    // first tool render doesn't pay the ~30ms lazy-init cost mid-frame.
    // Runs in parallel with session loading and is done well before first paint.
    std::thread::spawn(tui::content::warm_up_syntect);

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

    if (args.headless || args.subagent) && startup_auth_error.is_some() {
        eprintln!(
            "error: {}",
            startup_auth_error.as_deref().unwrap_or_default()
        );
        std::process::exit(1);
    }

    // Parse theme accent from config.
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
            let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
            let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
            let _ = std::io::stdout().execute(crossterm::event::DisableFocusChange);
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

    // Load instructions and workspace.
    let cwd = std::env::current_dir().unwrap_or_default();
    let instructions = if args.no_system_prompt {
        None
    } else {
        tui::instructions::load()
    };
    let system_prompt_override = if args.no_system_prompt {
        Some(String::new())
    } else {
        args.system_prompt.take().map(|s| {
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

    // Start the engine.
    let workspace = engine::paths::git_root(&cwd).unwrap_or_else(|| cwd.clone());
    let mut permissions = engine::Permissions::load();
    permissions.set_workspace(workspace);
    permissions.set_restrict_to_workspace(settings.restrict_to_workspace);
    let permissions = Arc::new(permissions);
    let initial_api_base = api_base.clone();
    let initial_provider_type = provider_type.clone();

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

    let skill_loader = {
        let extra_paths: Vec<std::path::PathBuf> = cfg
            .skills
            .paths
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        Arc::new(engine::SkillLoader::load(&extra_paths))
    };
    let tui_skill_section = skill_loader.prompt_section().map(String::from);
    let tui_instructions = instructions.clone();

    let engine_handle = engine::start(engine::EngineConfig {
        api: engine::ApiConfig {
            base: api_base,
            key: api_key,
            key_env: api_key_env.clone(),
            provider_type,
            model_config: (&model_config).into(),
        },
        model: model.clone(),
        auxiliary,
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
        skills: Some(skill_loader),
        auto_compact: settings.auto_compact,
        context_window: cfg.settings.context_window,
        redact_secrets: settings.redact_secrets,
    });
    let engine_injector = engine_handle.injector();

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
            .and_then(|env| resolve_api_key(env).ok())
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

    // Build the TUI app.
    let mut app = tui::app::App::new(
        model,
        initial_api_base,
        api_key_env,
        initial_provider_type,
        Arc::clone(&permissions),
        engine_handle,
        settings,
        multi_agent,
        reasoning_effort,
        reasoning_cycle,
        mode_cycle,
        shared_session,
        available_models,
        args.model.is_some(),
        args.api_base.is_some(),
        args.api_key_env.is_some(),
        startup_auth_error.take(),
    );
    app.model_config = (&model_config).into();
    app.extra_instructions = tui_instructions;
    app.skill_section = tui_skill_section;
    if let Some(mode) = mode_override {
        app.mode = mode;
    }
    if !app.mode_cycle.contains(&app.mode) {
        app.mode_cycle.push(app.mode);
    }

    if let Some(ref resume_val) = args.resume {
        if resume_val.is_empty() {
            // Open the resume dialog inside `run()` so dismissal goes
            // through the normal dialog lifecycle (clear_dialog_area).
            args.message = Some("/resume".to_string());
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
