//! Headless-safe Lua runtime. The TUI extends this with UI-specific queues and statusline rendering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use include_dir::{include_dir, Dir, DirEntry};
use mlua::prelude::*;

use crate::content::block_layout::{
    BlockLayout, ContentRenderSpec, ContentSpec, LuaLeaf, TextSpec,
};
use crate::content::display_safe_text;
#[cfg(test)]
use crate::content::markdown_stream::thinking_summary;
use crate::lua::{
    json_to_lua, LuaShared, LuaToolResultParts, TaskCompletion, TaskDriveOutput, TaskEvent,
    ToolCallIds, ToolEnv, ToolExecResult, TranscriptGroupSpec,
};
use crate::permissions::{PathTargetKind, ToolPath};
use crate::transcript_model::{Block, BlockId, ToolOutput, ToolOutputRef, ToolState, ToolStatus};

const DEFAULT_TOOL_TIMEOUT_MS: u64 = 30_000;
const MAX_TOOL_TIMEOUT_MS: u64 = 600_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolWorkspacePaths {
    Undeclared,
    Declared(Vec<ToolPath>),
}

impl ToolWorkspacePaths {
    pub fn as_slice(&self) -> &[ToolPath] {
        match self {
            Self::Undeclared => &[],
            Self::Declared(paths) => paths,
        }
    }

    pub fn into_paths(self) -> Vec<ToolPath> {
        match self {
            Self::Undeclared => Vec::new(),
            Self::Declared(paths) => paths,
        }
    }
}

pub struct ShutdownHookContext<'a> {
    pub session_id: &'a str,
    pub has_messages: bool,
    pub ephemeral: bool,
}

/// Embedded `runtime/lua/smelt/` tree; every `.lua` file is `require`-able as `smelt.<path>`.
static EMBEDDED_LUA: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../runtime/lua/smelt");

/// Embedded `runtime/skills/` tree; extracted alongside Lua built-ins so
/// built-in skills have real on-disk locations for `/skills` and `load_skill`.
static EMBEDDED_SKILLS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../runtime/skills");

/// One bundled bootstrap chunk. Full bootstrap loads every chunk; Host
/// bootstrap filters this manifest through `loads_in_host`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootstrapChunk {
    pub path: &'static str,
    loads_in_host: bool,
    pub default_tier: crate::lua::doc::Tier,
}

impl BootstrapChunk {
    const fn host(path: &'static str) -> Self {
        Self::host_with_tier(path, crate::lua::doc::Tier::Host)
    }

    const fn host_with_tier(path: &'static str, default_tier: crate::lua::doc::Tier) -> Self {
        Self {
            path,
            loads_in_host: true,
            default_tier,
        }
    }

    const fn ui(path: &'static str) -> Self {
        Self {
            path,
            loads_in_host: false,
            default_tier: crate::lua::doc::Tier::UiHost,
        }
    }

    pub const fn loads_in_host(self) -> bool {
        self.loads_in_host
    }
}

/// Bundled chunks executed at bootstrap time, in dependency order.
pub const BOOTSTRAP_CHUNKS: &[BootstrapChunk] = &[
    BootstrapChunk::host("_bootstrap.lua"),
    BootstrapChunk::host("transcript/defaults.lua"),
    BootstrapChunk::host("transcript.lua"),
    BootstrapChunk::ui("dialog.lua"),
    BootstrapChunk::ui("list.lua"),
    BootstrapChunk::ui("session.lua"),
    BootstrapChunk::ui("widgets/picker.lua"),
    BootstrapChunk::host_with_tier("widgets/completer.lua", crate::lua::doc::Tier::UiHost),
    BootstrapChunk::ui("widgets/prompt_picker.lua"),
    BootstrapChunk::host("cmd.lua"),
    BootstrapChunk::ui("label_value.lua"),
    BootstrapChunk::ui("dialogs/confirm.lua"),
    BootstrapChunk::ui("_bar.lua"),
    BootstrapChunk::ui("tips.lua"),
    BootstrapChunk::ui("prompt_bar.lua"),
    BootstrapChunk::ui("statusline.lua"),
    BootstrapChunk::ui("layout.lua"),
    BootstrapChunk::host("modes.lua"),
];

/// Subdirectories whose files are `require`'d at startup as side-effect registrations.
const AUTOLOAD_DIRS: &[&str] = &["tools", "commands", "completers", "plugins", "dialogs"];
const HOST_AUTOLOAD_DIRS: &[&str] = &["tools"];

/// Lua helper for filesystem-backed markdown slash commands. The regular
/// autoloader skips it; startup calls it explicitly after built-in commands
/// so `override: true` in command frontmatter can replace built-ins.
const CUSTOM_COMMANDS_MODULE: &str = "smelt.commands.custom_commands";

/// Subdirectory whose files run during the Early phase under the restricted
/// `smelt` view, BEFORE user `early.lua`. Plugins drop a file here to declare
/// CLI flags (`smelt.cli.register_flag{}`) or opt out of bundled modules
/// (`smelt.builtins.disable{}`).
const EARLY_DIRS: &[&str] = &["early"];

/// Bundled plugins that ship with smelt but are NOT autoloaded. Users opt in by
/// calling `require("smelt.plugins.<name>")` from their `init.lua`. Exposed
/// so the `gen-lua-docs` xtask can emit an opt-in vs autoload table in the
/// `customize` skill.
pub const OPTIONAL_PLUGINS: &[&str] = &[
    "smelt.plugins.which_key",
    "smelt.plugins.inspect",
    "smelt.plugins.lsp",
];

/// Command metadata used by command-line completion UIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCompletionItem {
    pub name: String,
    pub description: Option<String>,
}

/// Outcome of dispatching a keymap chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapResult {
    /// Handler ran and returned truthy or nothing.
    Consumed,
    /// Handler ran and returned `false`; key falls through.
    PassThrough,
    NoBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapPrefix {
    pub mode: String,
    pub chord: String,
    pub suffix: String,
    pub description: Option<String>,
}

fn keymap_mode_char(mode: &str) -> Option<&'static str> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | "*" | "any" | "all" => Some(""),
        "n" | "normal" => Some("n"),
        "i" | "insert" => Some("i"),
        "v" | "visual" | "visual_line" | "visualline" => Some("v"),
        _ => match mode {
            "Normal" => Some("n"),
            "Insert" => Some("i"),
            "Visual" | "VisualLine" => Some("v"),
            _ => None,
        },
    }
}

fn dispatch_mode_char(current_mode: Option<&str>) -> Option<&'static str> {
    current_mode.map(|mode| keymap_mode_char(mode).unwrap_or("n"))
}

fn query_mode_char(current_mode: Option<&str>) -> &'static str {
    current_mode.and_then(keymap_mode_char).unwrap_or("n")
}

