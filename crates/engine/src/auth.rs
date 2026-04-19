//! Authentication façade for provider login/logout and cached model lists.
//!
//! Consumers (`smelt auth`, the first-run wizard) should prefer this module
//! over reaching into `provider::codex` / `provider::copilot` directly. This
//! keeps provider internals free to evolve without breaking callers.

use crate::provider;

/// Which OAuth-based provider to authenticate with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProvider {
    Codex,
    Copilot,
}

/// Login method for providers that support multiple flows.
#[derive(Debug, Clone, Copy)]
pub enum LoginMethod {
    /// Opens a browser for the redirect flow (Codex only).
    Browser,
    /// Device-code flow, shown in-terminal.
    DeviceCode,
}

/// Callbacks invoked during interactive login flows.
///
/// `on_prompt` is called once with the URL to open and the code to enter.
/// `on_message` is called for each status update (polling, progress, etc.).
pub struct LoginProgress<'a> {
    pub on_prompt: &'a (dyn Fn(&str, &str) + Send + Sync),
    pub on_message: &'a (dyn Fn(&str) + Send + Sync),
}

/// Human-readable details about a successful login (for display only).
#[derive(Debug, Default, Clone)]
pub struct LoginDetails {
    pub account_id: Option<String>,
    pub api_base: Option<String>,
    pub expires_at: Option<String>,
}

pub async fn login(
    provider: AuthProvider,
    method: LoginMethod,
    client: &reqwest::Client,
    progress: &LoginProgress<'_>,
) -> Result<LoginDetails, String> {
    match provider {
        AuthProvider::Codex => codex_login(method, client).await,
        AuthProvider::Copilot => copilot_login(client, progress).await,
    }
}

pub fn logout(provider: AuthProvider) {
    match provider {
        AuthProvider::Codex => provider::codex::CodexTokens::delete(),
        AuthProvider::Copilot => provider::copilot::CopilotTokens::delete(),
    }
}

/// Return the cached model identifiers for a provider (loaded from disk).
/// The identifier is the string the user types to select the model
/// (Codex "slug", Copilot "id").
pub fn cached_models(kind: AuthProvider) -> Vec<String> {
    match kind {
        AuthProvider::Codex => provider::codex::load_cached_models()
            .into_iter()
            .map(|m| m.slug)
            .collect(),
        AuthProvider::Copilot => provider::copilot::load_cached_models()
            .into_iter()
            .map(|m| m.id)
            .collect(),
    }
}

/// Whether the user has stored credentials for this OAuth provider.
pub fn is_logged_in(provider: AuthProvider) -> bool {
    match provider {
        AuthProvider::Codex => provider::codex::CodexTokens::load().is_some(),
        AuthProvider::Copilot => provider::copilot::CopilotTokens::load().is_some(),
    }
}

/// Refresh the cached model list from the provider's API. Returns the
/// freshly fetched identifiers, or an empty vec on failure (logged by the
/// underlying implementation).
pub async fn refresh_models_cache(kind: AuthProvider, client: &reqwest::Client) -> Vec<String> {
    match kind {
        AuthProvider::Codex => provider::codex::refresh_models_cache(client)
            .await
            .into_iter()
            .map(|m| m.slug)
            .collect(),
        AuthProvider::Copilot => provider::copilot::refresh_models_cache(client)
            .await
            .into_iter()
            .map(|m| m.id)
            .collect(),
    }
}

// ── internals ─────────────────────────────────────────────────────────────

async fn codex_login(
    method: LoginMethod,
    client: &reqwest::Client,
) -> Result<LoginDetails, String> {
    let tokens = match method {
        LoginMethod::Browser => provider::codex::browser_login(client).await?,
        LoginMethod::DeviceCode => provider::codex::device_code_login(client).await?,
    };
    Ok(LoginDetails {
        account_id: tokens.account_id,
        ..Default::default()
    })
}

async fn copilot_login(
    client: &reqwest::Client,
    progress: &LoginProgress<'_>,
) -> Result<LoginDetails, String> {
    let callbacks = provider::copilot::LoginCallbacks {
        on_prompt: progress.on_prompt,
        on_progress: progress.on_message,
    };
    let tokens = provider::copilot::device_code_login(client, &callbacks).await?;
    Ok(LoginDetails {
        api_base: Some(tokens.api_base),
        expires_at: Some(tokens.expires_at.to_string()),
        ..Default::default()
    })
}
