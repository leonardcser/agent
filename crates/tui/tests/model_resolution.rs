use tui::config::{resolve_model_ref, AuxiliaryTask, Config, ResolveModelRefError};

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
fn auxiliary_model_use_for_defaults_to_all_enabled_and_disables_explicitly() {
    let yaml = r#"
providers:
  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5
      - gpt-5-mini
auxiliary:
  model: openai/gpt-5-mini
  use_for:
    btw: false
"#;
    let cfg: Config = serde_yml::from_str(yaml).unwrap();
    let resolved = cfg.resolve_models();
    let routing = cfg.resolve_auxiliary_routing(&resolved).unwrap();
    let aux_key = "openai/gpt-5-mini";
    assert_eq!(
        routing.model_for(AuxiliaryTask::Title).unwrap().key,
        aux_key
    );
    assert_eq!(
        routing.model_for(AuxiliaryTask::Prediction).unwrap().key,
        aux_key
    );
    assert_eq!(
        routing.model_for(AuxiliaryTask::Compaction).unwrap().key,
        aux_key
    );
    assert!(routing.model_for(AuxiliaryTask::Btw).is_none());
}

#[test]
fn auxiliary_model_unknown_reference_is_rejected() {
    let yaml = r#"
providers:
  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5
auxiliary:
  model: openai/gpt-typo
"#;
    let cfg: Config = serde_yml::from_str(yaml).unwrap();
    let resolved = cfg.resolve_models();
    let err = cfg.resolve_auxiliary_routing(&resolved).unwrap_err();
    assert!(matches!(err, ResolveModelRefError::NotFound { .. }));
}

#[test]
fn auxiliary_model_provider_name_works_for_codex_only() {
    let yaml = r#"
providers:
  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5
      - gpt-5-mini
  - name: chatgpt
    type: codex
    api_base: https://chatgpt.com/backend-api/codex
"#;
    let openai_cfg: Config = serde_yml::from_str(&format!(
        "{yaml}\nauxiliary:\n  model: openai\n",
        yaml = yaml
    ))
    .unwrap();
    let resolved = openai_cfg.resolve_models();
    let err = openai_cfg.resolve_auxiliary_routing(&resolved).unwrap_err();
    assert!(
        matches!(
            err,
            ResolveModelRefError::NotFound { .. } | ResolveModelRefError::Ambiguous { .. }
        ),
        "openai provider name should not be a valid aux ref: {err:?}"
    );

    let codex_cfg: Config = serde_yml::from_str(&format!(
        "{yaml}\nauxiliary:\n  model: chatgpt\n",
        yaml = yaml
    ))
    .unwrap();
    let resolved = codex_cfg.resolve_models();
    let routing = codex_cfg.resolve_auxiliary_routing(&resolved).unwrap();
    let model = routing.model_for(AuxiliaryTask::Title).unwrap();
    assert_eq!(model.provider_name, "chatgpt");
    assert_eq!(model.provider_type, "codex");
}

#[test]
fn auxiliary_routing_yields_no_model_when_unset() {
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
    let routing = cfg.resolve_auxiliary_routing(&resolved).unwrap();
    assert!(routing.model_for(AuxiliaryTask::Title).is_none());
    assert!(routing.model_for(AuxiliaryTask::Prediction).is_none());
    assert!(routing.model_for(AuxiliaryTask::Compaction).is_none());
    assert!(routing.model_for(AuxiliaryTask::Btw).is_none());
}

#[test]
fn auxiliary_model_reference_reuses_shared_resolution_rules() {
    let yaml = r#"
providers:
  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5-mini
  - name: openrouter
    type: openai-compatible
    api_base: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_API_KEY
    models:
      - gpt-5-mini
auxiliary:
  model: gpt-5-mini
  use_for:
    title: true
"#;
    let cfg: Config = serde_yml::from_str(yaml).unwrap();
    let resolved = cfg.resolve_models();

    let err = cfg.resolve_auxiliary_routing(&resolved).unwrap_err();
    assert_eq!(
        err,
        ResolveModelRefError::Ambiguous {
            reference: "gpt-5-mini".to_string(),
            matches: vec![
                "openai/gpt-5-mini".to_string(),
                "openrouter/gpt-5-mini".to_string(),
            ],
        }
    );
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
