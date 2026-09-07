//! Dispatch `engine::HostCall` requests through the matching Lua hooks
//! on the TUI main thread. Each `HostCall` carries its own
//! `oneshot::Sender` for the reply; we send back at most once.

use crate::app::TuiApp;
use engine::{HostCall, HostRequestDecision, PreparedRequestMessages};
use protocol::Message;
use smelt_core::lua::{HookRegistry, LuaShared};
use smelt_core::working::TurnPhase;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::oneshot;

type MessageReply = oneshot::Sender<HostRequestDecision>;
const PREPARE_CONTEXT_HISTORY_DELTA_MAX_ITEMS: usize = 256;

thread_local! {
    static DEFERRED_HOST_REPLIES: RefCell<Vec<DeferredHostReply>> = const { RefCell::new(Vec::new()) };
}

enum DeferredRequestDecision {
    Continue,
    Stop,
    ReplaceCanonical(Vec<Message>),
    ReplaceModelHistory,
    Abort(String),
}

#[derive(Default)]
pub(super) struct HostWorkState {
    pending: Option<Rc<RequestHook>>,
    context_recalculation: Option<super::BusyToken>,
    handoff_turn: Option<u64>,
}

struct RequestHook {
    turn_id: u64,
    cancel_generation: u64,
    reply: RefCell<Option<MessageReply>>,
    compaction: RefCell<Option<super::BusyToken>>,
}

impl RequestHook {
    fn is_current(&self, app: &TuiApp) -> bool {
        Some(self.turn_id) == app.active_agent_turn_id()
            && self.cancel_generation == app.conversation.cancel_generation()
    }

    fn complete(self: &Rc<Self>, decision: DeferredRequestDecision) {
        let Some(reply) = self.reply.borrow_mut().take() else {
            return;
        };
        DEFERRED_HOST_REPLIES.with(|replies| {
            replies.borrow_mut().push(DeferredHostReply {
                reply,
                decision,
                owner: Rc::clone(self),
            });
        });
    }

    fn finish_compaction(&self) {
        if let Some(token) = self.compaction.borrow_mut().take() {
            token.release();
        }
    }
}

/// The Lua callback, unlike the pending request, owns the default reply.
/// GC and reload complete through the same main-thread path as an explicit reply.
struct RequestHookReply(Rc<RequestHook>);

impl Drop for RequestHookReply {
    fn drop(&mut self) {
        self.0.complete(DeferredRequestDecision::Continue);
    }
}

/// An actual context recalculation, independent of request-hook waiting.
/// The hook may revoke the token even while Lua retains this handle.
pub(crate) struct ContextRecalculation(super::BusyToken);

impl Drop for ContextRecalculation {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl mlua::UserData for ContextRecalculation {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("remove", |_, this, ()| Ok(this.0.release()));
        methods.add_method("alive", |_, this, ()| Ok(this.0.is_active()));
    }
}

struct DeferredHostReply {
    reply: MessageReply,
    decision: DeferredRequestDecision,
    owner: Rc<RequestHook>,
}

fn current_model_history_decision(app: &TuiApp) -> HostRequestDecision {
    let coordinates = app.model_history_source().coordinates();
    HostRequestDecision::replace_model_history(app.model_history_messages(), coordinates)
}

fn request_decision_from_lua(
    inner_lua: &mlua::Lua,
    value: mlua::Value,
) -> mlua::Result<DeferredRequestDecision> {
    let mlua::Value::Table(t) = value else {
        return Ok(DeferredRequestDecision::Continue);
    };

    match t.get::<Option<String>>("action")?.as_deref() {
        Some("continue") | None => Ok(DeferredRequestDecision::Continue),
        Some("abort") => Ok(DeferredRequestDecision::Abort(t.get::<String>("message")?)),
        Some("replace") => {
            if t.get::<Option<String>>("source")?.as_deref() == Some("model_history") {
                return Ok(DeferredRequestDecision::ReplaceModelHistory);
            }
            Ok(t.get::<mlua::Value>("messages")
                .ok()
                .and_then(|v| smelt_core::lua::lua_to_serde::<Vec<Message>>(inner_lua, &v))
                .map(DeferredRequestDecision::ReplaceCanonical)
                .unwrap_or(DeferredRequestDecision::Continue))
        }
        Some(_) => Ok(DeferredRequestDecision::Continue),
    }
}

fn create_message_reply_fn(
    lua: &mlua::Lua,
    owner: Rc<RequestHook>,
) -> mlua::Result<mlua::Function> {
    let reply = RequestHookReply(owner);
    lua.create_function(move |inner_lua, value: mlua::Value| {
        if reply.0.reply.borrow().is_none() {
            // Defensive double replies are silent no-ops.
            return Ok(());
        }
        let (decision, result) = match request_decision_from_lua(inner_lua, value) {
            Ok(decision) => (decision, Ok(())),
            Err(error) => (DeferredRequestDecision::Continue, Err(error)),
        };
        reply.0.complete(decision);
        result
    })
}

