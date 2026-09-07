//! Application operations exposed through the scoped Lua capability host.

use crate::app::{LuaBringUpError, NotificationOperation, TuiApp};
use smelt_core::transcript_model::ConfirmChoice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LuaReloadKind {
    Manual,
    AutoConfig,
}

pub(crate) struct LuaRuntimeController {
    generation: crate::lua::LuaGeneration,
    wakeup_tx: tokio::sync::mpsc::UnboundedSender<()>,
    wakeup_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    pending_runtime_reconcile: bool,
    pending_reload: Option<LuaReloadKind>,
    failure: Option<LuaBringUpError>,
}

impl LuaRuntimeController {
    pub(super) fn new(generation: crate::lua::LuaGeneration) -> Self {
        let (wakeup_tx, wakeup_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = generation.shared().wakeup_tx.set(wakeup_tx.clone());
        Self {
            generation,
            wakeup_tx,
            wakeup_rx,
            pending_runtime_reconcile: false,
            pending_reload: None,
            failure: None,
        }
    }

    pub(super) fn wakeup_sender(&self) -> tokio::sync::mpsc::UnboundedSender<()> {
        self.wakeup_tx.clone()
    }

    pub(super) async fn receive_wakeup(&mut self) -> Option<()> {
        self.wakeup_rx.recv().await
    }

    pub(super) fn drain_wakeups(&mut self) {
        while self.wakeup_rx.try_recv().is_ok() {}
    }

    #[cfg(test)]
    pub(super) fn try_receive_wakeup(&mut self) -> bool {
        self.wakeup_rx.try_recv().is_ok()
    }

    fn schedule_reload(&mut self, kind: LuaReloadKind) -> bool {
        let was_pending = self.pending_reload.is_some();
        if !was_pending || matches!(kind, LuaReloadKind::Manual) {
            self.pending_reload = Some(kind);
        }
        !was_pending
    }

    fn pending_reload(&self) -> bool {
        self.pending_reload.is_some()
    }

    fn take_pending_reload(&mut self) -> Option<LuaReloadKind> {
        self.pending_reload.take()
    }

    fn clear_pending_reload(&mut self) {
        self.pending_reload = None;
    }

    fn schedule_runtime_reconcile(&mut self) {
        self.pending_runtime_reconcile = true;
    }

    fn take_runtime_reconcile(&mut self) -> bool {
        std::mem::take(&mut self.pending_runtime_reconcile)
    }

    #[cfg(test)]
    pub(super) fn runtime_reconcile_pending(&self) -> bool {
        self.pending_runtime_reconcile
    }

    pub(super) fn failure(&self) -> Option<&LuaBringUpError> {
        self.failure.as_ref()
    }

    fn set_failure(&mut self, failure: LuaBringUpError) {
        self.failure = Some(failure);
    }

    fn clear_failure(&mut self) {
        self.failure = None;
    }

    fn commit_generation(&mut self, generation: crate::lua::LuaGeneration) {
        self.generation.retire();
        self.generation = generation;
    }
}

impl std::ops::Deref for LuaRuntimeController {
    type Target = crate::lua::LuaGeneration;

    fn deref(&self) -> &Self::Target {
        &self.generation
    }
}

impl std::ops::DerefMut for LuaRuntimeController {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.generation
    }
}

fn bring_up_error(
    phase: &'static str,
    path: Option<std::path::PathBuf>,
    message: String,
) -> LuaBringUpError {
    LuaBringUpError {
        message,
        location: smelt_core::lua::LuaLoadFailureLocation { phase, path },
    }
}

fn startup_auth_error(
    runtime: &mut smelt_core::RuntimeState,
    managed_models: &smelt_core::ManagedModels,
) -> Option<String> {
    let active = runtime.active_model()?;
    let error = if let Some(provider) =
        engine::auth::AuthProvider::from_provider_type(&active.provider_type)
    {
        (!managed_models.provider(provider).authenticated)
            .then(|| format!("not logged in to managed provider {provider:?}"))
    } else {
        crate::app::agent::lookup_api_key(&active.api_key_env, |name| std::env::var(name))
            .err()
            .map(|error| error.message())
    };
    if error.is_some() {
        if let Some(active) = runtime.active_model_mut() {
            active.availability = smelt_core::ModelAvailability::Unavailable {
                reason: smelt_core::ModelUnavailableReason::MissingCredentials,
            };
        }
    }
    error
}

struct LuaTuiGeneration {
    ui: crate::smelt_edit::Ui,
    paint_registry: crate::lua::paint::PaintRegistry,
    picker_state: std::collections::HashMap<crate::smelt_edit::WinId, crate::picker::PickerState>,
    placeholders: crate::app::PlaceholderState,
    busy_stack: crate::app::BusyStack,
}