fn keymap_mode_matches(binding_mode: &str, active_mode: &str) -> bool {
    binding_mode.is_empty() || binding_mode == active_mode
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootstrapMode {
    Host,
    Full,
}

impl BootstrapMode {
    fn includes(self, chunk: &BootstrapChunk) -> bool {
        self == Self::Full || chunk.loads_in_host()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolVisibility {
    Interactive,
    Headless,
}

impl ToolVisibility {
    fn includes(self, headless: bool) -> bool {
        match self {
            Self::Interactive => true,
            Self::Headless => headless,
        }
    }
}

#[derive(Clone)]
struct LuaLaunchInputs {
    disabled_modules: std::collections::HashSet<String>,
    cli_flag_specs: Vec<crate::lua::CliFlagSpec>,
    cli_flag_values: HashMap<String, crate::lua::CliFlagValue>,
}

#[derive(Clone)]
struct LuaLoadPaths {
    home: PathBuf,
    config_dir: PathBuf,
    state_root: PathBuf,
    cache_root: PathBuf,
    runtime_override: Option<PathBuf>,
    development_runtime: Option<PathBuf>,
    project_cwd: Option<PathBuf>,
    data_runtime: PathBuf,
}

impl LuaLoadPaths {
    fn development_runtime() -> Option<PathBuf> {
        if !cfg!(debug_assertions) {
            return None;
        }
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("runtime")
            .join("lua");
        path.is_dir().then_some(path)
    }

    fn from_process() -> Self {
        Self {
            home: engine::paths::home_dir(),
            config_dir: crate::config::config_dir(),
            state_root: crate::config::state_dir(),
            cache_root: engine::paths::cache_dir(),
            runtime_override: std::env::var_os("SMELT_RUNTIME_DIR").map(PathBuf::from),
            development_runtime: Self::development_runtime(),
            project_cwd: std::env::current_dir().ok(),
            data_runtime: engine::data_dir().join("runtime"),
        }
    }

    fn from_runtime(
        env: &engine::env::RuntimeEnv,
        config_dir: Option<PathBuf>,
        runtime_override: Option<PathBuf>,
        project_cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            home: env.home().clone(),
            config_dir: config_dir.unwrap_or_else(|| env.config_dir().clone()),
            state_root: env.state_dir().clone(),
            cache_root: env.cache_dir().clone(),
            runtime_override,
            development_runtime: Self::development_runtime(),
            project_cwd: project_cwd.or_else(|| Some(env.cwd())),
            data_runtime: env.data_dir().join("runtime"),
        }
    }

    fn for_target_cwd(&self, cwd: Option<&std::path::Path>) -> Self {
        let mut paths = self.clone();
        paths.project_cwd = cwd.map(std::path::Path::to_path_buf);
        paths
    }

    /// Module overrides in priority order. Only `development_runtime` is trusted
    /// to receive bundled implementation capabilities.
    fn module_overlay_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(path) = &self.runtime_override {
            roots.push(path.clone());
        }
        if let Some(path) = &self.development_runtime {
            roots.push(path.clone());
        }
        if let Some(cwd) = &self.project_cwd {
            roots.push(cwd.join(".smelt").join("runtime"));
        }
        roots.push(self.config_dir.join("lua"));
        roots.push(self.data_runtime.clone());
        roots
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LuaLoadFailureLocation {
    pub phase: &'static str,
    pub path: Option<PathBuf>,
}

/// Headless-safe Lua runtime.
pub struct LuaRuntime {
    pub lua: Lua,
    pub load_error: Option<String>,
    load_failure_location: Option<LuaLoadFailureLocation>,
    shared: Arc<LuaShared>,
    init_lua_path: Option<PathBuf>,
    bootstrap_mode: BootstrapMode,
    load_paths: LuaLoadPaths,
    launch_inputs: Option<LuaLaunchInputs>,
    load_warnings: Vec<String>,
    loaded_files: Arc<Mutex<Vec<PathBuf>>>,
}

/// Owned handle for executing callbacks from an existing Lua generation.
///
/// It shares the Lua VM and synchronized registries. Frontends create it before
/// mutably lending their host, which keeps callback execution disjoint from the
/// runtime controller stored inside that host.
pub struct LuaExecution {
    runtime: LuaRuntime,
}

/// Launch-only owner that finishes loading the generation-zero VM while the
/// frontend retains access to the same VM and synchronized registries.
///
/// Load diagnostics, warnings, and other mutable bookkeeping belong only to
/// this owner and are not reentrantly observable through the frontend runtime.
/// The owner is consumed back into the frontend before that bookkeeping is
/// read. Generation zero performs live startup effects and is never a candidate.
pub struct LuaLaunch {
    runtime: LuaRuntime,
}

impl std::ops::Deref for LuaExecution {
    type Target = LuaRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl LuaLaunch {
    pub fn load_full_bootstrap(&mut self) {
        self.runtime.load_full_bootstrap();
    }

    pub fn load_autoload(&mut self) {
        self.runtime.load_autoload();
    }

    pub fn load_user_config(&mut self) {
        self.runtime.load_user_config();
    }

    pub fn load_global_plugins(&mut self) {
        self.runtime.load_global_plugins();
    }

    pub fn load_project_config(&mut self, cwd: &std::path::Path) -> crate::trust::TrustState {
        self.runtime.load_project_config(cwd)
    }

    pub fn finish(self) -> LuaRuntime {
        self.runtime
    }
}

impl Default for LuaRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaRuntime {
    /// Build a fresh runtime and register the `smelt` global with
    /// Host-tier APIs only.
    pub fn new() -> Self {
        #[allow(clippy::arc_with_non_send_sync)]
        let shared = Arc::new(LuaShared::default());
        Self::with_host_bootstrap(shared, LuaLoadPaths::from_process())
    }

    pub fn with_shared(shared: Arc<LuaShared>) -> Self {
        Self::with_host_bootstrap(shared, LuaLoadPaths::from_process())
    }

    /// Build a runtime from a captured process environment without evaluating
    /// bootstrap chunks. The owning frontend loads the appropriate bootstrap
    /// exactly once after its host capabilities are available.
    pub fn with_shared_for_runtime(
        shared: Arc<LuaShared>,
        env: &engine::env::RuntimeEnv,
        config_dir: Option<PathBuf>,
        runtime_override: Option<PathBuf>,
        project_cwd: Option<PathBuf>,
    ) -> Self {
        Self::with_shared_and_paths(
            shared,
            LuaLoadPaths::from_runtime(env, config_dir, runtime_override, project_cwd),
        )
    }

    fn with_host_bootstrap(shared: Arc<LuaShared>, load_paths: LuaLoadPaths) -> Self {
        let mut runtime = Self::with_shared_and_paths(shared, load_paths);
        runtime.load_bootstrap();
        runtime
    }

    fn with_shared_and_paths(shared: Arc<LuaShared>, load_paths: LuaLoadPaths) -> Self {
        shared.set_runtime_home(&load_paths.home);
        shared.set_project_cwd(load_paths.project_cwd.as_deref());
        let lua = Lua::new();
        lua.set_app_data(shared.lua_handle_ledger());
        let load_error = Self::register_api(
            &lua,
            &shared,
            &load_paths.state_root,
            &load_paths.cache_root,
        )
        .err()
        .map(|error| error.to_string());
        let load_failure_location = load_error.as_ref().map(|_| LuaLoadFailureLocation {
            phase: "api_registration",
            path: None,
        });
        let mut runtime = Self {
            lua,
            load_error,
            load_failure_location,
            shared,
            init_lua_path: None,
            bootstrap_mode: BootstrapMode::Host,
            load_paths,
            launch_inputs: None,
            load_warnings: Vec::new(),
            loaded_files: Arc::new(Mutex::new(Vec::new())),
        };

        if runtime.load_error.is_none() {
            let roots = runtime.load_paths.module_overlay_roots();
            if let Err(error) = register_module_searcher_with_roots(
                &runtime.lua,
                roots,
                runtime.load_paths.development_runtime.clone(),
                Some(Arc::clone(&runtime.loaded_files)),
            ) {
                runtime.set_load_error(
                    "module_searcher",
                    None,
                    format!("embedded searcher: {error}"),
                );
            }
        }
        runtime.snapshot_native_modules();
        runtime
    }

    pub fn fresh_with_shared(
        &self,
        shared: Arc<LuaShared>,
        target_cwd: Option<&std::path::Path>,
    ) -> Self {
        Self::with_shared_and_paths(shared, self.load_paths.for_target_cwd(target_cwd))
    }

    fn clone_for_execution(&self) -> Self {
        Self {
            lua: self.lua.clone(),
            load_error: self.load_error.clone(),
            load_failure_location: self.load_failure_location.clone(),
            shared: Arc::clone(&self.shared),
            init_lua_path: self.init_lua_path.clone(),
            bootstrap_mode: self.bootstrap_mode,
            load_paths: self.load_paths.clone(),
            launch_inputs: self.launch_inputs.clone(),
            load_warnings: self.load_warnings.clone(),
            loaded_files: Arc::clone(&self.loaded_files),
        }
    }

    /// Clone the VM-facing portion needed for synchronous callback execution.
    pub fn execution(&self) -> LuaExecution {
        LuaExecution {
            runtime: self.clone_for_execution(),
        }
    }

    /// Transfer generation-zero loading to a launch owner.
    ///
    /// The owner shares the existing VM and registries. It must be consumed
    /// with [`LuaLaunch::finish`] and installed back into the frontend before
    /// normal callback execution begins.
    pub fn continue_launch(&self) -> LuaLaunch {
        LuaLaunch {
            runtime: self.clone_for_execution(),
        }
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn load_failure_location(&self) -> Option<&LuaLoadFailureLocation> {
        self.load_failure_location.as_ref()
    }

    fn set_load_error(&mut self, phase: &'static str, path: Option<PathBuf>, message: String) {
        self.load_error = Some(message);
        self.load_failure_location = Some(LuaLoadFailureLocation { phase, path });
    }

    pub fn set_init_lua_path(&mut self, path: PathBuf) {
        self.init_lua_path = Some(path);
    }

    pub fn enable_ui_bootstrap(&mut self) {
        self.bootstrap_mode = BootstrapMode::Full;
    }

    pub fn load_host_bootstrap(&mut self) {
        self.bootstrap_mode = BootstrapMode::Host;
        self.load_bootstrap();
    }

    pub fn load_full_bootstrap(&mut self) {
        self.bootstrap_mode = BootstrapMode::Full;
        self.load_bootstrap();
    }

    fn current_launch_inputs(&self) -> LuaLaunchInputs {
        LuaLaunchInputs {
            disabled_modules: self
                .shared
                .disabled_modules
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            cli_flag_specs: self
                .shared
                .cli_flag_specs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            cli_flag_values: self
                .shared
                .cli_flag_values
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        }
    }

    pub fn freeze_launch_inputs(&mut self) {
        self.launch_inputs = Some(self.current_launch_inputs());
    }

    /// Copy immutable launch inputs into a fresh candidate generation.
    pub fn inherit_launch_inputs(&mut self, committed: &Self) {
        self.init_lua_path = committed.init_lua_path.clone();
        self.bootstrap_mode = committed.bootstrap_mode;
        let launch = committed
            .launch_inputs
            .clone()
            .unwrap_or_else(|| committed.current_launch_inputs());
        *self
            .shared
            .disabled_modules
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = launch.disabled_modules.clone();
        *self
            .shared
            .cli_flag_specs
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = launch.cli_flag_specs.clone();
        *self
            .shared
            .cli_flag_values
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = launch.cli_flag_values.clone();
        self.launch_inputs = Some(launch);
    }

    pub fn take_load_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.load_warnings)
    }

    pub fn configured_init_lua_path(&self) -> Option<&std::path::Path> {
        self.init_lua_path.as_deref()
    }

    pub fn state_root(&self) -> &std::path::Path {
        &self.load_paths.state_root
    }

    pub fn cache_root(&self) -> &std::path::Path {
        &self.load_paths.cache_root
    }

    pub fn project_trust_state(&self, cwd: &std::path::Path) -> crate::trust::TrustState {
        crate::trust::TrustStore::new(self.load_paths.state_root.clone()).project_trust_state(cwd)
    }

    pub fn mark_project_trusted(&self, cwd: &std::path::Path) -> Result<String, String> {
        crate::trust::TrustStore::new(self.load_paths.state_root.clone()).mark_trusted(cwd)
    }

    pub fn loaded_config_files(&self) -> Vec<PathBuf> {
        let mut files = self
            .loaded_files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        files.sort();
        files.dedup();
        files
    }

    fn record_loaded_file(&self, path: PathBuf) {
        self.loaded_files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(path);
    }

    pub fn load_manifest_roots(&self, target_cwd: Option<&std::path::Path>) -> Vec<PathBuf> {
        let mut roots = self.load_paths.module_overlay_roots();
        roots.push(self.load_paths.config_dir.clone());
        if let Some(cwd) = target_cwd {
            roots.push(cwd.join(".smelt"));
        }
        roots.sort();
        roots.dedup();
        roots
    }

    fn load_bootstrap(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let roots = self.load_paths.module_overlay_roots();
        let result = load_bootstrap_group_with_roots(
            &self.lua,
            self.bootstrap_mode,
            &roots,
            self.load_paths.development_runtime.as_deref(),
            Some(&self.loaded_files),
        );
        if let Err(error) = result {
            self.set_load_error("bootstrap", None, format!("bootstrap: {error}"));
        }
    }

    pub fn load_user_config(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let path = self
            .init_lua_path
            .clone()
            .unwrap_or_else(|| self.load_paths.config_dir.join("init.lua"));
        if path.exists() {
            if let Err(error) = self.load_init(&path) {
                let label = self
                    .init_lua_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "~/.config/smelt/init.lua".to_string());
                self.set_load_error("user", Some(path), format!("{label}: {error}"));
            }
        }
    }

    /// Run every bundled `runtime/lua/smelt/early/*.lua` file during the
    /// Early phase under the restricted `smelt` view. Intended for plugins
    /// shipped with smelt that want to declare a CLI flag via
    /// `smelt.cli.register_flag{}` (so argv parsing picks it up) without
    /// forcing every user to edit their `early.lua`. Runs BEFORE
    /// [`Self::load_early_init`] so user code can override.
    pub fn load_bundled_early(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let modules = early_modules();
        if modules.is_empty() {
            return;
        }
        let result = self.with_early_smelt(|this| {
            for name in &modules {
                let code = format!("require('{name}')");
                this.lua
                    .load(&code)
                    .set_name(name.as_str())
                    .exec()
                    .map_err(|e| LuaError::RuntimeError(format!("bundled early {name}: {e}")))?;
            }
            Ok(())
        });
        if let Err(e) = result {
            self.set_load_error("early", None, format!("bundled early init: {e}"));
        }
    }

    /// Evaluate `~/.config/smelt/early.lua` if present. Intended to run
    /// BEFORE [`autoload_modules`] so a user can call
    /// `smelt.builtins.disable{}` / `smelt.cli.register_flag{}` and
    /// have them take effect on the upcoming auto-load and argv parse.
    /// During evaluation the global `smelt` table is swapped for a
    /// restricted view exposing only the phase-zero namespaces
    /// (`builtins`, `cli`, `phase`, `provider`). Calls to any other
    /// namespace error with a clear message. The full `smelt` is
    /// restored afterward. No-op when the file is missing.
    pub fn load_early_init(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let path = self.load_paths.config_dir.join("early.lua");
        if !path.exists() {
            return;
        }
        if let Err(e) = self.run_early_phase(&path, "early.lua") {
            self.set_load_error(
                "early",
                Some(path.clone()),
                format!("{}: {e}", path.display()),
            );
        }
    }

    /// Evaluate `.smelt/early.lua` if present and the project is trusted.
    /// Companion to [`Self::load_early_init`] for project-scoped early
    /// config. No-op when the file is missing or the project is not
    /// trusted.
    pub fn load_project_early_init(&mut self, cwd: &std::path::Path) {
        if self.load_error.is_some() {
            return;
        }
        let state = self.project_trust_state(cwd);
        if !matches!(state, crate::trust::TrustState::Trusted { .. }) {
            return;
        }
        let path = cwd.join(".smelt").join("early.lua");
        if !path.exists() {
            return;
        }
        if let Err(e) = self.run_early_phase(&path, ".smelt/early.lua") {
            self.set_load_error(
                "early",
                Some(path.clone()),
                format!("{}: {e}", path.display()),
            );
        }
    }

    fn run_early_phase(&mut self, path: &std::path::Path, name: &str) -> LuaResult<()> {
        self.record_loaded_file(path.to_path_buf());
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read {name}: {e}")))?;
        self.with_early_smelt(|this| this.lua.load(&src).set_name(name).exec())
    }

    /// Swap the global `smelt` for the restricted Early-phase view, set the
    /// phase, run `body`, then restore the full `smelt` regardless of
    /// outcome. The single place that owns the Early-phase smelt-view
    /// contract - every early-phase loader (`run_early_phase`,
    /// `load_bundled_early`) routes through here. Body errors win over
    /// restore errors so a real user mistake isn't masked by table-restore
    /// noise.
    fn with_early_smelt<F, R>(&mut self, body: F) -> LuaResult<R>
    where
        F: FnOnce(&mut Self) -> LuaResult<R>,
    {
        let full_smelt: mlua::Table = self.lua.globals().get("smelt")?;
        let restricted = self.build_early_smelt_view(&full_smelt)?;
        self.lua.globals().set("smelt", restricted)?;
        self.shared.set_phase(crate::lua::Phase::Early);
        let body_result = body(self);
        let restore_result = self.lua.globals().set("smelt", full_smelt);
        match (body_result, restore_result) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }

    /// Build the restricted `smelt` table seen by `early.lua`. Exposes
    /// only the phase-zero namespaces; access to anything else returns
    /// nil, so calling e.g. `smelt.cmd.register(...)` from early.lua
    /// errors with "attempt to call nil" - loud, immediate, traceable.
    fn build_early_smelt_view(&self, full: &mlua::Table) -> LuaResult<mlua::Table> {
        const ALLOWED: &[&str] = &["builtins", "cli", "phase", "provider"];
        let view = self.lua.create_table()?;
        for ns in ALLOWED {
            if let Ok(v) = full.get::<mlua::Value>(*ns) {
                view.set(*ns, v)?;
            }
        }
        Ok(view)
    }

    /// Snapshot of the modules a user has disabled via `smelt.builtins.disable`.
    pub fn disabled_modules(&self) -> std::collections::HashSet<String> {
        self.shared
            .disabled_modules
            .lock()
            .map(|set| set.clone())
            .unwrap_or_default()
    }

    /// Promote the boot phase. The host loop should call:
    /// 1. nothing (`Early` is the default at construction)
    /// 2. `mark_init()` after `early.lua` has run and before autoload
    /// 3. `mark_running()` once the agent loop is live
    pub fn mark_init(&self) {
        self.shared.set_phase(crate::lua::Phase::Init);
    }
    pub fn mark_running(&self) {
        self.shared.set_phase(crate::lua::Phase::Running);
    }
    pub fn current_phase(&self) -> crate::lua::Phase {
        self.shared.phase()
    }

    /// Stash the stdlib keys in `package.loaded` so `/reload` knows what
    /// not to wipe. Called once from the constructor.
    fn snapshot_native_modules(&self) {
        let names: std::collections::HashSet<String> = (|| -> LuaResult<_> {
            let package: mlua::Table = self.lua.globals().get("package")?;
            let loaded: mlua::Table = package.get("loaded")?;
            let mut out = std::collections::HashSet::new();
            for (k, _) in loaded.pairs::<String, mlua::Value>().flatten() {
                out.insert(k);
            }
            Ok(out)
        })()
        .unwrap_or_default();
        if let Ok(mut set) = self.shared.native_module_names.lock() {
            *set = names;
        }
    }

    /// Nil out every `package.loaded` entry outside the stdlib snapshot
    /// so the next `require()` re-runs the module body.
    fn wipe_loaded_modules(&self) {
        let snapshot = match self.shared.native_module_names.lock() {
            Ok(s) => s.clone(),
            Err(_) => return,
        };
        let _ = (|| -> LuaResult<()> {
            let package: mlua::Table = self.lua.globals().get("package")?;
            let loaded: mlua::Table = package.get("loaded")?;
            let mut to_drop = Vec::new();
            for (k, _) in loaded.pairs::<String, mlua::Value>().flatten() {
                if !snapshot.contains(&k) {
                    to_drop.push(k);
                }
            }
            for k in to_drop {
                loaded.set(k, mlua::Value::Nil)?;
            }
            Ok(())
        })();
    }

    /// Move phase from `Early` to `Init`, then `require` every autoload
    /// module (modulo `smelt.builtins.disable{}` opt-outs from
    /// `early.lua`). First failure short-circuits onto `load_error`.
    pub fn load_autoload(&mut self) {
        self.load_autoload_from(AUTOLOAD_DIRS, true);
    }

    /// Load only bundled Host modules for a headless process. UI commands,
    /// completers, plugins, dialogs, and filesystem-backed slash commands stay
    /// unloaded because their registration paths require a terminal UI.
    pub fn load_host_autoload(&mut self) {
        self.load_autoload_from(HOST_AUTOLOAD_DIRS, false);
    }

    fn load_autoload_from(&mut self, directories: &[&str], load_commands: bool) {
        if self.load_error.is_some() {
            return;
        }
        self.mark_init();
        let disabled = self.disabled_modules();
        for name in autoload_modules_filtered_from(directories, &disabled) {
            if let Err(e) = self.require_module(&name) {
                self.set_load_error("autoload", None, format!("autoload {name}: {e}"));
                return;
            }
        }
        if load_commands {
            self.load_global_commands();
        }
    }

    fn load_global_commands(&mut self) {
        self.load_command_dir("register_global", "global commands");
    }

    fn load_project_commands(&mut self) {
        self.load_command_dir("register_project", "project commands");
    }

    fn load_command_dir(&mut self, function_name: &str, label: &str) {
        if self.disabled_modules().contains(CUSTOM_COMMANDS_MODULE) {
            return;
        }
        let result: LuaResult<()> = (|| {
            let module: mlua::Table = self
                .lua
                .load(format!("return require('{CUSTOM_COMMANDS_MODULE}')"))
                .set_name(CUSTOM_COMMANDS_MODULE)
                .eval()?;
            let register: mlua::Function = module.get(function_name)?;
            register.call(())
        })();
        if let Err(e) = result {
            self.set_load_error("commands", None, format!("{label}: {e}"));
        }
    }

    fn require_module(&self, name: &str) -> LuaResult<()> {
        let code = format!("require('{name}')");
        self.lua.load(&code).set_name(name).exec()
    }

    /// Flush dirty `smelt.state.persistent` entries before reload clears the
    /// timers that would otherwise perform debounced saves.
    pub fn flush_persistent_state(&self) -> Option<String> {
        let result: LuaResult<()> = (|| {
            let flush = crate::lua::module::internal_api_function(
                &self.lua,
                "smelt",
                "__flush_persistent_state",
            )?;
            flush.call(())
        })();
        result.err().map(|e| e.to_string())
    }

    /// Clear every Lua-owned registry, wipe non-stdlib `package.loaded`,
    /// re-run bootstrap (idempotent), then re-run autoload → user init →
    /// global plugins → project config. After loading, sweep stale
    /// `smelt.state` slots no plugin touched this cycle. `early.lua` is
    /// skipped (CLI flags and `builtins.disable` are startup-only).
    /// Returns any load error.
    pub fn reload(&mut self, cwd: Option<&std::path::Path>) -> Option<String> {
        self.reload_inner(cwd, None, false)
    }

    /// Load a fresh generation while carrying JSON-compatible `smelt.state`
    /// values from the committed runtime. Lua functions, userdata, and handles
    /// remain generation-owned and are intentionally not transferred.
    pub fn reload_with_state(
        &mut self,
        cwd: Option<&std::path::Path>,
        state: Option<&serde_json::Value>,
    ) -> Option<String> {
        self.reload_inner(cwd, state, true)
    }

    fn reload_inner(
        &mut self,
        cwd: Option<&std::path::Path>,
        state: Option<&serde_json::Value>,
        candidate: bool,
    ) -> Option<String> {
        self.load_error = None;
        self.load_failure_location = None;
        self.load_warnings.clear();
        self.loaded_files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        let launch = candidate.then(|| {
            self.launch_inputs
                .clone()
                .unwrap_or_else(|| self.current_launch_inputs())
        });
        self.clear_for_reload();
        if candidate {
            self.shared
                .disabled_modules
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
            self.shared
                .cli_flag_specs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
        }
        if candidate {
            crate::lua::doc::begin_candidate_load(&self.lua);
            if let Err(error) = self.install_candidate_stdlib_guards() {
                self.set_load_error(
                    "candidate_guards",
                    None,
                    format!("candidate standard-library guards: {error}"),
                );
                return self.load_error.clone();
            }
        }
        if let Some(state) = state {
            if let Err(error) = self.install_state_snapshot(state) {
                self.set_load_error(
                    "state_restore",
                    None,
                    format!("restore smelt.state: {error}"),
                );
                return self.load_error.clone();
            }
        }
        self.load_bootstrap();
        if self.load_error.is_some() {
            return self.load_error.clone();
        }
        if candidate {
            self.load_bundled_early();
            self.load_early_init();
            if let Some(cwd) = cwd {
                self.load_project_early_init(cwd);
            }
            if self.load_error.is_some() {
                return self.load_error.clone();
            }

            let candidate_disabled = self
                .shared
                .disabled_modules
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let candidate_specs = self
                .shared
                .cli_flag_specs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let launch = launch.as_ref().expect("candidate launch inputs");
            if candidate_disabled != launch.disabled_modules
                || candidate_specs != launch.cli_flag_specs
            {
                self.load_warnings.push(
                    "early.lua launch declarations changed; restart smelt to apply CLI flag or builtin module changes"
                        .to_string(),
                );
            }
            *self
                .shared
                .disabled_modules
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = launch.disabled_modules.clone();
            let candidate_names: std::collections::HashSet<&str> = candidate_specs
                .iter()
                .map(|spec| spec.name.as_str())
                .collect();
            let mut values = launch.cli_flag_values.clone();
            values.retain(|name, _| candidate_names.contains(name.as_str()));
            *self
                .shared
                .cli_flag_values
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = values;
        }
        self.load_autoload();
        self.load_user_config();
        self.load_global_plugins();
        if let Some(cwd) = cwd {
            let _ = self.load_project_config(cwd);
        }
        if let Ok(sweep) =
            crate::lua::module::internal_api_function(&self.lua, "smelt", "__sweep_state")
        {
            let _ = sweep.call::<()>(());
        }
        self.load_error.clone()
    }

    pub fn state_snapshot(&self) -> serde_json::Value {
        let state = crate::lua::module::internal_api_root(&self.lua)
            .and_then(|internal| internal.raw_get::<mlua::Value>("__state"));
        let value = state.and_then(|state| {
            self.lua
                .load(
                    r#"
                    return function(value)
                        local seen = {}
                        local function snapshot(item)
                            local kind = type(item)
                            if kind == "nil" or kind == "boolean" or kind == "number" or kind == "string" then
                                return item
                            end
                            if kind ~= "table" or seen[item] then
                                return nil
                            end
                            seen[item] = true
                            local copy = {}
                            for key, child in pairs(item) do
                                local key_kind = type(key)
                                if key_kind == "string" or key_kind == "number" then
                                    local copied = snapshot(child)
                                    if copied ~= nil then copy[key] = copied end
                                end
                            end
                            seen[item] = nil
                            return copy
                        end
                        return snapshot(value) or {}
                    end
                    "#,
                )
                .eval::<mlua::Function>()?
                .call::<mlua::Value>(state)
        });
        value
            .ok()
            .and_then(|value| crate::lua::lua_to_serde(&self.lua, &value))
            .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
    }

    fn install_state_snapshot(&self, state: &serde_json::Value) -> LuaResult<()> {
        let value = crate::lua::json_to_lua(&self.lua, state)?;
        crate::lua::module::internal_api_root(&self.lua)?.raw_set("__state", value)
    }

    /// Guard standard-library effects that cannot be part of candidate
    /// evaluation. Registered direct-effect APIs enforce the same candidate
    /// gate where the capability crosses the Rust boundary.
    fn install_candidate_stdlib_guards(&mut self) -> LuaResult<()> {
        const STANDARD_BLOCKED_FUNCTIONS: &[&str] = &[
            "io.output",
            "io.popen",
            "io.tmpfile",
            "os.execute",
            "os.exit",
            "os.remove",
            "os.rename",
            "os.setlocale",
            "os.tmpname",
        ];

        let functions = self.lua.create_table()?;
        for (index, path) in STANDARD_BLOCKED_FUNCTIONS.iter().copied().enumerate() {
            functions.set(index + 1, path)?;
        }
        let is_loading = self
            .lua
            .create_function(|lua, ()| Ok(crate::lua::doc::candidate_is_loading(lua)))?;
        let install = self
            .lua
            .load(
                r#"
                return function(blocked_functions, is_loading)
                    local function resolve(path)
                        local value = _G
                        local owner = nil
                        local key = nil
                        for part in string.gmatch(path, "[^.]+") do
                            owner = value
                            key = part
                            if type(owner) ~= "table" then return nil, nil end
                            value = rawget(owner, key)
                            if value == nil then return nil, nil end
                        end
                        return owner, key
                    end

                    local function guard(owner, key, path)
                        local original = rawget(owner, key)
                        if type(original) ~= "function" then return end
                        rawset(owner, key, function(...)
                            if is_loading() then
                                error(path .. " is unavailable while loading a Lua candidate", 2)
                            end
                            return original(...)
                        end)
                    end

                    local candidate_cwd = smelt.os.cwd()
                    local path_is_absolute = smelt.path.is_absolute
                    local path_join = smelt.path.join
                    local function candidate_path(path)
                        if not is_loading() or type(path) ~= "string" then return path end
                        if path_is_absolute(path) then return path end
                        return path_join(candidate_cwd, path)
                    end

                    if io and type(io.open) == "function" then
                        local original_open = io.open
                        io.open = function(path, mode)
                            if is_loading() then
                                mode = mode or "r"
                                if string.find(mode, "[wa+]") then
                                    error("io.open is read-only while loading a Lua candidate", 2)
                                end
                                path = candidate_path(path)
                            end
                            return original_open(path, mode)
                        end
                    end
                    if io and type(io.lines) == "function" then
                        local original_lines = io.lines
                        io.lines = function(path, ...)
                            if path == nil then return original_lines() end
                            return original_lines(candidate_path(path), ...)
                        end
                    end
                    if io and type(io.input) == "function" then
                        local original_input = io.input
                        io.input = function(path)
                            if path == nil then return original_input() end
                            return original_input(candidate_path(path))
                        end
                    end
                    if type(loadfile) == "function" then
                        local original_loadfile = loadfile
                        loadfile = function(path, ...)
                            return original_loadfile(candidate_path(path), ...)
                        end
                    end
                    if type(dofile) == "function" then
                        local original_dofile = dofile
                        dofile = function(path)
                            return original_dofile(candidate_path(path))
                        end
                    end

                    for _, path in ipairs(blocked_functions) do
                        local owner, key = resolve(path)
                        if owner then guard(owner, key, path) end
                    end
                end
                "#,
            )
            .eval::<mlua::Function>()?;
        install.call::<()>((functions, is_loading))?;
        Ok(())
    }

    pub fn commit_candidate(&self) -> LuaResult<()> {
        self.shared
            .activate_generation_resources()
            .map_err(LuaError::external)?;
        crate::lua::doc::commit_candidate_load(&self.lua)?;
        self.mark_running();
        Ok(())
    }

    /// Retire a committed runtime after its replacement has loaded and
    /// validated. External holders of the shared registries observe an empty
    /// retired generation instead of dangling Lua handles.
    pub fn retire(&mut self) {
        if let Ok(mut tasks) = self.shared.tasks.lock() {
            tasks.cancel_and_clear();
        }
        if let Ok(mut queue) = self.shared.task_inbox.lock() {
            queue.clear();
        }
        if let Ok(mut queue) = self.shared.json_inbox.lock() {
            queue.clear();
        }
        self.shared.clear_for_reload();
    }

    /// **Single ledger** of every Lua-side surface wiped at the top of a
    /// `/reload` cycle. Add new `LuaShared` registries here - `reload()`
    /// is the only caller, and the matching reload-survival test asserts
    /// every clearable surface is empty after calling this.
    ///
    /// Order matters: cancel in-flight tasks *before* clearing handles so
    /// no parked coroutine resumes with stale registry keys; drain inboxes
    /// before re-running modules so the new cycle starts with an empty
    /// resume queue.
    fn clear_for_reload(&mut self) {
        if let Ok(mut tasks) = self.shared.tasks.lock() {
            tasks.cancel_and_clear();
        }
        if let Ok(mut q) = self.shared.task_inbox.lock() {
            q.clear();
        }
        if let Ok(mut q) = self.shared.json_inbox.lock() {
            q.clear();
        }
        self.shared.clear_for_reload();
        self.wipe_loaded_modules();
    }

    pub fn load_init(&mut self, path: &std::path::Path) -> LuaResult<()> {
        self.record_loaded_file(path.to_path_buf());
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read init.lua: {e}")))?;
        // Push an unnamed loader frame; `smelt.plugin("name")` inside
        // the body opts in to hot-reload survival. Falls back to plain
        // execution when bootstrap has not installed the private scope wrapper.
        let loader = self.lua.load(&src).set_name("init.lua").into_function()?;
        match wrap_in_scope(&self.lua, loader.clone()) {
            Ok(wrapped) => wrapped.call::<()>(()),
            Err(_) => loader.call::<()>(()),
        }
    }

    pub fn load_global_plugins(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let dir = self.load_paths.config_dir.join("plugins");
        for path in lua_files_in(&dir) {
            if let Err(e) = self.load_plugin_file(&path) {
                self.set_load_error(
                    "plugin",
                    Some(path.clone()),
                    format!("{}: {e}", path.display()),
                );
                return;
            }
        }
    }

    /// Load `.smelt/init.lua` and `.smelt/plugins/*.lua`, gated by trust. Returns the trust state.
    pub fn load_project_config(&mut self, cwd: &std::path::Path) -> crate::trust::TrustState {
        let state = self.project_trust_state(cwd);
        if !matches!(state, crate::trust::TrustState::Trusted { .. }) {
            return state;
        }
        if self.load_error.is_some() {
            return state;
        }
        let smelt_dir = cwd.join(".smelt");
        self.load_project_commands();
        if self.load_error.is_some() {
            return state;
        }
        for path in lua_files_in(&smelt_dir.join("plugins")) {
            if let Err(e) = self.load_plugin_file(&path) {
                self.set_load_error(
                    "plugin",
                    Some(path.clone()),
                    format!("{}: {e}", path.display()),
                );
                return state;
            }
        }
        let init = smelt_dir.join("init.lua");
        if init.exists() {
            if let Err(e) = self.load_init(&init) {
                self.set_load_error(
                    "project",
                    Some(init.clone()),
                    format!("{}: {e}", init.display()),
                );
            }
        }
        state
    }

    fn load_plugin_file(&mut self, path: &std::path::Path) -> LuaResult<()> {
        self.record_loaded_file(path.to_path_buf());
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read {}: {e}", path.display())))?;
        let name = path.display().to_string();
        let loader = self
            .lua
            .load(&src)
            .set_name(name.as_str())
            .into_function()?;
        // Push an unnamed loader frame; the plugin opts in via
        // `smelt.plugin("name")`. Falls back to plain execution when bootstrap
        // has not installed the private scope wrapper.
        match wrap_in_scope(&self.lua, loader.clone()) {
            Ok(wrapped) => wrapped.call::<()>(()),
            Err(_) => loader.call::<()>(()),
        }
    }

    pub fn to_config(&self) -> crate::config::Config {
        let providers = self
            .shared
            .providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mcp = self
            .shared
            .mcp_configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let lsp = self.lsp_config_snapshot();
        let mut settings = crate::config::ResolvedSettings::default();
        let overrides = self
            .shared
            .settings_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (key, value) in overrides.iter() {
            if let Err(e) = settings.set(key, value) {
                eprintln!("settings override: {e}");
            }
        }
        let defaults = self
            .shared
            .defaults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let remember = self
            .shared
            .remember
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        crate::config::Config {
            providers,
            mcp,
            lsp,
            settings,
            defaults,
            remember,
        }
    }

    pub fn permission_rules_snapshot(&self) -> Option<crate::permissions::rules::RawPerms> {
        self.shared
            .permission_rules
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn lsp_config_snapshot(&self) -> crate::lsp::LspConfig {
        self.shared
            .lsp_config
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn default_shell_snapshot(&self) -> Option<crate::lua::DefaultShell> {
        self.shared
            .default_shell
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn tool_defaults(&self) -> crate::permissions::rules::ToolDefaults {
        self.shared
            .tool_defaults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn run_command(&self, name: &str, arg: Option<String>) -> bool {
        self.run_command_with_queue_target(name, arg, crate::lua::CommandQueueTarget::Turn, None)
    }

    pub fn run_command_with_queue_target(
        &self,
        name: &str,
        arg: Option<String>,
        queue_target: crate::lua::CommandQueueTarget,
        sent_at_ms: Option<u64>,
    ) -> bool {
        let func = {
            let Ok(map) = self.shared.commands.lock() else {
                return false;
            };
            let Some(entry) = map.get(name) else {
                return false;
            };
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&entry.handle.key) else {
                return false;
            };
            f
        };
        let _perf = smelt_perf::perf::begin("lua:cmd");

        let mut initial_args = mlua::MultiValue::new();
        if let Some(a) = arg {
            match self.lua.create_string(&a) {
                Ok(s) => initial_args.push_back(mlua::Value::String(s)),
                Err(e) => {
                    self.record_error(format!("cmd `{name}`: {e}"));
                    return true;
                }
            }
        }

        // Run the handler on the Lua task runtime so it executes inside a
        // coroutine. Yieldable APIs (`smelt.dialog.open`, `smelt.sleep`,
        // `smelt.task.wait`, ...) then work without each command wrapping its
        // body in `smelt.spawn`. Non-yielding handlers finish in the
        // synchronous drive below; yielding handlers park on the runtime and
        // resume on the next main-loop `drive_tasks` tick.
        let spawn_result = {
            let Ok(mut rt) = self.shared.tasks.lock() else {
                self.record_error(format!("cmd `{name}`: task runtime unavailable"));
                return true;
            };
            rt.spawn(
                &self.lua,
                func,
                initial_args,
                TaskCompletion::Command {
                    name: name.to_string(),
                    queue_target,
                    sent_at_ms,
                },
            )
        };
        if let Err(e) = spawn_result {
            self.record_error(format!("cmd `{name}`: {e}"));
            return true;
        }
        let _ = self.drive_tasks(Instant::now());
        true
    }

    /// Dispatch a keymap chord to any Lua-registered handler.
    /// `chord_ctx` is passed as a table for multi-key sequences; `None` for single-key.
    pub fn run_keymap(
        &self,
        chord: &str,
        current_mode: Option<&str>,
        chord_ctx: Option<&[(&str, String)]>,
    ) -> KeymapResult {
        let func = {
            let Ok(map) = self.shared.keymaps.lock() else {
                return KeymapResult::NoBinding;
            };
            let mode_char = dispatch_mode_char(current_mode);
            let entry = mode_char
                .and_then(|mc| map.get(&(mc.to_string(), chord.to_string())))
                .or_else(|| map.get(&(String::new(), chord.to_string())));
            let Some(entry) = entry else {
                return KeymapResult::NoBinding;
            };
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&entry.handle.key) else {
                return KeymapResult::NoBinding;
            };
            f
        };
        let result = match chord_ctx {
            Some(pairs) => {
                let ctx = match self.lua.create_table() {
                    Ok(t) => t,
                    Err(e) => {
                        self.record_error(format!("keymap `{chord}`: {e}"));
                        return KeymapResult::Consumed;
                    }
                };
                for (k, v) in pairs {
                    if let Err(e) = ctx.set(*k, v.as_str()) {
                        self.record_error(format!("keymap `{chord}`: {e}"));
                        return KeymapResult::Consumed;
                    }
                }
                let _perf = smelt_perf::perf::begin("lua:keymap");
                func.call::<mlua::Value>(ctx)
            }
            None => {
                let _perf = smelt_perf::perf::begin("lua:keymap");
                func.call::<mlua::Value>(())
            }
        };
        match result {
            Ok(mlua::Value::Boolean(false)) => KeymapResult::PassThrough,
            Ok(_) => KeymapResult::Consumed,
            Err(e) => {
                self.record_error(format!("keymap `{chord}`: {e}"));
                KeymapResult::Consumed
            }
        }
    }

    /// Returns true if a registered chord would be considered for this mode.
    /// Does not execute the handler, so pass-through callbacks are still treated
    /// as bindings by conservative callers.
    pub fn chord_has_binding(&self, chord: &str, current_mode: Option<&str>) -> bool {
        let Ok(map) = self.shared.keymaps.lock() else {
            return false;
        };
        let mode_char = dispatch_mode_char(current_mode);
        mode_char
            .map(|mc| map.contains_key(&(mc.to_string(), chord.to_string())))
            .unwrap_or(false)
            || map.contains_key(&(String::new(), chord.to_string()))
    }

    /// Returns true if `sequence` is a strict prefix of a registered chord (exact match excluded).
    pub fn chord_has_longer(&self, sequence: &str, current_mode: Option<&str>) -> bool {
        let Ok(map) = self.shared.keymaps.lock() else {
            return false;
        };
        let mode_char = query_mode_char(current_mode);
        for (m, chord) in map.keys() {
            if keymap_mode_matches(m, mode_char)
                && chord.len() > sequence.len()
                && crate::keymap::chord_sequence_starts_with(chord, sequence)
            {
                return true;
            }
        }
        false
    }

    /// Return effective registered chords that strictly extend `sequence` in `current_mode`.
    pub fn keymap_prefixes(&self, sequence: &str, current_mode: Option<&str>) -> Vec<KeymapPrefix> {
        let Ok(map) = self.shared.keymaps.lock() else {
            return Vec::new();
        };
        let mode_char = query_mode_char(current_mode);
        let mut rows: Vec<KeymapPrefix> = map
            .iter()
            .filter_map(|((mode, chord), entry)| {
                if !keymap_mode_matches(mode, mode_char)
                    || chord.len() <= sequence.len()
                    || !crate::keymap::chord_sequence_starts_with(chord, sequence)
                {
                    return None;
                }
                Some(KeymapPrefix {
                    mode: mode.clone(),
                    chord: chord.clone(),
                    suffix: chord[sequence.len()..].to_string(),
                    description: entry.description.clone(),
                })
            })
            .collect();

        rows.sort_by(|a, b| {
            a.chord
                .cmp(&b.chord)
                .then_with(|| a.mode.is_empty().cmp(&b.mode.is_empty()))
                .then_with(|| a.description.cmp(&b.description))
        });
        rows.dedup_by(|a, b| a.chord == b.chord);
        rows.sort_by(|a, b| {
            a.suffix
                .cmp(&b.suffix)
                .then_with(|| a.description.cmp(&b.description))
                .then_with(|| a.mode.cmp(&b.mode))
        });
        rows
    }

    pub fn cycle_mode(&self) {
        let result: mlua::Result<()> = (|| {
            let smelt: mlua::Table = self.lua.globals().get("smelt")?;
            let mode: mlua::Table = smelt.get("mode")?;
            let cycle: mlua::Function = mode.get("cycle")?;
            cycle.call::<()>(())
        })();
        if let Err(e) = result {
            self.record_error(format!("smelt.mode.cycle: {e}"));
        }
    }

    pub fn cycle_reasoning(&self) {
        let result: mlua::Result<()> = (|| {
            let smelt: mlua::Table = self.lua.globals().get("smelt")?;
            let reasoning: mlua::Table = smelt.get("reasoning")?;
            let cycle: mlua::Function = reasoning.get("cycle")?;
            cycle.call::<()>(())
        })();
        if let Err(e) = result {
            self.record_error(format!("smelt.reasoning.cycle: {e}"));
        }
    }

    /// Surface an error to the user. Routes through `smelt.notify.error`,
    /// which appends the full body to the persistent message log AND
    /// pops a one-line toast. Falls back to a direct log write if the
    /// Lua surface isn't bound (e.g. an early-boot failure before
    /// `register_api` has run).
    pub fn record_error(&self, msg: String) {
        Self::record_error_with(&self.lua, &self.shared, msg);
    }

    pub fn record_error_with(lua: &mlua::Lua, shared: &LuaShared, msg: String) {
        let routed = lua
            .globals()
            .get::<mlua::Table>("smelt")
            .and_then(|s| s.get::<mlua::Table>("notify"))
            .and_then(|n| n.get::<mlua::Function>("error"))
            .and_then(|f| f.call::<()>(msg.clone()))
            .is_ok();
        if routed {
            return;
        }
        if let Ok(mut messages) = shared.messages.lock() {
            messages.append(crate::messages::MessageKind::Error, "lua".to_string(), msg);
        }
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.shared
            .commands
            .lock()
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    /// Send+Sync handle to this generation's registered `/command` names.
    /// Host code can recognize commands without touching the `!Send` Lua
    /// handlers stored in `commands`.
    pub fn command_names_handle(&self) -> crate::commands::CommandNames {
        Arc::clone(&self.shared.command_names)
    }

    /// Invoke every `smelt.lifecycle.on(event, fn)` callback in registration
    /// order, then drop them. The host calls this at the corresponding phase:
    /// `"ready"` after Lua bootstrap and argv parse, `"shutdown"` after the
    /// TUI tears down but before the process exits. Per-hook errors are
    /// returned so the caller can surface them as in-app notifications without
    /// aborting the remaining hooks.
    ///
    /// `build_ctx` constructs the per-event ctx table fresh inside the Lua
    /// runtime borrow. Pass `|_| Ok(mlua::Value::Nil)` for events that don't
    /// need a ctx.
    pub fn drain_lifecycle_hooks<F>(&mut self, event: &str, build_ctx: F) -> Vec<String>
    where
        F: Fn(&mlua::Lua) -> mlua::Result<mlua::Value>,
    {
        let hooks = self.take_lifecycle_hooks(event);
        Self::invoke_lifecycle_hooks(&self.lua, event, hooks, build_ctx)
    }

    /// Remove and resolve all callbacks registered for one lifecycle event.
    ///
    /// Frontends can prepare this batch before lending their host, then invoke
    /// it from a scoped Lua entry without retaining a borrow of the runtime
    /// stored in that host.
    pub fn take_lifecycle_hooks(&self, event: &str) -> Vec<mlua::Function> {
        self.shared.hooks.lifecycle.drain_for(&self.lua, event)
    }

    /// Invoke a prepared lifecycle batch in registration order.
    pub fn invoke_lifecycle_hooks<F>(
        lua: &mlua::Lua,
        event: &str,
        hooks: Vec<mlua::Function>,
        build_ctx: F,
    ) -> Vec<String>
    where
        F: Fn(&mlua::Lua) -> mlua::Result<mlua::Value>,
    {
        let mut errors = Vec::with_capacity(hooks.len());
        for function in hooks {
            let ctx = match build_ctx(lua) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("lifecycle.{event}: ctx build: {error}"));
                    continue;
                }
            };
            if let Err(error) = function.call::<()>(ctx) {
                errors.push(format!("lifecycle.{event}: {error}"));
            }
        }
        errors
    }

    /// Convenience over [`drain_lifecycle_hooks`] for the `"shutdown"` event:
    /// builds the standard shutdown context table so callers don't need a direct
    /// `mlua` dependency.
    pub fn drain_shutdown_hooks(&mut self, ctx: ShutdownHookContext<'_>) -> Vec<String> {
        let sid = ctx.session_id.to_string();
        self.drain_lifecycle_hooks("shutdown", |lua| {
            let t = lua.create_table()?;
            t.set("session_id", sid.as_str())?;
            t.set("has_messages", ctx.has_messages)?;
            t.set("ephemeral", ctx.ephemeral)?;
            Ok(mlua::Value::Table(t))
        })
    }

    pub fn command_busy_behavior(
        &self,
        name: &str,
        arg: Option<&str>,
    ) -> mlua::Result<Option<crate::lua::CommandBusyBehavior>> {
        let (busy, callback) = {
            let commands =
                self.shared.commands.lock().map_err(|_| {
                    mlua::Error::RuntimeError("command registry lock poisoned".into())
                })?;
            let Some(command) = commands.get(name) else {
                return Ok(None);
            };
            let callback = command
                .busy_fn
                .as_ref()
                .map(|handle| self.lua.registry_value::<mlua::Function>(&handle.key))
                .transpose()?;
            (command.busy, callback)
        };
        let behavior = match callback {
            Some(callback) => match callback.call::<Option<String>>(arg)? {
                Some(value) => crate::lua::CommandBusyBehavior::parse(&value)
                    .map_err(mlua::Error::RuntimeError)?,
                None => busy,
            },
            None => busy,
        };
        Ok(Some(behavior))
    }

    pub fn command_startup_ok(&self, name: &str) -> Option<bool> {
        self.shared
            .commands
            .lock()
            .ok()?
            .get(name)
            .map(|c| c.startup_ok)
    }

    pub fn command_names(&self) -> Vec<String> {
        self.shared
            .commands
            .lock()
            .map(|m| {
                let mut v: Vec<String> = m.keys().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    pub fn command_completion_items(&self) -> Vec<CommandCompletionItem> {
        self.shared
            .commands
            .lock()
            .map(|m| {
                let mut v: Vec<CommandCompletionItem> = m
                    .iter()
                    .filter(|(_, cmd)| !cmd.hidden)
                    .map(|(name, cmd)| CommandCompletionItem {
                        name: name.clone(),
                        description: cmd.description.clone(),
                    })
                    .collect();
                v.sort_by(|a, b| a.name.cmp(&b.name));
                v
            })
            .unwrap_or_default()
    }

    pub fn remove_callback(&self, id: u64) {
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.remove(&id);
        }
    }

    pub fn resolve_external(&self, external_id: u64, value: mlua::Value) -> bool {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return false;
        };
        rt.resolve_external(external_id, value)
    }

    pub fn resolve_core_tool_call(
        &self,
        request_id: u64,
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
    ) {
        let table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tools.call result table: {e}"));
                return;
            }
        };
        if let Err(e) = table.set("content", content) {
            self.record_error(format!("tools.call result.content: {e}"));
            return;
        }
        if let Err(e) = table.set("is_error", is_error) {
            self.record_error(format!("tools.call result.is_error: {e}"));
            return;
        }
        if let Some(meta) = metadata {
            match json_to_lua(&self.lua, &meta) {
                Ok(v) => {
                    let _ = table.set("metadata", v);
                }
                Err(e) => self.record_error(format!("tools.call result.metadata: {e}")),
            }
        }
        self.resolve_external(request_id, mlua::Value::Table(table));
    }

    pub fn pump_task_events(&self) {
        let json_pending: Vec<(u64, serde_json::Value)> = {
            let Ok(mut inbox) = self.shared.json_inbox.lock() else {
                return;
            };
            std::mem::take(&mut *inbox)
        };
        if !json_pending.is_empty() {
            if let Ok(mut main) = self.shared.task_inbox.lock() {
                for (external_id, value) in json_pending {
                    main.push(TaskEvent::ExternalResolvedJson { external_id, value });
                }
            }
        }
        let events: Vec<TaskEvent> = {
            let Ok(mut inbox) = self.shared.task_inbox.lock() else {
                return;
            };
            std::mem::take(&mut *inbox)
        };
        for ev in events {
            match ev {
                TaskEvent::ExternalResolved { external_id, value } => {
                    let v = self.lua.registry_value(&value).unwrap_or(mlua::Value::Nil);
                    self.resolve_external(external_id, v);
                }
                TaskEvent::ExternalResolvedJson { external_id, value } => {
                    let v = json_to_lua(&self.lua, &value).unwrap_or(mlua::Value::Nil);
                    self.resolve_external(external_id, v);
                }
            }
        }
    }

    pub fn cancel_tasks(&self) {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return;
        };
        rt.cancel_all(&self.lua);
    }

    pub fn cancel_turn_tasks(&self) {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return;
        };
        rt.cancel_scope(&self.lua, super::task::TaskScope::Turn);
    }

    pub fn set_wakeup_sender(&self, tx: tokio::sync::mpsc::UnboundedSender<()>) {
        let _ = self.shared.wakeup_tx.set(tx);
    }

    pub fn next_task_wakeup(&self, now: Instant) -> Option<Instant> {
        let Ok(rt) = self.shared.tasks.lock() else {
            return None;
        };
        rt.next_wakeup(now)
    }

    fn take_next_ready_task(&self, now: Instant) -> Result<Option<super::task::LuaTask>, ()> {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return Err(());
        };
        Ok(rt.take_next_ready(now))
    }

    fn step_task_outside_runtime_lock(
        &self,
        task: super::task::LuaTask,
        now: Instant,
        outs: &mut Vec<TaskDriveOutput>,
    ) -> Result<(), ()> {
        // Keep the task mutex unlocked while resuming Lua; task code can re-enter
        // the runtime via `smelt.spawn`, cancellation handles, or task helpers.
        if let Some(parked) = crate::lua::step_task_owned(&self.lua, task, now, outs) {
            let Ok(mut rt) = self.shared.tasks.lock() else {
                return Err(());
            };
            rt.put_back(parked);
        }
        Ok(())
    }

    pub fn drive_tasks(&self, now: Instant) -> Vec<TaskDriveOutput> {
        let mut outs = Vec::new();
        loop {
            let task = match self.take_next_ready_task(now) {
                Ok(Some(task)) => task,
                Ok(None) => break,
                Err(()) => return Vec::new(),
            };
            if self
                .step_task_outside_runtime_lock(task, now, &mut outs)
                .is_err()
            {
                return Vec::new();
            }
        }
        let mut forward = Vec::with_capacity(outs.len());
        for out in outs {
            match out {
                TaskDriveOutput::ToolComplete { .. } => forward.push(out),
                TaskDriveOutput::NotifyError(msg) => self.record_error(msg),
            }
        }
        forward
    }

    pub fn has_tool(&self, tool_name: &str) -> bool {
        self.shared
            .tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(tool_name)
    }

    pub fn tool_available_for(&self, tool_name: &str, visibility: ToolVisibility) -> bool {
        self.has_tool(tool_name) && visibility.includes(self.tool_headless_enabled(tool_name))
    }

    fn tool_headless_enabled(&self, tool_name: &str) -> bool {
        self.lua
            .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool_name}"))
            .ok()
            .map(|meta| Self::meta_headless_enabled(&meta))
            .unwrap_or(true)
    }

    fn meta_headless_enabled(meta: &mlua::Table) -> bool {
        meta.get::<mlua::Value>("headless")
            .ok()
            .and_then(|v| v.as_boolean())
            .unwrap_or(true)
    }

    pub fn system_prompt_fragments(&self) -> Vec<String> {
        crate::lua::api::agent::system_prompt_fragments(&self.lua)
    }

    pub fn tool_defs(
        &self,
        _mode: protocol::AgentMode,
        visibility: ToolVisibility,
    ) -> Vec<protocol::ToolDef> {
        let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = handlers.keys().cloned().collect();
        drop(handlers);
        names.sort();
        let mut defs = Vec::new();
        for name in &names {
            if let Ok(meta_table) = self
                .lua
                .named_registry_value::<mlua::Table>(&format!("__pt_meta_{name}"))
            {
                let headless = Self::meta_headless_enabled(&meta_table);
                if !visibility.includes(headless) {
                    continue;
                }
                let description: String = meta_table.get("description").unwrap_or_default();
                let parameters: serde_json::Value = meta_table
                    .get::<mlua::LuaString>("parameters_json")
                    .ok()
                    .and_then(|s| serde_json::from_str(&s.to_string_lossy()).ok())
                    .unwrap_or(serde_json::json!({"type": "object", "properties": {}}));
                let modes: Option<Vec<protocol::AgentMode>> =
                    meta_table.get::<mlua::Table>("modes").ok().map(|t| {
                        t.sequence_values::<String>()
                            .filter_map(|r| r.ok())
                            .filter_map(|s| protocol::AgentMode::parse(&s))
                            .collect()
                    });
                let execution_mode = meta_table
                    .get::<String>("execution_mode")
                    .ok()
                    .and_then(|s| match s.as_str() {
                        "sequential" => Some(protocol::ToolExecutionMode::Sequential),
                        "concurrent" => Some(protocol::ToolExecutionMode::Concurrent),
                        _ => None,
                    })
                    .unwrap_or_default();
                let hooks = protocol::ToolHookFlags {
                    approval_patterns: meta_table.get("hook_approval_patterns").unwrap_or(false),
                    preflight: meta_table.get("hook_preflight").unwrap_or(false),
                };
                let override_core: bool = meta_table.get("override_core").unwrap_or(false);
                defs.push(protocol::ToolDef {
                    name: name.clone(),
                    description,
                    parameters,
                    modes,
                    execution_mode,
                    hooks,
                    override_core,
                    headless,
                });
            }
        }
        defs
    }

    fn lua_value_to_tool_path(value: mlua::Value) -> Result<ToolPath, String> {
        let (path, target_kind) = match value {
            mlua::Value::String(path) => (path.to_string_lossy(), PathTargetKind::Unknown),
            mlua::Value::Table(table) => {
                let path = match table
                    .get::<Option<String>>("path")
                    .map_err(|error| format!("read `path`: {error}"))?
                {
                    Some(path) => Some(path),
                    None => table
                        .get::<Option<String>>(1)
                        .map_err(|error| format!("read positional path: {error}"))?,
                }
                .ok_or_else(|| "missing string `path`".to_string())?;
                let kind = match table
                    .get::<Option<String>>("kind")
                    .map_err(|error| format!("read `kind`: {error}"))?
                {
                    Some(kind) => Some(kind),
                    None => table
                        .get::<Option<String>>("target_kind")
                        .map_err(|error| format!("read `target_kind`: {error}"))?,
                }
                .unwrap_or_else(|| "unknown".to_string())
                .trim()
                .to_ascii_lowercase();
                let target_kind = match kind.as_str() {
                    "file" => PathTargetKind::File,
                    "dir" | "directory" => PathTargetKind::Directory,
                    "" | "unknown" => PathTargetKind::Unknown,
                    _ => return Err(format!("unknown path kind `{kind}`")),
                };
                (path, target_kind)
            }
            value => {
                return Err(format!(
                    "expected a path string or table, got {}",
                    value.type_name()
                ));
            }
        };
        if path.is_empty() {
            return Err("path must not be empty".to_string());
        }
        Ok(ToolPath { path, target_kind })
    }

    fn parse_tool_workspace_paths(table: mlua::Table) -> Result<Vec<ToolPath>, String> {
        let sequence_len = table.raw_len();
        for pair in table.clone().pairs::<mlua::Value, mlua::Value>() {
            let (key, _) = pair.map_err(|error| format!("read returned paths: {error}"))?;
            let in_sequence = matches!(
                key,
                mlua::Value::Integer(index)
                    if index >= 1 && usize::try_from(index).ok().is_some_and(|index| index <= sequence_len)
            );
            if !in_sequence {
                return Err("expected an array of path entries".to_string());
            }
        }

        let mut paths = Vec::with_capacity(sequence_len);
        for index in 1..=sequence_len {
            let value = table
                .raw_get::<mlua::Value>(index)
                .map_err(|error| format!("read path entry {index}: {error}"))?;
            let path = Self::lua_value_to_tool_path(value)
                .map_err(|error| format!("invalid path entry {index}: {error}"))?;
            paths.push(path);
        }
        Ok(paths)
    }

    fn tool_workspace_paths_error(&self, tool_name: &str, error: impl std::fmt::Display) -> String {
        let message = format!("tool paths_for_workspace `{tool_name}` failed: {error}");
        self.record_error(message.clone());
        message
    }

    /// Call a tool's `paths_for_workspace(args)` callback.
    ///
    /// Missing callbacks are distinguished from callbacks that successfully
    /// declare no paths. Callback, conversion, and return-shape failures are
    /// errors so workspace restrictions cannot silently treat them as pathless.
    pub fn tool_paths_for_workspace(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Result<ToolWorkspacePaths, String> {
        let func = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(handles) = handlers.get(tool_name) else {
                return Ok(ToolWorkspacePaths::Undeclared);
            };
            let Some(handle) = handles.paths_for_workspace.as_ref() else {
                return Ok(ToolWorkspacePaths::Undeclared);
            };
            self.lua
                .registry_value::<mlua::Function>(&handle.key)
                .map_err(|error| self.tool_workspace_paths_error(tool_name, error))?
        };
        let args_table = self
            .args_to_lua_table(args)
            .map_err(|error| self.tool_workspace_paths_error(tool_name, error))?;
        let _perf = smelt_perf::perf::begin("lua:tool");
        let table = func
            .call::<mlua::Table>(args_table)
            .map_err(|error| self.tool_workspace_paths_error(tool_name, error))?;
        Self::parse_tool_workspace_paths(table)
            .map(ToolWorkspacePaths::Declared)
            .map_err(|error| self.tool_workspace_paths_error(tool_name, error))
    }

    pub fn tool_has_preview(&self, tool_name: &str) -> bool {
        let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
        handlers.get(tool_name).is_some_and(|h| h.preview.is_some())
    }

    /// Run a tool's `preview(args)` callback and return the composed `BlockLayout` tree.
    /// `None` means the tool registered no preview or returned nil.
    pub fn render_tool_preview(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Option<crate::content::block_layout::BlockLayout> {
        match Self::call_tool_preview(&self.lua, &self.shared, tool_name, args) {
            Ok(layout) => layout,
            Err(error) => {
                self.record_error(error);
                None
            }
        }
    }

    /// Invoke a preview callback using explicit Lua state rather than ambient host access.
    /// UI bindings use this after releasing their mutable app borrow so preview code may
    /// call host APIs safely.
    pub fn call_tool_preview(
        lua: &mlua::Lua,
        shared: &LuaShared,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Result<Option<crate::content::block_layout::BlockLayout>, String> {
        let preview_fn = {
            let handlers = shared
                .tools
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(handler) = handlers.get(tool_name) else {
                return Ok(None);
            };
            let Some(preview) = handler.preview.as_ref() else {
                return Ok(None);
            };
            lua.registry_value::<mlua::Function>(&preview.key)
                .map_err(|error| format!("tool preview `{tool_name}`: {error}"))?
        };
        let args_table = args_to_lua_table(lua, args)
            .map_err(|error| format!("tool preview: build args: {error}"))?;

        let _perf = smelt_perf::perf::begin("lua:tool");
        let result: mlua::Value = preview_fn
            .call(args_table)
            .map_err(|error| format!("tool preview `{tool_name}`: {error}"))?;

        match result {
            mlua::Value::Nil => Ok(None),
            mlua::Value::UserData(userdata) => userdata
                .borrow::<crate::lua::api::layout::LuaBlockLayout>()
                .map(|layout| Some(layout.0.clone()))
                .map_err(|error| {
                    format!("tool preview `{tool_name}`: expected smelt.layout value: {error}")
                }),
            _ => Err(format!(
                "tool preview `{tool_name}`: expected smelt.layout value or nil"
            )),
        }
    }

    pub fn tool_preview_output(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Option<ToolOutputRef> {
        let preview_output_fn = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let h = handlers.get(tool_name)?;
            let rh = h.preview_output.as_ref()?;
            self.lua.registry_value::<mlua::Function>(&rh.key).ok()?
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool preview_output: build args: {e}"));
                return None;
            }
        };

        let _perf = smelt_perf::perf::begin("lua:tool");
        let result: mlua::Value = match preview_output_fn.call(args_table) {
            Ok(v) => v,
            Err(e) => {
                self.record_error(format!("tool preview_output `{tool_name}`: {e}"));
                return None;
            }
        };

        match result {
            mlua::Value::Nil => None,
            mlua::Value::Table(table) => match Self::tool_output_from_lua_table(&self.lua, table) {
                Ok(output) => Some(Box::new(output)),
                Err(error) => {
                    self.record_error(format!(
                        "tool preview_output `{tool_name}`: invalid result: {error}"
                    ));
                    None
                }
            },
            other => {
                self.record_error(format!(
                    "tool preview_output `{tool_name}`: expected result table or nil, got {}",
                    other.type_name()
                ));
                None
            }
        }
    }

    fn tool_output_from_lua_table(lua: &Lua, result: mlua::Table) -> LuaResult<ToolOutput> {
        let result = crate::lua::tool_result_from_lua_table(lua, &result)?;
        Ok(ToolOutput::from_display_content(
            result.content,
            result.is_error,
            result.metadata,
            result.display_content,
        ))
    }

    /// Invoke the tool's `summary(args)` Lua hook. The hook may return:
    ///   * `nil` / no value - empty summary (no header text)
    ///   * a `string` - wrapped as a single plain span (each `\n`-line one row)
    ///   * a table of `{ {span, span}, {span, span} }` - multi-line styled output;
    ///     span shape matches `buf:styled` (`{ text, syntax?, style? }`).
    pub fn tool_summary(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> protocol::StyledLines {
        self.tool_summary_with_context(tool_name, args, true)
    }

    pub fn tool_summary_with_context(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        final_args: bool,
    ) -> protocol::StyledLines {
        let meta = match self
            .lua
            .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool_name}"))
        {
            Ok(meta) => meta,
            Err(_) => return protocol::StyledLines::empty(),
        };
        let func = match meta.get::<mlua::Function>("summary") {
            Ok(func) => func,
            Err(_) => return protocol::StyledLines::empty(),
        };
        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool summary: build args: {e}"));
                return protocol::StyledLines::empty();
            }
        };
        let ctx_table = match self.lua.create_table() {
            Ok(t) => {
                if let Err(e) = t.set("final", final_args) {
                    self.record_error(format!("tool summary: build context: {e}"));
                    return protocol::StyledLines::empty();
                }
                t
            }
            Err(e) => {
                self.record_error(format!("tool summary: build context: {e}"));
                return protocol::StyledLines::empty();
            }
        };
        let _perf = smelt_perf::perf::begin("lua:tool");
        match func.call::<mlua::Value>((args_table, ctx_table)) {
            Ok(v) => match crate::lua::styled_lines_from_lua(v, "tool summary") {
                Ok(lines) => lines,
                Err(e) => {
                    self.record_error(format!("tool summary `{tool_name}`: {e}"));
                    protocol::StyledLines::empty()
                }
            },
            Err(e) => {
                self.record_error(format!("tool summary `{tool_name}`: {e}"));
                protocol::StyledLines::empty()
            }
        }
    }

    pub fn evaluate_tool_metadata(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> protocol::ToolMetadata {
        let mut out = protocol::ToolMetadata::default();

        let (approval_patterns_fn, preflight_fn) = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return out;
            };
            let ap = h
                .approval_patterns
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            let pf = h
                .preflight
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            (ap, pf)
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool metadata: build args: {e}"));
                return out;
            }
        };

        if let Some(func) = approval_patterns_fn {
            let _perf = smelt_perf::perf::begin("lua:tool");
            match func.call::<Option<mlua::Table>>(args_table.clone()) {
                Ok(Some(t)) => {
                    out.approval_patterns = t
                        .sequence_values::<String>()
                        .filter_map(|r| r.ok())
                        .collect();
                }
                Ok(None) => {}
                Err(e) => self.record_error(format!("tool hook approval_patterns: {e}")),
            }
        }
        if let Some(func) = preflight_fn {
            let _perf = smelt_perf::perf::begin("lua:tool");
            match func.call::<Option<String>>(args_table) {
                Ok(Some(s)) => out.preflight_error = Some(s),
                Ok(None) => {}
                Err(e) => self.record_error(format!("tool hook preflight: {e}")),
            }
        }

        out.summary = self.tool_summary(tool_name, args);
        out
    }

    pub fn transcript_renderer_generation(&self) -> u64 {
        self.shared
            .transcript_renderer_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn transcript_renderer_cache_key(&self) -> Option<u64> {
        let key = self
            .shared
            .transcript_renderer_cache_key
            .load(std::sync::atomic::Ordering::Acquire);
        (key != 0).then_some(key)
    }

    pub fn transcript_settings_cache_key(&self) -> Option<u64> {
        let transcript = self
            .lua
            .globals()
            .get::<Option<mlua::Table>>("smelt")
            .ok()
            .flatten()
            .and_then(|smelt| smelt.get::<Option<mlua::Table>>("settings").ok().flatten())
            .and_then(|settings| {
                settings
                    .get::<Option<mlua::Table>>("transcript")
                    .ok()
                    .flatten()
            })?;
        Some(crate::utils::hash_serializable(
            &crate::lua::api::lua_table_to_json(&self.lua, &transcript),
        ))
    }

    pub fn transcript_group_generation(&self) -> u64 {
        self.shared
            .transcript_groups_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn transcript_group_cache_key(&self) -> Option<u64> {
        let key = self
            .shared
            .transcript_groups_cache_key
            .load(std::sync::atomic::Ordering::Acquire);
        (key != 0).then_some(key)
    }

    pub fn transcript_group_specs(&self) -> Vec<TranscriptGroupSpec> {
        self.shared
            .transcript_groups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .specs()
    }

    /// Call the composed root transcript renderer for any semantic node. Blocks,
    /// groups, and recursive children all enter through this method and therefore
    /// observe the same middleware chain. Every recursive call shares `now_ms`.
    pub fn render_transcript_layout(
        &self,
        node: &TranscriptRenderNode,
        view_state: crate::transcript_model::ViewState,
        now_ms: u64,
    ) -> BlockLayout {
        let fallback = || fallback_transcript_node_layout(node);
        let render_fn = {
            let slot = self
                .shared
                .transcript_renderer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(handle) = slot.as_ref() else {
                return fallback();
            };
            match self.lua.registry_value::<mlua::Function>(&handle.key) {
                Ok(function) => function,
                Err(error) => {
                    self.record_error(format!("transcript render: renderer handle: {error}"));
                    return fallback();
                }
            }
        };

        let lua_node = match transcript_render_node_to_lua_table(&self.lua, node) {
            Ok(table) => table,
            Err(error) => {
                self.record_error(format!("transcript render: build node: {error}"));
                return fallback();
            }
        };
        let ctx = match transcript_render_ctx_to_lua_table(
            &self.lua,
            view_state,
            self.transcript_renderer_generation(),
            now_ms,
        ) {
            Ok(table) => table,
            Err(error) => {
                self.record_error(format!("transcript render: build ctx: {error}"));
                return fallback();
            }
        };
        let render = match transcript_recursive_renderer(&self.lua, render_fn, ctx) {
            Ok(function) => function,
            Err(error) => {
                self.record_error(format!(
                    "transcript render: build recursive renderer: {error}"
                ));
                return fallback();
            }
        };

        let result: mlua::Value = match render.call(lua_node) {
            Ok(value) => value,
            Err(error) => {
                self.record_error(format!("transcript render: {error}"));
                return fallback();
            }
        };
        let kind = node.kind();
        let index = node.index();
        transcript_layout_from_lua_value(
            self,
            result,
            &format!("transcript render `{kind}` #{index}"),
            fallback,
        )
    }

    /// Current monotonic time used to schedule transcript refreshes.
    pub fn transcript_instant_now(&self) -> std::time::Instant {
        self.shared.clock().instant_now()
    }

    /// Current wall-clock time for one top-level transcript render pass.
    pub fn transcript_now_ms(&self) -> u64 {
        engine::clock::unix_time_ms(self.shared.clock().as_ref())
    }

    fn args_to_lua_table(
        &self,
        args: &HashMap<String, serde_json::Value>,
    ) -> mlua::Result<mlua::Table> {
        args_to_lua_table(&self.lua, args)
    }

    fn tool_timeout_ms(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Option<u64> {
        let meta = self
            .lua
            .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool_name}"))
            .ok();
        let default_ms = meta
            .as_ref()
            .and_then(|m| m.get::<Option<u64>>("watchdog_timeout_ms").ok().flatten())
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_MS);
        let max_ms = meta
            .as_ref()
            .and_then(|m| {
                m.get::<Option<u64>>("watchdog_max_timeout_ms")
                    .ok()
                    .flatten()
            })
            .unwrap_or(MAX_TOOL_TIMEOUT_MS)
            .max(1);
        let arg_name = meta
            .as_ref()
            .and_then(|m| {
                m.get::<Option<String>>("watchdog_timeout_arg")
                    .ok()
                    .flatten()
            })
            .unwrap_or_else(|| "timeout_ms".to_string());
        let arg_scale_ms = meta
            .as_ref()
            .and_then(|m| {
                m.get::<Option<u64>>("watchdog_timeout_arg_scale_ms")
                    .ok()
                    .flatten()
            })
            .unwrap_or(1)
            .max(1);
        let grace_ms = meta
            .as_ref()
            .and_then(|m| m.get::<Option<u64>>("watchdog_grace_ms").ok().flatten())
            .unwrap_or(0);

        let requested_ms = (!arg_name.is_empty())
            .then(|| args.get(&arg_name).and_then(serde_json::Value::as_u64))
            .flatten()
            .map(|n| n.saturating_mul(arg_scale_ms).saturating_add(grace_ms));
        let timeout_ms = requested_ms.unwrap_or(default_ms);
        (timeout_ms > 0).then(|| timeout_ms.min(max_ms))
    }

    fn tool_invocation_context(
        &self,
        tool_name: &str,
        invocation_id: protocol::InvocationId,
        request_id: u64,
    ) -> super::task::ToolInvocationContext {
        let execution_mode = self
            .shared
            .tools
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(tool_name)
            .map(|tool| tool.execution_mode)
            .unwrap_or_default();
        super::task::ToolInvocationContext {
            invocation_id,
            request_id,
            execution_mode,
        }
    }

    pub fn execute_tool(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        call: ToolCallIds<'_>,
        env: ToolEnv<'_>,
        now: Instant,
    ) -> ToolExecResult {
        self.execute_tool_with_context(tool_name, args, call, env, now)
            .1
    }

    pub fn execute_tool_with_context(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        call: ToolCallIds<'_>,
        env: ToolEnv<'_>,
        now: Instant,
    ) -> (super::task::ToolInvocationContext, ToolExecResult) {
        let invocation =
            self.tool_invocation_context(tool_name, call.invocation_id, call.request_id);
        let result = super::task::with_tool_invocation_context(invocation, || {
            self.execute_tool_inner(tool_name, args, invocation, call.call_id, env, now)
        });
        (invocation, result)
    }

    fn execute_tool_inner(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        invocation: super::task::ToolInvocationContext,
        call_id: &str,
        env: ToolEnv<'_>,
        now: Instant,
    ) -> ToolExecResult {
        let ToolEnv {
            mode,
            session_id,
            artifact_dir,
        } = env;
        let func = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(handle) = handlers.get(tool_name) else {
                return ToolExecResult::Immediate {
                    content: format!("no tool registered: {tool_name}"),
                    is_error: true,
                    metadata: None,
                    display_content: Vec::new(),
                    attachment: None,
                };
            };
            match self
                .lua
                .registry_value::<mlua::Function>(&handle.execute.key)
            {
                Ok(f) => f,
                Err(_) => {
                    return ToolExecResult::Immediate {
                        content: format!("tool handler not found: {tool_name}"),
                        is_error: true,
                        metadata: None,
                        display_content: Vec::new(),
                        attachment: None,
                    };
                }
            }
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                return ToolExecResult::Immediate {
                    content: format!("tool arg table: {e}"),
                    is_error: true,
                    metadata: None,
                    display_content: Vec::new(),
                    attachment: None,
                };
            }
        };

        let ctx_table = match build_tool_ctx(
            &self.lua,
            invocation.invocation_id,
            call_id,
            mode,
            session_id,
            artifact_dir,
        ) {
            Ok(t) => t,
            Err(e) => {
                return ToolExecResult::Immediate {
                    content: format!("tool ctx table: {e}"),
                    is_error: true,
                    metadata: None,
                    display_content: Vec::new(),
                    attachment: None,
                };
            }
        };

        let mut initial = mlua::MultiValue::new();
        initial.push_back(mlua::Value::Table(args_table));
        initial.push_back(mlua::Value::Table(ctx_table));

        let timeout_ms = self.tool_timeout_ms(tool_name, args);
        let deadline = timeout_ms.map(|ms| super::task::TaskDeadline {
            at: now + std::time::Duration::from_millis(ms),
            label_ms: ms,
            paused_at: None,
        });
        let task_opt = {
            let mut rt = match self.shared.tasks.lock() {
                Ok(g) => g,
                Err(_) => {
                    return ToolExecResult::Immediate {
                        content: "task runtime poisoned".into(),
                        is_error: true,
                        metadata: None,
                        display_content: Vec::new(),
                        attachment: None,
                    };
                }
            };
            let task_id = match rt.spawn_scoped(
                &self.lua,
                func,
                initial,
                TaskCompletion::ToolResult {
                    invocation,
                    call_id: call_id.to_string(),
                },
                super::task::TaskScope::Turn,
                deadline,
            ) {
                Ok(id) => id,
                Err(e) => {
                    return ToolExecResult::Immediate {
                        content: format!("tool spawn: {e}"),
                        is_error: true,
                        metadata: None,
                        display_content: Vec::new(),
                        attachment: None,
                    };
                }
            };
            rt.take_task(task_id)
        };
        // Single-step the freshly-spawned task: if the handler yields, callers
        // get `Pending` and the task is parked for the next `drive_tasks` tick.
        let mut outputs = Vec::new();
        if let Some(task) = task_opt {
            if self
                .step_task_outside_runtime_lock(task, now, &mut outputs)
                .is_err()
            {
                return ToolExecResult::Immediate {
                    content: "task runtime poisoned".into(),
                    is_error: true,
                    metadata: None,
                    display_content: Vec::new(),
                    attachment: None,
                };
            }
        }

        let mut immediate: Option<LuaToolResultParts> = None;
        for out in outputs {
            match out {
                TaskDriveOutput::ToolComplete {
                    invocation: completed_invocation,
                    call_id: cid,
                    content,
                    is_error,
                    metadata,
                    display_content,
                    attachment,
                } if completed_invocation == invocation && cid == call_id => {
                    immediate = Some(LuaToolResultParts {
                        content,
                        is_error,
                        metadata,
                        display_content,
                        attachment: attachment.map(|attachment| *attachment),
                    });
                }
                TaskDriveOutput::ToolComplete { .. } => {}
                TaskDriveOutput::NotifyError(msg) => self.record_error(msg),
            }
        }
        match immediate {
            Some(LuaToolResultParts {
                content,
                is_error,
                metadata,
                display_content,
                attachment,
            }) => ToolExecResult::Immediate {
                content,
                is_error,
                metadata,
                display_content,
                attachment: attachment.map(Box::new),
            },
            None => ToolExecResult::Pending,
        }
    }

    fn register_api(
        lua: &Lua,
        shared: &Arc<LuaShared>,
        state_root: &std::path::Path,
        cache_root: &std::path::Path,
    ) -> LuaResult<()> {
        let smelt = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        crate::lua::api::register_host_api(
            lua,
            &smelt,
            &smelt_keymap,
            shared,
            state_root,
            cache_root,
        )?;

        lua.globals().set("smelt", smelt)?;
        lua.globals().set("smelt_keymap", smelt_keymap)?;

        Ok(())
    }
}

