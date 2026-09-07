//! Typed reactive signal registry with deferred subscriber notification.
//!
//! Each signal is an `Rc<dyn Any>` slot. Writes queue direct + glob subscribers for firing after
//! the `&mut Signals` borrow releases, so subscriber bodies can re-enter `Signals` freely. Lua signals
//! store an `mlua::RegistryKey`; Rust-typed signals use a per-`TypeId` `LuaProjector` to convert to
//! `mlua::Value` at drain time.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use protocol::{TokenUsage, TurnMeta};

use crate::lua::LuaHandle;

/// Value for Lua-originated signals. Stored as a stable `mlua::RegistryKey` so it survives GC.
pub(crate) struct LuaSignalValue {
    pub(crate) key: mlua::RegistryKey,
}

/// Converter from a stored signal value (`&dyn Any`) to a Lua value, keyed by `TypeId`.
pub(crate) type LuaProjector = Box<dyn Fn(&dyn Any, &mlua::Lua) -> mlua::Value>;

/// Stable id returned by `subscribe_kind` and consumed by `unsubscribe`.
pub(crate) type SubscriptionId = u64;

#[derive(Clone)]
pub enum SubscriberKind {
    /// Lua function stashed in the registry; projected at fire time via the per-`TypeId` projector.
    Lua(Rc<LuaHandle>),
}

struct Subscriber {
    id: SubscriptionId,
    generation: u64,
    kind: SubscriberKind,
}

struct GlobSubscriber {
    id: SubscriptionId,
    generation: u64,
    pattern: glob::Pattern,
    kind: SubscriberKind,
}

struct Slot {
    value: Rc<dyn Any>,
    subscribers: Vec<Subscriber>,
    persistent: bool,
    lua_generations: HashSet<u64>,
}

/// One queued callback inside a `PendingFire`. `is_glob` selects the call shape:
/// direct → `(new, old)`, glob → `(name, new, old)`.
pub struct PendingCallback {
    pub kind: SubscriberKind,
    pub is_glob: bool,
    generation: u64,
}

/// One queued notification: value snapshot, optional previous value, and subscriber callbacks.
/// Fired in registration order after the `&mut Signals` borrow releases.
pub struct PendingFire {
    pub name: String,
    pub value: Rc<dyn Any>,
    pub prev: Option<Rc<dyn Any>>,
    pub callbacks: Vec<PendingCallback>,
}

pub struct Signals {
    slots: HashMap<String, Slot>,
    glob_subs: Vec<GlobSubscriber>,
    pending: Vec<PendingFire>,
    next_id: SubscriptionId,
    lua_projectors: HashMap<TypeId, LuaProjector>,
}

impl Default for Signals {
    fn default() -> Self {
        Self::new()
    }
}

impl Signals {
    pub(crate) fn new() -> Self {
        let mut s = Self {
            slots: HashMap::new(),
            glob_subs: Vec::new(),
            pending: Vec::new(),
            next_id: 0,
            lua_projectors: HashMap::new(),
        };
        s.register_lua_projector::<LuaSignalValue, _>(|v, lua| {
            lua.registry_value::<mlua::Value>(&v.key)
                .unwrap_or(mlua::Value::Nil)
        });
        s
    }

    fn register_lua_projector<T, F>(&mut self, project: F)
    where
        T: Any + 'static,
        F: Fn(&T, &mlua::Lua) -> mlua::Value + 'static,
    {
        let wrapper: LuaProjector = Box::new(move |any, lua| match any.downcast_ref::<T>() {
            Some(v) => project(v, lua),
            None => mlua::Value::Nil,
        });
        self.lua_projectors.insert(TypeId::of::<T>(), wrapper);
    }