impl LuaReloadKind {
    fn refresh_agent_inputs(&self) -> bool {
        matches!(self, Self::Manual)
    }
}

impl TuiApp {
    /// Route a runtime error through Lua while lending the frontend host only
    /// for the notification callback's dynamic extent.
    pub(crate) fn record_lua_error(&mut self, message: String) {
        let lua = self.lua.execution();
        crate::lua::scope_app(self, move || lua.record_error(message));
    }

    /// Run a Lua command line through the shared dispatcher. Bare names (`"btw foo"`)
    /// are normalized to prompt-command syntax so Lua APIs keep their historical shape,
    /// while explicit `:`, `/`, and `!` prefixes keep their typed meaning.
    pub(crate) fn apply_lua_command(&mut self, line: &str) {
        let trimmed = smelt_buffer::text::trim_start_whitespace(line);
        let normalized: std::borrow::Cow<str> =
            if trimmed.starts_with('/') || trimmed.starts_with(':') || trimmed.starts_with('!') {
                std::borrow::Cow::Borrowed(line)
            } else {
                std::borrow::Cow::Owned(format!("/{trimmed}"))
            };
        match crate::commands::run_command_with_context(
            self,
            &normalized,
            crate::commands::CommandContext::lua(),
        ) {
            crate::app::CommandAction::Exec(handle) => {
                self.overlays.install_execution(handle);
            }
            crate::app::CommandAction::Continue => {}
        }
    }

    /// `/reload` entry point. Runs the transactional candidate pipeline and
    /// reports its outcome with a user-facing toast.
    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn reload_lua(&mut self) {
        self.reload_lua_inner(LuaReloadKind::Manual);
    }

    /// Auto-reload entry point for Lua config edits. Keeps prompt inputs stable
    /// so changing AGENTS.md, skills, or `--system-prompt` only affects the
    /// agent after a manual `/reload`.
    pub(crate) fn reload_lua_config(&mut self) {
        self.reload_lua_inner(LuaReloadKind::AutoConfig);
    }

    fn reload_lua_inner(&mut self, kind: LuaReloadKind) {
        self.lua.clear_pending_reload();
        let err = self.bring_up_lua("reload", kind.refresh_agent_inputs());
        match err {
            Some(error) => {
                let message = format!("lua reload: {error}");
                if self
                    .lua
                    .failure()
                    .is_none_or(|failure| failure.message != message)
                {
                    self.notify_workspace_error_sticky(message.clone());
                }
                self.lua.set_failure(LuaBringUpError {
                    message,
                    location: error.location,
                });
            }
            None if self.lua.warnings().is_empty() => {
                self.lua.clear_failure();
                self.notify("lua reloaded".into());
            }
            None => {
                self.lua.clear_failure();
            }
        }
    }

    /// Mark a full reload for the next point where no turn or modal can hold
    /// callbacks that the reload would wipe. Returns `true` for a new request
    /// and `false` when one was already pending.
    pub(crate) fn schedule_lua_reload(&mut self) -> bool {
        self.lua.schedule_reload(LuaReloadKind::Manual)
    }

    /// Mark a Lua-config-only reload for auto-reload. If a full reload is
    /// already pending, keep it full.
    pub(crate) fn schedule_lua_config_reload(&mut self) -> bool {
        self.lua.schedule_reload(LuaReloadKind::AutoConfig)
    }

    pub(crate) fn schedule_runtime_reconcile(&mut self) {
        self.lua.schedule_runtime_reconcile();
    }

    pub(crate) fn lua_reload_pending(&self) -> bool {
        self.lua.pending_reload()
    }

    pub(crate) fn lua_reload_failure(&self) -> Option<&LuaBringUpError> {
        self.lua.failure()
    }

    #[cfg(test)]
    pub(crate) fn lua_runtime_reconcile_pending(&self) -> bool {
        self.lua.runtime_reconcile_pending()
    }

    #[cfg(test)]
    pub(crate) fn try_receive_lua_wakeup(&mut self) -> bool {
        self.lua.try_receive_wakeup()
    }

    pub(crate) fn drain_idle_work(&mut self) -> bool {
        let mut did_work = self.dismiss_expired_notification();
        did_work |= self.expire_pending_keymap_chord();
        did_work |= self.poll_managed_auth();
        did_work |= self.try_perform_scheduled_runtime_reconcile();
        did_work |= self.try_perform_scheduled_cwd_change();
        did_work |= self.try_perform_scheduled_lua_reload();
        did_work |= self.drain_deferred_layout();
        did_work |= self.conversation.drain_transcript_compaction_slice();
        did_work
    }

