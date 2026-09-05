//! Shared Lua state: the `Arc<LuaShared>` that outlives callbacks and lets tokio tasks post resume payloads.

use super::hooks::HookRegistry;
use super::{LuaHandle, LuaHandleLedger, LuaTaskRuntime, TaskEvent};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// Lua-allocated `BufId`s start here so they can never collide with the
/// Rust-side block-buffer id space (which begins at 1 and grows slowly).
/// `Ui::reap_anonymous` uses this threshold to decide which anonymous
/// bufs are safe to sweep on `/reload`.
pub const LUA_BUF_ID_BASE: u64 = 1u64 << 32;

/// Boot-time phase. Used by Lua API guards to refuse phase-sensitive
/// calls outside their valid window (e.g. `cli.register_flag` only runs
/// in `Early`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// `early.lua` is being evaluated. Only a restricted `smelt` table
    /// is visible; phase-sensitive APIs (`builtins`, `cli`, future
    /// pre-boot interceptors) are usable here.
    Early = 0,
    /// Autoload has run and `init.lua` is being evaluated. Full `smelt`
    /// surface is available.
    Init = 1,
    /// Steady state - the agent is running. Plugins added at this stage
    /// (e.g. from a `/cmd` handler) execute the same way as `Init`.
    Running = 2,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Early => "early",
            Phase::Init => "init",
            Phase::Running => "running",
        }
    }
    fn from_u8(n: u8) -> Self {
        match n {
            0 => Phase::Early,
            1 => Phase::Init,
            _ => Phase::Running,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandBusyBehavior {
    Run,
    Reject,
    QueueRequest,
    QueueCommand,
}

impl CommandBusyBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandBusyBehavior::Run => "run",
            CommandBusyBehavior::Reject => "reject",
            CommandBusyBehavior::QueueRequest => "queue_request",
            CommandBusyBehavior::QueueCommand => "queue_command",
        }
    }
}

pub struct RegisteredCommand {
    pub handle: LuaHandle,
    pub token: u64,
    pub description: Option<String>,
    pub args: Vec<String>,
    pub args_fn: Option<LuaHandle>,
    pub busy: CommandBusyBehavior,
    pub startup_ok: bool,
    /// If true, hidden from the completer but still dispatchable by name.
    pub hidden: bool,
}

pub struct RegisteredKeymap {
    pub handle: LuaHandle,
    pub description: Option<String>,
}

pub struct RegisteredWinRenderer {
    pub handle: LuaHandle,
    pub dirty: bool,
}