pub(crate) fn drain_deferred_host_replies(app: &mut TuiApp) {
    let replies = DEFERRED_HOST_REPLIES.with(|replies| std::mem::take(&mut *replies.borrow_mut()));
    for reply in replies {
        let decision = app.resolve_request_hook(&reply.owner, reply.decision);
        let _ = reply.reply.send(decision);
    }
    app.sync_compaction_phase();
}

fn prepare_request_to_lua(
    lua: &mlua::Lua,
    messages: PreparedRequestMessages,
    estimated_tokens: u32,
    context_estimate: PrepareContextEstimate,
) -> mlua::Result<mlua::Table> {
    let request = lua.create_table()?;
    request.set("estimated_tokens", estimated_tokens)?;
    request.set(
        "estimated_context_tokens",
        context_estimate.total_context_tokens,
    )?;
    request.set("context_estimate", context_estimate.into_lua_table(lua)?)?;

    let mt = lua.create_table()?;
    mt.set(
        "__index",
        lua.create_function(move |lua, (table, key): (mlua::Table, mlua::Value)| {
            if let mlua::Value::String(s) = key {
                if s.to_str()?.as_ref() == "messages" {
                    let value = smelt_core::lua::serde_to_lua(lua, &messages.model())?;
                    table.raw_set("messages", value.clone())?;
                    return Ok(value);
                }
            }
            Ok(mlua::Value::Nil)
        })?,
    )?;
    request.set_metatable(Some(mt))?;
    Ok(request)
}

impl TuiApp {
    fn begin_request_hook(&mut self, turn_id: u64, reply: MessageReply) -> Rc<RequestHook> {
        self.cancel_request_hook();
        let owner = Rc::new(RequestHook {
            turn_id,
            cancel_generation: self.conversation.cancel_generation(),
            reply: RefCell::new(Some(reply)),
            compaction: RefCell::new(None),
        });
        self.host_work.pending = Some(Rc::clone(&owner));
        owner
    }

    pub(crate) fn begin_context_recalculation(&mut self, label: String) -> ContextRecalculation {
        if let Some(previous) = self.host_work.context_recalculation.take() {
            previous.release();
            self.clear_compaction_preview();
        }
        let token = self.busy_stack.push_context_recalculation_token(label);
        if let Some(owner) = self
            .host_work
            .pending
            .as_ref()
            .filter(|owner| owner.is_current(self))
        {
            owner.finish_compaction();
            *owner.compaction.borrow_mut() = Some(token.clone());
        }
        self.host_work.context_recalculation = Some(token.clone());
        self.sync_compaction_phase();
        ContextRecalculation(token)
    }

    pub(super) fn cancel_request_hook(&mut self) {
        if let Some(owner) = self.host_work.pending.clone() {
            let decision = self.resolve_request_hook(&owner, DeferredRequestDecision::Stop);
            if let Some(reply) = owner.reply.borrow_mut().take() {
                let _ = reply.send(decision);
            }
        }
        self.host_work.handoff_turn = None;
    }

    pub(super) fn defer_queued_compaction_handoff(&mut self) -> bool {
        let Some(turn_id) = self.active_agent_turn_id() else {
            return false;
        };
        if self.host_work.handoff_turn == Some(turn_id) {
            return true;
        }
        if self.host_work.pending.as_ref().is_some_and(|owner| {
            owner.is_current(self)
                && owner
                    .compaction
                    .borrow()
                    .as_ref()
                    .is_some_and(super::BusyToken::is_active)
        }) {
            self.host_work.handoff_turn = Some(turn_id);
            return true;
        }
        false
    }

    pub(super) fn take_queued_compaction_handoff(&mut self) -> bool {
        self.host_work.handoff_turn.take().is_some_and(|turn_id| {
            Some(turn_id) == self.active_agent_turn_id() && self.prompt.has_queued_request()
        })
    }

    fn resolve_request_hook(
        &mut self,
        owner: &Rc<RequestHook>,
        decision: DeferredRequestDecision,
    ) -> HostRequestDecision {
        owner.finish_compaction();
        self.sync_compaction_phase();
        let owns_pending = self
            .host_work
            .pending
            .as_ref()
            .is_some_and(|pending| Rc::ptr_eq(pending, owner));
        if !owns_pending {
            return HostRequestDecision::Stop;
        }
        self.host_work.pending = None;
        if !owner.is_current(self) {
            self.host_work.handoff_turn = None;
            return HostRequestDecision::Stop;
        }
        if matches!(
            decision,
            DeferredRequestDecision::Abort(_) | DeferredRequestDecision::Stop
        ) {
            self.host_work.handoff_turn = None;
        } else if self.host_work.handoff_turn.is_some() {
            if self.prompt.has_queued_request() {
                return HostRequestDecision::Stop;
            }
            self.host_work.handoff_turn = None;
        }
        match decision {
            DeferredRequestDecision::Continue => HostRequestDecision::Continue,
            DeferredRequestDecision::Stop => HostRequestDecision::Stop,
            DeferredRequestDecision::ReplaceCanonical(messages) => {
                HostRequestDecision::replace_canonical_history(messages)
            }
            DeferredRequestDecision::ReplaceModelHistory => current_model_history_decision(self),
            DeferredRequestDecision::Abort(message) => HostRequestDecision::Abort(message),
        }
    }

