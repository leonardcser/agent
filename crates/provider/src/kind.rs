use crate::{
    anthropic, chat_completions, openai, CancellationToken, ParsedResponse, ProviderError,
    ProviderStreamEvent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiCompatible,
    OpenAi,
    Codex,
    AnthropicCompatible,
    Anthropic,
    Copilot,
    KimiCode,
}

impl ProviderKind {
    pub fn from_config(provider_type: &str) -> Self {
        match provider_type {
            "openai" => Self::OpenAi,
            "codex" => Self::Codex,
            "anthropic-compatible" => Self::AnthropicCompatible,
            "anthropic" => Self::Anthropic,
            "kimi-code" => Self::KimiCode,
            "copilot" | "github-copilot" => Self::Copilot,
            _ => Self::OpenAiCompatible,
        }
    }

    pub fn from_config_and_url(provider_type: &str, api_base: &str) -> Self {
        if is_kimi_code_api_base(api_base) {
            Self::KimiCode
        } else {
            Self::from_config(provider_type)
        }
    }

    pub fn detect_from_url(api_base: &str) -> Self {
        if is_kimi_code_api_base(api_base) {
            return Self::KimiCode;
        }

        let Some(host) = api_base_host(api_base) else {
            return Self::OpenAiCompatible;
        };
        if host_matches(&host, "api.anthropic.com") {
            Self::Anthropic
        } else if host_matches(&host, "api.openai.com") {
            Self::OpenAi
        } else if host_matches(&host, "chatgpt.com") {
            Self::Codex
        } else if host_matches(&host, "githubcopilot.com") {
            Self::Copilot
        } else {
            Self::OpenAiCompatible
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::OpenAi => "openai",
            Self::Codex => "codex",
            Self::AnthropicCompatible => "anthropic-compatible",
            Self::Anthropic => "anthropic",
            Self::Copilot => "copilot",
            Self::KimiCode => "kimi-code",
        }
    }

    pub fn descriptor(self) -> ProviderDescriptor {
        match self {
            Self::OpenAiCompatible => ProviderDescriptor {
                wire_api: WireApi::ChatCompletions,
                auth: Some(AuthKind::ApiKey),
                api_key_auth: Some(ApiKeyAuth::Bearer),
                anthropic_cache: false,
                mid_turn_reasoning_changes: true,
            },
            Self::OpenAi => ProviderDescriptor {
                wire_api: WireApi::OpenAiResponses,
                auth: Some(AuthKind::ApiKey),
                api_key_auth: Some(ApiKeyAuth::Bearer),
                anthropic_cache: false,
                mid_turn_reasoning_changes: true,
            },
            Self::Codex => ProviderDescriptor {
                wire_api: WireApi::OpenAiResponses,
                auth: None,
                api_key_auth: None,
                anthropic_cache: false,
                mid_turn_reasoning_changes: true,
            },
            Self::AnthropicCompatible => ProviderDescriptor {
                wire_api: WireApi::AnthropicMessages,
                auth: Some(AuthKind::ApiKey),
                api_key_auth: Some(ApiKeyAuth::XApiKey),
                anthropic_cache: true,
                mid_turn_reasoning_changes: true,
            },
            Self::Anthropic => ProviderDescriptor {
                wire_api: WireApi::AnthropicMessages,
                auth: Some(AuthKind::ApiKey),
                api_key_auth: Some(ApiKeyAuth::XApiKey),
                anthropic_cache: true,
                mid_turn_reasoning_changes: true,
            },
            Self::Copilot => ProviderDescriptor {
                wire_api: WireApi::ChatCompletions,
                auth: None,
                api_key_auth: None,
                anthropic_cache: false,
                mid_turn_reasoning_changes: true,
            },
            Self::KimiCode => ProviderDescriptor {
                wire_api: WireApi::AnthropicMessages,
                auth: Some(AuthKind::KimiCodeOAuth),
                api_key_auth: Some(ApiKeyAuth::Bearer),
                anthropic_cache: false,
                mid_turn_reasoning_changes: false,
            },
        }
    }

    pub fn supports_mid_turn_reasoning_changes(self) -> bool {
        self.descriptor().mid_turn_reasoning_changes
    }

    pub fn wire_api(self) -> WireApi {
        self.descriptor().wire_api
    }
}

fn api_base_host(api_base: &str) -> Option<String> {
    let trimmed = smelt_buffer::text::trim_whitespace(api_base);
    let parsed = url::Url::parse(trimmed)
        .or_else(|_| url::Url::parse(&format!("https://{trimmed}")))
        .ok()?;
    parsed.host_str().map(|host| host.to_ascii_lowercase())
}

fn host_matches(host: &str, expected: &str) -> bool {
    host == expected
        || host
            .strip_suffix(expected)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

pub fn is_kimi_code_api_base(api_base: &str) -> bool {
    let trimmed = smelt_buffer::text::trim_whitespace(api_base);
    let Ok(parsed) =
        url::Url::parse(trimmed).or_else(|_| url::Url::parse(&format!("https://{trimmed}")))
    else {
        return false;
    };
    let path = parsed.path().trim_end_matches('/');
    parsed
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("api.kimi.com"))
        && (path == "/coding" || path.starts_with("/coding/"))
}