fn view_state_label(view_state: crate::transcript_model::ViewState) -> &'static str {
    match view_state {
        crate::transcript_model::ViewState::Expanded => "expanded",
        crate::transcript_model::ViewState::Peek => "peek",
        crate::transcript_model::ViewState::Collapsed => "collapsed",
        crate::transcript_model::ViewState::TrimmedHead { .. } => "trimmed_head",
        crate::transcript_model::ViewState::TrimmedTail { .. } => "trimmed_tail",
    }
}

fn transcript_render_ctx_to_lua_table(
    lua: &Lua,
    view_state: crate::transcript_model::ViewState,
    renderer_generation: u64,
    now_ms: u64,
) -> LuaResult<mlua::Table> {
    let ctx = lua.create_table()?;
    ctx.set("view_state", view_state_label(view_state))?;
    ctx.set("renderer_generation", renderer_generation)?;
    ctx.set("surface", "transcript")?;
    ctx.set("now_ms", now_ms)?;

    let configured_limits = transcript_limits_table(lua)?;
    let default_rows = crate::content::block_layout::DEFAULT_TOOL_BLOCK_ROWS;
    let tool_rows =
        transcript_limit(configured_limits.as_ref(), "tool_rows")?.unwrap_or(default_rows);

    let limits = lua.create_table()?;
    limits.set(
        "tool_header_rows",
        transcript_limit(configured_limits.as_ref(), "tool_header_rows")?.unwrap_or(tool_rows),
    )?;
    limits.set(
        "tool_body_rows",
        transcript_limit(configured_limits.as_ref(), "tool_body_rows")?.unwrap_or(tool_rows),
    )?;
    limits.set(
        "tool_output_rows",
        transcript_limit(configured_limits.as_ref(), "tool_output_rows")?.unwrap_or(tool_rows),
    )?;
    limits.set(
        "collapsed_error_rows",
        transcript_limit(configured_limits.as_ref(), "collapsed_error_rows")?.unwrap_or(4),
    )?;
    limits.set(
        "thinking_peek_rows",
        transcript_limit(configured_limits.as_ref(), "thinking_peek_rows")?.unwrap_or(4),
    )?;
    limits.set(
        "thinking_peek_head_rows",
        transcript_limit(configured_limits.as_ref(), "thinking_peek_head_rows")?.unwrap_or(1),
    )?;
    ctx.set("limits", limits)?;
    Ok(ctx)
}