    fn sync_compaction_phase(&mut self) {
        let compacting = self
            .host_work
            .context_recalculation
            .as_ref()
            .is_some_and(super::BusyToken::is_active);
        if !compacting && self.host_work.context_recalculation.take().is_some() {
            self.clear_compaction_preview();
        }
        if self.agent_is_running() {
            if compacting && !self.working.is_compacting() {
                self.working.begin(TurnPhase::Compacting);
            } else if !compacting && self.working.is_compacting() {
                self.working.begin(TurnPhase::Working);
            }
        }
    }

    pub(crate) fn dispatch_host_call(&mut self, call: HostCall) {
        match call {
            HostCall::ProviderResponse {
                turn_id,
                message,
                reply,
            } => {
                if self.active_agent_turn_id() != Some(turn_id) {
                    let _ = reply.send(None);
                    return;
                }
                let mutated = self.run_middleware_chain::<Message>(message, "on_response", |s| {
                    &s.hooks.provider_response
                });
                let _ = reply.send(mutated);
            }
            HostCall::RecoverFromContextLimit {
                turn_id,
                messages,
                reply,
            } => {
                if self.active_agent_turn_id() != Some(turn_id) {
                    let _ = reply.send(HostRequestDecision::Stop);
                    return;
                }
                self.dispatch_recover_from_context_limit(turn_id, messages, reply);
            }
            HostCall::RequestAudit {
                persistence,
                entry,
                payload_mode,
            } => {
                if let Err(cause) =
                    self.conversation
                        .append_request_audit(persistence, *entry, payload_mode)
                {
                    self.notify_warn(format!("request audit was not queued: {}", cause.message));
                }
            }
            HostCall::PrepareRequest {
                turn_id,
                messages,
                estimated_tokens,
                reply,
            } => {
                if self.active_agent_turn_id() != Some(turn_id) {
                    let _ = reply.send(HostRequestDecision::Stop);
                    return;
                }
                self.dispatch_prepare_request(turn_id, messages, estimated_tokens, reply);
            }
        }
    }

    /// Hand the first registered `smelt.engine.on_context_limit` hook the
    /// truncated history along with a Lua `reply` function whose body
    /// holds the engine's `oneshot::Sender`. The hook MUST call `reply`
    /// exactly once with `{ action = "replace", messages = ... }` (engine
    /// swaps and retries), `{ action = "abort", message = ... }` (engine
    /// aborts the turn), or `nil`/`{ action = "continue" }` (engine
    /// continues with the original request). If the hook vanishes without
    /// calling `reply`, GC completes the hook with its default decision.
    fn dispatch_recover_from_context_limit(
        &mut self,
        turn_id: u64,
        messages: Vec<Message>,
        reply: MessageReply,
    ) {
        let lua = self.lua.lua().clone();
        let funcs = self
            .lua
            .core_shared()
            .hooks
            .context_limit
            .snapshot_for(&lua, "");
        let Some(func) = funcs.into_iter().next() else {
            let _ = reply.send(HostRequestDecision::Continue);
            return;
        };
        let payload = smelt_core::lua::serde_to_lua(&lua, &messages);
        self.call_message_reply_hook(turn_id, "on_context_limit", func, payload, reply);
    }