    /// Return the typed value at `name`, if the signal exists with that exact type.
    pub fn get<T: Any + Clone + 'static>(&self, name: &str) -> Option<T> {
        self.slots
            .get(name)
            .and_then(|slot| slot.value.downcast_ref::<T>().cloned())
    }

    /// Project the signal at `name` to a Lua value. Returns `Nil` when undeclared or no projector.
    pub(crate) fn get_lua(&self, name: &str, lua: &mlua::Lua) -> mlua::Value {
        let Some(slot) = self.slots.get(name) else {
            return mlua::Value::Nil;
        };
        self.project_to_lua(&*slot.value, lua)
    }

    pub fn project_to_lua(&self, value: &dyn Any, lua: &mlua::Lua) -> mlua::Value {
        let tid = (*value).type_id();
        match self.lua_projectors.get(&tid) {
            Some(p) => p(value, lua),
            None => mlua::Value::Nil,
        }
    }

    /// Declare or reset a signal. Re-declaration resets the value and drops all subscribers.
    pub(crate) fn declare<T: Any + 'static>(&mut self, name: impl Into<String>, initial: T) {
        self.slots.insert(
            name.into(),
            Slot {
                value: Rc::new(initial),
                subscribers: Vec::new(),
                persistent: true,
                lua_generations: HashSet::new(),
            },
        );
    }

    pub(crate) fn declare_if_missing_for_generation<T: Any + 'static>(
        &mut self,
        name: impl Into<String>,
        initial: T,
        generation: u64,
    ) {
        let slot = self.slots.entry(name.into()).or_insert_with(|| Slot {
            value: Rc::new(initial),
            subscribers: Vec::new(),
            persistent: false,
            lua_generations: HashSet::new(),
        });
        if !slot.persistent {
            slot.lua_generations.insert(generation);
        }
    }

    fn callbacks_for(&self, name: &str, subscribers: &[Subscriber]) -> Vec<PendingCallback> {
        let mut callbacks: Vec<PendingCallback> = subscribers
            .iter()
            .map(|s| PendingCallback {
                kind: s.kind.clone(),
                is_glob: false,
                generation: s.generation,
            })
            .collect();
        for g in &self.glob_subs {
            if g.pattern.matches(name) {
                callbacks.push(PendingCallback {
                    kind: g.kind.clone(),
                    is_glob: true,
                    generation: g.generation,
                });
            }
        }
        callbacks
    }

    /// Overwrite a signal and queue subscribers. Returns `false` when `name` is undeclared.
    pub fn set_dyn(&mut self, name: &str, value: Rc<dyn Any>) -> bool {
        let (prev, snapshot, direct_callbacks) = {
            let Some(slot) = self.slots.get_mut(name) else {
                return false;
            };
            let prev = std::mem::replace(&mut slot.value, value);
            let snapshot = Rc::clone(&slot.value);
            let callbacks: Vec<PendingCallback> = slot
                .subscribers
                .iter()
                .map(|s| PendingCallback {
                    kind: s.kind.clone(),
                    is_glob: false,
                    generation: s.generation,
                })
                .collect();
            (prev, snapshot, callbacks)
        };

        let mut callbacks = direct_callbacks;
        for g in &self.glob_subs {
            if g.pattern.matches(name) {
                callbacks.push(PendingCallback {
                    kind: g.kind.clone(),
                    is_glob: true,
                    generation: g.generation,
                });
            }
        }
        if callbacks.is_empty() {
            return true;
        }
        self.pending.push(PendingFire {
            name: name.to_string(),
            value: snapshot,
            prev: Some(prev),
            callbacks,
        });
        true
    }

    /// Queue an occurrence without replacing the signal's current value.
    pub fn emit_dyn(&mut self, name: &str, payload: Rc<dyn Any>) -> bool {
        let Some(slot) = self.slots.get(name) else {
            return false;
        };
        let callbacks = self.callbacks_for(name, &slot.subscribers);
        if callbacks.is_empty() {
            return true;
        }
        self.pending.push(PendingFire {
            name: name.to_string(),
            value: payload,
            prev: None,
            callbacks,
        });
        true
    }

    /// Subscribe to `name`. Returns `None` when the signal is undeclared.
    #[cfg(test)]
    pub(crate) fn subscribe_kind(
        &mut self,
        name: &str,
        kind: SubscriberKind,
    ) -> Option<SubscriptionId> {
        self.subscribe_kind_for_generation(name, kind, 0)
    }

    pub(crate) fn subscribe_kind_for_generation(
        &mut self,
        name: &str,
        kind: SubscriberKind,
        generation: u64,
    ) -> Option<SubscriptionId> {
        let slot = self.slots.get_mut(name)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        slot.subscribers.push(Subscriber {
            id,
            generation,
            kind,
        });
        Some(id)
    }

    /// Remove a subscriber. Returns `false` when the signal is undeclared or `id` is unknown.
    pub(crate) fn unsubscribe(&mut self, name: &str, id: SubscriptionId) -> bool {
        let Some(slot) = self.slots.get_mut(name) else {
            return false;
        };
        let Some(idx) = slot.subscribers.iter().position(|s| s.id == id) else {
            return false;
        };
        slot.subscribers.remove(idx);
        true
    }

    #[cfg(test)]
    pub(crate) fn glob_subscribe(
        &mut self,
        pattern: glob::Pattern,
        kind: SubscriberKind,
    ) -> SubscriptionId {
        self.glob_subscribe_for_generation(pattern, kind, 0)
    }

    pub(crate) fn glob_subscribe_for_generation(
        &mut self,
        pattern: glob::Pattern,
        kind: SubscriberKind,
        generation: u64,
    ) -> SubscriptionId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.glob_subs.push(GlobSubscriber {
            id,
            generation,
            pattern,
            kind,
        });
        id
    }

    pub(crate) fn unsubscribe_glob(&mut self, id: SubscriptionId) -> bool {
        let Some(idx) = self.glob_subs.iter().position(|g| g.id == id) else {
            return false;
        };
        self.glob_subs.remove(idx);
        true
    }

    pub fn drain_pending(&mut self) -> Vec<PendingFire> {
        std::mem::take(&mut self.pending)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.slots.keys().map(String::as_str)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.slots.contains_key(name)
    }

    /// Retire only subscriptions owned by one Lua generation.
    pub fn clear_lua_generation(&mut self, generation: u64) {
        for slot in self.slots.values_mut() {
            slot.subscribers
                .retain(|subscriber| subscriber.generation != generation);
            slot.lua_generations.remove(&generation);
        }
        self.slots
            .retain(|_, slot| slot.persistent || !slot.lua_generations.is_empty());
        self.glob_subs
            .retain(|subscriber| subscriber.generation != generation);
        for fire in &mut self.pending {
            fire.callbacks
                .retain(|callback| callback.generation != generation);
        }
        self.pending.retain(|fire| !fire.callbacks.is_empty());
    }

    /// Publish `value` only when it differs from the current slot. Skips subscribers on no-op writes.
    pub fn publish_if_changed<T>(&mut self, name: &str, value: T) -> bool
    where
        T: PartialEq + Any + 'static,
    {
        let Some(slot) = self.slots.get(name) else {
            return false;
        };
        if let Some(cur) = slot.value.downcast_ref::<T>() {
            if *cur == value {
                return false;
            }
        }
        self.set_dyn(name, Rc::new(value))
    }
}

/// Seed values for stateful built-in signals, so plugins read correct state at startup.
pub(crate) struct SignalSeeds {
    pub(crate) vim_mode: String,
    pub(crate) agent_mode: String,
    pub(crate) model: Option<String>,
    pub(crate) reasoning: String,
    pub(crate) cwd: String,
    pub(crate) session_title: String,
    pub(crate) branch: String,
}

/// Placeholder for event-shaped signals before the first typed payload is published. Projects to `nil`.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventStub;

/// Payload for the `turn_error` signal.
#[derive(Debug, Default, Clone)]
pub struct TurnError {
    pub message: String,
}

/// Payload for the `confirm_resolved` signal. `decision` is a stable string matching the
/// resolved `ConfirmChoice` variant + scope (e.g. `"yes"`, `"always_session"`).
#[derive(Debug, Clone)]
pub struct ConfirmResolved {
    pub handle_id: u64,
    pub decision: String,
}

/// Payload for the `history` signal. `kind` identifies the published history operation.
#[derive(Debug, Clone)]
pub struct HistoryDelta {
    pub kind: String,
    pub count: usize,
}

