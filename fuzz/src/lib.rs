//! Shared types for the smelt fuzz target and the `crash_to_scenario`
//! converter. The on-disk scenario format is a JSON-serialized
//! [`Scenario`] - also the exact shape libFuzzer's `arbitrary` decoder
//! produces, so a crash artifact round-trips into a readable file with no
//! lossy translation.

pub mod cache_common;
pub mod lua_loop;
pub mod runtime;
pub mod shrink;
pub use lua_loop::{run_lua_scenario, LuaScenario};

use arbitrary::{Arbitrary, Unstructured};
use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use protocol::UiCommand;
use protocol::{
    AgentMode, CanonicalHistoryDelta, Content, EngineAskError, EngineAskErrorKind, EngineEvent,
    Message, ReasoningBlock, ReasoningKind, TokenUsage, ToolOutcome,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tui::app::test_harness::{Action, AllocBudget, SourceEvent};
use tui::smelt_edit::VimMode;

pub use tui::app::test_harness::TestApp;

/// Bounded JSON value tree for tool argument fuzzing. Production paths
/// (`evaluate_tool_metadata`, `permissions.evaluate_tool`, pattern matching in
/// `RequestPermission`) consume `HashMap<String, serde_json::Value>` -
/// feeding them all-empty bags reaches none of that logic. The
/// `Arbitrary` impl synthesises small (≤ 6 keys, depth ≤ 3) trees so
/// each op stays cheap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArgsBag(pub HashMap<String, serde_json::Value>);

impl ArgsBag {
    pub fn into_map(self) -> HashMap<String, serde_json::Value> {
        self.0
    }
}

impl<'a> Arbitrary<'a> for ArgsBag {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let n = u.int_in_range(0u8..=6)? as usize;
        let mut map = HashMap::with_capacity(n);
        for _ in 0..n {
            let key: String = arb_short_string(u, 16)?;
            let val = arb_json(u, 3)?;
            map.insert(key, val);
        }
        Ok(ArgsBag(map))
    }
}

fn arb_short_string(u: &mut Unstructured<'_>, max_bytes: usize) -> arbitrary::Result<String> {
    let len = u.int_in_range(0..=max_bytes)?;
    let bytes: Vec<u8> = (0..len)
        .map(|_| u.arbitrary::<u8>())
        .collect::<Result<_, _>>()?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn arb_json(u: &mut Unstructured<'_>, depth: u8) -> arbitrary::Result<serde_json::Value> {
    if depth == 0 || u.is_empty() {
        return arb_json_leaf(u);
    }
    match u.int_in_range(0u8..=5)? {
        0 => arb_json_leaf(u),
        1 => Ok(serde_json::Value::String(arb_short_string(u, 16)?)),
        2 => {
            let n = u.int_in_range(0u8..=4)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(arb_json(u, depth - 1)?);
            }
            Ok(serde_json::Value::Array(v))
        }
        3 => {
            let n = u.int_in_range(0u8..=4)? as usize;
            let mut m = serde_json::Map::with_capacity(n);
            for _ in 0..n {
                m.insert(arb_short_string(u, 16)?, arb_json(u, depth - 1)?);
            }
            Ok(serde_json::Value::Object(m))
        }
        _ => arb_json_leaf(u),
    }
}

fn arb_json_leaf(u: &mut Unstructured<'_>) -> arbitrary::Result<serde_json::Value> {
    match u.int_in_range(0u8..=4)? {
        0 => Ok(serde_json::Value::Null),
        1 => Ok(serde_json::Value::Bool(u.arbitrary()?)),
        2 => Ok(serde_json::json!(u.arbitrary::<i64>()?)),
        3 => {
            let f: f64 = u.arbitrary()?;
            Ok(serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null))
        }
        _ => Ok(serde_json::Value::String(arb_short_string(u, 16)?)),
    }
}

/// Mouse event payload. `kind`/`button`/`mods` index lookup tables
/// declared below so `arbitrary` byte-budget stays tiny.
#[derive(Arbitrary, Debug, Clone, Serialize, Deserialize)]
pub struct MouseFuzz {
    pub kind: u8,
    pub button: u8,
    pub col: u8,
    pub row: u8,
    pub mods: u8,
}

#[derive(Arbitrary, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FuzzJobTermination {
    Exited,
    Signaled,
    #[serde(rename = "oom")]
    OutOfMemory,
    Stopped,
}

impl From<FuzzJobTermination> for protocol::JobTermination {
    fn from(value: FuzzJobTermination) -> Self {
        match value {
            FuzzJobTermination::Exited => Self::Exited,
            FuzzJobTermination::Signaled => Self::Signaled,
            FuzzJobTermination::OutOfMemory => Self::OutOfMemory,
            FuzzJobTermination::Stopped => Self::Stopped,
        }
    }
}

const MOUSE_BUTTONS: &[MouseButton] = &[MouseButton::Left, MouseButton::Right, MouseButton::Middle];

const ENGINE_ASK_ERROR_KINDS: &[EngineAskErrorKind] = &[
    EngineAskErrorKind::Network,
    EngineAskErrorKind::RateLimited,
    EngineAskErrorKind::Quota,
    EngineAskErrorKind::InvalidResponse,
    EngineAskErrorKind::ContextWindow,
    EngineAskErrorKind::CyberPolicy,
    EngineAskErrorKind::Cancelled,
    EngineAskErrorKind::Other,
];

fn decode_mouse_kind(kind: u8, button: u8) -> MouseEventKind {
    let btn = MOUSE_BUTTONS[(button as usize) % MOUSE_BUTTONS.len()];
    match kind % 8 {
        0 => MouseEventKind::Down(btn),
        1 => MouseEventKind::Up(btn),
        2 => MouseEventKind::Drag(btn),
        3 => MouseEventKind::Moved,
        4 => MouseEventKind::ScrollDown,
        5 => MouseEventKind::ScrollUp,
        6 => MouseEventKind::ScrollLeft,
        _ => MouseEventKind::ScrollRight,
    }
}

fn decode_mouse_mods(mods: u8) -> KeyModifiers {
    let mut m = KeyModifiers::NONE;
    if mods & 0b001 != 0 {
        m |= KeyModifiers::CONTROL;
    }
    if mods & 0b010 != 0 {
        m |= KeyModifiers::SHIFT;
    }
    if mods & 0b100 != 0 {
        m |= KeyModifiers::ALT;
    }
    m
}

