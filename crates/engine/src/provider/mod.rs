mod auth;
mod auth_storage;
pub mod codex;
pub mod copilot;
pub mod kimi_code;

use crate::log;
pub(crate) use auth::LoginCallbacks;
pub use protocol::TokenUsage;
use protocol::{Message, ModelConfig, ReasoningEffort};
use reqwest::Client;
#[cfg(test)]
use smelt_provider::ParsedResponse;
#[cfg(test)]
use smelt_provider::{apply_response_format, sanitize_tool_call_arguments};
use smelt_provider::{ChatProvider, ChatResponse, CopilotInitiator};
use std::path::Path;
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
use smelt_provider::WireApi;
#[cfg(test)]
use smelt_provider::{api_key_auth, emit_retry, endpoint_url, ApiKeyAuth};
use smelt_provider::{
    normalize_api_base, AuthKind, CacheConfig, ChatOptions, ChatRequestOptions, ProviderClient,
    ProviderKind, ToolDefinition,
};

#[cfg(test)]
use smelt_provider::{context_window_from_models_entry, models_entry_matches};

#[cfg(all(test, unix))]
use smelt_provider::format_epoch_local;
#[cfg(test)]
use smelt_provider::{
    anthropic_supports_structured_output, api_base_normalization_hint, format_rate_limit,
    json_as_u64, parse_claude_model_version, parse_resets_at, parse_retry_from_body,
    quota_exceeded_message, unix_now, ApiBaseNormalizationHint, CancellationToken,
    ClaudeModelFamily, FunctionSchema, ResponseFormat,
};
#[cfg(test)]
use smelt_provider::{collect_indexed_tool_calls, non_empty};

type EngineChatResponse = ChatResponse;

const MANAGED_MODEL_CACHE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManagedModelRefreshError {
    Retryable(String),
    Terminal(String),
    Unauthenticated(String),
}

impl ManagedModelRefreshError {
    pub(crate) fn retryable(message: impl Into<String>) -> Self {
        Self::Retryable(message.into())
    }

    pub(crate) fn terminal(message: impl Into<String>) -> Self {
        Self::Terminal(message.into())
    }

    pub(crate) fn unauthenticated(message: impl Into<String>) -> Self {
        Self::Unauthenticated(message.into())
    }

    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Retryable(message) | Self::Terminal(message) | Self::Unauthenticated(message) => {
                message
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ManagedModelCache<T> {
    schema_version: u32,
    provider_kind: String,
    fetched_at: u64,
    account_fingerprint: String,
    models: Vec<T>,
}

pub(crate) fn load_managed_model_cache<T>(
    path: &Path,
    provider_kind: &str,
    account_fingerprint: &str,
) -> Vec<T>
where
    T: serde::de::DeserializeOwned,
{
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(cache) = serde_json::from_slice::<ManagedModelCache<T>>(&data) else {
        return Vec::new();
    };
    if cache.schema_version != MANAGED_MODEL_CACHE_SCHEMA_VERSION
        || cache.provider_kind != provider_kind
        || cache.account_fingerprint != account_fingerprint
    {
        return Vec::new();
    }
    cache.models
}

pub(crate) fn save_managed_model_cache<T>(
    path: &Path,
    provider_kind: &str,
    account_fingerprint: String,
    models: &[T],
) -> Result<(), String>
where
    T: serde::Serialize + Clone,
{
    let cache = ManagedModelCache {
        schema_version: MANAGED_MODEL_CACHE_SCHEMA_VERSION,
        provider_kind: provider_kind.to_string(),
        fetched_at: smelt_provider::unix_now(),
        account_fingerprint,
        models: models.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&cache).map_err(|error| error.to_string())?;
    crate::paths::write_atomic(path, &json).map_err(|error| error.to_string())
}

use smelt_provider::ProviderError;
#[cfg(test)]
use smelt_provider::{backoff_delay, retry_delay_for};

#[cfg(test)]
pub(crate) mod test_http {
    pub(crate) async fn spawn_json_response(
        body: impl Into<String>,
    ) -> (String, tokio::task::JoinHandle<String>) {
        spawn_http_response("200 OK", body).await
    }

    pub(crate) async fn spawn_http_response(
        status: impl Into<String>,
        body: impl Into<String>,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let status = status.into();
        let body = body.into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let req = String::from_utf8_lossy(&buf);
                let Some((headers, request_body)) = req.split_once("\r\n\r\n") else {
                    continue;
                };
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                if request_body.len() >= content_len {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&buf).to_string();
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            req
        });
        (format!("http://{addr}"), task)
    }
}

#[derive(Clone)]
pub struct EngineProvider {
    api_base: String,
    api_key: String,
    client: ProviderClient,
    kind: ProviderKind,
    auth: Option<AuthKind>,
    model_config: ModelConfig,
    /// Sticky routing token for Codex: set from the first response, echoed on subsequent requests within the same turn.
    turn_state: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

struct EngineChatAuth {
    is_codex: bool,
    is_copilot: bool,
    request_api_key: String,
    codex_auth: Option<smelt_provider::codex::CodexTokens>,
    codex_401_retried: bool,
    copilot_auth: Option<smelt_provider::copilot::CopilotTokens>,
    copilot_401_retried: bool,
    copilot_initiator: CopilotInitiator,
    copilot_has_images: bool,
}

struct EngineChatAttempt<'a, 'opts> {
    messages: &'a [Message],
    tools: &'a [ToolDefinition],
    model: &'a str,
    effort: ReasoningEffort,
    request_opts: &'a ChatRequestOptions,
    opts: &'opts ChatOptions<'opts>,
    config: &'a ModelConfig,
    copilot_model: Option<&'a smelt_provider::copilot::CopilotModel>,
}

impl EngineProvider {
    pub(crate) fn supports_mid_turn_reasoning_changes(&self) -> bool {
        self.kind.descriptor().mid_turn_reasoning_changes
    }