    /// Hand the first registered `smelt.engine.on_prepare_request`
    /// hook the request metadata immediately. The `messages` field is built
    /// lazily if the hook reads it, so metadata-only hooks do not pay to
    /// serialize large histories into Lua.
    fn dispatch_prepare_request(
        &mut self,
        turn_id: u64,
        messages: PreparedRequestMessages,
        estimated_tokens: u32,
        reply: MessageReply,
    ) {
        let lua = self.lua.lua().clone();
        let funcs = self
            .lua
            .core_shared()
            .hooks
            .prepare_request
            .snapshot_for(&lua, "");
        let Some(func) = funcs.into_iter().next() else {
            let _ = reply.send(HostRequestDecision::Continue);
            return;
        };
        let identity = self.active_context_token_identity();
        let current_history_len = self.session_history_len();
        let checkpoint_context_tokens =
            self.conversation
                .session()
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| {
                    checkpoint
                        .tokens_after_estimate
                        .map(|tokens| (tokens, checkpoint.tokens_after_estimate_history_len))
                });
        let base_history_len = self
            .conversation
            .session()
            .context_tokens_history_len
            .or_else(|| checkpoint_context_tokens.and_then(|(_, len)| len))
            .unwrap_or(current_history_len);
        let history_delta_len = current_history_len.saturating_sub(base_history_len);
        let context_estimate = if history_delta_len > PREPARE_CONTEXT_HISTORY_DELTA_MAX_ITEMS {
            PrepareContextEstimate::full_request(estimated_tokens, current_history_len)
        } else {
            let history_delta = if base_history_len < current_history_len {
                match self.session_history_range(base_history_len..current_history_len) {
                    Ok(history) => Some(history),
                    Err(err) => {
                        self.notify_session_error_sticky(format!(
                            "failed to read canonical session history: {err}"
                        ));
                        None
                    }
                }
            } else {
                Some(Vec::new())
            };
            history_delta.map_or_else(
                || PrepareContextEstimate::full_request(estimated_tokens, current_history_len),
                |history_delta| {
                    PrepareContextEstimate::from_history_delta(
                        self.conversation.session().context_tokens_for(&identity),
                        self.conversation.session().context_tokens_history_len,
                        checkpoint_context_tokens,
                        current_history_len,
                        &history_delta,
                        messages.model(),
                        estimated_tokens,
                    )
                },
            )
        };
        let payload = prepare_request_to_lua(&lua, messages, estimated_tokens, context_estimate)
            .map(mlua::Value::Table);
        self.call_message_reply_hook(turn_id, "on_prepare_request", func, payload, reply);
    }

    fn call_message_reply_hook(
        &mut self,
        turn_id: u64,
        label: &'static str,
        func: mlua::Function,
        payload: mlua::Result<mlua::Value>,
        reply: MessageReply,
    ) {
        let lua = self.lua.lua().clone();
        let owner = self.begin_request_hook(turn_id, reply);
        let result = crate::lua::scope_app(self, || {
            let payload = payload?;
            let reply_fn = create_message_reply_fn(&lua, Rc::clone(&owner))?;
            func.call::<()>((payload, reply_fn))
        });
        if let Err(error) = result {
            self.record_lua_error(format!("{label}: {error}"));
            owner.complete(DeferredRequestDecision::Continue);
            drain_deferred_host_replies(self);
        }
    }

    /// Snapshot a `HookRegistry`, serialize `payload` into Lua via serde,
    /// call each hook in registration order (passing the previous hook's
    /// returned table as input), and deserialize the final value back
    /// into `T`. Returns `None` when no hook is registered or no hook
    /// returned a replacement table - caller treats `None` as "no
    /// mutation, proceed with the original payload".
    fn run_middleware_chain<T>(
        &mut self,
        payload: T,
        label: &'static str,
        registry: impl Fn(&LuaShared) -> &Arc<HookRegistry>,
    ) -> Option<T>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let lua = self.lua.lua().clone();
        let funcs = registry(self.lua.core_shared()).snapshot_for(&lua, "");
        if funcs.is_empty() {
            return None;
        }
        let shared = Arc::clone(self.lua.core_shared());
        let current = smelt_core::lua::serde_to_lua(&lua, &payload).ok()?;
        crate::lua::scope_app(self, move || {
            let mut current = current;
            let mut mutated = false;
            for function in funcs {
                match function.call::<mlua::Value>(current.clone()) {
                    Ok(mlua::Value::Table(table)) => {
                        current = mlua::Value::Table(table);
                        mutated = true;
                    }
                    Ok(_) => {}
                    Err(error) => smelt_core::lua::LuaRuntime::record_error_with(
                        &lua,
                        &shared,
                        format!("provider.middleware {label}: {error}"),
                    ),
                }
            }
            mutated
                .then(|| smelt_core::lua::lua_to_serde::<T>(&lua, &current))
                .flatten()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrepareContextEstimateSource {
    FullRequestEstimate,
    ProviderSnapshot,
    ProviderSnapshotPlusHistoryDelta,
    CheckpointEstimate,
    CheckpointEstimatePlusHistoryDelta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrepareContextEstimate {
    total_context_tokens: u32,
    provider_context_tokens: Option<u32>,
    estimated_delta_tokens: u32,
    latest_snapshot_history_len: Option<usize>,
    current_history_len: usize,
    source: PrepareContextEstimateSource,
}

impl PrepareContextEstimate {
    #[cfg(test)]
    fn from_request(
        current_context_tokens: Option<u32>,
        context_tokens_history_len: Option<usize>,
        current_history: &[protocol::HistoryItem],
        request_messages: &[Message],
        full_request_estimate: u32,
    ) -> Self {
        let base_history_len = context_tokens_history_len.unwrap_or(current_history.len());
        let history_delta = if base_history_len < current_history.len() {
            &current_history[base_history_len..]
        } else {
            &[]
        };
        Self::from_history_delta(
            current_context_tokens,
            context_tokens_history_len,
            None,
            current_history.len(),
            history_delta,
            request_messages,
            full_request_estimate,
        )
    }

    fn from_history_delta(
        current_context_tokens: Option<u32>,
        context_tokens_history_len: Option<usize>,
        checkpoint_context_tokens: Option<(u32, Option<usize>)>,
        current_history_len: usize,
        history_delta: &[protocol::HistoryItem],
        _request_messages: &[Message],
        full_request_estimate: u32,
    ) -> Self {
        let (base, base_history_len, exact_source, delta_source) =
            if let Some(base) = current_context_tokens {
                (
                    base,
                    context_tokens_history_len,
                    PrepareContextEstimateSource::ProviderSnapshot,
                    PrepareContextEstimateSource::ProviderSnapshotPlusHistoryDelta,
                )
            } else if let Some((base, history_len)) = checkpoint_context_tokens {
                (
                    base,
                    history_len,
                    PrepareContextEstimateSource::CheckpointEstimate,
                    PrepareContextEstimateSource::CheckpointEstimatePlusHistoryDelta,
                )
            } else {
                return Self::full_request(full_request_estimate, current_history_len);
            };
        let latest_snapshot_history_len = base_history_len;
        let base_history_len = base_history_len.unwrap_or(current_history_len);

        if base_history_len > current_history_len {
            return Self::full_request(full_request_estimate, current_history_len);
        }

        if base_history_len == current_history_len {
            return Self {
                total_context_tokens: base,
                provider_context_tokens: current_context_tokens,
                estimated_delta_tokens: 0,
                latest_snapshot_history_len,
                current_history_len,
                source: exact_source,
            };
        }

        let added_messages = protocol::history_to_messages(history_delta);
        let estimated_delta_tokens = smelt_core::session::estimate_message_tokens(&added_messages);
        Self {
            total_context_tokens: base.saturating_add(estimated_delta_tokens),
            provider_context_tokens: current_context_tokens,
            estimated_delta_tokens,
            latest_snapshot_history_len,
            current_history_len,
            source: delta_source,
        }
    }

    fn full_request(full_request_estimate: u32, current_history_len: usize) -> Self {
        Self {
            total_context_tokens: full_request_estimate,
            provider_context_tokens: None,
            estimated_delta_tokens: full_request_estimate,
            latest_snapshot_history_len: None,
            current_history_len,
            source: PrepareContextEstimateSource::FullRequestEstimate,
        }
    }

    fn into_lua_table(self, lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
        let table = lua.create_table()?;
        table.set("source", self.source.as_str())?;
        table.set("total_context_tokens", self.total_context_tokens)?;
        table.set("provider_context_tokens", self.provider_context_tokens)?;
        table.set("estimated_delta_tokens", self.estimated_delta_tokens)?;
        table.set(
            "latest_snapshot_history_len",
            self.latest_snapshot_history_len,
        )?;
        table.set("current_history_len", self.current_history_len)?;
        Ok(table)
    }
}

impl PrepareContextEstimateSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::FullRequestEstimate => "full_request_estimate",
            Self::ProviderSnapshot => "provider_snapshot",
            Self::ProviderSnapshotPlusHistoryDelta => "provider_snapshot_plus_history_delta",
            Self::CheckpointEstimate => "checkpoint_estimate",
            Self::CheckpointEstimatePlusHistoryDelta => "checkpoint_estimate_plus_history_delta",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{AssistantStep, Content, HistoryItem, ToolInvocation, ToolOutcome};

    #[test]
    fn retained_reply_callback_defers_without_scoped_host_access() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(42);
        let lua = app.app.lua.lua().clone();
        let (reply, mut response) = tokio::sync::oneshot::channel();
        let owner = app.app.begin_request_hook(42, reply);
        let callback = create_message_reply_fn(&lua, owner).unwrap();
        let value = lua.create_table().unwrap();
        value.set("action", "continue").unwrap();

        callback.call::<()>(value).unwrap();

        assert!(matches!(
            response.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        crate::lua::scope_app(&mut app.app, || ());
        assert!(matches!(
            response.try_recv().expect("deferred reply"),
            HostRequestDecision::Continue
        ));
    }

    #[test]
    fn malformed_request_reply_restores_owning_phase() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(42);
        let lua = app.app.lua.lua().clone();
        let (reply, mut response) = oneshot::channel();
        let owner = app.app.begin_request_hook(42, reply);
        let _compaction = app.app.begin_context_recalculation("compacting".into());
        let callback = create_message_reply_fn(&lua, owner).unwrap();
        let value = lua.create_table().unwrap();
        value.set("action", "abort").unwrap();

        assert!(callback.call::<()>(value).is_err());
        crate::lua::scope_app(&mut app.app, || ());

        assert_eq!(app.working_probe().phase_label(), Some("working"));
        assert!(matches!(
            response.try_recv().unwrap(),
            HostRequestDecision::Continue
        ));
    }

    #[test]
    fn queued_compaction_handoff_preserves_overrides_and_honors_cancellation() {
        use crossterm::event::KeyCode;
        use protocol::{EngineEvent, UiCommand};

        for cancel in [false, true] {
            let mut app = crate::app::test_harness::TestApp::builder().build();
            app.start_turn(42);
            let lua = app.app.lua.lua().clone();
            let (reply, mut response) = oneshot::channel();
            let owner = app.app.begin_request_hook(42, reply);
            let _compaction = app.app.begin_context_recalculation("compacting".into());
            let callback = create_message_reply_fn(&lua, owner).unwrap();
            assert!(app.run_lua(
                r#"
                smelt.engine.submit_command("queued", "override body",
                    { reasoning_effort = "high" }, "queued")
            "#
            ));
            app.press(KeyCode::Enter);
            app.press(KeyCode::Enter);
            assert_eq!(app.current_turn_id(), Some(42));
            assert_eq!(app.working_probe().phase_label(), Some("compacting"));
            assert!(!app
                .drain_engine_sends()
                .iter()
                .any(|cmd| matches!(cmd, UiCommand::Cancel | UiCommand::StartTurn(_))));
            if cancel {
                app.discard_turn(crate::app::TurnEnd::Cancelled);
            }
            crate::lua::scope_app(&mut app.app, || callback.call::<()>(mlua::Value::Nil)).unwrap();
            assert!(matches!(
                response.try_recv().unwrap(),
                HostRequestDecision::Stop
            ));
            if !cancel {
                app.press(KeyCode::Enter);
                assert_eq!(app.current_turn_id(), Some(42));
            }
            app.dispatch_engine_event(EngineEvent::TurnComplete {
                turn_id: 42,
                history: None,
                meta: None,
            });
            let commands = app.drain_engine_sends();
            if cancel {
                assert!(!app.agent_running());
                assert!(!commands
                    .iter()
                    .any(|cmd| matches!(cmd, UiCommand::StartTurn(_))));
                assert!(app.prompt_source().contains("/queued"));
            } else {
                assert!(commands
                    .iter()
                    .any(|cmd| matches!(cmd, UiCommand::StartTurn(payload)
                    if payload.input.provider_content().text_content() == "override body"
                        && payload.reasoning_effort == protocol::ReasoningEffort::High)));
                assert_eq!(app.queued_message_count(), 0);
            }
        }
    }

    #[test]
    fn compaction_request_hook_completes_all_exit_paths() {
        use crossterm::event::KeyCode;
        use protocol::{EngineEvent, UiCommand};

        for handoff in [false, true] {
            for finish in ["reply", "drop", "reload", "malformed", "abort", "cancel"] {
                let mut app = crate::app::test_harness::TestApp::builder().build();
                app.start_turn(42);
                app.app.lua.core_shared().hooks.prepare_request.clear();
                assert!(app.run_bundled_lua(
                    r#"
                    smelt.engine.on_prepare_request(function(_, reply)
                        _G.pending_reply = reply
                        _G.compaction = __smelt_internal.work._context_recalculation("compacting")
                    __smelt_internal.transcript._set_compaction_preview("PENDING_SUMMARY")
                    end)
                "#
                ));
                let (reply, mut response) = oneshot::channel();
                app.dispatch_host_call(HostCall::PrepareRequest {
                    turn_id: 42,
                    messages: PreparedRequestMessages::model_only(Vec::new()),
                    estimated_tokens: 0,
                    reply,
                });
                assert_eq!(app.working_probe().phase_label(), Some("compacting"));
                if handoff {
                    app.type_text("NEXT_TASK");
                    app.press(KeyCode::Enter);
                    app.press(KeyCode::Enter);
                    app.press(KeyCode::Enter);
                    assert_eq!(app.current_turn_id(), Some(42));
                }
                app.drain_engine_sends();
                match finish {
                    "reply" => assert!(app.run_lua("pending_reply(nil); pending_reply(nil)")),
                    "drop" => {
                        assert!(app.run_lua("pending_reply = nil; collectgarbage('collect')"))
                    }
                    "reload" => app.reload_lua(),
                    "malformed" => {
                        assert!(app.run_lua("assert(not pcall(pending_reply, {action = 'abort'}))"))
                    }
                    "abort" => assert!(app.run_lua(
                        "pending_reply({action = 'abort', message = 'terminal failure'})"
                    )),
                    "cancel" => {
                        app.app.finish_turn(crate::app::TurnEnd::Cancelled);
                    }
                    _ => unreachable!(),
                }
                app.drive_lua_tasks();
                let decision = response
                    .try_recv()
                    .unwrap_or_else(|error| panic!("{finish}, handoff={handoff}: {error}"));
                match finish {
                    "abort" => assert!(
                        matches!(decision, HostRequestDecision::Abort(message) if message == "terminal failure")
                    ),
                    "cancel" => assert!(matches!(decision, HostRequestDecision::Stop)),
                    _ if handoff => assert!(matches!(decision, HostRequestDecision::Stop)),
                    _ => assert!(matches!(decision, HostRequestDecision::Continue)),
                }
                assert!(
                    app.app.host_work.pending.is_none(),
                    "{finish}, handoff={handoff}"
                );
                assert!(!app.app.busy_stack.context_recalculating());
                assert!(!app.working_probe().is_compacting());
                assert!(
                    app.conversation_probe()
                        .transcript_compaction_preview_id()
                        .is_none(),
                    "{finish}, handoff={handoff}"
                );
                if finish != "reload" {
                    assert!(app.run_lua(
                        "assert(not compaction:alive()); assert(not compaction:remove())"
                    ));
                }
                if matches!(finish, "abort" | "cancel") {
                    assert!(app.app.host_work.handoff_turn.is_none());
                    if finish == "abort" {
                        app.dispatch_engine_event(EngineEvent::TurnError {
                            message: "terminal failure".into(),
                            kind: None,
                            retry_at_ms: None,
                        });
                        assert_eq!(app.queued_message_count(), usize::from(handoff));
                        assert!(!app
                            .drain_engine_sends()
                            .iter()
                            .any(|command| matches!(command, UiCommand::StartTurn(_))));
                    }
                } else if handoff {
                    app.press(KeyCode::Enter);
                    assert_eq!(app.current_turn_id(), Some(42));
                    app.dispatch_engine_event(EngineEvent::TurnComplete {
                        turn_id: 42,
                        history: None,
                        meta: None,
                    });
                    assert!(app
                        .drain_engine_sends()
                        .iter()
                        .any(|command| matches!(command, UiCommand::StartTurn(payload)
                        if payload.input.provider_content().text_content() == "NEXT_TASK")));
                    assert_eq!(app.queued_message_count(), 0);
                }
            }
        }
    }

    #[test]
    fn compaction_handle_drop_and_hook_error_release_owned_work() {
        for error in [false, true] {
            let mut app = crate::app::test_harness::TestApp::builder().build();
            app.start_turn(42);
            app.app.lua.core_shared().hooks.prepare_request.clear();
            assert!(app.run_bundled_lua(&format!(
                r#"
                smelt.engine.on_prepare_request(function(_, reply)
                    _G.pending_reply = reply
                    _G.compaction = __smelt_internal.work._context_recalculation("compacting")
                    __smelt_internal.transcript._set_compaction_preview("PENDING_SUMMARY")
                    if {error} then error("hook failed") end
                end)
            "#
            )));
            let (reply, mut response) = oneshot::channel();
            app.dispatch_host_call(HostCall::PrepareRequest {
                turn_id: 42,
                messages: PreparedRequestMessages::model_only(Vec::new()),
                estimated_tokens: 0,
                reply,
            });
            if !error {
                assert!(app.run_lua("compaction = nil; collectgarbage('collect')"));
                assert!(app.app.host_work.pending.is_some());
            } else {
                assert!(app.run_lua("assert(not compaction:alive())"));
            }
            assert_eq!(app.working_probe().phase_label(), Some("working"));
            assert!(!app.app.busy_stack.context_recalculating());
            assert!(app
                .conversation_probe()
                .transcript_compaction_preview_id()
                .is_none());
            assert!(app.run_lua("pending_reply(nil)"));
            assert!(matches!(
                response.try_recv().unwrap(),
                HostRequestDecision::Continue
            ));
            assert!(app.app.host_work.pending.is_none());
        }
    }

    #[test]
    fn prepare_context_estimate_uses_full_estimate_without_provider_baseline() {
        let estimate = PrepareContextEstimate::from_request(None, None, &[], &[], 123);

        assert_eq!(estimate.total_context_tokens, 123);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::FullRequestEstimate
        );
    }

    #[test]
    fn prepare_context_estimate_adds_only_history_after_latest_token_snapshot() {
        let history = vec![
            HistoryItem::user(Content::text("old")),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("reply")),
                None,
                Vec::new(),
            )),
            HistoryItem::user(Content::text("new prompt")),
        ];
        let request_messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_request(
            Some(100),
            Some(2),
            &history,
            &request_messages,
            10_000,
        );

        assert!(estimate.total_context_tokens > 100);
        assert!(estimate.total_context_tokens < 10_000);
        assert_eq!(estimate.provider_context_tokens, Some(100));
        assert!(estimate.estimated_delta_tokens > 0);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshotPlusHistoryDelta
        );
    }

    #[test]
    fn prepare_context_estimate_uses_checkpoint_estimate_until_provider_baseline() {
        let history = vec![
            HistoryItem::user(Content::text("checkpoint summary")),
            HistoryItem::user(Content::text("live suffix")),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("new reply")),
                None,
                Vec::new(),
            )),
        ];
        let history_delta = &history[2..];
        let messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_history_delta(
            None,
            None,
            Some((80, Some(2))),
            history.len(),
            history_delta,
            &messages,
            10_000,
        );

        assert!(estimate.total_context_tokens > 80);
        assert!(estimate.total_context_tokens < 10_000);
        assert_eq!(estimate.provider_context_tokens, None);
        assert_eq!(estimate.latest_snapshot_history_len, Some(2));
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::CheckpointEstimatePlusHistoryDelta
        );
    }

    #[test]
    fn prepare_context_estimate_marks_an_unchanged_checkpoint() {
        let history = vec![
            HistoryItem::user(Content::text("checkpoint summary")),
            HistoryItem::user(Content::text("live suffix")),
        ];
        let messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_history_delta(
            None,
            None,
            Some((120, Some(history.len()))),
            history.len(),
            &[],
            &messages,
            10_000,
        );

        assert_eq!(estimate.total_context_tokens, 120);
        assert_eq!(estimate.estimated_delta_tokens, 0);
        assert_eq!(estimate.latest_snapshot_history_len, Some(2));
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::CheckpointEstimate
        );
    }

    #[test]
    fn prepare_context_estimate_stays_at_baseline_when_snapshot_covers_history() {
        let history = vec![
            HistoryItem::user(Content::text("old")),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("reply")),
                None,
                Vec::new(),
            )),
        ];
        let request_messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_request(
            Some(100),
            Some(2),
            &history,
            &request_messages,
            10_000,
        );

        assert_eq!(estimate.total_context_tokens, 100);
        assert_eq!(estimate.estimated_delta_tokens, 0);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshot
        );
    }

    #[test]
    fn prepare_context_estimate_does_not_double_count_snapshotted_tool_output() {
        let history = vec![
            HistoryItem::user(Content::text("run tests")),
            HistoryItem::assistant(AssistantStep::with_invocations(
                None,
                None,
                Vec::new(),
                vec![ToolInvocation {
                    call_id: "call-1".into(),
                    name: "bash".into(),
                    arguments: r#"{"cmd":"cargo nextest run"}"#.into(),
                    result: ToolOutcome::new("test output\n".repeat(30_000), false, None),
                    elapsed_ms: None,
                    called_at_ms: None,
                }],
            )),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("done")),
                None,
                Vec::new(),
            )),
        ];
        let messages = protocol::history_to_messages(&history);

        assert!(matches!(
            messages
                .get(messages.len().saturating_sub(2))
                .map(|m| m.role),
            Some(protocol::Role::Tool)
        ));
        let estimate = PrepareContextEstimate::from_request(
            Some(171_359),
            Some(history.len()),
            &history,
            &messages,
            250_000,
        );

        assert_eq!(estimate.total_context_tokens, 171_359);
        assert_eq!(estimate.estimated_delta_tokens, 0);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshot
        );
    }

    #[test]
    fn prepare_context_estimate_adds_delta_when_baseline_predates_tool_output() {
        let history = vec![
            HistoryItem::user(Content::text("run tests")),
            HistoryItem::assistant(AssistantStep::with_invocations(
                None,
                None,
                Vec::new(),
                vec![ToolInvocation {
                    call_id: "call-1".into(),
                    name: "bash".into(),
                    arguments: r#"{"cmd":"cargo nextest run"}"#.into(),
                    result: ToolOutcome::new("test output\n".repeat(30_000), false, None),
                    elapsed_ms: None,
                    called_at_ms: None,
                }],
            )),
        ];
        let messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_request(
            Some(171_359),
            Some(1), // baseline recorded after user message, before assistant + tool
            &history,
            &messages,
            250_000,
        );

        assert!(estimate.total_context_tokens > 171_359);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshotPlusHistoryDelta
        );
    }

    #[test]
    fn prepare_context_estimate_falls_back_to_full_history_len_when_baseline_len_missing() {
        let history = vec![
            HistoryItem::user(Content::text("run command")),
            HistoryItem::assistant(AssistantStep::with_invocations(
                None,
                None,
                Vec::new(),
                vec![ToolInvocation {
                    call_id: "call-1".into(),
                    name: "bash".into(),
                    arguments: r#"{"cmd":"printf hello"}"#.into(),
                    result: ToolOutcome::new("hello\n".repeat(100), false, None),
                    elapsed_ms: None,
                    called_at_ms: None,
                }],
            )),
        ];
        let messages = protocol::history_to_messages(&history);
        // No context_tokens_history_len - defaults to current_history.len(),
        // so the baseline is assumed to cover the full history and no delta
        // is added.
        let estimate =
            PrepareContextEstimate::from_request(Some(1_000), None, &history, &messages, 10_000);

        assert_eq!(estimate.total_context_tokens, 1_000);
        assert_eq!(estimate.estimated_delta_tokens, 0);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshot
        );
    }

    #[test]
    fn prepare_context_estimate_uses_full_request_when_baseline_cleared() {
        let history = vec![
            HistoryItem::user(Content::text("hello")),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("hi")),
                None,
                Vec::new(),
            )),
        ];
        let messages = protocol::history_to_messages(&history);
        let estimate =
            PrepareContextEstimate::from_request(None, None, &history, &messages, 10_000);

        assert_eq!(estimate.total_context_tokens, 10_000);
        assert_eq!(estimate.provider_context_tokens, None);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::FullRequestEstimate
        );
    }

    #[test]
    fn prepare_context_estimate_uses_full_request_when_baseline_is_ahead() {
        let history = vec![HistoryItem::user(Content::text("rewound"))];
        let messages = protocol::history_to_messages(&history);
        let estimate =
            PrepareContextEstimate::from_request(Some(5_000), Some(4), &history, &messages, 900);

        assert_eq!(estimate.total_context_tokens, 900);
        assert_eq!(estimate.provider_context_tokens, None);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::FullRequestEstimate
        );
    }

    #[test]
    fn prepare_request_builds_messages_lazily_on_access() {
        let lua = mlua::Lua::new();
        let messages = vec![Message::user(Content::text("hello"))];
        let request = prepare_request_to_lua(
            &lua,
            PreparedRequestMessages::model_only(messages),
            42,
            PrepareContextEstimate::full_request(42, 1),
        )
        .expect("request table");

        assert!(request.raw_get::<mlua::Value>("messages").unwrap().is_nil());
        assert_eq!(request.get::<u32>("estimated_tokens").unwrap(), 42);

        let messages = request.get::<mlua::Table>("messages").unwrap();
        assert_eq!(
            messages
                .get::<mlua::Table>(1)
                .unwrap()
                .get::<String>("role")
                .unwrap(),
            "user"
        );
        assert!(matches!(
            request.raw_get::<mlua::Value>("messages").unwrap(),
            mlua::Value::Table(_)
        ));
    }
}
