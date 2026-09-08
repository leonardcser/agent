-- `smelt.list`: structured-list helper over a `smelt.dialog.list` leaf.
--
-- The dialog list primitive is a generic line buffer with a cursor. This
-- helper layers over it the bookkeeping a typical picker needs: a list of
-- arbitrary item tables, a per-item render function, an optional filter,
-- and a single mapping between the cursor row and the original item. Drop
-- to raw `dialog.list` whenever you need finer control.
--
-- Usage:
--   local leaf = smelt.dialog.list(buf, { surface = "list_inert" })
--   local list = smelt.list.new({
--     leaf = leaf, buf = buf, items = entries,
--     render = function(it)
--       return {
--         text  = string.format("%s%s", string.rep("  ", it.depth or 0), it.label),
--         marks = { { col = 0, opts = { end_col = 8, dim = true } } },
--       }
--     end,
--     filter = function(it) return it.cwd == current_cwd end,
--     empty_text = "  (no matches)",
--   })
--
--   list:set_filter(function(it) ... end)   -- swaps the predicate + refresh
--   list:set_items_preserve(items, function(it) return it.id end)
--   list:refresh()                          -- re-derive visible + re-render
--   local item = list:selected()            -- or nil
--   list:move_cursor(1)
--
-- Text rendered by `render(item).text` is truncated with `smelt.text.fit`
-- to the leaf's `content_width()` so long items don't trigger horizontal
-- panning. Return `spans = { { text, style?, syntax? }, ... }` to render the
-- fitted row through `buf:styled`, including inline syntax highlighting.
-- The list re-renders automatically when the leaf is resized.

smelt.list = smelt.list or {}

local NS = smelt.ns("smelt.list.marks")

local List = {}
List.__index = List

local function clone_marks(marks)
  if not marks then return nil end
  local out = {}
  for i, m in ipairs(marks) do out[i] = m end
  return out
end

local function span_text(spans)
  local out = {}
  for i, span in ipairs(spans or {}) do out[i] = span.text or "" end
  return table.concat(out)
end

