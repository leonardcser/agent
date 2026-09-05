use std::collections::{HashMap, HashSet};

use crate::config;
use protocol::{
    AgentMode, ModelCatalogMetadata, ModelConfig, ModelConfigOverrides, ModelTarget,
    ReasoningEffort, RequestAuditMode, RequestRuntimeConfig,
};

/// Immutable precedence values captured from CLI and environment policy at launch.
/// Runtime resolution reapplies these values instead of inferring them from mutable state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartupOverrides {
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub api_key_env: Option<String>,
    pub provider_type: Option<String>,
    pub mode: Option<AgentMode>,
    pub mode_cycle: Option<Vec<AgentMode>>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_cycle: Option<Vec<ReasoningEffort>>,
    pub model_config: ModelConfigOverrides,
    pub settings: HashMap<String, config::SettingValue>,
    pub request_audit_env: Option<RequestAuditMode>,
}

impl StartupOverrides {
    pub fn fixes_model_selection(&self) -> bool {
        self.model.is_some()
    }

    pub fn apply_to_active_model(&self, model: &mut ActiveModel) {
        if let Some(value) = &self.api_base {
            model.api_base = value.clone();
        }
        if let Some(value) = &self.api_key_env {
            model.api_key_env = value.clone();
        }
        if let Some(value) = &self.provider_type {
            model.provider_type = value.clone();
        } else if self.api_base.is_some() {
            model.provider_type = smelt_provider::ProviderKind::detect_from_url(&model.api_base)
                .as_config_str()
                .to_string();
        }
        model.config = model.config.clone().with_overrides(&self.model_config);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelSelectionSource {
    Cli,
    Session,
    Remembered,
    Default,
    /// First picker-visible model in stable configuration or provider priority order.
    CatalogDefault,
    Direct,
    User,
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelUnavailableReason {
    MissingCredentials,
    Unauthenticated,
    InvalidTransport,
    Other(String),
}

impl ModelUnavailableReason {
    pub fn status_reason(&self) -> &'static str {
        match self {
            Self::MissingCredentials => "missing_credentials",
            Self::Unauthenticated => "unauthenticated",
            Self::InvalidTransport => "invalid_transport",
            Self::Other(_) => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAvailability {
    Available,
    StaleCatalog,
    Unavailable { reason: ModelUnavailableReason },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveModel {
    pub key: String,
    pub model_name: String,
    pub display_name: Option<String>,
    pub api_base: String,
    pub api_key_env: String,
    pub provider_type: String,
    pub config: ModelConfig,
    pub catalog: ModelCatalogMetadata,
    pub availability: ModelAvailability,
}

impl ActiveModel {
    pub fn from_resolved(model: &config::ResolvedModel) -> Self {
        Self {
            key: model.key.clone(),
            model_name: model.model_name.clone(),
            display_name: model.display_name.clone(),
            api_base: model.api_base.clone(),
            api_key_env: model.api_key_env.clone(),
            provider_type: model.provider_type.clone(),
            config: model.config.clone(),
            catalog: model.catalog.clone(),
            availability: ModelAvailability::Available,
        }
    }

    pub fn reasoning_catalog(&self) -> ModelCatalogMetadata {
        smelt_provider::reasoning_catalog(
            &self.provider_type,
            &self.api_base,
            &self.model_name,
            &self.config,
            &self.catalog,
        )
    }

    pub fn target(&self, api_key: String) -> ModelTarget {
        ModelTarget {
            model: self.model_name.clone(),
            api_base: self.api_base.clone(),
            api_key,
            provider_type: self.provider_type.clone(),
            config: self.config.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelSelectionState {
    pub requested_key: Option<String>,
    pub requested_by: ModelSelectionSource,
    pub active: Option<ActiveModel>,
}

/// Mutable user selections supplied to the pure resolver at startup.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeSelections {
    pub model: Option<String>,
    pub model_source: ModelSelectionSource,
    pub mode: Option<AgentMode>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// One authoritative resolved application state.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeState {
    pub revision: u64,
    pub settings: config::ResolvedSettings,
    pub defaults: config::DefaultsConfig,
    pub remember: config::RememberConfig,
    pub providers: Vec<config::ProviderConfig>,
    pub available_models: Vec<config::ResolvedModel>,
    pub model_selection: ModelSelectionState,
    pub mode: AgentMode,
    pub mode_cycle: Vec<AgentMode>,
    pub reasoning_effort: ReasoningEffort,
    pub reasoning_cycle: Vec<ReasoningEffort>,
    pub request_audit: RequestAuditMode,
    pub context_window: Option<u32>,
    pub mcp: HashMap<String, crate::mcp::McpServerConfig>,
    pub lsp: crate::lsp::LspConfig,
}

impl RuntimeState {
    pub fn active_model(&self) -> Option<&ActiveModel> {
        self.model_selection.active.as_ref()
    }

    pub fn active_model_mut(&mut self) -> Option<&mut ActiveModel> {
        self.model_selection.active.as_mut()
    }

    /// Install a model selection and refresh its model-dependent reasoning cycle.
    pub fn set_model_selection(
        &mut self,
        selection: ModelSelectionState,
        startup: &StartupOverrides,
    ) {
        self.reasoning_effort =
            reconcile_reasoning_effort(self.reasoning_effort.clone(), selection.active.as_ref());
        self.reasoning_cycle = resolve_reasoning_cycle(startup, selection.active.as_ref());
        self.model_selection = selection;
    }

    pub fn request_runtime_config(&self) -> RequestRuntimeConfig {
        RequestRuntimeConfig {
            redact_secrets: self.settings.redact_secrets,
            cache_ttl_long: self.settings.cache_ttl_long,
            request_audit: self.request_audit,
        }
    }
}

pub struct RuntimeInputs<'a> {
    pub config: &'a config::Config,
    pub startup: &'a StartupOverrides,
    pub available_models: &'a [config::ResolvedModel],
    pub registered_modes: &'a [AgentMode],
    pub selections: &'a RuntimeSelections,
    pub previous: Option<&'a RuntimeState>,
    pub headless: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError(pub String);

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ResolveError {}

fn validate_floats(
    owner: &str,
    values: impl IntoIterator<Item = (&'static str, Option<f64>)>,
) -> Result<(), ResolveError> {
    for (name, value) in values {
        if value.is_some_and(|value| !value.is_finite()) {
            return Err(ResolveError(format!("{owner} '{name}' must be finite")));
        }
    }
    Ok(())
}

fn validate_model_overrides(overrides: &ModelConfigOverrides) -> Result<(), ResolveError> {
    validate_floats(
        "model override",
        [
            ("temperature", overrides.temperature),
            ("top_p", overrides.top_p),
            ("min_p", overrides.min_p),
            ("repeat_penalty", overrides.repeat_penalty),
        ],
    )
}

fn validate_model(model: &config::ResolvedModel) -> Result<(), ResolveError> {
    let owner = format!("model '{}' field", model.key);
    validate_floats(
        &owner,
        [
            ("temperature", model.config.temperature),
            ("top_p", model.config.top_p),
            ("min_p", model.config.min_p),
            ("repeat_penalty", model.config.repeat_penalty),
            ("input_cost", model.config.input_cost),
            ("output_cost", model.config.output_cost),
            ("cache_read_cost", model.config.cache_read_cost),
            ("cache_write_cost", model.config.cache_write_cost),
        ],
    )
}

fn dedup_preserving_order<T: PartialEq>(values: Vec<T>) -> Vec<T> {
    let mut deduped = Vec::with_capacity(values.len());
    for value in values {
        if !deduped.contains(&value) {
            deduped.push(value);
        }
    }
    deduped
}

fn resolve_mode_cycle(inputs: &RuntimeInputs<'_>) -> Vec<AgentMode> {
    let fixed_cycle = inputs.startup.mode_cycle.is_some();
    let mut cycle = inputs
        .startup
        .mode_cycle
        .clone()
        .unwrap_or_else(|| inputs.registered_modes.to_vec());
    if !inputs.registered_modes.is_empty() {
        cycle.retain(|mode| inputs.registered_modes.contains(mode));
        if fixed_cycle && cycle.is_empty() {
            cycle = inputs.registered_modes.to_vec();
        }
    }
    if cycle.is_empty() {
        AgentMode::default_cycle()
    } else {
        dedup_preserving_order(cycle)
    }
}

fn resolve_mode(inputs: &RuntimeInputs<'_>, cycle: &mut Vec<AgentMode>) -> AgentMode {
    let registered = |mode: &AgentMode| {
        inputs.registered_modes.is_empty() || inputs.registered_modes.contains(mode)
    };
    let selected = inputs
        .startup
        .mode
        .clone()
        .or_else(|| inputs.previous.map(|state| state.mode.clone()))
        .or_else(|| {
            inputs
                .config
                .remember
                .mode
                .then(|| inputs.selections.mode.clone())
                .flatten()
        })
        .or_else(|| {
            inputs
                .config
                .defaults
                .mode
                .as_deref()
                .and_then(AgentMode::parse)
        })
        .filter(registered)
        .unwrap_or_else(|| {
            let normal = AgentMode::normal();
            if registered(&normal) {
                normal
            } else {
                inputs.registered_modes.first().cloned().unwrap_or(normal)
            }
        });
    if inputs.startup.mode_cycle.is_none() && !cycle.contains(&selected) {
        cycle.push(selected.clone());
    }
    selected
}

fn resolve_reasoning_cycle(
    startup: &StartupOverrides,
    active: Option<&ActiveModel>,
) -> Vec<ReasoningEffort> {
    let supported = active.map_or_else(
        || vec![ReasoningEffort::Off],
        |model| model.reasoning_catalog().supported_reasoning_efforts,
    );
    let Some(configured) = &startup.reasoning_cycle else {
        return dedup_preserving_order(supported);
    };
    let cycle = dedup_preserving_order(
        configured
            .iter()
            .filter(|effort| supported.is_empty() || supported.contains(effort))
            .cloned()
            .collect(),
    );
    if cycle.is_empty() && !configured.is_empty() {
        dedup_preserving_order(supported)
    } else {
        cycle
    }
}

fn reconcile_reasoning_effort(
    selected: ReasoningEffort,
    active: Option<&ActiveModel>,
) -> ReasoningEffort {
    match active {
        Some(model) => model
            .reasoning_catalog()
            .reconcile_reasoning_effort(selected),
        None => selected,
    }
}

fn direct_model(startup: &StartupOverrides, model_name: &str) -> Result<ActiveModel, ResolveError> {
    let Some(api_base) = startup.api_base.clone() else {
        return Err(ResolveError(format!(
            "unknown model or provider: {model_name}"
        )));
    };
    let provider_type = startup.provider_type.clone().unwrap_or_else(|| {
        smelt_provider::ProviderKind::detect_from_url(&api_base)
            .as_config_str()
            .to_string()
    });
    Ok(ActiveModel {
        key: format!("@direct/{model_name}"),
        model_name: model_name.to_string(),
        display_name: None,
        api_base,
        api_key_env: startup.api_key_env.clone().unwrap_or_default(),
        provider_type,
        config: ModelConfig::default().with_overrides(&startup.model_config),
        catalog: ModelCatalogMetadata::default(),
        availability: ModelAvailability::Available,
    })
}

fn apply_transport_overrides(model: &mut ActiveModel, startup: &StartupOverrides) {
    startup.apply_to_active_model(model);
}

/// Return whether a stable `provider/model` selection is unresolved only
/// because a configured managed provider has no cached catalog yet.
pub fn managed_model_selection_is_pending(
    providers: &[config::ProviderConfig],
    available_models: &[config::ResolvedModel],
    reference: &str,
) -> bool {
    let Some((provider_name, _)) = reference.split_once('/') else {
        return false;
    };
    let managed_provider = providers.iter().any(|provider| {
        provider.name.as_deref() == Some(provider_name)
            && matches!(
                smelt_provider::ProviderKind::from_config_and_url(
                    provider.provider_type.as_deref().unwrap_or_default(),
                    provider.api_base.as_deref().unwrap_or_default(),
                ),
                smelt_provider::ProviderKind::Codex
                    | smelt_provider::ProviderKind::Copilot
                    | smelt_provider::ProviderKind::KimiCode
            )
    });
    managed_provider
        && !available_models
            .iter()
            .any(|model| model.provider_name == provider_name)
}

fn inherit_unavailable_state(inputs: &RuntimeInputs<'_>, active: &mut ActiveModel) {
    let Some(previous) = inputs.previous.and_then(RuntimeState::active_model) else {
        return;
    };
    if previous.key == active.key
        && previous.model_name == active.model_name
        && previous.api_base == active.api_base
        && previous.api_key_env == active.api_key_env
        && previous.provider_type == active.provider_type
        && matches!(
            previous.availability,
            ModelAvailability::Unavailable {
                reason: ModelUnavailableReason::Unauthenticated
                    | ModelUnavailableReason::InvalidTransport
                    | ModelUnavailableReason::Other(_),
            }
        )
    {
        active.availability = previous.availability.clone();
    }
}

fn resolved_selection(
    inputs: &RuntimeInputs<'_>,
    resolved: &config::ResolvedModel,
    source: ModelSelectionSource,
) -> ModelSelectionState {
    let mut active = ActiveModel::from_resolved(resolved);
    apply_transport_overrides(&mut active, inputs.startup);
    inherit_unavailable_state(inputs, &mut active);
    ModelSelectionState {
        requested_key: Some(resolved.key.clone()),
        requested_by: source,
        active: Some(active),
    }
}

fn pending_selection(requested: String, source: ModelSelectionSource) -> ModelSelectionState {
    ModelSelectionState {
        requested_key: Some(requested),
        requested_by: source,
        active: None,
    }
}

fn resolve_requested_selection(
    inputs: &RuntimeInputs<'_>,
    requested: &str,
    source: ModelSelectionSource,
) -> Result<Option<ModelSelectionState>, ResolveError> {
    match config::resolve_model_ref(inputs.available_models, requested) {
        Ok(resolved) => Ok(Some(resolved_selection(inputs, resolved, source))),
        Err(config::ResolveModelRefError::NotFound { .. })
            if source == ModelSelectionSource::Cli && inputs.startup.api_base.is_some() =>
        {
            let mut active = direct_model(inputs.startup, requested)?;
            inherit_unavailable_state(inputs, &mut active);
            Ok(Some(ModelSelectionState {
                requested_key: Some(active.key.clone()),
                requested_by: ModelSelectionSource::Direct,
                active: Some(active),
            }))
        }
        Err(config::ResolveModelRefError::NotFound { .. })
            if managed_model_selection_is_pending(
                &inputs.config.providers,
                inputs.available_models,
                requested,
            ) =>
        {
            Ok(Some(pending_selection(requested.to_string(), source)))
        }
        Err(config::ResolveModelRefError::NotFound { .. })
            if matches!(
                source,
                ModelSelectionSource::Session | ModelSelectionSource::Remembered
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(ResolveError(error.to_string())),
    }
}

fn resolve_model_selection(
    inputs: &RuntimeInputs<'_>,
) -> Result<ModelSelectionState, ResolveError> {
    if let Some(requested) = inputs.startup.model.as_deref() {
        return resolve_requested_selection(inputs, requested, ModelSelectionSource::Cli)?
            .ok_or_else(|| ResolveError(format!("unknown model or provider: {requested}")));
    }

    if let Some(previous) = inputs.previous {
        let mut selection = previous.model_selection.clone();
        if let Some(requested) = selection.requested_key.as_deref() {
            if let Some(active) = selection.active.as_mut() {
                if matches!(
                    config::resolve_model_ref(inputs.available_models, requested),
                    Err(config::ResolveModelRefError::NotFound { .. })
                ) {
                    if selection.requested_by != ModelSelectionSource::Direct {
                        active.availability = ModelAvailability::StaleCatalog;
                    }
                    apply_transport_overrides(active, inputs.startup);
                    return Ok(selection);
                }
            }
            if let Some(resolved) =
                resolve_requested_selection(inputs, requested, selection.requested_by)?
            {
                return Ok(resolved);
            }
        } else if selection.active.is_some() {
            return Ok(selection);
        }
    }

    if inputs.config.remember.model {
        if let Some(requested) = inputs.selections.model.as_deref() {
            if let Some(selection) =
                resolve_requested_selection(inputs, requested, inputs.selections.model_source)?
            {
                return Ok(selection);
            }
        }
    }

    if let Some(requested) = inputs.config.defaults.model.as_deref() {
        return resolve_requested_selection(inputs, requested, ModelSelectionSource::Default)?
            .ok_or_else(|| ResolveError(format!("unknown model or provider: {requested}")));
    }

    // Static catalogs retain configuration order, while managed catalogs retain
    // provider recommendation order. Prefer their first picker-visible entry,
    // falling back to the first internal entry when a catalog has no visible models.
    let Some(resolved) = inputs
        .available_models
        .iter()
        .find(|model| model.catalog.show_in_picker)
        .or_else(|| inputs.available_models.first())
    else {
        if inputs.startup.api_base.is_some() {
            return Err(ResolveError(
                "--model is required when using --api-base without a configured model".into(),
            ));
        }
        return Ok(ModelSelectionState::default());
    };
    Ok(resolved_selection(
        inputs,
        resolved,
        ModelSelectionSource::CatalogDefault,
    ))
}

fn same_context_target(left: Option<&ActiveModel>, right: Option<&ActiveModel>) -> bool {
    matches!((left, right), (Some(left), Some(right))
        if left.key == right.key
            && left.model_name == right.model_name
            && left.api_base == right.api_base
            && left.api_key_env == right.api_key_env
            && left.provider_type == right.provider_type
            && left.config == right.config)
}

/// Resolve a coherent runtime snapshot without side effects.
pub fn resolve_runtime(inputs: RuntimeInputs<'_>) -> Result<RuntimeState, ResolveError> {
    validate_model_overrides(&inputs.startup.model_config)?;
    for provider_type in ["codex", "copilot", "kimi-code"] {
        let providers = inputs
            .config
            .providers
            .iter()
            .filter(|provider| config::is_managed_provider_kind(provider, provider_type))
            .collect::<Vec<_>>();
        if providers.len() > 1 {
            let names = providers
                .iter()
                .filter_map(|provider| provider.name.as_deref())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ResolveError(format!(
                "managed provider type '{provider_type}' must be unique; found: {names}"
            )));
        }
    }
    let mut model_keys = HashSet::with_capacity(inputs.available_models.len());
    for model in inputs.available_models {
        validate_model(model)?;
        if !model_keys.insert(model.key.as_str()) {
            return Err(ResolveError(format!(
                "duplicate resolved model key '{}'",
                model.key
            )));
        }
    }

    let mut settings = inputs.config.settings.clone();
    for (key, value) in &inputs.startup.settings {
        settings.set(key, value).map_err(ResolveError)?;
    }
    if inputs.headless {
        settings.auto_compact = true;
    }
    let request_audit = inputs
        .startup
        .request_audit_env
        .unwrap_or_else(|| RequestAuditMode::parse(&settings.request_audit).unwrap_or_default());

    let model_selection = resolve_model_selection(&inputs)?;
    let mut mode_cycle = resolve_mode_cycle(&inputs);
    let mode = resolve_mode(&inputs, &mut mode_cycle);
    let configured_reasoning_effort = inputs
        .config
        .defaults
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse);
    let explicit_reasoning_effort = inputs
        .startup
        .reasoning_effort
        .as_ref()
        .or(configured_reasoning_effort.as_ref());
    if let (Some(effort), Some(model)) =
        (explicit_reasoning_effort, model_selection.active.as_ref())
    {
        if !model.reasoning_catalog().supports_reasoning_effort(effort) {
            return Err(ResolveError(format!(
                "reasoning effort '{}' is not supported by model '{}'",
                effort.label(),
                model.key
            )));
        }
    }
    let reasoning_effort = inputs
        .startup
        .reasoning_effort
        .clone()
        .or_else(|| inputs.previous.map(|state| state.reasoning_effort.clone()))
        .or_else(|| {
            inputs
                .config
                .remember
                .reasoning_effort
                .then(|| inputs.selections.reasoning_effort.clone())
                .flatten()
        })
        .or(configured_reasoning_effort)
        .or_else(|| {
            model_selection
                .active
                .as_ref()
                .and_then(|model| model.reasoning_catalog().default_reasoning_effort)
        })
        .unwrap_or(ReasoningEffort::Off);
    let reasoning_effort =
        reconcile_reasoning_effort(reasoning_effort, model_selection.active.as_ref());
    let reasoning_cycle = resolve_reasoning_cycle(inputs.startup, model_selection.active.as_ref());
    let context_window = inputs.previous.and_then(|previous| {
        same_context_target(
            previous.model_selection.active.as_ref(),
            model_selection.active.as_ref(),
        )
        .then_some(previous.context_window)
        .flatten()
    });

    Ok(RuntimeState {
        revision: inputs.previous.map_or(0, |state| state.revision),
        settings,
        defaults: inputs.config.defaults.clone(),
        remember: inputs.config.remember.clone(),
        providers: inputs.config.providers.clone(),
        available_models: inputs.available_models.to_vec(),
        model_selection,
        mode,
        mode_cycle,
        reasoning_effort,
        reasoning_cycle,
        request_audit,
        context_window,
        mcp: inputs.config.mcp.clone(),
        lsp: inputs.config.lsp.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_config() -> config::Config {
        config::Config {
            providers: vec![config::ProviderConfig {
                name: Some("test".into()),
                provider_type: Some("openai-compatible".into()),
                api_base: Some("https://example.test/v1".into()),
                api_key_env: Some("TEST_KEY".into()),
                models: vec![
                    ModelConfig {
                        name: Some("model-a".into()),
                        ..Default::default()
                    },
                    ModelConfig {
                        name: Some("model-b".into()),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn startup_and_noop_resolution_are_identical() {
        let config = provider_config();
        let models = config.resolve_models();
        let startup = StartupOverrides::default();
        let selections = RuntimeSelections::default();
        let modes = vec![AgentMode::normal()];
        let initial = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &modes,
            selections: &selections,
            previous: None,
            headless: false,
        })
        .unwrap();
        let resolved_again = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &modes,
            selections: &selections,
            previous: Some(&initial),
            headless: false,
        })
        .unwrap();

        assert_eq!(resolved_again, initial);
    }

    #[test]
    fn remembered_reasoning_distinguishes_missing_from_explicit_off() {
        let mut config = provider_config();
        config.defaults.reasoning_effort = Some("high".into());
        let models = config.resolve_models();
        let resolve = |reasoning_effort| {
            resolve_runtime(RuntimeInputs {
                config: &config,
                startup: &StartupOverrides::default(),
                available_models: &models,
                registered_modes: &[],
                selections: &RuntimeSelections {
                    reasoning_effort,
                    ..Default::default()
                },
                previous: None,
                headless: false,
            })
            .unwrap()
            .reasoning_effort
        };

        assert_eq!(resolve(None), ReasoningEffort::High);
        assert_eq!(resolve(Some(ReasoningEffort::Off)), ReasoningEffort::Off);
    }

    #[test]
    fn changed_target_metadata_clears_resolved_context_window() {
        let config = provider_config();
        let mut models = config.resolve_models();
        let startup = StartupOverrides::default();
        let selections = RuntimeSelections::default();
        let mut initial = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &selections,
            previous: None,
            headless: false,
        })
        .unwrap();
        initial.context_window = Some(128_000);
        models[0].api_base = "https://changed.example/v1".into();

        let next = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &selections,
            previous: Some(&initial),
            headless: false,
        })
        .unwrap();

        assert_eq!(next.active_model().unwrap().key, "test/model-a");
        assert_eq!(next.context_window, None);
    }

    #[test]
    fn empty_catalog_keeps_pending_selection_without_empty_active_model() {
        let mut config = config::Config {
            providers: vec![config::ProviderConfig {
                name: Some("managed".into()),
                provider_type: Some("codex".into()),
                api_base: Some("https://chatgpt.com/backend-api/codex".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.remember.model = true;
        let startup = StartupOverrides::default();
        let selections = RuntimeSelections {
            model: Some("managed/model-a".into()),
            model_source: ModelSelectionSource::Session,
            ..Default::default()
        };
        let state = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &[],
            registered_modes: &[],
            selections: &selections,
            previous: None,
            headless: false,
        })
        .unwrap();

        assert_eq!(
            state.model_selection.requested_key.as_deref(),
            Some("managed/model-a")
        );
        assert!(state.active_model().is_none());
    }

    #[test]
    fn immutable_overrides_reapply_to_noop_resolution() {
        let config = provider_config();
        let models = config.resolve_models();
        let startup = StartupOverrides {
            api_base: Some("https://cli.example/v1".into()),
            provider_type: Some("openai".into()),
            model_config: ModelConfigOverrides {
                temperature: Some(0.2),
                tool_calling: Some(false),
                ..Default::default()
            },
            settings: HashMap::from([("cache_ttl_long".into(), config::SettingValue::Bool(true))]),
            request_audit_env: Some(RequestAuditMode::Off),
            ..Default::default()
        };
        let selections = RuntimeSelections::default();
        let initial = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &selections,
            previous: None,
            headless: false,
        })
        .unwrap();
        let next = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &selections,
            previous: Some(&initial),
            headless: false,
        })
        .unwrap();

        assert_eq!(next, initial);
        let active = next.active_model().unwrap();
        assert_eq!(active.api_base, "https://cli.example/v1");
        assert_eq!(active.provider_type, "openai");
        assert_eq!(active.config.temperature, Some(0.2));
        assert_eq!(active.config.tool_calling, Some(false));
        assert!(next.settings.cache_ttl_long);
        assert_eq!(next.request_audit, RequestAuditMode::Off);
    }

    #[test]
    fn cli_model_selection_reapplies_over_previous_runtime_selection() {
        let config = provider_config();
        let models = config.resolve_models();
        let startup = StartupOverrides {
            model: Some("test/model-b".into()),
            ..Default::default()
        };
        let mut previous = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap();
        previous.model_selection = ModelSelectionState {
            requested_key: Some("test/model-a".into()),
            requested_by: ModelSelectionSource::User,
            active: Some(ActiveModel::from_resolved(&models[0])),
        };

        let resolved = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: Some(&previous),
            headless: false,
        })
        .unwrap();

        assert_eq!(resolved.active_model().unwrap().key, "test/model-b");
        assert_eq!(
            resolved.model_selection.requested_by,
            ModelSelectionSource::Cli
        );
    }

    #[test]
    fn missing_remembered_static_model_falls_back_to_catalog_default() {
        let config = provider_config();
        let models = config.resolve_models();
        let resolved = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides::default(),
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections {
                model: Some("removed/model".into()),
                model_source: ModelSelectionSource::Remembered,
                ..Default::default()
            },
            previous: None,
            headless: false,
        })
        .unwrap();

        assert_eq!(resolved.active_model().unwrap().key, "test/model-a");
        assert_eq!(
            resolved.model_selection.requested_by,
            ModelSelectionSource::CatalogDefault
        );
    }

    #[test]
    fn missing_remembered_managed_model_uses_provider_priority() {
        let config = config::Config {
            providers: vec![config::ProviderConfig {
                name: Some("codex".into()),
                provider_type: Some("codex".into()),
                api_base: Some("https://chatgpt.com/backend-api/codex".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let metadata = |id: &str| protocol::ModelMetadata {
            id: id.into(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            supports_reasoning: None,
            supports_fast_mode: None,
            input_modalities: None,
            catalog: protocol::ModelCatalogMetadata::default(),
        };
        let mut models = config.resolve_models();
        config.inject_codex_models(
            &mut models,
            &[metadata("gpt-5.6-sol"), metadata("gpt-5.3-codex-spark")],
        );

        let resolved = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides::default(),
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections {
                model: Some("codex/removed-model".into()),
                model_source: ModelSelectionSource::Remembered,
                ..Default::default()
            },
            previous: None,
            headless: false,
        })
        .unwrap();

        assert_eq!(resolved.active_model().unwrap().key, "codex/gpt-5.6-sol");
        assert_eq!(
            resolved.model_selection.requested_by,
            ModelSelectionSource::CatalogDefault
        );
    }

    #[test]
    fn catalog_default_skips_hidden_models_but_explicit_selection_accepts_them() {
        let config = provider_config();
        let mut models = config.resolve_models();
        models[0].catalog.show_in_picker = false;
        models[0].catalog.default_reasoning_effort = Some(ReasoningEffort::Ultra);
        models[0].catalog.supported_reasoning_efforts = vec![
            ReasoningEffort::Low,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
            ReasoningEffort::Ultra,
        ];
        models[1].catalog.default_reasoning_effort = Some(ReasoningEffort::Low);

        let default = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides::default(),
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap();
        assert_eq!(default.active_model().unwrap().key, "test/model-b");
        assert_eq!(default.reasoning_effort, ReasoningEffort::Low);

        let explicit = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides {
                model: Some("test/model-a".into()),
                ..Default::default()
            },
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap();
        assert_eq!(explicit.active_model().unwrap().key, "test/model-a");
        assert_eq!(explicit.reasoning_effort, ReasoningEffort::Ultra);
        assert_eq!(
            explicit.reasoning_cycle,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
                ReasoningEffort::Ultra,
            ]
        );
    }

    #[test]
    fn budget_reasoning_accepts_max_from_cli_and_defaults() {
        let mut config = provider_config();
        config.providers[0].provider_type = Some("anthropic".into());
        config.providers[0].models[0].name = Some("claude-sonnet-4-5".into());
        let models = config.resolve_models();

        for from_cli in [false, true] {
            config.defaults.reasoning_effort = (!from_cli).then(|| "max".into());
            let resolved = resolve_runtime(RuntimeInputs {
                config: &config,
                startup: &StartupOverrides {
                    model: Some(models[0].key.clone()),
                    reasoning_effort: from_cli.then_some(ReasoningEffort::Max),
                    ..Default::default()
                },
                available_models: &models,
                registered_modes: &[],
                selections: &RuntimeSelections::default(),
                previous: None,
                headless: false,
            })
            .unwrap();
            assert_eq!(resolved.reasoning_effort, ReasoningEffort::Max);
            assert!(resolved.reasoning_cycle.contains(&ReasoningEffort::Max));
        }
    }

    #[test]
    fn explicit_unsupported_startup_reasoning_is_rejected() {
        let config = provider_config();
        let mut models = config.resolve_models();
        models[0].catalog.supported_reasoning_efforts =
            vec![ReasoningEffort::Low, ReasoningEffort::High];

        let error = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides {
                reasoning_effort: Some(ReasoningEffort::Ultra),
                ..Default::default()
            },
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap_err();

        assert_eq!(
            error.0,
            "reasoning effort 'ultra' is not supported by model 'test/model-a'"
        );
    }

    #[test]
    fn cli_managed_selection_remains_pending_with_an_empty_catalog() {
        let config = config::Config {
            providers: vec![config::ProviderConfig {
                name: Some("codex".into()),
                provider_type: Some("codex".into()),
                api_base: Some("https://chatgpt.com/backend-api/codex".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let state = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides {
                model: Some("codex/future-model".into()),
                ..Default::default()
            },
            available_models: &[],
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap();

        assert_eq!(
            state.model_selection.requested_key.as_deref(),
            Some("codex/future-model")
        );
        assert_eq!(
            state.model_selection.requested_by,
            ModelSelectionSource::Cli
        );
        assert!(state.active_model().is_none());
    }

    #[test]
    fn model_selection_precedence_ends_with_catalog_default() {
        let mut config = provider_config();
        config.defaults.model = Some("test/model-b".into());
        let models = config.resolve_models();
        let selections = RuntimeSelections {
            model: Some("test/model-a".into()),
            model_source: ModelSelectionSource::Remembered,
            ..Default::default()
        };
        let resolve = |config: &config::Config,
                       startup: &StartupOverrides,
                       selections: &RuntimeSelections| {
            resolve_runtime(RuntimeInputs {
                config,
                startup,
                available_models: &models,
                registered_modes: &[],
                selections,
                previous: None,
                headless: false,
            })
            .unwrap()
        };

        let cli = resolve(
            &config,
            &StartupOverrides {
                model: Some("test/model-b".into()),
                ..Default::default()
            },
            &selections,
        );
        assert_eq!(cli.active_model().unwrap().key, "test/model-b");
        assert_eq!(cli.model_selection.requested_by, ModelSelectionSource::Cli);

        let remembered = resolve(&config, &StartupOverrides::default(), &selections);
        assert_eq!(remembered.active_model().unwrap().key, "test/model-a");
        assert_eq!(
            remembered.model_selection.requested_by,
            ModelSelectionSource::Remembered
        );

        config.remember.model = false;
        let defaulted = resolve(&config, &StartupOverrides::default(), &selections);
        assert_eq!(defaulted.active_model().unwrap().key, "test/model-b");
        assert_eq!(
            defaulted.model_selection.requested_by,
            ModelSelectionSource::Default
        );

        config.defaults.model = None;
        let first = resolve(&config, &StartupOverrides::default(), &selections);
        assert_eq!(first.active_model().unwrap().key, "test/model-a");
        assert_eq!(
            first.model_selection.requested_by,
            ModelSelectionSource::CatalogDefault
        );
    }

    #[test]
    fn pending_selection_activates_when_its_model_appears() {
        let mut empty_config = config::Config {
            providers: vec![config::ProviderConfig {
                name: Some("test".into()),
                provider_type: Some("codex".into()),
                api_base: Some("https://chatgpt.com/backend-api/codex".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        empty_config.remember.model = true;
        let startup = StartupOverrides::default();
        let selections = RuntimeSelections {
            model: Some("test/model-b".into()),
            model_source: ModelSelectionSource::Session,
            ..Default::default()
        };
        let pending = resolve_runtime(RuntimeInputs {
            config: &empty_config,
            startup: &startup,
            available_models: &[],
            registered_modes: &[],
            selections: &selections,
            previous: None,
            headless: false,
        })
        .unwrap();
        let config = provider_config();
        let models = config.resolve_models();

        let resolved = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: Some(&pending),
            headless: false,
        })
        .unwrap();

        assert_eq!(resolved.active_model().unwrap().key, "test/model-b");
        assert_eq!(
            resolved.model_selection.requested_by,
            ModelSelectionSource::Session
        );
    }

    #[test]
    fn runtime_without_selection_activates_first_model_when_catalog_appears() {
        let empty_config = config::Config::default();
        let startup = StartupOverrides::default();
        let selections = RuntimeSelections::default();
        let empty = resolve_runtime(RuntimeInputs {
            config: &empty_config,
            startup: &startup,
            available_models: &[],
            registered_modes: &[],
            selections: &selections,
            previous: None,
            headless: false,
        })
        .unwrap();
        let config = provider_config();
        let models = config.resolve_models();
        let resolved = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &selections,
            previous: Some(&empty),
            headless: false,
        })
        .unwrap();

        assert_eq!(resolved.active_model().unwrap().key, "test/model-a");
    }

    #[test]
    fn changed_transport_does_not_inherit_unavailable_status() {
        let config = provider_config();
        let mut models = config.resolve_models();
        let startup = StartupOverrides::default();
        let mut initial = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap();
        initial.active_model_mut().unwrap().availability = ModelAvailability::Unavailable {
            reason: ModelUnavailableReason::MissingCredentials,
        };
        models[0].api_key_env.clear();

        let next = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: Some(&initial),
            headless: false,
        })
        .unwrap();

        assert_eq!(
            next.active_model().unwrap().availability,
            ModelAvailability::Available
        );
    }

    #[test]
    fn runtime_cycles_are_deduplicated_without_reordering() {
        let config = provider_config();
        let models = config.resolve_models();
        let normal = AgentMode::normal();
        let plan = AgentMode::parse("plan").unwrap();
        let startup = StartupOverrides {
            mode_cycle: Some(vec![normal.clone(), plan.clone(), normal.clone()]),
            reasoning_cycle: Some(vec![
                ReasoningEffort::Off,
                ReasoningEffort::High,
                ReasoningEffort::Off,
            ]),
            ..Default::default()
        };
        let state = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &startup,
            available_models: &models,
            registered_modes: &[normal.clone(), plan.clone()],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap();

        assert_eq!(state.mode_cycle, vec![normal, plan]);
        assert_eq!(
            state.reasoning_cycle,
            vec![ReasoningEffort::Off, ReasoningEffort::High]
        );
    }

    #[test]
    fn reasoning_cycle_falls_back_when_all_configured_levels_are_unsupported() {
        let models = provider_config().resolve_models();
        let mut active = ActiveModel::from_resolved(&models[0]);
        active.catalog.supported_reasoning_efforts =
            vec![ReasoningEffort::Low, ReasoningEffort::High];
        let startup = StartupOverrides {
            reasoning_cycle: Some(vec![ReasoningEffort::Ultra]),
            ..Default::default()
        };
        assert_eq!(
            resolve_reasoning_cycle(&startup, Some(&active)),
            vec![ReasoningEffort::Low, ReasoningEffort::High]
        );
        let disabled = StartupOverrides {
            reasoning_cycle: Some(Vec::new()),
            ..startup
        };
        assert!(resolve_reasoning_cycle(&disabled, Some(&active)).is_empty());
    }

    #[test]
    fn cli_cycles_remain_immutable_when_current_selections_are_outside_them() {
        let config = provider_config();
        let models = config.resolve_models();
        let normal = AgentMode::normal();
        let plan = AgentMode::parse("plan").unwrap();
        let state = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides {
                mode: Some(normal.clone()),
                mode_cycle: Some(vec![plan.clone()]),
                reasoning_effort: Some(ReasoningEffort::High),
                reasoning_cycle: Some(vec![ReasoningEffort::Off]),
                ..Default::default()
            },
            available_models: &models,
            registered_modes: &[normal.clone(), plan.clone()],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap();

        assert_eq!(state.mode, normal);
        assert_eq!(state.mode_cycle, vec![plan]);
        assert_eq!(state.reasoning_effort, ReasoningEffort::High);
        assert_eq!(state.reasoning_cycle, vec![ReasoningEffort::Off]);
    }

    #[test]
    fn runtime_rejects_duplicate_model_keys() {
        let config = provider_config();
        let mut models = config.resolve_models();
        models.push(models[0].clone());
        let error = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides::default(),
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap_err();

        assert_eq!(error.0, "duplicate resolved model key 'test/model-a'");
    }

    #[test]
    fn runtime_rejects_duplicate_managed_provider_kinds() {
        let config = config::Config {
            providers: vec![
                config::ProviderConfig {
                    name: Some("kimi-code".into()),
                    provider_type: Some("kimi-code".into()),
                    ..Default::default()
                },
                config::ProviderConfig {
                    name: Some("second-kimi".into()),
                    api_base: Some(smelt_provider::kimi_code::API_BASE.into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let error = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides::default(),
            available_models: &[],
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap_err();

        assert_eq!(
            error.0,
            "managed provider type 'kimi-code' must be unique; found: kimi-code, second-kimi"
        );
    }

    #[test]
    fn runtime_rejects_non_finite_model_values() {
        let config = provider_config();
        let mut models = config.resolve_models();
        models[0].config.temperature = Some(f64::INFINITY);
        let error = resolve_runtime(RuntimeInputs {
            config: &config,
            startup: &StartupOverrides::default(),
            available_models: &models,
            registered_modes: &[],
            selections: &RuntimeSelections::default(),
            previous: None,
            headless: false,
        })
        .unwrap_err();

        assert_eq!(
            error.0,
            "model 'test/model-a' field 'temperature' must be finite"
        );
    }
}
