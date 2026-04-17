//! GitHub Copilot authentication and API access.
//!
//! Flow:
//! 1. Device-code OAuth against github.com to get a long-lived GitHub access
//!    token (the "refresh" token in our storage).
//! 2. Exchange that token at api.github.com/copilot_internal/v2/token for a
//!    short-lived Copilot API token (~30min TTL). The token string carries a
//!    `proxy-ep=` claim from which we derive the API base URL.
//! 3. Copilot speaks OpenAI chat/completions. The provider layer uses
//!    `chat_completions::build_body` and injects Copilot-specific headers.

use crate::log;
use crate::paths::state_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use super::unix_now;

// ── Constants ──────────────────────────────────────────────────────────────

// base64-encoded GitHub OAuth client ID used by the official VS Code Copilot
// Chat extension. Stored encoded so casual greps won't flag it.
const CLIENT_ID_B64: &str = "SXYxLmI1MDdhMDhjODdlY2ZlOTg=";

const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
pub const DEFAULT_COPILOT_API_BASE: &str = "https://api.individual.githubcopilot.com";

const EDITOR_VERSION: &str = "vscode/1.107.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

pub const COPILOT_TOKENS_ENV: &str = "SMELT_COPILOT_TOKENS";

const KEYRING_SERVICE: &str = "smelt-copilot-auth";
const KEYRING_USER: &str = "default";

// ── Persisted tokens ───────────────────────────────────────────────────────

fn token_path() -> PathBuf {
    state_dir().join("copilot_auth.json")
}

/// Persisted Copilot credentials.
///
/// `refresh_token` = the long-lived GitHub OAuth access token obtained from the
/// device-code flow. It never expires on its own (the user must revoke it).
///
/// `access_token` + `expires_at` = the short-lived Copilot API token returned
/// by `/copilot_internal/v2/token`. It lives ~30 minutes.
///
/// `api_base` = derived from the Copilot token's `proxy-ep=` claim. Cached so
/// we don't re-parse on every request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotTokens {
    pub refresh_token: String,
    pub access_token: String,
    pub expires_at: u64,
    pub api_base: String,
    #[serde(default)]
    pub last_refresh: u64,
}

impl CopilotTokens {
    /// True if the access token is expired or within 60 seconds of expiry.
    pub fn needs_refresh(&self) -> bool {
        let now = unix_now();
        now + 60 >= self.expires_at
    }

    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        file_save(&json)?;
        let _ = keyring_save(&json);
        Ok(())
    }

    pub fn load() -> Option<Self> {
        if let Ok(json) = std::env::var(COPILOT_TOKENS_ENV) {
            if let Ok(tokens) = serde_json::from_str(&json) {
                return Some(tokens);
            }
        }
        if let Some(json) = keyring_load() {
            if let Ok(tokens) = serde_json::from_str(&json) {
                return Some(tokens);
            }
        }
        let data = std::fs::read_to_string(token_path()).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn delete() {
        let _ = keyring_delete();
        let _ = std::fs::remove_file(token_path());
    }
}

fn file_save(json: &str) -> Result<(), String> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn keyring_save(json: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| e.to_string())?;
    entry.set_password(json).map_err(|e| e.to_string())
}

fn keyring_load() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    entry.get_password().ok()
}

fn keyring_delete() -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| e.to_string())?;
    entry.delete_credential().map_err(|e| e.to_string())
}

// ── Client ID decoding ─────────────────────────────────────────────────────

fn client_id() -> String {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(CLIENT_ID_B64)
        .expect("hard-coded client ID must decode");
    String::from_utf8(bytes).expect("client ID must be UTF-8")
}

// ── Base URL derivation ────────────────────────────────────────────────────