local function fit_spans(spans, width)
  if not spans then return nil end
  if not width or width <= 0 then return spans end

  local fitted = {}
  local remaining = width
  for _, span in ipairs(spans) do
    if remaining <= 0 then break end
    local text = span.text or ""
    local span_width = smelt.text.width(text)
    local part = text
    if span_width > remaining then
      part = smelt.text.fit(text, remaining)
    end
    if part ~= "" then
      local out = { text = part }
      if span.style then out.style = span.style end
      if span.syntax then out.syntax = span.syntax end
      fitted[#fitted + 1] = out
    end
    remaining = remaining - smelt.text.width(part)
  end
  if remaining > 0 then
    fitted[#fitted + 1] = { text = string.rep(" ", remaining) }
  end
  return fitted
end

local function plain_spans(text)
  return { { text = text } }
end

-- Read the leaf's resolved inner-content width in cells (gutter and pad
-- already subtracted). Returns nil until the leaf has been laid out.
local function content_width(self)
  return self.leaf:content_width()
end

local function content_height(self)
  local rect = self.leaf:rect() or {}
  return rect.height
end

local function top_padding(self, row_count)
  if self.anchor ~= "bottom" then return 0 end
  local height = content_height(self)
  if not height or height <= row_count then return 0 end
  return height - row_count
end

local function render_visible(self)
  self.buf:clear_ns(NS)
  local visible = self.visible_items
  local width = content_width(self)
  if #visible == 0 then
    local pad = top_padding(self, 1)
    local lines = {}
    for i = 1, pad do lines[i] = "" end
    lines[pad + 1] = self.empty_text
    self.buf:lines(lines)
    self.buf:mark(NS, pad + 1, 0, { end_col = #self.empty_text, dim = true })
    self.row_offset = pad
    return
  end
  local pad = top_padding(self, #visible)
  local lines = {}
  local styled_lines = {}
  local row_marks = {}
  local use_styled = false
  for i = 1, pad do
    lines[i] = ""
    styled_lines[i] = plain_spans("")
  end
  for i, item in ipairs(visible) do
    local row = pad + i
    local rendered = self.render(item) or {}
    local text = rendered.text or span_text(rendered.spans)
    -- Truncate to the leaf's content width so long items don't trigger
    -- horizontal panning. `fit` pads to exact width - trailing whitespace
    -- is invisible and keeps the selection-highlight row uniform.
    if width and width > 0 then
      text = smelt.text.fit(text, width)
    end
    lines[row] = text
    if rendered.spans then
      use_styled = true
      styled_lines[row] = fit_spans(rendered.spans, width)
    else
      styled_lines[row] = plain_spans(text)
    end
    row_marks[row] = clone_marks(rendered.marks)
  end
  if use_styled then
    self.buf:styled(styled_lines)
  else
    self.buf:lines(lines)
  end
  for row = 1, #lines do
    local marks = row_marks[row]
    if marks then
      for _, m in ipairs(marks) do
        self.buf:mark(NS, row, m.col or 0, m.opts or {})
      end
    end
  end
  self.row_offset = pad
end

local function rederive_visible(self)
  if not self.filter then
    self.visible_items = self.items
    return
  end
  local out = {}
  for _, it in ipairs(self.items) do
    if self.filter(it) then table.insert(out, it) end
  end
  self.visible_items = out
end

function List:refresh()
  rederive_visible(self)
  render_visible(self)
  self:set_cursor(0)
end

function List:set_items(items)
  self.items = items or {}
  self:refresh()
end

function List:set_items_preserve(items, key_fn)
  if type(key_fn) ~= "function" then
    error("smelt.list: key function is required", 2)
  end
  local selected = self:selected()
  local selected_key = selected and key_fn(selected)
  self.items = items or {}
  self:refresh()
  if selected_key == nil then return end
  for i, item in ipairs(self.visible_items) do
    if key_fn(item) == selected_key then
      self:set_cursor(i - 1)
      return
    end
  end
end

function List:set_filter(fn)
  self.filter = fn
  self:refresh()
end

function List:set_render(fn)
  if type(fn) ~= "function" then
    error("smelt.list: render must be a function", 2)
  end
  self.render = fn
  render_visible(self)
end

function List:visible()
  return self.visible_items
end

function List:size()
  return #self.visible_items
end

function List:selected_index()
  if #self.visible_items == 0 then return nil end
  local row = self.leaf:cursor() or self.row_offset or 0
  local idx = row - (self.row_offset or 0)
  if idx < 0 then idx = 0 end
  if idx >= #self.visible_items then idx = #self.visible_items - 1 end
  return idx
end

function List:selected()
  local idx = self:selected_index()
  if not idx then return nil end
  return self.visible_items[idx + 1]
end

function List:set_cursor(i)
  local n = #self.visible_items
  if n == 0 then
    self.leaf:cursor(self.row_offset or 0)
    return
  end
  local idx = math.max(0, math.min(n - 1, i or 0))
  self.leaf:cursor((self.row_offset or 0) + idx)
end

function List:move_cursor(delta)
  local idx = self:selected_index()
  if not idx then return end
  self:set_cursor(idx + delta)
end

--- Extmark attached to one rendered list row.
---@class smelt.list.Mark
---@field col? integer 0-based byte column for the mark.
---@field opts? smelt.buf.MarkOpts Mark/highlight options.

--- Styled text segment in a rendered list row.
---@class smelt.list.Span
---@field text string Span text.
---@field style? table Highlight style passed through to `buf:styled`.
---@field syntax? string Inline syntax token for this span.

--- Row shape returned by a `smelt.list` render callback.
---@class smelt.list.Row
---@field text? string Plain row text. Used when `spans` is omitted.
---@field spans? smelt.list.Span[] Styled spans for the row.
---@field marks? smelt.list.Mark[] Extmarks to apply after rendering.

--- Structured list handle returned by `smelt.list.new`.
---@class smelt.list.List
---@field refresh fun(self: smelt.list.List) Re-apply the filter, render visible rows, and move the cursor to the first item.
---@field set_items fun(self: smelt.list.List, items: any[]?) Replace the source items and refresh the list.
---@field set_items_preserve fun(self: smelt.list.List, items: any[]?, key_fn: fun(item: any): any) Replace items and restore the selected row by its `key_fn` result when possible.
---@field set_filter fun(self: smelt.list.List, fn: (fun(item: any): boolean)?) Replace or clear the filter predicate and refresh the list.
---@field set_render fun(self: smelt.list.List, fn: fun(item: any): smelt.list.Row) Replace the row renderer and redraw the current visible items.
---@field visible fun(self: smelt.list.List): any[] Return the filtered items in display order.
---@field size fun(self: smelt.list.List): integer Return the number of visible items.
---@field selected_index fun(self: smelt.list.List): integer? Return the selected visible-item index (0-based), or `nil` when empty.
---@field selected fun(self: smelt.list.List): any Return the selected source item, or `nil` when empty.
---@field set_cursor fun(self: smelt.list.List, i: integer) Move to the clamped 0-based visible-item index.
---@field move_cursor fun(self: smelt.list.List, delta: integer) Move the selection by `delta` rows, clamped to the visible list.

--- Options accepted by `smelt.list.new`. `leaf` and `buf` are mandatory -
--- they own the rendered selection cursor and the backing line buffer;
--- the rest configure how data is sourced, filtered, and rendered.
---@class smelt.list.Opts
---@field leaf smelt.win.Win Selectable list leaf (typically from `smelt.dialog.list`).
---@field buf smelt.buf.Buf Backing buffer that mirrors the rendered rows.
---@field items? any[] Initial item set. Mutate via `:set_items(...)` later if needed.
---@field render fun(item: any): smelt.list.Row Returns `{ text, spans?, marks? }` per visible row.
---@field filter? fun(item: any): boolean Predicate re-run on `:set_filter` / `:refresh`.
---@field empty_text? string Placeholder line shown when no row passes the filter.
---@field anchor? "top"|"bottom" Render short lists at the top or bottom of the viewport. Defaults to "top".

-- Build a structured list bound to the dialog-list `opts.leaf` and its
-- backing `opts.buf`. `opts.items` is the data source; `opts.render(item)`
-- returns `{ text, spans?, marks }`; `opts.filter(item)` is optional and re-runs
-- whenever `:set_filter` / `:refresh` fires. `opts.empty_text` shows
-- when no row passes the filter. `opts.anchor = "bottom"` pads short result
-- sets above the rows so filtered pickers stay pinned to the bottom of their
-- viewport. The returned handle can replace items while preserving selection,
-- change its filter or renderer, inspect visible/selected rows, and move its
-- 0-based cursor.
---@type fun(opts: smelt.list.Opts): smelt.list.List
function smelt.list.new(opts)
  if type(opts) ~= "table" then
    error("smelt.list.new: expected options table", 2)
  end
  if opts.leaf == nil then error("smelt.list.new: opts.leaf is required", 2) end
  if opts.buf == nil then error("smelt.list.new: opts.buf is required", 2) end
  if type(opts.render) ~= "function" then
    error("smelt.list.new: opts.render must be a function", 2)
  end

  local self = setmetatable({
    leaf          = opts.leaf,
    buf           = opts.buf,
    render        = opts.render,
    filter        = opts.filter,
    items         = opts.items or {},
    visible_items = {},
    empty_text    = opts.empty_text or "  (no items)",
    anchor        = opts.anchor or "top",
    row_offset    = 0,
  }, List)
  self:refresh()
  -- Fit rows at the resolved width before painting, preserving the selected
  -- item across width changes and bottom-anchor padding changes.
  local function reflow()
    local idx = self:selected_index()
    render_visible(self)
    if idx then self:set_cursor(idx) end
  end
  self.leaf:set_renderer(reflow)
  return self
end