/// Payload for the `turn_end` signal. `cancelled` is `true` for cancel/error legs.
#[derive(Debug, Clone)]
pub struct TurnEnd {
    pub cancelled: bool,
    pub continuation_token: Option<u64>,
    pub error_kind: Option<String>,
    pub retry_at_ms: Option<u64>,
}

/// Payload for the `tool_start` signal.
#[derive(Debug, Clone)]
pub struct ToolStart {
    pub tool: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
}

/// Payload for the `tool_end` signal. `elapsed_ms` is `None` when not timed.
#[derive(Debug, Clone)]
pub struct ToolEnd {
    pub tool: String,
    pub is_error: bool,
    pub elapsed_ms: Option<u64>,
}

/// Payload for the `confirm_requested` signal. Full snapshot the Lua dialog reads.
#[derive(Debug, Clone, Default)]
pub struct ConfirmRequested {
    pub handle_id: u64,
    pub tool_name: String,
    /// Styled summary - the sole source of truth for the dialog body header.
    pub summary: protocol::StyledLines,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub grant_options: Vec<crate::transcript_model::ConfirmApprovalOption>,
}

/// Single entry in the `work_busy` signal payload. One per live
/// `smelt.work.busy` token, projected newest-last as
/// `{ id, label }` Lua tables. `id` is the monotonic token id returned
/// by `push`; plugins compare it across ticks to spot specific tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkBusyEntry {
    pub id: u64,
    pub label: String,
}

/// Payload for the `cursor_pos` signal. Tracks the focused window's
/// cursor for the statusline position pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorPos {
    pub line: u32,
    pub col: u32,
    pub scroll_pct: u8,
}

/// Payload for the `viewport_pos` signal. Tracks the focused window's scroll
/// position through the full scrollable row extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewportPos {
    pub scroll_pct: u8,
}

/// Payload for the `stream_delta` signal. Emitted for every streaming
/// chunk arriving from the provider - text, thinking, and tool-call
/// argument JSON fragments. Use `bytes` for cheap counters (live TPS);
/// `text` carries the raw delta. For `kind == "tool_args"`, `call_id`
/// and `tool_name` identify which tool call the fragment belongs to.
#[derive(Debug, Clone)]
pub struct StreamDelta {
    /// `"text" | "thinking" | "tool_args"`.
    pub kind: String,
    /// UTF-8 byte length of the delta.
    pub bytes: usize,
    /// Raw delta bytes.
    pub text: String,
    /// `call_id` of the tool call this fragment belongs to. Populated
    /// only when `kind == "tool_args"`.
    pub call_id: Option<String>,
    /// `tool_name` of the tool call this fragment belongs to. Populated
    /// only when `kind == "tool_args"`.
    pub tool_name: Option<String>,
}

/// How a built-in signal should be presented to Lua users.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinSignalKind {
    /// Durable state where the current value is meaningful.
    State,
    /// Occurrence-shaped signal where subscribers usually care only about future payloads.
    Event,
}

/// Built-in signals declared by [`build_with_builtins`]. This is the source of
/// truth for generated `smelt.signal.Name` and `smelt.events.Name` autocomplete.
pub struct BuiltinSignal {
    pub name: &'static str,
    pub kind: BuiltinSignalKind,
}

const fn state(name: &'static str) -> BuiltinSignal {
    BuiltinSignal {
        name,
        kind: BuiltinSignalKind::State,
    }
}

const fn event(name: &'static str) -> BuiltinSignal {
    BuiltinSignal {
        name,
        kind: BuiltinSignalKind::Event,
    }
}

pub const BUILTIN_SIGNALS: &[BuiltinSignal] = &[
    state("agent_mode"),
    state("auto_continue_status"),
    event("block_done"),
    state("branch"),
    event("cmd_post"),
    event("cmd_pre"),
    state("confirm_requested"),
    event("confirm_resolved"),
    state("confirms_pending"),
    state("cursor_pos"),
    state("cwd"),
    state("cwd_branch"),
    state("cwd_managed_worktree"),
    state("cwd_project"),
    state("cwd_worktree"),
    state("cwd_worktree_path"),
    state("errors"),
    state("fast_mode"),
    event("history"),
    state("history_epoch"),
    state("input_epoch"),
    event("input_submit"),
    state("keymap_pending"),
    state("model"),
    state("now"),
    state("notification_visible"),
    state("permission_pending"),
    state("prompt_queue_revision"),
    state("prompt_resize_active"),
    state("prompt_resize_chrome"),
    state("reasoning"),
    state("running_procs"),
    event("session_ended"),
    state("session_epoch"),
    event("session_started"),
    state("session_slug"),
    state("session_title"),
    state("settings_auto_continue"),
    state("settings_terminal_title"),
    event("shutdown"),
    state("spinner_frame"),
    event("stream_delta"),
    event("stream_phase"),
    state("task_label"),
    state("tokens_used"),
    event("tool_end"),
    event("tool_start"),
    state("tps"),
    event("turn_complete"),
    event("turn_end"),
    event("turn_error"),
    event("turn_start"),
    state("viewport_pos"),
    state("vim_mode"),
    state("vim_pending_input"),
    state("work_busy"),
    state("work_continuation_token"),
    state("work_elapsed_ms"),
    state("work_label"),
    state("work_outcome"),
    state("work_pause_kind"),
    state("work_retry_attempt"),
    state("work_retry_remaining_ms"),
    state("work_state"),
];

pub fn builtin_signal_names() -> Vec<&'static str> {
    BUILTIN_SIGNALS.iter().map(|signal| signal.name).collect()
}

pub fn builtin_event_names() -> Vec<&'static str> {
    BUILTIN_SIGNALS
        .iter()
        .filter(|signal| signal.kind == BuiltinSignalKind::Event)
        .map(|signal| signal.name)
        .collect()
}