    pub(crate) fn drain_deferred_layout(&mut self) -> bool {
        if smelt_core::host::host_access_active() || !self.lua.shared().layout_refresh_pending() {
            return false;
        }
        self.refresh_main_layout();
        true
    }

    pub(crate) fn try_perform_scheduled_runtime_reconcile(&mut self) -> bool {
        if !self.lua.take_runtime_reconcile() {
            return false;
        }
        if let Err(error) = self.reconcile_committed_lua_runtime() {
            self.notify_workspace_error_sticky(error);
        }
        true
    }

    fn try_perform_scheduled_lua_reload(&mut self) -> bool {
        if !self.can_reload_lua_now() {
            return false;
        }
        let Some(kind) = self.lua.take_pending_reload() else {
            return false;
        };
        self.reload_lua_inner(kind);
        true
    }

    pub(crate) fn can_reload_lua_now(&self) -> bool {
        !self.prompt_input_is_busy() && self.ui.active_modal().is_none()
    }

    /// Finish loading the generation-zero VM after the frontend and terminal
    /// exist. Early files have already run so dynamic CLI flags are available;
    /// every remaining launch file executes here exactly once with a live host.
    pub(crate) fn finish_lua_launch(&mut self, run_ready_hooks: bool) -> Option<LuaBringUpError> {
        let selections = self.startup_selections.clone();
        let target_cwd = self.core.env.cwd().clone();
        let mut launch = self.lua.continue_launch();
        let (launch, project_trust) = crate::lua::scope_app(self, || {
            let project_trust = launch.load(&target_cwd);
            (launch, project_trust)
        });
        self.lua
            .finish_launch(launch, &target_cwd, project_trust.clone());
        crate::lua::api::theme::sync_metadata(self.lua.lua(), self.ui.theme());
        self.project_trust = Some(project_trust);

        let load_failure = self.lua.load_error().map(|error| LuaBringUpError {
            message: error.to_string(),
            location: self.lua.load_failure_location().cloned().unwrap_or(
                smelt_core::lua::LuaLoadFailureLocation {
                    phase: "load",
                    path: None,
                },
            ),
        });

        let runtime_failure =
            match self.resolve_lua_runtime_config_with(self.lua.desired(), &selections, None) {
                Ok((mut next_runtime, next_managed_models)) => {
                    let next_permissions = match smelt_core::permissions::resolve_permissions(
                        &self.lua.desired().permissions.rules,
                        &self.lua.desired().permissions.tool_defaults,
                        self.lua.desired().modes.behaviors.clone(),
                        &next_runtime.settings,
                        smelt_core::permissions::PermissionRuntimePaths {
                            cwd: &target_cwd,
                            home: self.core.env.home(),
                        },
                        &self.core.permission_store,
                        self.core.permissions.paths_fn(),
                    ) {
                        Ok(permissions) => permissions,
                        Err(error) => {
                            return Some(bring_up_error(
                                "permissions",
                                None,
                                format!("load persisted permissions: {error}"),
                            ));
                        }
                    };
                    let auth_error = startup_auth_error(&mut next_runtime, &next_managed_models);

                    if let Err(error) = self.lua.activate_launch() {
                        return Some(bring_up_error(
                            "activation",
                            None,
                            format!("activate Lua launch: {error}"),
                        ));
                    }
                    self.command_catalog
                        .activate(self.lua.command_names_handle());
                    self.core.lua_generation = self.lua.id;

                    self.managed_models.replace_catalog(next_managed_models);
                    self.commit_lua_runtime_config(next_runtime, next_permissions);
                    self.startup_auth_error = auth_error;
                    self.conversation.install_startup_runtime(&self.core.config);
                    self.workspace.refresh(std::path::Path::new(
                        &self.core.config.settings.worktree_root,
                    ));
                    self.core.engine.send(protocol::UiCommand::SetMode {
                        mode: self.core.config.mode.clone(),
                    });
                    self.core
                        .engine
                        .send(protocol::UiCommand::SetReasoningEffort {
                            effort: self.core.config.reasoning_effort.clone(),
                        });
                    self.core.engine.send(protocol::UiCommand::SetFastMode {
                        enabled: self.core.config.settings.fast_mode,
                    });

                    self.reconcile_auto_reload();
                    self.reconcile_runtime_controllers();
                    self.publish_diff_signals();

                    let lua_shared = std::sync::Arc::clone(self.lua.shared());
                    if let Err(error) = crate::lua::api::terminal::commit_staged_title(&lua_shared)
                    {
                        self.notify_workspace_error_sticky(format!("terminal title: {error}"));
                    }
                    for (kind, source, message) in
                        crate::lua::api::notify::take_staged_notices(&lua_shared)
                    {
                        self.record_notice(kind, source, message);
                    }
                    lua_shared.commit_staged_logs();
                    None
                }
                Err(error) => Some(bring_up_error("runtime_resolution", None, error)),
            };
        for warning in self.lua.warnings().to_vec() {
            self.notify_warn(warning);
        }

        self.refresh_main_layout();
        if load_failure.is_none() && runtime_failure.is_none() && run_ready_hooks {
            self.run_lua_ready_hooks("launch");
        }

        load_failure.or(runtime_failure)
    }

