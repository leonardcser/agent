mod agent;
mod app;
pub mod completer;
mod config;
pub mod input;
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
use provider::Provider;
use session::Session;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "agent", about = "Coding agent TUI")]
struct Args {
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    api_base: Option<String>,
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "info", value_name = "LEVEL")]
    log_level: String,
    #[arg(long, help = "Print performance timing summary on exit")]
    bench: bool,
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

    let provider_name = args.provider.or(cfg.default_provider);
    let provider_cfg = provider_name
        .as_deref()
        .and_then(|name| cfg.providers.get(name))
        .or_else(|| cfg.providers.values().next())
        .cloned()
        .unwrap_or_default();

    let api_base = args
        .api_base
        .or(provider_cfg.api_base)
        .unwrap_or_else(|| "http://localhost:11434/v1".into());
    let api_key_env = args
        .api_key_env
        .or(provider_cfg.api_key_env)
        .unwrap_or_default();
    let api_key = std::env::var(&api_key_env).unwrap_or_default();
    let model = args
        .model
        .or(provider_cfg.model)
        .expect("model must be set via --model or config file");

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
            std::process::exit(0);
        });
    }

    let mut app = App::new(
        api_base,
        api_key,
        model,
        vim_enabled,
        auto_compact,
        shared_session,
    );

    // Fetch context window in background so startup isn't blocked by the network call.
    let ctx_rx = {
        let provider = Provider::new(
            app.api_base.clone(),
            app.api_key.clone(),
            app.client.clone(),
        );
        let model = app.model.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(provider.fetch_context_window(&model).await);
        });
        Some(rx)
    };

    println!();
    app.run(ctx_rx).await;
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
