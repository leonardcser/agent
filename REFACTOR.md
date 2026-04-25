# Window Unification Refactor

Goal: every "viewport into a buffer" surface uses the **same** `ui::Window`
primitive — transcript, prompt, dialog content panels. One mouse handler, one
selection style, one vim-mode source. No `BufferPane`, no parallel
implementations.

Maps to the architecture doc's promise:
> The transcript window and the prompt window are the same kind of thing —
> they only differ in their buffer's `modifiable` flag.

## User complaints this resolves

1. Diff/preview buffers in dialogs lack double/triple-click word/line select.
2. Selection bg color differs from transcript (`DarkGrey` vs `theme::selection_bg()`).
3. Selection behavior differs (no anchored word/line drag, no edge autoscroll).
4. Status bar shows "Insert mode" when a dialog is focused (leaks prompt mode).
5. Clicking outside the diff buffer doesn't blur its cursor.

## Step list

Each step is one atomic commit. Tree green at every commit.
Run `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.

### ✅ Step 0 — Cherry-pick orthogonal cleanup [commit `82eb4d7`]

`Notification` toast drag-select-and-yank + `selection` field in
`NotificationStyle` + drop dead notification click-to-dismiss path in
`App::handle_mouse`. Independent of the unification.

### ✅ Step 1 — `Window::handle_mouse` + interactive dialog buffer panels [commit `a685eaf`]

- Add `Window::handle_mouse(event, ctx) -> MouseAction` covering click,
  double/triple click word/line yank, drag-extend (anchored to word/line when
  set on Down), mouse-up yank.
- Add `drag_anchor_word` / `drag_anchor_line` state to `Window`.
- `Ui::set_selection_bg(Color)` + `Ui::selection_style() -> Style`.
- `PanelSpec::interactive_content(buf, height)` builder. Lua:
  `{ kind = "content", buf = …, interactive = true }`.
- `Dialog::handle_mouse` routes Down/Drag/Up for interactive Buffer panels
  through their internal `Window::handle_mouse`. `Dialog::cursor` exposes
  the focused panel's Window cursor. Selection overlay painted each frame
  using `theme::selection_bg()` propagated via `DialogConfig::selection_style`.
- Click cadence (1/2/3-click within 400 ms on the same cell, same panel)
  tracked on `Dialog::last_click`. Cross-panel click resets.

Folded in from the stashed work (independent of BufferPane experiment):
- Empty buffer (1 line, empty) → `line_count = 0` so `collapse_when_empty`
  actually hides it.
- Hidden panels suppress separator chrome.
- Regression test `focused_dialog_esc_invokes_dismiss_callback` for the
  dispatch chain.

Resolves complaints 1, 2, 3, 5 for any panel that opts into
`interactive = true`. Built-in confirm preview hasn't been opted in yet
(that's Step 5).

### ✅ Step 2 — Status bar reads the focused buffer Window's vim mode

Fixes complaint 4 (status bar leaks prompt's "Insert" while a dialog is
focused).

Generalize `App::current_vim_mode_label`: drop the `AppFocus::Content/Prompt`
branch, instead ask `Ui` for the focused buffer-bearing Window and read
its mode.

- New `Ui::focused_buffer_window(&self) -> Option<&Window>` that walks the
  compositor's focus chain into the topmost focused dialog, asks the dialog
  for its focused panel's Window (if a buffer panel) and returns it. Falls
  back to the prompt or transcript Window for non-dialog focus.
- When the focused panel is a widget (`OptionList`, `TextInput`) or a
  non-interactive chrome buffer panel: returns `None`, status bar shows no
  mode (matches nvim).

### ✅ Step 3 — Switch transcript path to `Window::handle_mouse`

Pure code-quality: delete the duplicated cursor/selection/drag logic from
`App::handle_mouse`.

Transcript has projection-specific behavior (`full_transcript_display_text`,
`snap_col_to_selectable` for hidden thinking blocks, `copy_display_range` for
display→raw mapping). Keep those in `App` as adapters that build the
`MouseCtx` and translate the returned `Yank(text)`. The cursor / selection /
anchored-drag mechanics move to `Window::handle_mouse` (already there).

Net deletion: ~300 lines from `app/mouse.rs` (`extend_word_anchored_drag`,
`extend_line_anchored_drag`, `select_and_copy_word_in_content`,
`select_and_copy_line_in_content`, `position_content_cursor_from_hit`).

Drag-edge autoscroll stays App-side (frame-tick-driven).

### ✅ Step 4 — Drop per-widget selection styles (4b only)

Prompt-side mouse unification (4a) was deferred: `Window::handle_mouse`
assumes `rows.join("\n") == edit_buf.buf` (transcript model), but the
prompt's source buffer doesn't match the wrapped display rows. Adding a
row-space ↔ source-space translation layer would be net-zero on code
size with new bug surface, so the prompt keeps its existing
`position_prompt_cursor_from_click`.

Done (4b):
- `TextInput::selection_style` field + `with_selection_style` builder
  dropped; reads `ctx.selection_style` at draw time.
- `NotificationStyle::selection` field dropped; reads `ctx.selection_style`.
- `Compositor` carries `selection_style: Style` populated via
  `Ui::set_selection_bg`, propagated into every `DrawContext` it builds.
- `DrawContext` gains `selection_style: Style` (with `#[derive(Default)]`
  so test sites can `..Default::default()`).

