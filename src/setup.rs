//! Interactive setup flows: first-run wizard and `smelt auth` subcommand.
//!
//! Config manipulation is delegated to `engine::config_file`.

use dialoguer::{Input, Select};
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
    },
    ProviderTemplate {
        name: "codex",
        label: "OpenAI Codex (ChatGPT subscription)",
        provider_type: "codex",
        api_base: "https://chatgpt.com/backend-api/codex",
        api_key_env: "",
        default_model: "gpt-5.4",
        needs_api_base: false,
    },
    ProviderTemplate {
        name: "anthropic",
        label: "Anthropic (Claude)",
        provider_type: "anthropic",
        api_base: "https://api.anthropic.com/v1",
        api_key_env: "ANTHROPIC_API_KEY",
        default_model: "claude-sonnet-4-20250514",
        needs_api_base: false,
    },
    ProviderTemplate {
        name: "copilot",
        label: "GitHub Copilot (subscription)",
        provider_type: "copilot",
        api_base: "https://api.individual.githubcopilot.com",
        api_key_env: "",
        default_model: "",
        needs_api_base: false,
    },
    ProviderTemplate {
        name: "custom",
        label: "Other (OpenAI-compatible)",
        provider_type: "openai-compatible",
        api_base: "",
        api_key_env: "",
        default_model: "",
        needs_api_base: true,
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

    let api_key_env = if tmpl.provider_type == "codex" || tmpl.provider_type == "copilot" {
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

// ── Codex login/logout ─────────────────────────────────────────────────────

async fn codex_login() {
    let methods = &["Browser (opens a window)", "Device code (headless / SSH)"];
    let choice = Select::new()
        .with_prompt("Login method")
        .items(methods)
        .default(0)
        .interact()
        .unwrap_or(0);

    let client = reqwest::Client::new();
    let result = if choice == 1 {
        engine::provider::codex::device_code_login(&client).await
    } else {
        println!("\nOpening browser for authorization...\n");
        engine::provider::codex::browser_login(&client).await
    };

    match result {
        Ok(tokens) => {
            println!("Logged in successfully!");
            if let Some(id) = &tokens.account_id {
                println!("Account ID: {id}");
            }
        }
        Err(e) => {
            eprintln!("\nLogin failed: {e}");
            std::process::exit(1);
        }
    }
}

fn codex_logout() {
    engine::provider::codex::CodexTokens::delete();
    println!("\nLogged out of Codex.");
}

fn codex_new_provider(tmpl: &ProviderTemplate) -> engine::config_file::NewProvider {
    engine::config_file::NewProvider {
        name: tmpl.name.to_string(),
        provider_type: tmpl.provider_type.to_string(),
        api_base: tmpl.api_base.to_string(),
        api_key_env: None,
        models: vec![],
    }
}

fn ensure_codex_provider(tmpl: &ProviderTemplate) {
    let provider = codex_new_provider(tmpl);
    match engine::config_file::ensure_provider(&provider) {
        Ok(true) => println!("Added codex provider to config."),
        Ok(false) => {}
        Err(e) => eprintln!("error: {e}"),
    }
}

// ── Copilot login/logout ───────────────────────────────────────────────────

async fn copilot_login() {
    println!("\n  Starting GitHub device-code login…\n");

    let client = reqwest::Client::new();
    let on_prompt = |url: &str, code: &str| {
        println!("  Open this URL in a browser:\n");
        println!("    {url}\n");
        println!("  Then enter code: {code}\n");
    };
    let on_progress = |msg: &str| {
        println!("  {msg}");
    };

    let callbacks = engine::provider::copilot::LoginCallbacks {
        on_prompt: &on_prompt,
        on_progress: &on_progress,
    };

    match engine::provider::copilot::device_code_login(&client, &callbacks).await {
        Ok(tokens) => {
            println!("\nLogged in to GitHub Copilot.");
            println!(
                "API base: {}\nToken expires at: {}",
                tokens.api_base, tokens.expires_at
            );
        }
        Err(e) => {
            eprintln!("\nLogin failed: {e}");
            std::process::exit(1);
        }
    }
}

fn copilot_logout() {
    engine::provider::copilot::CopilotTokens::delete();
    println!("\nLogged out of GitHub Copilot.");
}

fn copilot_new_provider(tmpl: &ProviderTemplate) -> engine::config_file::NewProvider {
    engine::config_file::NewProvider {
        name: tmpl.name.to_string(),
        provider_type: tmpl.provider_type.to_string(),
        api_base: tmpl.api_base.to_string(),
        api_key_env: None,
        models: vec![],
    }
}

fn ensure_copilot_provider(tmpl: &ProviderTemplate) {
    let provider = copilot_new_provider(tmpl);
    match engine::config_file::ensure_provider(&provider) {
        Ok(true) => println!("Added copilot provider to config."),
        Ok(false) => {}
        Err(e) => eprintln!("error: {e}"),
    }
}

// ── Public entry points ────────────────────────────────────────────────────

/// First-time setup wizard. Returns true if config was written.
pub async fn run_initial_setup(config_path: &Path) -> bool {
    println!("\n  Welcome to smelt! No configuration found.\n");

    let Some(idx) = pick_provider() else {
        return false;
    };
    let tmpl = &PROVIDERS[idx];

    let provider = if tmpl.provider_type == "codex" {
        codex_login().await;
        codex_new_provider(tmpl)
    } else if tmpl.provider_type == "copilot" {
        copilot_login().await;
        copilot_new_provider(tmpl)
    } else {
        let Some(p) = collect_provider(tmpl) else {
            return false;
        };
        p
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

    if tmpl.provider_type == "codex" {
        let options = &["Log in", "Log out"];
        let Ok(choice) = Select::new()
            .with_prompt("OpenAI Codex")
            .items(options)
            .default(0)
            .interact()
        else {
            return;
        };
        match choice {
            0 => {
                codex_login().await;
                ensure_codex_provider(tmpl);
            }
            1 => codex_logout(),
            _ => {}
        }
    } else if tmpl.provider_type == "copilot" {
        let options = &["Log in", "Log out"];
        let Ok(choice) = Select::new()
            .with_prompt("GitHub Copilot")
            .items(options)
            .default(0)
            .interact()
        else {
            return;
        };
        match choice {
            0 => {
                copilot_login().await;
                ensure_copilot_provider(tmpl);
            }
            1 => copilot_logout(),
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