/// One unit of fuzz input. Each variant either translates to a
/// `SourceEvent` or invokes a harness side channel (`StartTurn`).
#[derive(Arbitrary, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FuzzOp {
    /// Single Unicode codepoint keystroke. Surrogates and out-of-range
    /// values are rewritten to `'?'` on translation.
    KeyUnicode(u32),
    /// Control-modified ASCII letter (`b % 26 + 'a'`).
    KeyCtrl(u8),
    /// Shift-modified ASCII letter.
    KeyShift(u8),
    /// Bare special key chosen by `which % SPECIALS.len()`.
    KeySpecial(u8),
    /// Shift-modified special key - drives shift+arrow / shift+Home/End
    /// selection-extend code paths that plain `KeySpecial` skips.
    KeySpecialShift(u8),
    /// Bracketed paste with arbitrary UTF-8 payload.
    Paste(String),
    /// Mouse event (click/drag/wheel/move). Routes through
    /// `dispatch_terminal_event` → `handle_mouse` so the scrollbar,
    /// click-to-focus, drag-selection, and viewport-pan paths are
    /// reachable from fuzzing.
    Mouse(MouseFuzz),
    /// Advance the virtual clock by `ms` milliseconds.
    Tick(u16),
    /// Wake any pending Lua callbacks.
    LuaWakeup,
    /// Terminal resize, clamped to `[1, 400]` per dimension.
    Resize {
        w: u16,
        h: u16,
    },

    /// Synthesize an active agent turn so subsequent engine events flow
    /// through the active-turn dispatch path.
    StartTurn(u8),

    EngineReady,
    EngineText(String),
    EngineTextDelta(String),
    EngineThinking(String),
    EngineThinkingDelta(String),
    EngineToolStart {
        call_id: u8,
        tool_name: String,
        args: ArgsBag,
    },
    EngineToolOutput {
        call_id: u8,
        chunk: String,
    },
    EngineToolFinish {
        call_id: u8,
        is_error: bool,
        content: String,
    },
    ExecOutput(String),
    ExecDone(Option<i32>),

    /// Emit `TurnComplete` against the currently-active turn (when any),
    /// carrying `msg_count` synthetic messages and a zero `TurnMeta`. Idle
    /// dispatch also accepts this and merges history when non-empty.
    /// If a compaction checkpoint is installed, the checkpoint prefix is
    /// preserved ahead of the incoming messages.
    EngineTurnComplete {
        msg_count: u8,
    },
    /// Emit `TurnError`. Active-turn dispatch ends the turn; idle dispatch
    /// surfaces a notification.
    EngineTurnError(String),
    /// Emit `Steered { text, count }`. Active-turn dispatch flushes both
    /// streams and drains up to `count` request-queued messages.
    EngineSteered {
        text: String,
        count: u8,
    },
    /// Emit `Retrying { delay_ms, attempt }`. Active-turn dispatch puts
    /// the working bar into `TurnPhase::Retrying`.
    EngineRetrying {
        delay_ms: u16,
        attempt: u8,
    },
    /// Emit `TokenUsage`. Active-turn dispatch accumulates cost, updates
    /// `context_tokens`, and re-enters `TurnPhase::Working`.
    EngineTokenUsage {
        prompt: u16,
        completion: u16,
        tps: u16,
        cost_cents: u8,
        background: bool,
    },
    /// Side channel: push a synthetic request-queued entry so `Steered`
    /// has something to drain.
    PushQueuedMessage(String),
    /// Deliver supervisor completion through the production job-completion handler.
    JobCompleted {
        id: String,
        exit_code: Option<i32>,
        termination: FuzzJobTermination,
    },
    /// Emit `Messages` against the currently-active turn (when any),
    /// carrying `msg_count` synthetic messages. Doesn't end the turn.
    EngineMessages {
        msg_count: u8,
    },
    /// Emit `RequestPermission`. Active-turn dispatch either auto-approves
    /// (queuing a `PermissionDecision`), defers the dialog onto the
    /// `pending_dialogs` queue (when a keystroke landed within
    /// `CONFIRM_DEFER_MS`), or registers a confirm dialog the user
    /// resolves. `request_id` is derived from `req_id` to round-trip
    /// through `PermissionDecision`.
    EngineRequestPermission {
        req_id: u8,
        call_id: u8,
        tool_name: String,
        summary: String,
        args: ArgsBag,
    },
    /// Side channel: approve the oldest pending confirm. Mirrors what
    /// `lua_handlers::handle_dialog_decision` does with `ConfirmChoice::Yes`.
    /// No-op when no confirm is pending.
    ApproveFirstConfirm,
    /// Side channel: deny the oldest pending confirm with `ConfirmChoice::No`.
    /// When `message` is `None`, denying cancels the agent turn (production
    /// `resolve_confirm` returns `true`); when `Some`, the turn continues.
    DenyFirstConfirm {
        message: Option<String>,
    },
    /// Emit `ToolDispatch`. Unknown `tool_name`s take the `Immediate
    /// { is_error: true }` path and queue a `ToolResult` UiCommand back
    /// in-step; known tools (autoloaded Lua) may yield Pending and resolve
    /// asynchronously, queueing 0 results this step. Exercises the dispatch
    /// surface and proves `handle_tool_call` is panic-free against arbitrary
    /// names and arg payloads.
    EngineToolDispatch {
        req_id: u8,
        call_id: u8,
        tool_name: String,
        args: ArgsBag,
    },
    /// Emit `ToolEvaluationRequest`. Active-turn dispatch always queues a
    /// `ToolEvaluationResponse` UiCommand back; `evaluate_tool_metadata` short-circuits
    /// when no Lua hook is registered.
    EngineToolEvaluationRequest {
        req_id: u8,
        call_id: u8,
        tool_name: String,
        args: ArgsBag,
    },
    /// Emit `CoreToolResult`. Active-turn dispatch routes to
    /// `lua.resolve_core_tool_call`, which is a no-op when no pending
    /// coroutine carries the `request_id`. Fuzz value: prove
    /// table-construction and `resolve_external` don't panic on arbitrary
    /// payloads.
    EngineCoreToolResult {
        req_id: u8,
        content: String,
        is_error: bool,
    },
    /// Emit `Shutdown`. Active-turn dispatch ends the turn without starting
    /// queued follow-up input; idle dispatch is a no-op.
    EngineShutdown {
        reason: Option<String>,
    },
    /// Side-channel: insert a synthetic image attachment at the prompt
    /// cursor through the real `Input::insert_image` path. Exercises the
    /// attachment_ids ↔ marker invariant under interleaved key + paste
    /// events.
    InsertAttachment {
        label: String,
    },
    /// Side-channel: flip pane focus between Prompt and Content. The Ctrl-W
    /// chord that reaches the same code path requires two keystrokes inside
    /// `PANE_CHORD_WINDOW`, which random fuzz inputs rarely hit.
    TogglePaneFocus,
    /// Emit `ToolCallDraftDelta`. The TUI accumulates `delta` into a per-stream
    /// JSON-fragment buffer, then displays draft args until the matching
    /// `ToolStarted` promotes the block. Exercises the streaming draft path
    /// against arbitrary out-of-order or orphan deltas.
    EngineToolDraftDelta {
        call_id: String,
        tool_name: String,
        delta: String,
    },
    /// Emit `EngineAskResponse`. Resumes a Lua coroutine that issued a
    /// one-shot `UiCommand::EngineAsk`; no-op when no coroutine is waiting
    /// on the synthesized `id`.
    EngineAskResponse {
        id: u64,
        content: String,
    },
    /// Emit `EngineAskResponse` with a typed error payload (empty content).
    /// Resumes a waiting Lua coroutine with the failure branch so plugins
    /// like compact.lua exercise their `err.kind` dispatch. `kind_idx` is
    /// modded against `ENGINE_ASK_ERROR_KINDS`.
    EngineAskError {
        id: u64,
        kind_idx: u8,
        message: String,
    },
    /// Side channel: invoke the `/reload` pipeline. Stresses named-slot
    /// survival (paint ids, buf/win/overlay NamedSlots) and `on_ready`
    /// re-entry (`ctx.kind = "reload"`) against arbitrary pre-state -
    /// the surface where the `fix(edit): drop named bindings on close`
    /// bug class lives.
    ReloadLua,
    /// Side channel: open a synthetic overlay via `smelt.overlay.new`.
    /// `variant % N` picks the shape (named/anonymous leaf, vbox, leaf
    /// with static measure, leaf with overlay-level keymap). Same
    /// `variant` reuses the same NamedSlot name so the dedup path is
    /// exercised, different variants populate different slots. Targets
    /// the new measure / overlay-keymap / NamedSlots surfaces.
    OpenOverlay {
        variant: u8,
    },
    /// Side channel: install a placeholder on the prompt window with the
    /// given text and accept / dismiss chord set. `variant % N` picks the
    /// chord pair (Tab/Esc, Enter/Esc, Ctrl-N/Ctrl-G, ...) so the routing
    /// branches in `dispatch_placeholder_key` are exercised against random
    /// follow-up keystrokes. The placeholder dispatcher fires on the next
    /// matching key while the buffer is empty.
    InstallPlaceholder {
        text: String,
        variant: u8,
    },
    /// Side channel: clear the prompt placeholder (extmark + opts). Mirrors
    /// `clear_placeholder` so scenarios can drop a placeholder without
    /// going through accept / dismiss.
    ClearPlaceholder,
    /// Side channel: emit `EngineAskResponse` with the smallest pending ask
    /// id, if any. Exercises `lua.fire_ask_callback` and plugin paths that
    /// depend on it (e.g., `/btw`, compaction plugins).
    EngineAskResponsePending {
        content: String,
    },
    /// Side channel: emit `EngineAskResponse` with a typed error payload
    /// against the smallest pending ask id, if any.
    EngineAskErrorPending {
        kind_idx: u8,
        message: String,
    },
    /// Side channel: open an exec block in the transcript so subsequent
    /// `ExecOutput`/`ExecDone` exercise the full lifecycle.
    StartExec {
        command: String,
    },
    /// Side channel: cancel the active turn (or idle background tasks).
    /// Exercises `discard_turn(true)` → flush streaming, send `Cancel`,
    /// `lua.cancel_tasks()`, and the interrupted-transcript path.
    Cancel,
    /// Emit `RequestPermission` with `tool_name = "bash"` so the
    /// permission hook evaluator routes through `bash::parse` and
    /// exercises multi-byte UTF-8 in the shell command parser.
    EngineRequestPermissionBash {
        req_id: u8,
        call_id: u8,
        command: String,
    },
    /// Side channel: push a steer text onto the queued-messages stack.
    /// Exercises the mid-turn steering queue growth path.
    Steer {
        text: String,
    },
    /// Side channel: remove up to `count` queued messages from the front.
    /// Exercises the mid-turn steering queue shrink path.
    Unsteer {
        count: u8,
    },
    /// Side channel: send a `CallCoreTool` UiCommand. Exercises the
    /// tool-call round-trip initiation path (pair with
    /// `EngineCoreToolResult` for the full lifecycle).
    CallCoreTool {
        tool_name: String,
        args: ArgsBag,
    },
    /// Side channel: change the active agent mode. Exercises mode-
    /// dependent rendering and permission rule-sets.
    SetAgentMode {
        mode: FuzzMode,
    },
    /// Side channel: install a prompt `text_changed` callback that attempts
    /// to repark the prompt cursor while typing is in progress.
    InstallPromptCursorTrap {
        variant: u8,
    },
    /// Side channel: register a Lua tool and begin a synthetic custom-command
    /// turn, asserting the outbound `StartTurn` carries Lua tool definitions.
    StartCustomCommand {
        variant: u8,
    },
    /// Side channel: issue a Lua `smelt.engine.ask` request so pending ask
    /// response/error ops exercise callback completion.
    StartEngineAsk {
        question: String,
    },
    /// Side channel: request host-owned reload scheduling. The harness then
    /// offers the same safe-point drain as the main loop, so active-turn
    /// schedules stay pending until a later turn-ending op while idle ones
    /// execute and clear the pending flag immediately.
    ScheduleReload,
    /// Side channel: deterministic regression probe for #15. Exercises typing
    /// and cursor motion after turn completion/error/cancel/reload/traps,
    /// app-level Escape sequences, and stale prompt side-request callbacks.
    ProbePromptCursorAfterTurn {
        variant: u8,
    },
    /// Side channel: drive the real compaction prepare-request + EngineAsk
    /// response path, including active-turn phase preservation.
    ProbeCompactionPrepareRequest {
        variant: u8,
    },
    /// Macro workload: paste a bounded text chunk repeatedly, move the cursor,
    /// and optionally submit. Random single-key streams rarely build large
    /// prompt buffers or hit multi-step edit/submit interactions.
    MacroPromptEdit {
        text: String,
        repetitions: u8,
        movement: u8,
        submit: bool,
    },
    /// Macro workload: create/target an active turn and run a matched
    /// ToolStarted -> ToolOutput -> ToolFinished sequence using one call id.
    /// This reaches paired lifecycle paths far more reliably than unrelated
    /// random engine events.
    MacroToolRoundTrip {
        call_id: u8,
        tool_name: String,
        args: ArgsBag,
        output: String,
        is_error: bool,
    },
    /// Macro workload: stream several text/thinking chunks, then finish or
    /// error the turn. Exercises transcript parser/render with realistic
    /// clustered provider output instead of isolated deltas.
    MacroStreamTurn {
        turn_id: u8,
        text: String,
        thinking: String,
        chunks: u8,
        error: bool,
    },
}

