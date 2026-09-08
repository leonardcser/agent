-- `smelt.dialog`: root-docked modal interactions.
--
-- A dialog replaces the composer block at the bottom of the root layout, keeping
-- the transcript visible above it. For centered viewers and other floating
-- content, use `smelt.overlay` directly.
--
-- The dialog primitive does not know what's inside it. Consumers build their own
-- buffers and leaves (using the helpers below or `smelt.win.new` directly) and pass
-- the leaves in `opts.panels`. Dialog handles only:
--   1. Mounting the panel tree in the root composer with a top border.
--   2. Setting and containing focus.
--   3. Installing dialog-level keymaps across the modal scope.
--   4. Bridging Submit/Dismiss/Tick events to user callbacks.
--   5. Coroutine lifecycle: `open(opts)` blocks until `ctx.resolve(v)` is called.
--
-- Keymap scoping (important):
--   - `opts.keymaps` are DIALOG-WIDE - installed on the modal scope so they
--     fire regardless of which panel is focused.
--     Use these for shortcuts that should always work in the dialog (e.g.
--     Alt-W, Ctrl-D).
--   - Ctrl-O toggles the dialog between its context-preserving height and an
--     expanded review layout.
--   - To scope a key to a specific panel, install it directly on that leaf via
--     `leaf:key(key, fn)` after `new` returns. Example:
--     the confirm dialog binds `tab` only on the options leaf (jump into the
--     reason input) and `esc` only on the reason leaf (pop focus back to the
--     options leaf instead of dismissing the dialog).
--
-- Buffer helpers:
--   smelt.dialog.input(placeholder, opts)   -> leaf, buf, input  (line input; opts.wrap soft-wraps)
--   smelt.dialog.menu(items, opts)          -> leaf, ctrl (numbered selectable list)
--   smelt.dialog.list(buf, opts)            -> leaf       (existing buffer as a list)
--   smelt.dialog.markdown(text)             -> leaf, buf  (markdown-rendered content)
--   smelt.dialog.content(opts)              -> leaf, buf  (plain content; opts.text or opts.buf)
--   smelt.dialog.viewer(opts)               -> handle, buf, leaf  (read-only content dialog)
--
-- Dialog context (active dialog introspection):
--   smelt.dialog.current()                  -> ctx | nil  (resolve/close/panels/focused_leaf)
--
-- Callable from any handler running while the dialog is open - including
-- leaf-level `leaf:key(...)` callbacks, which normally don't see the
-- dialog's resolve handle. Nested dialogs stack; `current()` always
-- returns the topmost.

local M = {}

smelt.dialog = smelt.dialog or {}

local REGION = "dialog"

local function report_callback_error(event, err)
  local msg = tostring(err)
  smelt.log.error("dialog.callback_failed", { event = event, error = msg })
  smelt.notify.error("dialog " .. event .. ": " .. msg)
end