    /// Build and commit a fresh Lua generation for manual reload, automatic
    /// config reload, or a cwd transition.
    ///
    /// Candidate evaluation uses fresh Lua registries plus an isolated fork of
    /// generation-owned TUI state. The committed runtime, resolved values,
    /// callbacks, UI resources, managers, and ready hooks remain untouched when
    /// loading or pure runtime resolution fails. On success the old generation
    /// is retired, staged TUI state and declarations become live, synchronous
    /// effects run in explicit order, and only then are `ready` hooks drained.
    /// Manual reloads additionally refresh AGENTS.md, skills, and explicit
    /// system-prompt inputs; automatic reloads leave those inputs unchanged.
    ///
    /// Candidate scripts and lifecycle hooks receive frontend access only for
    /// their individual Lua entry scopes. Returns a load or resolution error
    /// without changing the committed generation.
    pub(crate) fn bring_up_lua(
        &mut self,
        kind: &'static str,
        refresh_agent_inputs: bool,
    ) -> Option<LuaBringUpError> {
        self.bring_up_lua_at(kind, refresh_agent_inputs, None, true, true)
    }

    pub(crate) fn bring_up_lua_for_cwd(
        &mut self,
        path: std::path::PathBuf,
        mark_session_dirty: bool,
    ) -> Option<LuaBringUpError> {
        self.bring_up_lua_at("cwd", true, Some((path, mark_session_dirty)), true, true)
    }

