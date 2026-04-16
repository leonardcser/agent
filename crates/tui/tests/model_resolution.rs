use tui::config::{resolve_model_ref, Config, ResolveModelRefError};

#[test]
fn resolve_model_reference_prefers_exact_key_even_when_model_name_contains_slashes() {
    let yaml = r#"
providers:
  - name: openrouter
    type: openai-compatible
    api_base: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_API_KEY
    models:
      - anthropic/claude-sonnet-4
  - name: anthropic
    type: anthropic
    api_base: https://api.anthropic.com/v1
    api_key_env: ANTHROPIC_API_KEY
    models:
      - claude-sonnet-4
"#;
    let cfg: Config = serde_yml::from_str(yaml).unwrap();
    let resolved = cfg.resolve_models();

    let model = resolve_model_ref(&resolved, "openrouter/anthropic/claude-sonnet-4").unwrap();
    assert_eq!(model.key, "openrouter/anthropic/claude-sonnet-4");
}

#[test]
fn resolve_model_reference_accepts_unique_bare_model_name() {
    let yaml = r#"
providers:
  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5
"#;
    let cfg: Config = serde_yml::from_str(yaml).unwrap();
    let resolved = cfg.resolve_models();

    let model = resolve_model_ref(&resolved, "gpt-5").unwrap();
    assert_eq!(model.key, "openai/gpt-5");
}

#[test]
fn resolve_model_reference_rejects_ambiguous_bare_model_name() {
    let yaml = r#"
providers:
  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5
  - name: openrouter
    type: openai-compatible
    api_base: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_API_KEY
    models:
      - gpt-5
"#;
    let cfg: Config = serde_yml::from_str(yaml).unwrap();
    let resolved = cfg.resolve_models();

    let err = resolve_model_ref(&resolved, "gpt-5").unwrap_err();
    assert_eq!(
        err,
        ResolveModelRefError::Ambiguous {
            reference: "gpt-5".to_string(),
            matches: vec!["openai/gpt-5".to_string(), "openrouter/gpt-5".to_string()],
        }
    );
}
