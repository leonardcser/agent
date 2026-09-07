pub(crate) mod agent;
pub(crate) mod cmdline;
pub(crate) mod cmdline_edit;
pub(crate) mod cmdline_history;
pub(crate) mod content_keys;
pub(crate) mod conversation;
pub(crate) mod cwd;
pub(crate) mod dialog;
pub(crate) mod document;
pub(crate) mod drafts;
pub(crate) mod engine_events;
pub(crate) mod events;
#[cfg(test)]
mod harness_tests;
pub(crate) mod history;
pub(crate) mod host_dispatch;
pub(crate) mod lua_bridge;
pub(crate) mod lua_handlers;
pub(crate) mod managed_models;
pub(crate) mod mouse;
pub(crate) mod overlay_runtime;
pub(crate) mod pane_focus;
pub(crate) mod platform_runtime;
pub(crate) mod prompt_runtime;
pub(crate) mod queue;
pub(crate) mod render_loop;
pub(crate) mod reveal;
pub(crate) mod search;
pub(crate) mod session_document;
pub(crate) mod session_load;
pub(crate) mod session_preview;
pub(crate) mod shell_panel;
#[cfg(any(test, feature = "harness"))]
pub mod test_harness;
pub(crate) mod transcript;
pub(crate) mod transcript_detail;
pub(crate) mod transcript_hydration;
pub(crate) mod transcript_scroll;
pub(crate) mod transcript_scroll_trace;
pub(crate) mod transcript_search;
pub(crate) mod transcript_work;
pub(crate) mod ui_host;
pub(crate) mod well_known;

use crate::input::{PromptState, SubmitEdit};
use engine::EngineHandle;
use protocol::Content;
use smelt_core::history::History;
use smelt_core::transcript_model::Block;
use smelt_core::ConfirmRequest;
use smelt_core::FrontendKind;
use std::sync::Arc;

use crossterm::{event, terminal};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const READY_QUEUE_DRAIN_MAX_ITEMS_PER_FRAME: usize = 64;
const READY_QUEUE_DRAIN_MAX_DURATION: Duration = Duration::from_millis(8);
const BACKGROUND_WORKER_SHUTDOWN_DEADLINE: Duration = Duration::from_millis(50);
pub(crate) const DOCKED_DIALOG_TRANSCRIPT_ROWS: u16 = 5;