One source of truth: `theme::selection_bg()` flows into `Ui::set_selection_bg`,
which seeds both the dialog config slot (used by buffer panel overlays)
and the compositor slot (used by every widget via `DrawContext`).

### ⬜ Step 5 — Confirm dialog cleanup (the big one)

Separate architectural concern from Window unification. Drops 1000+ lines
across `crates/tui/src/app/dialogs/confirm.rs`,
`crates/tui/src/lua/confirm_ops.rs`, and confirm-specific OptionList flags.

#### 5a — Renderer primitives

Promote confirm-private renderers to general-purpose `smelt.*` modules
that any plugin can use:

```lua
smelt.diff.render_to_buf(buf, { old=, new=, path= })       -- print_inline_diff
smelt.syntax.render_to_buf(buf, { content=, path= })       -- print_syntax_file
smelt.bash.highlight_into(buf, line, col_start, col_end)   -- BashHighlighter
smelt.notebook.render_to_buf(buf, args)                    -- notebook preview
```

Underlying Rust functions stay (security-relevant rendering belongs in
core); confirm-specific wrappers around them go.

#### 5b — Confirm request as data + label policy in Lua

Replace `_build_title_buf`/`_build_summary_buf`/`_build_preview_buf`/
`_option_labels`/`_back_tab` opaque-handle wrappers with a single
`smelt.confirm.requests[handle]` table exposing the request fields
(`tool_name`, `desc`, `summary`, `outside_dir`, `approval_patterns`,
`cwd_label`, `args`). Lua composes the buffers and labels using the
renderer primitives from 5a and standard `smelt.buf.*` helpers.

#### 5c — Panel handles, not index pokes

`smelt.ui.dialog._open(opts)` returns a handle whose `panels` field is a
name-or-index map of typed panel handles:

```lua
local d = smelt.ui.dialog.open({ panels = { options = {...}, reason = {...} } })
d.panels.options:selected_index()
d.panels.reason:text()
d:focus("reason")
```

Replaces every `_focus_panel`, `_options_set_editing`,
`_options_is_editing`, `_focused_panel` confirm_ops shim with panel
methods.

#### 5d — Drop one-off widget flags

- `OptionList::detail_input` → drop. Reason field becomes a regular
  `kind = "input"` panel below options with `hide_when_unfocused = true`,
  Tab focuses it. Same row-spacing as nvim.
- `OptionList::numbered` (`with_index_prefix`) → drop. Caller pre-formats
  labels (`"  1. yes"`) or supplies a prefix function.
- The `interactive` flag stays — it's the unification's primary handle.

#### 5e — Plug `interactive_content` into confirm.lua's preview panel

One-line change in `runtime/lua/smelt/confirm.lua`: the diff/preview panel
becomes `kind = "content", interactive = true`. User immediately gets
double/triple click + theme selection bg + drag-extend in tool approval
dialogs.