/// Project a `StyledLines` payload into the same shape Lua sees from
/// `buf:styled` - a sequence of lines, each a sequence
/// of `{ text, syntax?, selectable?, title_suffix?, style? = { hl?, dim?, bold?, italic?, fg?, bg? } }`
/// span tables. Empty lines come through as `{}`.
fn styled_lines_to_lua(lua: &mlua::Lua, sl: &protocol::StyledLines) -> mlua::Value {
    let Ok(out) = lua.create_table() else {
        return mlua::Value::Nil;
    };
    for (i, line) in sl.0.iter().enumerate() {
        let Ok(line_tbl) = lua.create_table() else {
            continue;
        };
        for (j, span) in line.iter().enumerate() {
            let Ok(span_tbl) = lua.create_table() else {
                continue;
            };
            let _ = span_tbl.set("text", span.text.as_str());
            if let Some(s) = &span.syntax {
                let _ = span_tbl.set("syntax", s.as_str());
            }
            let needs_style = span.hl.is_some()
                || span.fg.is_some()
                || span.bg.is_some()
                || span.dim
                || span.bold
                || span.italic;
            if needs_style {
                if let Ok(style_tbl) = lua.create_table() {
                    if let Some(h) = &span.hl {
                        let _ = style_tbl.set("hl", h.as_str());
                    }
                    if let Some(f) = &span.fg {
                        let _ = style_tbl.set("fg", f.as_str());
                    }
                    if let Some(b) = &span.bg {
                        let _ = style_tbl.set("bg", b.as_str());
                    }
                    if span.dim {
                        let _ = style_tbl.set("dim", true);
                    }
                    if span.bold {
                        let _ = style_tbl.set("bold", true);
                    }
                    if span.italic {
                        let _ = style_tbl.set("italic", true);
                    }
                    let _ = span_tbl.set("style", style_tbl);
                }
            }
            if !span.selectable {
                let _ = span_tbl.set("selectable", false);
            }
            if span.title_suffix {
                let _ = span_tbl.set("title_suffix", true);
            }
            let _ = line_tbl.set(j + 1, span_tbl);
        }
        let _ = out.set(i + 1, line_tbl);
    }
    mlua::Value::Table(out)
}

