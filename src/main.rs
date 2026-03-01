mod agent;
mod app;
pub mod completer;
mod config;
pub mod input;
mod instructions;
mod log;
pub mod perf;
mod permissions;
mod provider;
pub mod render;
mod session;
mod state;
mod theme;
mod tools;
pub mod vim;

pub use app::App;

use clap::Parser;
use crossterm::ExecutableCommand;
use session::Session;
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
    let cfg = config::Config::load();
    let app_state = state::State::load();
    let available_models = cfg.resolve_models();

    // Resolve the active model: CLI flags > cached selection > default_model > first in config
    let (api_base, api_key, model, model_config) = {
        let resolved = if let Some(ref cli_model) = args.model {
            available_models
                .iter()
                .find(|m| m.model_name == *cli_model || m.key == *cli_model)
        } else if let Some(ref cached) = app_state.selected_model {
            available_models.iter().find(|m| m.key == *cached)
        } else if let Some(ref default) = cfg.default_model {
            available_models
                .iter()
                .find(|m| m.key == *default || m.model_name == *default)
        } else {
            available_models.first()
        };

        if let Some(r) = resolved {
            let base = args.api_base.clone().unwrap_or_else(|| r.api_base.clone());
            let key_env = args
                .api_key_env
                .clone()
                .unwrap_or_else(|| r.api_key_env.clone());
            let key = std::env::var(&key_env).unwrap_or_default();
            (base, key, r.model_name.clone(), r.config.clone())
        } else {
            // Fallback: pure CLI flags, no config providers
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
            (base, key, model, config::ModelConfig::default())
        }
    };

    if let Some(level) = log::parse_level(&args.log_level) {
        log::set_level(level);
    } else {
        eprintln!(
            "warning: invalid --log-level {}, defaulting to info",
            args.log_level
        );
    }

    if args.bench {
        perf::enable();
    }

    if args.headless && args.message.is_none() {
        eprintln!("error: --headless requires a message argument");
        std::process::exit(1);
    }

    let mode_override = args.mode.as_deref().map(|s| {
        input::Mode::parse(s).unwrap_or_else(|| {
            eprintln!("warning: unknown --mode '{s}', defaulting to normal");
            input::Mode::Normal
        })
    });

    let vim_enabled = cfg.settings.vim_mode.unwrap_or(false);
    let auto_compact = cfg.settings.auto_compact.unwrap_or(false);
    let shared_session: Arc<Mutex<Option<Session>>> = Arc::new(Mutex::new(None));

    {
        let shared = shared_session.clone();
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
            if let Ok(guard) = shared.lock() {
                if let Some(ref s) = *guard {
                    session::save(s);
                }
            }
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::io::stdout().execute(crossterm::event::DisableBracketedPaste);
            println!();
            std::process::exit(0);
        });
    }

    let mut app = App::new(
        api_base,
        api_key,
        model,
        model_config,
        vim_enabled,
        auto_compact,
        shared_session,
        available_models,
    );
    if let Some(mode) = mode_override {
        app.mode = mode;
    }

    // Fetch context window in background so startup isn't blocked by the network call.
    let ctx_rx = {
        let provider = app.build_provider();
        let model = app.model.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(provider.fetch_context_window(&model).await);
        });
        Some(rx)
    };

    if args.headless {
        app.run_headless(args.message.unwrap()).await;
    } else {
        println!();
        app.run(ctx_rx, args.message).await;
    }
    perf::print_summary();
}

/// Expand `@path` references in user input by appending file contents.
pub fn expand_at_refs(input: &str) -> String {
    let mut refs: Vec<String> = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '@' {
            continue;
        }
        // Collect non-whitespace chars after @
        let start = i + 1;
        let mut end = start;
        while let Some(&(j, nc)) = chars.peek() {
            if nc.is_whitespace() {
                break;
            }
            end = j + nc.len_utf8();
            chars.next();
        }
        if end > start {
            let path = &input[start..end];
            if std::path::Path::new(path).exists() {
                refs.push(path.to_string());
            }
        }
    }

    if refs.is_empty() {
        return input.to_string();
    }

    let mut result = input.to_string();
    for path in &refs {
        if let Ok(contents) = std::fs::read_to_string(path) {
            result.push_str(&format!(
                "\n\nContents of {}:\n```\n{}\n```",
                path, contents
            ));
        }
    }
    result
}