    fn bring_up_lua_at(
        &mut self,
        kind: &'static str,
        refresh_agent_inputs: bool,
        cwd_transition: Option<(std::path::PathBuf, bool)>,
        apply_runtime_effects: bool,
        run_ready_hooks: bool,
    ) -> Option<LuaBringUpError> {
        if matches!(kind, "reload" | "cwd") {
            let lua = self.lua.execution();
            let flush_error = crate::lua::scope_app(self, move || lua.flush_persistent_state());
            if let Some(error) = flush_error {
                return Some(bring_up_error(
                    "state_flush",
                    None,
                    format!("flush persistent state: {error}"),
                ));
            }
        }

        let target_cwd = cwd_transition
            .as_ref()
            .map(|(path, _)| path.clone())
            .unwrap_or_else(|| self.core.env.cwd().clone());
        let candidate_skills = self.prompt_inputs.skill_loader_for_cwd(&target_cwd);
        let retired_generation = self.lua.id;
        let candidate_id = retired_generation.wrapping_add(1);
        let candidate = self.lua.prepare_candidate(
            candidate_id,
            Some(&target_cwd),
            candidate_skills,
            self.lua.wakeup_sender(),
        );
        let committed_tui = self.begin_lua_tui_candidate();
        let candidate_result = crate::lua::scope_app(self, move || candidate.load());
        let mut candidate_tui = self.finish_lua_tui_candidate(committed_tui);
        let candidate = match candidate_result {
            Ok(candidate) => candidate,
            Err(failed) => {
                self.discard_lua_candidate_resources(candidate_id);
                return Some(LuaBringUpError {
                    message: failed.message,
                    location: failed.location,
                });
            }
        };
        let (next_runtime, next_managed_models) =
            match self.resolve_lua_runtime_config(candidate.desired()) {
                Ok(runtime) => runtime,
                Err(error) => {
                    self.discard_lua_candidate_resources(candidate_id);
                    return Some(bring_up_error("runtime_resolution", None, error));
                }
            };
        let next_permissions = match smelt_core::permissions::resolve_permissions(
            &candidate.desired().permissions.rules,
            &candidate.desired().permissions.tool_defaults,
            candidate.desired().modes.behaviors.clone(),
            &next_runtime.settings,
            smelt_core::permissions::PermissionRuntimePaths {
                cwd: &target_cwd,
                home: self.core.env.home(),
            },
            &self.core.permission_store,
            self.core.permissions.paths_fn(),
        ) {
            Ok(permissions) => permissions,
            Err(error) => {
                self.discard_lua_candidate_resources(candidate_id);
                return Some(bring_up_error(
                    "permissions",
                    None,
                    format!("load persisted permissions: {error}"),
                ));
            }
        };
        for callback in candidate_tui
            .ui
            .finish_lua_generation(smelt_core::lua::LUA_BUF_ID_BASE)
        {
            candidate.remove_callback(callback);
        }
        candidate_tui.paint_registry.finish_lua_generation();
        candidate_tui
            .picker_state
            .retain(|win, _| candidate_tui.ui.win(*win).is_some());
        candidate_tui.placeholders.retain_windows(&candidate_tui.ui);
        let staged_cwd = if let Some((path, mark_session_dirty)) = cwd_transition {
            match crate::app::cwd::StagedCwdTransition::stage(path.clone(), mark_session_dirty) {
                Ok(staged) => Some(staged),
                Err(error) => {
                    self.discard_lua_candidate_resources(candidate_id);
                    return Some(bring_up_error("cwd", Some(path), error));
                }
            }
        } else {
            None
        };
        let activation = crate::lua::scope_app(self, || candidate.activate());
        if let Err(error) = activation {
            self.discard_lua_candidate_resources(candidate_id);
            return Some(bring_up_error(
                "activation",
                None,
                format!("activate Lua candidate: {error}"),
            ));
        }
        self.core.signals.clear_lua_generation(retired_generation);
        self.core.timers.clear_generation(retired_generation);
        self.lua.commit_generation(candidate);
        self.commit_lua_tui_candidate(candidate_tui);
        self.dismiss_notification_for_workspace_change();
        self.command_catalog
            .activate(self.lua.command_names_handle());
        if let Some(prompt) = self.ui.buf_mut(crate::app::PROMPT_EDIT_BUF) {
            prompt.invalidate_render_cache();
        }
        self.core.lua_generation = self.lua.id;
        let lua_shared = std::sync::Arc::clone(self.lua.shared());
        if let Err(error) = crate::lua::api::terminal::commit_staged_title(&lua_shared) {
            self.notify_workspace_error_sticky(format!("terminal title: {error}"));
        }
        for (kind, source, message) in crate::lua::api::notify::take_staged_notices(&lua_shared) {
            self.record_notice(kind, source, message);
        }
        lua_shared.commit_staged_logs();
        if apply_runtime_effects {
            for warning in self.lua.warnings().to_vec() {
                self.notify_warn(warning);
            }
            self.managed_models.replace_catalog(next_managed_models);
            self.commit_lua_runtime_config(next_runtime, next_permissions);
            let committed_cwd = staged_cwd.map(|staged| staged.commit(self));
            self.submit_managed_model_refreshes();
            self.reconcile_auto_reload();
            if refresh_agent_inputs {
                self.refresh_agent_inputs();
            }
            if let Some(mark_session_dirty) = committed_cwd {
                self.sync_inline_options();
                self.publish_cwd_change(mark_session_dirty);
                self.lua.clear_pending_reload();
            } else if refresh_agent_inputs {
                self.publish_agent_project_context();
            }
            self.publish_diff_signals();
            self.reconcile_runtime_controllers();
        } else {
            debug_assert!(staged_cwd.is_none());
        }

        // Make layout geometry current before `ready` hooks open overlays or
        // query `Win:rect()`. Without this, cold-start hooks see the seed layout
        // until the first render, while resize/reload paths see the Lua layout.
        self.refresh_main_layout();
        if run_ready_hooks {
            self.run_lua_ready_hooks(kind);
        }
        None
    }

    fn run_lua_ready_hooks(&mut self, kind: &'static str) {
        let hooks = self.lua.take_lifecycle_hooks("ready");
        let lua = self.lua.lua().clone();
        for function in hooks {
            let lua = lua.clone();
            let result = crate::lua::scope_app(self, move || -> mlua::Result<()> {
                let table = lua.create_table()?;
                table.set("kind", kind)?;
                function.call(table)
            });
            if let Err(error) = result {
                self.notify_workspace_error_sticky(format!("lifecycle.ready: {error}"));
            }
        }
    }

    /// Drain generation-zero ready hooks retained by the snapshot harness.
    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn drain_launch_ready_hooks_for_harness(&mut self) {
        self.refresh_main_layout();
        self.run_lua_ready_hooks("launch");
    }

    fn discard_lua_candidate_resources(&mut self, generation: u64) {
        self.core.signals.clear_lua_generation(generation);
        self.core.timers.clear_generation(generation);
    }