fn transcript_limits_table(lua: &Lua) -> LuaResult<Option<mlua::Table>> {
    let globals = lua.globals();
    let Some(smelt) = globals.get::<Option<mlua::Table>>("smelt")? else {
        return Ok(None);
    };
    let Some(settings) = smelt.get::<Option<mlua::Table>>("settings")? else {
        return Ok(None);
    };
    let Some(transcript) = settings.get::<Option<mlua::Table>>("transcript")? else {
        return Ok(None);
    };
    transcript.get::<Option<mlua::Table>>("limits")
}

fn transcript_limit(limits: Option<&mlua::Table>, key: &str) -> LuaResult<Option<u16>> {
    let Some(limits) = limits else {
        return Ok(None);
    };
    let value = limits.get::<mlua::Value>(key)?;
    match value {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Integer(value) if value >= 1 => {
            Ok(Some(value.min(i64::from(u16::MAX)) as u16))
        }
        mlua::Value::Number(value) if value.is_finite() && value >= 1.0 => {
            Ok(Some(value.floor().min(f64::from(u16::MAX)) as u16))
        }
        _ => Err(mlua::Error::external(format!(
            "smelt.settings.transcript.limits.{key}: expected a positive number"
        ))),
    }
}

fn set_content_metadata(table: &mlua::Table, content: &TranscriptContentMetadata) -> LuaResult<()> {
    table.set("content_id", content.content_id)?;
    table.set("content_revision", content.content_revision)?;
    table.set("content_bytes", content.content_bytes)?;
    table.set("content_lines", content.content_lines)?;
    table.set("content_preview", content.content_preview.as_str())?;
    Ok(())
}