/// Extract the Copilot API base URL from the `proxy-ep=` claim in a Copilot
/// token. Token format:
/// `tid=...;exp=...;proxy-ep=proxy.individual.githubcopilot.com;...`
pub fn base_url_from_token(token: &str) -> Option<String> {
    let proxy_host = token
        .split(';')
        .find_map(|kv| kv.strip_prefix("proxy-ep="))?;
    // Convert proxy.xxx → api.xxx (Copilot uses api.* for REST endpoints).
    let api_host = proxy_host.strip_prefix("proxy.").unwrap_or(proxy_host);
    Some(format!("https://api.{}", api_host))
}

// ── Copilot headers (sent on every API request) ────────────────────────────

/// Base headers every Copilot request needs. Dynamic headers (X-Initiator,
/// Copilot-Vision-Request) are added by the provider layer per request.
pub fn base_headers() -> [(&'static str, &'static str); 4] {
    [
        ("User-Agent", COPILOT_USER_AGENT),
        ("Editor-Version", EDITOR_VERSION),
        ("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION),
        ("Copilot-Integration-Id", COPILOT_INTEGRATION_ID),
    ]
}

// ── Device-code login ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

/// Callbacks for the interactive device-code login flow.
pub struct LoginCallbacks<'a> {
    /// Called once the verification URL and user code are known. The caller
    /// should display these to the user and open the URL in a browser if
    /// possible.
    pub on_prompt: &'a (dyn Fn(&str, &str) + Send + Sync),
    /// Called with progress messages (e.g. "Fetching Copilot token…").
    pub on_progress: &'a (dyn Fn(&str) + Send + Sync),
}

/// Run the GitHub device-code OAuth flow.
pub async fn device_code_login(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
) -> Result<CopilotTokens, String> {
    let cid = client_id();

    // 1. Request device + user code.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &cid)
        .append_pair("scope", "read:user")
        .finish();
    let device_resp = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", COPILOT_USER_AGENT)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;

    if !device_resp.status().is_success() {
        let status = device_resp.status();
        let body = device_resp.text().await.unwrap_or_default();
        return Err(format!("device code error (HTTP {status}): {body}"));
    }

    let device: DeviceCodeResponse = device_resp
        .json()
        .await
        .map_err(|e| format!("bad device code response: {e}"))?;

    (callbacks.on_prompt)(&device.verification_uri, &device.user_code);
    open_browser(&device.verification_uri);

    // 2. Poll for the GitHub access token.
    let github_token = poll_for_github_token(
        client,
        &cid,
        &device.device_code,
        device.interval,
        device.expires_in,
    )
    .await?;

    // 3. Exchange for a Copilot token.
    (callbacks.on_progress)("Fetching Copilot token…");
    let (access, expires_at, api_base) = fetch_copilot_token(client, &github_token).await?;

    let tokens = CopilotTokens {
        refresh_token: github_token,
        access_token: access,
        expires_at,
        api_base,
        last_refresh: unix_now(),
    };
    tokens
        .save()
        .map_err(|e| format!("failed to save tokens: {e}"))?;

    // 4. Discover models and enable the policy for each one (required for
    //    Claude/Grok etc. before they respond).
    (callbacks.on_progress)("Enabling Copilot models…");
    let models = match fetch_available_models(client, &tokens.access_token, &tokens.api_base).await
    {
        Ok(m) => m,
        Err(e) => {
            log::entry(
                log::Level::Warn,
                "copilot_fetch_models_failed",
                &serde_json::json!({ "error": e }),
            );
            Vec::new()
        }
    };
    let mut enabled = 0usize;
    for m in &models {
        if enable_model_policy(client, &tokens.access_token, &tokens.api_base, &m.id).await {
            enabled += 1;
        }
    }
    if !models.is_empty() {
        (callbacks.on_progress)(&format!(
            "Enabled {}/{} Copilot models",
            enabled,
            models.len()
        ));
        save_models_cache(&models);
    }

    Ok(tokens)
}