    /// Re-read filesystem-backed inputs that feed the agent's system prompt.
    /// The caller publishes them with the rest of the project context after
    /// every part of the transaction has committed.
    pub(crate) fn refresh_agent_inputs(&mut self) {
        let cwd = self.core.env.cwd();
        let outcome = self.prompt_inputs.refresh(&cwd);
        self.core.skills = Some(outcome.loader);
        if let Some(err) = outcome.system_prompt_read_error {
            self.notify_workspace_error_sticky(err);
        }
    }

    pub(crate) fn reconcile_committed_lua_runtime(&mut self) -> Result<(), String> {
        let lua = self.lua.execution();
        let desired = crate::lua::scope_app(self, move || lua.snapshot_desired_state())?;
        self.lua.install_desired_state(desired);
        self.reconcile_runtime_snapshot()
    }

    pub fn shutdown_lua(&mut self) -> (Vec<String>, Option<String>) {
        let context = self.shutdown_context();
        let hooks = self.lua.take_lifecycle_hooks("shutdown");
        let lua = self.lua.lua().clone();
        let mut errors = Vec::new();
        for function in hooks {
            let lua = lua.clone();
            let session_id = context.session_id.clone();
            let result = crate::lua::scope_app(self, move || -> mlua::Result<()> {
                let table = lua.create_table()?;
                table.set("session_id", session_id)?;
                table.set("has_messages", context.has_messages)?;
                table.set("ephemeral", context.ephemeral)?;
                function.call(table)
            });
            if let Err(error) = result {
                errors.push(format!("lifecycle.shutdown: {error}"));
            }
        }

        let lua = self.lua.execution();
        let flush_error = crate::lua::scope_app(self, move || lua.flush_persistent_state());
        (errors, flush_error)
    }

    pub(crate) fn reconcile_runtime_snapshot(&mut self) -> Result<(), String> {
        let desired = self.lua.desired().clone();
        let (next, managed_models) = self.resolve_lua_runtime_config(&desired)?;
        let permissions = smelt_core::permissions::resolve_permissions(
            &desired.permissions.rules,
            &desired.permissions.tool_defaults,
            desired.modes.behaviors,
            &next.settings,
            smelt_core::permissions::PermissionRuntimePaths {
                cwd: &self.core.env.cwd(),
                home: self.core.env.home(),
            },
            &self.core.permission_store,
            self.core.permissions.paths_fn(),
        )
        .map_err(|error| format!("load persisted permissions: {error}"))?;
        self.managed_models.replace_catalog(managed_models);
        self.commit_lua_runtime_config(next, permissions);
        self.submit_managed_model_refreshes();
        self.reconcile_runtime_controllers();
        self.publish_diff_signals();
        self.drain_signals_pending();
        Ok(())
    }

    fn resolve_lua_runtime_config(
        &self,
        desired: &crate::lua::LuaDesiredState,
    ) -> Result<(smelt_core::RuntimeState, smelt_core::ManagedModels), String> {
        self.resolve_lua_runtime_config_with(
            desired,
            &smelt_core::RuntimeSelections::default(),
            Some(&self.core.config),
        )
    }

    fn resolve_lua_runtime_config_with(
        &self,
        desired: &crate::lua::LuaDesiredState,
        selections: &smelt_core::RuntimeSelections,
        previous: Option<&smelt_core::RuntimeState>,
    ) -> Result<(smelt_core::RuntimeState, smelt_core::ManagedModels), String> {
        let mut config = desired.config.clone();
        let mut managed_models = self.managed_models.catalog().clone();
        managed_models.inject_oauth_providers(&mut config);
        managed_models.sync_desired(&config, self.core.config.revision.wrapping_add(1));
        let mut available_models = config.resolve_models();
        managed_models.inject(&config, &mut available_models);
        let mut runtime = smelt_core::resolve_runtime(smelt_core::RuntimeInputs {
            config: &config,
            startup: &self.core.startup_overrides,
            available_models: &available_models,
            registered_modes: &desired.modes.cycle,
            selections,
            previous,
            headless: false,
        })
        .map_err(|error| format!("runtime config reconciliation failed: {error}"))?;
        if let Some(active) = runtime.active_model_mut() {
            let provider = engine::auth::AuthProvider::from_provider_type(&active.provider_type);
            if provider.is_some_and(|provider| !managed_models.provider(provider).authenticated) {
                active.availability = smelt_core::ModelAvailability::Unavailable {
                    reason: smelt_core::ModelUnavailableReason::MissingCredentials,
                };
            }
        }
        Ok((runtime, managed_models))
    }