fn argument_fields_to_lua_table(
    lua: &Lua,
    fields: &[TranscriptArgumentFieldMetadata],
) -> LuaResult<mlua::Table> {
    let out = lua.create_table_with_capacity(fields.len(), 0)?;
    for (index, field) in fields.iter().enumerate() {
        let item = lua.create_table_with_capacity(0, 7)?;
        item.set("name", field.name.as_str())?;
        set_content_metadata(&item, &field.content)?;
        item.set("complete", field.complete)?;
        out.set(index + 1, item)?;
    }
    Ok(out)
}

fn tool_output_to_lua_table(
    lua: &Lua,
    output: &TranscriptToolOutputMetadata,
) -> LuaResult<mlua::Table> {
    let table = lua.create_table_with_capacity(0, 7)?;
    set_content_metadata(&table, &output.content)?;
    table.set("is_error", output.is_error)?;
    if let Some(metadata) = output.metadata.as_ref() {
        table.set("metadata", json_to_lua(lua, metadata)?)?;
    }
    if !output.content_fields.is_empty() {
        let fields = lua.create_table_with_capacity(0, output.content_fields.len())?;
        for field in &output.content_fields {
            let metadata = lua.create_table_with_capacity(0, 5)?;
            set_content_metadata(&metadata, &field.content)?;
            fields.set(field.name.as_str(), metadata)?;
        }
        table.set("content_fields", fields)?;
    }
    Ok(table)
}

fn transcript_render_node_to_lua_table(
    lua: &Lua,
    node: &TranscriptRenderNode,
) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set("id", node.id)?;
    table.set("index", node.index)?;
    table.set("kind", node.kind)?;
    match &node.fields {
        TranscriptRenderFields::User {
            text,
            user_lines,
            image_labels,
            sent_at_ms,
        } => {
            table.set("text", text.as_str())?;
            table.set("user_lines", crate::lua::serde_to_lua(lua, user_lines)?)?;
            table.set("image_labels", crate::lua::serde_to_lua(lua, image_labels)?)?;
            table.set("sent_at_ms", *sent_at_ms)?;
        }
        TranscriptRenderFields::Mode {
            text,
            icon,
            hl_group,
        } => {
            table.set("text", text.as_str())?;
            table.set("icon", icon.as_str())?;
            table.set("hl_group", hl_group.as_str())?;
        }
        TranscriptRenderFields::ProcessStatus { text, event } => {
            table.set("text", text.as_str())?;
            if let Some(event) = event {
                table.set("event", event.event_type)?;
                table.set("event_type", event.event_type)?;
                if let Some(process_id) = event.process_id.as_deref() {
                    table.set("process_id", process_id)?;
                }
                if let Some(exit_code) = event.exit_code {
                    table.set("exit_code", exit_code)?;
                }
                table.set("termination", event.termination.as_str())?;
                let event_data = lua.create_table_with_capacity(0, 4)?;
                event_data.set("event", event.event_type)?;
                if let Some(process_id) = event.process_id.as_deref() {
                    event_data.set("process_id", process_id)?;
                }
                if let Some(exit_code) = event.exit_code {
                    event_data.set("exit_code", exit_code)?;
                }
                event_data.set("termination", event.termination.as_str())?;
                table.set("event_data", event_data)?;
            }
        }
        TranscriptRenderFields::Thinking {
            title,
            summary_titles,
            content,
            reasoning_kind,
            thinking_summary,
        } => {
            if let Some(title) = title {
                table.set("title", title.as_str())?;
            }
            table.set(
                "summary_titles",
                crate::lua::serde_to_lua(lua, summary_titles)?,
            )?;
            set_content_metadata(&table, content)?;
            table.set(
                "reasoning_kind",
                crate::lua::serde_to_lua(lua, reasoning_kind)?,
            )?;
            table.set("thinking_summary", thinking_summary.as_str())?;
        }
        TranscriptRenderFields::Text { content } => set_content_metadata(&table, content)?,
        TranscriptRenderFields::Code { content, lang } => {
            table.set("content", content.as_str())?;
            table.set("lang", lang.as_str())?;
        }
        TranscriptRenderFields::ToolDraft {
            stream_id,
            call_id,
            name,
            summary,
            summary_text,
            args,
            argument_fields,
            raw_arguments,
            draft_finished,
        } => {
            table.set("stream_id", stream_id.as_str())?;
            if let Some(call_id) = call_id {
                table.set("call_id", call_id.as_str())?;
            }
            table.set("name", name.as_str())?;
            table.set("summary", crate::lua::serde_to_lua(lua, summary)?)?;
            table.set("summary_text", summary_text.as_str())?;
            table.set("args", args_to_lua_table(lua, args)?)?;
            table.set(
                "argument_fields",
                argument_fields_to_lua_table(lua, argument_fields)?,
            )?;
            table.set("raw_arguments_id", raw_arguments.content_id)?;
            table.set("raw_arguments_revision", raw_arguments.content_revision)?;
            table.set("raw_arguments_bytes", raw_arguments.content_bytes)?;
            table.set("raw_arguments_lines", raw_arguments.content_lines)?;
            table.set(
                "raw_arguments_preview",
                raw_arguments.content_preview.as_str(),
            )?;
            table.set("status", "drafting")?;
            table.set("draft", true)?;
            table.set("draft_finished", *draft_finished)?;
        }
        TranscriptRenderFields::ToolCall {
            call_id,
            name,
            summary,
            summary_text,
            args,
            argument_fields,
            status,
            called_at_ms,
            elapsed_ms,
            elapsed_active,
            user_message,
            preview_output,
            output,
        } => {
            table.set("call_id", call_id.as_str())?;
            table.set("name", name.as_str())?;
            table.set("summary", crate::lua::serde_to_lua(lua, summary)?)?;
            table.set("summary_text", summary_text.as_str())?;
            table.set("args", args_to_lua_table(lua, args)?)?;
            table.set(
                "argument_fields",
                argument_fields_to_lua_table(lua, argument_fields)?,
            )?;
            table.set("status", *status)?;
            if let Some(called_at_ms) = called_at_ms {
                table.set("called_at_ms", *called_at_ms)?;
            }
            if let Some(elapsed_ms) = elapsed_ms {
                table.set("elapsed_ms", *elapsed_ms)?;
            }
            table.set("elapsed_active", *elapsed_active)?;
            if let Some(user_message) = user_message {
                table.set("user_message", user_message.as_str())?;
            }
            if let Some(preview_output) = preview_output {
                table.set(
                    "preview_output",
                    tool_output_to_lua_table(lua, preview_output)?,
                )?;
            }
            if let Some(output) = output {
                table.set("output", tool_output_to_lua_table(lua, output)?)?;
            }
        }
        TranscriptRenderFields::Exec {
            command,
            command_spans,
            output,
        } => {
            table.set("command", command.as_str())?;
            table.set(
                "command_spans",
                crate::lua::serde_to_lua(lua, command_spans)?,
            )?;
            table.set("output_id", output.content_id)?;
            table.set("output_revision", output.content_revision)?;
            table.set("output_bytes", output.content_bytes)?;
            table.set("output_lines", output.content_lines)?;
            table.set("output_preview", output.content_preview.as_str())?;
        }
        TranscriptRenderFields::Summary { summary } => {
            table.set("summary", summary.as_str())?;
        }
        TranscriptRenderFields::Group {
            group_kind,
            name,
            bucket,
            view_state,
            children,
            child_ids,
        } => {
            table.set("group_kind", group_kind.as_str())?;
            table.set("name", name.as_str())?;
            table.set("bucket", bucket.as_str())?;
            table.set("view_state", *view_state)?;
            table.set("children", crate::lua::serde_to_lua(lua, children)?)?;
            let ids = lua.create_table_with_capacity(child_ids.len(), 0)?;
            for (index, id) in child_ids.iter().enumerate() {
                ids.set(index + 1, id.get())?;
            }
            table.set("child_ids", ids)?;
            table.set("child_count", child_ids.len())?;
        }
    }
    Ok(table)
}

const TRANSCRIPT_RENDER_FACTORY_KEY: &str = "__smelt_transcript_render_factory";
const TRANSCRIPT_RENDER_FACTORY: &str = r#"
return function(root, base_ctx)
  local depth = 0
  local render
  render = function(node, overrides)
    if type(node) ~= "table" then
      error("ctx.render: node must be a table", 2)
    end
    if overrides ~= nil and type(overrides) ~= "table" then
      error("ctx.render: overrides must be a table", 2)
    end
    depth = depth + 1
    if depth > 64 then
      depth = depth - 1
      error("ctx.render: maximum recursion depth exceeded", 2)
    end
    local ctx = {}
    for key, value in pairs(base_ctx) do ctx[key] = value end
    if overrides then
      for key, value in pairs(overrides) do
        if key ~= "view_state" then
          depth = depth - 1
          error("ctx.render: unsupported override " .. tostring(key), 2)
        end
        ctx[key] = value
      end
    end
    ctx.render = render
    local ok, result = pcall(root, node, ctx)
    depth = depth - 1
    if not ok then error(result, 0) end
    return result
  end
  return render
end
"#;

fn transcript_recursive_renderer(
    lua: &Lua,
    root: mlua::Function,
    base_ctx: mlua::Table,
) -> LuaResult<mlua::Function> {
    let factory = match lua.named_registry_value::<mlua::Function>(TRANSCRIPT_RENDER_FACTORY_KEY) {
        Ok(factory) => factory,
        Err(_) => {
            let factory = lua
                .load(TRANSCRIPT_RENDER_FACTORY)
                .eval::<mlua::Function>()?;
            lua.set_named_registry_value(TRANSCRIPT_RENDER_FACTORY_KEY, factory.clone())?;
            factory
        }
    };
    factory.call((root, base_ctx))
}

fn transcript_layout_from_lua_value(
    runtime: &LuaRuntime,
    result: mlua::Value,
    label: &str,
    fallback: impl FnOnce() -> BlockLayout,
) -> BlockLayout {
    match result {
        mlua::Value::UserData(ud) => match ud.borrow::<crate::lua::api::layout::LuaBlockLayout>() {
            Ok(layout) => layout.0.clone(),
            Err(e) => {
                runtime.record_error(format!("{label}: expected smelt.layout value: {e}"));
                fallback()
            }
        },
        mlua::Value::Nil => {
            runtime.record_error(format!(
                "{label}: returned nil; use smelt.layout.empty() to hide a node"
            ));
            fallback()
        }
        other => {
            runtime.record_error(format!(
                "{label}: expected smelt.layout value, got {}",
                other.type_name()
            ));
            fallback()
        }
    }
}

#[derive(Clone, Debug)]
pub struct TranscriptRenderNode {
    id: u64,
    index: usize,
    kind: &'static str,
    fields: TranscriptRenderFields,
}

#[derive(Clone, Debug)]
struct TranscriptContentMetadata {
    content_id: u64,
    content_revision: u64,
    content_bytes: usize,
    content_lines: usize,
    content_preview: String,
}

#[derive(Clone, Debug)]
struct TranscriptProcessStatusMetadata {
    event_type: &'static str,
    process_id: Option<String>,
    exit_code: Option<i32>,
    termination: protocol::JobTermination,
}

#[derive(Clone, Debug)]
struct TranscriptToolOutputContentFieldMetadata {
    name: String,
    content: TranscriptContentMetadata,
}

#[derive(Clone, Debug)]
struct TranscriptToolOutputMetadata {
    content: TranscriptContentMetadata,
    is_error: bool,
    metadata: Option<serde_json::Value>,
    content_fields: Vec<TranscriptToolOutputContentFieldMetadata>,
}

#[derive(Clone, Debug)]
struct TranscriptArgumentFieldMetadata {
    name: String,
    content: TranscriptContentMetadata,
    complete: bool,
}

#[derive(Clone, Debug)]
enum TranscriptRenderFields {
    User {
        text: String,
        user_lines: protocol::StyledLines,
        image_labels: Vec<String>,
        sent_at_ms: Option<u64>,
    },
    Mode {
        text: String,
        icon: String,
        hl_group: String,
    },
    ProcessStatus {
        text: String,
        event: Option<TranscriptProcessStatusMetadata>,
    },
    Thinking {
        title: Option<String>,
        summary_titles: Vec<String>,
        content: TranscriptContentMetadata,
        reasoning_kind: protocol::ReasoningKind,
        thinking_summary: String,
    },
    Text {
        content: TranscriptContentMetadata,
    },
    Code {
        content: String,
        lang: String,
    },
    ToolDraft {
        stream_id: String,
        call_id: Option<String>,
        name: String,
        summary: protocol::StyledLines,
        summary_text: String,
        args: HashMap<String, serde_json::Value>,
        argument_fields: Vec<TranscriptArgumentFieldMetadata>,
        raw_arguments: TranscriptContentMetadata,
        draft_finished: bool,
    },
    ToolCall {
        call_id: String,
        name: String,
        summary: protocol::StyledLines,
        summary_text: String,
        args: HashMap<String, serde_json::Value>,
        argument_fields: Vec<TranscriptArgumentFieldMetadata>,
        status: &'static str,
        called_at_ms: Option<u64>,
        elapsed_ms: Option<u64>,
        elapsed_active: bool,
        user_message: Option<String>,
        preview_output: Option<Box<TranscriptToolOutputMetadata>>,
        output: Option<Box<TranscriptToolOutputMetadata>>,
    },
    Exec {
        command: String,
        command_spans: Vec<protocol::StyledSpan>,
        output: TranscriptContentMetadata,
    },
    Summary {
        summary: String,
    },
    Group {
        group_kind: String,
        name: String,
        bucket: String,
        view_state: &'static str,
        children: Vec<crate::transcript_model::TranscriptGroupChildMetadata>,
        child_ids: Vec<BlockId>,
    },
}

impl TranscriptRenderNode {
    pub fn group(
        id: u64,
        index: usize,
        group_kind: String,
        bucket: String,
        view_state: &'static str,
        children: Vec<crate::transcript_model::TranscriptGroupChildMetadata>,
        child_ids: Vec<BlockId>,
    ) -> Self {
        Self {
            id,
            index,
            kind: "group",
            fields: TranscriptRenderFields::Group {
                name: group_kind.clone(),
                group_kind,
                bucket,
                view_state,
                children,
                child_ids,
            },
        }
    }