pub fn api_key_auth(kind: ProviderKind, api_key: &str) -> Option<ApiKeyAuth> {
    if api_key.is_empty() {
        return None;
    }
    kind.descriptor().api_key_auth
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeyAuth {
    Bearer,
    XApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    ApiKey,
    KimiCodeOAuth,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderDescriptor {
    pub wire_api: WireApi,
    pub auth: Option<AuthKind>,
    pub api_key_auth: Option<ApiKeyAuth>,
    pub anthropic_cache: bool,
    pub mid_turn_reasoning_changes: bool,
}

impl ProviderDescriptor {
    pub fn supports_image_tool_results(self) -> bool {
        matches!(
            self.wire_api,
            WireApi::OpenAiResponses | WireApi::AnthropicMessages
        )
    }

    pub fn supports_pdf_tool_results(self) -> bool {
        self.wire_api == WireApi::AnthropicMessages
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireApi {
    ChatCompletions,
    OpenAiResponses,
    AnthropicMessages,
}

impl WireApi {
    pub fn is_anthropic(self) -> bool {
        self == Self::AnthropicMessages
    }

    pub fn copilot_url(self, base: &str) -> String {
        let base = crate::endpoint::trim_api_base(base);
        match self {
            Self::ChatCompletions => format!("{base}/chat/completions"),
            Self::OpenAiResponses => format!("{base}/responses"),
            Self::AnthropicMessages => format!("{base}/v1/messages"),
        }
    }

    pub fn parse_response(self, data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
        match self {
            Self::ChatCompletions => chat_completions::parse_response(data),
            Self::OpenAiResponses => openai::parse_response(data),
            Self::AnthropicMessages => anthropic::parse_response(data),
        }
    }

    pub async fn read_stream(
        self,
        resp: reqwest::Response,
        cancel: &CancellationToken,
        on_event: &(dyn Fn(ProviderStreamEvent<'_>) + Send + Sync),
        now_secs: u64,
    ) -> Result<ParsedResponse, ProviderError> {
        match self {
            Self::ChatCompletions => chat_completions::read_stream(resp, cancel, on_event).await,
            Self::OpenAiResponses => openai::read_stream(resp, cancel, on_event, now_secs).await,
            Self::AnthropicMessages => anthropic::read_stream(resp, cancel, on_event).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_from_config_recognizes_known_types() {
        assert_eq!(ProviderKind::from_config("openai"), ProviderKind::OpenAi);
        assert_eq!(
            ProviderKind::from_config("openrouter"),
            ProviderKind::OpenAiCompatible
        );
        assert_eq!(ProviderKind::from_config("codex"), ProviderKind::Codex);
        assert_eq!(
            ProviderKind::from_config("anthropic"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            ProviderKind::from_config("github-copilot"),
            ProviderKind::Copilot
        );
        assert_eq!(
            ProviderKind::from_config("unknown"),
            ProviderKind::OpenAiCompatible
        );
    }

    #[test]
    fn provider_kind_detect_from_url_matches_known_provider_hosts() {
        assert_eq!(
            ProviderKind::detect_from_url("https://api.openai.com/v1"),
            ProviderKind::OpenAi
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.anthropic.com/v1"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://openrouter.ai/api/v1"),
            ProviderKind::OpenAiCompatible
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://chatgpt.com/backend-api/codex"),
            ProviderKind::Codex
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.githubcopilot.com"),
            ProviderKind::Copilot
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.kimi.com/coding/claude"),
            ProviderKind::KimiCode
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://example.com/v1"),
            ProviderKind::OpenAiCompatible
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.openai.com.evil.test/v1"),
            ProviderKind::OpenAiCompatible
        );
        assert_eq!(
            ProviderKind::detect_from_url("api.anthropic.com/v1"),
            ProviderKind::Anthropic
        );
        assert!(!is_kimi_code_api_base("https://api.kimi.com/codingevil/v1"));
    }

    #[test]
    fn descriptors_keep_identity_separate_from_wire_api() {
        assert_eq!(
            ProviderKind::OpenAiCompatible.wire_api(),
            WireApi::ChatCompletions
        );
        assert_eq!(ProviderKind::OpenAi.wire_api(), WireApi::OpenAiResponses);
        assert_eq!(ProviderKind::Codex.wire_api(), WireApi::OpenAiResponses);
        assert_eq!(
            ProviderKind::Anthropic.wire_api(),
            WireApi::AnthropicMessages
        );
        assert_eq!(
            ProviderKind::KimiCode.wire_api(),
            WireApi::AnthropicMessages
        );
    }

    #[test]
    fn descriptor_reports_wire_tool_result_capabilities() {
        let codex = ProviderKind::Codex.descriptor();
        assert!(codex.supports_image_tool_results());
        assert!(!codex.supports_pdf_tool_results());

        let anthropic = ProviderKind::Anthropic.descriptor();
        assert!(anthropic.supports_image_tool_results());
        assert!(anthropic.supports_pdf_tool_results());

        let compatible = ProviderKind::OpenAiCompatible.descriptor();
        assert!(!compatible.supports_image_tool_results());
        assert!(!compatible.supports_pdf_tool_results());
    }
}