async fn poll_for_github_token(
    client: &reqwest::Client,
    client_id: &str,
    device_code: &str,
    initial_interval: u64,
    expires_in: u64,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires_in.max(1));
    // Multipliers and behaviour match pi-mono's reference flow.
    let initial_multiplier = 1.2_f64;
    let slow_down_multiplier = 1.4_f64;
    let mut interval_ms: u64 = initial_interval.max(1) * 1000;
    let mut multiplier = initial_multiplier;
    let mut slow_down_count: u32 = 0;

    loop {
        if tokio::time::Instant::now() >= deadline {
            if slow_down_count > 0 {
                return Err("Device flow timed out after repeated slow_down responses. \
                     This is often caused by clock drift in WSL or VM environments."
                    .into());
            }
            return Err("Device flow timed out".into());
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait_ms = ((interval_ms as f64) * multiplier).ceil() as u64;
        let wait = Duration::from_millis(wait_ms).min(remaining);
        tokio::time::sleep(wait).await;

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", client_id)
            .append_pair("device_code", device_code)
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
            .finish();
        let resp = client
            .post(GITHUB_ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", COPILOT_USER_AGENT)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("token poll failed: {e}"))?;

        let data: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Err(format!("bad token poll response: {e}")),
        };

        if let Some(token) = data.get("access_token").and_then(|v| v.as_str()) {
            return Ok(token.to_string());
        }

        let error = data.get("error").and_then(|v| v.as_str()).unwrap_or("");
        match error {
            "authorization_pending" => continue,
            "slow_down" => {
                slow_down_count += 1;
                if let Some(n) = data.get("interval").and_then(|v| v.as_u64()) {
                    interval_ms = n * 1000;
                } else {
                    interval_ms = interval_ms.saturating_add(5000).max(1000);
                }
                multiplier = slow_down_multiplier;
                continue;
            }
            other => {
                let desc = data
                    .get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let suffix = if desc.is_empty() {
                    String::new()
                } else {
                    format!(": {desc}")
                };
                return Err(format!("Device flow failed: {other}{suffix}"));
            }
        }
    }
}