    pub fn kind(&self) -> &'static str {
        self.kind
    }

    pub fn index(&self) -> usize {
        self.index
    }
}

const LUA_CONTENT_PREVIEW_BYTES: usize = 4 * 1024;

fn transcript_content_metadata(
    content: &crate::transcript_content::TranscriptContent,
) -> TranscriptContentMetadata {
    let read = content.read();
    let mut content_preview = if read.len() <= LUA_CONTENT_PREVIEW_BYTES {
        read.to_string()
    } else {
        const OMISSION_MARKER: &str = "\n…\n";
        let payload_bytes = LUA_CONTENT_PREVIEW_BYTES.saturating_sub(OMISSION_MARKER.len());
        let head_end = payload_bytes / 2;
        let tail_bytes = payload_bytes.saturating_sub(head_end);
        let tail_start = read.len().saturating_sub(tail_bytes);
        format!(
            "{}{}{}",
            read.slice(0..head_end),
            OMISSION_MARKER,
            read.slice(tail_start..read.len())
        )
    };
    if content_preview.len() > LUA_CONTENT_PREVIEW_BYTES {
        content_preview =
            smelt_buffer::text::slice(&content_preview, 0..LUA_CONTENT_PREVIEW_BYTES).to_owned();
    }
    TranscriptContentMetadata {
        content_id: content.id().get(),
        content_revision: read.revision(),
        content_bytes: read.len(),
        content_lines: read.logical_line_count(),
        content_preview,
    }
}

fn transcript_argument_field_metadata(
    fields: &[crate::content::tool_draft::ToolArgumentField],
) -> Vec<TranscriptArgumentFieldMetadata> {
    fields
        .iter()
        .map(|field| TranscriptArgumentFieldMetadata {
            name: field.name.clone(),
            content: transcript_content_metadata(&field.content),
            complete: field.complete,
        })
        .collect()
}

fn transcript_tool_output_metadata(
    output: &crate::transcript_model::ToolOutput,
) -> TranscriptToolOutputMetadata {
    TranscriptToolOutputMetadata {
        content: transcript_content_metadata(&output.content),
        is_error: output.is_error,
        metadata: output.metadata.clone(),
        content_fields: output
            .content_fields
            .iter()
            .map(|field| TranscriptToolOutputContentFieldMetadata {
                name: field.name.clone(),
                content: transcript_content_metadata(&field.content),
            })
            .collect(),
    }
}

/// Build compact, typed renderer metadata for one transcript block.
pub fn transcript_block_render_node(
    id: BlockId,
    index: usize,
    block: &Block,
    state: Option<&ToolState>,
) -> TranscriptRenderNode {
    let fields = match block {
        Block::User {
            text,
            image_labels,
            command,
            sent_at_ms,
        } => TranscriptRenderFields::User {
            text: text.clone(),
            user_lines: user_styled_lines(text, image_labels, *command),
            image_labels: image_labels.clone(),
            sent_at_ms: *sent_at_ms,
        },
        Block::Mode {
            text,
            icon,
            hl_group,
        } => TranscriptRenderFields::Mode {
            text: text.clone(),
            icon: icon.clone(),
            hl_group: hl_group.clone(),
        },
        Block::ProcessStatus { text, event } => TranscriptRenderFields::ProcessStatus {
            text: text.clone(),
            event: event.as_ref().map(|event| TranscriptProcessStatusMetadata {
                event_type: event.event_type(),
                process_id: event.process_id().map(str::to_owned),
                exit_code: event.exit_code(),
                termination: event.termination(),
            }),
        },
        Block::Thinking {
            title,
            summary_titles,
            content,
            kind,
        } => {
            let (first_line, line_count) = {
                let content = content.read();
                (content.first_nonempty_line(), content.logical_line_count())
            };
            TranscriptRenderFields::Thinking {
                title: title.clone(),
                summary_titles: summary_titles.clone(),
                content: transcript_content_metadata(content),
                reasoning_kind: *kind,
                thinking_summary: thinking_fallback_summary(
                    title.as_deref(),
                    &first_line,
                    line_count,
                ),
            }
        }
        Block::Text { content } => TranscriptRenderFields::Text {
            content: transcript_content_metadata(content),
        },
        Block::CodeLine { content, lang } => TranscriptRenderFields::Code {
            content: content.clone(),
            lang: lang.clone(),
        },
        Block::ToolDraft(draft) => TranscriptRenderFields::ToolDraft {
            stream_id: draft.stream_id.clone(),
            call_id: draft.call_id.clone(),
            name: draft.name.clone(),
            summary: draft.summary.clone(),
            summary_text: draft.summary.as_plain_text(),
            args: draft.arguments.preview().clone(),
            argument_fields: transcript_argument_field_metadata(draft.string_fields()),
            raw_arguments: transcript_content_metadata(&draft.raw_arguments),
            draft_finished: draft.finished,
        },
        Block::ToolCall {
            call_id,
            name,
            summary,
            args,
        } => {
            let status = state
                .map(|state| state.status)
                .unwrap_or(ToolStatus::Pending);
            let elapsed_ms = state
                .and_then(|state| state.elapsed)
                .map(|elapsed| elapsed.as_millis() as u64);
            TranscriptRenderFields::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                summary: summary.clone(),
                summary_text: summary.as_plain_text(),
                args: args.preview().clone(),
                argument_fields: transcript_argument_field_metadata(args.string_fields()),
                status: status.label(),
                called_at_ms: state.and_then(|state| state.called_at_ms),
                elapsed_ms,
                elapsed_active: state.is_some_and(|state| state.elapsed_active),
                user_message: state.and_then(|state| state.user_message.clone()),
                preview_output: state
                    .and_then(|state| state.preview_output.as_deref())
                    .map(transcript_tool_output_metadata)
                    .map(Box::new),
                output: state
                    .and_then(|state| state.output.as_deref())
                    .map(transcript_tool_output_metadata)
                    .map(Box::new),
            }
        }
        Block::Exec { command, output } => TranscriptRenderFields::Exec {
            command: command.clone(),
            command_spans: exec_command_spans(command),
            output: transcript_content_metadata(output),
        },
        Block::Compacted { summary } | Block::CompactionPreview { summary } => {
            TranscriptRenderFields::Summary {
                summary: summary.clone(),
            }
        }
    };
    TranscriptRenderNode {
        id: id.0,
        index,
        kind: block.kind(),
        fields,
    }
}

fn args_to_lua_table(
    lua: &Lua,
    args: &HashMap<String, serde_json::Value>,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    for (key, value) in args {
        t.set(key.as_str(), json_to_lua(lua, value)?)?;
    }
    Ok(t)
}

fn user_styled_lines(text: &str, image_labels: &[String], command: bool) -> protocol::StyledLines {
    let lines = user_display_lines(text);
    let command_token_chars = if command {
        lines
            .first()
            .and_then(|line| crate::commands::command_token(line))
            .map(|token| token.chars().count())
            .unwrap_or(0)
    } else {
        0
    };
    protocol::StyledLines(
        lines
            .iter()
            .enumerate()
            .map(|(line_idx, line)| {
                let command_prefix_chars = if line_idx == 0 {
                    command_token_chars
                } else {
                    0
                };
                user_line_spans(line, image_labels, command_prefix_chars)
            })
            .collect(),
    )
}

fn user_display_lines(text: &str) -> Vec<String> {
    let all_lines: Vec<String> = text
        .lines()
        .map(|line| display_safe_text(&line.replace('\t', "    ")))
        .collect();
    let start = all_lines
        .iter()
        .position(|line| !line.is_empty())
        .unwrap_or(0);
    let end = all_lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |idx| idx + 1);
    all_lines[start..end]
        .iter()
        .map(|line| smelt_buffer::text::trim_end_whitespace(line).to_owned())
        .collect()
}