    fn commit_lua_runtime_config(
        &mut self,
        mut next: smelt_core::RuntimeState,
        permissions: smelt_core::permissions::PermissionResolution,
    ) {
        let old_settings = self.core.config.settings.clone();
        let old_mode = self.core.config.mode.clone();
        let old_reasoning = self.core.config.reasoning_effort.clone();
        let old_model = self.core.config.active_model().cloned();
        let changed = next != self.core.config;
        if changed {
            next.revision = self.core.config.revision.wrapping_add(1);
            self.core.config = next;
            self.apply_settings_effects(&old_settings);
            if self.core.config.mode != old_mode {
                self.core.signals.set_dyn(
                    "agent_mode",
                    std::rc::Rc::new(self.core.config.mode.as_str().to_string()),
                );
            }
            if self.core.config.reasoning_effort != old_reasoning {
                self.core.signals.set_dyn(
                    "reasoning",
                    std::rc::Rc::new(self.core.config.reasoning_effort.label().to_string()),
                );
            }
            if self.core.config.active_model() != old_model.as_ref() {
                self.core.signals.set_dyn(
                    "model",
                    std::rc::Rc::new(
                        self.core
                            .config
                            .active_model()
                            .map(|model| model.key.clone()),
                    ),
                );
                self.refresh_context_window();
                self.warn_if_api_base_normalized();
            }
        }

        self.core.permissions.apply_resolution(permissions);
    }

    /// Reconcile MCP servers against the post-reload `smelt.mcp.register`
    /// desired-state set. Spawns/stops/restarts servers off-thread; the
    /// TUI continues without blocking on handshakes. The
    /// [`smelt_core::mcp::McpDispatcher`] reads tool defs live from the
    /// manager, so the engine's dispatch path picks up the new server
    /// set without further coordination.
    fn reconcile_runtime_controllers(&mut self) {
        self.reconcile_mcp_servers();
        self.lua.shared().lsp.configure_detached(
            self.core.config.lsp.clone(),
            &self.core.env.cwd(),
            self.core.env.home(),
        );
    }

    pub(crate) fn reconcile_mcp_servers(&mut self) {
        let Some(manager) = self.core.mcp.clone() else {
            return;
        };
        manager.reconcile_detached(self.core.config.mcp.clone(), self.core.env.cwd());
    }

    /// Replace live TUI generation projections with isolated candidate copies.
    /// Candidate module bodies can freely refresh named buffers, windows,
    /// overlays, paint slots, callbacks, and theme state without making those
    /// changes observable before the Lua transaction commits.
    fn begin_lua_tui_candidate(&mut self) -> LuaTuiGeneration {
        let mut candidate_ui = self.ui.fork_for_lua_generation();
        let _ = candidate_ui.reap_anonymous(smelt_core::lua::LUA_BUF_ID_BASE);
        let candidate_paint_registry = self.paint_registry.fork_for_lua_generation();
        let candidate_placeholders = self.prompt.fork_lua_placeholders(&candidate_ui);

        LuaTuiGeneration {
            ui: std::mem::replace(&mut self.ui, candidate_ui),
            paint_registry: std::mem::replace(&mut self.paint_registry, candidate_paint_registry),
            picker_state: self.overlays.take_lua_pickers(),
            placeholders: self.prompt.swap_lua_placeholders(candidate_placeholders),
            busy_stack: std::mem::take(&mut self.busy_stack),
        }
    }

    /// Restore the committed TUI state after candidate evaluation and return
    /// the isolated candidate state for either commit or discard.
    fn finish_lua_tui_candidate(&mut self, committed: LuaTuiGeneration) -> LuaTuiGeneration {
        let candidate = LuaTuiGeneration {
            ui: std::mem::replace(&mut self.ui, committed.ui),
            paint_registry: std::mem::replace(&mut self.paint_registry, committed.paint_registry),
            picker_state: self.overlays.swap_lua_pickers(committed.picker_state),
            placeholders: self.prompt.swap_lua_placeholders(committed.placeholders),
            busy_stack: std::mem::replace(&mut self.busy_stack, committed.busy_stack),
        };
        crate::lua::api::theme::sync_metadata(self.lua.lua(), self.ui.theme());
        self.sync_prompt_placeholder_display();
        candidate
    }

    fn commit_lua_tui_candidate(&mut self, mut candidate: LuaTuiGeneration) {
        candidate.ui.merge_rust_callbacks_from(&mut self.ui);
        self.ui = candidate.ui;
        self.paint_registry = candidate.paint_registry;
        self.overlays.swap_lua_pickers(candidate.picker_state);
        self.prompt.swap_lua_placeholders(candidate.placeholders);
        self.busy_stack = candidate.busy_stack;
        crate::lua::api::theme::sync_metadata(self.lua.lua(), self.ui.theme());
        self.sync_prompt_placeholder_display();
    }

