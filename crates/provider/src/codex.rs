use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::time::Duration;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
pub const CHATGPT_BACKEND_API_BASE: &str = "https://chatgpt.com/backend-api";
pub const CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const FAST_SERVICE_TIER: &str = "priority";
pub const REFRESH_INTERVAL_SECS: u64 = 8 * 24 * 3600;
pub const OAUTH_PORT: u16 = 1455;
// Known working client version for model discovery when GitHub is unavailable.
const FALLBACK_CLIENT_VERSION: &str = "0.153.4";

pub struct CodexLoginProgress<'a> {
    pub on_prompt: &'a (dyn Fn(&str, &str) + Send + Sync),
    pub on_progress: &'a (dyn Fn(&str) + Send + Sync),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub account_id: Option<String>,
    #[serde(default)]
    pub last_refresh: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexModel {
    pub slug: String,
    pub display_name: String,
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_fast_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    #[serde(flatten)]
    pub catalog: protocol::ModelCatalogMetadata,
}

impl From<CodexModel> for protocol::ModelMetadata {
    fn from(model: CodexModel) -> Self {
        Self {
            id: model.slug,
            display_name: Some(model.display_name),
            context_window: model.context_window,
            max_output_tokens: None,
            supports_reasoning: model.supports_reasoning,
            supports_fast_mode: model.supports_fast_mode,
            catalog: model.catalog,
            input_modalities: None,
        }
    }
}

impl CodexTokens {
    pub fn needs_refresh_at(&self, now: u64) -> bool {
        now + 60 >= self.expires_at
            || (self.last_refresh > 0 && now - self.last_refresh >= REFRESH_INTERVAL_SECS)
    }

    pub fn apply_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = req.bearer_auth(&self.access_token);
        if let Some(account_id) = self.account_id.as_deref() {
            req.header("ChatGPT-Account-ID", account_id)
        } else {
            req
        }
    }
}

struct TokenRefreshRequest<'a> {
    url: String,
    body: TokenRefreshBody<'a>,
}

#[derive(Serialize)]
struct TokenRefreshBody<'a> {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<u64>,
}