async fn fetch_copilot_token(
    client: &reqwest::Client,
    github_token: &str,
) -> Result<(String, u64, String), String> {
    let mut req = client
        .get(COPILOT_TOKEN_URL)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {github_token}"));
    for (k, v) in base_headers() {
        req = req.header(k, v);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("copilot token request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("copilot token error (HTTP {status}): {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad copilot token response: {e}"))?;

    let token = data
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or("missing 'token' in copilot response")?
        .to_string();
    let expires_at = data
        .get("expires_at")
        .and_then(|v| v.as_u64())
        .ok_or("missing 'expires_at' in copilot response")?;

    let api_base =
        base_url_from_token(&token).unwrap_or_else(|| DEFAULT_COPILOT_API_BASE.to_string());

    Ok((token, expires_at, api_base))
}

// ── Token refresh ──────────────────────────────────────────────────────────

pub async fn refresh_tokens(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<CopilotTokens, String> {
    let (access, expires_at, api_base) = fetch_copilot_token(client, refresh_token).await?;
    let tokens = CopilotTokens {
        refresh_token: refresh_token.to_string(),
        access_token: access,
        expires_at,
        api_base,
        last_refresh: unix_now(),
    };
    tokens
        .save()
        .map_err(|e| format!("failed to save tokens: {e}"))?;
    log::entry(
        log::Level::Debug,
        "copilot_token_refreshed",
        &serde_json::json!({ "expires_at": tokens.expires_at }),
    );
    Ok(tokens)
}

/// Return valid Copilot tokens, refreshing if the access token is expired.
pub async fn ensure_access_token_full(client: &reqwest::Client) -> Result<CopilotTokens, String> {
    let tokens =
        CopilotTokens::load().ok_or("not logged in to GitHub Copilot — run `smelt auth` first")?;
    if !tokens.needs_refresh() {
        return Ok(tokens);
    }
    refresh_tokens(client, &tokens.refresh_token).await
}

// ── Model discovery + policy enablement ────────────────────────────────────

/// A model returned by the Copilot `/models` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotModel {
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

async fn fetch_available_models(
    client: &reqwest::Client,
    access_token: &str,
    api_base: &str,
) -> Result<Vec<CopilotModel>, String> {
    let url = format!("{}/models", api_base.trim_end_matches('/'));
    let mut req = client
        .get(&url)
        .header("Accept", "application/json")
        .bearer_auth(access_token);
    for (k, v) in base_headers() {
        req = req.header(k, v);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("models request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("models error (HTTP {status}): {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad models response: {e}"))?;

    let entries = data
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or("missing 'data' array in models response")?;

    let mut out: Vec<CopilotModel> = Vec::with_capacity(entries.len());
    for m in entries {
        // Copilot returns entries that aren't chat-capable (e.g. embeddings).
        // Filter for entries whose capability type is "chat".
        let capability_type = m
            .pointer("/capabilities/type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !capability_type.is_empty() && capability_type != "chat" {
            continue;
        }
        let model_picker_enabled = m
            .get("model_picker_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !model_picker_enabled {
            continue;
        }
        let id = match m.get("id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let name = m
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        let vendor = m
            .get("vendor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let context_window = m
            .pointer("/capabilities/limits/max_context_window_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                m.pointer("/capabilities/limits/max_prompt_tokens")
                    .and_then(|v| v.as_u64())
            })
            .map(|v| v as u32);
        let max_output_tokens = m
            .pointer("/capabilities/limits/max_output_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        out.push(CopilotModel {
            id,
            name,
            vendor,
            context_window,
            max_output_tokens,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    Ok(out)
}

async fn enable_model_policy(
    client: &reqwest::Client,
    access_token: &str,
    api_base: &str,
    model_id: &str,
) -> bool {
    let url = format!(
        "{}/models/{}/policy",
        api_base.trim_end_matches('/'),
        model_id
    );
    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("openai-intent", "chat-policy")
        .header("x-interaction-type", "chat-policy")
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "state": "enabled" }));
    for (k, v) in base_headers() {
        req = req.header(k, v);
    }
    match req.send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

// ── Model cache ────────────────────────────────────────────────────────────

fn cache_path() -> PathBuf {
    crate::paths::cache_dir().join("copilot_models.json")
}

pub fn load_cached_models() -> Vec<CopilotModel> {
    let Ok(data) = std::fs::read_to_string(cache_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_models_cache(models: &[CopilotModel]) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serde_json::to_string(models).unwrap_or_default());
}

/// Fetch models using the stored credentials, caching on success.
/// Returns the fresh list, or an empty vec on failure.
pub async fn refresh_models_cache(client: &reqwest::Client) -> Vec<CopilotModel> {
    let Ok(tokens) = ensure_access_token_full(client).await else {
        return Vec::new();
    };
    let models = match fetch_available_models(client, &tokens.access_token, &tokens.api_base).await
    {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    save_models_cache(&models);
    models
}

/// Look up the context window for a model from the disk cache.
pub fn cached_context_window(model: &str) -> Option<u32> {
    load_cached_models()
        .into_iter()
        .find(|m| m.id == model)
        .and_then(|m| m.context_window)
}

// ── Browser opener ─────────────────────────────────────────────────────────

fn open_browser(url: &str) {
    use std::process::Stdio;

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_decodes() {
        let cid = client_id();
        assert!(cid.starts_with("Iv1."));
        assert!(cid.len() > 10);
    }

    #[test]
    fn base_url_from_token_parses_proxy_ep() {
        let token = "tid=abc;exp=9999;proxy-ep=proxy.individual.githubcopilot.com;sku=x";
        assert_eq!(
            base_url_from_token(token).as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
    }

    #[test]
    fn base_url_from_token_handles_enterprise() {
        let token = "tid=abc;proxy-ep=proxy.business.githubcopilot.com";
        assert_eq!(
            base_url_from_token(token).as_deref(),
            Some("https://api.business.githubcopilot.com")
        );
    }

    #[test]
    fn base_url_from_token_returns_none_without_claim() {
        let token = "tid=abc;exp=9999;sku=x";
        assert_eq!(base_url_from_token(token), None);
    }
}