fn join_background_worker(thread: std::thread::JoinHandle<()>) {
    let deadline = Instant::now() + BACKGROUND_WORKER_SHUTDOWN_DEADLINE;
    while !thread.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    if thread.is_finished() {
        let _ = thread.join();
    } else {
        smelt_perf::perf::record_value("app:worker_shutdown:detached", 1);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ContextWindowTarget {
    pub(crate) model_key: String,
    pub(crate) model: String,
    pub(crate) api_base: String,
    pub(crate) provider_type: String,
    pub(crate) config: protocol::ModelConfig,
}

impl ContextWindowTarget {
    pub(crate) fn from_active(model: &smelt_core::ActiveModel) -> Self {
        Self {
            model_key: model.key.clone(),
            model: model.model_name.clone(),
            api_base: model.api_base.clone(),
            provider_type: model.provider_type.clone(),
            config: model.config.clone(),
        }
    }
}

pub(crate) struct ContextWindowUpdate {
    pub(crate) revision: u64,
    pub(crate) target: ContextWindowTarget,
    pub(crate) value: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerRevisionStatus {
    pub desired_revision: u64,
    pub observed_revision: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeControllerStatus {
    pub mcp: Option<smelt_core::mcp::McpControllerStatus>,
    pub lsp: smelt_core::lsp::LspControllerStatus,
    pub auto_reload: ControllerRevisionStatus,
    pub context_window: ControllerRevisionStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedProviderStatusSnapshot {
    pub(crate) name: String,
    pub(crate) authenticated: bool,
    pub(crate) status: &'static str,
    pub(crate) error: Option<String>,
    pub(crate) request_id: Option<u64>,
    pub(crate) auth_revision: u64,
    pub(crate) desired_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelStatusSnapshot {
    pub(crate) current: Option<String>,
    pub(crate) requested: Option<String>,
    pub(crate) availability: &'static str,
    pub(crate) reason: Option<String>,
    pub(crate) providers: Vec<ManagedProviderStatusSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LuaBringUpError {
    pub(crate) message: String,
    pub(crate) location: smelt_core::lua::LuaLoadFailureLocation,
}

impl std::fmt::Display for LuaBringUpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Debug)]
pub struct SharedSessionState {
    pub id: String,
    pub has_messages: bool,
    pub ephemeral: bool,
}

#[derive(Clone, Debug)]
pub struct ShutdownContext {
    pub session_id: String,
    pub has_messages: bool,
    pub ephemeral: bool,
}

#[derive(Debug)]
pub enum SessionPersistence {
    Persistent,
    Ephemeral { dir: tempfile::TempDir },
}

impl SessionPersistence {
    pub fn persistent() -> Self {
        Self::Persistent
    }

    pub fn ephemeral() -> std::io::Result<Self> {
        tempfile::Builder::new()
            .prefix("smelt-session-")
            .tempdir()
            .map(|dir| Self::Ephemeral { dir })
    }

    fn is_ephemeral(&self) -> bool {
        matches!(self, Self::Ephemeral { .. })
    }

    fn artifact_dir(
        &self,
        sessions: &smelt_core::session::SessionStorage,
        session: &smelt_core::session::Session,
    ) -> std::path::PathBuf {
        match self {
            Self::Persistent => sessions.artifact_dir_for(session),
            Self::Ephemeral { dir } => dir.path().to_path_buf(),
        }
    }

    fn transcript_store_address(
        &self,
        sessions: &smelt_core::session::SessionStorage,
        session: &smelt_core::session::Session,
    ) -> Option<crate::app::transcript::TranscriptStoreAddress> {
        matches!(self, Self::Persistent)
            .then(|| sessions.resolve_session_for_read(&session.id))
            .flatten()
            .map(|resolved| {
                crate::app::transcript::TranscriptStoreAddress::new(
                    resolved.sessions_root,
                    resolved.id,
                    resolved.lineage_id,
                )
            })
    }
}

pub struct TuiAppOptions {
    pub startup_auth_error: Option<String>,
    pub app_events: Option<(
        tokio::sync::mpsc::UnboundedSender<AppEvent>,
        tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    )>,
    pub managed_models: Option<smelt_core::ManagedModels>,
    pub skills: Option<Arc<engine::SkillLoader>>,
    pub mcp: Option<Arc<smelt_core::mcp::McpManager>>,
    pub prompt_inputs: Option<crate::prompt_inputs::PromptInputs>,
    /// Remembered startup selections applied after generation-zero Lua has
    /// declared the final model and mode catalogs.
    pub startup_selections: smelt_core::RuntimeSelections,
    pub session_persistence: SessionPersistence,
}

impl Default for TuiAppOptions {
    fn default() -> Self {
        Self {
            startup_auth_error: None,
            app_events: None,
            managed_models: None,
            skills: None,
            mcp: None,
            prompt_inputs: None,
            startup_selections: smelt_core::RuntimeSelections::default(),
            session_persistence: SessionPersistence::persistent(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum SessionAccess {
    Owned,
    ReadOnly { reason: String },
}

impl SessionAccess {
    fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptViewState {
    pub(crate) session_id: String,
    pub(crate) navigation_generation: u64,
    pub(crate) anchor: Option<crate::app::transcript::TranscriptSemanticAnchor>,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) content_width: u16,
    pub(crate) scrollable: bool,
    pub(crate) following_tail: bool,
    pub(crate) at_top: bool,
    pub(crate) at_bottom: bool,
    pub(crate) focused: bool,
    pub(crate) cursor_viewport_row: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommittedTranscriptView {
    pub(crate) revision: u64,
    pub(crate) state: TranscriptViewState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MainLayoutInputs {
    terminal_width: u16,
    terminal_height: u16,
    prompt_input_rows: u16,
    dialog: Option<crate::smelt_edit::ContainerId>,
    dialog_buffers: Vec<(crate::smelt_edit::BufId, u64)>,
}

#[cfg(all(test, feature = "transcript-bench"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptReaderMetrics {
    pub(crate) metadata_readers: usize,
    pub(crate) hydration_readers: usize,
    pub(crate) total_readers: usize,
    pub(crate) metadata_open_attempts: usize,
    pub(crate) hydration_open_attempts: usize,
    pub(crate) total_open_attempts: usize,
}

pub struct TuiApp {
    pub(crate) core: smelt_core::Core,
    pub(crate) conversation: crate::app::conversation::ConversationRuntime,
    pub(crate) lua: crate::app::lua_handlers::LuaRuntimeController,
    command_catalog: Arc<smelt_core::commands::CommandCatalog>,
    pub(crate) document_render_cache: crate::app::document::DocumentRenderCache,
    pub(crate) prompt: crate::app::prompt_runtime::PromptRuntime,
    pub(crate) overlays: crate::app::overlay_runtime::OverlayRuntime,
    pub(crate) workspace: crate::app::cwd::WorkspaceState,
    pub(crate) task_label: Option<String>,
    pub(crate) pending_quit: bool,
    pub(crate) pending_session_save: bool,
    pending_turn_dispatch: Option<crate::app::agent::PendingTurnDispatch>,
    pub(crate) paint_registry: crate::lua::paint::PaintRegistry,
    pub(crate) working: smelt_core::working::WorkingState,
    host_work: host_dispatch::HostWorkState,
    /// Resolved viewport rectangles read by mouse hit-testing and scroll estimation.
    pub(crate) layout: crate::content::layout::LayoutState,
    /// Inputs that produced the retained root layout tree.
    pub(crate) main_layout_inputs: Option<MainLayoutInputs>,
    platform: crate::app::platform_runtime::PlatformRuntime,
    transcript_hydration_worker:
        Option<crate::app::transcript_hydration::TranscriptHydrationWorker>,
    transcript_detail: crate::app::transcript_detail::TranscriptDetailRuntime,
    session_load: crate::app::session_load::SessionLoadRuntime,
    session_preview: crate::app::session_preview::SessionPreviewRuntime,
    pending_transcript_search: Option<crate::app::search::PendingTranscriptSearch>,
    frame_scheduler: crate::app::render_loop::FrameScheduler,
    transcript_work: crate::app::transcript_work::TranscriptWorkQueue,
    continuing_engine_ask_ids: std::collections::HashSet<u64>,
    pub(crate) last_width: u16,
    pub(crate) last_height: u16,
    managed_models: crate::app::managed_models::ManagedModelState,
    /// `smelt.work.busy` token stack. Non-empty → prompt top-bar
    /// indicator animates with the top token's label.
    pub(crate) busy_stack: BusyStack,
    /// API base endpoint-shape warnings already surfaced this session.
    pub(crate) api_base_normalization_warnings: HashSet<String>,
    startup_auth_error: Option<String>,
    startup_selections: smelt_core::RuntimeSelections,
    /// Trust state for `<cwd>/.smelt/`; surfaced as a startup toast then dropped.
    pub(crate) project_trust: Option<smelt_core::trust::TrustState>,
    pub(crate) app_focus: AppFocus,
    /// On-disk inputs that feed the agent's system prompt. Single
    /// home for `AGENTS.md`, the [`engine::SkillLoader`] section, and
    /// the `--system-prompt` file content; refreshed in place by
    /// `/reload`.
    pub(crate) prompt_inputs: crate::prompt_inputs::PromptInputs,
    /// Latest-desired owner for filesystem watcher setup and events.
    pub(crate) auto_reload: crate::auto_reload::AutoReloadController,
    pub(crate) ui: crate::smelt_edit::Ui,
    pub(crate) well_known: WellKnown,
    /// Timers + chord state observed and updated by event dispatch.
    pub(crate) timers: Timers,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PromptResizeDrag {
    pub(crate) chrome: &'static str,
    pub(crate) start_row: u16,
    pub(crate) start_input_rows: u16,
    pub(crate) dragged: bool,
}

#[derive(Clone, Copy, Debug)]
struct PromptResizeClick {
    row: u16,
    col: u16,
    at: Instant,
}

#[derive(Debug)]
pub(crate) struct PromptHeightState {
    rows: u16,
    manual_rows: Option<u16>,
    drag: Option<PromptResizeDrag>,
    last_click: Option<PromptResizeClick>,
}

impl Default for PromptHeightState {
    fn default() -> Self {
        Self {
            rows: 1,
            manual_rows: None,
            drag: None,
            last_click: None,
        }
    }
}

impl PromptHeightState {
    const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

    #[cfg(test)]
    pub(crate) fn rows(&self) -> u16 {
        self.rows
    }

    #[cfg(test)]
    pub(crate) fn manual_rows(&self) -> Option<u16> {
        self.manual_rows
    }

    #[cfg(test)]
    pub(crate) fn set_manual_rows(&mut self, rows: Option<u16>) {
        self.manual_rows = rows;
    }

    pub(crate) fn max_auto_rows(term_height: u16) -> u16 {
        (term_height / 2).max(1)
    }

    pub(crate) fn max_manual_rows(term_height: u16) -> u16 {
        ((term_height as u32 * 7) / 10).max(1) as u16
    }

    pub(crate) fn resolve_rows(&mut self, wrapped_rows: u16, term_height: u16) -> u16 {
        self.rows = match self.manual_rows {
            Some(rows) => rows.clamp(1, Self::max_manual_rows(term_height)),
            None => wrapped_rows.clamp(1, Self::max_auto_rows(term_height)),
        };
        self.rows
    }

    pub(crate) fn drag(&self) -> Option<PromptResizeDrag> {
        self.drag
    }

    #[cfg(test)]
    pub(crate) fn set_drag(&mut self, drag: Option<PromptResizeDrag>) {
        self.drag = drag;
    }

    pub(crate) fn active_chrome(&self) -> &'static str {
        self.drag.map(|drag| drag.chrome).unwrap_or_default()
    }

    pub(crate) fn start_drag(&mut self, chrome: &'static str, row: u16) {
        self.drag = Some(PromptResizeDrag {
            chrome,
            start_row: row,
            start_input_rows: self.rows.max(1),
            dragged: false,
        });
    }

    pub(crate) fn resize_drag_to(&mut self, row: u16, term_height: u16) -> bool {
        let Some(mut drag) = self.drag else {
            return false;
        };
        self.last_click = None;
        drag.dragged = true;
        self.drag = Some(drag);
        let delta = drag.start_row as i32 - row as i32;
        let rows = (drag.start_input_rows as i32 + delta)
            .clamp(1, Self::max_manual_rows(term_height) as i32) as u16;
        self.manual_rows = Some(rows);
        self.rows = rows;
        true
    }

    pub(crate) fn finish_drag(&mut self) -> bool {
        let Some(drag) = self.drag.take() else {
            return false;
        };
        if drag.dragged && self.manual_rows == Some(1) {
            self.reset();
        }
        true
    }

    pub(crate) fn register_click(&mut self, row: u16, col: u16, now: Instant) -> bool {
        let double_click = self.last_click.is_some_and(|last| {
            last.row == row
                && last.col == col
                && now.saturating_duration_since(last.at) <= Self::DOUBLE_CLICK_WINDOW
        });
        if double_click {
            self.reset();
            return true;
        }
        self.last_click = Some(PromptResizeClick { row, col, at: now });
        false
    }

    pub(crate) fn reset(&mut self) {
        self.manual_rows = None;
        self.drag = None;
        self.last_click = None;
    }
}

#[derive(Debug)]
pub enum AppEvent {
    ManagedModelsRefreshCompleted {
        token: smelt_core::RefreshToken,
        outcome: engine::auth::ManagedModelsRefreshOutcome,
    },
    ManagedModelsRetry {
        provider: engine::auth::AuthProvider,
        auth_revision: u64,
        desired_revision: u64,
    },
    ManagedAuthChecked {
        snapshots: Vec<(
            engine::auth::AuthProvider,
            Option<u64>,
            Vec<protocol::ModelMetadata>,
        )>,
    },
    McpStartupReady {
        busy_token: u64,
        readiness: smelt_core::mcp::McpReadiness,
    },
    TranscriptSearchCompleted(crate::app::transcript_search::TranscriptSearchWorkerResult),
    TranscriptHydrationCompleted(
        Box<crate::app::transcript_hydration::TranscriptHydrationWorkerResult>,
    ),
    SessionLoadCompleted(Box<crate::app::session_load::SessionLoadWorkerResult>),
    SessionPreviewCompleted(crate::app::session_preview::SessionPreviewWorkerResult),
    ShutdownSignal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NotificationOperation {
    CwdChange,
    SessionLoad,
    SessionPersistence(String),
    TurnStart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NotificationScope {
    Application,
    Workspace,
    Transient,
    Session(String),
    Turn,
    Operation(NotificationOperation),
}

impl NotificationScope {
    fn is_replaced_by_workspace(&self) -> bool {
        matches!(self, Self::Workspace)
    }

    fn is_replaced_by_session(&self) -> bool {
        !matches!(self, Self::Application | Self::Workspace)
    }

    fn is_replaced_by_turn(&self) -> bool {
        matches!(
            self,
            Self::Transient | Self::Turn | Self::Operation(NotificationOperation::TurnStart)
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SuspendedNotification {
    pub(crate) lifetime: SuspendedNotificationLifetime,
    pub(crate) kind: smelt_core::messages::MessageKind,
    pub(crate) summary: String,
    pub(crate) scope: NotificationScope,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SuspendedNotificationLifetime {
    Timed(Duration),
    Sticky,
}

#[derive(Clone, Debug)]
pub(crate) struct Notification {
    pub(crate) win: crate::smelt_edit::WinId,
    pub(crate) lifetime: NotificationLifetime,
    pub(crate) kind: smelt_core::messages::MessageKind,
    pub(crate) summary: String,
    pub(crate) scope: NotificationScope,
    pub(crate) rendered_width: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum NotificationLifetime {
    Timed { expires_at: Instant },
    Sticky,
}

impl NotificationLifetime {
    fn timed(now: Instant) -> Self {
        Self::Timed {
            expires_at: now + Duration::from_millis(NOTIFICATION_TTL_MS),
        }
    }

    fn is_sticky(self) -> bool {
        matches!(self, Self::Sticky)
    }

    fn is_expired(self, now: Instant) -> bool {
        match self {
            Self::Timed { expires_at } => expires_at <= now,
            Self::Sticky => false,
        }
    }

    fn expiry_delay(self, now: Instant) -> Option<Duration> {
        match self {
            Self::Timed { expires_at } => Some(expires_at.saturating_duration_since(now)),
            Self::Sticky => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptWorkState {
    Idle,
    Paused,
    BackgroundBusy,
    TurnActive,
}

impl PromptWorkState {
    pub(crate) fn is_busy(self) -> bool {
        matches!(self, Self::BackgroundBusy | Self::TurnActive)
    }

    pub(crate) fn turn_is_active(self) -> bool {
        matches!(self, Self::TurnActive)
    }
}

/// Per-window dispatch policy for a placeholder. Set via Lua's
/// `Win:placeholder(text, opts)`; the dispatcher consults it on key events.
#[derive(Default, Clone)]
pub struct PlaceholderOpts {
    pub accept_keys: Vec<crate::smelt_edit::KeyBind>,
    pub dismiss_keys: Vec<crate::smelt_edit::KeyBind>,
}

#[derive(Clone)]
pub(crate) struct PlaceholderState {
    prompt_display: Arc<Mutex<Option<String>>>,
    text: HashMap<crate::smelt_edit::WinId, String>,
    options: HashMap<crate::smelt_edit::WinId, PlaceholderOpts>,
}

impl Default for PlaceholderState {
    fn default() -> Self {
        Self {
            prompt_display: Arc::new(Mutex::new(None)),
            text: HashMap::new(),
            options: HashMap::new(),
        }
    }
}

impl PlaceholderState {
    fn prompt_display(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.prompt_display)
    }

    pub(crate) fn text(&self, win: crate::smelt_edit::WinId) -> Option<&str> {
        self.text.get(&win).map(String::as_str)
    }

    pub(crate) fn options(&self, win: crate::smelt_edit::WinId) -> Option<&PlaceholderOpts> {
        self.options.get(&win)
    }

    pub(crate) fn set_options(&mut self, win: crate::smelt_edit::WinId, options: PlaceholderOpts) {
        debug_assert!(self.text.contains_key(&win));
        self.options.insert(win, options);
    }

    pub(crate) fn set_text(&mut self, win: crate::smelt_edit::WinId, text: String) {
        debug_assert!(!text.is_empty());
        self.text.insert(win, text);
    }

    pub(crate) fn clear(&mut self, win: crate::smelt_edit::WinId) {
        self.text.remove(&win);
        self.options.remove(&win);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn contains_options(&self, win: crate::smelt_edit::WinId) -> bool {
        self.options.contains_key(&win)
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn option_windows(&self) -> impl Iterator<Item = crate::smelt_edit::WinId> + '_ {
        self.options.keys().copied()
    }

    pub(crate) fn fork_for_lua_generation(&self, ui: &crate::smelt_edit::Ui) -> Self {
        let mut candidate = self.clone();
        candidate.retain_windows(ui);
        candidate
    }

    pub(crate) fn retain_windows(&mut self, ui: &crate::smelt_edit::Ui) {
        self.text.retain(|win, _| ui.win(*win).is_some());
        self.options.retain(|win, _| ui.win(*win).is_some());
    }

    pub(crate) fn sync_prompt_display(&self) -> bool {
        let text = self.text.get(&crate::app::PROMPT_WIN).cloned();
        let mut display = self
            .prompt_display
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *display == text {
            return false;
        }
        *display = text;
        true
    }
}

#[cfg(any(test, feature = "harness"))]
pub(crate) use queue::MAX_QUEUED_MESSAGES;
pub(crate) use queue::{QueueStage, QueuedInput, QueuedTurnOptions};

pub use well_known::{PROMPT_EDIT_BUF, PROMPT_WIN, TRANSCRIPT_DOCUMENT, TRANSCRIPT_WIN};

/// Stack of live `smelt.work.busy` tokens. Each `push` returns a
/// monotonic id consumed by `release`; the prompt top-bar indicator
/// animates with the most recently pushed token's label. The `since`
/// anchor marks when the stack first became non-empty so the spinner
/// glyph can advance even when no agent turn is live.
#[derive(Default)]
pub(crate) struct BusyStack {
    state: std::rc::Rc<std::cell::RefCell<BusyStackState>>,
}

#[derive(Default)]
struct BusyStackState {
    entries: Vec<BusyStackEntry>,
    next_id: u64,
    since: Option<Instant>,
}

struct BusyStackEntry {
    id: u64,
    label: String,
    context_recalculating: bool,
}

#[derive(Clone)]
pub(crate) struct BusyToken {
    state: std::rc::Weak<std::cell::RefCell<BusyStackState>>,
    id: u64,
}

impl BusyToken {
    pub(crate) fn is_active(&self) -> bool {
        self.state.upgrade().is_some_and(|state| {
            state
                .borrow()
                .entries
                .iter()
                .any(|entry| entry.id == self.id)
        })
    }

    pub(crate) fn release(&self) -> bool {
        self.state
            .upgrade()
            .is_some_and(|state| release_busy_entry(&mut state.borrow_mut(), self.id))
    }
}

pub use smelt_core::signals::WorkBusyEntry;

impl BusyStack {
    pub(crate) fn push(&mut self, label: String) -> u64 {
        self.push_entry(label, false).0
    }

    pub(crate) fn push_token(&mut self, label: String) -> BusyToken {
        self.push_token_with_context_state(label, false)
    }

    pub(crate) fn push_context_recalculation_token(&mut self, label: String) -> BusyToken {
        self.push_token_with_context_state(label, true)
    }

    fn push_token_with_context_state(
        &mut self,
        label: String,
        context_recalculating: bool,
    ) -> BusyToken {
        let (id, state) = self.push_entry(label, context_recalculating);
        BusyToken {
            state: std::rc::Rc::downgrade(&state),
            id,
        }
    }

    fn push_entry(
        &mut self,
        label: String,
        context_recalculating: bool,
    ) -> (u64, std::rc::Rc<std::cell::RefCell<BusyStackState>>) {
        let mut state = self.state.borrow_mut();
        state.next_id += 1;
        let id = state.next_id;
        if state.entries.is_empty() {
            state.since = Some(Instant::now());
        }
        state.entries.push(BusyStackEntry {
            id,
            label,
            context_recalculating,
        });
        drop(state);
        (id, std::rc::Rc::clone(&self.state))
    }

    /// Drop the entry with `id`. Returns `true` if an entry was removed.
    pub(crate) fn release(&mut self, id: u64) -> bool {
        release_busy_entry(&mut self.state.borrow_mut(), id)
    }

    pub(crate) fn is_busy(&self) -> bool {
        !self.state.borrow().entries.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        let mut state = self.state.borrow_mut();
        state.entries.clear();
        state.since = None;
    }

    pub(crate) fn top_label(&self) -> Option<String> {
        self.state
            .borrow()
            .entries
            .last()
            .map(|entry| entry.label.clone())
    }

    pub(crate) fn context_recalculating(&self) -> bool {
        self.state
            .borrow()
            .entries
            .iter()
            .any(|entry| entry.context_recalculating)
    }

    /// Elapsed time since the first token was pushed, or `None` when empty.
    pub(crate) fn elapsed(&self) -> Option<std::time::Duration> {
        self.state.borrow().since.map(|instant| instant.elapsed())
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn since(&self) -> Option<Instant> {
        self.state.borrow().since
    }

    /// Full stack newest-last, projected onto `WorkBusyEntry`. Cheap
    /// clone of the per-entry `(id, label)` pair; called once per tick
    /// by the cell publisher.
    pub(crate) fn entries_snapshot(&self) -> Vec<WorkBusyEntry> {
        self.state
            .borrow()
            .entries
            .iter()
            .map(|entry| WorkBusyEntry {
                id: entry.id,
                label: entry.label.clone(),
            })
            .collect()
    }
}

fn release_busy_entry(state: &mut BusyStackState, id: u64) -> bool {
    if let Some(position) = state.entries.iter().position(|entry| entry.id == id) {
        state.entries.remove(position);
        if state.entries.is_empty() {
            state.since = None;
        }
        true
    } else {
        false
    }
}

/// Well-known stable `WinId`s for the always-present split-tree windows.
/// Prompt input and the transcript stay engine-owned; the prompt bars and
/// statusline are Lua-allocated by `runtime/lua/smelt/prompt_bar.lua` and
/// `runtime/lua/smelt/statusline.lua`.
pub(crate) struct WellKnown {
    pub(crate) prompt: crate::smelt_edit::WinId,
    pub(crate) transcript: crate::smelt_edit::WinId,
    pub(crate) cmdline: Option<crate::smelt_edit::WinId>,
}

/// Which pane currently holds focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppFocus {
    Prompt,
    Content,
}

pub(crate) struct TurnState {
    pub(crate) turn_id: u64,
    pub(crate) canonical: bool,
    pub(crate) pending: Vec<PendingTool>,
    pub(crate) permissions: std::sync::Arc<smelt_core::permissions::Permissions>,
    pub(crate) submitted_history_idx: usize,
    pub(crate) rewind_history_idx: Option<usize>,
    pub(crate) assistant_output_started: bool,
    pub(crate) _perf: Option<smelt_perf::perf::Guard>,
}

pub(crate) enum EventOutcome {
    Noop,
    Redraw,
    Quit,
    CancelAgent,
    /// Cancel the running agent and immediately start a new turn with the next queued item.
    InterruptWithQueued,
    /// Start a new turn without adding prompt text.
    ContinueTurn,
    Submit {
        content: Content,
        display: String,
        edit: SubmitEdit,
    },
    Exec(crate::commands::ExecHandle),
}

pub(crate) enum CommandAction {
    Continue,
    Exec(crate::commands::ExecHandle),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShellPanel {
    pub(crate) overlay: crate::smelt_edit::OverlayId,
    pub(crate) win: crate::smelt_edit::WinId,
    pub(crate) buf: crate::smelt_edit::BufId,
}

pub(crate) enum InputOutcome {
    Continue,
    StartAgent,
    Command(String),
}

/// Mutable timer state shared across event handlers.
pub(crate) struct Timers {
    pub(crate) last_ctrlc: Option<Instant>,
    pub(crate) last_keypress: Option<Instant>,
    /// Pending `Ctrl-W` pane chord; next key navigates panes instead of flowing to input.
    pub(crate) pending_pane_chord: Option<Instant>,
    /// Pending transcript `z` fold chord while content pane is focused.
    /// This is a content-viewer chord because global Lua keymaps intentionally
    /// run before focus-specific transcript dispatch.
    pub(crate) pending_transcript_fold_chord: Option<Instant>,
    /// Active Lua-keymap chord sequence; `None` between chords.
    pub(crate) pending_chord: Option<PendingChord>,
}

/// State carried between keys of a multi-key chord. See [`Timers::pending_chord`].
pub(crate) struct PendingChord {
    pub(crate) tokens: Vec<String>,
    /// Vim mode captured before the first key was dispatched; surfaced to chord handlers.
    pub(crate) vim_mode_at_start: Option<crate::smelt_edit::VimMode>,
    pub(crate) policy: PendingChordPolicy,
}

#[derive(Clone, Copy)]
pub(crate) enum PendingChordPolicy {
    Sticky,
    Timed { expires_at: Instant },
}

impl PendingChordPolicy {
    fn expires_at(self) -> Option<Instant> {
        match self {
            Self::Sticky => None,
            Self::Timed { expires_at } => Some(expires_at),
        }
    }
}

/// Idle time after which a pending Esc-led Lua keymap chord expires.
pub(crate) const ESC_CHORD_TIMEOUT_MS: u64 = 500;

/// Idle time after the last keypress before showing a deferred permission dialog.
pub(crate) const CONFIRM_DEFER_MS: u64 = 1500;

/// How long a timed notification toast stays visible without user interaction.
pub(crate) const NOTIFICATION_TTL_MS: u64 = 5000;

pub(crate) enum DeferredDialog {
    Confirm(Box<ConfirmRequest>),
}

#[derive(Default)]
pub(crate) struct DeferredDialogs {
    queue: VecDeque<DeferredDialog>,
}

impl DeferredDialogs {
    pub(crate) fn defer_confirm(&mut self, request: Box<ConfirmRequest>) {
        self.queue.push_back(DeferredDialog::Confirm(request));
    }

    pub(crate) fn pop(&mut self) -> Option<DeferredDialog> {
        self.queue.pop_front()
    }

    pub(crate) fn clear(&mut self) {
        self.queue.clear();
    }

    pub(crate) fn is_pending(&self) -> bool {
        !self.queue.is_empty()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }
}

pub(crate) enum SessionControl {
    Continue,
    NeedsConfirm(Box<ConfirmRequest>),
    Done,
    Error {
        kind: Option<protocol::EngineAskErrorKind>,
        retry_at_ms: Option<u64>,
    },
}

/// How the active turn is ending. Drives whether queued inputs are preserved
/// and whether the next queued turn is auto-started.
#[derive(Clone, Copy)]
pub(crate) enum TurnEnd {
    /// Clean completion: queue may chain into the next turn.
    Complete,
    /// User cancelled: queue is drained back to the prompt.
    Cancelled,
    /// Provider/engine error: queue is preserved so the user can retry.
    Errored {
        kind: Option<protocol::EngineAskErrorKind>,
        retry_at_ms: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandTurnStart {
    Fresh,
    ContinueFromLast,
}

pub(crate) struct PendingTool {
    pub(crate) invocation_id: protocol::InvocationId,
    pub(crate) name: String,
}

#[derive(Clone)]
pub(crate) struct PendingHistoryAppend {
    item: protocol::HistoryItem,
    coalescing_note_kind: Option<protocol::HistoryNoteKind>,
    context_name: Option<String>,
    clear_context: bool,
    delivery: PendingHistoryDelivery,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingHistoryLifecycle {
    TurnScoped,
    SessionScoped,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingHistoryDelivery {
    HistoryOnly,
    FollowUpIfUnconsumed,
}

impl PendingHistoryAppend {
    pub(crate) fn mode_change(mode: String, text: String) -> Self {
        Self {
            item: protocol::HistoryItem::note(protocol::HistoryNote::mode_change_for_mode(
                mode, text,
            )),
            coalescing_note_kind: Some(protocol::HistoryNoteKind::ModeChange),
            context_name: None,
            clear_context: false,
            delivery: PendingHistoryDelivery::HistoryOnly,
        }
    }

    pub(crate) fn context(name: String, text: String) -> Self {
        Self {
            item: protocol::HistoryItem::note(protocol::HistoryNote::named_context(
                name.clone(),
                text,
            )),
            coalescing_note_kind: Some(protocol::HistoryNoteKind::Context),
            context_name: Some(name),
            clear_context: false,
            delivery: PendingHistoryDelivery::HistoryOnly,
        }
    }

    pub(crate) fn clear_context(name: String) -> Self {
        Self {
            item: protocol::HistoryItem::note(protocol::HistoryNote::named_context(
                name.clone(),
                String::new(),
            )),
            coalescing_note_kind: Some(protocol::HistoryNoteKind::Context),
            context_name: Some(name),
            clear_context: true,
            delivery: PendingHistoryDelivery::HistoryOnly,
        }
    }

    pub(crate) fn process_status(note: protocol::HistoryNote) -> Self {
        Self {
            item: protocol::HistoryItem::note(note),
            coalescing_note_kind: None,
            context_name: None,
            clear_context: false,
            delivery: PendingHistoryDelivery::FollowUpIfUnconsumed,
        }
    }

    #[cfg(test)]
    pub(crate) fn history_item(&self) -> protocol::HistoryItem {
        self.item.clone()
    }

    pub(crate) fn transcript_block(&self, lua: &crate::lua::LuaRuntime) -> Option<Block> {
        crate::app::history::history_note_to_block(
            lua,
            self.item
                .as_note()
                .expect("pending history appends are notes"),
        )
    }

    pub(crate) fn coalescing_note_kind(&self) -> Option<protocol::HistoryNoteKind> {
        self.coalescing_note_kind
    }

    pub(crate) fn context_name(&self) -> Option<&str> {
        self.context_name.as_deref()
    }

    fn same_coalescing_target(&self, other: &Self) -> bool {
        match (&self.context_name, &other.context_name) {
            (Some(a), Some(b)) => a == b,
            (Some(_), None) | (None, Some(_)) => false,
            (None, None) => {
                self.coalescing_note_kind.is_some()
                    && self.coalescing_note_kind == other.coalescing_note_kind
            }
        }
    }

    pub(crate) fn mode(&self) -> Option<&str> {
        self.item.as_note().and_then(protocol::HistoryNote::mode)
    }

    pub(crate) fn lifecycle(&self) -> PendingHistoryLifecycle {
        if self.coalescing_note_kind == Some(protocol::HistoryNoteKind::ModeChange) {
            PendingHistoryLifecycle::SessionScoped
        } else {
            PendingHistoryLifecycle::TurnScoped
        }
    }

    pub(crate) fn delivery(&self) -> PendingHistoryDelivery {
        self.delivery
    }

    pub(crate) fn history_append(
        &self,
        mode_base: Option<protocol::AgentMode>,
    ) -> protocol::HistoryAppend {
        match self.coalescing_note_kind {
            Some(protocol::HistoryNoteKind::ModeChange) => {
                let note = self.item.as_note().expect("mode history appends are notes");
                let mode = note.mode().expect("mode history appends require a mode");
                let base = note
                    .base_mode()
                    .and_then(protocol::AgentMode::parse)
                    .or(mode_base)
                    .expect("mode history appends require a base mode");
                let item =
                    protocol::HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                        base.as_str(),
                        mode,
                        note.text(),
                    ));
                protocol::HistoryAppend::mode_change(item, base)
            }
            Some(protocol::HistoryNoteKind::Context) => {
                let name = self
                    .context_name
                    .clone()
                    .unwrap_or_else(|| protocol::DEFAULT_CONTEXT_NOTE_NAME.to_string());
                if self.clear_context {
                    protocol::HistoryAppend::clear_context(name)
                } else {
                    protocol::HistoryAppend::set_context(self.item.clone(), name)
                }
            }
            Some(kind) => protocol::HistoryAppend::replace_note_kind(self.item.clone(), kind),
            None => protocol::HistoryAppend::append(self.item.clone()),
        }
    }

    pub(crate) fn matches_history_item(&self, item: &protocol::HistoryItem) -> bool {
        if &self.item == item {
            return true;
        }
        let Some(expected) = self.item.as_note() else {
            return false;
        };
        let Some(actual) = item.as_note() else {
            return false;
        };
        expected.kind() == actual.kind()
            && expected.context_name() == actual.context_name()
            && expected.text() == actual.text()
    }
}

impl TuiApp {
    #[cfg(all(test, feature = "transcript-bench"))]
    pub(crate) fn transcript_reader_metrics_for_harness(&self) -> TranscriptReaderMetrics {
        let transcript = self.conversation.transcript();
        let metadata_readers = transcript.retained_store_reader_count_for_harness();
        let metadata_open_attempts = transcript.store_open_attempt_count_for_harness();
        let hydration_readers = self
            .transcript_hydration_worker
            .as_ref()
            .map_or(0, |worker| worker.retained_reader_count());
        let hydration_open_attempts = self
            .transcript_hydration_worker
            .as_ref()
            .map_or(0, |worker| worker.open_attempt_count());
        TranscriptReaderMetrics {
            metadata_readers,
            hydration_readers,
            total_readers: metadata_readers.saturating_add(hydration_readers),
            metadata_open_attempts,
            hydration_open_attempts,
            total_open_attempts: metadata_open_attempts.saturating_add(hydration_open_attempts),
        }
    }

    pub(crate) fn active_context_token_identity(
        &self,
    ) -> smelt_core::session::ContextTokenIdentity {
        let active = self.core.config.active_model();
        smelt_core::session::ContextTokenIdentity {
            model: active.map(|model| model.model_name.clone()),
            api_base: active.map(|model| model.api_base.clone()),
            provider_type: active.map(|model| model.provider_type.clone()),
        }
    }

    pub(crate) fn active_provider_supports_mid_turn_reasoning_changes(&self) -> bool {
        self.core.config.active_model().is_some_and(|model| {
            smelt_provider::ProviderKind::from_config_and_url(&model.provider_type, &model.api_base)
                .supports_mid_turn_reasoning_changes()
        })
    }

    pub(crate) fn reasoning_effort_pending(&self) -> bool {
        self.conversation.applied_reasoning_effort() != &self.core.config.reasoning_effort
    }

    pub(crate) fn mode_pending(&self) -> bool {
        self.active_agent_turn_id().is_some()
            && self.conversation.applied_mode() != &self.core.config.mode
    }

    pub(crate) fn sync_agent_mode_applied(&mut self) {
        self.conversation
            .set_applied_mode(self.core.config.mode.clone());
    }

    pub(crate) fn sync_reasoning_effort_applied(&mut self) {
        self.conversation
            .set_applied_reasoning_effort(self.core.config.reasoning_effort.clone());
    }

    pub(crate) fn active_agent_turn_id(&self) -> Option<u64> {
        self.conversation.active_id()
    }

    pub(crate) fn agent_is_running(&self) -> bool {
        self.active_agent_turn_id().is_some()
    }

    pub(crate) fn turn_lifecycle_is_active(&self) -> bool {
        self.agent_is_running()
            || self.turn_submission_is_pending()
            || self.conversation.canonical_operations_are_pending()
    }

    pub(crate) fn request_urgent_render(&mut self) {
        self.frame_scheduler
            .request(crate::app::render_loop::FrameUrgency::Urgent);
    }

    pub(crate) fn request_continuation_render(&mut self) {
        self.frame_scheduler
            .request(crate::app::render_loop::FrameUrgency::Continuation);
    }

    pub(crate) fn request_animation_render(&mut self, interval: Duration) {
        self.frame_scheduler
            .request(crate::app::render_loop::FrameUrgency::Animation(interval));
    }

    pub(crate) fn scheduled_frame_delay(&self) -> Option<Duration> {
        self.frame_scheduler
            .next_delay(self.core.clock.instant_now())
    }

    pub(crate) fn scheduled_frame_is_due(&self) -> bool {
        self.frame_scheduler.is_due(self.core.clock.instant_now())
    }

    pub(crate) fn prompt_work_state(&self) -> PromptWorkState {
        let turn_active = self.turn_lifecycle_is_active() || self.working.is_compacting();
        if turn_active {
            PromptWorkState::TurnActive
        } else if self.busy_stack.is_busy() {
            PromptWorkState::BackgroundBusy
        } else if self
            .conversation
            .turn_pause()
            .is_some_and(|pause| pause.kind != Some(protocol::EngineAskErrorKind::Cancelled))
        {
            PromptWorkState::Paused
        } else {
            PromptWorkState::Idle
        }
    }

    pub(crate) fn turn_input_is_active(&self) -> bool {
        self.prompt_work_state().turn_is_active()
    }

    pub(crate) fn prompt_input_is_busy(&self) -> bool {
        self.prompt_work_state().is_busy()
    }

    fn visible_session_history(&self) -> Result<bool, String> {
        if let Some(live) = self.conversation.live_session() {
            return live.any_transcript_visible_before(live.history_len());
        }
        Ok(self
            .conversation
            .session()
            .history
            .iter()
            .any(protocol::HistoryItem::is_transcript_visible))
    }

    pub(crate) fn has_visible_session_history(&mut self) -> bool {
        match self.visible_session_history() {
            Ok(visible) => visible,
            Err(err) => {
                smelt_perf::perf::record_value("live_session:visible_scan_error", 1);
                self.notify_session_error_sticky(format!("failed to read session history: {err}"));
                false
            }
        }
    }

    pub(crate) fn ephemeral(&self) -> bool {
        self.conversation.is_ephemeral()
    }

    pub(crate) fn current_artifact_dir(&self) -> std::path::PathBuf {
        self.conversation.current_artifact_dir()
    }

    pub fn shutdown_context(&self) -> ShutdownContext {
        self.conversation.shutdown_context()
    }

    pub(crate) fn publish_shared_session_state(&self) {
        self.conversation.publish_shared_state();
    }

    pub(crate) fn can_continue_turn(&mut self) -> bool {
        self.has_visible_session_history()
    }

    pub(crate) fn queue_input_for_request(&mut self, queued: QueuedInput) -> bool {
        if self.conversation.turn_pause().is_some() {
            return self.prompt.try_queue_request(queued);
        }
        if !self.turn_input_is_active() {
            return self.prompt.try_queue_turn(queued);
        }
        let input = queued.steer_input();
        if !self.prompt.try_queue_request(queued) {
            return false;
        }
        if let Some(input) = input.filter(|input| !input.provider_content().is_empty()) {
            self.core.engine.send(protocol::UiCommand::Steer { input });
        }
        true
    }

    pub(crate) fn drain_queued_inputs_into_prompt(&mut self) {
        let (request_count, queued) = self.prompt.drain_for_prompt();
        if request_count > 0 {
            self.core.engine.send(protocol::UiCommand::Unsteer {
                count: request_count,
            });
        }
        if queued.is_empty() {
            return;
        }

        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let mut prefix = queued
            .iter()
            .map(QueuedInput::prompt_replay_text)
            .collect::<Vec<_>>()
            .join("\n");
        if !prefix.is_empty() && !pctx.buf.source().is_empty() {
            prefix.push('\n');
        }
        self.prompt.prepend_text(&mut pctx, prefix);
    }

    pub(crate) fn clear_prompt_prediction(&mut self) {
        self.clear_placeholder(self.well_known.prompt);
    }

    pub(crate) fn invalidate_prompt_prediction(&mut self) {
        self.clear_prompt_prediction();
        self.bump_epoch("input_epoch");
    }

    pub(crate) fn publish_input_submit(&mut self, submitted: String) {
        self.invalidate_prompt_prediction();
        if !submitted.is_empty() {
            self.core
                .signals
                .emit_dyn("input_submit", std::rc::Rc::new(submitted));
        }
        self.pump_lua();
    }

    pub(crate) fn bump_epoch(&mut self, name: &str) {
        let next = self
            .core
            .signals
            .get::<u64>(name)
            .unwrap_or_default()
            .wrapping_add(1);
        self.core.signals.set_dyn(name, std::rc::Rc::new(next));
    }

    pub(crate) fn mode_history_base(&self) -> Result<protocol::AgentMode, String> {
        let len = self.session_history_len();
        let last_is_mode_change = if let Some(live) = self.conversation.live_session() {
            live.history_range(len.saturating_sub(1)..len)?
                .last()
                .is_some_and(|item| {
                    item.note_kind() == Some(protocol::HistoryNoteKind::ModeChange)
                        && item
                            .as_note()
                            .and_then(protocol::HistoryNote::mode)
                            .is_some()
                })
        } else {
            self.conversation
                .session()
                .history
                .last()
                .is_some_and(|item| {
                    item.note_kind() == Some(protocol::HistoryNoteKind::ModeChange)
                        && item
                            .as_note()
                            .and_then(protocol::HistoryNote::mode)
                            .is_some()
                })
        };
        let end = if last_is_mode_change {
            len.saturating_sub(1)
        } else {
            len
        };
        self.mode_at_history_boundary(end)
    }

    pub(crate) fn mode_at_history_boundary(
        &self,
        hist_idx: usize,
    ) -> Result<protocol::AgentMode, String> {
        let fallback = self
            .conversation
            .session()
            .mode
            .as_deref()
            .unwrap_or("normal");
        let mode = if let Some(live) = self.conversation.live_session() {
            live.effective_mode_at(hist_idx, fallback)?
        } else {
            protocol::effective_mode_at(&self.conversation.session().history, hist_idx, fallback)
                .to_string()
        };
        Ok(protocol::AgentMode::parse(&mode).unwrap_or_else(protocol::AgentMode::normal))
    }

    pub(crate) fn current_context_note_text(&self) -> String {
        self.workspace.context_note(std::path::Path::new(
            &self.core.config.settings.worktree_root,
        ))
    }

    fn latest_context_note_text(&self, name: &str) -> Option<&str> {
        if let Some(text) = self.conversation.pending_context_note(name) {
            return text;
        }
        self.conversation
            .session()
            .history
            .iter()
            .rev()
            .filter_map(protocol::HistoryItem::as_note)
            .find(|note| note.context_name() == Some(name))
            .map(protocol::HistoryNote::text)
    }

    pub(crate) fn ensure_current_context_note(&mut self) {
        let text = self.current_context_note_text();
        if self.latest_context_note_text(protocol::DEFAULT_CONTEXT_NOTE_NAME) == Some(text.as_str())
        {
            return;
        }
        self.set_context_note(protocol::DEFAULT_CONTEXT_NOTE_NAME.to_string(), Some(text));
    }

    pub(crate) fn set_context_note(&mut self, name: String, text: Option<String>) {
        let append = match text {
            Some(text) => PendingHistoryAppend::context(name, text),
            None => PendingHistoryAppend::clear_context(name),
        };
        let has_visible_history = if self.agent_is_running() {
            true
        } else {
            match self.visible_session_history() {
                Ok(visible) => visible,
                Err(err) => {
                    smelt_perf::perf::record_value("live_session:visible_scan_error", 1);
                    self.notify_session_error_sticky(format!(
                        "failed to read session history: {err}"
                    ));
                    return;
                }
            }
        };
        if has_visible_history {
            self.queue_history_append(append);
        } else if let Err(err) = self.conversation.replace_or_push_history_append(append) {
            smelt_perf::perf::record_value("live_session:history_append_plan_error", 1);
            self.notify_session_error_sticky(format!("failed to update session context: {err}"));
        }
    }

    pub(crate) fn mode_append_base(&self) -> Result<protocol::AgentMode, String> {
        if self.agent_is_running() {
            Ok(self.conversation.applied_mode().clone())
        } else {
            self.mode_history_base()
        }
    }

    pub(crate) fn queue_history_append(&mut self, append: PendingHistoryAppend) {
        let mode_base = match append.mode() {
            Some(_) => match self.mode_append_base() {
                Ok(mode) => Some(mode),
                Err(err) => {
                    smelt_perf::perf::record_value("live_session:mode_scan_error", 1);
                    self.notify_session_error_sticky(format!("failed to read session mode: {err}"));
                    return;
                }
            },
            None => None,
        };
        let history_append = append.history_append(mode_base);
        let coalescing_note_kind = history_append.coalescing_note_kind();

        if self.agent_is_running() {
            let pending_append =
                if coalescing_note_kind == Some(protocol::HistoryNoteKind::ModeChange) {
                    PendingHistoryAppend {
                        item: history_append.item.clone(),
                        coalescing_note_kind: append.coalescing_note_kind,
                        context_name: None,
                        clear_context: false,
                        delivery: append.delivery,
                    }
                } else {
                    append.clone()
                };
            if let Err(err) = self.conversation.queue_history_append(
                pending_append,
                match &history_append.policy {
                    protocol::HistoryAppendPolicy::ModeChange { base } => Some(base),
                    _ => None,
                },
            ) {
                smelt_perf::perf::record_value("live_session:history_append_plan_error", 1);
                self.notify_session_error_sticky(format!("failed to queue session history: {err}"));
                return;
            }
            self.core
                .engine
                .send(protocol::UiCommand::AppendHistoryItem {
                    append: history_append,
                });
            return;
        }

        match self.visible_session_history() {
            Ok(true) => {
                let result = self.apply_history_append_to_history(&history_append);
                if let Some(block) = append.transcript_block(&self.lua) {
                    self.commit_history_append_block(block, coalescing_note_kind, result);
                }
            }
            Ok(false) if coalescing_note_kind.is_some() => {
                self.conversation.remove_matching_history_append(&append);
            }
            Ok(false) => {}
            Err(err) => {
                smelt_perf::perf::record_value("live_session:visible_scan_error", 1);
                self.notify_session_error_sticky(format!("failed to read session history: {err}"));
            }
        }
    }

    pub(crate) fn run_queued_command_line(&mut self, line: &str, sent_at_ms: u64) {
        crate::commands::run_command_with_context(
            self,
            line,
            crate::commands::CommandContext::prompt().with_sent_at_ms(sent_at_ms),
        );
    }

    pub(crate) fn start_queued_input(&mut self, queued: QueuedInput) -> Result<(), QueuedInput> {
        self.clear_prompt_prediction();
        let retry = queued.clone();
        let started = match queued {
            QueuedInput::Request(req) => {
                let req = *req;
                match req.turn_options {
                    QueuedTurnOptions::CustomCommand { overrides } => {
                        let text = req.content.text_content().into_owned();
                        let turn = self.begin_command_request_turn(
                            req.display,
                            text,
                            *overrides,
                            CommandTurnStart::Fresh,
                            req.sent_at_ms,
                        );
                        let started = turn.is_some() || self.turn_submission_is_pending();
                        self.conversation.set_active(turn);
                        started
                    }
                    QueuedTurnOptions::Default if !req.content.is_empty() => {
                        let turn = self.begin_agent_turn(&req.display, req.content, req.sent_at_ms);
                        let started = turn.is_some() || self.turn_submission_is_pending();
                        self.conversation.set_active(turn);
                        started
                    }
                    QueuedTurnOptions::Default => true,
                }
            }
            QueuedInput::Command {
                line, sent_at_ms, ..
            } => {
                self.run_queued_command_line(&line, sent_at_ms);
                true
            }
            QueuedInput::ProcessStatus(note) if !note.text().is_empty() => {
                let turn = self.begin_process_status_turn(note);
                let started = turn.is_some() || self.turn_submission_is_pending();
                self.conversation.set_active(turn);
                started
            }
            QueuedInput::ProcessStatus(_) => true,
        };
        if started {
            Ok(())
        } else {
            Err(retry)
        }
    }

    pub(crate) fn start_next_queued_input_if_idle(&mut self) -> bool {
        if self.prompt_input_is_busy()
            || self.conversation.turn_pause().is_some()
            || self.prompt.queue_is_empty()
        {
            return false;
        }
        let Some((stage, queued)) = self.prompt.pop_next_for_turn() else {
            return false;
        };
        let was_animating = self.working.is_animating();
        if let Err(queued) = self.start_queued_input(queued) {
            self.prompt.queue_front(stage, queued);
        }
        if was_animating && !self.conversation.is_active() {
            self.working.finish(smelt_core::working::TurnOutcome::Done);
        }
        true
    }

    pub(crate) fn apply_context_window_update(&mut self, update: ContextWindowUpdate) {
        if !self.platform.accept_context_window_update(&update) {
            return;
        }
        if self.core.config.context_window != update.value {
            self.core.config.revision = self.core.config.revision.wrapping_add(1);
            self.core.config.context_window = update.value;
        }
    }

    #[cfg(test)]
    fn prepare_context_window_for_test(&mut self, target: ContextWindowTarget) -> Option<u64> {
        self.platform.prepare_context_window_for_test(target)
    }

    pub(crate) fn model_status_snapshot(&self) -> ModelStatusSnapshot {
        let selection = &self.core.config.model_selection;
        let (availability, reason) =
            match selection.active.as_ref().map(|model| &model.availability) {
                Some(smelt_core::ModelAvailability::Available) => ("available", None),
                Some(smelt_core::ModelAvailability::StaleCatalog) => ("stale_catalog", None),
                Some(smelt_core::ModelAvailability::Unavailable { reason }) => {
                    ("unavailable", Some(reason.status_reason().to_string()))
                }
                None if selection.requested_key.is_some() => ("pending", None),
                None => ("none", None),
            };
        let providers = smelt_core::ManagedModels::provider_kinds()
            .into_iter()
            .map(|provider| {
                let state = self.managed_models.provider(provider);
                ManagedProviderStatusSnapshot {
                    name: provider.provider_type().replace('-', "_"),
                    authenticated: state.authenticated,
                    status: state.status.as_str(),
                    error: state.last_error.clone(),
                    request_id: state.in_flight_request_id(),
                    auth_revision: state.auth_revision,
                    desired_revision: state.desired_revision,
                }
            })
            .collect();
        ModelStatusSnapshot {
            current: selection.active.as_ref().map(|model| model.key.clone()),
            requested: selection.requested_key.clone(),
            availability,
            reason,
            providers,
        }
    }

    pub fn runtime_controller_status(&self) -> RuntimeControllerStatus {
        let (auto_desired, auto_observed, auto_error) = self.auto_reload.status();
        RuntimeControllerStatus {
            mcp: self
                .core
                .mcp
                .as_ref()
                .map(|manager| manager.controller_status()),
            lsp: self.lua.shared().lsp.controller_status(),
            auto_reload: ControllerRevisionStatus {
                desired_revision: auto_desired,
                observed_revision: auto_observed,
                error: auto_error,
            },
            context_window: self.platform.context_window_status(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: smelt_core::RuntimeState,
        startup_overrides: smelt_core::StartupOverrides,
        engine: EngineHandle,
        permissions: smelt_core::permissions::PermissionsHandle,
        shared_session: Arc<Mutex<Option<SharedSessionState>>>,
        lua: crate::lua::LuaRuntime,
        project_trust: smelt_core::trust::TrustState,
        clock: Arc<dyn engine::clock::Clock>,
        env: Arc<engine::env::RuntimeEnv>,
        options: TuiAppOptions,
    ) -> Self {
        lua.core_shared().set_clock(Arc::clone(&clock));
        let TuiAppOptions {
            startup_auth_error,
            startup_selections,
            app_events,
            managed_models,
            skills,
            mcp,
            prompt_inputs,
            session_persistence,
        } = options;
        let managed_models = managed_models.unwrap_or_else(smelt_core::ManagedModels::empty);
        let command_catalog = Arc::new(smelt_core::commands::CommandCatalog::new(
            lua.command_names_handle(),
        ));
        let input = PromptState::new_for_runtime(env.cwd(), env.runtime_dir().to_path_buf());
        let vim_enabled = config.settings.vim;

        let cwd = env.cwd().to_string_lossy().into_owned();
        let workspace = crate::app::cwd::WorkspaceState::new(
            cwd.clone(),
            env.home().clone(),
            std::path::Path::new(&config.settings.worktree_root),
        );

        let runtime_state = config;

        let (term_w, term_h) = terminal::size().unwrap_or((80, 24));
        let placeholder_state = PlaceholderState::default();
        let prompt_placeholder_display = placeholder_state.prompt_display();
        let (ui, well_known) = {
            let mut ui = crate::smelt_edit::Ui::new();
            ui.set_terminal_size(term_w, term_h);
            // Install the baked default theme up-front. `run()` may
            // overwrite it after detecting light/dark, and Lua's
            // `theme.use(...)` will later swap to the user's
            // colorscheme - but until then every render path sees a
            // working theme. Tests that construct TuiApp without
            // calling run() rely on this.
            *ui.theme_mut() = crate::theme::default_baked().as_ref().clone();
            let input_display_buf = ui
                .buf_create_with_id(
                    crate::app::PROMPT_EDIT_BUF,
                    crate::smelt_edit::BufCreateOpts::default(),
                )
                .expect("PROMPT_EDIT_BUF slot is free");
            let parser: std::sync::Arc<dyn crate::smelt_edit::BufferParser> = std::sync::Arc::new(
                crate::content::prompt_parser::PromptBufferParser::with_placeholder(
                    input.store.clone(),
                    prompt_placeholder_display.clone(),
                    Arc::clone(&command_catalog),
                ),
            );
            let copier: std::sync::Arc<dyn crate::smelt_edit::BufferCopy> = std::sync::Arc::new(
                crate::content::prompt_parser::PromptCopier::new(input.store.clone()),
            );
            if let Some(b) = ui.buf_mut(input_display_buf) {
                b.set_parser(parser);
                b.set_copier(copier);
                b.history = crate::smelt_edit::UndoHistory::new(Some(100));
            }
            let transcript_display_buf = ui.buf_create(crate::smelt_edit::BufCreateOpts::default());
            let transcript_copier: std::sync::Arc<dyn crate::smelt_edit::BufferCopy> =
                std::sync::Arc::new(crate::content::transcript_buf::TranscriptCopier);
            if let Some(b) = ui.buf_mut(transcript_display_buf) {
                b.readonly = true;
                b.set_copier(transcript_copier);
            }
            assert!(ui.win_open_split_at(
                crate::app::TRANSCRIPT_WIN,
                transcript_display_buf,
                crate::smelt_edit::SplitConfig {
                    region: "transcript".into(),
                    gutters: crate::smelt_edit::Gutters {
                        pad_left: 0,
                        pad_right: 0,
                        scrollbar: true,
                    },
                },
            ));
            ui.set_window_host_document(
                crate::app::TRANSCRIPT_WIN,
                Some(crate::app::TRANSCRIPT_DOCUMENT),
            );
            if let Some(w) = ui.win_mut(crate::app::TRANSCRIPT_WIN) {
                w.set_surface(crate::smelt_edit::WindowSurface::readonly_text());
                w.set_vim_enabled(vim_enabled);
                // Transcript blocks (code, diff) stamp `SourceLine` per row;
                // `LineNumberGutter` is strict - text/markdown rows leave no
                // stamp and contribute no gutter width.
                w.gutter = Some(std::sync::Arc::new(
                    crate::smelt_edit::gutter::LineNumberGutter::new(),
                ));
                // Opt the transcript into per-frame tail-follow. Plugin leaves
                // stay pinned unless a caller explicitly requests tail mode.
                w.follow_tail();
            }
            assert!(ui.win_open_split_at(
                crate::app::PROMPT_WIN,
                input_display_buf,
                crate::smelt_edit::SplitConfig {
                    region: "prompt".into(),
                    gutters: crate::smelt_edit::Gutters {
                        // The reserved scrollbar column doubles as the right gutter
                        // when content fits, so `pad_right` stays 0 to avoid a
                        // double-wide gap. When content overflows, the scrollbar
                        // paints in that column and `pad_left` keeps the input
                        // off the left edge.
                        pad_left: 1,
                        pad_right: 0,
                        ..Default::default()
                    },
                },
            ));
            if let Some(w) = ui.win_mut(crate::app::PROMPT_WIN) {
                w.set_vim_enabled(vim_enabled);
                w.wrap = true;
                w.wrap_cursor_padding = true;
                // Chat input ergonomics: the prompt is for typing, so even with vim
                // enabled the first keystroke after startup should insert, not act
                // as a Normal-mode motion. Other vim-enabled leaves keep the default.
                if vim_enabled {
                    w.set_vim_mode(crate::smelt_edit::VimMode::Insert);
                }
            }
            // Seed a minimal splits tree (transcript + prompt) so overlay
            // anchors resolve before the first render frame. The Lua
            // composer (registered by `runtime/lua/smelt/layout.lua`)
            // replaces this on the next render once `bring_up_lua` runs
            // and `prompt_bar.lua` / `statusline.lua` have allocated
            // their windows.
            ui.set_layout(crate::content::layout::seed_layout_tree(
                /* prompt_input_rows */ 1,
            ));
            ui.set_focus(crate::app::PROMPT_WIN);
            (
                ui,
                WellKnown {
                    prompt: crate::app::PROMPT_WIN,
                    transcript: crate::app::TRANSCRIPT_WIN,
                    cmdline: None,
                },
            )
        };

        let inline_options = smelt_core::content::highlight::InlineOptions {
            file_icons: smelt_core::content::file_icons::FileIconOptions::new(
                runtime_state.settings.file_icons,
                runtime_state.settings.file_icon_colors,
                ui.theme().is_light(),
                Some(std::path::PathBuf::from(&cwd)),
            ),
        };
        let mut transcript = crate::app::transcript::TranscriptDocument::new();
        transcript.set_inline_options(inline_options.clone());
        let mut resume_preview_cache = crate::app::transcript::ResumePreviewCache::new(6);
        resume_preview_cache.set_inline_options(inline_options);

        let working_clock = Arc::clone(&clock);
        let managed_models =
            crate::app::managed_models::ManagedModelState::new(managed_models, clock.instant_now());
        let initial_agent_mode = runtime_state.mode.clone();
        let initial_reasoning_effort = runtime_state.reasoning_effort.clone();
        let auto_reload_enabled = runtime_state.settings.auto_reload;
        let mut session = smelt_core::session::Session::new(env.pid(), env.cwd());
        session.fast_mode = Some(runtime_state.settings.fast_mode);
        let prompt_history = History::load_from_state_root(env.state_dir().clone());
        let prompt_inputs =
            prompt_inputs.unwrap_or_else(|| crate::prompt_inputs::PromptInputs::for_runtime(&env));
        let mut core = smelt_core::Core::new(
            runtime_state,
            startup_overrides,
            engine,
            FrontendKind::Tui,
            permissions,
            clock,
            env,
        );
        core.skills = skills;
        core.mcp = mcp;
        let sessions = core.sessions.clone();
        let (job_completion_tx, job_completion_rx) = tokio::sync::mpsc::unbounded_channel();
        core.jobs.set_completion_sender(job_completion_tx);
        let platform = crate::app::platform_runtime::PlatformRuntime::new(
            &core.env,
            sessions.clone(),
            job_completion_rx,
            app_events,
        );
        let lua = crate::lua::LuaGeneration::initial(
            lua,
            Some(std::path::Path::new(&cwd)),
            project_trust.clone(),
        );
        let lua = crate::app::lua_handlers::LuaRuntimeController::new(lua);
        let watch_paths = crate::auto_reload::WatchPaths::from_manifest(
            lua.manifest.roots.clone(),
            lua.manifest.target_cwd.as_deref(),
        );
        let auto_reload =
            crate::auto_reload::AutoReloadController::new(auto_reload_enabled, watch_paths);
        Self {
            core,
            conversation: crate::app::conversation::ConversationRuntime::new(
                session,
                transcript,
                resume_preview_cache,
                shared_session,
                crate::app::agent::TurnLifecycle::new(initial_agent_mode, initial_reasoning_effort),
                sessions,
                session_persistence,
            ),
            lua,
            command_catalog,
            document_render_cache: crate::app::document::DocumentRenderCache::new(),
            prompt: crate::app::prompt_runtime::PromptRuntime::new(
                prompt_history,
                input,
                placeholder_state,
            ),
            overlays: crate::app::overlay_runtime::OverlayRuntime::default(),
            workspace,
            task_label: None,
            pending_quit: false,
            pending_session_save: false,
            pending_turn_dispatch: None,
            paint_registry: crate::lua::paint::PaintRegistry::default(),
            working: smelt_core::working::WorkingState::new(working_clock),
            host_work: host_dispatch::HostWorkState::default(),
            layout: crate::content::layout::LayoutState::default(),
            main_layout_inputs: None,
            platform,
            transcript_hydration_worker: None,
            transcript_detail: crate::app::transcript_detail::TranscriptDetailRuntime::default(),
            session_load: crate::app::session_load::SessionLoadRuntime::default(),
            session_preview: crate::app::session_preview::SessionPreviewRuntime::default(),
            pending_transcript_search: None,
            frame_scheduler: crate::app::render_loop::FrameScheduler::default(),
            transcript_work: crate::app::transcript_work::TranscriptWorkQueue::default(),
            continuing_engine_ask_ids: std::collections::HashSet::new(),
            last_width: term_w,
            last_height: term_h,
            managed_models,
            busy_stack: BusyStack::default(),
            api_base_normalization_warnings: HashSet::new(),
            startup_auth_error,
            startup_selections,
            project_trust: Some(project_trust),
            app_focus: AppFocus::Prompt,
            prompt_inputs,
            auto_reload,
            ui,
            well_known,
            timers: Timers {
                last_ctrlc: None,
                last_keypress: None,
                pending_pane_chord: None,
                pending_transcript_fold_chord: None,
                pending_chord: None,
            },
        }
    }

    /// Returns the system prompt without mutating app state.
    pub(crate) fn assemble_system_prompt(&self) -> String {
        let mut prompt = engine::assemble_system_prompt(
            self.prompt_inputs.system_prompt_override.as_deref(),
            engine::SystemPromptBehavior::Interactive,
            engine::SystemPromptCapabilities::from_tool_calling(
                self.core
                    .config
                    .active_model()
                    .is_none_or(|model| model.config.tool_calling()),
            ),
            self.prompt_inputs.instructions.as_deref(),
            self.prompt_inputs.skill_section.as_deref(),
        );
        let fragments = self.lua.system_prompt_fragments();
        if !fragments.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&fragments.join("\n\n"));
        }
        prompt
    }

    pub(crate) fn stop_shell_jobs(&mut self) {
        self.core.jobs.clear();
        let _ = self.platform.drain_job_completions();
    }

    /// Fire due timer callbacks; re-arms recurring entries and drops one-shots.
    pub(crate) fn tick_timers(&mut self) {
        let due = self.core.timers.drain_due(self.lua.lua());
        if due.is_empty() {
            return;
        }
        let lua = self.lua.execution();
        crate::lua::scope_app(self, move || {
            for func in due {
                let _perf = smelt_perf::perf::begin("lua:timer");
                if let Err(e) = func.call::<()>(()) {
                    lua.record_error(format!("timer: {e}"));
                }
            }
        });
    }

    fn keymap_pending_cell_value(&self) -> String {
        self.timers
            .pending_chord
            .as_ref()
            .map(|pending| crate::lua::display_chord_sequence(&pending.tokens.concat()))
            .unwrap_or_default()
    }

    pub(crate) fn expire_pending_keymap_chord(&mut self) -> bool {
        let now = self.core.clock.instant_now();
        let expired = self
            .timers
            .pending_chord
            .as_ref()
            .and_then(|pending| pending.policy.expires_at())
            .is_some_and(|expires_at| expires_at <= now);
        if expired {
            self.timers.pending_chord = None;
            return true;
        }
        false
    }

    fn pending_keymap_chord_expiry_delay(&self) -> Option<Duration> {
        let expires_at = self.timers.pending_chord.as_ref()?.policy.expires_at()?;
        Some(expires_at.saturating_duration_since(self.core.clock.instant_now()))
    }

    /// Publish prompt, mode, keymap, transcript navigation, time, notification,
    /// spinner, and work-state signals whenever their values change.
    pub(crate) fn publish_diff_signals(&mut self) {
        let keymap_pending = self.keymap_pending_cell_value();
        self.core
            .signals
            .publish_if_changed("vim_mode", self.vim_mode_cell_value());
        self.core
            .signals
            .publish_if_changed("vim_pending_input", self.vim_pending_input_cell_value());
        self.core
            .signals
            .publish_if_changed("keymap_pending", keymap_pending);
        self.core
            .signals
            .publish_if_changed("confirms_pending", !self.core.confirms.is_clear());
        let fast_mode = self.fast_mode_active();
        let prompt_queue_revision = self.prompt.queue_revision();
        self.core.signals.publish_if_changed("fast_mode", fast_mode);
        self.core
            .signals
            .publish_if_changed("prompt_queue_revision", prompt_queue_revision);
        let now_secs = self
            .core
            .clock
            .system_now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.core.signals.publish_if_changed("now", now_secs);
        let frame = self
            .working
            .elapsed()
            .filter(|_| self.working.is_animating())
            .map(|e| smelt_core::content::spinner_frame_index(e) as u8)
            .unwrap_or(0);
        self.core.signals.publish_if_changed("spinner_frame", frame);

        let tps = self.working.display_tps().unwrap_or(0.0);
        self.core.signals.publish_if_changed("tps", tps);

        self.core
            .signals
            .publish_if_changed("cwd_project", self.workspace.project().to_owned());
        self.core
            .signals
            .publish_if_changed("cwd_branch", self.workspace.branch().to_owned());
        self.core
            .signals
            .publish_if_changed("cwd_worktree", self.workspace.worktree().to_owned());
        self.core.signals.publish_if_changed(
            "cwd_worktree_path",
            self.workspace.worktree_path().to_owned(),
        );
        self.core
            .signals
            .publish_if_changed("cwd_managed_worktree", self.workspace.is_managed_worktree());

        let task_label = self.task_label.clone().unwrap_or_default();
        self.core
            .signals
            .publish_if_changed("task_label", task_label);
        self.core.signals.publish_if_changed(
            "session_title",
            self.conversation
                .session()
                .title
                .clone()
                .unwrap_or_default(),
        );
        self.core.signals.publish_if_changed(
            "session_slug",
            self.conversation.session().slug.clone().unwrap_or_default(),
        );
        self.core.signals.publish_if_changed(
            "settings_terminal_title",
            self.core.config.settings.terminal_title,
        );

        let running_procs = self.core.jobs.running_count() as u32;
        self.core
            .signals
            .publish_if_changed("running_procs", running_procs);

        let permission_pending = self.overlays.has_deferred_dialog();
        self.core
            .signals
            .publish_if_changed("permission_pending", permission_pending);

        self.core.signals.publish_if_changed(
            "notification_visible",
            self.overlays.notification_is_visible(),
        );
        self.publish_prompt_resize_state();

        let cursor = self.focused_cursor_pos();
        let viewport = self.focused_viewport_pos();
        self.core.signals.publish_if_changed("cursor_pos", cursor);
        self.core
            .signals
            .publish_if_changed("viewport_pos", viewport);

        self.publish_work_signals();
    }

    #[cfg(test)]
    pub(crate) fn set_prompt_resize_drag(&mut self, drag: Option<PromptResizeDrag>) {
        self.prompt.set_resize_drag_for_harness(drag);
        self.publish_prompt_resize_state();
    }

    pub(crate) fn publish_prompt_resize_state(&mut self) {
        let active_chrome = self.prompt.active_resize_chrome();
        self.core
            .signals
            .publish_if_changed("prompt_resize_active", !active_chrome.is_empty());
        self.core
            .signals
            .publish_if_changed("prompt_resize_chrome", active_chrome.to_string());
    }

    /// User-facing vim mode label for the focused surface - or empty
    /// when no vim-enabled surface owns input. The Lua statusline reads
    /// this directly from the `vim_mode` cell.
    fn vim_mode_cell_value(&self) -> String {
        let Some(mode) = self.focused_vim_mode() else {
            return String::new();
        };
        match mode {
            crate::smelt_edit::VimMode::Insert => "INSERT",
            crate::smelt_edit::VimMode::Visual => "VISUAL",
            crate::smelt_edit::VimMode::VisualLine => "V-LINE",
            crate::smelt_edit::VimMode::Normal => "NORMAL",
        }
        .into()
    }

    fn vim_pending_input_cell_value(&self) -> String {
        self.ui
            .focused_window()
            .and_then(|w| w.vim_pending_input())
            .unwrap_or_default()
    }

    /// Cursor position of the focused window, published as `cursor_pos`.
    /// Returns the default `(0, 0, 0)` when no focused window has lines.
    fn focused_cursor_pos(&self) -> smelt_core::signals::CursorPos {
        let Some(w) = self.ui.focused_window() else {
            return smelt_core::signals::CursorPos::default();
        };
        let Some(buf) = self.ui.buf(w.buf) else {
            return smelt_core::signals::CursorPos::default();
        };
        let total = buf.line_count();
        if total == 0 {
            return smelt_core::signals::CursorPos::default();
        }
        let line_idx = w.cursor_abs_row();
        let scroll_pct = if total <= 1 {
            100u8
        } else {
            ((line_idx * 100) / (total.saturating_sub(1) as u64)).min(100) as u8
        };
        smelt_core::signals::CursorPos {
            line: (line_idx as u32) + 1,
            col: (w.cursor_col() as u32) + 1,
            scroll_pct,
        }
    }

    /// Viewport position of the focused window, published as `viewport_pos`.
    /// Row-backed transcript views report progress through their full logical extent.
    fn focused_viewport_pos(&self) -> smelt_core::signals::ViewportPos {
        let Some(w) = self.ui.focused_window() else {
            return smelt_core::signals::ViewportPos::default();
        };
        let Some(buf) = self.ui.buf(w.buf) else {
            return smelt_core::signals::ViewportPos::default();
        };
        let viewport_rows = w.viewport.map(|v| v.rect.height).unwrap_or(1);
        let metrics = crate::smelt_edit::ViewportMetrics::new(
            w.scroll_top(),
            w.scroll_row_total(buf),
            viewport_rows,
        );
        smelt_core::signals::ViewportPos {
            scroll_pct: metrics.scroll_pct,
        }
    }

    /// Resolve the public `WorkState` and label from `WorkingState` +
    /// the per-app busy stack. Engine-side `Working`/`Retrying`/`Paused`
    /// win over busy tokens; an empty engine state with a live busy
    /// token reads as `Busy`; otherwise `Done`/`Interrupted` if a turn
    /// just archived, else `Idle`. The label is the top busy entry's
    /// when set, otherwise `"working"` for live engine phases, otherwise
    /// empty.
    pub(crate) fn resolve_work_state(&self) -> (smelt_core::working::WorkState, String) {
        use smelt_core::working::{TurnOutcome, WorkState};
        let engine = self.working.engine_state();
        let busy_label = self.busy_stack.top_label();
        let outcome = self.working.last_outcome();

        let state = if let Some(s) = engine {
            s
        } else if self.busy_stack.is_busy() {
            WorkState::Busy
        } else {
            match outcome {
                Some(TurnOutcome::Done) => WorkState::Done,
                Some(TurnOutcome::Cancelled | TurnOutcome::Errored) => WorkState::Interrupted,
                None => WorkState::Idle,
            }
        };

        let label = if let Some(l) = busy_label {
            l
        } else if matches!(
            engine,
            Some(WorkState::Working) | Some(WorkState::Retrying) | Some(WorkState::Paused)
        ) {
            self.working.phase_label().unwrap_or("working").to_string()
        } else {
            String::new()
        };

        (state, label)
    }

    /// Derive and publish the `work_*` signals from `WorkingState` and the
    /// per-app busy stack.
    fn publish_work_signals(&mut self) {
        use smelt_core::working::TurnOutcome;

        let (state, label) = self.resolve_work_state();
        let engine = self.working.engine_state();
        let outcome = self.working.last_outcome();

        let outcome_str = match outcome {
            Some(TurnOutcome::Done) if engine.is_none() => "done",
            Some(TurnOutcome::Cancelled | TurnOutcome::Errored) if engine.is_none() => {
                "interrupted"
            }
            _ => "",
        };

        let elapsed_ms = if self.working.is_animating() {
            self.working.elapsed()
        } else if self.busy_stack.is_busy() {
            self.busy_stack.elapsed()
        } else {
            self.working.elapsed()
        }
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

        let (retry_attempt, retry_remaining_ms) = self.working.retry_info().unwrap_or((0, 0));

        self.core
            .signals
            .publish_if_changed("work_state", state.as_str().to_string());
        self.core.signals.publish_if_changed("work_label", label);
        self.core.signals.publish_if_changed(
            "work_continuation_token",
            self.conversation.continuation_token().unwrap_or(0),
        );
        self.core.signals.publish_if_changed(
            "work_pause_kind",
            self.conversation
                .turn_pause()
                .map(|pause| pause.kind.map_or("other", |kind| kind.as_str()))
                .unwrap_or("")
                .to_string(),
        );
        self.core
            .signals
            .publish_if_changed("work_elapsed_ms", elapsed_ms);
        self.core
            .signals
            .publish_if_changed("work_outcome", outcome_str.to_string());
        self.core
            .signals
            .publish_if_changed("work_retry_attempt", retry_attempt);
        self.core
            .signals
            .publish_if_changed("work_retry_remaining_ms", retry_remaining_ms);
        self.core
            .signals
            .publish_if_changed("work_busy", self.busy_stack.entries_snapshot());
    }

    fn public_attention_reason(&self) -> Option<smelt_core::public_status::PublicReason> {
        use smelt_core::public_status::PublicReason;

        if !self.core.confirms.is_clear() {
            Some(PublicReason::Permission)
        } else if self.modal_blocks_agent() {
            Some(PublicReason::Question)
        } else if self.overlays.has_deferred_dialog() {
            Some(PublicReason::Permission)
        } else {
            None
        }
    }

    fn public_status_state_reason(
        &self,
    ) -> (
        smelt_core::public_status::PublicState,
        Option<smelt_core::public_status::PublicReason>,
    ) {
        use smelt_core::public_status::{PublicReason, PublicState};
        use smelt_core::working::WorkState;

        if let Some(reason) = self.public_attention_reason() {
            return (PublicState::NeedsAttention, Some(reason));
        }

        let (work_state, _) = self.resolve_work_state();
        match work_state {
            WorkState::Working | WorkState::Retrying | WorkState::Paused | WorkState::Busy => {
                (PublicState::Busy, None)
            }
            WorkState::Done if !self.platform.terminal_is_focused() => (
                PublicState::NeedsAttention,
                Some(PublicReason::TurnComplete),
            ),
            WorkState::Done => (PublicState::Idle, Some(PublicReason::TurnComplete)),
            WorkState::Interrupted => match self.working.last_outcome() {
                Some(smelt_core::working::TurnOutcome::Errored) => {
                    (PublicState::NeedsAttention, Some(PublicReason::Error))
                }
                _ => (PublicState::Idle, Some(PublicReason::Interrupted)),
            },
            WorkState::Idle => (PublicState::Idle, None),
        }
    }

    fn publish_public_status(&mut self) {
        use smelt_core::public_status::{FocusState, StatusUpdate};

        let focus = if self.platform.terminal_is_focused() {
            FocusState::Focused
        } else {
            FocusState::Unfocused
        };
        let (state, reason) = self.public_status_state_reason();
        self.platform.publish_status(StatusUpdate {
            state,
            reason,
            focus,
            cwd: Some(self.workspace.cwd().to_owned()),
            session_id: Some(self.conversation.session().id.clone()),
            mode: self.conversation.session().mode.clone(),
            headless: false,
        });
    }

    /// Drain pending signal notifications and invoke subscribers.
    pub(crate) fn drain_signals_pending(&mut self) {
        if !self.core.signals.has_pending() {
            return;
        }
        let fires = self.core.signals.drain_pending();
        let lua = self.lua.lua();
        let mut calls = Vec::new();
        for fire in fires {
            let value = self.core.signals.project_to_lua(&*fire.value, lua);
            let prev = fire
                .prev
                .as_deref()
                .map(|prev| self.core.signals.project_to_lua(prev, lua))
                .unwrap_or(mlua::Value::Nil);
            for cb in &fire.callbacks {
                let smelt_core::signals::SubscriberKind::Lua(handle) = &cb.kind;
                let func = match lua.registry_value::<mlua::Function>(&handle.key) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                calls.push((
                    fire.name.clone(),
                    value.clone(),
                    prev.clone(),
                    func,
                    cb.is_glob,
                ));
            }
        }
        let lua = self.lua.execution();
        crate::lua::scope_app(self, move || {
            for (name, value, prev, func, is_glob) in calls {
                let _perf = smelt_perf::perf::begin("lua:cell_cb");
                let result = if is_glob {
                    func.call::<()>((name.clone(), value, prev))
                } else {
                    func.call::<()>((value, prev))
                };
                if let Err(e) = result {
                    lua.record_error(format!("cell `{name}`: {e}"));
                }
            }
        });
    }

    pub(crate) fn sync_prompt_placeholder_display(&mut self) {
        if self.prompt.sync_prompt_placeholder_display() {
            if let Some(buf) = self.ui.buf_mut(crate::app::PROMPT_EDIT_BUF) {
                buf.invalidate_render_cache();
            }
        }
    }

    /// Returns the current placeholder text on `win`, if any.
    pub(crate) fn placeholder_text(&mut self, win: crate::smelt_edit::WinId) -> Option<String> {
        if let Some(text) = self.prompt.placeholder_text(win) {
            return Some(text.to_string());
        }
        let buf = self.ui.win_buf_mut(win)?;
        crate::content::prompt_buf::placeholder_text(buf)
    }

    /// Set the placeholder text on `win`. Empty text clears any prior placeholder.
    pub fn set_placeholder(&mut self, win: crate::smelt_edit::WinId, text: String) {
        if text.is_empty() {
            self.clear_placeholder(win);
            return;
        }
        if win == crate::app::PROMPT_WIN {
            self.prompt.set_placeholder_text(win, text);
            self.sync_prompt_placeholder_display();
            return;
        }
        if self.ui.win(win).is_none() {
            return;
        }
        self.prompt.set_placeholder_text(win, text.clone());
        if let Some(buf) = self.ui.win_buf_mut(win) {
            crate::content::prompt_buf::set_placeholder_extmark(buf, Some(text));
        }
    }

    pub(crate) fn set_placeholder_options(
        &mut self,
        win: crate::smelt_edit::WinId,
        options: PlaceholderOpts,
    ) {
        self.prompt.set_placeholder_options(win, options);
    }

    /// Clear the placeholder on `win` (text + opts). Idempotent.
    pub fn clear_placeholder(&mut self, win: crate::smelt_edit::WinId) {
        self.prompt.clear_placeholder(win);
        if win == crate::app::PROMPT_WIN {
            self.sync_prompt_placeholder_display();
        }
        if let Some(buf) = self.ui.win_buf_mut(win) {
            crate::content::prompt_buf::set_placeholder_extmark(buf, None);
        }
    }

    /// Match a key against the placeholder dispatch policy for `win`.
    /// Returns `Some(Redraw)` if the key was consumed by accept/dismiss,
    /// `None` otherwise.
    ///
    /// Accept replaces the buffer with the first line of the placeholder text
    /// (multi-line predictions collapse to a single line for safety) and fires
    /// `WinEvent::PlaceholderAccepted`. Dismiss clears the placeholder and fires
    /// `WinEvent::PlaceholderDismissed`. Both run only when the buffer is empty
    /// - the same visibility rule that gates rendering.
    pub(crate) fn dispatch_placeholder_key(
        &mut self,
        win: crate::smelt_edit::WinId,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> Option<EventOutcome> {
        let text = self.placeholder_text(win)?;
        let opts = self.prompt.placeholder_options(win)?.clone();
        let buf_empty = self
            .ui
            .win_buf_mut(win)
            .map(|b| b.source().is_empty())
            .unwrap_or(true);
        if !buf_empty {
            return None;
        }
        let kb = crate::smelt_edit::KeyBind::new(code, mods);
        if opts.accept_keys.contains(&kb) {
            self.clear_placeholder(win);
            if win == self.well_known.prompt {
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.prompt.replace_text(&mut pctx, text.clone());
            }
            self.fire_placeholder_event(
                win,
                crate::smelt_edit::WinEvent::PlaceholderAccepted,
                text,
            );
            return Some(EventOutcome::Redraw);
        }
        if opts.dismiss_keys.contains(&kb) {
            self.clear_placeholder(win);
            self.fire_placeholder_event(
                win,
                crate::smelt_edit::WinEvent::PlaceholderDismissed,
                text,
            );
            return Some(EventOutcome::Redraw);
        }
        None
    }

    fn fire_placeholder_event(
        &mut self,
        win: crate::smelt_edit::WinId,
        event: crate::smelt_edit::WinEvent,
        text: String,
    ) {
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              w: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, w, payload);
        };
        self.ui.fire_win_event(
            win,
            event,
            crate::smelt_edit::Payload::Text { content: text },
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    pub(crate) fn transcript_win(&self) -> &crate::smelt_edit::Window {
        self.ui
            .win(self.well_known.transcript)
            .expect("transcript window")
    }

    pub(crate) fn transcript_win_mut(&mut self) -> &mut crate::smelt_edit::Window {
        self.ui
            .win_mut(self.well_known.transcript)
            .expect("transcript window")
    }

    pub(crate) fn prompt_buf(&self) -> &crate::smelt_edit::Buffer {
        self.ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .expect("prompt edit buffer")
    }

    pub(crate) fn prompt_win(&self) -> &crate::smelt_edit::Window {
        self.ui.win(crate::app::PROMPT_WIN).expect("prompt window")
    }

    pub(crate) fn prompt_win_mut(&mut self) -> &mut crate::smelt_edit::Window {
        self.ui
            .win_mut(crate::app::PROMPT_WIN)
            .expect("prompt window")
    }

    /// Width available for transcript content (window width minus gutter and scrollbar columns).
    pub(crate) fn transcript_width(&self) -> usize {
        self.ui
            .win_content_width(self.well_known.transcript)
            .map(|width| (width as usize).max(1))
            .unwrap_or_else(|| {
                let (terminal_width, _) = self.ui.terminal_size();
                let win = self.transcript_win();
                let gutter_width = self
                    .ui
                    .buf(win.buf)
                    .map(|buf| win.gutter_width(buf))
                    .unwrap_or(0)
                    .min(terminal_width);
                let width = win
                    .config
                    .gutters
                    .content_width_with_gutter(terminal_width, gutter_width);
                (width as usize).max(1)
            })
    }

    /// Resolves a raw leaf id to a live `Window` or a registered paint region (`None` if unrecognised).
    pub(crate) fn resolve_leaf_id(&self, raw_id: u64) -> Option<crate::lua::paint::LeafKind> {
        let win = crate::smelt_edit::WinId(raw_id);
        if self.ui.win(win).is_some() {
            // Catches a future regression where the smelt-edit allocator
            // grows past the partition boundary into paint-id space.
            debug_assert!(
                raw_id < crate::lua::paint::PAINT_ID_BASE,
                "WinId {raw_id} crossed into PaintId half (>= {})",
                crate::lua::paint::PAINT_ID_BASE
            );
            return Some(crate::lua::paint::LeafKind::Window(win));
        }
        let paint_id = crate::smelt_edit::layout::PaintId(raw_id);
        if self.paint_registry.contains(paint_id) {
            return Some(crate::lua::paint::LeafKind::Paint(paint_id));
        }
        None
    }

    pub(crate) fn notify(&mut self, message: String) {
        self.record_notice(
            smelt_core::messages::MessageKind::Info,
            "smelt".into(),
            message,
        );
    }

    pub(crate) fn drain_persist_reports(&mut self) {
        let Some(report) = self.conversation.drain_persistence_report() else {
            return;
        };
        if let Some(session_id) = report.acknowledged_session_id {
            self.dismiss_session_save_failure_notification(&session_id);
        }
        for completion in report.canonical_completions {
            self.handle_canonical_completion(completion);
        }
        if let Some(failure) = report.failure {
            let notify = match failure.target {
                crate::app::conversation::PersistenceFailureTarget::Command(command_id) => {
                    self.fail_pending_turn_submission(Some(command_id), &failure.cause);
                    false
                }
                crate::app::conversation::PersistenceFailureTarget::AllCanonicalCommands => {
                    self.fail_pending_turn_submission(None, &failure.cause);
                    self.conversation.abandon_all_canonical_operations();
                    true
                }
                crate::app::conversation::PersistenceFailureTarget::None => true,
            };
            if notify {
                self.notify_session_persistence_failure(&failure.session_id, &failure.cause);
            }
        }
        if let Some(warning) = report.audit_warning {
            self.notify_warn(warning);
        }
        self.drive_canonical_operations();
    }

    fn handle_canonical_completion(
        &mut self,
        completion: crate::persist::CanonicalCommandCompletion,
    ) {
        match completion {
            crate::persist::CanonicalCommandCompletion::Submit(acknowledgement) => {
                let command_id = acknowledgement.command_id;
                if self.resume_turn_submission_after_persistence(*acknowledgement) {
                    self.conversation.confirm_canonical_completion(command_id);
                }
            }
            crate::persist::CanonicalCommandCompletion::Transition(acknowledgement) => {
                let command_id = acknowledgement.command_id;
                let turn_id = acknowledgement.receipt.turn_id;
                let terminal = acknowledgement.receipt.state.is_terminal();
                if self.conversation.confirm_canonical_completion(command_id) && terminal {
                    self.conversation.mark_terminal(turn_id.get());
                    self.start_next_queued_input_if_idle();
                }
            }
            crate::persist::CanonicalCommandCompletion::Failed {
                command_id, cause, ..
            } => {
                if !self.fail_pending_turn_submission(Some(command_id), &cause) {
                    self.conversation.confirm_canonical_completion(command_id);
                }
                self.notify_session_persistence_failure(
                    &self.conversation.session().id.clone(),
                    &cause,
                );
            }
        }
    }

    fn drive_canonical_operations(&mut self) {
        loop {
            let events = self.conversation.drive_canonical_operations();
            if events.is_empty() {
                return;
            }
            for event in events {
                match event {
                    crate::app::conversation::CanonicalOperationEvent::Submit(outcome) => {
                        self.handle_canonical_submit_outcome(outcome);
                    }
                    crate::app::conversation::CanonicalOperationEvent::TransitionDurable(
                        acknowledgement,
                    ) => {
                        if acknowledgement.receipt.state.is_terminal() {
                            self.conversation
                                .mark_terminal(acknowledgement.receipt.turn_id.get());
                            self.start_next_queued_input_if_idle();
                        }
                    }
                    crate::app::conversation::CanonicalOperationEvent::Failed {
                        command_id,
                        cause,
                    } => {
                        self.fail_pending_turn_submission(Some(command_id), &cause);
                        self.notify_session_persistence_failure(
                            &self.conversation.session().id.clone(),
                            &cause,
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::ManagedModelsRefreshCompleted { token, outcome } => {
                self.handle_managed_models_refresh(token, outcome)
            }
            AppEvent::ManagedModelsRetry {
                provider,
                auth_revision,
                desired_revision,
            } => self.handle_managed_models_retry(provider, auth_revision, desired_revision),
            AppEvent::ManagedAuthChecked { snapshots } => {
                self.handle_managed_auth_checked(snapshots)
            }
            AppEvent::McpStartupReady {
                busy_token,
                readiness,
            } => {
                let unavailable = self
                    .core
                    .mcp
                    .as_ref()
                    .map(|manager| {
                        manager
                            .unavailable_servers()
                            .into_iter()
                            .map(|(name, status)| format!("{name} ({})", status.as_str()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.busy_stack.release(busy_token);
                self.start_next_queued_input_if_idle();
                if !unavailable.is_empty() {
                    let reason =
                        if matches!(readiness, smelt_core::mcp::McpReadiness::TimedOut { .. }) {
                            "discovery is still in progress"
                        } else {
                            "discovery failed"
                        };
                    self.notify_warn(format!(
                        "MCP tool {reason} for {}; continuing without those tools",
                        unavailable.join(", ")
                    ));
                }
            }
            AppEvent::TranscriptSearchCompleted(result) => {
                self.handle_transcript_search_worker_result(result)
            }
            AppEvent::TranscriptHydrationCompleted(result) => {
                let current = self
                    .transcript_hydration_worker
                    .as_ref()
                    .is_some_and(|worker| worker.is_current(&result));
                if current {
                    let redraw = self
                        .conversation
                        .install_transcript_hydration_result(*result);
                    if self.transcript_work.front_waits_for_hydration() {
                        self.request_continuation_render();
                    }
                    if self.complete_pending_transcript_details() {
                        self.request_urgent_render();
                    }
                    self.drive_canonical_operations();
                    self.save_deferred_session_batch_if_ready();
                    if !self.conversation.transcript_hydration_is_pending() {
                        self.retry_pending_transcript_search();
                    }
                    if redraw {
                        self.ui.force_redraw();
                        self.request_urgent_render();
                    }
                }
            }
            AppEvent::SessionLoadCompleted(result) => {
                self.handle_session_load_worker_result(*result)
            }
            AppEvent::SessionPreviewCompleted(result) => {
                self.handle_session_preview_worker_result(result)
            }
            AppEvent::ShutdownSignal => self.pending_quit = true,
        }
    }

    pub(crate) fn notify_error(&mut self, message: String) {
        self.record_notice(
            smelt_core::messages::MessageKind::Error,
            "smelt".into(),
            message,
        );
    }

    fn notify_error_sticky(&mut self, scope: NotificationScope, message: String) {
        self.record_notice_with_lifetime(
            smelt_core::messages::MessageKind::Error,
            "smelt".into(),
            message,
            NotificationLifetime::Sticky,
            scope,
        );
    }

    pub(crate) fn notify_application_error_sticky(&mut self, message: String) {
        self.notify_error_sticky(NotificationScope::Application, message);
    }

    pub(crate) fn notify_workspace_error_sticky(&mut self, message: String) {
        self.notify_error_sticky(NotificationScope::Workspace, message);
    }

    pub(crate) fn notify_session_error_sticky_for(&mut self, session_id: &str, message: String) {
        self.notify_error_sticky(NotificationScope::Session(session_id.to_string()), message);
    }

    pub(crate) fn notify_session_error_sticky(&mut self, message: String) {
        let session_id = self.conversation.session().id.clone();
        self.notify_session_error_sticky_for(&session_id, message);
    }

    pub(crate) fn notify_turn_error_sticky(&mut self, message: String) {
        self.notify_error_sticky(NotificationScope::Turn, message);
    }

    pub(crate) fn notify_operation_error_sticky(
        &mut self,
        operation: NotificationOperation,
        message: String,
    ) {
        self.notify_error_sticky(NotificationScope::Operation(operation), message);
    }

    pub(crate) fn notify_operation_error(
        &mut self,
        operation: NotificationOperation,
        message: String,
    ) {
        self.record_notice_with_lifetime(
            smelt_core::messages::MessageKind::Error,
            "smelt".into(),
            message,
            NotificationLifetime::timed(self.core.clock.instant_now()),
            NotificationScope::Operation(operation),
        );
    }

    pub(crate) fn notify_session_save_failure(&mut self, session_id: &str, message: &str) {
        self.record_notice_with_lifetime(
            smelt_core::messages::MessageKind::Error,
            "smelt".into(),
            format!("failed to save session {session_id}: {message}"),
            NotificationLifetime::Sticky,
            NotificationScope::Operation(NotificationOperation::SessionPersistence(
                session_id.to_string(),
            )),
        );
    }

    pub(crate) fn notify_session_persistence_failure(
        &mut self,
        session_id: &str,
        cause: &crate::persist::PersistenceCause,
    ) {
        let message = match cause.recovery_action() {
            Some(crate::persist::PersistenceRecoveryAction::Retry) => format!(
                "{}; fix the storage problem, then run /retry-save to continue this session",
                cause.message
            ),
            None => cause.message.clone(),
        };
        self.notify_session_save_failure(session_id, &message);
    }

    #[allow(dead_code)] // Only reachable via the Lua surface today.
    pub(crate) fn notify_warn(&mut self, message: String) {
        self.record_notice(
            smelt_core::messages::MessageKind::Warning,
            "smelt".into(),
            message,
        );
    }

    pub(crate) fn warn_if_api_base_normalized(&mut self) {
        let Some(active) = self.core.config.active_model() else {
            return;
        };
        let Some(hint) = smelt_provider::api_base_normalization_hint(&active.api_base) else {
            return;
        };
        let key = format!(
            "{}\n{}\n{}",
            active.provider_type, hint.original, hint.normalized
        );
        if !self.api_base_normalization_warnings.insert(key) {
            return;
        }
        self.record_notice(
            smelt_core::messages::MessageKind::Warning,
            "config".into(),
            format!(
                "api_base includes /{}; using {} instead.\nSet api_base to the base URL to avoid ambiguity.",
                hint.endpoint, hint.normalized
            ),
        );
    }

    /// Append `body` to the persistent message log AND surface the first
    /// line of `body` as a toast clipped to the terminal width. Every
    /// user-visible toast in the TUI goes through here so `/messages`
    /// stays a faithful audit log and no toast can ever wrap past its
    /// reserved row.
    pub(crate) fn record_notice(
        &mut self,
        kind: smelt_core::messages::MessageKind,
        source: String,
        body: String,
    ) {
        self.record_notice_with_lifetime(
            kind,
            source,
            body,
            NotificationLifetime::timed(self.core.clock.instant_now()),
            NotificationScope::Transient,
        );
    }

    fn record_notice_with_lifetime(
        &mut self,
        kind: smelt_core::messages::MessageKind,
        source: String,
        body: String,
        lifetime: NotificationLifetime,
        scope: NotificationScope,
    ) {
        if let Ok(mut messages) = self.lua.core_shared().messages.lock() {
            messages.append(kind, source, body.clone());
        }
        self.open_notification(kind, &body, lifetime, scope);
    }

    fn open_notification(
        &mut self,
        kind: smelt_core::messages::MessageKind,
        body: &str,
        lifetime: NotificationLifetime,
        scope: NotificationScope,
    ) {
        if let Some(notification) = self.overlays.take_notification() {
            self.close_overlay_leaf(notification.win);
        }

        let summary = body.lines().next().unwrap_or("");
        if self.has_docked_dialog() {
            let now = self.core.clock.instant_now();
            let lifetime = match lifetime {
                NotificationLifetime::Timed { expires_at } => {
                    SuspendedNotificationLifetime::Timed(expires_at.saturating_duration_since(now))
                }
                NotificationLifetime::Sticky => SuspendedNotificationLifetime::Sticky,
            };
            self.overlays
                .install_suspended_notification(SuspendedNotification {
                    lifetime,
                    kind,
                    summary: summary.to_string(),
                    scope,
                });
            return;
        }

        let width = self.ui.terminal_size().0 as usize;
        let buf = self
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());
        Self::write_notification_buf(&mut self.ui, buf, kind, summary, width);

        let Some(win) = self.ui.win_open_split(
            buf,
            crate::smelt_edit::SplitConfig {
                region: "notification".into(),
                gutters: Default::default(),
            },
        ) else {
            return;
        };
        if let Some(w) = self.ui.win_mut(win) {
            w.set_surface(crate::smelt_edit::WindowSurface::selectable_text());
        }

        let layout = crate::smelt_edit::LayoutTree::vbox(vec![(
            crate::smelt_edit::Constraint::Length(1),
            crate::smelt_edit::LayoutTree::hbox(vec![(
                crate::smelt_edit::Constraint::Percentage(100),
                crate::smelt_edit::LayoutTree::leaf(win),
            )]),
        )]);
        // Float `1` row above the prompt's Lua-allocated top bar (or the
        // prompt input on cold start, before Lua has registered the bar).
        // Anchoring against the named window keeps the toast correctly
        // placed even when queued/stash rows grow the top bar.
        let anchor = crate::content::layout::anchor_above_prompt_chrome(&self.ui, 1);
        let _overlay_id = self.ui.overlay_open(
            crate::smelt_edit::Overlay::new(layout, anchor)
                // Sits below dialogs (default overlay z 50) so a toast
                // never obscures a modal asking for input.
                .with_z(40),
        );
        self.overlays.install_notification(Notification {
            win,
            lifetime,
            kind,
            summary: summary.to_string(),
            scope,
            rendered_width: width,
        });
    }

    fn notification_parts(
        kind: smelt_core::messages::MessageKind,
        summary: &str,
        width: usize,
    ) -> (String, u16, u16, u16, u16) {
        use smelt_core::messages::MessageKind;

        let label = match kind {
            MessageKind::Info => "info",
            MessageKind::Warning => "warn",
            MessageKind::Error => "error",
        };
        let indent = "  ";
        let gap = "  ";

        // The toast is a single visible row and clamps to the current
        // viewport width.
        let prefix_w = smelt_buffer::cell_width::joined_text_width([indent, label, gap]);
        let available = width.saturating_sub(prefix_w);
        let summary =
            smelt_core::content::width::truncate_with_right_padding(summary, available, 2, "…");
        let line = format!("{indent}{label}{gap}{}", summary.text);

        let label_start = smelt_buffer::cell_width::text_width_u16(indent);
        let label_end = smelt_buffer::cell_width::joined_text_width_u16([indent, label]);
        let msg_start = smelt_buffer::cell_width::joined_text_width_u16([indent, label, gap]);
        let msg_end =
            smelt_buffer::cell_width::joined_text_width_u16([indent, label, gap, &summary.body]);
        (line, label_start, label_end, msg_start, msg_end)
    }

    pub(crate) fn write_notification_buf(
        ui: &mut crate::smelt_edit::Ui,
        buf: crate::smelt_edit::BufId,
        kind: smelt_core::messages::MessageKind,
        summary: &str,
        width: usize,
    ) {
        use smelt_core::messages::MessageKind;

        let (line, label_start, label_end, msg_start, msg_end) =
            Self::notification_parts(kind, summary, width);
        let label_color = match kind {
            MessageKind::Error => ui.theme().get("ErrorMsg").fg,
            MessageKind::Warning => ui.theme().get("WarningMsg").fg,
            MessageKind::Info => None,
        };
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines(vec![line]);
            b.add_highlight(
                0,
                label_start,
                label_end,
                crate::smelt_edit::SpanStyle {
                    fg: label_color,
                    bold: true,
                    ..Default::default()
                },
            );
            b.add_highlight(
                0,
                msg_start,
                msg_end,
                crate::smelt_edit::SpanStyle {
                    dim: true,
                    ..Default::default()
                },
            );
        }
    }

    pub(crate) fn dismiss_notification(&mut self) {
        self.overlays.clear_suspended_notification();
        if let Some(notification) = self.overlays.take_notification() {
            self.close_overlay_leaf(notification.win);
        }
    }

    fn dismiss_notification_if(&mut self, predicate: impl Fn(&NotificationScope) -> bool) -> bool {
        let active_matches = self
            .overlays
            .notification()
            .is_some_and(|notification| predicate(&notification.scope));
        let suspended_matches = self
            .overlays
            .suspended_notification()
            .is_some_and(|notification| predicate(&notification.scope));
        if suspended_matches {
            self.overlays.clear_suspended_notification();
        }
        if active_matches {
            if let Some(notification) = self.overlays.take_notification() {
                self.close_overlay_leaf(notification.win);
            }
        }
        active_matches || suspended_matches
    }

    pub(crate) fn dismiss_notification_for_workspace_change(&mut self) -> bool {
        self.dismiss_notification_if(NotificationScope::is_replaced_by_workspace)
    }

    pub(crate) fn dismiss_notification_for_session_change(&mut self) -> bool {
        self.dismiss_notification_if(NotificationScope::is_replaced_by_session)
    }

    pub(crate) fn dismiss_notification_for_turn_start(&mut self) -> bool {
        self.dismiss_notification_if(NotificationScope::is_replaced_by_turn)
    }

    pub(crate) fn dismiss_operation_notification(
        &mut self,
        operation: &NotificationOperation,
    ) -> bool {
        self.dismiss_notification_if(
            |scope| matches!(scope, NotificationScope::Operation(active) if active == operation),
        )
    }

    pub(crate) fn dismiss_session_save_failure_notification(&mut self, session_id: &str) {
        self.dismiss_notification_if(|scope| {
            matches!(
                scope,
                NotificationScope::Operation(NotificationOperation::SessionPersistence(
                    owner_session_id
                )) if owner_session_id == session_id
            )
        });
    }

    pub(crate) fn dismiss_expired_notification(&mut self) -> bool {
        let now = self.core.clock.instant_now();
        if self
            .overlays
            .notification()
            .is_some_and(|notification| notification.lifetime.is_expired(now))
        {
            self.dismiss_notification();
            return true;
        }
        false
    }

    pub(crate) fn notification_expiry_delay(&self) -> Option<Duration> {
        let now = self.core.clock.instant_now();
        self.overlays.notification_expiry_delay(now)
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn notification_win(&self) -> Option<crate::smelt_edit::WinId> {
        self.overlays.notification_win()
    }

    pub(crate) fn set_task_label(&mut self, label: String) {
        self.task_label = if label.trim().is_empty() {
            None
        } else {
            Some(label)
        };
    }

    fn dispatch_ui_window_events(&mut self, include_tick: bool) {
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_edit::LuaHandle,
                 win: crate::smelt_edit::WinId,
                 payload: &crate::smelt_edit::Payload| {
                    lua.queue_invocation(handle, win, payload);
                };
            if include_tick {
                self.ui.dispatch_tick(&mut lua_invoke);
            }
            self.ui.dispatch_focus_events(&mut lua_invoke);
            self.ui.dispatch_resize_events(&mut lua_invoke);
            self.ui.dispatch_scroll_events(&mut lua_invoke);
        }
        self.flush_lua_callbacks();
    }

    fn warmup_workspace_files(&mut self) {
        let _ = self.core.workspace_files.warmup(self.workspace.cwd_path());
    }

    fn render_scheduled_after_startup_work(
        &mut self,
        workspace_warmup_pending: &mut bool,
        pre_first_frame_startup: &mut Option<smelt_perf::perf::Guard>,
        first_frame_pending: &mut bool,
    ) {
        if !*first_frame_pending && !self.scheduled_frame_is_due() {
            return;
        }
        self.render_normal_after_startup_work(
            workspace_warmup_pending,
            pre_first_frame_startup,
            first_frame_pending,
        );
    }

    fn render_normal_after_startup_work(
        &mut self,
        workspace_warmup_pending: &mut bool,
        pre_first_frame_startup: &mut Option<smelt_perf::perf::Guard>,
        first_frame_pending: &mut bool,
    ) {
        if *first_frame_pending {
            drop(pre_first_frame_startup.take());
        }
        let first_render_startup = if *first_frame_pending {
            smelt_perf::perf::begin("startup:first_render")
        } else {
            None
        };
        self.render_normal();
        if std::mem::take(first_frame_pending) {
            smelt_perf::perf::record_value(
                "startup:first_frame_at_us",
                smelt_perf::perf::timestamp_us(),
            );
        }
        drop(first_render_startup);
        if std::mem::take(workspace_warmup_pending) {
            self.warmup_workspace_files();
        }
    }

    pub(crate) fn finalize_graceful_shutdown(&mut self) -> Result<(), String> {
        if self.conversation.is_active() {
            self.cancel_agent();
            self.conversation.clear_active();
        }
        self.core
            .signals
            .emit_dyn("shutdown", std::rc::Rc::new(smelt_core::signals::EventStub));
        self.drain_signals_pending();
        self.stop_shell_jobs();
        let shutdown = self.shutdown_persist().err();
        let unflushed = self.session_document_has_unflushed_work().then(|| {
            format!(
                "session {} still has unflushed changes",
                self.conversation.session().id
            )
        });
        match (unflushed, shutdown) {
            (Some(unflushed), Some(shutdown)) => Err(format!("{unflushed}; {shutdown}")),
            (Some(error), None) | (None, Some(error)) => Err(error),
            (None, None) => Ok(()),
        }
    }

    fn handle_platform_event(&mut self, event: crate::app::platform_runtime::PlatformEvent) {
        match event {
            crate::app::platform_runtime::PlatformEvent::App(event) => {
                let hydration_completed =
                    matches!(&event, AppEvent::TranscriptHydrationCompleted(_));
                self.handle_app_event(event);
                if !hydration_completed || self.frame_scheduler.has_pending() {
                    self.render_normal();
                }
            }
            crate::app::platform_runtime::PlatformEvent::ContextWindow(update) => {
                self.apply_context_window_update(*update);
            }
            crate::app::platform_runtime::PlatformEvent::JobCompleted(completion) => {
                self.handle_job_completed(completion);
            }
            crate::app::platform_runtime::PlatformEvent::PublicStatusHeartbeat => {
                self.publish_public_status();
            }
        }
    }

    pub async fn run(&mut self, http_client: engine::HttpClient, initial_message: Option<String>) {
        let platform_startup = smelt_perf::perf::begin("startup:platform");
        crate::theme::detect_background(self.ui.theme_mut());
        // Install the background-aware baked theme before Lua loads so a
        // configured colorscheme remains authoritative for the first frame.
        let is_light = self.ui.theme().is_light();
        let baked = crate::theme::default_baked_with_background(is_light);
        self.install_theme(baked);

        // PlatformRuntime owns the terminal envelope and restores it on normal
        // shutdown, early return, or panic. Lua title effects are committed only
        // after the platform has claimed the terminal.
        self.platform.start(http_client);
        drop(platform_startup);

        let lua_launch_startup = smelt_perf::perf::begin("startup:lua_launch");
        let lua_launch_error = self.finish_lua_launch(true);
        drop(lua_launch_startup);
        if let Some(error) = lua_launch_error {
            self.notify_workspace_error_sticky(format!("lua init: {error}"));
        }

        let mut pre_first_frame_startup = smelt_perf::perf::begin("startup:pre_first_frame");
        let mut first_frame_pending = true;
        self.submit_managed_model_refreshes();
        self.refresh_context_window();

        if !self.session_is_empty() {
            if !self.conversation.has_live_session() {
                self.restore_screen();
            }
            if let Some(ref slug) = self.conversation.session().slug {
                self.set_task_label(slug.clone());
            }
            self.finish_transcript_turn();
            self.transcript_win_mut().follow_tail();
        }
        if let Some(message) = self.startup_auth_error.take() {
            self.notify_workspace_error_sticky(message);
        }
        self.warn_if_api_base_normalized();

        self.core.signals.set_dyn(
            "session_started",
            std::rc::Rc::new(self.conversation.session().id.clone()),
        );
        self.drain_signals_pending();
        if let Some(state) = self.project_trust.take() {
            if matches!(state, smelt_core::trust::TrustState::Untrusted { .. }) {
                self.notify(
                    "project .smelt/ content not trusted; run /trust to load it".to_string(),
                );
            }
        }

        let mut workspace_warmup_pending = true;

        let mut term_events = match crate::term_input::TerminalInput::spawn() {
            Ok(input) => input,
            Err(e) => {
                self.notify_error(format!("terminal input: {e}"));
                self.platform.claim_failed_terminal();
                self.platform.shutdown();
                return;
            }
        };
        // Independent SIGWINCH listener: crossterm's Unix signal source intermittently
        // drops resize events (signal-hook-mio counter / mio readiness race), so we keep
        // our own tokio-native handler there. Both fire on resize; the duplicate just
        // hits an idempotent `compositor.resize` and one extra full repaint.
        #[cfg(unix)]
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                .expect("install SIGWINCH listener");

        // Workspace indexing and auto-reload watcher setup are kicked off
        // after the first frame so filesystem subscription and snapshot work
        // can never leave the alternate screen blank during startup.

        // Auto-submit initial message if provided (e.g. `agent "fix the bug"`).
        if let Some(msg) = initial_message {
            let trimmed = smelt_buffer::text::trim_whitespace(&msg);
            if let Some(cmd) = trimmed.strip_prefix('!') {
                if let Some(handle) = self.start_shell_escape(cmd) {
                    self.overlays.install_execution(handle);
                }
            } else if let Some(name) = smelt_core::commands::command_name(trimmed)
                .filter(|name| self.lua.has_command(name))
            {
                if self.lua.command_startup_ok(name) == Some(true) {
                    self.apply_lua_command(trimmed);
                } else {
                    self.notify_error(format!(
                        "\"{}\" has no effect as a startup argument",
                        trimmed
                    ));
                }
            } else {
                let sent_at_ms = engine::clock::unix_time_ms(self.core.clock.as_ref());
                let content = Content::text(msg.clone());
                let pending_mcp = self
                    .core
                    .mcp
                    .as_ref()
                    .filter(|manager| !manager.controller_status().is_ready())
                    .cloned();
                if let Some(manager) = pending_mcp {
                    self.prompt.queue_front(
                        QueueStage::Turn,
                        QueuedInput::request(msg, content, sent_at_ms),
                    );
                    let busy_token = self.busy_stack.push("connecting MCP tools".into());
                    let app_event_tx = self.platform.app_event_sender();
                    tokio::spawn(async move {
                        let readiness = manager
                            .wait_until_ready(smelt_core::mcp::STARTUP_DISCOVERY_WAIT)
                            .await;
                        let _ = app_event_tx.send(AppEvent::McpStartupReady {
                            busy_token,
                            readiness,
                        });
                    });
                } else {
                    let turn = self.begin_agent_turn(&msg, content, sent_at_ms);
                    self.conversation.set_active(turn);
                }
            }
        }

        'main: loop {
            if self.pending_quit {
                self.discard_turn(TurnEnd::Cancelled);
                break 'main;
            }
            self.tick_timers();
            self.drain_persist_reports();
            self.publish_diff_signals();
            self.publish_public_status();
            self.drain_signals_pending();
            self.drive_lua_tasks();
            for _id in self.drain_finished_blocks() {
                self.core.signals.emit_dyn(
                    "block_done",
                    std::rc::Rc::new(smelt_core::signals::EventStub),
                );
            }
            self.pump_lua();
            self.dispatch_ui_window_events(true);

            for update in self.platform.drain_context_window_updates() {
                self.apply_context_window_update(update);
            }

            let mut drained_idle_work = self.drain_idle_work();
            if drained_idle_work {
                self.render_normal_after_startup_work(
                    &mut workspace_warmup_pending,
                    &mut pre_first_frame_startup,
                    &mut first_frame_pending,
                );
            }

            self.drain_ready_engine_outputs_for_frame();

            for completion in self.platform.drain_job_completions() {
                self.handle_job_completed(completion);
            }

            let drained_more_idle_work = self.drain_idle_work();
            if drained_more_idle_work {
                self.render_normal_after_startup_work(
                    &mut workspace_warmup_pending,
                    &mut pre_first_frame_startup,
                    &mut first_frame_pending,
                );
            }
            drained_idle_work |= drained_more_idle_work;

            self.start_next_queued_input_if_idle();

            if !self.conversation.is_active() && self.overlays.has_deferred_dialog() {
                self.overlays.clear_deferred_dialogs();
            }
            if self.overlays.has_deferred_dialog()
                && !self.modal_blocks_agent()
                && self.conversation.is_active()
            {
                let idle = self
                    .timers
                    .last_keypress
                    .map(|lk| lk.elapsed() >= Duration::from_millis(CONFIRM_DEFER_MS))
                    .unwrap_or(true);
                while idle
                    && self.overlays.has_deferred_dialog()
                    && !self.modal_blocks_agent()
                    && self.conversation.is_active()
                {
                    let deferred = self
                        .overlays
                        .pop_deferred_dialog()
                        .expect("pending deferred dialog");
                    let ctrl = match deferred {
                        DeferredDialog::Confirm(req) => SessionControl::NeedsConfirm(req),
                    };
                    let end = self
                        .with_dispatched_turn(|app, turn| app.dispatch_control(ctrl, turn))
                        .expect("deferred dialog requires active turn");
                    match end {
                        SessionControl::Continue | SessionControl::NeedsConfirm(_) => {}
                        SessionControl::Done => {
                            self.discard_turn(TurnEnd::Complete);
                        }
                        SessionControl::Error { kind, retry_at_ms } => {
                            self.discard_turn(TurnEnd::Errored { kind, retry_at_ms });
                        }
                    }
                }
            }

            self.render_scheduled_after_startup_work(
                &mut workspace_warmup_pending,
                &mut pre_first_frame_startup,
                &mut first_frame_pending,
            );
            if self.auto_reload.start_pending {
                self.auto_reload.start_setup();
            }

            let now = self.core.clock.instant_now();
            let yank_flash_active = self
                .core
                .clipboard
                .kill_ring
                .yank_flash_until()
                .is_some_and(|t| t > now);
            let window_yank_flash_active = self.ui.yank_flash_active(now);
            let drag_active = self.ui.drag_capture_window().is_some();
            let has_animation = self.ui.focused_overlay().is_some()
                || self.has_active_exec()
                || self.working.is_animating()
                || self.busy_stack.is_busy()
                || yank_flash_active
                || window_yank_flash_active
                || drag_active;
            let next_timer_delay = self
                .core
                .timers
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(now));
            let next_notification_delay = self.notification_expiry_delay();
            let next_keymap_delay = self.pending_keymap_chord_expiry_delay();
            let next_transcript_refresh_delay = self
                .conversation
                .next_transcript_refresh_at()
                .map(|deadline| deadline.saturating_duration_since(now));
            let next_idle_delay = [
                next_timer_delay,
                next_notification_delay,
                next_keymap_delay,
                next_transcript_refresh_delay,
            ]
            .into_iter()
            .flatten()
            .min();
            let window_change = async {
                #[cfg(unix)]
                {
                    sigwinch.recv().await
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<Option<()>>().await
                }
            };
            if has_animation {
                let interval = self
                    .ui
                    .drag_autoscroll_interval()
                    .unwrap_or(Duration::from_millis(16));
                self.request_animation_render(interval);
            }
            let next_scheduled_frame_delay = self.scheduled_frame_delay();

            tokio::select! {
                biased;

                Some(ev) = term_events.recv() => {
                    // Coalesce scroll: batch rapid wheel ticks into a single motion + one render.
                    // Disabled when an overlay is focused so wheel events route into it.
                    let coalesce_scroll = self.ui.focused_overlay().is_none();
                    let mut scroll_delta: isize = 0;
                    let mut scroll_row: u16 = 0;
                    let mut scroll_col: u16 = 0;
                    let absorb = |ev: event::Event,
                                      delta: &mut isize,
                                      row: &mut u16,
                                      col: &mut u16|
                     -> Option<event::Event> {
                        if !coalesce_scroll {
                            return Some(ev);
                        }
                        if let event::Event::Mouse(m) = &ev {
                            match m.kind {
                                event::MouseEventKind::ScrollUp => {
                                    *delta -= 3;
                                    *row = m.row;
                                    *col = m.column;
                                    return None;
                                }
                                event::MouseEventKind::ScrollDown => {
                                    *delta += 3;
                                    *row = m.row;
                                    *col = m.column;
                                    return None;
                                }
                                _ => {}
                            }
                        }
                        Some(ev)
                    };

                    let drain_started_at = Instant::now();
                    let mut drained_events = 1usize;
                    if let Some(ev) = absorb(
                        ev,
                        &mut scroll_delta,
                        &mut scroll_row,
                        &mut scroll_col,
                    ) {
                        if self.dispatch_terminal_event(ev) {
                            break 'main;
                        }
                    }

                    while drained_events < READY_QUEUE_DRAIN_MAX_ITEMS_PER_FRAME
                        && drain_started_at.elapsed() < READY_QUEUE_DRAIN_MAX_DURATION
                    {
                        let Ok(ev) = term_events.try_recv() else {
                            break;
                        };
                        drained_events = drained_events.saturating_add(1);
                        if let Some(ev) = absorb(
                            ev,
                            &mut scroll_delta,
                            &mut scroll_row,
                            &mut scroll_col,
                        ) {
                            if self.dispatch_terminal_event(ev) {
                                break 'main;
                            }
                        }
                    }

                    if scroll_delta != 0 {
                        let _ = self.scroll_at_with_transcript_intent(
                            scroll_row,
                            scroll_col,
                            scroll_delta,
                            format!("coalesced_wheel:{scroll_delta}"),
                        );
                    }

                    self.dispatch_ui_window_events(false);
                    self.publish_diff_signals();
                    self.render_normal();
                }

                event = self.platform.receive() => {
                    self.handle_platform_event(event);
                }

                Some(output) = self.core.engine.recv_output() => {
                    self.dispatch_engine_output_in_render_loop(output);
                }

                Some(_) = self.lua.receive_wakeup() => {
                    self.lua.drain_wakeups();
                    self.flush_lua_callbacks();
                    self.drive_lua_tasks();
                    self.render_normal();
                }

                setup = async {
                    match self.auto_reload.setup.as_mut() {
                        Some(rx) => rx.await.ok(),
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some((revision, setup)) = setup {
                        self.auto_reload.apply_setup(revision, setup);
                    } else {
                        self.auto_reload.setup = None;
                    }
                }

                Some(_) = async {
                    match self.auto_reload.events.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // Drain follow-up signals so an editor that produced
                    // a fresh burst right at the boundary doesn't queue
                    // a second reload tick we'd execute immediately.
                    if let Some(rx) = self.auto_reload.events.as_mut() {
                        while rx.try_recv().is_ok() {}
                    }
                    if self.prompt_input_is_busy() || self.ui.active_modal().is_some() {
                        self.schedule_lua_config_reload();
                        continue;
                    }
                    self.reload_lua_config();
                    self.render_normal();
                }

                Some(ev) = self.overlays.next_execution_event() => {
                    let sink = self.overlays.execution_sink();
                    match ev {
                        crate::commands::ExecEvent::Output(line) => {
                            if let Some(sink) = sink {
                                self.append_shell_output(&line, sink);
                            }
                        }
                        crate::commands::ExecEvent::Done {
                            output,
                            exit_code,
                            termination,
                        } => {
                            if let Some(sink) = sink {
                                self.finish_shell_output(output, exit_code, termination, sink);
                            }
                            self.overlays.finish_execution();
                        }
                    }
                }

                _ = tokio::time::sleep(next_scheduled_frame_delay.unwrap_or(Duration::MAX)), if next_scheduled_frame_delay.is_some() => {
                    if has_animation && self.tick_drag_autoscroll_with_transcript_intent() {
                        self.dispatch_ui_window_events(false);
                    }
                    self.publish_diff_signals();
                    self.render_normal();
                }

                _ = tokio::time::sleep(next_idle_delay.unwrap_or(Duration::MAX)), if next_idle_delay.is_some() => {
                    self.tick_timers();
                    self.drive_lua_tasks();
                    self.dismiss_expired_notification();
                    self.expire_pending_keymap_chord();
                    self.publish_diff_signals();
                    self.render_normal();
                }

                Some(_) = window_change => {
                    if let Ok((w, h)) = terminal::size() {
                        if w != self.last_width || h != self.last_height {
                            self.handle_resize(w, h);
                            self.render_normal();
                        }
                    }
                }

                // Continue bounded background work only after every foreground
                // event source above had a chance to run.
                _ = std::future::ready(()), if drained_idle_work => {}
            }
        }

        let persistence_error = self.finalize_graceful_shutdown().err();

        // Stop the stdin reader before releasing terminal modes so no background
        // thread can keep consuming bytes after the TUI gives the terminal back.
        drop(term_events);

        // Release platform resources last so shutdown rendering stays in TUI mode.
        self.platform.shutdown();
        if let Some(error) = persistence_error {
            eprintln!("smelt: persistence shutdown incomplete: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_active_model(app: &mut TuiApp, model: &str, api_base: &str, provider_type: &str) {
        let active = app.core.config.active_model_mut().unwrap();
        active.model_name = model.into();
        active.api_base = api_base.into();
        active.provider_type = provider_type.into();
    }

    fn active_context_target(app: &TuiApp) -> ContextWindowTarget {
        ContextWindowTarget::from_active(app.core.config.active_model().unwrap())
    }

    #[test]
    fn worker_shutdown_does_not_wait_for_a_blocked_thread() {
        let thread = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
        });

        let started = Instant::now();
        join_background_worker(thread);

        assert!(
            started.elapsed() < Duration::from_millis(250),
            "background worker shutdown exceeded its deadline"
        );
    }

    #[test]
    fn busy_token_releases_without_a_lua_host_scope() {
        let mut stack = BusyStack::default();
        let token = stack.push_token("indexing".to_owned());

        assert!(stack.is_busy());
        assert!(token.release());
        assert!(!stack.is_busy());
    }

    #[test]
    fn busy_token_is_invalid_after_its_owner_is_replaced() {
        let mut stack = BusyStack::default();
        let token = stack.push_token("old runtime".to_owned());

        stack = BusyStack::default();

        assert!(!token.release());
        assert!(!stack.is_busy());
    }

    #[test]
    fn context_recalculation_busy_token_survives_stale_release_after_clear() {
        let mut stack = BusyStack::default();
        let cleared = stack.push_context_recalculation_token("old compaction".to_owned());
        stack.clear();
        let active = stack.push_context_recalculation_token("new compaction".to_owned());

        assert!(!cleared.release());
        assert!(stack.context_recalculating());
        assert!(active.release());
        assert!(!stack.context_recalculating());
    }

    #[test]
    fn stale_context_window_update_does_not_overwrite_current_generation() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        set_active_model(&mut app.app, "gpt-5.5", "https://codex.example", "codex");
        app.app.core.config.context_window = Some(272_000);
        let target = active_context_target(&app.app);
        let stale_revision = app
            .app
            .prepare_context_window_for_test(target.clone())
            .unwrap();
        let mut newer_target = target.clone();
        newer_target.config.max_tokens = Some(16_384);
        app.app
            .prepare_context_window_for_test(newer_target)
            .unwrap();

        app.app.apply_context_window_update(ContextWindowUpdate {
            revision: stale_revision,
            target,
            value: None,
        });

        assert_eq!(app.app.core.config.context_window, Some(272_000));
    }

    #[test]
    fn current_context_window_update_applies_even_when_value_is_none() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        set_active_model(
            &mut app.app,
            "Qwen/Qwen3.6-27B",
            "https://openai-compatible.example",
            "openai-compatible",
        );
        app.app.core.config.context_window = Some(272_000);
        let target = active_context_target(&app.app);
        let revision = app
            .app
            .prepare_context_window_for_test(target.clone())
            .unwrap();

        app.app.apply_context_window_update(ContextWindowUpdate {
            revision,
            target,
            value: None,
        });

        assert_eq!(app.app.core.config.context_window, None);
    }

    #[test]
    fn matching_revision_with_stale_model_identity_does_not_apply() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        set_active_model(&mut app.app, "gpt-5.5", "https://codex.example", "codex");
        app.app.core.config.context_window = Some(272_000);
        let desired = active_context_target(&app.app);
        let revision = app
            .app
            .prepare_context_window_for_test(desired.clone())
            .unwrap();
        let mut stale = desired;
        stale.model_key = "other/Qwen3.6-27B".into();
        stale.model = "Qwen/Qwen3.6-27B".into();
        stale.api_base = "https://openai-compatible.example".into();
        stale.provider_type = "openai-compatible".into();

        app.app.apply_context_window_update(ContextWindowUpdate {
            revision,
            target: stale,
            value: None,
        });

        assert_eq!(app.app.core.config.context_window, Some(272_000));
    }

    #[test]
    fn equal_context_window_target_does_not_start_another_revision() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let target = active_context_target(&app.app);
        let revision = app
            .app
            .prepare_context_window_for_test(target.clone())
            .unwrap();

        assert!(app.app.prepare_context_window_for_test(target).is_none());
        assert_eq!(
            app.app.runtime_controller_status().context_window,
            ControllerRevisionStatus {
                desired_revision: revision,
                observed_revision: 0,
                error: None,
            }
        );
    }
}