    pub(crate) fn rewind_active_user_turn_if_no_output(
        &mut self,
        restore_vim_insert: bool,
    ) -> bool {
        let Some(turn) = self.conversation.active() else {
            return false;
        };
        let Some(history_idx) = turn.rewind_history_idx else {
            return false;
        };
        if !self.prompt.queue_is_empty() || turn.assistant_output_started {
            return false;
        }
        self.rewind_to_history_index(Some(history_idx), restore_vim_insert);
        true
    }

    fn restore_vim_insert_after_rewind(&mut self) {
        let win = self
            .ui
            .win_mut(crate::app::PROMPT_WIN)
            .expect("prompt window");
        self.prompt
            .set_vim_mode(win, crate::smelt_edit::VimMode::Insert);
    }

    /// Rewind to a user history item, or to before the first turn when the index is `None`.
    pub(crate) fn rewind_to_history_index(
        &mut self,
        history_idx: Option<usize>,
        restore_vim_insert: bool,
    ) {
        self.cancel_live_search(false);
        if let Some(history_idx) = history_idx {
            if self.conversation.is_active() {
                self.cancel_agent();
                self.conversation.clear_active();
            }
            let rewound = if let Some((text, images)) = self.rewind_to_history(history_idx) {
                self.clear_prompt_prediction();
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.prompt.restore_from_rewind(&mut pctx, text, images);
                true
            } else {
                false
            };
            while self.core.engine.try_recv().is_ok() {}
            if rewound {
                self.save_session();
                if restore_vim_insert {
                    self.restore_vim_insert_after_rewind();
                }
            }
        } else {
            if self.conversation.is_active() {
                self.cancel_agent();
                self.conversation.clear_active();
            }
            self.clear_prompt_prediction();
            self.rewind_to_start();
            while self.core.engine.try_recv().is_ok() {}
            self.save_session();
            if restore_vim_insert {
                self.restore_vim_insert_after_rewind();
            }
        }
    }

    /// Load a saved session by id, refresh screen, and scroll to bottom.
    pub(crate) fn load_session_by_id(&mut self, id: &str) -> bool {
        let width = self.last_width;
        let target_rows = crate::app::transcript::record_tail_target_rows(self.last_height);
        let prepared = crate::app::session_load::prepare_session_load(
            &self.core.sessions,
            id,
            width,
            target_rows,
            &|| false,
        );
        match prepared {
            Ok(prepared) => self.install_prepared_session_load(prepared),
            Err(error) => {
                self.notify_operation_error_sticky(NotificationOperation::SessionLoad, error);
                false
            }
        }
    }

    /// Resolve a Confirm dialog. Cancels the active turn when the choice requires it.
    pub(crate) fn handle_confirm_resolve(
        &mut self,
        choice: ConfirmChoice,
        message: Option<String>,
        invocation_id: protocol::InvocationId,
        request_id: u64,
        tool_name: &str,
    ) {
        let should_cancel =
            self.resolve_confirm((choice, message), invocation_id, request_id, tool_name);
        if should_cancel {
            self.finish_turn(crate::app::TurnEnd::Cancelled);
            self.conversation.clear_active();
        }
    }
}

#[cfg(test)]
mod controller_tests {
    use super::*;

    fn controller() -> LuaRuntimeController {
        let runtime = crate::lua::LuaRuntime::new();
        let generation = crate::lua::LuaGeneration::initial(
            runtime,
            None,
            smelt_core::trust::TrustState::NoContent,
        );
        LuaRuntimeController::new(generation)
    }

    #[test]
    fn manual_reload_upgrades_pending_config_reload() {
        let mut controller = controller();

        assert!(controller.schedule_reload(LuaReloadKind::AutoConfig));
        assert!(!controller.schedule_reload(LuaReloadKind::Manual));
        assert_eq!(
            controller.take_pending_reload(),
            Some(LuaReloadKind::Manual)
        );
        assert!(!controller.pending_reload());
    }

    #[test]
    fn runtime_reconcile_and_wakeup_are_owned_and_drained() {
        let mut controller = controller();
        controller.schedule_runtime_reconcile();
        controller.schedule_runtime_reconcile();
        assert!(controller.take_runtime_reconcile());
        assert!(!controller.take_runtime_reconcile());

        controller.wakeup_sender().send(()).unwrap();
        assert!(controller.try_receive_wakeup());
        assert!(!controller.try_receive_wakeup());
    }
}