fn user_line_spans(
    text: &str,
    image_labels: &[String],
    command_prefix_chars: usize,
) -> Vec<protocol::StyledSpan> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut i = 0usize;

    let flush_plain = |out: &mut Vec<protocol::StyledSpan>, plain: &mut String| {
        if !plain.is_empty() {
            out.push(protocol::StyledSpan {
                text: std::mem::take(plain),
                bold: true,
                ..Default::default()
            });
        }
    };
    let push_accent = |out: &mut Vec<protocol::StyledSpan>, text: String| {
        if !text.is_empty() {
            out.push(protocol::StyledSpan {
                text,
                fg: Some("SmeltAccent".into()),
                bold: true,
                ..Default::default()
            });
        }
    };

    let take = command_prefix_chars.min(len);
    if take > 0 {
        push_accent(&mut out, chars[..take].iter().collect());
        i = take;
    }

    while i < len {
        if chars[i] == '[' {
            let remaining: String = chars[i..].iter().collect();
            if let Some(label) = image_labels
                .iter()
                .find(|label| remaining.starts_with(label.as_str()))
            {
                flush_plain(&mut out, &mut plain);
                push_accent(&mut out, label.clone());
                i += label.chars().count();
                continue;
            }
        }

        if let Some((token, end)) = crate::content::selection::try_at_ref(&chars, i) {
            flush_plain(&mut out, &mut plain);
            push_accent(&mut out, token);
            i = end;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    flush_plain(&mut out, &mut plain);
    out
}

fn exec_command_spans(command: &str) -> Vec<protocol::StyledSpan> {
    vec![
        protocol::StyledSpan {
            text: "!".into(),
            fg: Some("SmeltExecPrefix".into()),
            bold: true,
            ..Default::default()
        },
        protocol::StyledSpan {
            text: display_safe_text(command),
            bold: true,
            ..Default::default()
        },
    ]
}

fn layout_text(content: impl Into<String>, hl_group: Option<&str>, ansi: bool) -> BlockLayout {
    BlockLayout::Leaf(LuaLeaf::Text(TextSpec {
        content: content.into(),
        hl_group: hl_group.map(str::to_string),
        ansi,
    }))
}

fn fallback_transcript_node_layout(node: &TranscriptRenderNode) -> BlockLayout {
    let content = match &node.fields {
        TranscriptRenderFields::Thinking { content, .. }
        | TranscriptRenderFields::Text { content }
        | TranscriptRenderFields::Exec {
            output: content, ..
        } => Some(content),
        TranscriptRenderFields::ToolDraft { raw_arguments, .. } => Some(raw_arguments),
        TranscriptRenderFields::ToolCall { output, .. } => {
            output.as_ref().map(|output| &output.content)
        }
        _ => None,
    };
    if let Some(content) = content {
        let render = if matches!(node.kind, "assistant" | "thinking") {
            ContentRenderSpec::Markdown {
                dim: node.kind == "thinking",
                italic: node.kind == "thinking",
                inline: false,
            }
        } else {
            ContentRenderSpec::Text {
                hl_group: Some("ErrorMsg".into()),
                ansi: matches!(node.kind, "exec" | "tool"),
            }
        };
        return BlockLayout::Leaf(LuaLeaf::Content(ContentSpec {
            id: crate::transcript_content::ContentId::new(content.content_id),
            render,
        }));
    }

    let text = match &node.fields {
        TranscriptRenderFields::User { text, .. }
        | TranscriptRenderFields::Mode { text, .. }
        | TranscriptRenderFields::ProcessStatus { text, .. }
        | TranscriptRenderFields::Code { content: text, .. }
        | TranscriptRenderFields::Summary { summary: text } => text.as_str(),
        TranscriptRenderFields::ToolDraft {
            summary_text, name, ..
        }
        | TranscriptRenderFields::ToolCall {
            summary_text, name, ..
        } => {
            if summary_text.is_empty() {
                name.as_str()
            } else {
                summary_text.as_str()
            }
        }
        TranscriptRenderFields::Group { name, .. } => name.as_str(),
        TranscriptRenderFields::Thinking {
            thinking_summary, ..
        } => thinking_summary.as_str(),
        TranscriptRenderFields::Text { .. } | TranscriptRenderFields::Exec { .. } => node.kind,
    };
    layout_text(text, Some("ErrorMsg"), false)
}

fn thinking_fallback_summary(
    title: Option<&str>,
    first_nonempty_line: &str,
    line_count: usize,
) -> String {
    if let Some(title) = title.filter(|_| line_count == 0) {
        return title.to_string();
    }
    let inferred_label = thinking_summary_label(first_nonempty_line);
    let label = title.unwrap_or(&inferred_label);
    let collapsed_lines = if title.is_some() || inferred_label == "thinking" {
        line_count
    } else {
        line_count.saturating_sub(1)
    };
    format!(
        "{label}\n… {} collapsed …",
        pluralize(collapsed_lines, "line", "lines")
    )
}

fn thinking_summary_label(first_nonempty_line: &str) -> String {
    let trimmed = smelt_buffer::text::trim_whitespace(first_nonempty_line);
    trimmed
        .strip_prefix("**")
        .and_then(|label| label.strip_suffix("**"))
        .map(smelt_buffer::text::trim_whitespace)
        .filter(|label| !label.is_empty())
        .unwrap_or("thinking")
        .to_string()
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

pub fn load_host_bootstrap_chunks(lua: &Lua) -> mlua::Result<()> {
    load_bootstrap_chunks_from_process(lua, BootstrapMode::Host)
}

pub fn load_bootstrap_chunks(lua: &Lua) -> mlua::Result<()> {
    load_bootstrap_chunks_from_process(lua, BootstrapMode::Full)
}

fn load_bootstrap_chunks_from_process(lua: &Lua, mode: BootstrapMode) -> mlua::Result<()> {
    let paths = LuaLoadPaths::from_process();
    load_bootstrap_group_with_roots(
        lua,
        mode,
        &paths.module_overlay_roots(),
        paths.development_runtime.as_deref(),
        None,
    )
}

fn load_bootstrap_group_with_roots(
    lua: &Lua,
    mode: BootstrapMode,
    roots: &[PathBuf],
    trusted_root: Option<&std::path::Path>,
    loaded_files: Option<&Arc<Mutex<Vec<PathBuf>>>>,
) -> mlua::Result<()> {
    for chunk in BOOTSTRAP_CHUNKS.iter().filter(|chunk| mode.includes(chunk)) {
        let rel = chunk.path;
        let (source, name, path, trusted) =
            read_bootstrap_source_from_roots(rel, roots, trusted_root)?;
        if let (Some(files), Some(path)) = (loaded_files, path) {
            files
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(path);
        }
        lua.load(&source)
            .set_name(name)
            .set_environment(crate::lua::module::bootstrap_chunk_environment(
                lua, trusted,
            )?)
            .exec()?;
    }
    Ok(())
}

/// Extract the embedded `runtime/lua/smelt/` tree to
/// `<data_dir>/builtins/lua/smelt/` so the agent (and humans) can inspect
/// the built-in source as worked examples. Versioned by `CARGO_PKG_VERSION`:
/// re-extracts on smelt upgrade, skips otherwise.
///
/// Best-effort. Returns the target directory on success, or the I/O
/// error on failure - callers should log and continue, since the
/// runtime stays fully functional from the embedded copy.
///
/// This is intentionally separate from the user-overlay path
/// (`<XDG_DATA_HOME>/smelt/runtime/`) so user overrides don't get
/// clobbered on upgrade.
pub fn ensure_builtins_extracted(data_dir: &std::path::Path) -> std::io::Result<PathBuf> {
    let target = data_dir.join("builtins");
    let version_file = target.join(".version");
    let expected = env!("CARGO_PKG_VERSION");
    if let Ok(found) = std::fs::read_to_string(&version_file) {
        let has_lua = target.join("lua").join("smelt").exists();
        let has_skills = target.join("skills").exists();
        if found.trim() == expected && has_lua && has_skills {
            return Ok(target);
        }
    }
    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    let lua_root = target.join("lua").join("smelt");
    std::fs::create_dir_all(&lua_root)?;
    write_dir_recursive(&EMBEDDED_LUA, &lua_root)?;

    let skills_root = target.join("skills");
    std::fs::create_dir_all(&skills_root)?;
    write_dir_recursive(&EMBEDDED_SKILLS, &skills_root)?;

    std::fs::write(&version_file, expected)?;
    Ok(target)
}

fn write_dir_recursive(dir: &Dir<'_>, target: &std::path::Path) -> std::io::Result<()> {
    for entry in dir.entries() {
        match entry {
            DirEntry::File(f) => {
                let rel = f.path();
                let dest = target.join(rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, f.contents())?;
            }
            DirEntry::Dir(d) => {
                let dest = target.join(d.path());
                std::fs::create_dir_all(&dest)?;
                write_dir_recursive(d, target)?;
            }
        }
    }
    Ok(())
}

/// Resolve a bootstrap-file relative path to its source, walking the
/// same disk-overlay roots as `require()` before falling back to the
/// baked-in [`EMBEDDED_LUA`] snapshot. Lets `dialog.lua`, `cmd.lua`,
/// etc. hot-reload from disk on `/reload` - same dev-loop parity as
/// autoloaded plugins. Returns `(source, chunk_name)`; the chunk name
/// reflects where the source actually came from so Lua tracebacks
/// point at the file you're editing.
fn read_bootstrap_source_from_roots(
    rel: &str,
    roots: &[PathBuf],
    trusted_root: Option<&std::path::Path>,
) -> mlua::Result<(String, String, Option<PathBuf>, bool)> {
    for root in roots {
        let candidate = root.join("smelt").join(rel);
        if let Ok(source) = std::fs::read_to_string(&candidate) {
            let trusted = trusted_root.is_some_and(|root| candidate.starts_with(root));
            let name = candidate.display().to_string();
            return Ok((source, name, Some(candidate), trusted));
        }
    }
    let file = EMBEDDED_LUA.get_file(rel).ok_or_else(|| {
        LuaError::RuntimeError(format!("missing embedded bootstrap chunk: {rel}"))
    })?;
    let source = file
        .contents_utf8()
        .ok_or_else(|| LuaError::RuntimeError(format!("bootstrap chunk not utf-8: {rel}")))?
        .to_string();
    Ok((source, format!("smelt/{rel}"), None, true))
}

fn embedded_lua_modules() -> impl Iterator<Item = (String, &'static str)> {
    fn walk(dir: &'static Dir<'static>, out: &mut Vec<(String, &'static str)>) {
        for entry in dir.entries() {
            match entry {
                DirEntry::File(f) => {
                    let path = f.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("lua") {
                        continue;
                    }
                    let Some(rel) = path.to_str() else { continue };
                    let module = path_to_module(rel);
                    if let Some(src) = f.contents_utf8() {
                        out.push((module, src));
                    }
                }
                DirEntry::Dir(d) => walk(d, out),
            }
        }
    }
    let mut out = Vec::new();
    walk(&EMBEDDED_LUA, &mut out);
    out.into_iter()
}

fn path_to_module(rel: &str) -> String {
    let trimmed = rel.strip_suffix(".lua").unwrap_or(rel);
    let dotted = trimmed.replace('/', ".");
    format!("smelt.{dotted}")
}

/// Modules to `require` at startup; sorted per directory, bootstrap files excluded.
///
/// `disabled` lets callers (typically the TUI runtime after `early.lua`
/// has run) skip a user-opted-out module. Pass an empty set for
/// no-filter behavior.
pub fn autoload_modules_filtered(disabled: &std::collections::HashSet<String>) -> Vec<String> {
    autoload_modules_filtered_from(AUTOLOAD_DIRS, disabled)
}

fn autoload_modules_filtered_from(
    directories: &[&str],
    disabled: &std::collections::HashSet<String>,
) -> Vec<String> {
    let bootstrap_modules: std::collections::HashSet<String> = BOOTSTRAP_CHUNKS
        .iter()
        .map(|chunk| path_to_module(chunk.path))
        .collect();
    let mut out = Vec::new();
    for dir_name in directories {
        let Some(dir) = EMBEDDED_LUA.get_dir(*dir_name) else {
            continue;
        };
        let mut names: Vec<String> = dir
            .files()
            .filter(|f| f.path().extension().and_then(|s| s.to_str()) == Some("lua"))
            .filter_map(|f| f.path().to_str().map(path_to_module))
            .filter(|m| !bootstrap_modules.contains(m))
            .filter(|m| !OPTIONAL_PLUGINS.contains(&m.as_str()))
            .filter(|m| m != CUSTOM_COMMANDS_MODULE)
            .filter(|m| !disabled.contains(m))
            .collect();
        names.sort();
        out.extend(names);
    }
    out
}

/// Backwards-compatible wrapper that applies no user filter. Prefer
/// `autoload_modules_filtered` when a `LuaShared::disabled_modules` set
/// is available.
pub fn autoload_modules() -> Vec<String> {
    autoload_modules_filtered(&std::collections::HashSet::new())
}

/// Bundled `runtime/lua/smelt/early/*.lua` modules to `require` during the
/// Early phase. Sorted lex order for deterministic registration.
pub fn early_modules() -> Vec<String> {
    let mut out = Vec::new();
    for dir_name in EARLY_DIRS {
        let Some(dir) = EMBEDDED_LUA.get_dir(*dir_name) else {
            continue;
        };
        let mut names: Vec<String> = dir
            .files()
            .filter(|f| f.path().extension().and_then(|s| s.to_str()) == Some("lua"))
            .filter_map(|f| f.path().to_str().map(path_to_module))
            .collect();
        names.sort();
        out.extend(names);
    }
    out
}

fn register_module_searcher_with_roots(
    lua: &Lua,
    roots: Vec<PathBuf>,
    trusted_root: Option<PathBuf>,
    loaded_files: Option<Arc<Mutex<Vec<PathBuf>>>>,
) -> LuaResult<()> {
    let modules: HashMap<String, &'static str> = embedded_lua_modules().collect();
    let searcher = lua.create_function(move |lua, module: String| {
        let rel = module_to_relpath(&module);
        for root in &roots {
            let path = root.join(&rel);
            if let Ok(source) = std::fs::read_to_string(&path) {
                if let Some(files) = &loaded_files {
                    files
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(path.clone());
                }
                let name = path.display().to_string();
                let mut chunk = lua.load(source).set_name(name);
                if trusted_root
                    .as_ref()
                    .is_some_and(|root| path.starts_with(root))
                {
                    chunk =
                        chunk.set_environment(crate::lua::module::bundled_chunk_environment(lua)?);
                }
                let loader = chunk.into_function()?;
                // Push an unnamed loader frame so the required module can opt
                // in to hot-reload survival via `smelt.plugin(name)`.
                let wrapped = wrap_in_scope(lua, loader.clone()).unwrap_or(loader);
                return Ok(mlua::Value::Function(wrapped));
            }
        }
        if let Some(source) = modules.get(&module) {
            let loader = lua
                .load(*source)
                .set_name(module.as_str())
                .set_environment(crate::lua::module::bundled_chunk_environment(lua)?)
                .into_function()?;
            let wrapped = wrap_in_scope(lua, loader.clone()).unwrap_or(loader);
            return Ok(mlua::Value::Function(wrapped));
        }
        Ok(mlua::Value::String(lua.create_string(format!(
            "\n\tno embedded module '{module}'"
        ))?))
    })?;

    let package: mlua::Table = lua.globals().get("package")?;
    let searchers: mlua::Table = package.get("searchers")?;
    let len = searchers.raw_len();
    searchers.raw_set(len + 1, searcher)?;
    Ok(())
}

/// Wrap a Lua loader function so its body runs inside a fresh private scope
/// frame. The frame starts unnamed; the module body opts in to hot-reload
/// survival via `smelt.plugin(name)`.
fn wrap_in_scope(lua: &Lua, loader: mlua::Function) -> LuaResult<mlua::Function> {
    let with_scope = crate::lua::module::internal_api_function(lua, "smelt", "__with_scope")?;
    lua.create_function(move |_, args: mlua::MultiValue| {
        let mut call_args = mlua::MultiValue::new();
        call_args.push_back(mlua::Value::Function(loader.clone()));
        for a in args {
            call_args.push_back(a);
        }
        with_scope.call::<mlua::MultiValue>(call_args)
    })
}

fn module_to_relpath(module: &str) -> PathBuf {
    let mut path = PathBuf::from(module.replace('.', "/"));
    path.set_extension("lua");
    path
}

fn lua_files_in(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lua"))
        .collect();
    out.sort();
    out
}

pub fn init_lua_path() -> Option<PathBuf> {
    Some(engine::config_dir().join("init.lua"))
}

fn build_tool_ctx(
    lua: &Lua,
    invocation_id: protocol::InvocationId,
    call_id: &str,
    mode: protocol::AgentMode,
    session_id: &str,
    artifact_dir: &std::path::Path,
) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    t.set("invocation_id", invocation_id.get())?;
    t.set("call_id", call_id.to_string())?;
    t.set("mode", mode.as_str())?;
    t.set("session_id", session_id.to_string())?;
    t.set("artifact_dir", artifact_dir.to_string_lossy().into_owned())?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_command_semantics_accent_only_the_slash_token() {
        let lines = user_styled_lines("/retired-command argument", &[], true);
        let spans = &lines.0[0];

        assert_eq!(spans[0].text, "/retired-command");
        assert_eq!(spans[0].fg.as_deref(), Some("SmeltAccent"));
        assert_eq!(spans[1].text, " argument");
        assert_eq!(spans[1].fg, None);

        let normal = user_styled_lines("/retired-command argument", &[], false);
        assert!(normal.0[0].iter().all(|span| span.fg.is_none()));
    }

    #[test]
    fn thinking_summary_counts_retained_lines_without_copying_content() {
        assert_eq!(
            thinking_fallback_summary(None, "**Inspecting the renderer**", 7),
            "Inspecting the renderer\n… 6 lines collapsed …"
        );
        assert_eq!(
            thinking_fallback_summary(None, "Read the transcript model first.", 6),
            "thinking\n… 6 lines collapsed …"
        );
    }

    #[test]
    fn title_only_thinking_summary_has_no_empty_line_count() {
        assert_eq!(
            thinking_fallback_summary(Some("Checking files"), "", 0),
            "Checking files"
        );
    }

    #[test]
    fn snapshot_fallback_trimming_keeps_graphemes_atomic() {
        assert_eq!(user_display_lines("x\u{600} "), vec!["x\u{600} "]);
        assert_eq!(
            thinking_summary("** \u{301}title\u{600} **"),
            (" \u{301}title\u{600} ".to_string(), 1)
        );
    }

    #[test]
    fn module_relpath_translates_dots_to_slashes() {
        assert_eq!(
            module_to_relpath("smelt.dialogs.confirm"),
            PathBuf::from("smelt/dialogs/confirm.lua")
        );
        assert_eq!(module_to_relpath("smelt"), PathBuf::from("smelt.lua"));
    }

    #[test]
    fn path_to_module_translates_slashes_to_dots() {
        assert_eq!(
            path_to_module("dialogs/confirm.lua"),
            "smelt.dialogs.confirm"
        );
        assert_eq!(path_to_module("modes.lua"), "smelt.modes");
    }

    #[test]
    fn autoload_covers_tools_commands_plugins() {
        let modules = autoload_modules();
        assert!(modules.contains(&"smelt.tools.bash".to_string()));
        assert!(modules.contains(&"smelt.commands.btw".to_string()));
        assert!(modules.contains(&"smelt.commands.mcp".to_string()));
        assert!(modules.contains(&"smelt.plugins.esc_chord".to_string()));
        assert!(modules.contains(&"smelt.plugins.plan_mode".to_string()));
    }

    #[test]
    fn autoload_defers_custom_markdown_commands() {
        let modules = autoload_modules();
        assert!(!modules.contains(&CUSTOM_COMMANDS_MODULE.to_string()));
    }

    #[test]
    fn command_busy_callback_resolves_arguments_and_is_not_called_by_listing() {
        use crate::lua::CommandBusyBehavior;
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
            local calls = 0
            local busy_fn = function(arg)
                calls = calls + 1
                -- Registry access from the callback must not deadlock.
                assert(#smelt.cmd.list() == 1)
                if arg == "status" then return "run" end
                if arg == "reject" then return "reject" end
                if arg == "request" then return "queue_request" end
                if arg ~= nil then return "queue_command" end
            end
            smelt.cmd.register("probe", function() end, {
                busy = "reject", busy_fn = busy_fn,
            })
            local row = smelt.cmd.list()[1]
            assert(row.busy == "reject" and row.busy_fn == busy_fn)
            assert(calls == 0)
        "#,
            )
            .exec()
            .unwrap();
        for (arg, expected) in [
            (None, CommandBusyBehavior::Reject),
            (Some("status"), CommandBusyBehavior::Run),
            (Some("reject"), CommandBusyBehavior::Reject),
            (Some("request"), CommandBusyBehavior::QueueRequest),
            (Some("create objective"), CommandBusyBehavior::QueueCommand),
        ] {
            assert_eq!(
                rt.command_busy_behavior("probe", arg).unwrap(),
                Some(expected)
            );
        }
        assert_eq!(rt.command_busy_behavior("unknown", None).unwrap(), None);
    }

    #[test]
    fn command_busy_callback_errors_do_not_fall_back_to_running() {
        let rt = LuaRuntime::new();
        for body in [
            "error('policy failed')",
            "return 'invalid'",
            "return {}",
            "coroutine.yield()",
        ] {
            rt.lua
                .load(format!(
                    r#"
                smelt.cmd.register("probe", function() end, {{
                    override = true,
                    busy = "run",
                    busy_fn = function() {body} end,
                }})
            "#
                ))
                .exec()
                .unwrap();
            assert!(rt.command_busy_behavior("probe", None).is_err(), "{body}");
        }
        rt.lua
            .load(
                r#"
            smelt.cmd.register("probe", function() end, { override = true, busy = "queue_command" })
        "#,
            )
            .exec()
            .unwrap();
        assert_eq!(
            rt.command_busy_behavior("probe", None).unwrap(),
            Some(crate::lua::CommandBusyBehavior::QueueCommand)
        );
    }

    #[test]
    fn command_register_requires_explicit_override_for_duplicates() {
        let rt = LuaRuntime::new();
        let (ok, err): (bool, String) = rt
            .lua
            .load(
                r#"
                smelt.cmd.register("same", function() end)
                local ok, err = pcall(smelt.cmd.register, "same", function() end)
                return ok, tostring(err)
                "#,
            )
            .eval()
            .unwrap();
        assert!(!ok);
        assert!(err.contains("override = true"));
    }

    #[test]
    fn command_register_override_replaces_without_old_reg_removing_new_entry() {
        let rt = LuaRuntime::new();
        let names: Vec<String> = rt
            .lua
            .load(
                r#"
                local old = smelt.cmd.register("same", function() end)
                smelt.cmd.register("same", function() end, { override = true })
                old:remove()
                local names = {}
                for _, row in ipairs(smelt.cmd.list()) do names[#names + 1] = row.name end
                return names
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(names, vec!["same"]);
    }

    #[test]
    fn tool_schema_preserves_parameter_property_order() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                    name = "order_probe",
                    description = "test tool",
                    parameters = {
                        type = "object",
                        properties = {
                            file_path = { type = "string" },
                            content = { type = "string" },
                        },
                        required = { "file_path", "content" },
                    },
                    execute = function(args) return "ok" end,
                })
                "#,
            )
            .exec()
            .unwrap();

        let defs = rt.tool_defs(protocol::AgentMode::normal(), ToolVisibility::Interactive);
        let def = defs
            .iter()
            .find(|def| def.name == "order_probe")
            .expect("registered tool definition");
        let keys: Vec<_> = def.parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();

        assert_eq!(keys, vec!["file_path", "content"]);
    }

    #[test]
    fn autoload_excludes_optional_plugins() {
        let modules = autoload_modules();
        for optional in OPTIONAL_PLUGINS {
            assert!(
                !modules.contains(&optional.to_string()),
                "optional plugin must not be autoloaded: {optional}"
            );
        }
        for optional in OPTIONAL_PLUGINS {
            let rel = optional.strip_prefix("smelt.").unwrap().replace('.', "/") + ".lua";
            assert!(
                EMBEDDED_LUA.get_file(&rel).is_some(),
                "optional plugin must still be embedded so users can require it: {optional}"
            );
        }
    }

    #[test]
    fn bootstrap_manifest_paths_are_unique_and_embedded() {
        let mut paths = std::collections::HashSet::new();
        for chunk in BOOTSTRAP_CHUNKS {
            assert!(
                paths.insert(chunk.path),
                "duplicate bootstrap manifest path: {}",
                chunk.path
            );
            assert!(
                EMBEDDED_LUA.get_file(chunk.path).is_some(),
                "bootstrap file missing from embedded tree: {}",
                chunk.path
            );
        }

        let completer = BOOTSTRAP_CHUNKS
            .iter()
            .filter(|chunk| chunk.path == "widgets/completer.lua")
            .collect::<Vec<_>>();
        assert_eq!(completer.len(), 1);
        assert!(completer[0].loads_in_host());
    }

    #[test]
    fn bundled_lua_does_not_read_private_hooks_from_the_public_facade() {
        for (module, source) in embedded_lua_modules() {
            for (index, line) in source.lines().enumerate() {
                let has_public_private_lookup = line
                    .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.')))
                    .filter(|token| token.starts_with("smelt."))
                    .any(|token| token.split('.').skip(1).any(|part| part.starts_with("__")));
                assert!(
                    !has_public_private_lookup,
                    "{module}:{} reads a private hook through the public smelt facade: {line}",
                    index + 1
                );
            }
        }
    }

    #[test]
    fn bootstrap_keeps_persistent_state_flush_hook_private() {
        let rt = LuaRuntime::new();

        let publicly_visible: bool = rt
            .lua
            .load("return smelt.__flush_persistent_state ~= nil")
            .eval()
            .unwrap();
        assert!(!publicly_visible);
        assert!(crate::lua::module::internal_api_function(
            &rt.lua,
            "smelt",
            "__flush_persistent_state"
        )
        .is_ok());
        assert!(rt.flush_persistent_state().is_none());
    }

    fn runtime_without_builtin_groups() -> LuaRuntime {
        let rt = LuaRuntime::new();
        {
            let mut registry = rt
                .shared
                .transcript_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry.entries.clear();
            registry.next_order = 0;
        }
        rt.shared
            .transcript_groups_cache_key
            .store(0, std::sync::atomic::Ordering::Release);
        rt.shared
            .transcript_groups_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        rt
    }

    #[test]
    fn process_status_block_table_exposes_typed_event_fields() {
        let lua = Lua::new();
        let block = Block::ProcessStatus {
            text: "background process 42 exited with code 7".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "42",
                Some(7),
                protocol::JobTermination::Exited,
            )),
        };

        let node = transcript_block_render_node(BlockId::new(9), 3, &block, None);
        let table = transcript_render_node_to_lua_table(&lua, &node).unwrap();

        assert_eq!(table.get::<String>("kind").unwrap(), "process_status");
        assert_eq!(
            table.get::<String>("event").unwrap(),
            "background_process_completed"
        );
        assert_eq!(
            table.get::<String>("event_type").unwrap(),
            "background_process_completed"
        );
        assert_eq!(table.get::<String>("process_id").unwrap(), "42");
        assert_eq!(table.get::<i32>("exit_code").unwrap(), 7);
        assert_eq!(table.get::<String>("termination").unwrap(), "exited");
        let event_data: mlua::Table = table.get("event_data").unwrap();
        assert_eq!(
            event_data.get::<String>("event").unwrap(),
            "background_process_completed"
        );
    }
    #[test]
    fn transcript_payload_metadata_is_opaque_and_strictly_bounded() {
        let lua = Lua::new();
        let payload = "αβγ\n".repeat(2_000);

        let assistant_content: crate::transcript_content::TranscriptContent =
            payload.clone().into();
        let assistant_id = assistant_content.id().get();
        let assistant_node = transcript_block_render_node(
            BlockId::new(1),
            0,
            &Block::Text {
                content: assistant_content,
            },
            None,
        );
        let assistant = transcript_render_node_to_lua_table(&lua, &assistant_node).unwrap();
        assert_eq!(assistant.get::<u64>("content_id").unwrap(), assistant_id);
        assert_eq!(
            assistant.get::<usize>("content_bytes").unwrap(),
            payload.len()
        );
        assert_eq!(assistant.get::<usize>("content_lines").unwrap(), 2_000);
        assert!(!assistant.contains_key("content").unwrap());
        assert!(
            assistant.get::<String>("content_preview").unwrap().len() <= LUA_CONTENT_PREVIEW_BYTES
        );

        let thinking_content: crate::transcript_content::TranscriptContent = payload.clone().into();
        let thinking_node = transcript_block_render_node(
            BlockId::new(2),
            1,
            &Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                content: thinking_content,
                kind: protocol::ReasoningKind::Raw,
            },
            None,
        );
        let thinking = transcript_render_node_to_lua_table(&lua, &thinking_node).unwrap();
        assert!(!thinking.contains_key("content").unwrap());
        assert!(
            thinking.get::<String>("content_preview").unwrap().len() <= LUA_CONTENT_PREVIEW_BYTES
        );

        let draft_arguments: crate::transcript_content::TranscriptContent = payload.clone().into();
        let draft_id = draft_arguments.id().get();
        let mut draft_state =
            crate::content::tool_draft::ToolDraft::new("draft-1".into(), None, "write_file".into());
        draft_state.summary = protocol::StyledLines::empty();
        draft_state.raw_arguments = draft_arguments;
        let draft_node =
            transcript_block_render_node(BlockId::new(3), 2, &Block::ToolDraft(draft_state), None);
        let draft = transcript_render_node_to_lua_table(&lua, &draft_node).unwrap();
        assert_eq!(draft.get::<u64>("raw_arguments_id").unwrap(), draft_id);
        assert_eq!(
            draft.get::<usize>("raw_arguments_bytes").unwrap(),
            payload.len()
        );
        assert_eq!(draft.get::<usize>("raw_arguments_lines").unwrap(), 2_000);
        assert!(!draft.contains_key("raw_arguments").unwrap());
        assert!(
            draft.get::<String>("raw_arguments_preview").unwrap().len()
                <= LUA_CONTENT_PREVIEW_BYTES
        );

        let tool_output: crate::transcript_content::TranscriptContent = payload.clone().into();
        let tool_output_id = tool_output.id().get();
        let old_source_payload = "before source payload\n".repeat(100_000);
        let new_source_payload = "after source payload\n".repeat(100_000);
        let old_source: crate::transcript_content::TranscriptContent =
            old_source_payload.clone().into();
        let new_source: crate::transcript_content::TranscriptContent =
            new_source_payload.clone().into();
        let old_source_id = old_source.id().get();
        let new_source_id = new_source.id().get();
        let tool_args: crate::content::tool_draft::ToolArguments =
            HashMap::from([("content".into(), serde_json::Value::String(payload.clone()))]).into();
        let argument_content_id = tool_args
            .string_field("content")
            .expect("content argument")
            .content
            .id()
            .get();
        let tool_block = Block::ToolCall {
            call_id: "call-1".into(),
            name: "bash".into(),
            summary: protocol::StyledLines::from_plain("run"),
            args: tool_args,
        };
        let tool_state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            called_at_ms: None,
            elapsed_active: false,
            output: Some(Box::new(ToolOutput {
                content: tool_output,
                is_error: false,
                metadata: Some(serde_json::json!({ "path": "src/lib.rs" })),
                content_fields: vec![
                    crate::transcript_model::ToolOutputContentField {
                        name: "old_source".into(),
                        content: old_source,
                    },
                    crate::transcript_model::ToolOutputContentField {
                        name: "new_source".into(),
                        content: new_source,
                    },
                ],
            })),
            user_message: None,
            preview_output: None,
        };
        let tool_node =
            transcript_block_render_node(BlockId::new(4), 3, &tool_block, Some(&tool_state));
        let render_metadata_bytes = format!("{tool_node:?}").len();
        assert!(
            render_metadata_bytes < 8 * LUA_CONTENT_PREVIEW_BYTES,
            "typed render metadata grew with multi-megabyte retained source fields: {render_metadata_bytes} bytes"
        );
        let tool = transcript_render_node_to_lua_table(&lua, &tool_node).unwrap();
        let argument_fields: mlua::Table = tool.get("argument_fields").unwrap();
        let argument_field: mlua::Table = argument_fields.get(1).unwrap();
        assert_eq!(argument_field.get::<String>("name").unwrap(), "content");
        assert_eq!(
            argument_field.get::<u64>("content_id").unwrap(),
            argument_content_id
        );
        assert_eq!(
            argument_field.get::<usize>("content_bytes").unwrap(),
            payload.len()
        );
        assert!(!argument_field.contains_key("content").unwrap());
        let args: mlua::Table = tool.get("args").unwrap();
        assert!(args.get::<String>("content").unwrap().len() <= 4 * 1024 + 4);
        let output: mlua::Table = tool.get("output").unwrap();
        assert_eq!(output.get::<u64>("content_id").unwrap(), tool_output_id);
        assert_eq!(output.get::<usize>("content_bytes").unwrap(), payload.len());
        assert!(!output.contains_key("content").unwrap());
        assert!(
            output.get::<String>("content_preview").unwrap().len() <= LUA_CONTENT_PREVIEW_BYTES
        );
        let content_fields: mlua::Table = output.get("content_fields").unwrap();
        let old_source: mlua::Table = content_fields.get("old_source").unwrap();
        assert_eq!(old_source.get::<u64>("content_id").unwrap(), old_source_id);
        assert_eq!(
            old_source.get::<usize>("content_bytes").unwrap(),
            old_source_payload.len()
        );
        assert!(!old_source.contains_key("content").unwrap());
        assert!(
            old_source.get::<String>("content_preview").unwrap().len() <= LUA_CONTENT_PREVIEW_BYTES
        );
        let new_source: mlua::Table = content_fields.get("new_source").unwrap();
        assert_eq!(new_source.get::<u64>("content_id").unwrap(), new_source_id);
        assert_eq!(
            new_source.get::<usize>("content_bytes").unwrap(),
            new_source_payload.len()
        );
        assert!(!new_source.contains_key("content").unwrap());
        assert!(
            new_source.get::<String>("content_preview").unwrap().len() <= LUA_CONTENT_PREVIEW_BYTES
        );

        let exec_output: crate::transcript_content::TranscriptContent = payload.clone().into();
        let exec_id = exec_output.id().get();
        let exec_node = transcript_block_render_node(
            BlockId::new(5),
            4,
            &Block::Exec {
                command: "run".into(),
                output: exec_output,
            },
            None,
        );
        let exec = transcript_render_node_to_lua_table(&lua, &exec_node).unwrap();
        assert_eq!(exec.get::<u64>("output_id").unwrap(), exec_id);
        assert_eq!(exec.get::<usize>("output_bytes").unwrap(), payload.len());
        assert_eq!(exec.get::<usize>("output_lines").unwrap(), 2_000);
        assert!(!exec.contains_key("output").unwrap());
        assert!(exec.get::<String>("output_preview").unwrap().len() <= LUA_CONTENT_PREVIEW_BYTES);
    }

    #[test]
    fn tool_metadata_exposes_raw_active_and_terminal_timing_facts() {
        let lua = Lua::new();
        let block = Block::ToolCall {
            call_id: "call-1".into(),
            name: "bash".into(),
            summary: protocol::StyledLines::from_plain("echo hi"),
            args: HashMap::new().into(),
        };
        let mut state = ToolState {
            status: ToolStatus::Pending,
            elapsed: Some(std::time::Duration::from_millis(1_250)),
            called_at_ms: Some(1_700_000_000_123),
            elapsed_active: true,
            output: None,
            user_message: None,
            preview_output: None,
        };

        let active_node = transcript_block_render_node(BlockId::new(1), 0, &block, Some(&state));
        let active = transcript_render_node_to_lua_table(&lua, &active_node).unwrap();
        assert_eq!(
            active.get::<u64>("called_at_ms").unwrap(),
            1_700_000_000_123
        );
        assert_eq!(active.get::<u64>("elapsed_ms").unwrap(), 1_250);
        assert!(active.get::<bool>("elapsed_active").unwrap());
        assert_eq!(active.get::<String>("status").unwrap(), "pending");
        assert!(!active.contains_key("elapsed_text").unwrap());
        assert!(!active.contains_key("elapsed_secs").unwrap());

        state.status = ToolStatus::Ok;
        state.elapsed = Some(std::time::Duration::from_millis(2_500));
        state.elapsed_active = false;
        let terminal_node = transcript_block_render_node(BlockId::new(1), 0, &block, Some(&state));
        let terminal = transcript_render_node_to_lua_table(&lua, &terminal_node).unwrap();
        assert_eq!(
            terminal.get::<u64>("called_at_ms").unwrap(),
            1_700_000_000_123
        );
        assert_eq!(terminal.get::<u64>("elapsed_ms").unwrap(), 2_500);
        assert!(!terminal.get::<bool>("elapsed_active").unwrap());
        assert_eq!(terminal.get::<String>("status").unwrap(), "ok");
    }

    #[test]
    fn host_runtime_installs_transcript_renderer_api() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load error: {:?}", rt.load_error);
        let has_api: bool = rt
            .lua
            .load(
                r#"
                local default_count = 0
                for _ in pairs(smelt.transcript.defaults) do
                  default_count = default_count + 1
                end
                return type(smelt.transcript.set_renderer) == "function"
                  and type(smelt.transcript.extend_renderer) == "function"
                  and type(smelt.transcript.groups.register) == "function"
                  and type(smelt.transcript.defaults.render) == "function"
                  and type(smelt.transcript.defaults.render_tool_output) == "function"
                  and default_count == 2
                  and smelt.transcript.get_renderer() ~= nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(has_api);
    }

    #[test]
    fn transcript_settings_cache_key_tracks_transcript_table() {
        let rt = LuaRuntime::new();
        assert_eq!(rt.transcript_settings_cache_key(), None);

        rt.lua
            .load(
                r#"
                smelt.settings = {
                  transcript = {
                    view = { tools = { bash = "collapsed" } },
                    limits = { collapsed_error_rows = 2 },
                  },
                }
                "#,
            )
            .exec()
            .unwrap();
        let before = rt.transcript_settings_cache_key().unwrap();

        rt.lua
            .load("smelt.settings.transcript.limits.collapsed_error_rows = 3")
            .exec()
            .unwrap();
        let after = rt.transcript_settings_cache_key().unwrap();

        assert_ne!(before, after);
    }

    #[test]
    fn transcript_limits_accept_numbers_and_reject_invalid_values() {
        let lua = Lua::new();
        let limits = lua.create_table().unwrap();
        limits.set("integer_rows", 2).unwrap();
        limits.set("float_rows", 2.9).unwrap();
        limits.set("zero_rows", 0).unwrap();
        limits.set("string_rows", "2").unwrap();

        assert_eq!(
            transcript_limit(Some(&limits), "missing_rows").unwrap(),
            None
        );
        assert_eq!(
            transcript_limit(Some(&limits), "integer_rows").unwrap(),
            Some(2)
        );
        assert_eq!(
            transcript_limit(Some(&limits), "float_rows").unwrap(),
            Some(2)
        );

        let zero_err = transcript_limit(Some(&limits), "zero_rows").unwrap_err();
        assert!(zero_err
            .to_string()
            .contains("smelt.settings.transcript.limits.zero_rows"));
        let string_err = transcript_limit(Some(&limits), "string_rows").unwrap_err();
        assert!(string_err
            .to_string()
            .contains("smelt.settings.transcript.limits.string_rows"));
    }

    #[test]
    fn transcript_group_registry_orders_and_replaces_specs() {
        let rt = runtime_without_builtin_groups();
        rt.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "tool_batch",
                  cache_key = "v1",
                  priority = 5,
                  min = 3,
                  default_view = "collapsed",
                  selector = { kind = "tool", name = "read_file", terminal = true },
                  bucket = { "name" },
                })
                smelt.transcript.groups.register({
                  name = "low",
                  cache_key = "v1",
                  selector = { kind = "tool" },
                })
                smelt.transcript.groups.register({
                  name = "tool_batch",
                  cache_key = "v2",
                  priority = 10,
                  selector = { kind = "tool", names = { "read_file", "grep", "glob" }, terminal = true },
                })
                "#,
            )
            .exec()
            .unwrap();

        let specs = rt.transcript_group_specs();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "tool_batch");
        assert_eq!(specs[0].cache_key.as_deref(), Some("v2"));
        assert_eq!(specs[0].priority, 10);
        assert_eq!(specs[0].min, 2);
        assert_eq!(specs[0].selector.kind.as_deref(), Some("tool"));
        assert_eq!(
            specs[0]
                .selector
                .names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["read_file", "grep", "glob"]
        );
        assert_eq!(specs[0].selector.terminal, Some(true));
        assert_eq!(specs[1].name, "low");
        assert!(rt.transcript_group_generation() >= 3);
        assert!(rt.transcript_group_cache_key().is_some());
    }

    #[test]
    fn transcript_group_selector_rejects_conflicting_field_aliases() {
        let rt = LuaRuntime::new();
        let err = rt
            .lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "processes",
                  selector = {
                    kind = "process_status",
                    event = "background_process_completed",
                    fields = { event_type = "other_event" },
                  },
                })
                "#,
            )
            .exec()
            .expect_err("conflicting selector fields should fail");

        assert!(err.to_string().contains("conflicts"));
    }

    #[test]
    fn transcript_group_registration_remove_is_token_scoped() {
        let rt = runtime_without_builtin_groups();
        rt.lua
            .load(
                r#"
                local first = smelt.transcript.groups.register({
                  name = "batch",
                  cache_key = "v1",
                  selector = { kind = "tool" },
                })
                smelt.transcript.groups.register({
                  name = "batch",
                  cache_key = "v2",
                  selector = { kind = "tool" },
                })
                first:remove()
                "#,
            )
            .exec()
            .unwrap();
        let specs = rt.transcript_group_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].cache_key.as_deref(), Some("v2"));
    }

    #[test]
    fn transcript_group_without_cache_key_opts_out_of_cache() {
        let rt = runtime_without_builtin_groups();
        rt.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "uncached",
                  selector = { kind = "tool" },
                })
                "#,
            )
            .exec()
            .unwrap();
        assert_eq!(rt.transcript_group_cache_key(), None);
    }

    #[test]
    fn semantic_group_uses_root_transcript_renderer() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.transcript.set_renderer(function(node, ctx)
                  return smelt.layout.text(node.name .. ":" .. tostring(node.child_count) .. ":" .. tostring(ctx.view_state))
                end, { cache_key = "group-root:v1" })
                "#,
            )
            .exec()
            .unwrap();

        let child_ids = vec![BlockId::new(1), BlockId::new(2), BlockId::new(3)];
        let children = child_ids
            .iter()
            .copied()
            .map(|id| crate::transcript_model::TranscriptGroupChildMetadata {
                id,
                kind: "tool",
                name: Some("bash".into()),
                status: Some("ok"),
                summary_text: None,
                called_at_ms: None,
                args: None,
                output: Default::default(),
                event: None,
                process_id: None,
                exit_code: None,
                termination: None,
                event_data: Default::default(),
            })
            .collect();
        let node = TranscriptRenderNode::group(
            7,
            0,
            "batch".into(),
            "default".into(),
            "expanded",
            children,
            child_ids,
        );
        let layout = rt.render_transcript_layout(
            &node,
            crate::transcript_model::ViewState::Expanded,
            1_742_573_823_000,
        );
        match layout {
            BlockLayout::Leaf(LuaLeaf::Text(spec)) => assert_eq!(spec.content, "batch:3:expanded"),
            other => panic!("unexpected layout: {other:?}"),
        }
    }

    #[test]
    fn project_config_skipped_when_untrusted() {
        let tmp = tempfile::tempdir().unwrap();
        let smelt_dir = tmp.path().join(".smelt");
        std::fs::create_dir_all(&smelt_dir).unwrap();
        std::fs::write(smelt_dir.join("init.lua"), "PROJECT_LOADED = true\n").unwrap();

        let state = tempfile::tempdir().unwrap();
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let mut rt = LuaRuntime::new();
        let trust = rt.load_project_config(tmp.path());
        assert!(matches!(trust, crate::trust::TrustState::Untrusted { .. }));
        let loaded: bool = rt.lua.load("return PROJECT_LOADED == true").eval().unwrap();
        assert!(!loaded, "project init.lua must not run when untrusted");
    }

    #[test]
    fn project_config_runs_after_mark_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let smelt_dir = tmp.path().join(".smelt");
        std::fs::create_dir_all(smelt_dir.join("plugins")).unwrap();
        std::fs::write(smelt_dir.join("init.lua"), "PROJECT_INIT = true\n").unwrap();
        std::fs::write(
            smelt_dir.join("plugins").join("a.lua"),
            "PROJECT_PLUGIN = true\n",
        )
        .unwrap();

        let state = tempfile::tempdir().unwrap();
        let _g = crate::test_util::isolate_xdg_state(state.path());
        crate::trust::mark_trusted(tmp.path()).unwrap();

        let mut rt = LuaRuntime::new();
        let trust = rt.load_project_config(tmp.path());
        assert!(matches!(trust, crate::trust::TrustState::Trusted { .. }));
        let init_ran: bool = rt.lua.load("return PROJECT_INIT == true").eval().unwrap();
        let plugin_ran: bool = rt.lua.load("return PROJECT_PLUGIN == true").eval().unwrap();
        assert!(init_ran, "project init.lua must run after trust");
        assert!(plugin_ran, "project plugins/*.lua must run after trust");
    }

    #[test]
    fn clear_lua_handles_drops_every_registry() {
        let rt = LuaRuntime::new();
        let baseline_groups = rt.transcript_group_specs().len();
        rt.lua
            .load(
                r#"
                smelt.cmd.register("plug_cmd", function() end)
                smelt.lifecycle.on_ready(function() end)
                smelt.transcript.groups.register({
                  name = "batch",
                  selector = { kind = "tool" },
                })
                "#,
            )
            .exec()
            .expect("register");
        assert!(rt.shared.commands.lock().unwrap().contains_key("plug_cmd"));
        assert!(!rt.shared.hooks.lifecycle.is_empty());
        let specs = rt.transcript_group_specs();
        assert_eq!(specs.len(), baseline_groups + 1);
        assert!(specs.iter().any(|spec| spec.name == "batch"));

        rt.shared.clear_lua_handles();
        assert!(rt.shared.commands.lock().unwrap().is_empty());
        assert!(rt.shared.hooks.lifecycle.is_empty());
        assert!(rt.transcript_group_specs().is_empty());
    }

    #[test]
    fn clear_reload_scoped_config_drops_config_registries() {
        let rt = LuaRuntime::new();
        rt.shared.providers.lock().unwrap().push(Default::default());
        rt.shared.mcp_configs.lock().unwrap().insert(
            "srv".into(),
            crate::mcp::McpServerConfig {
                description: String::new(),
                enabled: true,
                transport: crate::mcp::McpTransportConfig::Local {
                    command: vec!["echo".into()],
                    env: Default::default(),
                    timeout: 1,
                },
            },
        );
        *rt.shared.permission_rules.lock().unwrap() =
            Some(crate::permissions::rules::RawPerms::default());
        rt.shared
            .settings_overrides
            .lock()
            .unwrap()
            .insert("vim".into(), crate::config::SettingValue::Bool(true));
        rt.shared.defaults.lock().unwrap().model = Some("model-a".into());
        rt.shared.remember.lock().unwrap().model = false;
        rt.shared.tool_defaults.lock().unwrap().tool_effects.insert(
            "bash".into(),
            crate::permissions::rules::ToolEffectKind::Process,
        );
        *rt.shared.default_shell.lock().unwrap() = Some(crate::lua::DefaultShell {
            program: "zsh".into(),
            args: vec!["-fc".into()],
        });

        rt.shared.clear_reload_scoped_config();

        assert!(rt.shared.providers.lock().unwrap().is_empty());
        assert!(rt.shared.mcp_configs.lock().unwrap().is_empty());
        assert!(rt.shared.permission_rules.lock().unwrap().is_none());
        assert!(rt.shared.settings_overrides.lock().unwrap().is_empty());
        assert!(rt.shared.defaults.lock().unwrap().model.is_none());
        assert!(rt.shared.remember.lock().unwrap().model);
        assert!(rt
            .shared
            .tool_defaults
            .lock()
            .unwrap()
            .tool_effects
            .is_empty());
        assert!(rt.shared.default_shell.lock().unwrap().is_none());
    }

    #[test]
    fn dropping_runtime_releases_lua_reg_cycles() {
        let rt = LuaRuntime::new();
        rt.lua
            .load("LEAK_REG = smelt.reg.new(function() end)")
            .exec()
            .expect("create registration");
        let lua = rt.lua.weak();

        drop(rt);

        assert!(
            lua.try_upgrade().is_none(),
            "LuaReg retained a strong reference to its own Lua state"
        );
    }

    #[test]
    fn lifecycle_on_ready_fires_in_registration_order_and_drains() {
        let mut rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                LIFECYCLE_LOG = {}
                smelt.lifecycle.on_ready(function() table.insert(LIFECYCLE_LOG, "a") end)
                smelt.lifecycle.on("ready", function() table.insert(LIFECYCLE_LOG, "b") end)
                "#,
            )
            .exec()
            .expect("register hooks");

        let errs = rt.drain_lifecycle_hooks("ready", |_| Ok(mlua::Value::Nil));
        assert!(errs.is_empty(), "no per-hook errors expected, got {errs:?}");
        let log: Vec<String> = rt
            .lua
            .load("return LIFECYCLE_LOG")
            .eval()
            .expect("read log");
        assert_eq!(log, vec!["a".to_string(), "b".to_string()]);

        // Second drain returns nothing - hooks are one-shot.
        let again = rt.drain_lifecycle_hooks("ready", |_| Ok(mlua::Value::Nil));
        assert!(again.is_empty());
        assert!(rt.shared.hooks.lifecycle.is_empty());
    }

    #[test]
    fn lifecycle_unknown_event_drains_to_empty() {
        let mut rt = LuaRuntime::new();
        let errs = rt.drain_lifecycle_hooks("never_emitted", |_| Ok(mlua::Value::Nil));
        assert!(errs.is_empty());
    }

    #[test]
    fn lifecycle_passes_ctx_table_to_hook() {
        let mut rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                LIFECYCLE_CTX = nil
                smelt.lifecycle.on("shutdown", function(ctx) LIFECYCLE_CTX = ctx end)
                "#,
            )
            .exec()
            .expect("register");

        let errs = rt.drain_lifecycle_hooks("shutdown", |lua| {
            let t = lua.create_table()?;
            t.set("session_id", "sess-42")?;
            t.set("has_messages", true)?;
            Ok(mlua::Value::Table(t))
        });
        assert!(errs.is_empty(), "no errors expected, got {errs:?}");
        let id: String = rt
            .lua
            .load("return LIFECYCLE_CTX.session_id")
            .eval()
            .expect("ctx.session_id readable");
        let has: bool = rt
            .lua
            .load("return LIFECYCLE_CTX.has_messages")
            .eval()
            .expect("ctx.has_messages readable");
        assert_eq!(id, "sess-42");
        assert!(has);
    }

    #[test]
    fn lifecycle_hook_error_isolates_per_callback() {
        let mut rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                LIFECYCLE_RAN = 0
                smelt.lifecycle.on_ready(function() error("boom") end)
                smelt.lifecycle.on_ready(function() LIFECYCLE_RAN = LIFECYCLE_RAN + 1 end)
                "#,
            )
            .exec()
            .expect("register");

        let errs = rt.drain_lifecycle_hooks("ready", |_| Ok(mlua::Value::Nil));
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        assert!(
            errs[0].contains("boom"),
            "error message preserved: {}",
            errs[0]
        );
        let ran: i64 = rt.lua.load("return LIFECYCLE_RAN").eval().unwrap();
        assert_eq!(ran, 1, "second hook still ran after first one errored");
    }

    #[test]
    fn bundled_early_modules_run_in_lex_order_with_restricted_smelt() {
        let modules = early_modules();
        assert!(
            modules.contains(&"smelt.early.resume".to_string()),
            "bundled early should include smelt.early.resume, got {modules:?}"
        );
        let mut sorted = modules.clone();
        sorted.sort();
        assert_eq!(modules, sorted, "early modules must be lex-sorted");
    }

    #[test]
    fn wipe_loaded_modules_keeps_native_stdlib() {
        let rt = LuaRuntime::new();
        rt.lua
            .load("package.loaded['user_mod'] = { v = 1 }")
            .exec()
            .unwrap();
        let stdlib_before: bool = rt
            .lua
            .load("return package.loaded['string'] ~= nil")
            .eval()
            .unwrap();
        assert!(stdlib_before, "stdlib must be in package.loaded");

        rt.wipe_loaded_modules();
        let stdlib_after: bool = rt
            .lua
            .load("return package.loaded['string'] ~= nil")
            .eval()
            .unwrap();
        assert!(stdlib_after, "stdlib must survive wipe");
        let user_after: bool = rt
            .lua
            .load("return package.loaded['user_mod'] ~= nil")
            .eval()
            .unwrap();
        assert!(!user_after, "user module must be wiped");
    }

    // End-to-end `reload()` lives in `tui::lua::tests::reload_clears_tui_surfaces`
    // - the bundled autoload modules need the TUI Lua API to run.

    #[test]
    fn config_lua_directory_is_a_module_root() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config").join("smelt");
        let lua_dir = config_dir.join("lua");
        std::fs::create_dir_all(&lua_dir).unwrap();
        std::fs::write(
            lua_dir.join("example_plugin.lua"),
            "return { tag = 'user-module' }\n",
        )
        .unwrap();

        let paths = LuaLoadPaths {
            home: tmp.path().join("home"),
            config_dir,
            state_root: tmp.path().join("state"),
            cache_root: tmp.path().join("cache"),
            runtime_override: None,
            development_runtime: None,
            project_cwd: None,
            data_runtime: tmp.path().join("data-runtime"),
        };
        let lua = Lua::new();
        register_module_searcher_with_roots(&lua, paths.module_overlay_roots(), None, None)
            .unwrap();

        let module: mlua::Table = lua.load("return require('example_plugin')").eval().unwrap();
        assert_eq!(module.get::<String>("tag").unwrap(), "user-module");
    }

    #[test]
    fn overlay_file_overrides_embedded_module() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("smelt").join("dialogs");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("confirm.lua"), "return { tag = 'overlay' }\n").unwrap();

        let lua = Lua::new();
        let roots = vec![tmp.path().to_path_buf()];
        register_module_searcher_with_roots(&lua, roots, None, None).unwrap();

        let v: mlua::Table = lua
            .load("return require('smelt.dialogs.confirm')")
            .eval()
            .unwrap();
        let tag: String = v.get("tag").unwrap();
        assert_eq!(tag, "overlay");
    }
}