    pub fn new(
        api_base: String,
        api_key: String,
        provider_type: &str,
        client: Client,
        _clock: std::sync::Arc<dyn crate::clock::Clock>,
    ) -> Self {
        let api_base = normalize_api_base(&api_base);
        let kind = ProviderKind::from_config_and_url(provider_type, &api_base);
        let auth = kind.descriptor().auth;
        Self {
            api_base,
            api_key,
            client: ProviderClient::new(client),
            kind,
            auth,
            model_config: Default::default(),
            turn_state: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub(crate) fn reset_turn_state(&self) {
        *self.turn_state.lock().unwrap() = None;
    }

    #[cfg(test)]
    pub(crate) fn api_base(&self) -> &str {
        &self.api_base
    }

    #[cfg(test)]
    pub(crate) fn model_config(&self) -> &ModelConfig {
        &self.model_config
    }

    #[cfg(test)]
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn with_model_config(mut self, config: ModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub(crate) fn tool_calling(&self) -> bool {
        self.model_config.tool_calling()
    }

    /// Whether this provider speaks the Anthropic wire format and accepts
    /// `cache_control` markers on system, tools, and message content.
    pub fn supports_anthropic_cache(&self) -> bool {
        self.kind.descriptor().anthropic_cache
    }

    pub fn default_cache_config(&self, ttl_long: bool, session_id: Option<&str>) -> CacheConfig {
        CacheConfig {
            anthropic_markers: self.supports_anthropic_cache(),
            ttl_long,
            prompt_cache_key: session_id.filter(|s| !s.is_empty()).map(|s| s.to_string()),
        }
    }

    pub(crate) async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        effort: ReasoningEffort,
        request_opts: &ChatRequestOptions,
        opts: &ChatOptions<'_>,
    ) -> Result<EngineChatResponse, ProviderError> {
        let is_copilot = self.kind == ProviderKind::Copilot;
        let copilot_model = is_copilot.then(|| copilot::cached_model(model)).flatten();
        let config = self
            .resolve_model_config(model, copilot_model.as_ref())
            .await;
        let effort = self.resolve_reasoning_effort(effort, &config, model).await;
        let mut auth = self.prepare_chat_auth(messages).await?;

        loop {
            let attempt = EngineChatAttempt {
                messages,
                tools,
                model,
                effort: effort.clone(),
                request_opts,
                opts,
                config: &config,
                copilot_model: copilot_model.as_ref(),
            };
            let result = self.send_chat_attempt(attempt, &auth).await;

            match result {
                Err(ProviderError::Auth(_)) if self.refresh_after_auth_error(&mut auth).await? => {
                    continue;
                }
                Ok(response) => {
                    self.store_response_metadata(&response, &auth);
                    return Ok(response);
                }
                result => return result,
            }
        }
    }

    async fn prepare_chat_auth(
        &self,
        messages: &[Message],
    ) -> Result<EngineChatAuth, ProviderError> {
        let is_codex = self.kind == ProviderKind::Codex;
        let is_copilot = self.kind == ProviderKind::Copilot;
        let codex_auth = if is_codex {
            Some(
                codex::ensure_access_token_full(self.client.http())
                    .await
                    .map_err(ProviderError::Auth)?,
            )
        } else {
            None
        };
        let copilot_auth = if is_copilot {
            Some(
                copilot::ensure_access_token_full(self.client.http())
                    .await
                    .map_err(ProviderError::Auth)?,
            )
        } else {
            None
        };
        let request_api_key = match self.auth {
            Some(AuthKind::KimiCodeOAuth) => kimi_code::access_token(self.client.http())
                .await
                .map_err(ProviderError::Auth)?,
            Some(AuthKind::ApiKey) | None => self.api_key.clone(),
        };
        let copilot_initiator = if is_copilot {
            match messages.last().map(|m| m.role) {
                Some(protocol::Role::User) | None => CopilotInitiator::User,
                _ => CopilotInitiator::Agent,
            }
        } else {
            CopilotInitiator::User
        };

        Ok(EngineChatAuth {
            is_codex,
            is_copilot,
            request_api_key,
            codex_auth,
            codex_401_retried: false,
            copilot_auth,
            copilot_401_retried: false,
            copilot_initiator,
            copilot_has_images: is_copilot && messages_have_images(messages),
        })
    }

    async fn send_chat_attempt(
        &self,
        attempt: EngineChatAttempt<'_, '_>,
        auth: &EngineChatAuth,
    ) -> Result<EngineChatResponse, ProviderError> {
        let turn_state = self.turn_state.lock().unwrap().clone();
        let kimi_headers =
            (self.auth == Some(AuthKind::KimiCodeOAuth)).then(kimi_code::protocol_headers);
        let provider = match self.auth {
            Some(AuthKind::KimiCodeOAuth) => {
                let Some(headers) = kimi_headers.as_ref() else {
                    return Err(ProviderError::InvalidResponse(
                        "kimi-code headers are missing".to_string(),
                    ));
                };
                ChatProvider::kimi_code(&auth.request_api_key, headers)
            }
            Some(AuthKind::ApiKey) => ChatProvider::api_key(self.kind, &auth.request_api_key),
            None if auth.is_codex => {
                let Some(tokens) = auth.codex_auth.as_ref() else {
                    return Err(ProviderError::Auth(
                        "codex authentication is missing".to_string(),
                    ));
                };
                ChatProvider::codex(tokens, turn_state.as_deref())
            }
            None if auth.is_copilot => {
                let Some(tokens) = auth.copilot_auth.as_ref() else {
                    return Err(ProviderError::Auth(
                        "copilot authentication is missing".to_string(),
                    ));
                };
                ChatProvider::copilot(
                    tokens,
                    attempt.model,
                    attempt.copilot_model,
                    auth.copilot_initiator,
                    auth.copilot_has_images,
                )
            }
            None => ChatProvider::none(self.kind),
        };
        let request = smelt_provider::ChatRequest {
            provider,
            api_base: &self.api_base,
            model: attempt.model,
            messages: attempt.messages,
            tools: attempt.tools,
            effort: attempt.effort,
            config: attempt.config,
            cache: attempt.request_opts.cache.clone(),
            response_format: attempt.request_opts.response_format.clone(),
            fast_mode: attempt.request_opts.fast_mode,
        };

        self.client.chat(request, attempt.opts).await
    }

    async fn refresh_after_auth_error(
        &self,
        auth: &mut EngineChatAuth,
    ) -> Result<bool, ProviderError> {
        if auth.is_codex && !auth.codex_401_retried {
            auth.codex_401_retried = true;
            let Some(stale) = auth.codex_auth.as_ref() else {
                return Ok(false);
            };
            if let Ok(refreshed) =
                codex::refresh_tokens(self.client.http(), &stale.refresh_token).await
            {
                log::entry(
                    log::Level::Info,
                    "codex_401_recovery",
                    &serde_json::json!({ "account_id": refreshed.account_id }),
                );
                auth.codex_auth = Some(refreshed);
                return Ok(true);
            }
        }

        if auth.is_copilot && !auth.copilot_401_retried {
            auth.copilot_401_retried = true;
            let Some(stale) = auth.copilot_auth.as_ref() else {
                return Ok(false);
            };
            if let Ok(refreshed) =
                copilot::refresh_tokens(self.client.http(), &stale.refresh_token).await
            {
                log::entry(
                    log::Level::Info,
                    "copilot_401_recovery",
                    &serde_json::json!({ "expires_at": refreshed.expires_at }),
                );
                auth.copilot_auth = Some(refreshed);
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn store_response_metadata(&self, response: &EngineChatResponse, auth: &EngineChatAuth) {
        if !auth.is_codex {
            return;
        }
        if let Some(state) = response.metadata.codex_turn_state.as_deref() {
            let mut turn_state = self.turn_state.lock().unwrap();
            if turn_state.is_none() {
                *turn_state = Some(state.to_string());
            }
        }
    }

    async fn resolve_model_config(
        &self,
        model: &str,
        copilot_model: Option<&smelt_provider::copilot::CopilotModel>,
    ) -> ModelConfig {
        let mut config = self.model_config.clone();
        if config.max_tokens.is_some() {
            return config;
        }

        let copilot_output_tokens = if self.kind == ProviderKind::Copilot {
            copilot_model
                .and_then(|m| m.max_output_tokens)
                .or_else(|| copilot::cached_output_tokens(model))
        } else {
            None
        };
        if let Some(tokens) = copilot_output_tokens {
            config.max_tokens = Some(tokens);
            return config;
        }

        self.ensure_catalog_loaded().await;
        if let Some(tokens) =
            smelt_provider::catalog::output_tokens(self.kind.as_config_str(), &self.api_base, model)
        {
            config.max_tokens = Some(tokens);
        }
        config
    }

    async fn resolve_reasoning_effort(
        &self,
        requested: ReasoningEffort,
        config: &ModelConfig,
        model: &str,
    ) -> ReasoningEffort {
        if config.supports_reasoning.is_none() {
            self.ensure_catalog_loaded().await;
        }
        let supports_reasoning = config.supports_reasoning.or_else(|| {
            smelt_provider::catalog::supports_reasoning(
                self.kind.as_config_str(),
                &self.api_base,
                model,
            )
        });
        smelt_provider::effective_reasoning_effort(
            requested,
            self.kind.as_config_str(),
            supports_reasoning,
        )
    }

    async fn ensure_catalog_loaded(&self) {
        let catalog_cache_dir = crate::paths::cache_dir().join("web");
        smelt_provider::catalog::ensure_loaded(self.client.http(), Some(&catalog_cache_dir)).await;
    }

    pub async fn fetch_context_window(&self, model: &str) -> Option<u32> {
        let provider_label = self.kind.as_config_str();
        if let Some(v) = self.model_config.context_window {
            crate::log::entry(
                crate::log::Level::Info,
                "fetch_context_window",
                &serde_json::json!({
                    "model": model,
                    "provider": provider_label,
                    "from_config": v,
                    "result": v,
                }),
            );
            return Some(v);
        }
        // Hit the provider's own `/v1/models` first - that's the
        // authoritative source. Fall through to the models.dev catalog
        // if it doesn't expose a window field.
        let from_provider = match self.kind {
            ProviderKind::OpenAiCompatible => {
                self.client
                    .fetch_context_window_openai_compatible(&self.api_base, &self.api_key, model)
                    .await
            }
            ProviderKind::OpenAi => None,
            ProviderKind::Codex => codex::cached_context_window(model),
            ProviderKind::KimiCode => match kimi_code::cached_context_window(model) {
                Some(v) => Some(v),
                None => kimi_code::fetch_model_info(self.client.http())
                    .await
                    .ok()
                    .into_iter()
                    .flatten()
                    .find(|info| info.matches_name(model))
                    .and_then(|info| info.context_length),
            },
            ProviderKind::Anthropic => {
                self.client
                    .fetch_context_window_anthropic(&self.api_base, &self.api_key, model)
                    .await
            }
            ProviderKind::AnthropicCompatible => {
                match self
                    .client
                    .fetch_context_window_anthropic(&self.api_base, &self.api_key, model)
                    .await
                {
                    Some(v) => Some(v),
                    None => {
                        self.client
                            .fetch_context_window_openai_compatible(
                                &self.api_base,
                                &self.api_key,
                                model,
                            )
                            .await
                    }
                }
            }
            ProviderKind::Copilot => copilot::cached_context_window(model),
        };
        let result = from_provider.or_else(|| {
            smelt_provider::catalog::context_window(provider_label, &self.api_base, model)
        });
        crate::log::entry(
            crate::log::Level::Info,
            "fetch_context_window",
            &serde_json::json!({
                "model": model,
                "provider": provider_label,
                "from_provider": from_provider,
                "result": result,
            }),
        );
        result
    }
}

fn messages_have_images(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m.role {
        protocol::Role::User | protocol::Role::Tool => {
            m.content.as_ref().is_some_and(|c| c.image_count() > 0)
        }
        _ => false,
    })
}

pub fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Content, FunctionCall, Role, ToolCall};
    use serde_json::json;

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: Some(Content::Text(text.into())),
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        }
    }