local function dialog_keymaps(opts)
  local keymaps = {}
  local has_q = false
  for _, km in ipairs(opts.keymaps or {}) do
    if type(km) == "table" then
      if km.key == "q" then has_q = true end
      keymaps[#keymaps + 1] = km
    end
  end
  if opts.close_with_q and not has_q then
    table.insert(keymaps, 1, { key = "q", on_press = function(ctx) ctx.close() end })
  end
  return keymaps
end

-- Stack of active dialog contexts. Pushed by `setup_lifecycle` at open
-- time, popped on resolve. `smelt.dialog.current()` returns the topmost
-- ctx so nested dialogs (e.g. confirm-on-top-of-picker) don't shadow each
-- other.
local dialog_stack = {}

--- Return the topmost active dialog ctx (the same shape passed to
--- `on_submit`/`keymap` handlers: `{ resolve, close, win, panels,
--- focused_leaf }`), or `nil` if no dialog is open. Use it inside
--- `leaf:key(...)` callbacks - those normally lack a path to the
--- dialog's resolve handle.
---@type fun(): table | nil
function smelt.dialog.current()
  return dialog_stack[#dialog_stack]
end

-- ── Buffer/leaf builders ──────────────────────────────────────────────
--
-- Most helpers add a one-cell gutter on the left and right so labels,
-- menus, and inputs never sit flush against the frame. Content leaves can
-- opt out for full-width previews, where padding would steal columns and
-- create horizontal overflow.
--
-- Scrollbars: buffer-viewer leaves (`markdown`, `content`) inherit the default
-- `scrollbar = true` from `smelt.win.new` so a thumb appears when content
-- overflows. Cursor-driven leaves (`input`, `options`, `list`) opt out - the
-- selection cursor and key nav already convey position.

local GUTTER = 1

--- One body panel inside a dialog. `leaf` is the win/leaf built by one of
--- the `smelt.dialog.*` helpers; `height` follows the same grammar as
--- `smelt.dialog.open` (integer cells, `"N%"`, `"fill"`, `"fit"`).
---@class smelt.dialog.Panel
---@field leaf smelt.win.Win A leaf returned by `smelt.dialog.input/options/list/markdown/content`.
---@field height? any Integer cells, `"N%"`, `"fill"`, or `"fit"`.

--- One dialog-level keymap entry. `on_press(ctx)` receives the dialog
--- context exposing `ctx.close()` and `ctx.resolve(value)` so the
--- handler can dismiss the dialog or resolve the blocking `open` call.
---@class smelt.dialog.Keymap
---@field key string Chord string (e.g. `"q"`, `"<Esc>"`, `"ctrl-j"`).
---@field hint? string Optional one-line hint surfaced in the dialog footer.
---@field on_press fun(ctx: any): any Handler invoked when the key fires.

--- Options accepted by `smelt.dialog.open` / `smelt.dialog.new`.
--- Dialogs fit their content by default while preserving transcript context.
--- Integer `height` values are body-relative and gain
--- the top chrome row automatically. Pick one of `height` or `max_height`;
--- setting both raises.
---@class smelt.dialog.Opts
---@field title? string Title rendered in the chrome row.
---@field panels? smelt.dialog.Panel[] Ordered list of body panels. Mutually exclusive with `layout`.
---@field layout? smelt.ui.layout Composable body layout, including nested resizable splits. Window leaves are discovered automatically. Mutually exclusive with `panels` and `bottom_panels`.
---@field bottom_panels? smelt.dialog.Panel[] Panels pinned to the bottom when the dialog has surplus height; extra height is placed between them and `panels`.
---@field bottom_gap? integer Minimum blank rows between `panels` and `bottom_panels` (default 0).
---@field focus? smelt.win.Win Leaf that should receive initial focus.
---@field height? any Fixed total body size: integer cells, `"N%"`, `"fill"`, or `"fit"`.
---@field max_height? any Maximum root-layout height while fitting content.
---@field min_height? any Minimum root-layout height while fitting content.
---@field blocks_agent? boolean Block the agent loop while the dialog is open. Defaults to `false`.
---@field border? table Top border style override; defaults to `{ top = "SmeltAccent" }`.
---@field resizable? boolean Set `false` to disable the default top-edge resize handle.
---@field keymaps? smelt.dialog.Keymap[] Dialog-level key bindings (merged with built-ins).
---@field close_with_q? boolean Bind `q` to close for read-only/list dialogs. Leave false for dialogs that accept text input.
---@field on_submit? fun(ctx: any): any Handler invoked on Enter. Without a handler, Enter leaves the dialog open.
---@field on_dismiss? fun(): nil Handler invoked when the dialog is dismissed.
---@field on_close? fun(ctx: any): nil Handler invoked once whenever the dialog resolves or closes.

--- Options accepted by `smelt.dialog.picker`. Layered on top of
--- `smelt.dialog.Opts`; only the picker-specific fields are listed.
---@class smelt.dialog.PickerOpts
---@field items? any[] | fun(): any[] Eager item table or a lazy producer; re-evaluated by `on_query`.
---@field render fun(item: any): table Per-item `{ text, marks }` table - see `smelt.list.new`.
---@field filter? fun(item: any): boolean Predicate applied during `set_filter` / `refresh`.
---@field placeholder? string Input placeholder; defaults to `""`.
---@field empty_text? string Shown in the list when nothing matches.
---@field on_open? fun(ctx: any): nil Fires once after the input/list have been built.
---@field on_query? fun(query: string, ctx: any): nil Fires on every keystroke; default re-applies `filter`.
---@field on_submit? fun(ctx: any): any Fires on Enter. `ctx.item` is the highlighted row; defaults to resolving with `ctx.item` when non-nil.
---@field on_dismiss? fun(): nil Fires when the dialog is dismissed.
---@field keymaps? smelt.dialog.Keymap[] Extra dialog-level keymaps merged on top of navigation bindings.
---@field title? string Forwarded to `smelt.dialog.open`.
---@field height? any Forwarded to `smelt.dialog.open`.
---@field max_height? any Forwarded to `smelt.dialog.open`.
---@field min_height? any Forwarded to `smelt.dialog.open`.
---@field blocks_agent? boolean Forwarded to `smelt.dialog.open`.

-- Build a line-input leaf with a fresh buffer. `placeholder` shows when the
-- buffer is empty. `opts.pad_left` / `opts.pad_right` override the dialog
-- gutter; `opts.wrap = true` lets long input soft-wrap across visual rows
-- while preserving single-line submit semantics. Returns `(leaf, buf, input)`
-- so callers can keep using the buffer directly or opt into the first-class
-- input handle.
---@type fun(placeholder: string?, opts: table?): smelt.win.Win, smelt.buf.Buf, smelt.input.Input
function smelt.dialog.input(placeholder, opts)
  opts = opts or {}
  local input = smelt.input.new({
    region = REGION,
    placeholder = placeholder or "",
    pad_left = opts.pad_left or GUTTER,
    pad_right = opts.pad_right or GUTTER,
    scrollbar = false,
    wrap = opts.wrap == true,
  })
  return input:win(), input:buf(), input
end

-- ── Menu primitive ─────────────────────────────────────────────────────
--
-- `smelt.dialog.menu(items, opts)` builds a selectable list leaf shaped
-- for a small, fixed set of choices: dim ` N. ` numbering, optional
-- description row per item (rendered dim under the label), digit-key
-- shortcuts (`1`..`9`), and a controller that talks in **1-based item
-- indices** so callers never have to compute a row stride.
--
-- Items may be strings (label-only) or `{ label, description?, key? }`
-- tables. If any item has a non-empty description every item includes
-- description rows beneath its label. Labels and descriptions wrap to the
-- menu width; navigation always rests on the first label row.
--
-- Shortcuts (`opts.shortcuts`, default `"submit"`):
--   * `"submit"` - pressing the item's digit moves the cursor to it AND
--                  fires the submit path (same as Enter on that row).
--   * `"select"` - digit moves the cursor; Enter still submits.
--   * `false`   - no digit handling; consumers can install their own.
--
-- Submit path: by default the menu resolves the active dialog with
-- `{ index = i, item = items[i] }`. Override via `opts.on_submit(ctx)`
-- - `ctx` exposes the standard dialog handles (`resolve`, `close`,
-- `win`, `panels`, `focused_leaf`) plus `ctx.index` (1-based) and
-- `ctx.item` for the selection - to map to a caller-specific payload
-- (e.g. confirm's `decisions[idx]`) or to defer (focus a sibling leaf
-- instead of resolving).
--
-- Returned `ctrl` exposes 1-based selection helpers:
--   ctrl:cursor()         -- currently selected enabled index (1-based)
--   ctrl:cursor(i)        -- set selection (1-based; clamped, skips disabled items)
--   ctrl:item()           -- currently selected item table
--   ctrl:items()          -- normalized item list
--   ctrl:set_items(items) -- replace items and keep selection on an enabled row
--   ctrl:size()           -- number of items
--   ctrl:submit()         -- trigger the submit path programmatically

local NS_MENU_NUM      = smelt.ns("smelt.dialog.menu.num")
local NS_MENU_SELECTED = smelt.ns("smelt.dialog.menu.selected")
local NS_MENU_DESC     = smelt.ns("smelt.dialog.menu.desc")
local NS_MENU_DISABLED = smelt.ns("smelt.dialog.menu.disabled")

local function wrap_prefixed_text(prefix, text, cont_prefix, width)
  return smelt.text.wrap_prefixed(tostring(text or ""), width or 0, {
    prefix      = prefix,
    cont_prefix = cont_prefix,
  })
end

-- Render `items` into `buf`, applying dim numbering and item metadata for
-- selected-row styling. `has_descriptions` reserves description rows. When
-- `width` is supplied, label and description rows are hard-wrapped into buffer
-- lines so fit-height dialogs grow instead of relying on horizontal panning.
local function render_menu(buf, items, has_descriptions, numbered, width)
  local rendered = {}
  local meta = {}
  local desc_indent = "    "

  for i, it in ipairs(items) do
    local prefix = numbered and string.format(" %d. ", i) or " "
    local cont_prefix = string.rep(" ", smelt.text.width(prefix))
    local label_lines = wrap_prefixed_text(prefix, it.label or "", cont_prefix, width)
    local label_row = #rendered + 1
    local label_spans = {}
    for n, line in ipairs(label_lines) do
      rendered[#rendered + 1] = line
      label_spans[#label_spans + 1] = {
        row     = #rendered,
        start   = n == 1 and #prefix or #cont_prefix,
        end_col = #line,
      }
    end
    local label_last_row = #rendered

    local desc_row, desc_end
    if has_descriptions then
      local desc = it.description or ""
      local desc_lines = wrap_prefixed_text(desc_indent, desc, desc_indent, width)
      desc_row = #rendered + 1
      for _, line in ipairs(desc_lines) do rendered[#rendered + 1] = line end
      desc_end = #(rendered[#rendered] or "")
    end

    meta[i] = {
      label_row      = label_row,
      label_last_row = label_last_row,
      last_row    = #rendered,
      label_start = #prefix,
      label_spans = label_spans,
      label_end   = #(rendered[label_row] or ""),
      desc_row    = desc_row,
      desc_end    = desc_end,
    }
  end

  buf:lines(rendered):clear_ns(NS_MENU_NUM):clear_ns(NS_MENU_SELECTED):clear_ns(NS_MENU_DESC):clear_ns(NS_MENU_DISABLED)
  for i, m in ipairs(meta) do
    local item = items[i] or {}
    if item.disabled then
      for row = m.label_row, m.last_row do
        local line = rendered[row] or ""
        if #line > 0 then buf:mark(NS_MENU_DISABLED, row, 0, { end_col = #line, dim = true }) end
      end
    else
      if numbered and m.label_start > 0 then
        buf:mark(NS_MENU_NUM, m.label_row, 0, { end_col = m.label_start, dim = true })
      end
      if m.desc_row and m.desc_end and m.desc_end > 0 then
        for row = m.desc_row, m.last_row do
          local line = rendered[row] or ""
          if #line > 0 then buf:mark(NS_MENU_DESC, row, 0, { end_col = #line, dim = true }) end
        end
      end
    end
  end
  return meta
end

-- Normalize `items` into `{ label, description?, key? }` tables.
local function normalize_items(items)
  local out = {}
  local has_descriptions = false
  for i, it in ipairs(items or {}) do
    local entry
    if type(it) == "string" then
      entry = { label = it }
    elseif type(it) == "table" then
      entry = {}
      for k, v in pairs(it) do entry[k] = v end
      entry.label = entry.label or ""
      if entry.description and entry.description ~= "" then
        has_descriptions = true
      end
    else
      entry = { label = tostring(it) }
    end
    out[i] = entry
  end
  return out, has_descriptions
end

--- Each item displayed in `smelt.dialog.menu`. Strings are also accepted
--- and lifted into this shape automatically.
---@class smelt.dialog.MenuItem
---@field label string Row text after the dim ` N. ` numbering.
---@field description? string Optional second row, rendered dim.
---@field key? string Optional chord that triggers this item (defaults to its 1-based index for items 1..9).
---@field disabled? boolean Render dimmed and skip selection/submission when true.

--- Options accepted by `smelt.dialog.menu`.
---@class smelt.dialog.MenuOpts
---@field selected? integer 1-based starting cursor (default 1).
---@field shortcuts? "submit"|"select"|false Digit-key behavior. Default `"submit"`.
---@field numbered? boolean Show the dim ` N. ` prefix (default true).
---@field wrap? boolean Wrap long labels/descriptions to the menu width (default true). Set false to clip long rows.
---@field wrap_width? integer Optional pre-layout width hint. Mounted menus use their resolved content width.
---@field on_submit? fun(ctx: any): any Override the submit path. `ctx` carries the dialog handles plus `ctx.index` (1-based) and `ctx.item`. Default resolves the active dialog with `{ index, item }`.

-- Build a selectable menu and return `(leaf, controller)`. The controller uses
-- 1-based item indices for cursor, item replacement, and submission. By
-- default, submitting resolves the active dialog with `{ index, item }`;
-- `opts.on_submit` can replace that behavior.
---@type fun(items: (string|smelt.dialog.MenuItem)[], opts: smelt.dialog.MenuOpts?): smelt.win.Win, table
function smelt.dialog.menu(items, opts)
  opts = opts or {}
  local normalized, has_descriptions = normalize_items(items)
  if #normalized == 0 then
    normalized = { { label = "" } }
  end
  local numbered  = opts.numbered ~= false
  local shortcuts = opts.shortcuts
  if shortcuts == nil then shortcuts = "submit" end

  local wrap = opts.wrap ~= false
  local initial_wrap_width = tonumber(opts.wrap_width or 0)
  if not initial_wrap_width or initial_wrap_width <= 0 then initial_wrap_width = nil end
  local menu_meta = {}
  local item_count = #normalized

  local function row_of(i)
    local m = menu_meta[i]
    if m and m.label_row then return m.label_row - 1 end
    return i - 1
  end
  local function index_of_row(r)
    local row = (r or 0) + 1
    for i, m in ipairs(menu_meta) do
      if row >= m.label_row and row <= m.last_row then return i end
    end
    return math.max(1, math.min(item_count, row))
  end
  local function enabled(i) return normalized[i] and normalized[i].disabled ~= true end

  local function selectable_index(i, dir)
    if item_count == 0 then return 1 end
    if i < 1 then i = 1 end
    if i > item_count then i = item_count end
    if enabled(i) then return i end

    dir = dir or 1
    for step = 1, item_count - 1 do
      local candidate = i + step * dir
      if candidate >= 1 and candidate <= item_count and enabled(candidate) then return candidate end
    end
    for step = 1, item_count - 1 do
      local candidate = i - step * dir
      if candidate >= 1 and candidate <= item_count and enabled(candidate) then return candidate end
    end
    return i
  end

  local selected = selectable_index(tonumber(opts.selected or 1) or 1, 1)

  local buf = smelt.buf.new()
  local rendered_width
  local function render_current(width)
    menu_meta = render_menu(buf, normalized, has_descriptions, numbered, wrap and width or nil)
    rendered_width = width
  end
  render_current(initial_wrap_width)

  local leaf = smelt.win.new(buf, {
    region         = REGION,
    surface       = "list",
    pad_left       = GUTTER,
    pad_right      = GUTTER,
    scrollbar      = false,
    kind           = "list",
    -- Menu metadata already accounts for every rendered row. Soft wrapping
    -- would introduce visual rows that selection and highlights cannot track.
    wrap           = false,
    initial_cursor = row_of(selected),
  })

  local function place_cursor(index)
    local row = row_of(index)
    local item = menu_meta[index]
    local continuation_rows = item and (item.last_row - item.label_row) or 0
    leaf:cursor(row)
    leaf:reveal(row, { cursor = false, bottom_padding = continuation_rows })
  end

  local function sync_highlight()
    buf:clear_ns(NS_MENU_SELECTED)
    local m = menu_meta[selected]
    if not m then
      leaf:row_highlights({})
      return
    end
    for _, span in ipairs(m.label_spans or {}) do
      if span.end_col > span.start then
        buf:mark(NS_MENU_SELECTED, span.row, span.start, {
          end_col  = span.end_col,
          hl_group = "SmeltAccent",
        })
      end
    end
    leaf:row_highlights({ {
      start    = m.label_row - 1,
      ["end"]  = m.label_last_row,
      hl_group = "CursorLine",
      mode     = "always",
      width    = "full",
    } })
  end
  sync_highlight()

  local function select(index, dir)
    selected = selectable_index(index, dir or 1)
    place_cursor(selected)
    sync_highlight()
  end

  -- Window cursor writes can come from plugins as well as menu bindings.
  -- Read the current row, not the event payload: queued events may precede
  -- a reflow or a newer selection change.
  local function sync_selection()
    local row = leaf:cursor() or 0
    local index = index_of_row(row)
    if index ~= selected or row ~= row_of(selected) then select(index) end
  end
  leaf:on("selection_changed", sync_selection)

  leaf:set_renderer(function()
    local width = leaf:content_width()
    if width == nil then return end
    sync_selection()
    if wrap and width ~= rendered_width then
      render_current(width)
      sync_highlight()
    end
    place_cursor(selected)
  end)

  local function sync_menu(next_items)
    sync_selection()
    local next_has_descriptions
    normalized, next_has_descriptions = normalize_items(next_items)
    if #normalized == 0 then normalized = { { label = "" } } end
    has_descriptions = next_has_descriptions
    item_count = #normalized
    render_current(wrap and leaf:content_width() or nil)
    select(selected)
  end

  local ctrl = {}
  function ctrl:cursor(i)
    if i == nil then
      sync_selection()
      return selected
    end
    select(i)
    return self
  end
  function ctrl:item() return normalized[self:cursor()] end
  function ctrl:items() return normalized end
  function ctrl:set_items(next_items)
    sync_menu(next_items)
    return self
  end
  function ctrl:size() return item_count end

  local function default_on_submit(ctx)
    if ctx.resolve then
      ctx.resolve({ index = ctx.index, item = ctx.item })
    end
  end

  -- Builds the submit ctx by layering `index`/`item` over the active
  -- dialog's ctx so handlers get one consistent shape (matches
  -- `dialog.open`'s `on_submit(ctx)` argument).
  local function submit_at(i)
    if i < 1 or i > item_count or not enabled(i) then return end
    select(i)
    local dlg = smelt.dialog.current() or {}
    local ctx = {
      win          = dlg.win,
      panels       = dlg.panels,
      focused_leaf = dlg.focused_leaf,
      resolve      = dlg.resolve,
      close        = dlg.close,
      index        = i,
      item         = normalized[i],
    }
    local handler = opts.on_submit or default_on_submit
    local ok, err = pcall(handler, ctx)
    if not ok then report_callback_error("menu submit", err) end
  end

  function ctrl:submit()
    -- Reject disabled cursor targets before normalizing an external write.
    if not enabled(index_of_row(leaf:cursor() or 0)) then return end
    sync_selection()
    submit_at(selected)
  end

  local function move(units)
    return function()
      local cur = ctrl:cursor()
      local target = cur + units
      if target < 1 then target = 1 end
      if target > item_count then target = item_count end
      select(target, units < 0 and -1 or 1)
    end
  end
  leaf:key("up",   move(-1))
  leaf:key("down", move(1))
  leaf:key("k",    move(-1))
  leaf:key("j",    move(1))
  leaf:key("c-k",  move(-1))
  leaf:key("c-j",  move(1))
  leaf:key("c-p",  move(-1))
  leaf:key("c-n",  move(1))
  leaf:key("pgup", move(-10))
  leaf:key("pgdn", move(10))
  leaf:key("c-u",  move(-5))
  leaf:key("c-d",  move(5))
  leaf:key("home", function() ctrl:cursor(1) end)
  leaf:key("end",  function() ctrl:cursor(item_count) end)

  leaf:key("enter", function() ctrl:submit() end)

  -- Digit shortcuts. `key = "X"` on an item overrides its digit binding;
  -- without an override items 1..9 use their 1-based index. Bindings live
  -- on the leaf (tier 1a) so typing digits into a sibling input leaf
  -- still inserts the character.
  if shortcuts then
    local function chord_for(i, item)
      if item.key and item.key ~= "" then return item.key end
      if i <= 9 then return tostring(i) end
      return nil
    end
    for i, item in ipairs(normalized) do
      local chord = chord_for(i, item)
      if chord then
        if shortcuts == "submit" then
          leaf:key(chord, function() submit_at(i) end)
        else
          leaf:key(chord, function() ctrl:cursor(i) end)
        end
      end
    end
  end

  return leaf, ctrl
end

-- Wrap an existing `buf` as a selectable list leaf. Use when the buffer
-- contents need to be mutated live (vs. the snapshot supplied to
-- `smelt.dialog.menu`). `opts.surface` defaults to `"list"`; `opts.selected`
-- (0-based) sets the initial cursor row. Long lines clip instead of wrapping
-- so each buffer line remains one selectable row.
---@type fun(buf: smelt.buf.Buf, opts: table?): smelt.win.Win
function smelt.dialog.list(buf, opts)
  opts = opts or {}
  local surface = opts.surface or "list"
  local leaf = smelt.win.new(buf, {
    region = REGION, surface = surface,
    pad_left = GUTTER, pad_right = GUTTER, scrollbar = false,
    kind = "list", wrap = false,
    initial_cursor = opts.selected or 0,
  })
  return leaf
end

local function split_lines(text)
  if text == "" then return { "" } end
  local out = {}
  for line in tostring(text):gmatch("([^\n]*)\n?") do
    if line == "" and #out > 0 and out[#out] == "" then break end
    table.insert(out, line)
  end
  if #out == 0 then out = { "" } end
  return out
end

-- Render `text` as a non-focusable markdown leaf. Convenience wrapper
-- around `smelt.dialog.content` for static narrative panels (notes,
-- summaries, intros). Returns `(leaf, buf)`.
---@type fun(text: string): smelt.win.Win, smelt.buf.Buf
function smelt.dialog.markdown(text)
  local buf = smelt.buf.new({ mode = "markdown" })
  buf:source(text or "")
  local leaf = smelt.win.new(buf, {
    region = REGION, surface = "selectable_text",
    pad_left = GUTTER, pad_right = GUTTER,
  })
  return leaf, buf
end

-- General-purpose body leaf. Pass `opts.buf` to wrap an existing buffer
-- or `opts.text` to spin up a fresh read-only one. `opts.readonly` can
-- force the backing buffer's readonly flag when a caller supplies `opts.buf`.
-- `opts.interactive` enables focus + vim keymaps (when the user has vim mode
-- on); `opts.wrap` mirrors `smelt.win.new`. `opts.pad_left` / `opts.pad_right`
-- override the dialog gutter. Returns `(leaf, buf)`.
---@type fun(opts: table?): smelt.win.Win, smelt.buf.Buf
function smelt.dialog.content(opts)
  opts = opts or {}
  local buf = opts.buf
  if not buf then
    buf = smelt.buf.new({ readonly = true })
    if opts.text and opts.text ~= "" then
      buf:lines(split_lines(opts.text))
    end
  end
  if opts.readonly ~= nil then
    buf:readonly(opts.readonly and true or false)
  end
  local surface = opts.surface
  if surface == nil then surface = opts.interactive and "readonly_text" or "selectable_text" end
  -- `wrap` defaults to true (matches `smelt.win.new`); pass `wrap = false` to
  -- show pre-styled content (e.g. via `buf:styled(...)`) at its
  -- intrinsic width without soft-wrapping the row.
  local leaf = smelt.win.new(buf, {
    region      = REGION,
    surface     = surface,
    vim_enabled = (opts.interactive and smelt.settings.vim) and true or false,
    pad_left    = opts.pad_left or GUTTER,
    pad_right   = opts.pad_right or GUTTER,
    wrap        = opts.wrap,
  })
  return leaf, buf
end

local function viewer_keymaps(extra)
  local keymaps = {
    { key = "?", on_press = function(ctx) ctx.close() end },
  }
  for _, km in ipairs(extra or {}) do keymaps[#keymaps + 1] = km end
  return keymaps
end

--- Open a read-only content dialog. Pass `text` for plain text, `lines` for
--- plain line tables, `styled` for styled lines, or `buf` for a live buffer the
--- caller will update after opening. Returns `(handle, buf, leaf)`.
---@type fun(opts: table): table, smelt.buf.Buf, smelt.win.Win
function smelt.dialog.viewer(opts)
  opts = opts or {}
  local buf = opts.buf or smelt.buf.new({ readonly = true })
  if opts.styled then
    buf:styled(opts.styled)
  elseif opts.lines then
    buf:lines(opts.lines)
  elseif opts.text ~= nil then
    buf:lines(split_lines(opts.text or ""))
  end

  local leaf = smelt.dialog.content({
    buf         = buf,
    interactive = opts.interactive ~= false,
    wrap        = opts.wrap,
    surface     = opts.surface,
  })

  local panel = { leaf = leaf, height = opts.panel_height }
  local max_height = opts.max_height
  if opts.height == nil and max_height == nil then max_height = "50%" end
  local handle = smelt.dialog.new({
    title      = opts.title,
    height     = opts.height,
    max_height = max_height,
    min_height = opts.min_height,
    panels     = { panel },
    keymaps    = viewer_keymaps(opts.keymaps),
    close_with_q = opts.close_with_q ~= false,
  })
  return handle, buf, leaf
end

-- ── Dialog root-layout wrapper ───────────────────────────────────────

local function copy_title_span(span)
  if type(span) == "string" then
    return { text = span, dim = true }
  end
  if type(span) ~= "table" then return span end
  local out = {}
  for k, v in pairs(span) do out[k] = v end
  out.dim = true
  return out
end

--- Return a title spec styled for dialog/window chrome. Use this for raw
--- overlays or layout leaves that should match dialog title treatment;
--- `smelt.dialog.open` applies it automatically to its own title and panels.
---@type fun(title: string|table, opts?: { pad?: boolean }): table|string|nil
function smelt.dialog.title(title, opts)
  opts = opts or {}
  if title == nil or title == "" then return title end

  local spans
  if type(title) == "string" then
    spans = { { text = title, dim = true } }
  elseif type(title) == "table" then
    if title.text ~= nil then
      spans = { copy_title_span(title) }
    elseif #title > 0 then
      spans = {}
      for _, span in ipairs(title) do spans[#spans + 1] = copy_title_span(span) end
    else
      return title
    end
  else
    return title
  end

  if opts.pad then
    table.insert(spans, 1, { text = " " })
    spans[#spans + 1] = { text = " " }
  end
  return spans
end

local function build_dialog(opts)
  if opts.height ~= nil and opts.max_height ~= nil then
    error("smelt.dialog: use `height` (fixed) or `max_height` (fit to content), not both", 3)
  end
  local fit_mode = opts.max_height ~= nil or opts.height == nil
  local default_panel_height = fit_mode and "fit" or nil

  local top_panels = opts.panels or {}
  local bottom_panels = opts.bottom_panels or {}
  local chrome = {
    border = opts.border or { top = "SmeltAccent" },
    title = smelt.dialog.title(opts.title, { pad = true }),
  }
  -- Numeric heights describe the body, excluding the top chrome row.
  local height = opts.height or "fit"
  if type(height) == "number" then height = height + 1 end
  if opts.layout ~= nil then
    if opts.panels ~= nil or opts.bottom_panels ~= nil then
      error("smelt.dialog: use `layout` or `panels`, not both", 3)
    end
    local leaves = smelt.ui.layout.windows(opts.layout)
    if #leaves == 0 then error("smelt.dialog: layout must contain a window", 3) end
    local layout = smelt.ui.layout.frame(opts.layout, chrome)
    return leaves[1], leaves, layout, height
  end
  if #top_panels == 0 and #bottom_panels == 0 then
    error("smelt.dialog: panels or bottom_panels must be non-empty", 3)
  end

  local leaves = {}
  local function build_layout_items(panels, start_index)
    local layout_items = {}
    for i, p in ipairs(panels) do
      if type(p) ~= "table" or p.leaf == nil then
        error("smelt.dialog: panel " .. (start_index + i - 1) .. " requires a `leaf`", 3)
      end
      leaves[#leaves + 1] = p.leaf
      local leaf_node = smelt.ui.layout.leaf(p.leaf, {
        border              = p.border,
        title               = smelt.dialog.title(p.title),
        collapse_when_empty = p.collapse_when_empty or false,
      })
      layout_items[i] = { leaf_node, height = p.height or default_panel_height }
    end
    return layout_items
  end

  local top_items = build_layout_items(top_panels, 1)
  local bottom_items = build_layout_items(bottom_panels, #top_panels + 1)

  local layout
  if #top_items > 0 and #bottom_items > 0 then
    chrome.gap = opts.bottom_gap or 0
    chrome.justify = "space-between"
    layout = smelt.ui.layout.vbox({
      { smelt.ui.layout.vbox(top_items),    height = "fit" },
      { smelt.ui.layout.vbox(bottom_items), height = "fit" },
    }, chrome)
  else
    layout = smelt.ui.layout.vbox(#top_items > 0 and top_items or bottom_items, chrome)
  end

  return leaves[1], leaves, layout, height
end

-- Wire dialog-level keymaps, focus, events, and the resolve handle. Shared between
-- `open` (coroutine) and `new` (sync).
local function setup_lifecycle(opts, leaves, layout, height, resolve_fn)
  local root = leaves[1]

  -- Shared dialog ctx. `focused_leaf` is mutated live by the focus event
  -- handlers below so callbacks always read the current value. Exposed via
  -- `smelt.dialog.current()` and as the `ctx` arg to `opts.keymaps` /
  -- `on_submit` handlers.
  local ctx = {
    win          = root,
    panels       = leaves,
    focused_leaf = opts.focus,
  }

  local make_ctx
  local resolved = false
  local function resolve(value)
    if resolved then return end
    resolved = true
    if type(opts.on_close) == "function" then
      local ok, err = pcall(opts.on_close, make_ctx())
      if not ok then report_callback_error("on_close", err) end
    end
    -- Pop our entry off the dialog stack. We scan the stack from the top
    -- in case nested dialogs resolve out of order (a child dialog closes
    -- last) - only remove our own entry.
    for i = #dialog_stack, 1, -1 do
      if dialog_stack[i] == ctx then
        table.remove(dialog_stack, i)
        break
      end
    end
    ctx.host:close()
    resolve_fn(value)
  end
  ctx.resolve = resolve
  ctx.close   = function() resolve(nil) end

  ctx.host = __smelt_internal.dialog.__open({
    layout = layout,
    height = height,
    min_height = opts.min_height,
    max_height = opts.max_height,
    blocks_agent = opts.blocks_agent or false,
    resizable = opts.resizable ~= false,
  })
  ctx.toggle_expanded = function() ctx.host:toggle_expanded() end
  table.insert(dialog_stack, ctx)

  -- Explicit focus override; otherwise the modal host focuses the first
  -- focusable leaf after mounting the dialog in the root layout.
  if opts.focus then opts.focus:focus() end

  for _, leaf in ipairs(leaves) do
    leaf:on("focus", function()
      ctx.focused_leaf = leaf
    end)
  end

  -- Build a ctx for user callbacks. Raw event fields (`text`, `index`, `code`,
  -- `mods`, `leaf`) flow through unchanged; the shared dialog ctx is layered
  -- underneath so callbacks see `resolve`, `close`, `panels`, `focused_leaf`.
  make_ctx = function(raw_ctx)
    local out = {
      win          = ctx.win,
      panels       = ctx.panels,
      focused_leaf = ctx.focused_leaf,
      resolve         = ctx.resolve,
      close           = ctx.close,
      toggle_expanded = ctx.toggle_expanded,
    }
    if type(raw_ctx) == "table" then
      for k, v in pairs(raw_ctx) do out[k] = v end
    end
    return out
  end

  -- Dialog-level keymaps belong to the modal scope, independent of whether
  -- the dialog is mounted in the root layout or a future floating container.
  ctx.host:key("ctrl-o", function() ctx.toggle_expanded() end)
  local keymaps = dialog_keymaps(opts)
  if #keymaps > 0 then
    for _, km in ipairs(keymaps) do
      if type(km) == "table" and km.key and type(km.on_press) == "function" then
        local on_press = km.on_press
        ctx.host:key(km.key, function(raw_ctx)
          local ok, err = pcall(on_press, make_ctx(raw_ctx))
          if not ok then report_callback_error("keymap", err) end
        end)
      end
    end
  end

  -- Events fire on the leaf that emits them (no implicit bubbling), so dialog-wide
  -- handlers register on every leaf to catch events from any panel.
  local function register_on_all(event_name, handler)
    for _, leaf in ipairs(leaves) do
      leaf:on(event_name, handler)
    end
  end

  -- Submit: fires `opts.on_submit` if provided. With no handler, the dialog stays
  -- open - Enter doing nothing is easier to diagnose than Enter mysteriously
  -- closing. Esc/Ctrl-C still dismisses via the Dismiss event below.
  if type(opts.on_submit) == "function" then
    register_on_all("submit", function(raw_ctx)
      local ok, err = pcall(opts.on_submit, make_ctx(raw_ctx))
      if not ok then report_callback_error("on_submit", err) end
    end)
  end

  -- Dismiss: Esc / Ctrl-C / outside-modal click. Defaults to resolve(nil).
  register_on_all("dismiss", function(raw_ctx)
    if type(opts.on_dismiss) == "function" then
      local ok, err = pcall(opts.on_dismiss, make_ctx(raw_ctx))
      if not ok then report_callback_error("on_dismiss", err) end
    else
      resolve(nil)
    end
  end)

  if type(opts.on_tick) == "function" then
    register_on_all("tick", function(raw_ctx)
      local ok, err = pcall(opts.on_tick, make_ctx(raw_ctx))
      if not ok then report_callback_error("on_tick", err) end
    end)
  end

  if type(opts.on_event) == "table" then
    for event_name, fn in pairs(opts.on_event) do
      if type(fn) == "function" then
        register_on_all(event_name, function(raw_ctx)
          local ok, err = pcall(fn, make_ctx(raw_ctx))
          if not ok then report_callback_error("on_event[" .. event_name .. "]", err) end
        end)
      end
    end
  end

  return resolve, root
end

-- Coroutine-blocking dialog opener. Builds the root dialog from `opts.panels`
-- (each `{ leaf, height }`), wires `opts.keymaps`, then yields the
-- caller until a handler calls `ctx.resolve(value)`. Must run inside a
-- `smelt.spawn` (or tool execute) frame; returns the resolved value or
-- `nil` on dismiss.
---@type fun(opts: smelt.dialog.Opts): any
function smelt.dialog.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.dialog.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.dialog.open: expected table of options", 2)
  end

  local _, leaves, layout, height = build_dialog(opts)
  local task_id = smelt.task.alloc()
  setup_lifecycle(opts, leaves, layout, height, function(value)
    smelt.task.resume(task_id, value)
  end)
  return smelt.task.wait(task_id, { interactive = true })
end

-- ── Picker preset ─────────────────────────────────────────────────────
--
-- `smelt.dialog.picker(opts)` is a thin wrapper that bundles the recurring
-- Telescope-style shape: a single-line input on top, a non-focusable list
-- below, navigation forwarded from the input to the list, Enter submits.
-- Coroutine-blocking like `smelt.dialog.open`; returns the value resolved
-- from `on_submit` (or `nil` on dismiss).
--
-- Opts:
--   * `items`       - array of arbitrary item tables (passed to `render`).
--   * `render`      - `function(item) -> { text = ..., marks = ... }` (see
--                     `smelt.list`).
--   * `filter`      - optional predicate `function(item) -> bool` applied
--                     to every refilter; the picker re-runs it whenever
--                     the query changes (so it can close over the live
--                     query state).
--   * `placeholder` - input prompt text. Defaults to `""`.
--   * `empty_text`  - shown in the list when nothing matches.
--   * `on_open`     - `function(ctx)` fires once before the dialog blocks,
--                     after the input/list have been built. Use it to seed
--                     marks on the input buffer or to set an initial cursor
--                     row on the list.
--   * `on_query`    - `function(query, ctx)` fires on every keystroke.
--                     The default is `list:set_filter(opts.filter)`. Pass
--                     this when you want to swap the filter (e.g. rebuild
--                     it from a fresh query).
--   * `on_submit`   - `function(ctx)` fires on Enter. `ctx.item` is the
--                     highlighted row (nil when the list is empty);
--                     `ctx.list`/`ctx.input`/`ctx.input_buf` are added
--                     by the picker. Default resolves with `ctx.item`
--                     when non-nil.
--   * `keymaps`     - extra dialog-level keymaps merged on top of the
--                     built-in navigation bindings. Each entry's
--                     `on_press(ctx)` receives the picker ctx with
--                     `ctx.list`, `ctx.input`, `ctx.input_buf` added.
--   * `title`, `height`, `max_height`, `min_height`, `blocks_agent` - forwarded to
--                     `smelt.dialog.open`.

local NAV_KEYS = {
  { "up",     -1  },
  { "down",   1   },
  { "ctrl-k", -1  },
  { "ctrl-j", 1   },
  { "ctrl-p", -1  },
  { "ctrl-n", 1   },
  { "pgup",   -10 },
  { "pgdn",   10  },
  { "ctrl-u", -5  },
  { "ctrl-d", 5   },
}

-- Coroutine-blocking Telescope-style picker. Stacks a single-line input
-- on top of a list driven by `smelt.list.new`; navigation forwards from
-- input to list, Enter resolves with the selected item. See the doc
-- block above `NAV_KEYS` for every accepted `opts` field. Returns the
-- value resolved from `on_submit` (defaults to the highlighted item) or
-- `nil` on dismiss.
---@type fun(opts: smelt.dialog.PickerOpts): any
function smelt.dialog.picker(opts)
  if not coroutine.isyieldable() then
    error("smelt.dialog.picker: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.dialog.picker: expected table of options", 2)
  end
  if type(opts.render) ~= "function" then
    error("smelt.dialog.picker: opts.render must be a function", 2)
  end

  local input_leaf, input_buf = smelt.dialog.input(opts.placeholder or "")
  local list_buf  = smelt.buf.new()
  local list_leaf = smelt.dialog.list(list_buf, { surface = "list_inert" })

  local list = smelt.list.new({
    leaf       = list_leaf,
    buf        = list_buf,
    items      = opts.items or {},
    render     = opts.render,
    filter     = opts.filter,
    empty_text = opts.empty_text or "  (no matches)",
  })

  local function augment(ctx)
    ctx.list      = list
    ctx.input     = input_leaf
    ctx.input_buf = input_buf
    return ctx
  end

  if type(opts.on_open) == "function" then
    opts.on_open(augment({}))
  end

  input_leaf:on("text_changed", function(raw)
    local query = (raw and raw.text) or ""
    if type(opts.on_query) == "function" then
      opts.on_query(query, augment({ text = query }))
    elseif opts.filter ~= nil then
      list:set_filter(opts.filter)
    end
  end)

  local function nav(delta)
    return function() list:move_cursor(delta) end
  end

  local keymaps = {}
  for _, n in ipairs(NAV_KEYS) do
    table.insert(keymaps, { key = n[1], on_press = nav(n[2]) })
  end
  if type(opts.keymaps) == "table" then
    for _, km in ipairs(opts.keymaps) do
      local fn = km.on_press
      table.insert(keymaps, {
        key      = km.key,
        hint     = km.hint,
        on_press = function(ctx) return fn(augment(ctx)) end,
      })
    end
  end

  local on_submit
  if type(opts.on_submit) == "function" then
    on_submit = function(ctx)
      ctx.item = list:selected()
      return opts.on_submit(augment(ctx))
    end
  else
    on_submit = function(ctx)
      local item = list:selected()
      if item ~= nil then ctx.resolve(item) end
    end
  end

  return smelt.dialog.open({
    title        = opts.title,
    height       = opts.height,
    max_height   = opts.max_height,
    min_height   = opts.min_height,
    blocks_agent = opts.blocks_agent,
    panels = {
      { leaf = input_leaf, height = 1      },
      { leaf = list_leaf,  height = "fill" },
    },
    focus     = input_leaf,
    keymaps   = keymaps,
    on_submit = on_submit,
    on_dismiss = opts.on_dismiss,
  })
end

-- Non-coroutine open. Returns `{ win, panels, close() }` synchronously.
-- The consumer drives the lifecycle via `on_submit` / `on_dismiss`
-- callbacks and tears down with `handle:close()`. No value flows back
-- - use `smelt.dialog.open` when you need to read the result.
---@type fun(opts: smelt.dialog.Opts): table
function smelt.dialog.new(opts)
  if type(opts) ~= "table" then
    error("smelt.dialog.new: expected table of options", 2)
  end
  local _, leaves, layout, height = build_dialog(opts)
  local resolve, root = setup_lifecycle(opts, leaves, layout, height, function(_) end)
  return {
    win    = root,
    panels = leaves,
    close  = function() resolve(nil) end,
  }
end

return M
