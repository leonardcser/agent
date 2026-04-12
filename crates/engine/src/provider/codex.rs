use crate::log;
use crate::paths::state_dir;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::PathBuf;
use std::time::Duration;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
pub const CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const OAUTH_PORT: u16 = 1455;
const REFRESH_INTERVAL_SECS: u64 = 8 * 3600; // 8 hours

pub const CODEX_TOKENS_ENV: &str = "SMELT_CODEX_TOKENS";

use super::unix_now;

// ── Persisted tokens ───────────────────────────────────────────────────────

fn token_path() -> PathBuf {
    state_dir().join("codex_auth.json")
}

const KEYRING_SERVICE: &str = "smelt-codex-auth";
const KEYRING_USER: &str = "default";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp (seconds) when the access token expires.
    pub expires_at: u64,
    pub account_id: Option<String>,
    /// Unix timestamp (seconds) of the last successful token refresh.
    #[serde(default)]
    pub last_refresh: u64,
}

impl CodexTokens {
    /// Returns true if the token is expired (within 60s) or stale (>8h since refresh).
    pub fn needs_refresh(&self) -> bool {
        let now = unix_now();
        now + 60 >= self.expires_at
            || (self.last_refresh > 0 && now - self.last_refresh >= REFRESH_INTERVAL_SECS)
    }

    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;