    #[test]
    fn kimi_code_api_base_uses_anthropic_messages_wire_even_for_old_configs() {
        let provider = EngineProvider::new(
            smelt_provider::kimi_code::API_BASE.to_string(),
            "token".to_string(),
            "openai-compatible",
            Client::new(),
            std::sync::Arc::new(crate::clock::RealClock),
        );

        assert_eq!(provider.kind, ProviderKind::KimiCode);
        assert_eq!(provider.kind.wire_api(), WireApi::AnthropicMessages);
    }

    #[test]
    fn kimi_code_defers_mid_turn_reasoning_changes() {
        let kimi = EngineProvider::new(
            smelt_provider::kimi_code::API_BASE.to_string(),
            "token".to_string(),
            "kimi-code",
            Client::new(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        let openai = EngineProvider::new(
            "https://api.openai.com/v1".to_string(),
            "token".to_string(),
            "openai",
            Client::new(),
            std::sync::Arc::new(crate::clock::RealClock),
        );

        assert!(!kimi.supports_mid_turn_reasoning_changes());
        assert!(openai.supports_mid_turn_reasoning_changes());
    }

    // ---- api_key_auth ----

    #[test]
    fn api_key_auth_uses_bearer_for_kimi_code_oauth_tokens() {
        assert_eq!(
            api_key_auth(ProviderKind::KimiCode, "token"),
            Some(ApiKeyAuth::Bearer)
        );
    }

    #[test]
    fn api_key_auth_keeps_x_api_key_for_anthropic_wire_providers() {
        assert_eq!(
            api_key_auth(ProviderKind::Anthropic, "key"),
            Some(ApiKeyAuth::XApiKey)
        );
        assert_eq!(
            api_key_auth(ProviderKind::AnthropicCompatible, "key"),
            Some(ApiKeyAuth::XApiKey)
        );
    }

    #[test]
    fn api_key_auth_uses_bearer_for_openai_family_keys() {
        assert_eq!(
            api_key_auth(ProviderKind::OpenAiCompatible, "key"),
            Some(ApiKeyAuth::Bearer)
        );
        assert_eq!(
            api_key_auth(ProviderKind::OpenAi, "key"),
            Some(ApiKeyAuth::Bearer)
        );
    }

    #[test]
    fn api_key_auth_returns_none_without_key_or_for_managed_auth_providers() {
        assert_eq!(api_key_auth(ProviderKind::OpenAiCompatible, ""), None);
        assert_eq!(api_key_auth(ProviderKind::Codex, "key"), None);
        assert_eq!(api_key_auth(ProviderKind::Copilot, "key"), None);
    }

    #[test]
    fn endpoint_url_accepts_base_or_full_endpoint() {
        assert_eq!(
            endpoint_url("https://api.cerebras.ai/v1", "chat/completions"),
            "https://api.cerebras.ai/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url(
                "https://api.cerebras.ai/v1/chat/completions",
                "chat/completions"
            ),
            "https://api.cerebras.ai/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://api.openai.com/v1/responses", "responses"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            endpoint_url("https://api.anthropic.com/v1/messages/", "messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn api_base_normalization_hint_reports_endpoint_suffix() {
        assert_eq!(
            api_base_normalization_hint(" https://api.cerebras.ai/v1/chat/completions/ "),
            Some(ApiBaseNormalizationHint {
                original: "https://api.cerebras.ai/v1/chat/completions".into(),
                normalized: "https://api.cerebras.ai/v1".into(),
                endpoint: "chat/completions",
            })
        );
        assert_eq!(
            api_base_normalization_hint("https://api.cerebras.ai/v1"),
            None
        );
        assert_eq!(
            api_base_normalization_hint("https://api.cerebras.ai/v1/chat/completions?x=1"),
            None
        );
    }

    #[test]
    fn provider_normalizes_endpoint_shaped_api_base() {
        let provider = EngineProvider::new(
            "https://api.cerebras.ai/v1/chat/completions".to_string(),
            "token".to_string(),
            "openai-compatible",
            Client::new(),
            std::sync::Arc::new(crate::clock::RealClock),
        );

        assert_eq!(provider.api_base(), "https://api.cerebras.ai/v1");
    }

    #[test]
    fn openai_compatible_reasoning_requires_explicit_support() {
        assert_eq!(
            smelt_provider::effective_reasoning_effort(
                ReasoningEffort::High,
                "openai-compatible",
                None
            ),
            ReasoningEffort::Off
        );

        assert_eq!(
            smelt_provider::effective_reasoning_effort(
                ReasoningEffort::High,
                "openai-compatible",
                Some(true)
            ),
            ReasoningEffort::High
        );
    }

    // ---- non_empty ----

    #[test]
    fn non_empty_returns_none_for_empty_string() {
        assert_eq!(non_empty(String::new()), None);
    }

    #[test]
    fn non_empty_returns_some_for_non_empty_string() {
        assert_eq!(non_empty("hi".into()), Some("hi".to_string()));
    }

    // ---- collect_indexed_tool_calls ----

    #[test]
    fn collect_indexed_tool_calls_returns_sorted_by_index() {
        let mut map = std::collections::HashMap::new();
        map.insert(2, ("c".into(), "C".into(), "{}".into()));
        map.insert(0, ("a".into(), "A".into(), "{}".into()));
        map.insert(1, ("b".into(), "B".into(), "{}".into()));
        let calls = collect_indexed_tool_calls(map);
        let names: Vec<_> = calls.iter().map(|c| c.function.name.clone()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn collect_indexed_tool_calls_empty_input_returns_empty_vec() {
        let calls = collect_indexed_tool_calls(std::collections::HashMap::new());
        assert!(calls.is_empty());
    }

    // ---- json_as_u64 ----

    #[test]
    fn json_as_u64_handles_positive_u64() {
        assert_eq!(json_as_u64(&json!(42)), Some(42));
    }

    #[test]
    fn json_as_u64_rejects_negative_i64() {
        assert_eq!(json_as_u64(&json!(-1)), None);
    }

    #[test]
    fn json_as_u64_returns_none_for_non_numeric() {
        assert_eq!(json_as_u64(&json!("nope")), None);
        assert_eq!(json_as_u64(&json!(null)), None);
    }

    // ---- parse_retry_from_body ----

    #[test]
    fn parse_retry_from_body_seconds_default_unit() {
        let d = parse_retry_from_body("you exceeded quota, try again in 5 seconds").unwrap();
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn parse_retry_from_body_ms_unit_uses_milliseconds() {
        let d = parse_retry_from_body("try again in 250ms please").unwrap();
        assert_eq!(d, Duration::from_millis(250));
    }

    #[test]
    fn parse_retry_from_body_fractional_seconds() {
        let d = parse_retry_from_body("try again in 1.5s").unwrap();
        assert!((d.as_secs_f64() - 1.5).abs() < 1e-6);
    }

    #[test]
    fn parse_retry_from_body_returns_none_when_phrase_absent() {
        assert_eq!(parse_retry_from_body("rate limited"), None);
    }

    #[test]
    fn parse_retry_from_body_returns_none_when_no_number_follows() {
        assert_eq!(parse_retry_from_body("try again in soon"), None);
    }

    #[test]
    fn parse_retry_from_body_case_insensitive() {
        assert!(parse_retry_from_body("TRY AGAIN IN 3 SECONDS").is_some());
    }

    // ---- parse_resets_at ----

    #[test]
    fn parse_resets_at_extracts_from_error_object() {
        let body = r#"{"error": {"resets_at": 1700000000}}"#;
        assert_eq!(parse_resets_at(body), Some(1700000000));
    }

    #[test]
    fn parse_resets_at_returns_none_for_invalid_json() {
        assert_eq!(parse_resets_at("not json"), None);
    }

    #[test]
    fn parse_resets_at_returns_none_when_field_absent() {
        assert_eq!(parse_resets_at(r#"{"error": {}}"#), None);
    }

    // ---- ProviderError::from_http ----

    #[test]
    fn from_http_400_is_invalid_response() {
        let err = ProviderError::from_http(400, "bad".into(), None);
        assert!(matches!(err, ProviderError::InvalidResponse(_)));
    }

    #[test]
    fn from_http_401_and_403_are_auth() {
        assert!(matches!(
            ProviderError::from_http(401, "no".into(), None),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            ProviderError::from_http(403, "no".into(), None),
            ProviderError::Auth(_)
        ));
    }

    #[test]
    fn from_http_404_is_not_found() {
        assert!(matches!(
            ProviderError::from_http(404, "gone".into(), None),
            ProviderError::NotFound(_)
        ));
    }

    #[test]
    fn from_http_500_is_server() {
        match ProviderError::from_http(500, "boom".into(), None) {
            ProviderError::Server { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "boom");
            }
            e => panic!("expected Server, got {e:?}"),
        }
    }

    #[test]
    fn from_http_429_is_rate_limited_with_resets_from_body() {
        let body = r#"{"error":{"resets_at": 999}}"#;
        match ProviderError::from_http(429, body.into(), None) {
            ProviderError::RateLimited { resets_at } => assert_eq!(resets_at, Some(999)),
            e => panic!("expected RateLimited, got {e:?}"),
        }
    }

    #[test]
    fn from_http_429_falls_back_to_retry_after_when_body_lacks_resets() {
        match ProviderError::from_http_at(
            429,
            "no body".into(),
            Some(Duration::from_secs(10)),
            1_000,
        ) {
            ProviderError::RateLimited { resets_at } => assert_eq!(resets_at, Some(1_010)),
            e => panic!("expected RateLimited with resets_at, got {e:?}"),
        }
    }

    #[test]
    fn from_http_quota_strings_promote_to_quota_exceeded_regardless_of_status() {
        let cases = [
            (500, "insufficient_quota"),
            (500, "billing_not_active"),
            (500, "your credit balance is too low"),
            (429, "monthly quota exhausted"),
            (429, "usage_limit_reached"),
        ];
        for (code, body) in cases {
            let err = ProviderError::from_http(code, body.into(), None);
            assert!(
                matches!(err, ProviderError::QuotaExceeded { .. }),
                "expected QuotaExceeded for ({code}, {body:?})"
            );
            assert_eq!(err.to_string(), quota_exceeded_message());
        }
    }

    #[test]
    fn from_http_quota_429_preserves_retry_after() {
        match ProviderError::from_http_at(
            429,
            "usage_limit_reached".into(),
            Some(Duration::from_secs(30)),
            1_000,
        ) {
            ProviderError::QuotaExceeded { resets_at, .. } => assert_eq!(resets_at, Some(1_030)),
            e => panic!("expected QuotaExceeded with resets_at, got {e:?}"),
        }
    }

    #[test]
    fn from_http_429_rate_exceeded_without_retry_time_is_rate_limited() {
        let err = ProviderError::from_http(429, "request rate exceeded".into(), None);
        assert!(matches!(
            err,
            ProviderError::RateLimited { resets_at: None }
        ));
        assert_eq!(err.to_string(), "rate limited");
    }

    #[test]
    fn from_http_429_long_retry_after_stays_rate_limited() {
        let err = ProviderError::from_http(
            429,
            "request rate exceeded".into(),
            Some(Duration::from_secs(60 * 60)),
        );
        assert!(matches!(
            err,
            ProviderError::RateLimited { resets_at: Some(_) }
        ));
    }

    #[test]
    fn retry_delay_for_rate_limit_requires_short_reset_time() {
        let now = unix_now();
        let short = ProviderError::RateLimited {
            resets_at: Some(now + 60),
        };
        assert!(retry_delay_for(&short, 0, None, now).is_some());

        let long = ProviderError::RateLimited {
            resets_at: Some(now + 60 * 60),
        };
        assert_eq!(retry_delay_for(&long, 0, None, now), None);

        let missing = ProviderError::RateLimited { resets_at: None };
        assert_eq!(retry_delay_for(&missing, 0, None, now), None);
    }

    #[test]
    fn emit_retry_reports_one_based_retry_count() {
        let cancel = CancellationToken::new();
        let seen = std::sync::Mutex::new(Vec::new());
        let on_retry = |delay, attempt| seen.lock().unwrap().push((delay, attempt));
        let mut opts = ChatOptions::new(&cancel);
        opts.on_retry = Some(&on_retry);

        emit_retry(&opts, Duration::from_secs(2), 0);
        emit_retry(&opts, Duration::from_secs(4), 1);

        assert_eq!(
            *seen.lock().unwrap(),
            vec![(Duration::from_secs(2), 1), (Duration::from_secs(4), 2)]
        );
    }

    // ---- format_rate_limit ----

    #[test]
    fn format_rate_limit_without_resets_returns_plain_message() {
        assert_eq!(format_rate_limit(&None), "rate limited");
    }

    #[test]
    fn format_rate_limit_with_resets_includes_try_again_phrase() {
        let msg = format_rate_limit(&Some(1_700_000_000));
        assert!(msg.starts_with("rate limited"));
        assert!(msg.contains("try again at"));
    }

    // ---- format_epoch_local ----

    #[cfg(unix)]
    #[test]
    fn format_epoch_local_produces_month_day_year_time_ampm() {
        let s = format_epoch_local(1_700_000_000);
        // Loose checks (depends on local TZ): contains a month abbreviation, a year, and AM/PM.
        let has_month = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
        .iter()
        .any(|m| s.contains(m));
        assert!(has_month, "{s}");
        assert!(s.contains("2023"));
        assert!(s.contains("AM") || s.contains("PM"));
    }

    // ---- backoff_delay ----

    #[test]
    fn backoff_delay_doubles_each_attempt() {
        assert_eq!(backoff_delay(0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1), Duration::from_millis(1000));
        assert_eq!(backoff_delay(2), Duration::from_millis(2000));
        assert_eq!(backoff_delay(3), Duration::from_millis(4000));
    }

    // ---- ProviderKind ----

    #[test]
    fn provider_kind_from_config_recognizes_known_types() {
        assert_eq!(ProviderKind::from_config("openai"), ProviderKind::OpenAi);
        assert_eq!(ProviderKind::from_config("codex"), ProviderKind::Codex);
        assert_eq!(
            ProviderKind::from_config("anthropic"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            ProviderKind::from_config("anthropic-compatible"),
            ProviderKind::AnthropicCompatible
        );
        assert_eq!(ProviderKind::from_config("copilot"), ProviderKind::Copilot);
        assert_eq!(
            ProviderKind::from_config("github-copilot"),
            ProviderKind::Copilot
        );
        assert_eq!(
            ProviderKind::from_config("kimi-code"),
            ProviderKind::KimiCode
        );
    }

    #[test]
    fn provider_kind_from_config_unknown_defaults_to_openai_compatible() {
        assert_eq!(
            ProviderKind::from_config("gibberish"),
            ProviderKind::OpenAiCompatible
        );
        assert_eq!(
            ProviderKind::from_config(""),
            ProviderKind::OpenAiCompatible
        );
    }

    #[test]
    fn provider_kind_detect_from_url_matches_host_substrings() {
        assert_eq!(
            ProviderKind::detect_from_url("https://api.kimi.com/coding"),
            ProviderKind::KimiCode
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.anthropic.com"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.openai.com/v1"),
            ProviderKind::OpenAi
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://chatgpt.com/backend"),
            ProviderKind::Codex
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.githubcopilot.com"),
            ProviderKind::Copilot
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://local.host"),
            ProviderKind::OpenAiCompatible
        );
    }

    #[test]
    fn provider_kind_as_config_str_round_trips() {
        for k in [
            ProviderKind::OpenAi,
            ProviderKind::Codex,
            ProviderKind::Anthropic,
            ProviderKind::AnthropicCompatible,
            ProviderKind::KimiCode,
            ProviderKind::Copilot,
            ProviderKind::OpenAiCompatible,
        ] {
            let s = k.as_config_str();
            assert_eq!(ProviderKind::from_config(s), k);
        }
    }

    // ---- sanitize_tool_call_arguments ----

    #[test]
    fn sanitize_replaces_invalid_argument_string_with_empty_object_string() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tool_calls".into(),
            json!([{"function": {"arguments": "not json"}}]),
        );
        sanitize_tool_call_arguments(&mut obj);
        assert_eq!(obj["tool_calls"][0]["function"]["arguments"], "{}");
    }

    #[test]
    fn sanitize_keeps_valid_argument_strings() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tool_calls".into(),
            json!([{"function": {"arguments": "{\"a\":1}"}}]),
        );
        sanitize_tool_call_arguments(&mut obj);
        assert_eq!(obj["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn sanitize_is_noop_when_tool_calls_absent() {
        let mut obj = serde_json::Map::new();
        obj.insert("other".into(), json!("data"));
        sanitize_tool_call_arguments(&mut obj);
        assert_eq!(obj["other"], "data");
    }

    #[test]
    fn sanitize_ignores_arguments_that_are_not_strings() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tool_calls".into(),
            json!([{"function": {"arguments": {"a": 1}}}]),
        );
        sanitize_tool_call_arguments(&mut obj);
        // Non-string arguments are left untouched.
        assert!(obj["tool_calls"][0]["function"]["arguments"].is_object());
    }

    // ---- apply_response_format ----

    fn fmt() -> ResponseFormat {
        ResponseFormat {
            name: "out".into(),
            schema: json!({"type":"object"}),
        }
    }

    #[test]
    fn apply_response_format_openai_compatible_writes_response_format_json_schema() {
        let mut body = json!({});
        apply_response_format(&mut body, WireApi::ChatCompletions, &fmt());
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "out");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn apply_response_format_copilot_uses_same_shape_as_openai_compatible() {
        let mut body = json!({});
        apply_response_format(&mut body, WireApi::ChatCompletions, &fmt());
        assert_eq!(body["response_format"]["json_schema"]["name"], "out");
    }

    #[test]
    fn apply_response_format_openai_writes_text_format_block() {
        let mut body = json!({});
        apply_response_format(&mut body, WireApi::OpenAiResponses, &fmt());
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["name"], "out");
    }

    #[test]
    fn apply_response_format_codex_writes_text_format_block() {
        let mut body = json!({});
        apply_response_format(&mut body, WireApi::OpenAiResponses, &fmt());
        assert_eq!(body["text"]["format"]["name"], "out");
    }

    #[test]
    fn apply_response_format_anthropic_modern_model_creates_output_config_format() {
        let mut body = json!({"model": "claude-sonnet-4-6"});
        apply_response_format(&mut body, WireApi::AnthropicMessages, &fmt());
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn apply_response_format_anthropic_merges_into_existing_output_config_object() {
        let mut body = json!({"model": "claude-opus-4-6", "output_config": {"effort": "high"}});
        apply_response_format(&mut body, WireApi::AnthropicMessages, &fmt());
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn apply_response_format_anthropic_legacy_model_does_not_write_field() {
        let mut body = json!({"model": "claude-3-5-sonnet"});
        apply_response_format(&mut body, WireApi::AnthropicMessages, &fmt());
        assert!(body.get("output_config").is_none());
    }

    // ---- anthropic_supports_structured_output ----

    #[test]
    fn anthropic_supports_structured_output_recognizes_4_5_4_6_and_mythos() {
        assert!(anthropic_supports_structured_output("claude-haiku-4-5"));
        assert!(anthropic_supports_structured_output("claude-opus-4-6"));
        assert!(anthropic_supports_structured_output("mythos-1"));
    }

    #[test]
    fn anthropic_supports_structured_output_rejects_older_models() {
        assert!(!anthropic_supports_structured_output("claude-3-5-sonnet"));
        assert!(!anthropic_supports_structured_output("claude-3-7-sonnet"));
    }

    #[test]
    fn parse_claude_model_version_handles_dash_and_dot_versions() {
        let dashed = parse_claude_model_version("claude-sonnet-4-6-20260101").unwrap();
        assert_eq!(dashed.family, Some(ClaudeModelFamily::Sonnet));
        assert_eq!((dashed.major, dashed.minor), (4, 6));

        let dotted = parse_claude_model_version("claude-opus-4.8").unwrap();
        assert_eq!(dotted.family, Some(ClaudeModelFamily::Opus));
        assert_eq!((dotted.major, dotted.minor), (4, 8));
    }

    #[test]
    fn parse_claude_model_version_handles_legacy_order() {
        let version = parse_claude_model_version("claude-3-5-sonnet").unwrap();
        assert_eq!(version.family, Some(ClaudeModelFamily::Sonnet));
        assert_eq!((version.major, version.minor), (3, 5));
    }

    // ---- slugify ----

    #[test]
    fn slugify_lowercases_and_replaces_nonalnum_with_dashes() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
    }

    #[test]
    fn slugify_collapses_consecutive_separators() {
        assert_eq!(slugify("a---b   c"), "a-b-c");
    }

    #[test]
    fn slugify_empty_string_returns_empty() {
        assert_eq!(slugify(""), "");
    }

    // ---- messages_have_images ----

    #[test]
    fn messages_have_images_returns_false_when_only_text() {
        assert!(!messages_have_images(&[user_msg("hi")]));
    }

    #[test]
    fn messages_have_images_returns_true_when_user_has_image_part() {
        let m = Message {
            role: Role::User,
            content: Some(Content::Parts(vec![protocol::ContentPart::ImageUrl {
                url: "x".into(),
                label: None,
            }])),
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        };
        assert!(messages_have_images(&[m]));
    }

    #[test]
    fn messages_have_images_ignores_assistant_and_system_roles() {
        let m = Message {
            role: Role::Assistant,
            content: Some(Content::Parts(vec![protocol::ContentPart::ImageUrl {
                url: "x".into(),
                label: None,
            }])),
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        };
        assert!(!messages_have_images(&[m]));
    }

    // ---- Provider basics ----

    fn http_client() -> Client {
        Client::new()
    }

    #[test]
    fn provider_new_strips_trailing_slashes_from_api_base() {
        let p = EngineProvider::new(
            "https://x/".into(),
            "k".into(),
            "openai",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert_eq!(p.api_base, "https://x");
    }

    #[test]
    fn provider_new_resolves_kind_from_provider_type() {
        let p = EngineProvider::new(
            "https://x".into(),
            "k".into(),
            "anthropic",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert_eq!(p.kind, ProviderKind::Anthropic);
    }

    #[test]
    fn provider_tool_calling_default_is_true() {
        let p = EngineProvider::new(
            "".into(),
            "".into(),
            "openai",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert!(p.tool_calling());
    }

    #[test]
    fn provider_with_model_config_overrides_default() {
        let cfg = ModelConfig {
            tool_calling: Some(false),
            ..Default::default()
        };
        let p = EngineProvider::new(
            "".into(),
            "".into(),
            "openai",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        )
        .with_model_config(cfg);
        assert!(!p.tool_calling());
    }

    // ---- context_window_from_models_entry ----

    #[test]
    fn context_window_prefers_max_model_len() {
        let entry = serde_json::json!({"max_model_len": 32768, "context_length": 8192});
        assert_eq!(context_window_from_models_entry(&entry), Some(32768));
    }

    #[test]
    fn context_window_falls_back_to_context_length() {
        let entry = serde_json::json!({"context_length": 256000});
        assert_eq!(context_window_from_models_entry(&entry), Some(256_000));
    }

    #[test]
    fn context_window_falls_back_to_context_window_field() {
        let entry = serde_json::json!({"context_window": 131072});
        assert_eq!(context_window_from_models_entry(&entry), Some(131_072));
    }

    #[test]
    fn context_window_parses_llama_cpp_ctx_size_arg() {
        let entry =
            serde_json::json!({"status": {"args": ["--port", "8080", "--ctx-size", "4096"]}});
        assert_eq!(context_window_from_models_entry(&entry), Some(4096));
    }

    #[test]
    fn context_window_returns_none_when_no_known_fields_present() {
        let entry = serde_json::json!({"id": "m"});
        assert_eq!(context_window_from_models_entry(&entry), None);
    }

    // ---- models_entry_matches ----

    #[test]
    fn models_entry_matches_by_id_case_insensitive() {
        let entry = serde_json::json!({"id": "Kimi-for-Coding"});
        assert!(models_entry_matches(&entry, "kimi-for-coding"));
    }

    #[test]
    fn models_entry_matches_by_display_name_when_id_differs() {
        let entry = serde_json::json!({"id": "kimi-for-coding", "display_name": "Kimi-k2.6", "context_length": 262144});
        assert!(models_entry_matches(&entry, "kimi-k2.6"));
        assert_eq!(context_window_from_models_entry(&entry), Some(262_144));
    }

    #[test]
    fn models_entry_matches_returns_false_when_neither_field_matches() {
        let entry = serde_json::json!({"id": "a", "display_name": "B"});
        assert!(!models_entry_matches(&entry, "c"));
    }

    #[test]
    fn models_entry_matches_handles_missing_display_name() {
        let entry = serde_json::json!({"id": "gpt-5.5"});
        assert!(models_entry_matches(&entry, "gpt-5.5"));
        assert!(!models_entry_matches(&entry, "gpt-5"));
    }

    #[test]
    fn provider_reset_turn_state_clears_to_none() {
        let p = EngineProvider::new(
            "".into(),
            "".into(),
            "codex",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        *p.turn_state.lock().unwrap() = Some("abc".into());
        p.reset_turn_state();
        assert!(p.turn_state.lock().unwrap().is_none());
    }

    // ---- ToolDefinition serialization ----

    #[test]
    fn tool_definition_serializes_with_function_type_tag() {
        let t = ToolDefinition::new(FunctionSchema {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({}),
        });
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "f");
    }

    // ---- ParsedResponse::into_response ----

    #[test]
    fn parsed_into_response_propagates_fields() {
        let p = ParsedResponse {
            content: Some("c".into()),
            reasoning: Some("r".into()),
            reasoning_parts: Vec::new(),
            reasoning_blocks: None,
            tool_calls: vec![ToolCall::new(
                "id".into(),
                FunctionCall {
                    name: "n".into(),
                    arguments: "{}".into(),
                },
            )],
            usage: TokenUsage::default(),
        };
        let r = ChatResponse::from_parsed(p, Some(12.5));
        assert_eq!(r.content.as_deref(), Some("c"));
        assert_eq!(r.reasoning_content.as_deref(), Some("r"));
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tokens_per_sec, Some(12.5));
    }
}
