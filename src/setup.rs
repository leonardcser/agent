//! Interactive setup flows: first-run wizard and `smelt auth` subcommand.
//!
//! Provider login/logout goes through `engine::auth`; YAML manipulation
//! goes through `engine::config_file`.

use dialoguer::{Input, Select};
use engine::auth::{AuthProvider, LoginMethod, LoginProgress};
use std::path::Path;

// ── Provider templates ─────────────────────────────────────────────────────

struct ProviderTemplate {
    name: &'static str,
    label: &'static str,
    provider_type: &'static str,
    api_base: &'static str,
    api_key_env: &'static str,
    default_model: &'static str,
    needs_api_base: bool,
    /// Authentication kind for providers that require OAuth. `None` means
    /// the provider uses a bearer API key from an env var.
    oauth: Option<AuthProvider>,
}

const PROVIDERS: &[ProviderTemplate] = &[
    ProviderTemplate {
        name: "openai",
        label: "OpenAI (API key)",
        provider_type: "openai",
        api_base: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
        default_model: "gpt-4.1",
        needs_api_base: false,
        oauth: None,
    },
    ProviderTemplate {
        name: "codex",
        label: "OpenAI Codex (ChatGPT subscription)",
        provider_type: "codex",
        api_base: "https://chatgpt.com/backend-api/codex",
        api_key_env: "",
        default_model: "gpt-5.4",
        needs_api_base: false,
        oauth: Some(AuthProvider::Codex),
    },
    ProviderTemplate {
        name: "anthropic",
        label: "Anthropic (Claude)",
        provider_type: "anthropic",
        api_base: "https://api.anthropic.com/v1",
        api_key_env: "ANTHROPIC_API_KEY",
        default_model: "claude-sonnet-4-20250514",
        needs_api_base: false,
        oauth: None,
    },
    ProviderTemplate {
        name: "copilot",
        label: "GitHub Copilot (subscription)",
        provider_type: "copilot",
        api_base: "https://api.individual.githubcopilot.com",
        api_key_env: "",
        default_model: "",
        needs_api_base: false,
        oauth: Some(AuthProvider::Copilot),
    },
    ProviderTemplate {
        name: "custom",
        label: "Other (OpenAI-compatible)",
        provider_type: "openai-compatible",
        api_base: "",
        api_key_env: "",
        default_model: "",
        needs_api_base: true,
        oauth: None,
    },
];

// ── Interactive prompts ────────────────────────────────────────────────────

fn pick_provider() -> Option<usize> {
    let labels: Vec<&str> = PROVIDERS.iter().map(|p| p.label).collect();
    Select::new()
        .with_prompt("Select a provider")
        .items(&labels)
        .default(0)
        .interact()
        .ok()
}

fn collect_provider(tmpl: &ProviderTemplate) -> Option<engine::config_file::NewProvider> {
    let api_base = if tmpl.needs_api_base {
        Input::<String>::new()
            .with_prompt("API base URL")
            .interact_text()
            .ok()?
    } else {
        tmpl.api_base.to_string()
    };

    let api_key_env = if tmpl.oauth.is_some() {
        None
    } else if tmpl.api_key_env.is_empty() {
        Some(
            Input::<String>::new()
                .with_prompt("API key environment variable")
                .interact_text()
                .ok()?,
        )
    } else {
        Some(
            Input::new()
                .with_prompt("API key environment variable")
                .default(tmpl.api_key_env.to_string())
                .interact_text()
                .ok()?,
        )
    };

    let model: String = if tmpl.default_model.is_empty() {
        Input::new().with_prompt("Model").interact_text().ok()?
    } else {
        Input::new()
            .with_prompt("Model")
            .default(tmpl.default_model.to_string())
            .interact_text()
            .ok()?
    };

    if model.is_empty() {
        eprintln!("error: model name is required");
        return None;
    }

    let name = if tmpl.name == "custom" {
        Input::new()
            .with_prompt("Provider name (short label)")
            .default("custom".to_string())
            .interact_text()
            .ok()?
    } else {
        tmpl.name.to_string()
    };

    Some(engine::config_file::NewProvider {
        name,
        provider_type: tmpl.provider_type.to_string(),
        api_base,
        api_key_env,
        models: vec![model],
    })
}

// ── OAuth flows ───────────────────────────────────────────────────────────

async fn run_login(kind: AuthProvider) {
    let method = match kind {
        AuthProvider::Codex => {
            let methods = &["Browser (opens a window)", "Device code (headless / SSH)"];
            let choice = Select::new()
                .with_prompt("Login method")
                .items(methods)
                .default(0)
                .interact()
                .unwrap_or(0);
            if choice == 1 {
                LoginMethod::DeviceCode
            } else {
                println!("\nOpening browser for authorization...\n");
                LoginMethod::Browser
            }
        }
        AuthProvider::Copilot => {
            println!("\n  Starting GitHub device-code login…\n");
            LoginMethod::DeviceCode
        }
    };

    let on_prompt = |url: &str, code: &str| {
        println!("  Open this URL in a browser:\n\n    {url}\n");
        if !code.is_empty() {
            println!("  Then enter code: {code}\n");
        }
    };
    let on_message = |msg: &str| println!("  {msg}");
    let progress = LoginProgress {
        on_prompt: &on_prompt,
        on_message: &on_message,
    };

    let client = reqwest::Client::new();
    match engine::auth::login(kind, method, &client, &progress).await {
        Ok(details) => {
            println!("\nLogged in successfully!");
            if let Some(id) = details.account_id {
                println!("Account ID: {id}");
            }
            if let (Some(base), Some(exp)) = (details.api_base, details.expires_at) {
                println!("API base: {base}\nToken expires at: {exp}");
            }
        }
        Err(e) => {
            eprintln!("\nLogin failed: {e}");
            std::process::exit(1);
        }
    }
}

fn run_logout(kind: AuthProvider, label: &str) {
    engine::auth::logout(kind);
    println!("\nLogged out of {label}.");
}

// ── Public entry points ────────────────────────────────────────────────────

/// First-time setup wizard. Returns true if config was written.
pub async fn run_initial_setup(config_path: &Path) -> bool {
    println!("\n  Welcome to smelt! No configuration found.\n");

    let Some(idx) = pick_provider() else {
        return false;
    };
    let tmpl = &PROVIDERS[idx];

    if let Some(kind) = tmpl.oauth {
        run_login(kind).await;
        println!("Provider auto-detected from credentials — no config file needed.");
        return true;
    }

    let Some(provider) = collect_provider(tmpl) else {
        return false;
    };

    match engine::config_file::write_initial_config(config_path, &provider) {
        Ok(()) => {
            println!("Config written to {}", config_path.display());
            true
        }
        Err(e) => {
            eprintln!("error: {e}");
            false
        }
    }
}

/// `smelt auth` — provider picker, then provider-specific flow.
pub async fn run_auth_command() {
    let Some(idx) = pick_provider() else {
        return;
    };
    let tmpl = &PROVIDERS[idx];

    if let Some(kind) = tmpl.oauth {
        let options = &["Log in", "Log out"];
        let Ok(choice) = Select::new()
            .with_prompt(tmpl.label)
            .items(options)
            .default(0)
            .interact()
        else {
            return;
        };
        match choice {
            0 => run_login(kind).await,
            1 => run_logout(kind, tmpl.label),
            _ => {}
        }
    } else {
        let Some(provider) = collect_provider(tmpl) else {
            return;
        };
        match engine::config_file::add_provider(&provider) {
            Ok(()) => println!("Provider '{}' added.", provider.name),
            Err(e) => eprintln!("error: {e}"),
        }
    }
}