impl FuzzOp {
    /// Short human label used by `play_scenario` for its status line. Keeping
    /// labels next to the operation enum makes new variants a single edit.
    pub fn label(&self) -> String {
        use FuzzOp::*;
        match self {
            KeyUnicode(c) => format!("key {:?}", char::from_u32(*c).unwrap_or('?')),
            KeyCtrl(b) => format!("ctrl-{}", (b'a' + (b % 26)) as char),
            KeyShift(b) => format!("shift-{}", (b'a' + (b % 26)) as char),
            KeySpecial(_) => "special".into(),
            KeySpecialShift(_) => "shift+special".into(),
            Paste(s) => format!("paste {} chars", s.chars().count()),
            Mouse(m) => format!("mouse k={} b={} {},{}", m.kind, m.button, m.col, m.row),
            Tick(ms) => format!("tick {ms}ms"),
            LuaWakeup => "lua wakeup".into(),
            Resize { w, h } => format!("resize {w}x{h}"),
            StartTurn(id) => format!("start turn {id}"),
            EngineReady => "engine ready".into(),
            EngineText(_) => "engine text".into(),
            EngineTextDelta(_) => "engine text delta".into(),
            EngineThinking(_) => "engine thinking".into(),
            EngineThinkingDelta(_) => "engine thinking delta".into(),
            EngineToolStart { tool_name, .. } => format!("tool start {tool_name}"),
            EngineToolOutput { .. } => "tool output".into(),
            EngineToolFinish { is_error, .. } => {
                if *is_error {
                    "tool error".into()
                } else {
                    "tool done".into()
                }
            }
            ExecOutput(_) => "exec output".into(),
            ExecDone(code) => format!("exec done {code:?}"),
            EngineTurnComplete { msg_count } => format!("turn complete ({msg_count} msgs)"),
            EngineTurnError(_) => "turn error".into(),
            EngineSteered { count, .. } => format!("steered (drain {count})"),
            EngineRetrying { attempt, .. } => format!("retrying (attempt {attempt})"),
            EngineTokenUsage { prompt, .. } => format!("token usage (prompt {prompt})"),
            PushQueuedMessage(_) => "push queued message".into(),
            JobCompleted { id, .. } => format!("job completed {id}"),
            EngineMessages { msg_count } => format!("messages ({msg_count})"),
            EngineRequestPermission { tool_name, .. } => {
                format!("request permission {tool_name}")
            }
            ApproveFirstConfirm => "approve confirm".into(),
            DenyFirstConfirm { .. } => "deny confirm".into(),
            EngineToolDispatch { tool_name, .. } => format!("tool dispatch {tool_name}"),
            EngineToolEvaluationRequest { tool_name, .. } => format!("tool hooks {tool_name}"),
            EngineCoreToolResult { .. } => "core tool result".into(),
            EngineShutdown { .. } => "shutdown".into(),
            InsertAttachment { label } => format!("insert attachment {label}"),
            TogglePaneFocus => "toggle pane focus".into(),
            EngineToolDraftDelta { tool_name, .. } => format!("tool draft delta {tool_name}"),
            EngineAskResponse { id, .. } => format!("ask response {id}"),
            EngineAskError { id, kind_idx, .. } => format!("ask error {id} k={kind_idx}"),
            ReloadLua => "reload lua".into(),
            OpenOverlay { variant } => format!("open overlay v={variant}"),
            InstallPlaceholder { variant, .. } => format!("install placeholder v={variant}"),
            ClearPlaceholder => "clear placeholder".into(),
            EngineAskResponsePending { .. } => "ask response (pending)".into(),
            EngineAskErrorPending { .. } => "ask error (pending)".into(),
            StartExec { .. } => "start exec".into(),
            Cancel => "cancel".into(),
            EngineRequestPermissionBash { .. } => "request permission (bash)".into(),
            Steer { .. } => "steer".into(),
            Unsteer { count } => format!("unsteer {count}"),
            CallCoreTool { tool_name, .. } => format!("call core tool {tool_name}"),
            SetAgentMode { mode } => format!("set mode {mode:?}"),
            InstallPromptCursorTrap { variant } => format!("prompt cursor trap v={variant}"),
            StartCustomCommand { variant } => format!("custom command v={variant}"),
            StartEngineAsk { .. } => "engine ask".into(),
            ScheduleReload => "schedule reload".into(),
            ProbePromptCursorAfterTurn { variant } => format!("prompt cursor probe v={variant}"),
            ProbeCompactionPrepareRequest { variant } => format!("compaction probe v={variant}"),
            MacroPromptEdit {
                repetitions,
                submit,
                ..
            } => format!("macro prompt reps={repetitions} submit={submit}"),
            MacroToolRoundTrip { tool_name, .. } => format!("macro tool round-trip {tool_name}"),
            MacroStreamTurn { chunks, error, .. } => {
                format!("macro stream chunks={chunks} error={error}")
            }
        }
    }
}

#[derive(Arbitrary, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FuzzMode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

impl From<FuzzMode> for AgentMode {
    fn from(m: FuzzMode) -> Self {
        match m {
            FuzzMode::Normal => AgentMode::normal(),
            FuzzMode::Plan => AgentMode::parse("plan").unwrap(),
            FuzzMode::Apply => AgentMode::parse("apply").unwrap(),
            FuzzMode::Yolo => AgentMode::parse("yolo").unwrap(),
        }
    }
}

/// A full reproducible scenario: initial app config plus the event stream.
/// `FuzzInput` is also what `arbitrary` decodes from libFuzzer bytes, so a
/// crash artifact converts to a `Scenario` JSON via a single
/// `serde_json::to_string_pretty`. `Arbitrary` selects one named workload
/// profile up front, then samples operations from that profile. JSON
/// serialization round-trips the resulting operations; generator profile
/// state does not need to be persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzInput {
    pub vim: bool,
    pub mode: FuzzMode,
    pub ops: Vec<FuzzOp>,
}

impl<'a> Arbitrary<'a> for FuzzInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let vim = u.arbitrary()?;
        let mode = u.arbitrary()?;
        // One profile byte selects a coherent workload without consuming one
        // byte for every operation variant before any operation can decode.
        let swarm = app_swarm(u)?;
        let mut ops = Vec::new();
        while !u.is_empty() && ops.len() < MAX_OPS {
            let idx = swarm.pick(u)?;
            ops.push(build_fuzz_op(idx, u)?);
        }
        Ok(FuzzInput { vim, mode, ops })
    }
}

/// Fixed per-scenario operation weights. A target selects one named workload
/// profile with a single byte, then uses this table for the whole scenario.
/// Profiles make prerequisites common, isolate coherent subsystems, and leave
/// almost all input entropy for operations and payloads.
pub struct SwarmWeights {
    weights: Vec<u32>,
    total: u32,
}

impl SwarmWeights {
    pub fn profile(n: usize, enabled: &[usize], boosted: &[usize]) -> Self {
        assert!(n > 0, "swarm profiles need at least one variant");
        let mut weights = vec![0u32; n];
        for &index in enabled {
            *weights
                .get_mut(index)
                .unwrap_or_else(|| panic!("enabled swarm index {index} exceeds {n} variants")) = 4;
        }
        for &index in boosted {
            *weights
                .get_mut(index)
                .unwrap_or_else(|| panic!("boosted swarm index {index} exceeds {n} variants")) = 16;
        }
        if weights.iter().all(|weight| *weight == 0) {
            weights.fill(1);
        }
        let total = weights.iter().copied().sum();
        Self { weights, total }
    }

    pub fn uniform(n: usize) -> Self {
        assert!(n > 0, "uniform swarms need at least one variant");
        Self {
            weights: vec![1; n],
            total: n as u32,
        }
    }

    /// Sample a variant index, weighted by the selected profile.
    pub fn pick(&self, u: &mut Unstructured<'_>) -> arbitrary::Result<usize> {
        let pick = u.int_in_range(0u32..=self.total.saturating_sub(1))?;
        let mut acc = 0u32;
        for (i, &weight) in self.weights.iter().enumerate() {
            acc = acc.saturating_add(weight);
            if pick < acc {
                return Ok(i);
            }
        }
        Ok(self.weights.len() - 1)
    }
}

fn app_swarm(u: &mut Unstructured<'_>) -> arbitrary::Result<SwarmWeights> {
    const EDITING: &[usize] = &[
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 25, 32, 33, 42, 43, 44, 48, 53, 54, 55, 58, 60,
    ];
    const ENGINE: &[usize] = &[
        7, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 34, 35, 36,
        37, 38, 39, 40, 45, 46, 47, 48, 49, 50, 51, 56, 62,
    ];
    const TOOLS: &[usize] = &[10, 16, 17, 18, 28, 29, 30, 31, 34, 38, 39, 49, 52, 61];
    const RELOAD: &[usize] = &[
        0, 3, 7, 8, 9, 10, 25, 37, 40, 41, 42, 43, 44, 45, 46, 47, 48, 53, 54, 55, 56, 57, 58, 59,
        62,
    ];
    const TRANSCRIPT: &[usize] = &[
        6, 7, 9, 10, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 26, 27, 34, 37, 40, 42, 61, 62,
    ];

    let profile = u.arbitrary::<u8>()? % 6;
    Ok(match profile {
        0 => SwarmWeights::uniform(N_FUZZOP_VARIANTS),
        1 => SwarmWeights::profile(N_FUZZOP_VARIANTS, EDITING, &[0, 3, 5, 9, 60]),
        2 => SwarmWeights::profile(N_FUZZOP_VARIANTS, ENGINE, &[10, 12, 13, 37, 56, 62]),
        3 => SwarmWeights::profile(N_FUZZOP_VARIANTS, TOOLS, &[10, 16, 18, 28, 29, 39, 61]),
        4 => SwarmWeights::profile(N_FUZZOP_VARIANTS, RELOAD, &[10, 37, 41, 48, 57, 62]),
        _ => SwarmWeights::profile(N_FUZZOP_VARIANTS, TRANSCRIPT, &[9, 10, 13, 18, 37, 62]),
    })
}