fn token_url(issuer: &str) -> String {
    format!("{}/oauth/token", issuer)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> PkceCodes {
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

pub fn generate_state() -> String {
    use base64::Engine;
    use rand::RngExt;

    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

struct TokenExchangeRequest {
    url: String,
    body: String,
}

fn build_exchange_request(
    issuer: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> TokenExchangeRequest {
    TokenExchangeRequest {
        url: token_url(issuer),
        body: url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", CLIENT_ID)
            .append_pair("code_verifier", code_verifier)
            .finish(),
    }
}

pub fn build_authorize_url(
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> String {
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
    format!("{issuer}/oauth/authorize?{params}")
}

const HTML_SUCCESS: &str = r#"<!doctype html>
<html><head><title>smelt - Authorization Successful</title>
<style>body{font-family:system-ui,sans-serif;display:flex;justify-content:center;
align-items:center;height:100vh;margin:0;background:#131010;color:#f1ecec}
.c{text-align:center;padding:2rem}h1{margin-bottom:1rem}p{color:#b7b1b1}</style>
</head><body><div class="c"><h1>Authorization Successful</h1>
<p>You can close this window and return to smelt.</p></div>
<script>setTimeout(()=>window.close(),2000)</script></body></html>"#;

fn html_error(msg: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><title>smelt - Authorization Failed</title>
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

pub async fn browser_login(
    client: &reqwest::Client,
    progress: &CodexLoginProgress<'_>,
    issuer: &str,
    now: fn() -> u64,
) -> Result<CodexTokens, String> {
    let pkce = generate_pkce();
    let state = generate_state();
    let redirect_uri = format!("http://localhost:{OAUTH_PORT}/auth/callback");
    let auth_url = build_authorize_url(issuer, &redirect_uri, &pkce, &state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", OAUTH_PORT))
        .await
        .map_err(|e| format!("failed to bind port {OAUTH_PORT}: {e}"))?;

    (progress.on_prompt)(&auth_url, "");

    let (code, received_state) =
        tokio::time::timeout(Duration::from_secs(300), wait_for_callback(&listener))
            .await
            .map_err(|_| "login timed out (5 minutes)".to_string())?
            .map_err(|e| format!("callback error: {e}"))?;

    if received_state != state {
        return Err("state mismatch; potential CSRF attack".into());
    }

    exchange_authorization_code(
        client,
        issuer,
        &code,
        &pkce.verifier,
        &redirect_uri,
        None,
        now(),
    )
    .await
}

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

pub async fn exchange_authorization_code(
    client: &reqwest::Client,
    issuer: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    previous: Option<&CodexTokens>,
    now: u64,
) -> Result<CodexTokens, String> {
    let spec = build_exchange_request(issuer, code, code_verifier, redirect_uri);

    let resp = client
        .post(spec.url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(spec.body)
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

    merge_token_response(tokens, previous, now)
}

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

pub async fn device_code_login(
    client: &reqwest::Client,
    progress: &CodexLoginProgress<'_>,
    issuer: &str,
    now: fn() -> u64,
) -> Result<CodexTokens, String> {
    let body = serde_json::json!({ "client_id": CLIENT_ID });

    let resp = client
        .post(format!("{issuer}/api/accounts/deviceauth/usercode"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.as_u16() == 404 {
        return Err(
            "device code login is not enabled for this server; use browser login instead"
                .to_string(),
        );
    }
    if !status.is_success() {
        return Err(format!("device code error (HTTP {status}): {text}"));
    }

    let dc: DeviceCodeResponse = serde_json::from_str(&text)
        .map_err(|e| format!("bad device code response: {e}\nBody: {text}"))?;

    let verification_url = format!("{issuer}/codex/device");
    (progress.on_prompt)(&verification_url, &dc.user_code);

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
            .post(format!("{issuer}/api/accounts/deviceauth/token"))
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

            let redirect_uri = format!("{issuer}/deviceauth/callback");
            return exchange_authorization_code(
                client,
                issuer,
                &code,
                &verifier,
                &redirect_uri,
                None,
                now(),
            )
            .await;
        }

        if poll_status.as_u16() != 403 && poll_status.as_u16() != 404 {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("device auth failed (HTTP {poll_status}): {body}"));
        }
    }
}

fn build_refresh_request<'a>(issuer: &str, refresh_token: &'a str) -> TokenRefreshRequest<'a> {
    TokenRefreshRequest {
        url: token_url(issuer),
        body: TokenRefreshBody {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        },
    }
}

fn model_supports_fast_mode(model: &serde_json::Value) -> bool {
    model["service_tiers"]
        .as_array()
        .is_some_and(|tiers| tiers.iter().any(|tier| tier["id"] == FAST_SERVICE_TIER))
        || model["additional_speed_tiers"]
            .as_array()
            .is_some_and(|tiers| tiers.iter().any(|tier| tier == "fast"))
}

fn model_reasoning_efforts(model: &serde_json::Value) -> Vec<protocol::ReasoningEffort> {
    model["supported_reasoning_levels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|level| level["effort"].as_str().or_else(|| level.as_str()))
        .filter_map(protocol::ReasoningEffort::parse)
        .collect()
}

pub fn parse_models_response(data: &serde_json::Value) -> Result<Vec<CodexModel>, String> {
    let models = data["models"]
        .as_array()
        .ok_or("missing 'models' key in response")?;
    let mut result: Vec<(i64, CodexModel)> = models
        .iter()
        .filter_map(|model| {
            let slug = model["slug"].as_str()?.to_string();
            let display_name = model["display_name"].as_str().unwrap_or(&slug).to_string();
            let description = model["description"].as_str().map(str::to_string);
            let context_window = model["context_window"].as_u64().map(|v| v as u32);
            let show_in_picker = model["visibility"].as_str() == Some("list");
            let priority = model["priority"].as_i64().unwrap_or(999);
            let supports_fast_mode = Some(model_supports_fast_mode(model));
            let supports_reasoning = model["supported_reasoning_levels"]
                .as_array()
                .map(|levels| !levels.is_empty());
            let default_reasoning_effort = model["default_reasoning_level"]
                .as_str()
                .and_then(protocol::ReasoningEffort::parse);
            let supported_reasoning_efforts = model_reasoning_efforts(model);
            Some((
                priority,
                CodexModel {
                    slug,
                    display_name,
                    context_window,
                    supports_fast_mode,
                    supports_reasoning,
                    catalog: protocol::ModelCatalogMetadata {
                        description,
                        show_in_picker,
                        default_reasoning_effort,
                        supported_reasoning_efforts,
                    },
                },
            ))
        })
        .collect();
    result.sort_by_key(|(priority, _)| *priority);
    Ok(result.into_iter().map(|(_, model)| model).collect())
}

pub async fn fetch_models(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<Vec<CodexModel>, String> {
    let version = fetch_latest_release_version(client)
        .await
        .unwrap_or_else(|_| FALLBACK_CLIENT_VERSION.into());
    let url = format!("{CHATGPT_BACKEND_API_BASE}/codex/models?client_version={version}");

    let mut req = client
        .get(&url)
        .header("Accept", "application/json")
        .bearer_auth(access_token);
    if let Some(id) = account_id {
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
    parse_models_response(&data)
}

async fn fetch_latest_release_version(client: &reqwest::Client) -> Result<String, String> {
    let resp = client
        .head("https://github.com/openai/codex/releases/latest")
        .header("User-Agent", "smelt")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("github request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("github release lookup returned {}", resp.status()));
    }
    release_version(resp.url())
        .map(str::to_string)
        .ok_or("invalid github release URL".into())
}

fn release_version(url: &url::Url) -> Option<&str> {
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        return None;
    }
    let version = url
        .path()
        .strip_prefix("/openai/codex/releases/tag/rust-v")?;
    let parts: Vec<_> = version.split('.').collect();
    (parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
                && part.parse::<u64>().is_ok()
        }))
    .then_some(version)
}

pub async fn refresh_tokens(
    client: &reqwest::Client,
    refresh_token: &str,
    previous: Option<&CodexTokens>,
    issuer: &str,
    now: u64,
) -> Result<CodexTokens, String> {
    let spec = build_refresh_request(issuer, refresh_token);

    let resp = client
        .post(spec.url)
        .header("Content-Type", "application/json")
        .json(&spec.body)
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

    merge_token_response(tokens, previous, now)
}

pub fn classify_refresh_error(body: &str) -> String {
    if body.contains("refresh_token_expired") {
        "your refresh token has expired; run `smelt auth` to sign in again".into()
    } else if body.contains("refresh_token_reused") {
        "your refresh token was already used; run `smelt auth` to sign in again".into()
    } else if body.contains("refresh_token_invalidated") {
        "your refresh token was revoked; run `smelt auth` to sign in again".into()
    } else {
        format!("token refresh error: {body}")
    }
}

pub fn merge_token_response(
    tokens: TokenResponse,
    previous: Option<&CodexTokens>,
    now: u64,
) -> Result<CodexTokens, String> {
    let access_token = tokens
        .access_token
        .or_else(|| previous.map(|t| t.access_token.clone()))
        .ok_or("missing access_token in OAuth response")?;
    let refresh_token = tokens
        .refresh_token
        .or_else(|| previous.map(|t| t.refresh_token.clone()))
        .ok_or("missing refresh_token in OAuth response")?;

    let expires_at = parse_jwt_expiration(&access_token)
        .or_else(|| tokens.expires_in.map(|s| now + s))
        .unwrap_or(now + 3600);

    let account_id = extract_account_id(&access_token, tokens.id_token.as_deref())
        .or_else(|| previous.and_then(|t| t.account_id.clone()));

    Ok(CodexTokens {
        account_id,
        access_token,
        refresh_token,
        expires_at,
        last_refresh: now,
    })
}

pub fn extract_account_id(access_token: &str, id_token: Option<&str>) -> Option<String> {
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
pub struct JwtClaims {
    pub chatgpt_account_id: Option<String>,
    #[serde(rename = "https://api.openai.com/auth")]
    pub auth_ext: Option<AuthExt>,
}

#[derive(Deserialize)]
pub struct AuthExt {
    pub chatgpt_account_id: Option<String>,
}

pub fn parse_jwt_claims(token: &str) -> Option<JwtClaims> {
    decode_jwt_payload(token)
}

fn decode_jwt_payload<T: serde::de::DeserializeOwned>(token: &str) -> Option<T> {
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

#[derive(Deserialize)]
struct ExpClaim {
    exp: Option<u64>,
}

pub fn parse_jwt_expiration(token: &str) -> Option<u64> {
    decode_jwt_payload::<ExpClaim>(token).and_then(|c| c.exp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_redirect_extracts_stable_version() {
        let url =
            url::Url::parse("https://github.com/openai/codex/releases/tag/rust-v0.153.4").unwrap();
        assert_eq!(release_version(&url), Some("0.153.4"));
    }

    #[test]
    fn release_redirect_rejects_unexpected_destinations_and_versions() {
        for destination in [
            "https://github.com/openai/codex/releases/latest",
            "https://github.com/login",
            "https://example.com/openai/codex/releases/tag/rust-v0.153.4",
            "http://github.com/openai/codex/releases/tag/rust-v0.153.4",
            "https://github.com/other/codex/releases/tag/rust-v0.153.4",
            "https://github.com/openai/codex/releases/tag/v0.153.4",
            "https://github.com/openai/codex/releases/tag/rust-v0.153.4-alpha.1",
            "https://github.com/openai/codex/releases/tag/rust-v0.153",
            "https://github.com/openai/codex/releases/tag/rust-v0.153.4.1",
            "https://github.com/openai/codex/releases/tag/rust-v0.153.4/extra",
            "https://github.com/openai/codex/releases/tag/rust-v0.153.x",
            "https://github.com/openai/codex/releases/tag/rust-v0.153.",
            "https://github.com/openai/codex/releases/tag/rust-v0.0153.4",
        ] {
            assert_eq!(
                release_version(&url::Url::parse(destination).unwrap()),
                None,
                "{destination}",
            );
        }
    }

    fn tokens(expires_at: u64, last_refresh: u64) -> CodexTokens {
        CodexTokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at,
            account_id: None,
            last_refresh,
        }
    }

    #[test]
    fn needs_refresh_true_when_within_60_seconds_of_expiry() {
        let t = tokens(1_030, 900);
        assert!(t.needs_refresh_at(1_000));
    }

    #[test]
    fn needs_refresh_true_when_past_expiry() {
        let t = tokens(900, 800);
        assert!(t.needs_refresh_at(1_000));
    }

    #[test]
    fn needs_refresh_false_when_expiry_far_and_recent_refresh() {
        let t = tokens(8_200, 940);
        assert!(!t.needs_refresh_at(1_000));
    }

    #[test]
    fn needs_refresh_true_when_last_refresh_older_than_interval() {
        let t = tokens(REFRESH_INTERVAL_SECS + 2_000, 1_000);
        assert!(t.needs_refresh_at(1_000 + REFRESH_INTERVAL_SECS + 1));
    }

    #[test]
    fn needs_refresh_ignores_zero_last_refresh_interval() {
        let t = tokens(8_200, 0);
        assert!(!t.needs_refresh_at(1_000));
    }

    #[test]
    fn build_authorize_url_includes_all_required_params() {
        let pkce = PkceCodes {
            verifier: "v".into(),
            challenge: "ch".into(),
        };
        let url = build_authorize_url(
            ISSUER,
            "http://localhost:1455/auth/callback",
            &pkce,
            "STATE",
        );
        assert!(url.starts_with(ISSUER));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id="));
        assert!(url.contains("code_challenge=ch"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE"));
        assert!(url.contains("originator=smelt"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    #[test]
    fn html_error_embeds_message_in_body() {
        let html = html_error("boom");
        assert!(html.contains("boom"));
        assert!(html.contains("Authorization Failed"));
    }

    #[test]
    fn deserialize_interval_accepts_number() {
        let r: DeviceCodeResponse = serde_json::from_value(
            serde_json::json!({"device_auth_id":"d","user_code":"u","interval": 7}),
        )
        .unwrap();
        assert_eq!(r.interval, Some(7));
    }

    #[test]
    fn deserialize_interval_accepts_numeric_string() {
        let r: DeviceCodeResponse = serde_json::from_value(
            serde_json::json!({"device_auth_id":"d","user_code":"u","interval":"3"}),
        )
        .unwrap();
        assert_eq!(r.interval, Some(3));
    }

    #[test]
    fn deserialize_interval_handles_null() {
        let r: DeviceCodeResponse = serde_json::from_value(
            serde_json::json!({"device_auth_id":"d","user_code":"u","interval": null}),
        )
        .unwrap();
        assert!(r.interval.is_none());
    }

    #[test]
    fn deserialize_interval_defaults_to_none_when_field_missing() {
        let r: DeviceCodeResponse =
            serde_json::from_value(serde_json::json!({"device_auth_id":"d","user_code":"u"}))
                .unwrap();
        assert!(r.interval.is_none());
    }

    #[test]
    fn deserialize_device_code_response_accepts_usercode_alias() {
        let r: DeviceCodeResponse =
            serde_json::from_value(serde_json::json!({"device_auth_id":"d","usercode":"alt"}))
                .unwrap();
        assert_eq!(r.user_code, "alt");
    }

    #[test]
    fn generate_pkce_produces_nonempty_verifier_and_challenge() {
        let p = generate_pkce();
        assert!(!p.verifier.is_empty());
        assert!(!p.challenge.is_empty());
        assert_ne!(p.verifier, p.challenge);
    }

    #[test]
    fn generate_state_produces_nonempty_unique_strings() {
        let a = generate_state();
        let b = generate_state();
        assert!(!a.is_empty());
        assert_ne!(a, b);
    }

    #[test]
    fn model_catalog_and_cache_retain_hidden_models_and_reasoning_metadata() {
        let models = parse_models_response(&serde_json::json!({
            "models": [
                {
                    "slug": "visible",
                    "display_name": "Visible",
                    "visibility": "list",
                    "priority": 2,
                    "default_reasoning_level": "medium",
                    "supported_reasoning_levels": [
                        {"effort": "low"},
                        {"effort": "high"}
                    ]
                },
                {
                    "slug": "gpt-6-astra",
                    "display_name": "GPT-6-Astra",
                    "visibility": "hide",
                    "priority": 1,
                    "context_window": 272000,
                    "default_reasoning_level": "low",
                    "supported_reasoning_levels": [
                        {"effort": "low"},
                        {"effort": "medium"},
                        {"effort": "high"},
                        {"effort": "xhigh"},
                        {"effort": "max"},
                        {"effort": "ultra"}
                    ]
                }
            ]
        }))
        .unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-6-astra", "visible"]
        );
        let astra = &models[0];
        assert!(!astra.catalog.show_in_picker);
        assert_eq!(astra.context_window, Some(272_000));
        assert_eq!(astra.supports_reasoning, Some(true));
        assert_eq!(
            astra.catalog.default_reasoning_effort,
            Some(protocol::ReasoningEffort::Low)
        );
        assert_eq!(
            astra.catalog.supported_reasoning_efforts,
            vec![
                protocol::ReasoningEffort::Low,
                protocol::ReasoningEffort::Medium,
                protocol::ReasoningEffort::High,
                protocol::ReasoningEffort::XHigh,
                protocol::ReasoningEffort::Max,
                protocol::ReasoningEffort::Ultra,
            ]
        );
        assert!(models[1].catalog.show_in_picker);

        let cached: Vec<CodexModel> =
            serde_json::from_value(serde_json::to_value(&models).unwrap()).unwrap();
        assert!(!cached[0].catalog.show_in_picker);
        assert_eq!(
            cached[0].catalog.supported_reasoning_efforts,
            astra.catalog.supported_reasoning_efforts
        );
    }

    #[test]
    fn model_fast_capability_accepts_current_and_legacy_catalog_fields() {
        assert!(model_supports_fast_mode(&serde_json::json!({
            "service_tiers": [{
                "id": "priority",
                "name": "Fast",
                "description": "1.5x speed, increased usage"
            }]
        })));
        assert!(model_supports_fast_mode(&serde_json::json!({
            "additional_speed_tiers": ["fast"]
        })));
        assert!(!model_supports_fast_mode(&serde_json::json!({
            "service_tiers": [{"id": "flex"}],
            "additional_speed_tiers": []
        })));
    }

    #[test]
    fn legacy_cached_model_preserves_unknown_fast_capability() {
        let model: CodexModel = serde_json::from_value(serde_json::json!({
            "slug": "gpt-5.6-sol",
            "display_name": "GPT-5.6-Sol",
            "description": "Latest frontier agentic coding model.",
            "context_window": 272000
        }))
        .unwrap();

        assert_eq!(model.supports_fast_mode, None);
        assert_eq!(model.supports_reasoning, None);
        assert_eq!(
            model.catalog.description.as_deref(),
            Some("Latest frontier agentic coding model.")
        );
        assert!(model.catalog.show_in_picker);
        assert_eq!(model.catalog.default_reasoning_effort, None);
        assert!(model.catalog.supported_reasoning_efforts.is_empty());
    }
}