Resolution after 5e: every original user complaint addressed end-to-end.

### ⬜ Step 6 — nvim-style highlight registry, drop ad-hoc theme module

Replaces `crates/tui/src/theme.rs` (flat module of hardcoded color
constants/functions) and the one-off `Ui::selection_bg` slot with a
real **highlight group registry** modeled on nvim. Same indirection
nvim uses: code references *names* (`Visual`, `SmeltAgent`); users
override *names* via Lua; new plugins extend the registry without
touching core. After this, "the selection background" is just one
entry in a map — no longer special.

#### 6a — `ui::Theme` registry

New `crates/ui/src/theme.rs`:

```rust
pub struct Theme {
    groups: HashMap<String, Style>,
    links: HashMap<String, String>,
}

impl Theme {
    pub fn set(&mut self, name: &str, style: Style);
    pub fn link(&mut self, from: &str, to: &str);
    /// Chases links until a real entry is hit; falls back to
    /// `Style::default()` for unknown names. No panics on typos —
    /// nvim's policy too.
    pub fn get(&self, name: &str) -> Style;
}
```

Color parsing: `"#3a3a3a"` (hex), `"darkgrey"` (named, mapped through
`crossterm::style::Color`), `"196"` (palette index 0–255). Done at
`set` time so `get` returns a ready-to-paint `Style`.

#### 6b — Plumb `&Theme` through `DrawContext`

```rust
pub struct DrawContext<'a> {
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub focused: bool,
    pub theme: &'a Theme,
}
```

Cascades a lifetime through `Component::draw`/`prepare`. Mechanical;
touches every widget (`TextInput`, `Notification`, `OptionList`,
`Picker`, `Dialog`, `BufferList`, `BufferView`, `StatusBar`,
`Cmdline`, …) but only the signature, not the body.

`Compositor::render` borrows `&self.ui.theme()` (or holds an
`Arc<Theme>` clone refreshed when `Ui::theme_mut` is called) and
embeds the reference into both `DrawContext` builds.

Drops:
- `Ui::selection_bg` field, `set_selection_bg`, `selection_style`.
- `Compositor::selection_style` field, `set_selection_style`.
- `DrawContext::selection_style`.
- `DialogConfig::selection_style` (Dialog reads `theme.get("Visual")`
  during `sync_from_bufs_mut` — need to thread `&Theme` into that
  call too).

Everything that used to read `selection_style` now reads
`theme.get("Visual")`.

#### 6c — Migrate `crate::theme::*` call sites

Delete `crates/tui/src/theme.rs` as a module of constants. Replace
each call:

```rust
// before
crate::theme::selection_bg()
crate::theme::accent()
crate::theme::AGENT
crate::theme::PLAN
crate::theme::APPLY
crate::theme::YOLO
crate::theme::ERROR
crate::theme::muted()
crate::theme::slug_color()

// after
ctx.theme.get("Visual").bg.unwrap_or_default()
ctx.theme.get("SmeltAccent").fg.unwrap_or_default()
ctx.theme.get("SmeltAgent").fg.unwrap_or_default()
ctx.theme.get("SmeltModePlan").fg.unwrap_or_default()
ctx.theme.get("SmeltModeApply").fg.unwrap_or_default()
ctx.theme.get("SmeltModeYolo").fg.unwrap_or_default()
ctx.theme.get("ErrorMsg").fg.unwrap_or_default()
ctx.theme.get("Comment").fg.unwrap_or_default()  // or SmeltMuted
ctx.theme.get("SmeltSlug").bg.unwrap_or_default()
```

For host-side code that builds spans without a `DrawContext` (e.g.,
`App::refresh_status_bar`): take `&Theme` as a parameter or read
`self.ui.theme()` at frame start.

#### 6d — Default smelt theme

New `crates/tui/src/theme.rs` (much smaller) becomes a single
function:

```rust
pub fn default_smelt_theme() -> ui::Theme {
    let mut t = ui::Theme::new();
    // nvim-stock groups smelt uses
    t.set("Normal",     Style { fg: …, bg: … });
    t.set("Visual",     Style { bg: Some(Color::AnsiValue(237)), .. });
    t.set("Search",     …);
    t.set("Comment",    …);
    t.set("Statement",  …);
    t.set("Function",   …);
    t.set("Keyword",    …);
    t.set("LineNr",     …);
    t.set("CursorLine", …);
    t.set("StatusLine", …);
    t.set("Pmenu",      …);
    t.set("PmenuSel",   …);
    t.set("ErrorMsg",   …);
    t.set("WarningMsg", …);
    // smelt-specific groups
    t.set("SmeltSlug",         …);
    t.set("SmeltAccent",       …);
    t.set("SmeltMuted",        …);
    t.set("SmeltAgent",        …);
    t.set("SmeltModePlan",     …);
    t.set("SmeltModeApply",    …);
    t.set("SmeltModeYolo",     …);
    t.set("SmeltModeNormal",   …);
    t.set("SmeltScrollbar",    …);
    t.set("SmeltStatusBg",     …);
    // links — code refers to one name, theme can move them around
    t.link("SmeltMuted", "Comment");
    t
}
```

`App::new` calls `ui.theme_mut().extend(default_smelt_theme())`
once at startup.

#### 6e — Lua bindings: `smelt.theme.*`

```lua
smelt.theme.set("Visual", { bg = "#3a3a3a", fg = "#eeeeee", bold = false })
smelt.theme.link("ErrorMsg", "SmeltAccent")
smelt.theme.get("Visual")  -- → { bg = "#3a3a3a", fg = "#eeeeee", … }
smelt.theme.colorscheme("retrobox")  -- runs runtime/lua/smelt/colorschemes/retrobox.lua
```

Implementation:
- `smelt.theme.set(name, style_table)` → parse colors (hex / name /
  palette index → `crossterm::style::Color`), call `Theme::set`.
- `smelt.theme.link(from, to)` → `Theme::link`.
- `smelt.theme.get(name)` → serialize `Style` back to a table.
- `smelt.theme.colorscheme(name)` → `dofile` resolution against a
  search path (built-in `runtime/lua/smelt/colorschemes/<name>.lua`
  + user's config dir).

A theme-mutation triggers `Compositor::force_redraw = true` so the
next frame repaints from scratch (no diff-based partial updates that
still show the old palette).

#### 6f — Starter colorschemes

Ship one or two `runtime/lua/smelt/colorschemes/*.lua` files (e.g.
`default.lua` mirroring `default_smelt_theme()`, plus one alternate
like `retrobox` or `tokyonight`). Each is a flat list of
`smelt.theme.set` calls — same shape as nvim colorschemes, instantly
familiar.

User config can `smelt.theme.colorscheme("custom")` and ship their
own file in `~/.config/smelt/colorschemes/custom.lua`.

### Net deletion target

After all steps:

- `app/dialogs/confirm.rs` — gone (≈235 lines)
- `app/dialogs/confirm_preview.rs` — kept as-is, but its renderer body
  exposed as `smelt.*` modules (no new wrappers).
- `lua/confirm_ops.rs` — shrinks 250 → ~40 lines (just request snapshot
  + resolve).
- `app/mouse.rs` — sheds ≈300 lines (transcript + prompt mouse handling
  moves to `Window::handle_mouse`).
- `OptionList` detail-field code — gone (≈350 lines if we count the
  stash's additions).
- `BufferPane` — already gone (stashed).
- `crates/tui/src/theme.rs` constants module — replaced by
  `default_smelt_theme()` builder (≈80 lines net negative).
- `Ui::selection_bg`, `Compositor::selection_style`,
  `DrawContext::selection_style`, `DialogConfig::selection_style` —
  all collapse into `theme.get("Visual")` (≈40 lines).

Estimated: ~1000 lines lighter, three "almost-Windows" merged into one
real Window, every buffer surface gets transcript-grade interaction.

## Process notes

- Each step's commit message answers WHY, not WHAT.
- Atomic refactors only — don't ship a step that leaves the tree
  half-migrated.
- Never use `--no-verify` or `--allow-dirty`. Investigate hook failures.
- Stash `stash@{0}` keeps the BufferPane experiment recoverable. Drop
  it once Step 5e ships and we're sure nothing in there is still needed.
