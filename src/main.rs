mod agent;
mod app;
pub mod completer;
mod config;
pub mod input;
mod log;
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
use provider::Provider;
use session::Session;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "agent", about = "Coding agent TUI")]
struct Args {
    #[arg(long)]
    api_base: Option<String>,
    #[arg(long)]
    api_key: Option<String>,
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value = "info", value_name = "LEVEL")]
    log_level: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let cfg = config::Config::load();

    let api_base = args.api_base
        .or(cfg.api_base)
        .unwrap_or_else(|| "http://localhost:11434/v1".into());
    let api_key_env = args.api_key_env.or(cfg.api_key_env).unwrap_or_default();
    let api_key = args.api_key
        .or(cfg.api_key)
        .unwrap_or_else(|| std::env::var(&api_key_env).unwrap_or_default());
    let model = args.model
        .or(cfg.model)
        .expect("model must be set via --model or config file");

    if let Some(level) = log::parse_level(&args.log_level) {
        log::set_level(level);
    } else {
        eprintln!("warning: invalid --log-level {}, defaulting to info", args.log_level);
    }

    let vim_enabled = cfg.vim_mode.unwrap_or(false);
    let auto_compact = cfg.auto_compact.unwrap_or(false);
    let shared_session: Arc<Mutex<Option<Session>>> = Arc::new(Mutex::new(None));

    {
        let shared = shared_session.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
                let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
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
            std::process::exit(0);
        });
    }

    let mut app = App::new(api_base, api_key, model, vim_enabled, auto_compact, shared_session);

    // Fetch context window once at startup. Re-fetch here if model switching is ever added.
    {
        let provider = Provider::new(&app.api_base, &app.api_key);
        app.context_window = provider.fetch_context_window(&app.model).await;
    }

    println!();
    loop {
        let input = if !app.queued_messages.is_empty() {
            let mut parts = std::mem::take(&mut app.queued_messages);
            let buf = std::mem::take(&mut app.input.buf);
            app.input.cpos = 0;
            if !buf.trim().is_empty() {
                parts.push(buf);
            }
            parts.join("\n")
        } else {
            match app.read_input() {
                Some(s) => s,
                None => break,
            }
        };
        app.app_state.set_mode(app.mode);

        let input = input.trim().to_string();
        if input.is_empty() { continue; }

        // Handle settings close signal: \x00settings:{json}
        if let Some(json) = input.strip_prefix("\x00settings:") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
                let vim_val = v["vim"].as_bool().unwrap_or(app.input.vim_enabled());
                let ac_val = v["auto_compact"].as_bool().unwrap_or(app.auto_compact);
                app.input.set_vim_enabled(vim_val);
                app.app_state.set_vim_enabled(vim_val);
                app.auto_compact = ac_val;
            }
            continue;
        }

        // Handle rewind signal from double-Esc menu
        if let Some(idx_str) = input.strip_prefix("\x00rewind:") {
            if let Ok(block_idx) = idx_str.parse::<usize>() {
                if let Some(text) = app.rewind_to(block_idx) {
                    app.input.buf = text;
                    app.input.cpos = app.input.buf.len();
                }
            }
            continue;
        }

        app.input_history.push(input.clone());
        if !app.handle_command(&input) { break; }
        if input == "/compact" {
            app.compact_history().await;
            continue;
        }
        if input.starts_with('/') { continue; }

        if let Some(cmd) = input.strip_prefix('!') {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                let output = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .output()
                    .map(|o| {
                        let mut s = String::from_utf8_lossy(&o.stdout).to_string();
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        if !stderr.is_empty() {
                            if !s.is_empty() { s.push('\n'); }
                            s.push_str(&stderr);
                        }
                        s.truncate(s.trim_end().len());
                        s
                    })
                    .unwrap_or_else(|e| format!("error: {}", e));
                app.screen.push(render::Block::Exec { command: cmd.to_string(), output });
            }
            continue;
        }

        app.screen.begin_turn();
        app.show_user_message(&input);
        if app.session.first_user_message.is_none() {
            app.session.first_user_message = Some(input.clone());
        }
        app.push_user_message(input);
        app.save_session();
        app.run_session().await;
        app.save_session();
        // Title first: uses original history before compaction may truncate it.
        app.maybe_generate_title().await;
        app.maybe_auto_compact().await;
    }
    app.save_session();
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
            result.push_str(&format!("\n\nContents of {}:\n```\n{}\n```", path, contents));
        }
    }
    result
}