pub struct ToolHandles {
    pub execute: LuaHandle,
    pub execution_mode: protocol::ToolExecutionMode,
    pub approval_patterns: Option<LuaHandle>,
    pub preflight: Option<LuaHandle>,
    pub paths_for_workspace: Option<LuaHandle>,
    pub preview: Option<LuaHandle>,
    pub preview_output: Option<LuaHandle>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptGroupSpec {
    pub name: String,
    pub cache_key: Option<String>,
    pub priority: i64,
    pub registration_order: u64,
    pub min: usize,
    pub default_view: Option<String>,
    pub selector: TranscriptGroupSelector,
    pub bucket: Option<TranscriptGroupBucket>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptGroupSelector {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub names: Vec<String>,
    pub terminal: Option<bool>,
    pub fields: Vec<TranscriptGroupFieldMatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptGroupFieldMatch {
    pub field: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptGroupBucket {
    pub fields: Vec<String>,
}

pub struct RegisteredTranscriptGroup {
    pub spec: TranscriptGroupSpec,
    pub token: u64,
}

#[derive(Default)]
pub struct TranscriptGroupRegistry {
    pub entries: HashMap<String, RegisteredTranscriptGroup>,
    pub next_order: u64,
}

impl TranscriptGroupRegistry {
    pub fn specs(&self) -> Vec<TranscriptGroupSpec> {
        let mut specs: Vec<_> = self
            .entries
            .values()
            .map(|entry| entry.spec.clone())
            .collect();
        specs.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.registration_order.cmp(&b.registration_order))
                .then_with(|| a.name.cmp(&b.name))
        });
        specs
    }
}

/// All shared state between Lua closures and the app loop.
pub struct LuaShared {
    pub commands: Mutex<HashMap<String, RegisteredCommand>>,
    /// Send+Sync mirror of `commands`' key set. Host code can recognize a
    /// command without touching the `!Send` Lua handler stored with it.
    pub command_names: crate::commands::CommandNames,
    pub keymaps: Mutex<HashMap<(String, String), RegisteredKeymap>>,
    /// Canonical single-token expansion for `<leader>` in keymap registrations.
    /// Matches nvim's default leader (`\\`) unless user config sets another token.
    pub keymap_leader: Mutex<String>,
    /// Lua-registered composer for the retained main TUI layout. When `Some`,
    /// the host invokes it after registration, dimension changes, or explicit
    /// invalidation. Falls back to the hardcoded tree on error or `None`. The
    /// callback signature is `fun(state) -> Layout`.
    pub main_layout_composer: Mutex<Option<LuaHandle>>,
    /// Retained per-window render callbacks keyed by raw `WinId.0`. A callback
    /// runs after registration and after explicit invalidation, then its backing
    /// buffer remains authoritative until the next invalidation.
    pub win_renderers: Mutex<HashMap<u64, RegisteredWinRenderer>>,
    pub tools: Mutex<HashMap<String, ToolHandles>>,
    pub transcript_renderer: Mutex<Option<LuaHandle>>,
    pub transcript_renderer_generation: AtomicU64,
    pub transcript_renderer_cache_key: AtomicU64,
    pub transcript_groups: Mutex<TranscriptGroupRegistry>,
    pub transcript_groups_generation: AtomicU64,
    pub transcript_groups_cache_key: AtomicU64,
    pub callbacks: Mutex<HashMap<u64, LuaHandle>>,
    /// Callbacks registered for `smelt.engine.ask`. Separate from
    /// `callbacks` so `fire_ask_callback` can't accidentally fire a
    /// paint/win/overlay handler that happens to share an id namespace:
    /// the two maps allocate ids from the same `next_id` counter but each
    /// call_id only ever lands in one of them.
    pub ask_callbacks: Mutex<HashMap<u64, AskCallbacks>>,
    pub next_id: Arc<AtomicU64>,
    lua_handle_ledger: Arc<LuaHandleLedger>,
    pub next_registry_token: AtomicU64,
    /// Starts at `LUA_BUF_ID_BASE` so Lua-allocated `BufId`s never collide with Rust-side buffers.
    /// Shared across Lua generations so candidate allocations cannot collide
    /// with resources retained by the committed generation.
    pub next_buf_id: Arc<AtomicU64>,
    /// Lives on the shared arc (not `LuaTaskRuntime`) so a coroutine inside `drive_tasks`
    /// can mint an id without re-entering the `tasks` mutex.
    pub next_external_id: AtomicU64,
    pub tasks: Mutex<LuaTaskRuntime>,
    /// Resume-events for Lua coroutines; pumped on wakeup/tick.
    pub task_inbox: Mutex<Vec<TaskEvent>>,
    /// Cross-thread inbox: tokio tasks push `(external_id, json)`; main loop drains to `task_inbox`.
    pub json_inbox: Arc<Mutex<Vec<(u64, serde_json::Value)>>>,
    /// Session-long HTTP transport shared by replacement Lua generations.
    pub(crate) http_client: Arc<crate::http::Client>,
    /// Wakes the main loop when a coroutine resume payload is queued.
    pub wakeup_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<()>>,
    pub providers: Mutex<Vec<crate::config::ProviderConfig>>,
    pub permission_rules: Mutex<Option<crate::permissions::rules::RawPerms>>,
    pub mcp_configs: Mutex<HashMap<String, crate::mcp::McpServerConfig>>,
    /// Stable session service used by committed LSP calls.
    pub lsp: Arc<crate::lsp::LspManager>,
    /// Generation-local desired LSP declaration. Loading Lua never mutates the
    /// stable manager; the app applies this value only after commit.
    pub lsp_config: Mutex<crate::lsp::LspConfig>,
    pub settings_overrides: Mutex<HashMap<String, crate::config::SettingValue>>,
    pub defaults: Mutex<crate::config::DefaultsConfig>,
    pub remember: Mutex<crate::config::RememberConfig>,
    pub tool_defaults: Mutex<crate::permissions::rules::ToolDefaults>,
    pub messages: Arc<Mutex<crate::messages::Messages>>,
    /// Session-long clock used by transcript rendering without re-entering the
    /// scoped frontend host. Shared by replacement Lua generations.
    clock: Arc<Mutex<Arc<dyn engine::clock::Clock>>>,
    /// Bundled `smelt.<dotted>` module names the user has opted out of via
    /// `smelt.builtins.disable{}` in `early.lua`. Read by `autoload_modules`
    /// to skip the matching `require()` calls. Names use the dotted module
    /// form, e.g. `"smelt.tools.web_search"`, `"smelt.commands.compact"`.
    pub disabled_modules: Mutex<HashSet<String>>,
    /// `package.loaded` keys present right after `Lua::new()` - the
    /// stdlib. `/reload` wipes everything outside this set.
    pub native_module_names: Mutex<HashSet<String>>,
    /// CLI flag specs registered from `early.lua` via `smelt.cli.register_flag`.
    /// The main binary reads this after the early phase and re-runs clap with
    /// the merged surface.
    pub cli_flag_specs: Mutex<Vec<CliFlagSpec>>,
    /// CLI flag *values* parsed from argv after `early.lua` declared them.
    /// Populated by the main binary; read by `smelt.cli.get(name)`.
    pub cli_flag_values: Mutex<HashMap<String, CliFlagValue>>,
    /// Every callback-list surface, grouped under one struct so registration
    /// and cleanup use one shared pattern.
    pub hooks: Hooks,
    /// Default shell + argv for `smelt.process.run`/`run_streaming` when no
    /// explicit shell is given. `None` means hardcoded `sh -c`.
    pub default_shell: Mutex<Option<DefaultShell>>,
    /// Active filesystem watchers registered through `smelt.fs.watch`. The
    /// notify-backed `WatcherEntry` keeps the OS subscription alive; dropping
    /// the entry tears it down. `next_watcher_id` mints fresh ids.
    pub watchers: Mutex<HashMap<u64, crate::lua::watchers::WatcherEntry>>,
    pub next_watcher_id: AtomicU64,
    external_effects_active: AtomicBool,
    candidate_skills: Mutex<Option<Arc<engine::SkillLoader>>>,
    staged_logs: Mutex<Vec<(engine::log::Level, String, serde_json::Value)>>,
    /// Runtime-owned home used by path expansion and display callbacks.
    runtime_home: Mutex<PathBuf>,
    /// Runtime-owned project cwd used by candidate and committed generations.
    project_cwd: Mutex<PathBuf>,
    /// Current boot phase. Phase-sensitive APIs use this to gate their
    /// behavior (refuse-when-late or warn-when-late). Defaults to `Early`;
    /// the runtime promotes it to `Init` before autoload and `Running` once
    /// the agent loop is live.
    phase: AtomicU8,
    generation_id: AtomicU64,
}

/// Registry entry for one `smelt.engine.ask` request. Response and
/// streaming callbacks share a lifecycle: deltas may fire many times, and
/// the final response removes the whole entry.
pub struct AskCallbacks {
    pub response: Option<LuaHandle>,
    pub delta: Option<LuaHandle>,
}

/// Every hook-registry surface bundled into one struct. New callback streams
/// get a field here rather than a fresh `Mutex<Vec<HookEntry>>` on `LuaShared`.
#[derive(Default)]
pub struct Hooks {
    /// `provider.middleware{on_response=...}` registry.
    pub provider_response: Arc<HookRegistry>,
    /// `smelt.engine.on_context_limit(fn)` registry. Engine consults
    /// these in registration order when a provider returns a
    /// context-window error mid-turn; the first hook to return a
    /// non-nil messages array wins. Always uses `name = ""`.
    pub context_limit: Arc<HookRegistry>,
    /// `smelt.engine.on_prepare_request(fn)` registry. Engine
    /// consults these immediately before each model request so plugins
    /// can replace an oversized conversation before the provider sees it.
    /// Always uses `name = ""`.
    pub prepare_request: Arc<HookRegistry>,
    /// `smelt.lifecycle.on(event, fn)` registry. Unlike the other
    /// surfaces this one uses `drain_for` semantics - the host takes
    /// every hook matching the event name and clears them, so each
    /// hook fires at most once per launch. `name` is the event name
    /// (currently only `"ready"`).
    pub lifecycle: Arc<HookRegistry>,
}

/// Spec for a Lua-declared CLI flag. Mirrors the subset of clap we need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliFlagSpec {
    pub name: String,
    pub kind: CliFlagKind,
    pub default: CliFlagValue,
    pub description: Option<String>,
    pub short: Option<char>,
    pub long: Option<String>,
    /// String/Integer flags only. When true, the flag may appear without a
    /// value (`smelt -r` valid alongside `smelt -r abc`); the absent-value
    /// form yields `""` for String and `0` for Integer. Maps to clap's
    /// `num_args(0..=1).default_missing_value(...)`. Ignored for Boolean.
    pub value_optional: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliFlagKind {
    Boolean,
    String,
    Integer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliFlagValue {
    Boolean(bool),
    String(String),
    Integer(i64),
    None,
}

/// Default shell used by `smelt.process.run`/`run_streaming` when no
/// per-call override is provided. `program` is the executable; `args`
/// are the leading argv slots before the command string is appended
/// (e.g. `{ "-fc" }`, `{ "-c" }`).
#[derive(Clone, Debug)]
pub struct DefaultShell {
    pub program: String,
    pub args: Vec<String>,
}

/// Session-long services injected into each replaceable Lua generation.
#[derive(Clone)]
pub struct LuaHostServices {
    pub lsp: Arc<crate::lsp::LspManager>,
    pub messages: Arc<Mutex<crate::messages::Messages>>,
    pub next_buf_id: Arc<AtomicU64>,
    pub next_callback_id: Arc<AtomicU64>,
    clock: Arc<Mutex<Arc<dyn engine::clock::Clock>>>,
    lua_handle_ledger: Arc<LuaHandleLedger>,
}

impl Default for LuaHostServices {
    fn default() -> Self {
        Self {
            lsp: Arc::new(crate::lsp::LspManager::default()),
            messages: Arc::new(Mutex::new(crate::messages::Messages::new())),
            next_buf_id: Arc::new(AtomicU64::new(LUA_BUF_ID_BASE)),
            next_callback_id: Arc::new(AtomicU64::new(1)),
            clock: Arc::new(Mutex::new(Arc::new(engine::clock::RealClock))),
            lua_handle_ledger: Arc::new(LuaHandleLedger::default()),
        }
    }
}

impl Default for LuaShared {
    fn default() -> Self {
        Self {
            commands: Mutex::new(HashMap::new()),
            command_names: Arc::new(Mutex::new(HashSet::new())),
            keymaps: Mutex::new(HashMap::new()),
            keymap_leader: Mutex::new("\\".to_string()),
            main_layout_composer: Mutex::new(None),
            win_renderers: Mutex::new(HashMap::new()),
            tools: Mutex::new(HashMap::new()),
            transcript_renderer: Mutex::new(None),
            transcript_renderer_generation: AtomicU64::new(0),
            transcript_renderer_cache_key: AtomicU64::new(0),
            transcript_groups: Mutex::new(TranscriptGroupRegistry::default()),
            transcript_groups_generation: AtomicU64::new(0),
            transcript_groups_cache_key: AtomicU64::new(0),
            callbacks: Mutex::new(HashMap::new()),
            ask_callbacks: Mutex::new(HashMap::new()),
            next_id: Arc::new(AtomicU64::new(1)),
            lua_handle_ledger: Arc::new(LuaHandleLedger::default()),
            next_registry_token: AtomicU64::new(1),
            next_buf_id: Arc::new(AtomicU64::new(LUA_BUF_ID_BASE)),
            next_external_id: AtomicU64::new(1),
            tasks: Mutex::new(LuaTaskRuntime::new()),
            task_inbox: Mutex::new(Vec::new()),
            json_inbox: Arc::new(Mutex::new(Vec::new())),
            http_client: Arc::new(crate::http::Client::default()),
            wakeup_tx: std::sync::OnceLock::new(),
            providers: Mutex::new(Vec::new()),
            permission_rules: Mutex::new(None),
            mcp_configs: Mutex::new(HashMap::new()),
            lsp: Arc::new(crate::lsp::LspManager::default()),
            lsp_config: Mutex::new(crate::lsp::LspConfig {
                servers: HashMap::new(),
            }),
            settings_overrides: Mutex::new(HashMap::new()),
            defaults: Mutex::new(crate::config::DefaultsConfig::default()),
            remember: Mutex::new(crate::config::RememberConfig::default()),
            tool_defaults: Mutex::new(crate::permissions::rules::ToolDefaults::default()),
            messages: Arc::new(Mutex::new(crate::messages::Messages::new())),
            clock: Arc::new(Mutex::new(Arc::new(engine::clock::RealClock))),
            disabled_modules: Mutex::new(HashSet::new()),
            native_module_names: Mutex::new(HashSet::new()),
            cli_flag_specs: Mutex::new(Vec::new()),
            cli_flag_values: Mutex::new(HashMap::new()),
            hooks: Hooks::default(),
            default_shell: Mutex::new(None),
            watchers: Mutex::new(HashMap::new()),
            next_watcher_id: AtomicU64::new(1),
            external_effects_active: AtomicBool::new(true),
            candidate_skills: Mutex::new(None),
            staged_logs: Mutex::new(Vec::new()),
            runtime_home: Mutex::new(PathBuf::new()),
            project_cwd: Mutex::new(PathBuf::new()),
            phase: AtomicU8::new(Phase::Early as u8),
            generation_id: AtomicU64::new(0),
        }
    }
}

impl LuaShared {
    pub fn with_host_services(host: LuaHostServices) -> Self {
        Self {
            lsp: host.lsp,
            messages: host.messages,
            next_buf_id: host.next_buf_id,
            next_id: host.next_callback_id,
            clock: host.clock,
            lua_handle_ledger: host.lua_handle_ledger,
            ..Self::default()
        }
    }

    pub fn host_services(&self) -> LuaHostServices {
        LuaHostServices {
            lsp: Arc::clone(&self.lsp),
            messages: Arc::clone(&self.messages),
            next_buf_id: Arc::clone(&self.next_buf_id),
            next_callback_id: Arc::clone(&self.next_id),
            clock: Arc::clone(&self.clock),
            lua_handle_ledger: Arc::clone(&self.lua_handle_ledger),
        }
    }

    pub fn set_clock(&self, clock: Arc<dyn engine::clock::Clock>) {
        *self.clock.lock().unwrap_or_else(|error| error.into_inner()) = clock;
    }

    pub fn clock(&self) -> Arc<dyn engine::clock::Clock> {
        Arc::clone(&self.clock.lock().unwrap_or_else(|error| error.into_inner()))
    }

    pub(crate) fn lua_handle_ledger(&self) -> Arc<LuaHandleLedger> {
        Arc::clone(&self.lua_handle_ledger)
    }

    pub fn lua_handles_live(&self) -> u64 {
        self.lua_handle_ledger.live()
    }

    pub fn generation_id(&self) -> u64 {
        self.generation_id.load(Ordering::Acquire)
    }

    pub fn set_generation_id(&self, id: u64) {
        self.generation_id.store(id, Ordering::Release);
    }

    pub fn stage_external_effects(&self) {
        self.external_effects_active.store(false, Ordering::Release);
    }

    pub fn external_effects_active(&self) -> bool {
        self.external_effects_active.load(Ordering::Acquire)
    }

    pub fn set_candidate_skills(&self, skills: Arc<engine::SkillLoader>) {
        *self
            .candidate_skills
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(skills);
    }

    pub fn candidate_skills(&self) -> Option<Arc<engine::SkillLoader>> {
        self.candidate_skills
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn log_entry(&self, level: engine::log::Level, event: String, payload: serde_json::Value) {
        if self.external_effects_active() {
            engine::log::entry(level, &event, &payload);
        } else {
            self.staged_logs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((level, event, payload));
        }
    }

    pub fn commit_staged_logs(&self) {
        let logs = std::mem::take(
            &mut *self
                .staged_logs
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for (level, event, payload) in logs {
            engine::log::entry(level, &event, &payload);
        }
    }

    pub fn set_runtime_home(&self, home: &Path) {
        *self
            .runtime_home
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = home.to_path_buf();
    }

    pub fn runtime_home(&self) -> PathBuf {
        self.runtime_home
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn set_project_cwd(&self, cwd: Option<&Path>) {
        *self
            .project_cwd
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            cwd.map(Path::to_path_buf).unwrap_or_default();
    }

    /// Resolve cwd-sensitive reads against this generation's runtime project.
    pub fn evaluation_cwd(&self) -> PathBuf {
        self.project_cwd
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn resolve_project_path(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.evaluation_cwd().join(path)
        }
    }

    pub fn activate_generation_resources(&self) -> Result<(), String> {
        let mut watchers = self
            .watchers
            .lock()
            .map_err(|_| "filesystem watcher registry unavailable".to_string())?;
        for watcher in watchers.values_mut() {
            watcher.activate()?;
        }
        self.external_effects_active.store(true, Ordering::Release);
        self.candidate_skills
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        Ok(())
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }
    pub fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::Release);
    }

    fn next_registry_token(&self) -> u64 {
        self.next_registry_token.fetch_add(1, Ordering::Relaxed)
    }

    pub fn register_command(
        &self,
        name: String,
        mut command: RegisteredCommand,
        override_existing: bool,
    ) -> Result<u64, String> {
        let token = self.next_registry_token();
        command.token = token;
        let mut commands = self
            .commands
            .lock()
            .map_err(|_| "command registry unavailable".to_string())?;
        if !override_existing && commands.contains_key(&name) {
            return Err(format!(
                "command `{name}` is already registered; pass override = true to replace it"
            ));
        }
        commands.insert(name.clone(), command);
        if let Ok(mut set) = self.command_names.lock() {
            set.insert(name);
        }
        Ok(token)
    }

    pub fn unregister_command_token(&self, name: &str, token: u64) -> bool {
        let removed = self
            .commands
            .lock()
            .map(|mut commands| {
                if commands.get(name).is_some_and(|cmd| cmd.token == token) {
                    commands.remove(name);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if removed {
            if let Ok(mut set) = self.command_names.lock() {
                set.remove(name);
            }
        }
        removed
    }

    /// Drop every Lua handle from registries repopulated by `/reload`.
    pub fn clear_lua_handles(&self) {
        if let Ok(mut m) = self.commands.lock() {
            m.clear();
        }
        if let Ok(mut s) = self.command_names.lock() {
            s.clear();
        }
        if let Ok(mut m) = self.keymaps.lock() {
            m.clear();
        }
        if let Ok(mut c) = self.main_layout_composer.lock() {
            *c = None;
        }
        if let Ok(mut m) = self.win_renderers.lock() {
            m.clear();
        }
        if let Ok(mut m) = self.tools.lock() {
            m.clear();
        }
        if let Ok(mut renderer) = self.transcript_renderer.lock() {
            *renderer = None;
        }
        self.transcript_renderer_generation
            .fetch_add(1, Ordering::AcqRel);
        self.transcript_renderer_cache_key
            .store(0, Ordering::Release);
        if let Ok(mut groups) = self.transcript_groups.lock() {
            groups.entries.clear();
            groups.next_order = 0;
        }
        self.transcript_groups_generation
            .fetch_add(1, Ordering::AcqRel);
        self.transcript_groups_cache_key.store(0, Ordering::Release);
        if let Ok(mut m) = self.callbacks.lock() {
            m.clear();
        }
        if let Ok(mut m) = self.ask_callbacks.lock() {
            m.clear();
        }
        self.hooks.provider_response.clear();
        self.hooks.context_limit.clear();
        self.hooks.prepare_request.clear();
        self.hooks.lifecycle.clear();
    }

    /// Clear non-handle state whose desired value is declared by Lua config
    /// on each load cycle.
    pub fn clear_reload_scoped_config(&self) {
        if let Ok(mut m) = self.watchers.lock() {
            m.clear();
        }
        if let Ok(mut m) = self.mcp_configs.lock() {
            m.clear();
        }
        if let Ok(mut lsp) = self.lsp_config.lock() {
            lsp.servers.clear();
        }
        if let Ok(mut p) = self.providers.lock() {
            p.clear();
        }
        if let Ok(mut rules) = self.permission_rules.lock() {
            *rules = None;
        }
        if let Ok(mut settings) = self.settings_overrides.lock() {
            settings.clear();
        }
        if let Ok(mut defaults) = self.defaults.lock() {
            *defaults = crate::config::DefaultsConfig::default();
        }
        if let Ok(mut remember) = self.remember.lock() {
            *remember = crate::config::RememberConfig::default();
        }
        if let Ok(mut defaults) = self.tool_defaults.lock() {
            *defaults = crate::permissions::rules::ToolDefaults::default();
        }
        if let Ok(mut shell) = self.default_shell.lock() {
            *shell = None;
        }
        if let Ok(mut leader) = self.keymap_leader.lock() {
            *leader = "\\".to_string();
        }
    }

    pub fn clear_for_reload(&self) {
        self.clear_lua_handles();
        self.clear_reload_scoped_config();
    }
}

impl LuaShared {
    /// `Send`-safe handle for pushing JSON resume payloads from tokio tasks.
    /// `Arc<LuaShared>` is `!Send` (owns `mlua::Thread`s); this is the narrowest cross-thread surface.
    pub fn resume_sink(&self) -> LuaResumeSink {
        LuaResumeSink {
            inbox: Arc::clone(&self.json_inbox),
            wakeup: self.wakeup_tx.get().cloned(),
        }
    }
}

/// `Send`-safe handle for resuming a parked Lua coroutine from a tokio task.
#[derive(Clone)]
pub struct LuaResumeSink {
    inbox: Arc<Mutex<Vec<(u64, serde_json::Value)>>>,
    wakeup: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

impl LuaResumeSink {
    pub fn resolve_json(&self, external_id: u64, value: serde_json::Value) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.push((external_id, value));
        }
        if let Some(ref tx) = self.wakeup {
            let _ = tx.send(());
        }
    }

    /// Run `work` on tokio's blocking-thread pool and resolve `task_id`
    /// with the produced JSON payload. The common bridge for sync work
    /// (file I/O, blocking process calls) that needs to surface back to
    /// a yielded Lua coroutine without blocking the main loop.
    pub fn spawn_blocking_resolve<F>(self, task_id: u64, work: F)
    where
        F: FnOnce() -> serde_json::Value + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let payload = work();
            self.resolve_json(task_id, payload);
        });
    }
}