/// Per-variant builder. The Arbitrary impl picks an index into
/// `FUZZOP_BUILDERS` and calls the corresponding closure to mint the
/// variant with random payload. Centralising this as a slice means
/// adding a new variant is exactly one edit: append a closure here.
/// `N_FUZZOP_VARIANTS` derives from `.len()`, so the index space and the
/// dispatch table cannot drift.
type FuzzOpBuilder = fn(&mut Unstructured<'_>) -> arbitrary::Result<FuzzOp>;

const FUZZOP_BUILDERS: &[FuzzOpBuilder] = &[
    |u| Ok(FuzzOp::KeyUnicode(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeyCtrl(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeyShift(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeySpecial(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeySpecialShift(u.arbitrary()?)),
    |u| Ok(FuzzOp::Paste(u.arbitrary()?)),
    |u| Ok(FuzzOp::Mouse(u.arbitrary()?)),
    |u| Ok(FuzzOp::Tick(u.arbitrary()?)),
    |_| Ok(FuzzOp::LuaWakeup),
    |u| {
        Ok(FuzzOp::Resize {
            w: u.arbitrary()?,
            h: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::StartTurn(u.arbitrary()?)),
    |_| Ok(FuzzOp::EngineReady),
    |u| Ok(FuzzOp::EngineText(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineTextDelta(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineThinking(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineThinkingDelta(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::EngineToolStart {
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolOutput {
            call_id: u.arbitrary()?,
            chunk: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolFinish {
            call_id: u.arbitrary()?,
            is_error: u.arbitrary()?,
            content: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::ExecOutput(u.arbitrary()?)),
    |u| Ok(FuzzOp::ExecDone(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineTurnError(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::EngineSteered {
            text: u.arbitrary()?,
            count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineRetrying {
            delay_ms: u.arbitrary()?,
            attempt: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineTokenUsage {
            prompt: u.arbitrary()?,
            completion: u.arbitrary()?,
            tps: u.arbitrary()?,
            cost_cents: u.arbitrary()?,
            background: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::PushQueuedMessage(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::JobCompleted {
            id: u.arbitrary()?,
            exit_code: u.arbitrary()?,
            termination: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineMessages {
            msg_count: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ApproveFirstConfirm),
    |u| {
        Ok(FuzzOp::DenyFirstConfirm {
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineCoreToolResult {
            req_id: u.arbitrary()?,
            content: u.arbitrary()?,
            is_error: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolEvaluationRequest {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::InsertAttachment {
            label: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::TogglePaneFocus),
    |u| {
        Ok(FuzzOp::EngineToolDraftDelta {
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            delta: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskResponse {
            id: u.arbitrary()?,
            content: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskError {
            id: u.arbitrary()?,
            kind_idx: u.arbitrary()?,
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineTurnComplete {
            msg_count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolDispatch {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineRequestPermission {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            summary: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineShutdown {
            reason: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ReloadLua),
    |u| {
        Ok(FuzzOp::OpenOverlay {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::InstallPlaceholder {
            text: u.arbitrary()?,
            variant: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ClearPlaceholder),
    |u| {
        Ok(FuzzOp::EngineAskResponsePending {
            content: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskErrorPending {
            kind_idx: u.arbitrary()?,
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::StartExec {
            command: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::Cancel),
    |u| {
        Ok(FuzzOp::EngineRequestPermissionBash {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            command: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::Steer {
            text: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::Unsteer {
            count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::CallCoreTool {
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::SetAgentMode {
            mode: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::InstallPromptCursorTrap {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::StartCustomCommand {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::StartEngineAsk {
            question: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ScheduleReload),
    |u| {
        Ok(FuzzOp::ProbePromptCursorAfterTurn {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::ProbeCompactionPrepareRequest {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::MacroPromptEdit {
            text: u.arbitrary()?,
            repetitions: u.arbitrary()?,
            movement: u.arbitrary()?,
            submit: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::MacroToolRoundTrip {
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
            output: u.arbitrary()?,
            is_error: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::MacroStreamTurn {
            turn_id: u.arbitrary()?,
            text: u.arbitrary()?,
            thinking: u.arbitrary()?,
            chunks: u.arbitrary()?,
            error: u.arbitrary()?,
        })
    },
];

/// Total `FuzzOp` variant count, derived from the dispatch table so it
/// cannot drift.
pub const N_FUZZOP_VARIANTS: usize = FUZZOP_BUILDERS.len();

/// Build one `FuzzOp` by variant index. Indices outside the table never
/// happen because `Arbitrary` for `FuzzOp` always picks within range.
fn build_fuzz_op(idx: usize, u: &mut Unstructured<'_>) -> arbitrary::Result<FuzzOp> {
    FUZZOP_BUILDERS[idx](u)
}

/// Alias clarifying intent at use sites: on-disk JSON is a `Scenario`,
/// the in-memory fuzz input is a `FuzzInput`. Same bytes either way.
pub type Scenario = FuzzInput;

/// Should `run_scenario` render a frame after applying this op? `true`
/// covers anything that changes what would be on screen: input edits,
/// engine stream updates, resize, mouse drag, overlay/dialog motion,
/// reload (re-fires `on_ready`, may open splashes). `false` for ops
/// that only nudge clock or queue-meta state (`Tick`, `EngineReady`,
/// `EngineTokenUsage`) - rendering after those is pure cost. Lifting
/// every `content/*` parser out of 0% coverage is exactly what this
/// classification unlocks; the trade-off is one frame projection per
/// non-trivial op (≈50% of ops, on average).
fn render_trigger(op: &FuzzOp) -> bool {
    !matches!(
        op,
        FuzzOp::Tick(_) | FuzzOp::EngineReady | FuzzOp::EngineTokenUsage { .. }
    )
}

pub const SPECIALS: &[KeyCode] = &[
    KeyCode::Enter,
    KeyCode::Esc,
    KeyCode::Backspace,
    KeyCode::Tab,
    KeyCode::Up,
    KeyCode::Down,
    KeyCode::Left,
    KeyCode::Right,
    KeyCode::Home,
    KeyCode::End,
    KeyCode::PageUp,
    KeyCode::PageDown,
    KeyCode::Delete,
];

pub const MAX_OPS: usize = 256;

/// Slack allowed on top of the post-build interner baseline.
pub const INTERN_SLACK: usize = 64;

const RESIZE_MIN: u16 = 1;
const RESIZE_MAX: u16 = 400;

fn key_event(code: KeyCode, mods: KeyModifiers) -> TermEvent {
    TermEvent::Key(KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn decode_codepoint(raw: u32) -> char {
    char::from_u32(raw).unwrap_or('?')
}

fn clamp_dim(d: u16) -> u16 {
    d.clamp(RESIZE_MIN, RESIZE_MAX)
}

/// Curated `(accept, dismiss)` chord pairs for `InstallPlaceholder`.
/// Each variant exercises a different placeholder dispatch path: bare
/// keys, modifier chords, multi-chord accept sets, and the empty-set
/// case where the placeholder is purely cosmetic.
fn placeholder_chord_pair(
    variant: u8,
) -> (Vec<tui::smelt_edit::KeyBind>, Vec<tui::smelt_edit::KeyBind>) {
    use tui::smelt_edit::KeyBind;
    let kb = |code, mods| KeyBind { code, mods };
    let tab = kb(KeyCode::Tab, KeyModifiers::NONE);
    let enter = kb(KeyCode::Enter, KeyModifiers::NONE);
    let esc = kb(KeyCode::Esc, KeyModifiers::NONE);
    let right = kb(KeyCode::Right, KeyModifiers::NONE);
    let ctrl_n = kb(KeyCode::Char('n'), KeyModifiers::CONTROL);
    let ctrl_g = kb(KeyCode::Char('g'), KeyModifiers::CONTROL);
    let ctrl_y = kb(KeyCode::Char('y'), KeyModifiers::CONTROL);
    match variant % 6 {
        0 => (vec![tab], vec![esc]),
        1 => (vec![enter], vec![esc]),
        2 => (vec![ctrl_n], vec![ctrl_g]),
        3 => (vec![tab, right, ctrl_y], vec![esc, ctrl_g]),
        4 => (vec![], vec![esc]),
        _ => (vec![], vec![]),
    }
}

/// Compress the random `u8` call-id space down to `CALL_ID_BUCKETS` so
/// `ToolStarted` and `ToolFinished` actually pair up under random fuzz
/// inputs. Without this, collisions happen at 1/256 per pair, which the
/// coverage report shows is too rare to ever exercise the matching
/// branches in `handle_engine_event`.
const CALL_ID_BUCKETS: u8 = 8;

fn fuzz_invocation_id(id: u8) -> protocol::InvocationId {
    protocol::InvocationId::new(u64::from(id % CALL_ID_BUCKETS))
}

fn call_id_string(id: u8) -> String {
    format!("call-{:02x}", id % CALL_ID_BUCKETS)
}

/// Synthesize a deterministic, lightweight history vector for compaction
/// payloads. Rotates through user, terminal assistant, and tool-call assistant
/// turns so history updates and screen restoration exercise every variant.
fn synth_history(count: usize) -> Vec<protocol::HistoryItem> {
    (0..count)
        .map(|i| {
            let body = format!("compacted-{i}");
            let reasoning_blocks = if i % 4 == 0 {
                Vec::new()
            } else {
                vec![ReasoningBlock {
                    provider: "fuzz".to_string(),
                    data: serde_json::Value::Null,
                }]
            };
            match i % 3 {
                0 => protocol::HistoryItem::user(Content::text(body)),
                1 => protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                    Some(Content::text(body)),
                    None,
                    reasoning_blocks,
                )),
                _ => {
                    let invocation = protocol::ToolInvocation {
                        call_id: format!("synth-call-{i:02}"),
                        name: "synth".to_string(),
                        arguments: "{}".to_string(),
                        result: ToolOutcome::new(format!("synth-result-{i}"), false, None),
                        elapsed_ms: None,
                        called_at_ms: Some(i as u64),
                    };
                    protocol::HistoryItem::Assistant(protocol::AssistantStep::with_invocations(
                        Some(Content::text(body)),
                        None,
                        reasoning_blocks,
                        vec![invocation],
                    ))
                }
            }
        })
        .collect()
}

/// Cheap snapshot of state needed by event-specific post-checks. Captured
/// before dispatch so a `PostCheck` can compare pre/post without re-deriving
/// what it cares about from scratch.
struct Snapshot {
    agent_running: bool,
    pending: Vec<protocol::InvocationId>,
    streaming: tui::app::test_harness::StreamingState,
    session_messages: usize,
    queued_messages: usize,
    queued_next_starts: bool,
    follow_up_context_append: bool,
    working: tui::app::test_harness::WorkingSnapshot,
    session_cost_usd: f64,
    context_tokens: Option<u32>,
    checkpoint_first_live_index: Option<usize>,
    pending_history_appends: usize,
    pending_history_follow_up: bool,
    pending_lua_reload: bool,
    transcript_blocks: usize,
    pending_confirms: usize,
    deferred_dialogs: usize,
    prompt_source: String,
    prompt_cpos: usize,
    prompt_vim_mode: VimMode,
    prompt_selection_range: Option<(usize, usize)>,
    prompt_text_input_ready: bool,
    prompt_paste_target_ready: bool,
    /// Length of the harness action log at capture time. Post-checks inspect
    /// actions produced after this snapshot.
    action_count: usize,
}

impl Snapshot {
    fn capture(app: &TestApp) -> Self {
        let prompt_selection_range = app.prompt_selection_range();
        Self {
            agent_running: app.agent_running(),
            pending: app.pending_tool_invocation_ids(),
            streaming: app.streaming_state(),
            session_messages: app.session_message_count(),
            queued_messages: app.queued_message_count(),
            queued_next_starts: app.next_queued_input_starts_turn(),
            follow_up_context_append: app.next_follow_up_appends_context_note(),
            working: app.working_state(),
            session_cost_usd: app.session_cost_usd(),
            context_tokens: app.context_tokens(),
            checkpoint_first_live_index: app.checkpoint_first_live_index(),
            pending_history_appends: app.pending_history_append_count(),
            pending_history_follow_up: app.has_pending_history_follow_up(),
            pending_lua_reload: app.pending_lua_reload(),
            transcript_blocks: app.transcript_block_count(),
            pending_confirms: app.pending_confirm_count(),
            deferred_dialogs: app.pending_deferred_dialog_count(),
            prompt_source: app.state().prompt_text,
            prompt_cpos: app.prompt_cpos(),
            prompt_vim_mode: app.state().vim_mode,
            prompt_selection_range,
            prompt_text_input_ready: app.prompt_text_input_ready(),
            prompt_paste_target_ready: app.prompt_paste_target_ready(prompt_selection_range),
            action_count: app.actions().len(),
        }
    }
}

fn expected_model_history_len(pre: &Snapshot, msg_count: usize) -> usize {
    pre.checkpoint_first_live_index.unwrap_or(0) + msg_count
}

fn completed_turn_starts_queued_followup(pre: &Snapshot, targeted_active: bool) -> bool {
    targeted_active
        && (pre.queued_next_starts || pre.pending_history_follow_up)
        && !pre.working.busy
}

fn expected_completed_history_len(
    pre: &Snapshot,
    msg_count: usize,
    targeted_active: bool,
) -> usize {
    let queued_followup = completed_turn_starts_queued_followup(pre, targeted_active);
    let queued_context_append = usize::from(queued_followup && pre.follow_up_context_append);
    let queued_request_append = usize::from(queued_followup);
    expected_model_history_len(pre, msg_count) + queued_context_append + queued_request_append
}

fn scheduled_reload_mode_change(
    pre: &Snapshot,
    targeted_active: bool,
    new_actions: &[Action],
) -> bool {
    pre.pending_lua_reload
        && targeted_active
        && count_action(new_actions, |c| matches!(c, UiCommand::SetMode { .. })) > 0
}

fn normalize_prompt_paste_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(smelt_buffer::ATTACHMENT_MARKER, "")
}

fn count_action<F>(actions: &[Action], pred: F) -> usize
where
    F: Fn(&UiCommand) -> bool,
{
    actions
        .iter()
        .filter(|a| matches!(a, Action::EngineSend(cmd) if pred(cmd.as_ref())))
        .count()
}

/// Post-dispatch invariant tied to a specific `FuzzOp`. Holds just the
/// payload the check needs; the pre/post `Snapshot`s carry everything else.
/// Variants gated on `agent_running` self-skip in idle dispatch, where the
/// relevant event arms are no-ops.
enum PostCheck {
    None,
    /// `Text` commits any streaming text buffer. Cascade: `flush_streaming_text`
    /// also flushes thinking, so both buffers must be empty after.
    TextFlushed,
    /// `Thinking { content }` commits any streaming thinking buffer.
    ThinkingFlushed,
    /// `ToolStarted` flushes streaming text + thinking and adds the invocation
    /// identity to pending.
    ToolStarted {
        invocation_id: protocol::InvocationId,
    },
    /// `ToolOutput` for an already-pending invocation is a pure append to that
    /// tool's output; the pending entry stays put.
    ToolOutput {
        invocation_id: protocol::InvocationId,
    },
    /// `ToolFinished` clears the invocation from pending when it was present.
    ToolFinished {
        invocation_id: protocol::InvocationId,
    },
    /// `ExecDone` finishes the exec block and clears `stream_exec_id`.
    ExecCleared,
    /// `TurnComplete` against the active turn. Non-empty messages replace
    /// session.messages; an active turn ends.
    TurnCompleted {
        msg_count: usize,
        targeted_active: bool,
    },
    /// `TurnError`. When an active turn was running, it ends.
    TurnErrored,
    /// `Steered`. Active-turn dispatch flushes streams and drains up to
    /// `count` request-queued messages.
    Steered {
        count: usize,
    },
    /// `Retrying`. Active-turn dispatch moves working into `Retrying`
    /// phase (still animating).
    Retrying,
    /// `TokenUsage`. Accumulates cost monotonically and (when non-background
    /// and prompt > 0) updates `context_tokens`.
    TokenUsageReceived {
        prompt: u32,
        cost_usd: f64,
        background: bool,
    },
    /// Job completion immediately pushes a transcript block when idle.
    JobCompleted,
    /// `Messages` against the active turn (matching turn_id) merges
    /// `session.messages` mid-turn; idle dispatch is a no-op.
    MessagesReplaced {
        msg_count: usize,
        targeted_active: bool,
    },
    /// `RequestPermission` against an active turn lands on exactly one of
    /// three branches: auto-approve (one new `PermissionDecision`,
    /// no new confirm), defer (one new `pending_dialogs` entry, no new
    /// confirm or decision), or register (one new entry in `core.confirms`,
    /// no decision yet).
    PermissionRequested,
    /// Approving / denying a confirm consumes one pending entry and queues
    /// a `PermissionDecision`. Approve never ends the turn; deny without a
    /// message ends it.
    ConfirmResolved {
        approved: bool,
        had_message: bool,
    },
    /// `ToolDispatch` with an unregistered tool produces exactly one
    /// `ToolResult` UiCommand (the synthetic error path). With a real Lua
    /// tool registered it could yield `Pending` instead, but the harness
    /// loads no tools so strict equality holds.
    ToolDispatched,
    /// `ToolEvaluationRequest` always produces exactly one `ToolEvaluationResponse`
    /// UiCommand, regardless of whether hooks are registered.
    ToolEvaluationRequested,
    /// `CoreToolResult` with no pending Lua coroutine is silently dropped;
    /// only the no-panic invariant matters.
    CoreToolResultReceived,
    /// `Shutdown` ends any active turn.
    ShutdownReceived,
    /// `Cancel` side channel ends any active turn and flushes streaming.
    TurnCancelled,
    /// Plain prompt character insertion should mutate source at the old cursor
    /// and advance the cursor by the inserted UTF-8 width.
    PromptCharInserted {
        ch: char,
        lua_keymap_bound: bool,
    },
    /// Paste into a ready prompt should insert the normalized paste text at the
    /// old cursor and advance by the inserted byte length.
    PromptTextInserted {
        text: String,
    },
}

fn run_check(check: PostCheck, pre: &Snapshot, post: &Snapshot, new_actions: &[Action]) {
    match check {
        PostCheck::None => {}
        PostCheck::TextFlushed => {
            if pre.agent_running {
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "EngineEvent::Text left streaming active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::ThinkingFlushed => {
            if pre.agent_running {
                assert!(
                    !post.streaming.thinking,
                    "EngineEvent::Reasoning left streaming thinking active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::ToolOutput { invocation_id } => {
            if pre.pending.contains(&invocation_id) {
                let count = post
                    .pending
                    .iter()
                    .filter(|id| **id == invocation_id)
                    .count();
                assert!(
                    count == 1,
                    "ToolOutput({invocation_id:?}) disturbed pending entry: {count} entries, pre {:?} post {:?}",
                    pre.pending,
                    post.pending
                );
            }
        }
        PostCheck::ToolStarted { invocation_id } => {
            if pre.agent_running {
                if !pre.pending.contains(&invocation_id) {
                    // Fresh ToolStarted flushes streaming text + thinking
                    // before pushing the tool block.
                    assert!(
                        !post.streaming.text && !post.streaming.thinking,
                        "ToolStarted({invocation_id:?}) left streaming active: {:?}",
                        post.streaming
                    );
                }
                // Duplicate dispatch must be a no-op on pending; either way,
                // the invocation ends up in `pending` exactly once.
                let count = post
                    .pending
                    .iter()
                    .filter(|id| **id == invocation_id)
                    .count();
                assert!(
                    count == 1,
                    "ToolStarted({invocation_id:?}) yields {count} pending entries, expected 1: {:?}",
                    post.pending
                );
            }
        }
        PostCheck::ToolFinished { invocation_id } => {
            if pre.pending.contains(&invocation_id) {
                assert!(
                    !post.pending.contains(&invocation_id),
                    "ToolFinished({invocation_id:?}) left pending entry: {:?}",
                    post.pending
                );
            }
        }
        PostCheck::ExecCleared => {
            assert!(
                !post.streaming.exec,
                "ExecDone left active-exec state: {:?}",
                post.streaming
            );
        }
        PostCheck::TurnCompleted {
            msg_count,
            targeted_active,
        } => {
            // Either active-turn dispatch (matching turn_id) or idle
            // dispatch with non-empty messages merges history. A compaction
            // checkpoint preserves its pre-summary prefix before appending
            // incoming model messages.
            let history_path = targeted_active || msg_count > 0;
            if history_path {
                let expected = expected_completed_history_len(pre, msg_count, targeted_active);
                let pending_appends = if targeted_active {
                    pre.pending_history_appends
                        .saturating_sub(usize::from(pre.pending_history_follow_up))
                } else {
                    0
                };
                if scheduled_reload_mode_change(pre, targeted_active, new_actions) {
                    // A reload deferred until turn end can invalidate the
                    // active mode and switch back to normal mode. That queues
                    // a mode-change history note after the final model history
                    // snapshot has been merged; depending on whether the
                    // merged tail is already a mode note, it either appends or
                    // replaces in place.
                    let max_expected = expected + pending_appends + usize::from(expected > 0);
                    assert!(
                        (expected..=max_expected).contains(&post.session_messages),
                        "TurnComplete did not merge session.messages with scheduled reload mode change: post {} (expected {expected}..={max_expected}, incoming {msg_count}, checkpoint prefix {:?}, pending appends {}, queued follow-up {}, queued context append {}, busy {})",
                        post.session_messages,
                        pre.checkpoint_first_live_index,
                        pending_appends,
                        pre.queued_next_starts,
                        pre.follow_up_context_append,
                        pre.working.busy,
                    );
                } else {
                    let max_expected = expected + pending_appends;
                    assert!(
                        (expected..=max_expected).contains(&post.session_messages),
                        "TurnComplete did not merge session.messages: post {} (expected {expected}..={max_expected}, incoming {msg_count}, checkpoint prefix {:?}, pending appends {}, queued follow-up {}, queued context append {}, busy {})",
                        post.session_messages,
                        pre.checkpoint_first_live_index,
                        pending_appends,
                        pre.queued_next_starts,
                        pre.follow_up_context_append,
                        pre.working.busy,
                    );
                }
            }
            if targeted_active && !completed_turn_starts_queued_followup(pre, targeted_active) {
                assert!(
                    !post.agent_running,
                    "TurnComplete against active turn did not end it",
                );
            }
        }
        PostCheck::TurnErrored => {
            if pre.agent_running {
                assert!(!post.agent_running, "TurnError did not end the active turn",);
            }
        }
        PostCheck::TokenUsageReceived {
            prompt,
            cost_usd,
            background,
        } => {
            // Idle dispatch drops TokenUsage entirely (falls through to
            // the `_ => {}` arm); only the active-turn path mutates
            // session state.
            if pre.agent_running {
                let expected = pre.session_cost_usd + cost_usd;
                assert!(
                    (post.session_cost_usd - expected).abs() < 1e-6,
                    "TokenUsage did not add cost {cost_usd}: pre {} → post {} (expected {expected})",
                    pre.session_cost_usd,
                    post.session_cost_usd,
                );
                if !background && prompt > 0 {
                    assert_eq!(
                        post.context_tokens,
                        Some(prompt),
                        "TokenUsage(prompt={prompt}, background=false) did not set context_tokens",
                    );
                }
            } else {
                assert_eq!(
                    post.session_cost_usd, pre.session_cost_usd,
                    "TokenUsage in idle dispatch should not accumulate cost",
                );
            }
        }
        PostCheck::JobCompleted => {
            if !pre.agent_running && !pre.working.busy {
                assert_eq!(
                    post.transcript_blocks,
                    pre.transcript_blocks + 1,
                    "job completion did not push exactly one transcript block: pre {} -> post {}",
                    pre.transcript_blocks,
                    post.transcript_blocks,
                );
            }
        }
        PostCheck::MessagesReplaced {
            msg_count,
            targeted_active,
        } => {
            // Only the active-turn arm with matching turn_id calls
            // set_history; idle dispatch is an explicit no-op.
            if targeted_active {
                let expected = expected_model_history_len(pre, msg_count);
                assert_eq!(
                    post.session_messages, expected,
                    "Messages did not merge session.messages: post {} (expected {expected}, incoming {msg_count}, checkpoint prefix {:?})",
                    post.session_messages,
                    pre.checkpoint_first_live_index,
                );
                // Doesn't end the turn.
                assert!(
                    post.agent_running,
                    "Messages on active turn ended it unexpectedly",
                );
            } else {
                assert_eq!(
                    post.session_messages, pre.session_messages,
                    "Messages in idle dispatch should not change session.messages",
                );
            }
        }
        PostCheck::Retrying => {
            if pre.agent_running {
                assert!(
                    post.working.animating,
                    "Retrying on active turn left working idle: {:?}",
                    post.working
                );
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "Retrying left streaming active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::Steered { count } => {
            // Idle dispatch is a no-op for Steered; only enforce on active.
            if pre.agent_running {
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "Steered left streaming active: {:?}",
                    post.streaming
                );
                let drained = count.min(pre.queued_messages);
                assert_eq!(
                    post.queued_messages,
                    pre.queued_messages - drained,
                    "Steered(count={count}) drain mismatch: pre {} → post {} (expected drain {})",
                    pre.queued_messages,
                    post.queued_messages,
                    drained,
                );
            }
        }
        PostCheck::PermissionRequested => {
            // Idle dispatch falls through to the `_ => {}` arm in
            // `handle_idle_engine_event` - no state change. Only enforce
            // the trichotomy when a turn was running.
            if pre.agent_running {
                let new_confirms = post.pending_confirms.saturating_sub(pre.pending_confirms);
                let new_deferred = post.deferred_dialogs.saturating_sub(pre.deferred_dialogs);
                let new_decisions = count_action(new_actions, |c| {
                    matches!(c, UiCommand::PermissionDecision { .. })
                });
                let total = new_confirms + new_deferred + new_decisions;
                assert_eq!(
                    total, 1,
                    "RequestPermission produced {total} effects (confirms={new_confirms}, deferred={new_deferred}, decisions={new_decisions}), expected exactly 1",
                );
            }
        }
        PostCheck::ConfirmResolved {
            approved,
            had_message,
        } => {
            // Side-channel returns false when no confirm was pending -
            // skip in that case.
            if pre.pending_confirms == 0 {
                return;
            }
            assert_eq!(
                post.pending_confirms,
                pre.pending_confirms - 1,
                "Resolve did not remove one confirm: pre {} -> post {}",
                pre.pending_confirms,
                post.pending_confirms,
            );
            let new_decisions = count_action(new_actions, |c| {
                matches!(c, UiCommand::PermissionDecision { .. })
            });
            assert_eq!(
                new_decisions, 1,
                "Resolve did not queue exactly one PermissionDecision (got {new_decisions})",
            );
            // Approve never ends the turn. Deny without a message ends it
            // (resolve_confirm returns true → discard_turn). Deny with a
            // message keeps the turn alive - the user sent steering text
            // instead of stopping.
            if pre.agent_running {
                let should_end = !approved && !had_message;
                if should_end {
                    assert!(
                        !post.agent_running,
                        "Deny without message did not end the turn",
                    );
                } else {
                    assert!(
                        post.agent_running,
                        "{} ended the turn unexpectedly",
                        if approved {
                            "Approve"
                        } else {
                            "Deny with message"
                        },
                    );
                }
            }
        }
        PostCheck::ToolDispatched => {
            if pre.agent_running {
                let new_results =
                    count_action(new_actions, |c| matches!(c, UiCommand::ToolResult { .. }));
                assert!(
                    new_results <= 1,
                    "ToolDispatch should queue at most one ToolResult in-step (got {new_results})",
                );
            }
        }
        PostCheck::ToolEvaluationRequested => {
            if pre.agent_running {
                let new_responses = count_action(new_actions, |c| {
                    matches!(c, UiCommand::ToolEvaluationResponse { .. })
                });
                assert_eq!(
                    new_responses, 1,
                    "ToolEvaluationRequest should queue exactly one ToolEvaluationResponse (got {new_responses})",
                );
            }
        }
        PostCheck::CoreToolResultReceived => {
            // No-op when no pending coroutine matches the request_id.
            // `assert_invariants` covers the no-panic property.
        }
        PostCheck::ShutdownReceived => {
            if pre.agent_running {
                assert!(!post.agent_running, "Shutdown did not end the active turn",);
            }
        }
        PostCheck::TurnCancelled => {
            if pre.agent_running {
                assert!(!post.agent_running, "Cancel did not end the active turn",);
            }
            assert!(
                !post.streaming.text && !post.streaming.thinking,
                "Cancel left streaming active: {:?}",
                post.streaming
            );
        }
        PostCheck::PromptCharInserted {
            ch,
            lua_keymap_bound,
        } => {
            if !pre.prompt_text_input_ready
                || lua_keymap_bound
                || ch.is_control()
                || ch == '\u{FFFC}'
                || (ch == '?' && pre.prompt_source.is_empty())
            {
                return;
            }
            let mut expected = pre.prompt_source.clone();
            let at = smelt_buffer::text::insert(&mut expected, pre.prompt_cpos, ch);
            assert_eq!(
                post.prompt_source, expected,
                "prompt char {ch:?} inserted at wrong location: pre cpos {}, pre {:?}, post {:?}",
                pre.prompt_cpos, pre.prompt_source, post.prompt_source,
            );
            assert_eq!(
                post.prompt_cpos,
                at + ch.len_utf8(),
                "prompt char {ch:?} left cursor at {}, expected {}",
                post.prompt_cpos,
                at + ch.len_utf8(),
            );
        }
        PostCheck::PromptTextInserted { text } => {
            if !pre.prompt_paste_target_ready {
                return;
            }
            let inserted = normalize_prompt_paste_text(&text);
            let mut expected = pre.prompt_source.clone();
            let expected_cpos = if let Some((start, end)) = pre.prompt_selection_range {
                smelt_buffer::text::replace_range(&mut expected, start..end, &inserted);
                start + inserted.len()
            } else {
                let at = smelt_buffer::text::insert_str(&mut expected, pre.prompt_cpos, &inserted);
                at + inserted.len()
            };
            assert_eq!(
                post.prompt_source, expected,
                "prompt paste changed source incorrectly: pre cpos {}, selection {:?}, pasted {:?}, normalized {:?}, pre {:?}, post {:?}",
                pre.prompt_cpos, pre.prompt_selection_range, text, inserted, pre.prompt_source, post.prompt_source,
            );
            assert_eq!(
                post.prompt_cpos, expected_cpos,
                "prompt paste left cursor at {}, expected {} after inserting {:?} from paste {:?}",
                post.prompt_cpos, expected_cpos, inserted, text,
            );
            if matches!(pre.prompt_vim_mode, VimMode::Visual | VimMode::VisualLine)
                && pre.prompt_selection_range.is_some()
            {
                assert_eq!(
                    post.prompt_vim_mode,
                    VimMode::Normal,
                    "prompt paste over {:?} left vim mode at {:?}",
                    pre.prompt_vim_mode,
                    post.prompt_vim_mode,
                );
            }
        }
    }
}

/// Translate a `FuzzOp` into the `SourceEvent` to feed and the post-check
/// to run after. `None` for the event means the op was handled inline (via
/// a harness side channel) and the caller should not feed anything.
fn plan(app: &TestApp, op: FuzzOp) -> (Option<SourceEvent>, PostCheck) {
    match op {
        FuzzOp::KeyUnicode(raw) => {
            let valid = char::from_u32(raw).is_some();
            let c = decode_codepoint(raw);
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::NONE));
            let check = if valid {
                PostCheck::PromptCharInserted {
                    ch: c,
                    lua_keymap_bound: app.prompt_plain_char_has_lua_keymap(c),
                }
            } else {
                PostCheck::None
            };
            (Some(ev), check)
        }
        FuzzOp::KeyCtrl(b) => {
            let c = (b'a' + (b % 26)) as char;
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::CONTROL));
            (Some(ev), PostCheck::None)
        }
        FuzzOp::KeyShift(b) => {
            let c = (b'a' + (b % 26)) as char;
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::SHIFT));
            (Some(ev), PostCheck::None)
        }
        FuzzOp::KeySpecial(which) => {
            let code = SPECIALS[(which as usize) % SPECIALS.len()];
            (
                Some(SourceEvent::Term(key_event(code, KeyModifiers::NONE))),
                PostCheck::None,
            )
        }
        FuzzOp::KeySpecialShift(which) => {
            let code = SPECIALS[(which as usize) % SPECIALS.len()];
            (
                Some(SourceEvent::Term(key_event(code, KeyModifiers::SHIFT))),
                PostCheck::None,
            )
        }
        FuzzOp::Paste(s) => {
            let check = PostCheck::PromptTextInserted { text: s.clone() };
            (Some(SourceEvent::Term(TermEvent::Paste(s))), check)
        }
        FuzzOp::Mouse(m) => {
            let ev = MouseEvent {
                kind: decode_mouse_kind(m.kind, m.button),
                column: u16::from(m.col),
                row: u16::from(m.row),
                modifiers: decode_mouse_mods(m.mods),
            };
            (
                Some(SourceEvent::Term(TermEvent::Mouse(ev))),
                PostCheck::None,
            )
        }
        FuzzOp::Tick(ms) => (Some(SourceEvent::Tick(u64::from(ms))), PostCheck::None),
        FuzzOp::LuaWakeup => (Some(SourceEvent::LuaWakeup), PostCheck::None),
        FuzzOp::Resize { w, h } => (
            Some(SourceEvent::Resize {
                width: clamp_dim(w),
                height: clamp_dim(h),
            }),
            PostCheck::None,
        ),
        // Side-channel: not a SourceEvent.
        FuzzOp::StartTurn(_) => (None, PostCheck::None),
        FuzzOp::EngineReady => (
            Some(SourceEvent::engine(EngineEvent::Ready)),
            PostCheck::None,
        ),
        FuzzOp::EngineText(s) => (
            Some(SourceEvent::engine(EngineEvent::Text { content: s })),
            PostCheck::TextFlushed,
        ),
        FuzzOp::EngineTextDelta(s) => (
            Some(SourceEvent::engine(EngineEvent::TextDelta { delta: s })),
            PostCheck::None,
        ),
        FuzzOp::EngineThinking(s) => (
            Some(SourceEvent::engine(EngineEvent::Reasoning {
                kind: ReasoningKind::Raw,
                title: None,
                content: s,
            })),
            PostCheck::ThinkingFlushed,
        ),
        FuzzOp::EngineThinkingDelta(s) => (
            Some(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                id: "fuzz:raw:0".into(),
                kind: ReasoningKind::Raw,
                delta: s,
                title: None,
            })),
            PostCheck::None,
        ),
        FuzzOp::EngineToolStart {
            call_id,
            tool_name,
            args,
        } => {
            let invocation_id = fuzz_invocation_id(call_id);
            let cid = call_id_string(call_id);
            let ev = SourceEvent::engine(EngineEvent::ToolStarted {
                invocation_id,
                call_id: cid,
                tool_name,
                args: args.into_map(),
                called_at_ms: u64::from(call_id),
            });
            (Some(ev), PostCheck::ToolStarted { invocation_id })
        }
        FuzzOp::EngineToolOutput { call_id, chunk } => {
            let invocation_id = fuzz_invocation_id(call_id);
            let cid = call_id_string(call_id);
            let ev = SourceEvent::engine(EngineEvent::ToolOutput {
                invocation_id,
                call_id: cid,
                line: chunk.replace(['\r', '\n'], " "),
            });
            (Some(ev), PostCheck::ToolOutput { invocation_id })
        }
        FuzzOp::EngineToolFinish {
            call_id,
            is_error,
            content,
        } => {
            let invocation_id = fuzz_invocation_id(call_id);
            let cid = call_id_string(call_id);
            let ev = SourceEvent::engine(EngineEvent::ToolFinished {
                invocation_id,
                call_id: cid,
                result: ToolOutcome::new(content, is_error, None),
                elapsed_ms: Some(0),
            });
            (Some(ev), PostCheck::ToolFinished { invocation_id })
        }
        FuzzOp::ExecOutput(s) => (Some(SourceEvent::ExecOutput(s)), PostCheck::None),
        FuzzOp::ExecDone(code) => (Some(SourceEvent::ExecDone(code)), PostCheck::ExecCleared),
        FuzzOp::EngineTurnComplete { .. } => {
            // Needs the live turn_id (not accessible here); handled
            // inline in `apply` before reaching `plan`.
            unreachable!("EngineTurnComplete handled inline in apply()")
        }
        FuzzOp::EngineTurnError(message) => {
            let ev = SourceEvent::engine(EngineEvent::TurnError {
                message,
                kind: None,
                retry_at_ms: None,
            });
            (Some(ev), PostCheck::TurnErrored)
        }
        FuzzOp::EngineSteered { text, count } => {
            let n = usize::from(count);
            let ev = SourceEvent::engine(EngineEvent::Steered {
                text,
                count: n,
                sent_at_ms: 1_742_567_823_000,
            });
            (Some(ev), PostCheck::Steered { count: n })
        }
        FuzzOp::EngineRetrying { delay_ms, attempt } => {
            let ev = SourceEvent::engine(EngineEvent::Retrying {
                delay_ms: u64::from(delay_ms),
                attempt: u32::from(attempt),
            });
            (Some(ev), PostCheck::Retrying)
        }
        FuzzOp::EngineTokenUsage {
            prompt,
            completion,
            tps,
            cost_cents,
            background,
        } => {
            let prompt = u32::from(prompt);
            let completion = u32::from(completion);
            let cost_usd = f64::from(cost_cents) / 100.0;
            let usage = TokenUsage {
                context_tokens: None,
                prompt_tokens: Some(prompt),
                completion_tokens: Some(completion),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            };
            let ev = SourceEvent::engine(EngineEvent::TokenUsage {
                usage,
                tokens_per_sec: Some(f64::from(tps)),
                cost_usd: Some(cost_usd),
                background,
            });
            (
                Some(ev),
                PostCheck::TokenUsageReceived {
                    prompt,
                    cost_usd,
                    background,
                },
            )
        }
        FuzzOp::PushQueuedMessage(_) => {
            unreachable!("PushQueuedMessage handled inline in apply()")
        }
        FuzzOp::JobCompleted { .. } => {
            unreachable!("JobCompleted handled inline in apply()")
        }
        FuzzOp::EngineMessages { .. } => {
            // Needs the live turn_id; handled inline in `apply` before
            // reaching `plan`.
            unreachable!("EngineMessages handled inline in apply()")
        }
        FuzzOp::EngineRequestPermission {
            req_id,
            call_id,
            tool_name,
            summary,
            args,
        } => {
            let ev = SourceEvent::engine(EngineEvent::RequestPermission {
                invocation_id: protocol::InvocationId::new(u64::from(req_id)),
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: args.into_map(),
                approval_patterns: Vec::new(),
                summary: protocol::style::StyledLines::from_plain(summary),
                called_at_ms: u64::from(req_id),
            });
            (Some(ev), PostCheck::PermissionRequested)
        }
        FuzzOp::EngineRequestPermissionBash {
            req_id,
            call_id,
            command,
        } => {
            let mut args = std::collections::HashMap::new();
            args.insert("command".to_string(), serde_json::Value::String(command));
            let ev = SourceEvent::engine(EngineEvent::RequestPermission {
                invocation_id: protocol::InvocationId::new(u64::from(req_id)),
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name: "bash".to_string(),
                args,
                approval_patterns: Vec::new(),
                summary: protocol::style::StyledLines::from_plain("bash".to_string()),
                called_at_ms: u64::from(req_id),
            });
            (Some(ev), PostCheck::PermissionRequested)
        }
        FuzzOp::ApproveFirstConfirm | FuzzOp::DenyFirstConfirm { .. } => {
            unreachable!("Approve/Deny side channels handled inline in apply()")
        }
        FuzzOp::EngineToolDispatch {
            req_id,
            call_id,
            tool_name,
            args,
        } => {
            let ev = SourceEvent::engine(EngineEvent::ToolDispatch {
                invocation_id: protocol::InvocationId::new(u64::from(req_id)),
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: args.into_map(),
            });
            (Some(ev), PostCheck::ToolDispatched)
        }
        FuzzOp::EngineToolEvaluationRequest {
            req_id,
            call_id,
            tool_name,
            args,
        } => {
            let ev = SourceEvent::engine(EngineEvent::ToolEvaluationRequest {
                invocation_id: protocol::InvocationId::new(u64::from(req_id)),
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: args.into_map(),
                mode: AgentMode::normal(),
            });
            (Some(ev), PostCheck::ToolEvaluationRequested)
        }
        FuzzOp::EngineCoreToolResult {
            req_id,
            content,
            is_error,
        } => {
            let ev = SourceEvent::engine(EngineEvent::CoreToolResult {
                request_id: u64::from(req_id),
                content,
                is_error,
                metadata: None,
            });
            (Some(ev), PostCheck::CoreToolResultReceived)
        }
        FuzzOp::EngineShutdown { reason } => {
            let ev = SourceEvent::engine(EngineEvent::Shutdown { reason });
            (Some(ev), PostCheck::ShutdownReceived)
        }
        FuzzOp::Cancel
        | FuzzOp::Steer { .. }
        | FuzzOp::Unsteer { .. }
        | FuzzOp::CallCoreTool { .. }
        | FuzzOp::SetAgentMode { .. }
        | FuzzOp::InstallPromptCursorTrap { .. }
        | FuzzOp::StartCustomCommand { .. }
        | FuzzOp::StartEngineAsk { .. }
        | FuzzOp::ScheduleReload
        | FuzzOp::ProbePromptCursorAfterTurn { .. }
        | FuzzOp::ProbeCompactionPrepareRequest { .. }
        | FuzzOp::InsertAttachment { .. }
        | FuzzOp::TogglePaneFocus
        | FuzzOp::ReloadLua
        | FuzzOp::OpenOverlay { .. }
        | FuzzOp::InstallPlaceholder { .. }
        | FuzzOp::ClearPlaceholder
        | FuzzOp::EngineAskResponsePending { .. }
        | FuzzOp::EngineAskErrorPending { .. }
        | FuzzOp::StartExec { .. } => {
            unreachable!("side channels handled inline in apply()")
        }
        FuzzOp::EngineToolDraftDelta {
            call_id,
            tool_name,
            delta,
        } => {
            let ev = SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
                stream_id: call_id.clone(),
                call_id: Some(call_id),
                tool_name: Some(tool_name),
                delta,
            });
            (Some(ev), PostCheck::None)
        }
        FuzzOp::EngineAskResponse { id, content } => {
            let ev = SourceEvent::engine(EngineEvent::EngineAskResponse {
                id,
                message: Some(Message::assistant(Some(Content::text(content)), None, None)),
                error: None,
            });
            (Some(ev), PostCheck::None)
        }
        FuzzOp::EngineAskError {
            id,
            kind_idx,
            message,
        } => {
            let kind = ENGINE_ASK_ERROR_KINDS[(kind_idx as usize) % ENGINE_ASK_ERROR_KINDS.len()];
            let ev = SourceEvent::engine(EngineEvent::EngineAskResponse {
                id,
                message: None,
                error: Some(EngineAskError { kind, message }),
            });
            (Some(ev), PostCheck::None)
        }
        FuzzOp::MacroPromptEdit { .. }
        | FuzzOp::MacroToolRoundTrip { .. }
        | FuzzOp::MacroStreamTurn { .. } => {
            unreachable!("macro ops handled inline in apply()")
        }
    }
}

fn apply_macro_prompt_edit(
    app: &mut TestApp,
    text: String,
    repetitions: u8,
    movement: u8,
    submit: bool,
) {
    let bounded = smelt_buffer::text::slice(&text, 0..128).to_string();
    let reps = usize::from(repetitions % 8) + 1;
    for _ in 0..reps {
        apply(app, FuzzOp::Paste(bounded.clone()));
        if app.quit_requested() {
            return;
        }
    }
    let moves = usize::from(movement % 16);
    let key = if movement & 0x80 == 0 { 7 } else { 6 }; // right / left in SPECIALS
    for _ in 0..moves {
        apply(app, FuzzOp::KeySpecial(key));
        if app.quit_requested() {
            return;
        }
    }
    if submit {
        apply(app, FuzzOp::KeySpecial(0));
    }
}

fn apply_macro_tool_round_trip(
    app: &mut TestApp,
    call_id: u8,
    tool_name: String,
    args: ArgsBag,
    output: String,
    is_error: bool,
) {
    if !app.agent_running() {
        app.start_turn(u64::from(call_id));
    }
    apply(
        app,
        FuzzOp::EngineToolStart {
            call_id,
            tool_name,
            args,
        },
    );
    if app.quit_requested() {
        return;
    }
    let output = smelt_buffer::text::slice(&output, 0..256).to_string();
    apply(
        app,
        FuzzOp::EngineToolOutput {
            call_id,
            chunk: output.clone(),
        },
    );
    if app.quit_requested() {
        return;
    }
    apply(
        app,
        FuzzOp::EngineToolFinish {
            call_id,
            is_error,
            content: output,
        },
    );
}

fn apply_macro_stream_turn(
    app: &mut TestApp,
    turn_id: u8,
    text: String,
    thinking: String,
    chunks: u8,
    error: bool,
) {
    if !app.agent_running() {
        app.start_turn(u64::from(turn_id));
    }
    let text = smelt_buffer::text::slice(&text, 0..256).to_string();
    let thinking = smelt_buffer::text::slice(&thinking, 0..256).to_string();
    let chunks = usize::from(chunks % 8) + 1;
    for i in 0..chunks {
        if !text.is_empty() {
            apply(app, FuzzOp::EngineTextDelta(format!("{i}:{text}")));
        }
        if !thinking.is_empty() {
            apply(app, FuzzOp::EngineThinkingDelta(format!("{i}:{thinking}")));
        }
        if app.quit_requested() {
            return;
        }
    }
    if error {
        apply(app, FuzzOp::EngineTurnError("macro stream error".into()));
    } else {
        apply(app, FuzzOp::EngineTurnComplete { msg_count: 1 });
    }
}

/// Side channels are `FuzzOp` variants that bypass `feed_one_within_budget`
/// and poke the app directly (host-level pokes the engine never sees). The
/// dispatcher returns `Ok(())` if the op was a side channel and was handled,
/// or `Err(op)` if the op should fall through to the event-feeding path.
/// Owned-data variants take ownership cleanly via this single match.
fn try_dispatch_side_channel(app: &mut TestApp, op: FuzzOp) -> Result<(), FuzzOp> {
    match op {
        FuzzOp::PushQueuedMessage(text) => app.push_queued_message(text),
        FuzzOp::JobCompleted {
            id,
            exit_code,
            termination,
        } => {
            let pre = Snapshot::capture(app);
            app.handle_job_completed(id, exit_code, termination.into());
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(PostCheck::JobCompleted, &pre, &post, new_actions);
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::InsertAttachment { label } => app.insert_attachment(label),
        FuzzOp::InstallPlaceholder { text, variant } => {
            let (accept, dismiss) = placeholder_chord_pair(variant);
            app.install_prompt_placeholder(text, accept, dismiss);
        }
        FuzzOp::ClearPlaceholder => app.clear_prompt_placeholder(),
        FuzzOp::StartTurn(id) => app.start_turn(u64::from(id)),
        FuzzOp::TogglePaneFocus => app.toggle_pane_focus(),
        FuzzOp::OpenOverlay { variant } => app.open_synthetic_overlay(variant),
        FuzzOp::ReloadLua => {
            app.reload_lua();
        }
        FuzzOp::ApproveFirstConfirm => {
            let pre = Snapshot::capture(app);
            app.resolve_first_confirm(true, None);
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(
                PostCheck::ConfirmResolved {
                    approved: true,
                    had_message: false,
                },
                &pre,
                &post,
                new_actions,
            );
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::DenyFirstConfirm { message } => {
            let pre = Snapshot::capture(app);
            let had_message = message.is_some();
            app.resolve_first_confirm(false, message);
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(
                PostCheck::ConfirmResolved {
                    approved: false,
                    had_message,
                },
                &pre,
                &post,
                new_actions,
            );
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::EngineAskResponsePending { content } => {
            if let Some(id) = app.pending_ask_id() {
                let ev = SourceEvent::engine(EngineEvent::EngineAskResponse {
                    id,
                    message: Some(Message::assistant(Some(Content::text(content)), None, None)),
                    error: None,
                });
                app.feed_one(ev);
            }
        }
        FuzzOp::EngineAskErrorPending { kind_idx, message } => {
            if let Some(id) = app.pending_ask_id() {
                let kind =
                    ENGINE_ASK_ERROR_KINDS[(kind_idx as usize) % ENGINE_ASK_ERROR_KINDS.len()];
                let ev = SourceEvent::engine(EngineEvent::EngineAskResponse {
                    id,
                    message: None,
                    error: Some(EngineAskError { kind, message }),
                });
                app.feed_one(ev);
            }
        }
        FuzzOp::StartExec { command } => {
            app.start_exec(&command);
        }
        FuzzOp::Cancel => {
            let pre = Snapshot::capture(app);
            app.cancel();
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(PostCheck::TurnCancelled, &pre, &post, new_actions);
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::Steer { text } => {
            app.steer(&text);
        }
        FuzzOp::Unsteer { count } => {
            app.unsteer(usize::from(count));
        }
        FuzzOp::CallCoreTool { tool_name, args } => {
            app.call_core_tool(&tool_name, args.into_map());
        }
        FuzzOp::SetAgentMode { mode } => {
            app.set_agent_mode(mode.into());
        }
        FuzzOp::InstallPromptCursorTrap { variant } => {
            app.install_prompt_cursor_trap(variant);
        }
        FuzzOp::StartCustomCommand { variant } => {
            let payload = app
                .start_custom_command_with_lua_tool(variant)
                .expect("custom-command side channel did not send StartTurn");
            let expected = format!("fuzz_custom_tool_{}", variant % 4);
            assert!(
                payload.tools.iter().any(|t| t.name == expected),
                "custom-command StartTurn missing registered tool {expected}: {:?}",
                payload.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
            );
        }
        FuzzOp::StartEngineAsk { question } => {
            app.start_engine_ask_probe(&question);
        }
        FuzzOp::ScheduleReload => {
            let was_running = app.agent_running();
            let already_pending = app.pending_lua_reload();
            app.schedule_lua_reload();
            app.drain_idle_work();
            if was_running {
                assert!(
                    app.pending_lua_reload(),
                    "scheduled reload ran while an agent turn was active"
                );
            } else if !already_pending {
                // Idle scheduled reloads drain at the next safe point, which
                // this side channel offers immediately after scheduling.
                app.assert_lua_handles_alive();
            }
        }
        FuzzOp::MacroPromptEdit {
            text,
            repetitions,
            movement,
            submit,
        } => apply_macro_prompt_edit(app, text, repetitions, movement, submit),
        FuzzOp::MacroToolRoundTrip {
            call_id,
            tool_name,
            args,
            output,
            is_error,
        } => apply_macro_tool_round_trip(app, call_id, tool_name, args, output, is_error),
        FuzzOp::MacroStreamTurn {
            turn_id,
            text,
            thinking,
            chunks,
            error,
        } => apply_macro_stream_turn(app, turn_id, text, thinking, chunks, error),
        FuzzOp::ProbePromptCursorAfterTurn { variant } => {
            app.probe_prompt_cursor_after_turn(variant);
        }
        FuzzOp::ProbeCompactionPrepareRequest { variant } => {
            app.probe_compaction_prepare_request(variant);
        }
        other => return Err(other),
    }
    Ok(())
}

/// Apply one `FuzzOp` to a `TestApp`. Every op rolls through the same path:
/// pre-snapshot → feed event (or side-channel) → post-snapshot → check →
/// global invariants.
pub fn apply(app: &mut TestApp, op: FuzzOp) {
    let op = match try_dispatch_side_channel(app, op) {
        Ok(()) => {
            app.assert_invariants();
            return;
        }
        Err(op) => op,
    };

    let pre = Snapshot::capture(app);
    // `TurnComplete` is gated on `turn_id` matching the live agent - read
    // it here so the dispatched event hits the active arm whenever a turn
    // is running. Idle dispatch still applies for the `agent_running ==
    // false` case.
    let (ev, check) = match op {
        FuzzOp::EngineTurnComplete { msg_count } => {
            let count = usize::from(msg_count);
            let id = app.current_turn_id().unwrap_or(0);
            let first_index = pre.checkpoint_first_live_index.unwrap_or(0);
            let ev = SourceEvent::engine(EngineEvent::TurnComplete {
                turn_id: id,
                history: Some(CanonicalHistoryDelta::new(
                    first_index,
                    synth_history(count),
                )),
                meta: None,
            });
            (
                Some(ev),
                PostCheck::TurnCompleted {
                    msg_count: count,
                    targeted_active: pre.agent_running,
                },
            )
        }
        FuzzOp::EngineMessages { msg_count } => {
            let count = usize::from(msg_count);
            let id = app.current_turn_id().unwrap_or(0);
            let first_index = pre.checkpoint_first_live_index.unwrap_or(0);
            let ev = SourceEvent::engine(EngineEvent::HistoryUpdated {
                turn_id: id,
                update: CanonicalHistoryDelta::new(first_index, synth_history(count)),
            });
            (
                Some(ev),
                PostCheck::MessagesReplaced {
                    msg_count: count,
                    targeted_active: pre.agent_running,
                },
            )
        }
        op => plan(app, op),
    };
    if let Some(ev) = ev {
        app.feed_one_within_budget(ev, AllocBudget::DEFAULT);
    }
    let post = Snapshot::capture(app);
    let new_actions = app.actions_since(pre.action_count);
    run_check(check, &pre, &post, new_actions);
    check_turn_end_invariants(&pre, &post);
    app.assert_invariants();
}

/// Cross-cutting invariants for any op that ends an active turn.
/// `finish_turn` always flushes streaming text + thinking before clearing
/// the agent; if either survives, the flush path was skipped.
fn check_turn_end_invariants(pre: &Snapshot, post: &Snapshot) {
    if pre.agent_running && !post.agent_running {
        assert!(
            !post.streaming.text && !post.streaming.thinking,
            "turn ended with streaming active: {:?}",
            post.streaming
        );
    }
}

/// Build a fresh `TestApp` configured for the scenario's initial state.
/// Bypasses the invariant-only path so visual replay code can advance
/// step-by-step.
pub fn build_app(scenario: &Scenario) -> TestApp {
    TestApp::builder()
        .with_vim(scenario.vim)
        .with_mode(scenario.mode.into())
        .build()
}

/// Apply the first `n` ops from `scenario` to `app`. Used by replay
/// drivers that need to rewind to an earlier step by rebuilding and
/// fast-forwarding.
pub fn apply_n(app: &mut TestApp, scenario: &Scenario, n: usize) {
    let n = n.min(scenario.ops.len()).min(MAX_OPS);
    for op in scenario.ops.iter().take(n).cloned() {
        apply(app, op);
        if app.quit_requested() {
            break;
        }
    }
}

/// Drive a fresh `TestApp` through a scenario from start to finish.
/// Returns when the scenario is exhausted or the app requests quit.
/// Used by the fuzz target itself and by any external replay code that
/// just wants to re-run a scenario to confirm a crash.
pub fn run_scenario(scenario: Scenario) {
    runtime::with_current_thread_runtime("fuzz harness", || {
        let mut app = TestApp::builder()
            .with_vim(scenario.vim)
            .with_mode(scenario.mode.into())
            .build();
        let theme_baseline = smelt_style::theme::registry_len();
        let ns_baseline = smelt_buffer::buffer::namespace_count();

        let take = scenario.ops.len().min(MAX_OPS);
        for op in scenario.ops.into_iter().take(take) {
            let render_after = render_trigger(&op);
            apply(&mut app, op);
            if app.quit_requested() {
                break;
            }
            if render_after {
                app.render_silent();
            }
        }
        // Always render once at the end so the final state passes through the
        // projection - covers scenarios that end on a `Tick` and would
        // otherwise skip the renderer entirely.
        app.render_silent();

        let theme_end = smelt_style::theme::registry_len();
        let ns_end = smelt_buffer::buffer::namespace_count();
        assert!(
            theme_end <= theme_baseline + INTERN_SLACK,
            "theme registry leaked: {} -> {} (slack {})",
            theme_baseline,
            theme_end,
            INTERN_SLACK
        );
        assert!(
            ns_end <= ns_baseline + INTERN_SLACK,
            "namespace registry leaked: {} -> {} (slack {})",
            ns_baseline,
            ns_end,
            INTERN_SLACK
        );
        app.assert_lua_runtime_released();
    });
}