        file_save(&json)?;
        let _ = keyring_save(&json);
        Ok(())
    }

    pub fn load() -> Option<Self> {
        if let Ok(json) = std::env::var(CODEX_TOKENS_ENV) {
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

    pub fn to_env_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
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

// ── JWT helpers ────────────────────────────────────────────────────────────

fn extract_account_id(access_token: &str, id_token: Option<&str>) -> Option<String> {
    for token in id_token.into_iter().chain(std::iter::once(access_token)) {
        if let Some(claims) = parse_jwt_claims(token) {
            if let Some(id) = claims.chatgpt_account_id.or_else(|| {
                claims
                    .auth_ext
                    .as_ref()
                    .and_then(|a| a.chatgpt_account_id.clone())
            }) {
                return Some(id);
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct JwtClaims {
    chatgpt_account_id: Option<String>,
    #[serde(rename = "https://api.openai.com/auth")]
    auth_ext: Option<AuthExt>,
}

#[derive(Deserialize)]
struct AuthExt {
    chatgpt_account_id: Option<String>,
}

fn parse_jwt_claims(token: &str) -> Option<JwtClaims> {
    use base64::Engine;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    serde_json::from_slice(&payload).ok()
}

// ── PKCE helpers ───────────────────────────────────────────────────────────

struct PkceCodes {
    verifier: String,
    challenge: String,
}

fn generate_pkce() -> PkceCodes {
    use base64::Engine;
    use rand::RngExt;

    let mut bytes = [0u8; 64];
    rand::rng().fill(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    let hash = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

    PkceCodes {
        verifier,
        challenge,
    }
}

fn generate_state() -> String {
    use base64::Engine;
    use rand::RngExt;

    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ── Browser OAuth flow ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

fn build_authorize_url(redirect_uri: &str, pkce: &PkceCodes, state: &str) -> String {
    let params = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "smelt")
        .finish();
    format!("{ISSUER}/oauth/authorize?{params}")
}

const HTML_SUCCESS: &str = r#"<!doctype html>
<html><head><title>smelt — Authorization Successful</title>
<style>body{font-family:system-ui,sans-serif;display:flex;justify-content:center;
align-items:center;height:100vh;margin:0;background:#131010;color:#f1ecec}
.c{text-align:center;padding:2rem}h1{margin-bottom:1rem}p{color:#b7b1b1}</style>
</head><body><div class="c"><h1>Authorization Successful</h1>
<p>You can close this window and return to smelt.</p></div>
<script>setTimeout(()=>window.close(),2000)</script></body></html>"#;

fn html_error(msg: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><title>Agent — Authorization Failed</title>
<style>body{{font-family:system-ui,sans-serif;display:flex;justify-content:center;
align-items:center;height:100vh;margin:0;background:#131010;color:#f1ecec}}
.c{{text-align:center;padding:2rem}}h1{{color:#fc533a;margin-bottom:1rem}}
p{{color:#b7b1b1}}.e{{color:#ff917b;font-family:monospace;margin-top:1rem;
padding:1rem;background:#3c140d;border-radius:.5rem}}</style>
</head><body><div class="c"><h1>Authorization Failed</h1>
<p>An error occurred during authorization.</p>
<div class="e">{msg}</div></div></body></html>"#
    )
}

/// Run the browser-based OAuth + PKCE flow.
///
/// 1. Starts a local HTTP server on port 1455
/// 2. Opens the browser to OpenAI's authorize endpoint
/// 3. Waits for the redirect callback with the authorization code
/// 4. Exchanges the code for tokens
pub async fn browser_login(client: &reqwest::Client) -> Result<CodexTokens, String> {
    let pkce = generate_pkce();
    let state = generate_state();
    let redirect_uri = format!("http://localhost:{OAUTH_PORT}/auth/callback");
    let auth_url = build_authorize_url(&redirect_uri, &pkce, &state);

    // Start the local callback server.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", OAUTH_PORT))
        .await
        .map_err(|e| format!("failed to bind port {OAUTH_PORT}: {e}"))?;

    // Open the browser.
    open_browser(&auth_url);

    // Wait for the callback (with a 5 minute timeout).
    let (code, received_state) =
        tokio::time::timeout(Duration::from_secs(300), wait_for_callback(&listener))
            .await
            .map_err(|_| "login timed out (5 minutes)".to_string())?
            .map_err(|e| format!("callback error: {e}"))?;

    if received_state != state {
        return Err("state mismatch — potential CSRF attack".into());
    }

    exchange_code(client, &code, &pkce.verifier, &redirect_uri).await
}

/// Wait for a single HTTP request on the callback listener, parse the code.
async fn wait_for_callback(listener: &tokio::net::TcpListener) -> Result<(String, String), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| format!("accept failed: {e}"))?;

    let mut buf = vec![0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the GET request line: "GET /auth/callback?code=...&state=... HTTP/1.1"
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");

    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params: std::collections::HashMap<&str, &str> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .collect();

    let error = params.get("error").copied();
    let error_desc = params.get("error_description").copied();

    if let Some(err) = error {
        let msg = error_desc.unwrap_or(err);
        let body = html_error(msg);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        return Err(msg.to_string());
    }

    let code = params
        .get("code")
        .copied()
        .ok_or("missing authorization code")?
        .to_string();
    let state = params.get("state").copied().unwrap_or("").to_string();

    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML_SUCCESS}",
        HTML_SUCCESS.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;

    Ok((code, state))
}

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
}

async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<CodexTokens, String> {
    let form_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code_verifier", code_verifier)
        .finish();

    let resp = client
        .post(format!("{ISSUER}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .map_err(|e| format!("token exchange failed: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token exchange error: {body}"));
    }

    let tokens: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("bad token response: {e}"))?;

    save_token_response(tokens)
}

// ── Token refresh ──────────────────────────────────────────────────────────

pub async fn refresh_tokens(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<CodexTokens, String> {
    let form_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .append_pair("client_id", CLIENT_ID)
        .finish();

    let resp = client
        .post(format!("{ISSUER}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .map_err(|e| format!("token refresh failed: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_refresh_error(&body));
    }

    let tokens: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("bad refresh response: {e}"))?;

    let result = save_token_response(tokens)?;

    log::entry(
        log::Level::Debug,
        "codex_token_refreshed",
        &serde_json::json!({ "expires_at": result.expires_at }),
    );

    Ok(result)
}

fn classify_refresh_error(body: &str) -> String {
    if body.contains("refresh_token_expired") {
        "your refresh token has expired — run `smelt auth` to sign in again".into()
    } else if body.contains("refresh_token_reused") {
        "your refresh token was already used — run `smelt auth` to sign in again".into()
    } else if body.contains("refresh_token_invalidated") {
        "your refresh token was revoked — run `smelt auth` to sign in again".into()
    } else {
        format!("token refresh error: {body}")
    }
}

fn save_token_response(tokens: TokenResponse) -> Result<CodexTokens, String> {
    let now = unix_now();
    let result = CodexTokens {
        account_id: extract_account_id(&tokens.access_token, tokens.id_token.as_deref()),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: now + tokens.expires_in.unwrap_or(3600),
        last_refresh: now,
    };
    result
        .save()
        .map_err(|e| format!("failed to save tokens: {e}"))?;
    Ok(result)
}

// ── Model discovery ────────────────────────────────────────────────────────

/// A model returned by the Codex models endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexModel {
    pub slug: String,
    pub display_name: String,
    pub description: Option<String>,
    pub context_window: Option<u32>,
}

/// Fetch the list of models available to the logged-in Codex account.
/// Returns models with `visibility: "list"`, sorted by priority.
pub async fn fetch_models(client: &reqwest::Client) -> Result<Vec<CodexModel>, String> {
    let (access_token, account_id) = ensure_access_token(client).await?;

    let version = fetch_codex_version(client)
        .await
        .unwrap_or_else(|_| "0.1.0".into());

    let url = format!("https://chatgpt.com/backend-api/codex/models?client_version={version}");

    let mut req = client
        .get(&url)
        .header("Accept", "application/json")
        .bearer_auth(&access_token);
    if let Some(id) = &account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("models request failed: {e}"))?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("models endpoint error: {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad models response: {e}"))?;

    let models = data["models"]
        .as_array()
        .ok_or("missing 'models' key in response")?;

    let mut result: Vec<(i64, CodexModel)> = models
        .iter()
        .filter_map(|m| {
            let slug = m["slug"].as_str()?.to_string();
            let display_name = m["display_name"].as_str().unwrap_or(&slug).to_string();
            let description = m["description"].as_str().map(|s| s.to_string());
            let context_window = m["context_window"].as_u64().map(|v| v as u32);
            let visibility = m["visibility"].as_str().unwrap_or("none");
            let priority = m["priority"].as_i64().unwrap_or(999);

            // Only include models visible in the picker.
            if visibility != "list" {
                return None;
            }

            Some((
                priority,
                CodexModel {
                    slug,
                    display_name,
                    description,
                    context_window,
                },
            ))
        })
        .collect();

    result.sort_by_key(|(p, _)| *p);

    Ok(result.into_iter().map(|(_, m)| m).collect())
}

/// Look up the context window for a model from the disk cache.
pub fn cached_context_window(model: &str) -> Option<u32> {
    load_cached_models()
        .into_iter()
        .find(|m| m.slug == model)
        .and_then(|m| m.context_window)
}

/// Load cached models from disk (fast, synchronous).
pub fn load_cached_models() -> Vec<CodexModel> {
    let cache_path = crate::paths::cache_dir().join("codex_models.json");
    let Ok(data) = std::fs::read_to_string(&cache_path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<CodexModel>>(&data).unwrap_or_default()
}

/// Write models to the disk cache.
fn save_models_cache(models: &[CodexModel]) {
    let cache_path = crate::paths::cache_dir().join("codex_models.json");
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &cache_path,
        serde_json::to_string(models).unwrap_or_default(),
    );
}

/// Fetch models from the API and update the cache. Returns the fresh list,
/// or an empty vec on failure.
pub async fn refresh_models_cache(client: &reqwest::Client) -> Vec<CodexModel> {
    let models = match fetch_models(client).await {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    save_models_cache(&models);
    models
}

async fn fetch_codex_version(client: &reqwest::Client) -> Result<String, String> {
    let resp = client
        .get("https://api.github.com/repos/openai/codex/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "smelt")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("github request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err("github API error".into());
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad github response: {e}"))?;

    data["name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or("missing release name".into())
}

// ── Device code flow ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "deserialize_interval")]
    interval: Option<u64>,
}

fn deserialize_interval<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    use serde::de;
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => Ok(n.as_u64()),
        serde_json::Value::String(s) => {
            s.trim().parse::<u64>().map(Some).map_err(de::Error::custom)
        }
        _ => Err(de::Error::custom("expected number or string for interval")),
    }
}

#[derive(Deserialize)]
struct DeviceCodePollResponse {
    authorization_code: Option<String>,
    code_verifier: Option<String>,
}

/// Run the device-code flow for headless environments.
///
/// 1. Request a user code from the auth server
/// 2. Display the code and verification URL to the user
/// 3. Poll until the user authorizes (up to 15 minutes)
/// 4. Exchange the authorization code for tokens
pub async fn device_code_login(client: &reqwest::Client) -> Result<CodexTokens, String> {
    let body = serde_json::json!({ "client_id": CLIENT_ID });

    let resp = client
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.as_u16() == 404 {
        return Err(
            "device code login is not enabled for this server — use browser login instead"
                .to_string(),
        );
    }
    if !status.is_success() {
        return Err(format!("device code error (HTTP {status}): {text}"));
    }

    let dc: DeviceCodeResponse = serde_json::from_str(&text)
        .map_err(|e| format!("bad device code response: {e}\nBody: {text}"))?;

    println!("\n  Open this URL in a browser:\n");
    println!("    {ISSUER}/codex/device\n");
    println!("  Then enter code: {}\n", dc.user_code);

    let interval = Duration::from_secs(dc.interval.unwrap_or(5));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15 * 60);

    loop {
        tokio::time::sleep(interval).await;

        if tokio::time::Instant::now() >= deadline {
            return Err("device code login timed out (15 minutes)".into());
        }

        let poll_body = serde_json::json!({
            "device_auth_id": dc.device_auth_id,
            "user_code": dc.user_code,
        });

        let resp = client
            .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
            .json(&poll_body)
            .send()
            .await
            .map_err(|e| format!("device code poll failed: {e}"))?;

        let poll_status = resp.status();
        if poll_status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let poll: DeviceCodePollResponse = serde_json::from_str(&body)
                .map_err(|e| format!("bad poll response: {e}\nBody: {body}"))?;

            let code = poll
                .authorization_code
                .ok_or("missing authorization_code in poll response")?;
            let verifier = poll
                .code_verifier
                .ok_or("missing code_verifier in poll response")?;

            let redirect_uri = format!("http://localhost:{OAUTH_PORT}/auth/callback");
            return exchange_code(client, &code, &verifier, &redirect_uri).await;
        }

        // 403/404 = authorization pending, keep polling. Other errors = bail.
        if poll_status.as_u16() != 403 && poll_status.as_u16() != 404 {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("device auth failed (HTTP {poll_status}): {body}"));
        }
    }
}

// ── Access token ───────────────────────────────────────────────────────────

/// Get valid tokens, refreshing if needed. Returns the full `CodexTokens`.
pub async fn ensure_access_token_full(client: &reqwest::Client) -> Result<CodexTokens, String> {
    let tokens = CodexTokens::load().ok_or("not logged in to Codex — run `smelt auth` first")?;

    if !tokens.needs_refresh() {
        return Ok(tokens);
    }

    refresh_tokens(client, &tokens.refresh_token).await
}

/// Get a valid access token, refreshing if needed. Returns `(access_token, account_id)`.
pub async fn ensure_access_token(
    client: &reqwest::Client,
) -> Result<(String, Option<String>), String> {
    let tokens = ensure_access_token_full(client).await?;
    Ok((tokens.access_token, tokens.account_id))
}