/// Register projectors and declare all built-in signals with their initial values.
pub(crate) fn build_with_builtins(seeds: SignalSeeds) -> Signals {
    let mut signals = Signals::new();

    signals.register_lua_projector::<String, _>(|s, lua| match lua.create_string(s.as_str()) {
        Ok(s) => mlua::Value::String(s),
        Err(_) => mlua::Value::Nil,
    });
    signals.register_lua_projector::<Option<String>, _>(|value, lua| match value {
        Some(value) => lua
            .create_string(value.as_str())
            .map_or(mlua::Value::Nil, mlua::Value::String),
        None => mlua::Value::Nil,
    });
    signals.register_lua_projector::<bool, _>(|b, _| mlua::Value::Boolean(*b));
    signals.register_lua_projector::<u32, _>(|n, _| mlua::Value::Integer(*n as i64));
    signals.register_lua_projector::<u64, _>(|n, _| mlua::Value::Integer(*n as i64));
    signals.register_lua_projector::<u8, _>(|n, _| mlua::Value::Integer(*n as i64));
    signals.register_lua_projector::<f64, _>(|n, _| mlua::Value::Number(*n));
    signals.register_lua_projector::<CursorPos, _>(|p, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("line", p.line as i64);
        let _ = t.set("col", p.col as i64);
        let _ = t.set("scroll_pct", p.scroll_pct as i64);
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<ViewportPos, _>(|p, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("scroll_pct", p.scroll_pct as i64);
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<EventStub, _>(|_, _| mlua::Value::Nil);
    // `None` fields are absent so plugins can write `usage.prompt_tokens or 0`.
    signals.register_lua_projector::<TokenUsage, _>(|u, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        if let Some(n) = u.context_tokens {
            let _ = t.set("context_tokens", n);
        }
        if let Some(n) = u.prompt_tokens {
            let _ = t.set("prompt_tokens", n);
        }
        if let Some(n) = u.completion_tokens {
            let _ = t.set("completion_tokens", n);
        }
        if let Some(n) = u.cache_read_tokens {
            let _ = t.set("cache_read_tokens", n);
        }
        if let Some(n) = u.cache_write_tokens {
            let _ = t.set("cache_write_tokens", n);
        }
        if let Some(n) = u.reasoning_tokens {
            let _ = t.set("reasoning_tokens", n);
        }
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<TurnMeta, _>(|m, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("elapsed_ms", m.elapsed_ms);
        if let Some(tps) = m.avg_tps {
            let _ = t.set("avg_tps", tps);
        }
        if let Some(tps) = m.display_tps {
            let _ = t.set("display_tps", tps);
        }
        let _ = t.set("interrupted", m.interrupted);
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<TurnError, _>(|e, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("message", e.message.as_str());
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<ConfirmResolved, _>(|r, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("handle_id", r.handle_id);
        let _ = t.set("decision", r.decision.as_str());
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<HistoryDelta, _>(|d, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("kind", d.kind.as_str());
        let _ = t.set("count", d.count as i64);
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<TurnEnd, _>(|e, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("cancelled", e.cancelled);
        if let Some(token) = e.continuation_token {
            let _ = t.set("continuation_token", token);
        }
        if let Some(kind) = &e.error_kind {
            let _ = t.set("error_kind", kind.as_str());
        }
        if let Some(retry_at_ms) = e.retry_at_ms {
            let _ = t.set("retry_at_ms", retry_at_ms);
        }
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<ToolStart, _>(|s, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("tool", s.tool.as_str());
        if let Ok(args) = lua.create_table() {
            for (k, v) in &s.args {
                if let Ok(lv) = crate::lua::json_to_lua(lua, v) {
                    let _ = args.set(k.as_str(), lv);
                }
            }
            let _ = t.set("args", args);
        }
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<ToolEnd, _>(|s, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("tool", s.tool.as_str());
        let _ = t.set("is_error", s.is_error);
        if let Some(n) = s.elapsed_ms {
            let _ = t.set("elapsed_ms", n);
        }
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<StreamDelta, _>(|d, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("kind", d.kind.as_str());
        let _ = t.set("bytes", d.bytes);
        let _ = t.set("text", d.text.as_str());
        if let Some(cid) = &d.call_id {
            let _ = t.set("call_id", cid.as_str());
        }
        if let Some(name) = &d.tool_name {
            let _ = t.set("tool_name", name.as_str());
        }
        mlua::Value::Table(t)
    });
    signals.register_lua_projector::<ConfirmRequested, _>(|r, lua| {
        let Ok(t) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        let _ = t.set("handle_id", r.handle_id);
        let _ = t.set("tool_name", r.tool_name.as_str());
        let _ = t.set("summary", styled_lines_to_lua(lua, &r.summary));
        if let Ok(options) = lua.create_table() {
            for (i, option) in r.grant_options.iter().enumerate() {
                if let Ok(option_tbl) = lua.create_table() {
                    let _ = option_tbl.set("id", option.id.as_str());
                    let _ = option_tbl.set("label", option.label.as_str());
                    let _ = options.set(i + 1, option_tbl);
                }
            }
            let _ = t.set("grant_options", options);
        }
        if let Ok(args) = lua.create_table() {
            for (k, v) in &r.args {
                if let Ok(lv) = crate::lua::json_to_lua(lua, v) {
                    let _ = args.set(k.as_str(), lv);
                }
            }
            let _ = t.set("args", args);
        }
        mlua::Value::Table(t)
    });

    signals.declare("vim_mode", seeds.vim_mode);
    signals.declare("agent_mode", seeds.agent_mode);
    signals.declare("auto_continue_status", None::<String>);
    signals.declare("model", seeds.model);
    signals.declare("reasoning", seeds.reasoning);
    signals.declare("fast_mode", false);
    signals.declare("confirms_pending", false);
    signals.declare("tokens_used", TokenUsage::default());
    signals.declare("errors", 0u32);
    signals.declare("cwd", seeds.cwd);
    signals.declare("cwd_branch", String::new());
    signals.declare("cwd_managed_worktree", false);
    signals.declare("cwd_project", String::new());
    signals.declare("cwd_worktree", String::new());
    signals.declare("cwd_worktree_path", String::new());
    signals.declare("session_epoch", 0u64);
    signals.declare("session_slug", String::new());
    signals.declare("session_title", seeds.session_title);
    signals.declare("settings_auto_continue", String::from("goal"));
    signals.declare("settings_terminal_title", true);
    signals.declare("branch", seeds.branch);
    signals.declare("history_epoch", 0u64);
    signals.declare("input_epoch", 0u64);
    signals.declare("now", 0u64);
    signals.declare("spinner_frame", 0u8);
    signals.declare("tps", 0.0f64);
    signals.declare("task_label", String::new());
    signals.declare("running_procs", 0u32);
    signals.declare("permission_pending", false);
    signals.declare("notification_visible", false);
    signals.declare("prompt_queue_revision", 0u64);
    signals.declare("prompt_resize_active", false);
    signals.declare("prompt_resize_chrome", String::new());
    signals.declare("cursor_pos", CursorPos::default());
    signals.declare("viewport_pos", ViewportPos::default());

    signals.register_lua_projector::<Vec<WorkBusyEntry>, _>(|v, lua| {
        let Ok(out) = lua.create_table() else {
            return mlua::Value::Nil;
        };
        for (i, e) in v.iter().enumerate() {
            let Ok(t) = lua.create_table() else { continue };
            let _ = t.set("id", e.id);
            let _ = t.set("label", e.label.as_str());
            let _ = out.set(i + 1, t);
        }
        mlua::Value::Table(out)
    });
    signals.declare("work_state", String::from("idle"));
    signals.declare("work_label", String::new());
    signals.declare("work_elapsed_ms", 0u64);
    signals.declare("work_busy", Vec::<WorkBusyEntry>::new());
    signals.declare("work_continuation_token", 0u64);
    signals.declare("work_pause_kind", String::new());
    signals.declare("work_outcome", String::new());
    signals.declare("work_retry_attempt", 0u32);
    signals.declare("work_retry_remaining_ms", 0u64);

    signals.declare("keymap_pending", String::new());
    signals.declare("vim_pending_input", String::new());
    signals.declare("confirm_requested", ConfirmRequested::default());

    for signal in BUILTIN_SIGNALS
        .iter()
        .filter(|signal| signal.kind == BuiltinSignalKind::Event)
    {
        signals.declare(signal.name, EventStub);
    }

    signals
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn handle(lua: &Lua, src: &str) -> Rc<LuaHandle> {
        let func: mlua::Function = lua.load(src).eval().expect("load");
        Rc::new(LuaHandle::from_func(lua, func).expect("registry"))
    }

    #[test]
    fn builtin_signal_metadata_matches_declarations() {
        let signals = build_with_builtins(SignalSeeds {
            vim_mode: "insert".into(),
            agent_mode: "normal".into(),
            model: Some("model".into()),
            reasoning: "off".into(),
            cwd: "/tmp".into(),
            session_title: "session".into(),
            branch: "main".into(),
        });
        let mut declared: Vec<_> = signals.names().collect();
        let mut metadata: Vec<_> = BUILTIN_SIGNALS.iter().map(|signal| signal.name).collect();
        declared.sort_unstable();
        metadata.sort_unstable();
        assert_eq!(declared, metadata);
    }

    #[test]
    fn declare_then_get_lua_returns_initial_value() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.register_lua_projector::<u32, _>(|n, _| mlua::Value::Integer(*n as i64));
        c.declare("count", 7u32);
        match c.get_lua("count", &lua) {
            mlua::Value::Integer(7) => {}
            other => panic!("expected Integer(7), got {other:?}"),
        }
    }

    #[test]
    fn get_lua_returns_nil_for_undeclared() {
        let lua = Lua::new();
        let c = Signals::new();
        assert!(matches!(c.get_lua("missing", &lua), mlua::Value::Nil));
    }

    #[test]
    fn set_dyn_updates_value() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.register_lua_projector::<u32, _>(|n, _| mlua::Value::Integer(*n as i64));
        c.declare("count", 0u32);
        assert!(c.set_dyn("count", Rc::new(42u32)));
        match c.get_lua("count", &lua) {
            mlua::Value::Integer(42) => {}
            other => panic!("expected Integer(42), got {other:?}"),
        }
    }

    #[test]
    fn set_dyn_returns_false_for_undeclared() {
        let mut c = Signals::new();
        assert!(!c.set_dyn("missing", Rc::new(1u32)));
    }

    #[test]
    fn set_without_subscribers_does_not_queue() {
        let mut c = Signals::new();
        c.declare("count", 0u32);
        c.set_dyn("count", Rc::new(1u32));
        assert!(!c.has_pending());
        assert_eq!(c.drain_pending().len(), 0);
    }

    #[test]
    fn subscribe_queues_fire_on_set() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("count", 0u32);
        let id = c
            .subscribe_kind(
                "count",
                SubscriberKind::Lua(handle(&lua, "function(v) end")),
            )
            .expect("declared");
        assert!(id == 0); // first subscription is id 0
        c.set_dyn("count", Rc::new(5u32));
        assert!(c.has_pending());
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].name, "count");
        assert_eq!(fires[0].callbacks.len(), 1);
        assert!(!fires[0].callbacks[0].is_glob);
        // Snapshot carries the post-set value.
        assert_eq!(fires[0].value.downcast_ref::<u32>(), Some(&5u32));
    }

    #[test]
    fn multiple_subscribers_appear_in_registration_order() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("count", 0u32);
        for src in [
            "function() return 1 end",
            "function() return 2 end",
            "function() return 3 end",
        ] {
            c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, src)))
                .unwrap();
        }
        c.set_dyn("count", Rc::new(1u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].callbacks.len(), 3);
    }

    #[test]
    fn unsubscribe_removes_callback() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("count", 0u32);
        let id = c
            .subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        assert!(c.unsubscribe("count", id));
        c.set_dyn("count", Rc::new(1u32));
        // No subscribers, no pending fire.
        assert!(!c.has_pending());
        // Unsubscribing again is a no-op.
        assert!(!c.unsubscribe("count", id));
    }

    #[test]
    fn snapshot_carries_value_at_set_time() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("count", 0u32);
        c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.set_dyn("count", Rc::new(1u32));
        c.set_dyn("count", Rc::new(2u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].value.downcast_ref::<u32>(), Some(&1u32));
        assert_eq!(fires[1].value.downcast_ref::<u32>(), Some(&2u32));
    }

    #[test]
    fn fire_carries_prev_value() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("count", 7u32);
        c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        // First publish: prev = initial value from declare.
        c.set_dyn("count", Rc::new(8u32));
        // Second publish: prev = the value set by the first publish.
        c.set_dyn("count", Rc::new(9u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].value.downcast_ref::<u32>(), Some(&8u32));
        assert_eq!(
            fires[0]
                .prev
                .as_deref()
                .and_then(|v| v.downcast_ref::<u32>()),
            Some(&7u32)
        );
        assert_eq!(fires[1].value.downcast_ref::<u32>(), Some(&9u32));
        assert_eq!(
            fires[1]
                .prev
                .as_deref()
                .and_then(|v| v.downcast_ref::<u32>()),
            Some(&8u32)
        );
    }

    #[test]
    fn emit_dyn_has_no_previous_value() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("event", EventStub);
        c.subscribe_kind("event", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.emit_dyn("event", Rc::new(42u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert!(fires[0].prev.is_none());
    }

    #[test]
    fn drain_pending_is_idempotent() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("count", 0u32);
        c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.set_dyn("count", Rc::new(1u32));
        assert_eq!(c.drain_pending().len(), 1);
        assert!(c.drain_pending().is_empty());
    }

    #[test]
    fn redeclare_resets_value_and_drops_subscribers() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.register_lua_projector::<bool, _>(|b, _| mlua::Value::Boolean(*b));
        c.declare("flag", false);
        c.subscribe_kind("flag", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.declare("flag", true);
        match c.get_lua("flag", &lua) {
            mlua::Value::Boolean(true) => {}
            other => panic!("expected Boolean(true), got {other:?}"),
        }
        c.set_dyn("flag", Rc::new(false));
        // Redeclare dropped the prior subscriber.
        assert!(!c.has_pending());
    }

    #[test]
    fn subscribe_returns_none_for_undeclared() {
        let lua = Lua::new();
        let mut c = Signals::new();
        assert!(c
            .subscribe_kind(
                "missing",
                SubscriberKind::Lua(handle(&lua, "function() end"))
            )
            .is_none());
    }

    #[test]
    fn glob_subscribe_fires_for_matching_names() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("agent:1:status", "idle");
        c.declare("agent:2:status", "idle");
        c.declare("vim_mode", "Insert");
        let id = c.glob_subscribe(
            glob::Pattern::new("agent:*:status").unwrap(),
            SubscriberKind::Lua(handle(&lua, "function() end")),
        );
        // Sequential ids share the next_id counter with direct subs.
        assert!(id == 0);
        c.set_dyn("agent:1:status", Rc::new("running"));
        c.set_dyn("vim_mode", Rc::new("Normal"));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].name, "agent:1:status");
        assert_eq!(fires[0].callbacks.len(), 1);
        assert!(fires[0].callbacks[0].is_glob);
    }

    #[test]
    fn glob_and_direct_subscribers_both_fire() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("turn_complete", false);
        c.subscribe_kind(
            "turn_complete",
            SubscriberKind::Lua(handle(&lua, "function() end")),
        )
        .unwrap();
        c.glob_subscribe(
            glob::Pattern::new("turn_*").unwrap(),
            SubscriberKind::Lua(handle(&lua, "function() end")),
        );
        c.set_dyn("turn_complete", Rc::new(true));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].callbacks.len(), 2);
        // Direct subscriber appears before glob in registration order.
        assert!(!fires[0].callbacks[0].is_glob);
        assert!(fires[0].callbacks[1].is_glob);
    }

    #[test]
    fn unsubscribe_glob_removes_callback() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.declare("foo", 0u32);
        let id = c.glob_subscribe(
            glob::Pattern::new("*").unwrap(),
            SubscriberKind::Lua(handle(&lua, "function() end")),
        );
        assert!(c.unsubscribe_glob(id));
        c.set_dyn("foo", Rc::new(1u32));
        assert!(!c.has_pending());
        // Unsubscribing again is a no-op.
        assert!(!c.unsubscribe_glob(id));
    }

    #[test]
    fn glob_subscriber_does_not_fire_for_undeclared_name() {
        let lua = Lua::new();
        let mut c = Signals::new();
        c.glob_subscribe(
            glob::Pattern::new("*").unwrap(),
            SubscriberKind::Lua(handle(&lua, "function() end")),
        );
        // No declared signal, so set_dyn returns false and queues nothing.
        assert!(!c.set_dyn("missing", Rc::new(1u32)));
        assert!(!c.has_pending());
    }

    #[test]
    fn lua_signal_value_round_trip() {
        let lua = Lua::new();
        let value: mlua::Value = lua.load("\"hello\"").eval().unwrap();
        let key = lua.create_registry_value(value).unwrap();
        let mut c = Signals::new();
        c.declare("greeting", LuaSignalValue { key });
        match c.get_lua("greeting", &lua) {
            mlua::Value::String(s) => {
                assert_eq!(s.to_str().unwrap(), "hello");
            }
            other => panic!("expected String(hello), got {other:?}"),
        }
    }

    #[test]
    fn builtin_seeds_declare_every_signal() {
        let lua = Lua::new();
        let signals = build_with_builtins(SignalSeeds {
            vim_mode: "Insert".into(),
            agent_mode: "normal".into(),
            model: Some("anthropic/claude-opus-4-7".into()),
            reasoning: "off".into(),
            cwd: "/tmp/work".into(),
            session_title: String::new(),
            branch: String::new(),
        });

        // Stateful signals with primitive projectors return their seeds.
        for (name, expected) in [
            ("vim_mode", "Insert"),
            ("agent_mode", "normal"),
            ("model", "anthropic/claude-opus-4-7"),
            ("reasoning", "off"),
            ("cwd", "/tmp/work"),
        ] {
            match signals.get_lua(name, &lua) {
                mlua::Value::String(s) => assert_eq!(s.to_str().unwrap(), expected),
                other => panic!("signal {name}: expected String({expected}), got {other:?}"),
            }
        }

        // Absent state and event payloads project to nil.
        for name in [
            "auto_continue_status",
            "history",
            "turn_complete",
            "turn_error",
            "confirm_resolved",
            "session_started",
            "session_ended",
        ] {
            assert!(
                matches!(signals.get_lua(name, &lua), mlua::Value::Nil),
                "signal {name} should project to Nil"
            );
        }

        // `now` initialises at 0 (epoch); `spinner_frame` at 0; both
        // project as Lua integers via the u64 / u8 projectors.
        assert!(matches!(
            signals.get_lua("now", &lua),
            mlua::Value::Integer(0)
        ));
        assert!(matches!(
            signals.get_lua("spinner_frame", &lua),
            mlua::Value::Integer(0)
        ));

        // `tokens_used` initialises as `TokenUsage::default()` whose
        // every field is `None`; the projector returns an empty table.
        match signals.get_lua("tokens_used", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.len().unwrap(), 0);
                assert_eq!(t.pairs::<String, i64>().count(), 0);
            }
            other => panic!("expected Table, got {other:?}"),
        }

        // Nullable state must still have a declared slot and a Lua projector.
        for signal in BUILTIN_SIGNALS {
            let name = signal.name;
            let slot = signals
                .slots
                .get(name)
                .unwrap_or_else(|| panic!("builtin signal `{name}` is not declared"));
            assert!(
                signal.kind == BuiltinSignalKind::Event
                    || signals
                        .lua_projectors
                        .contains_key(&(*slot.value).type_id()),
                "builtin state signal `{name}` has no Lua projector"
            );
        }
    }

    #[test]
    fn token_usage_projector_emits_named_fields() {
        let lua = Lua::new();
        let mut c = Signals::new();
        // The TokenUsage projector lives in build_with_builtins; mirror
        // the registration here so the unit test is hermetic.
        c.register_lua_projector::<TokenUsage, _>(|u, lua| {
            let Ok(t) = lua.create_table() else {
                return mlua::Value::Nil;
            };
            if let Some(n) = u.context_tokens {
                let _ = t.set("context_tokens", n);
            }
            if let Some(n) = u.prompt_tokens {
                let _ = t.set("prompt_tokens", n);
            }
            if let Some(n) = u.completion_tokens {
                let _ = t.set("completion_tokens", n);
            }
            mlua::Value::Table(t)
        });
        c.declare(
            "tokens_used",
            TokenUsage {
                context_tokens: Some(1690),
                prompt_tokens: Some(1234),
                completion_tokens: Some(456),
                ..Default::default()
            },
        );
        match c.get_lua("tokens_used", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("prompt_tokens").unwrap(), 1234);
                assert_eq!(t.get::<i64>("context_tokens").unwrap(), 1690);
                assert_eq!(t.get::<i64>("completion_tokens").unwrap(), 456);
                // Absent fields surface as nil - not 0 - so plugins can
                // distinguish "no data" from "0 tokens".
                assert!(matches!(
                    t.get::<mlua::Value>("reasoning_tokens").unwrap(),
                    mlua::Value::Nil
                ));
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn event_payload_projectors_emit_named_fields() {
        let lua = Lua::new();
        let signals = build_with_builtins(SignalSeeds {
            vim_mode: "Insert".into(),
            agent_mode: "normal".into(),
            model: Some("m".into()),
            reasoning: "off".into(),
            cwd: "/".into(),
            session_title: String::new(),
            branch: String::new(),
        });

        // Set typed payloads via set_dyn - Signals::project_to_lua keys
        // on the stored value's TypeId, so the typed projector takes
        // over even though the slot was declared with EventStub.
        let mut signals = signals;
        signals.set_dyn(
            "turn_complete",
            Rc::new(TurnMeta {
                elapsed_ms: 12000,
                avg_tps: Some(33.5),
                display_tps: Some(33.5),
                interrupted: false,
            }),
        );
        signals.set_dyn(
            "turn_error",
            Rc::new(TurnError {
                message: "boom".into(),
            }),
        );
        signals.set_dyn(
            "turn_end",
            Rc::new(TurnEnd {
                cancelled: true,
                continuation_token: Some(9),
                error_kind: Some("quota".into()),
                retry_at_ms: Some(123_000),
            }),
        );
        signals.set_dyn(
            "confirm_resolved",
            Rc::new(ConfirmResolved {
                handle_id: 7,
                decision: "always_session".into(),
            }),
        );
        signals.set_dyn(
            "history",
            Rc::new(HistoryDelta {
                kind: "set".into(),
                count: 4,
            }),
        );
        signals.set_dyn("session_started", Rc::new(String::from("sess-001")));
        signals.set_dyn(
            "confirm_requested",
            Rc::new(ConfirmRequested {
                handle_id: 42,
                tool_name: "bash".into(),
                summary: protocol::StyledLines::from_plain("ls"),
                args: std::collections::HashMap::new(),
                grant_options: vec![crate::transcript_model::ConfirmApprovalOption {
                    id: "grant_0_session".into(),
                    label: "allow bash for this session".into(),
                    target: crate::transcript_model::ApprovalTarget::Session,
                    grants: Vec::new(),
                }],
            }),
        );

        match signals.get_lua("turn_complete", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("elapsed_ms").unwrap(), 12000);
                assert!((t.get::<f64>("avg_tps").unwrap() - 33.5).abs() < f64::EPSILON);
                assert!((t.get::<f64>("display_tps").unwrap() - 33.5).abs() < f64::EPSILON);
                assert!(!t.get::<bool>("interrupted").unwrap());
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match signals.get_lua("turn_error", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<String>("message").unwrap(), "boom");
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match signals.get_lua("turn_end", &lua) {
            mlua::Value::Table(t) => {
                assert!(t.get::<bool>("cancelled").unwrap());
                assert_eq!(t.get::<i64>("continuation_token").unwrap(), 9);
                assert_eq!(t.get::<String>("error_kind").unwrap(), "quota");
                assert_eq!(t.get::<i64>("retry_at_ms").unwrap(), 123_000);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match signals.get_lua("confirm_resolved", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("handle_id").unwrap(), 7);
                assert_eq!(t.get::<String>("decision").unwrap(), "always_session");
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match signals.get_lua("history", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<String>("kind").unwrap(), "set");
                assert_eq!(t.get::<i64>("count").unwrap(), 4);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match signals.get_lua("session_started", &lua) {
            mlua::Value::String(s) => assert_eq!(s.to_str().unwrap(), "sess-001"),
            other => panic!("expected String, got {other:?}"),
        }
        match signals.get_lua("confirm_requested", &lua) {
            mlua::Value::Table(t) => {
                assert_eq!(t.get::<i64>("handle_id").unwrap(), 42);
                assert_eq!(t.get::<String>("tool_name").unwrap(), "bash");
                let summary: mlua::Table = t.get("summary").unwrap();
                let line: mlua::Table = summary.get(1).unwrap();
                let span: mlua::Table = line.get(1).unwrap();
                assert_eq!(span.get::<String>("text").unwrap(), "ls");
                let options: mlua::Table = t.get("grant_options").unwrap();
                let option: mlua::Table = options.get(1).unwrap();
                assert_eq!(option.get::<String>("id").unwrap(), "grant_0_session");
                assert_eq!(
                    option.get::<String>("label").unwrap(),
                    "allow bash for this session"
                );
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn builtin_signals_queue_subscribers_on_set() {
        // Every state-changing event in the engine pipeline reaches
        // the right signal setter and queues subscribers.
        let lua = Lua::new();
        let mut signals = build_with_builtins(SignalSeeds {
            vim_mode: "Insert".into(),
            agent_mode: "normal".into(),
            model: Some("m".into()),
            reasoning: "off".into(),
            cwd: "/".into(),
            session_title: String::new(),
            branch: String::new(),
        });

        // Subscribe to a mix of stateful and event-shaped built-in signals.
        for name in [
            "agent_mode",
            "turn_complete",
            "turn_error",
            "tool_start",
            "tool_end",
            "history",
            "confirm_requested",
            "confirm_resolved",
        ] {
            signals
                .subscribe_kind(name, SubscriberKind::Lua(handle(&lua, "function() end")))
                .expect("builtin signal should be declared");
        }

        signals.set_dyn("agent_mode", Rc::new("apply".to_string()));
        signals.set_dyn(
            "turn_complete",
            Rc::new(TurnMeta {
                elapsed_ms: 100,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            }),
        );
        signals.set_dyn(
            "turn_error",
            Rc::new(TurnError {
                message: "err".into(),
            }),
        );
        signals.set_dyn(
            "tool_start",
            Rc::new(ToolStart {
                tool: "bash".into(),
                args: std::collections::HashMap::new(),
            }),
        );
        signals.set_dyn(
            "tool_end",
            Rc::new(ToolEnd {
                tool: "bash".into(),
                is_error: false,
                elapsed_ms: None,
            }),
        );
        signals.set_dyn(
            "history",
            Rc::new(HistoryDelta {
                kind: "append".into(),
                count: 1,
            }),
        );
        signals.set_dyn(
            "confirm_requested",
            Rc::new(ConfirmRequested {
                handle_id: 1,
                tool_name: "bash".into(),
                summary: protocol::StyledLines::from_plain("ls"),
                args: std::collections::HashMap::new(),
                grant_options: vec![],
            }),
        );
        signals.set_dyn(
            "confirm_resolved",
            Rc::new(ConfirmResolved {
                handle_id: 1,
                decision: "yes".into(),
            }),
        );

        let fires = signals.drain_pending();
        let names: std::collections::HashSet<_> = fires.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            fires.len(),
            8,
            "missing signals: {:?}",
            [
                "agent_mode",
                "turn_complete",
                "turn_error",
                "tool_start",
                "tool_end",
                "history",
                "confirm_requested",
                "confirm_resolved"
            ]
            .iter()
            .filter(|n| !names.contains(**n))
            .collect::<Vec<_>>()
        );
        for expected in [
            "agent_mode",
            "turn_complete",
            "turn_error",
            "tool_start",
            "tool_end",
            "history",
            "confirm_requested",
            "confirm_resolved",
        ] {
            assert!(
                names.contains(expected),
                "expected {expected} to fire subscribers"
            );
        }
    }
}
