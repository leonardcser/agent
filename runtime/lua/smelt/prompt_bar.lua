-- Prompt auxiliary + top + bottom bar windows.
--
-- Default `M.aux_win` owns the stable one-row gap between transcript and
-- prompt chrome. It renders either the discovery tip or a blank row;
-- notification overlays use the same row with higher z. `M.top_win` renders
-- queued messages, a stash marker, and a horizontal-rule bar with a
-- left-aligned working indicator (traveling wave) and a right-aligned
-- model/tokens/cost group. `M.bottom_win` renders a separator-only bar row.
--
-- Plugins can replace any window's renderer via
-- `prompt_bar.aux_win:set_renderer(fn)`, `top_win:set_renderer(fn)` /
-- `bottom_win:set_renderer(fn)`, or replace the windows entirely from a
-- custom `smelt.ui.layout.set` composer.

local bar = require("smelt._bar")
local tips = require("smelt.tips")

local M = {}

local AUX_NS = smelt.ns("smelt.prompt_bar.aux")
local TOP_NS = smelt.ns("smelt.prompt_bar.top")
local BOT_NS = smelt.ns("smelt.prompt_bar.bottom")
local TOKEN_PRIORITY = 0
local INDICATOR_PRIORITY = 1
local LABEL_PRIORITY = 2
local SECONDARY_PRIORITY = 3
local OPTIONAL_PRIORITY = 4
local DEFAULT_BAR_STYLE = { fg = "SmeltSeparator" }
local COMMENT_DIM_STYLE = { fg = "Comment", dim = true }
local NORMAL_BOLD_STYLE = { fg = "Normal", bold = true }
local indicator_cache = { spans = {}, pool = {}, count = 0 }
local renderer_subscriptions = {}

local function invalidate_on(win, names)
  if not win or type(smelt.signal.subscribe) ~= "function" then return end
  for _, name in ipairs(names) do
    renderer_subscriptions[#renderer_subscriptions + 1] = smelt.signal.subscribe(name, function()
      win:invalidate_renderer()
    end)
  end
end

local function invalidate_when_changed(win, name, display_value)
  if not win or type(smelt.signal.subscribe) ~= "function" then return end
  renderer_subscriptions[#renderer_subscriptions + 1] = smelt.signal.subscribe(
    name,
    function(value, previous)
      if display_value(value) ~= display_value(previous) then
        win:invalidate_renderer()
      end
    end)
end

-- ── helpers ─────────────────────────────────────────────────────────

local function queued_message_row(row, width)
  width = math.max(width or 0, 0)
  local kind = row.kind or "turn"
  local marker = kind == "request" and "»" or "›"
  local prefix = "  " .. marker .. " "
  local text, body_end = bar.truncate_right_padded(prefix .. (row.text or ""), width)
  local prefix_end = math.min(#prefix, body_end)
  return {
    text = text,
    highlights = {
      {
        bytes_start = 0,
        bytes_end = prefix_end,
        style = { fg = "Comment" },
        selectable = false,
      },
      {
        bytes_start = prefix_end,
        bytes_end = body_end,
        style = { fg = "Comment" },
      },
    },
  }
end

local function stash_row(width)
  width = math.max(width or 0, 0)
  local indent = "  "
  local label = "◌ Stashed (ctrl+s to unstash)"
  local text, body_end = bar.truncate_right_padded(indent .. label, width)
  return {
    text = text,
    highlights = {
      {
        bytes_start = math.min(#indent, body_end),
        bytes_end = body_end,
        style = { fg = "Comment" },
      },
    },
  }
end

local function more_row(hidden, width)
  width = math.max(width or 0, 0)
  local text, body_end = bar.truncate_right_padded(
    "  +" .. hidden .. " more queued", width)
  return {
    text = text,
    highlights = {
      {
        bytes_start = 0,
        bytes_end = body_end,
        style = { fg = "Comment", dim = true },
      },
    },
  }
end

-- Baseline for whether the aux row may show the tip: empty prompt unless a
-- modal picker owns it, idle, no stash/queue/notification, and tips enabled.
-- The row itself is always reserved by the layout so tips do not shift content.
local function tip_eligible(queued)
  if not tips.enabled() then return false end
  if #queued > 0 or smelt.prompt.has_stash() then return false end
  if (smelt.prompt.text() or "") ~= "" and not smelt.prompt.is_modal() then return false end
  if smelt.signal.get("notification_visible") then return false end
  local work_state = smelt.signal.get("work_state")
  if work_state and work_state ~= "idle" then return false end
  return true
end

local function should_show_tip(queued)
  return tip_eligible(queued)
end

local function tip_row(width)
  width = math.max(width or 0, 0)
  local tip = tips.prompt_tip()
  if not tip then return nil end
  local prefix = "  tip "
  local key = tip.key or ""
  local separator = key ~= "" and ", " or ""
  local body = key .. separator .. (tip.text or "")
  local text, body_end = bar.truncate_right_padded(prefix .. body, width)
  local prefix_end = math.min(#prefix, body_end)
  local key_end = math.min(prefix_end + #key, body_end)
  local desc_start = math.min(key_end + #separator, body_end)
  local highlights = {
    {
      bytes_start = 0,
      bytes_end = prefix_end,
      style = { fg = "SmeltAccent", bold = true },
      selectable = false,
    },
  }
  if key ~= "" then
    highlights[#highlights + 1] = {
      bytes_start = prefix_end,
      bytes_end = key_end,
      style = { fg = "Comment" },
    }
    highlights[#highlights + 1] = {
      bytes_start = key_end,
      bytes_end = desc_start,
      style = { fg = "Comment", dim = true },
      selectable = false,
    }
  end
  highlights[#highlights + 1] = {
    bytes_start = desc_start,
    bytes_end = body_end,
    style = { fg = "Comment" },
  }
  return { text = text, highlights = highlights }
end

-- ── working indicator (top bar left spans) ───────────────────────────
--
-- Reads work_* signals published by the engine. Returns a list of bar
-- spans suitable for `_bar.compose`. Each character of the indicator's
-- "glyph + label" gets its own span so the traveling wave can paint a
-- per-cell gradient.

local function indicator_span(text, style, priority, selectable)
  local index = indicator_cache.count + 1
  indicator_cache.count = index
  local span = indicator_cache.pool[index]
  if not span then
    span = {}
    indicator_cache.pool[index] = span
  end
  span.text = text
  span.style = style
  span.priority = priority
  span.selectable = selectable
  indicator_cache.spans[index] = span
  return span
end

local function finish_indicator()
  for index = indicator_cache.count + 1, #indicator_cache.spans do
    indicator_cache.spans[index] = nil
  end
  return indicator_cache.spans
end

local function indicator_spans(opts)
  local bar_style = (opts and opts.bar_style) or DEFAULT_BAR_STYLE
  local pause_status = smelt.signal.get("auto_continue_status")
  if pause_status then
    local reason = pause_status.kind == "quota" and "quota exceeded"
      or pause_status.kind == "rate_limited" and "rate limited" or "interrupted"
    local action = "paused"
    if pause_status.phase == "waiting_for_idle" then
      action = "resuming when idle"
    elseif pause_status.phase == "scheduled" then
      local at = math.floor(pause_status.next_attempt_at_ms / 1000)
      local today = smelt.time.format(math.floor(smelt.time.now_ms() / 1000), "%Y-%m-%d")
      local format = smelt.time.format(at, "%Y-%m-%d") == today and "%H:%M" or "%b %d %H:%M"
      local time = smelt.time.format(at, format)
      if time then action = "resuming at " .. time end
    end
    return {
      { text = "─", style = bar_style, priority = INDICATOR_PRIORITY, selectable = false },
      { text = " " .. reason, style = COMMENT_DIM_STYLE, priority = INDICATOR_PRIORITY },
      { text = " ·", style = bar_style, priority = LABEL_PRIORITY, selectable = false },
      { text = " " .. action, style = COMMENT_DIM_STYLE, priority = LABEL_PRIORITY },
    }
  end
  local state = smelt.signal.get("work_state")
  if not state or state == "idle" then return nil end

  indicator_cache.count = 0
  local label = smelt.signal.get("work_label") or ""
  local elapsed_ms = smelt.signal.get("work_elapsed_ms") or 0
  local retry_attempt = smelt.signal.get("work_retry_attempt") or 0
  local retry_remaining_ms = smelt.signal.get("work_retry_remaining_ms") or 0

  if label == "" then
    if state == "done" then label = "done"
    elseif state == "interrupted" then label = "interrupted"
    elseif state == "paused" then label = "paused"
    end
  end

  local active = state == "working" or state == "retrying" or state == "busy"
  if active and label ~= "" then label = label .. "\u{2026}" end

  local glyph
  if active or state == "paused" then
    glyph = smelt.spinner.glyph()
  else
    glyph = ""
  end

  local wave_x = 0
  local wave_t, wave_low, wave_high
  if active then wave_t, wave_low, wave_high = smelt.spinner.wave_state() end
  local function add_wave_text(text, priority, selectable_start)
    for _, codepoint in utf8.codes(text) do
      local span = indicator_cache.pool[indicator_cache.count + 1]
      local style = span and span.wave_style
      if not style then
        style = { fg = { 0, 0, 0 }, bold = true }
        if not span then span = {} end
        span.wave_style = style
        indicator_cache.pool[indicator_cache.count + 1] = span
      end
      local level = smelt.spinner.wave_level_at(wave_x, wave_t, wave_low, wave_high)
      style.fg[1], style.fg[2], style.fg[3] = level, level, level
      indicator_span(
        utf8.char(codepoint),
        style,
        priority,
        selectable_start and wave_x >= selectable_start)
      wave_x = wave_x + 1
    end
  end

  -- Leading `─` so the indicator sits one cell in from the edge. It drops with
  -- the compact spinner so narrow bars fall back to a clean rule instead of an
  -- orphan edge cell.
  indicator_span("\u{2500}", bar_style, INDICATOR_PRIORITY, false)

  if active then
    if glyph ~= "" then add_wave_text(" " .. glyph, INDICATOR_PRIORITY, nil) end
    if label ~= "" then add_wave_text(" " .. label, LABEL_PRIORITY, 3) end
  else
    local style = NORMAL_BOLD_STYLE
    if state == "paused" or state == "done" or state == "interrupted" then
      style = COMMENT_DIM_STYLE
    end
    if glyph ~= "" then
      indicator_span(" " .. glyph, style, INDICATOR_PRIORITY, false)
    end
    if label ~= "" then
      indicator_span(" " .. label, style, LABEL_PRIORITY, nil)
    end
  end

  -- Duration suppressed for `interrupted` (label alone reads cleaner).
  local secs = math.floor(elapsed_ms / 1000)
  if state ~= "interrupted" and secs > 0 then
    indicator_span(
      " " .. smelt.text.format_duration(secs),
      COMMENT_DIM_STYLE,
      SECONDARY_PRIORITY,
      nil)
  end

  if retry_attempt > 0 then
    local retry_secs = math.max(1, math.ceil(retry_remaining_ms / 1000))
    indicator_span(
      string.format(" (retrying in %ds #%d)", retry_secs, retry_attempt),
      COMMENT_DIM_STYLE,
      OPTIONAL_PRIORITY,
      nil)
  end

  return finish_indicator()
end

-- ── right-side spans (model + reasoning + tokens + cost) ─────────────

local function reasoning_color_group(effort)
  if effort == "low" then return "SmeltReasonLow"
  elseif effort == "medium" then return "SmeltReasonMedium"
  elseif effort == "high" then return "SmeltReasonHigh"
  elseif effort == "xhigh" or effort == "max" then return "SmeltReasonMax"
  elseif effort == "ultra" then return "SmeltReasonUltra"
  else return "Comment"
  end
end

local function right_spans(opts)
  local bar_style = (opts and opts.bar_style) or DEFAULT_BAR_STYLE
  local spans = {}
  local status = smelt.session.status and smelt.session.status() or {}
  local model = status.model or smelt.model.current()
  if model and model ~= "" then
    local fast_active = status.fast and status.fast.active
    if fast_active then
      spans[#spans + 1] = {
        text = " >>",
        style = { fg = "Comment", bold = true },
        priority = SECONDARY_PRIORITY,
      }
    end
    spans[#spans + 1] = {
      text = " " .. model,
      style = { fg = "Comment" },
      priority = SECONDARY_PRIORITY,
    }
    local reasoning = status.reasoning or {}
    local effort = reasoning.effort or smelt.reasoning.current()
    if effort and effort ~= "off" then
      spans[#spans + 1] = {
        text = " " .. effort .. (reasoning.marker or ""),
        style = { fg = reasoning_color_group(effort) },
        priority = SECONDARY_PRIORITY,
      }
    end
  end

  if smelt.settings.show_tokens then
    local context = status.context or {}
    local ctx = context.tokens
    if ctx == nil then
      ctx = smelt.session.context_tokens()
    end
    if ctx then
      if #spans > 0 then
        spans[#spans + 1] = {
          text = " ·",
          style = bar_style,
          priority = SECONDARY_PRIORITY,
          selectable = false,
        }
      end
      local window = context.window or smelt.session.context_window()
      local stale_mark = context.marker or ""
      local tok_text
      if window and window > 0 then
        local pct = math.floor(ctx / window * 100)
        tok_text = string.format(" %s%s (%d%%)", smelt.text.format_tokens(ctx), stale_mark, pct)
      else
        tok_text = " " .. smelt.text.format_tokens(ctx) .. stale_mark
      end
      spans[#spans + 1] = {
        text = tok_text,
        style = { fg = "Comment" },
        priority = TOKEN_PRIORITY,
      }
    end
  end

  if smelt.settings.show_cost then
    local cost = smelt.session.cost()
    if cost and cost > 0 then
      if #spans > 0 then
        spans[#spans + 1] = {
          text = " ·",
          style = bar_style,
          priority = OPTIONAL_PRIORITY,
          selectable = false,
        }
      end
      spans[#spans + 1] = {
        text = " " .. smelt.text.format_cost(cost),
        style = { fg = "Comment" },
        priority = OPTIONAL_PRIORITY,
      }
    end
  end

  return spans
end

local function resize_bar_opts(position)
  if not smelt.signal.get("prompt_resize_active") then return nil end
  local chrome = smelt.signal.get("prompt_resize_chrome") or ""
  if chrome == position or chrome == "both" then
    return { bar_style = { hl_group = "SmeltResizeHandle" } }
  end
  return nil
end

-- ── renderers ───────────────────────────────────────────────────────

local function render_top(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  local rows = {}
  local queued = smelt.prompt.queued_rows()

  -- The layout composer already capped our window height; trim from the
  -- oldest queued messages inward when the queue does not all fit.
  local win_height = (win:rect() or {}).height
  -- During the very first frame a renderer can run before the window has
  -- a resolved rect; in that case use the natural row count as a fallback.
  local natural_rows = M.top_rows()
  local reserved = 1 -- indicator bar row
  if smelt.prompt.has_stash() then reserved = reserved + 1 end
  local queue_slots = math.max(0, (win_height or natural_rows) - reserved)
  local visible_queued = math.min(#queued, queue_slots)
  local hidden = #queued - visible_queued
  local show_more = hidden > 0 and queue_slots > 0
  if show_more then
    -- The summary row occupies one of the queue slots, so keep one fewer
    -- concrete queued message when collapsing. Otherwise the indicator bar is
    -- pushed out of the capped top window on short terminals.
    visible_queued = queue_slots - 1
    hidden = #queued - visible_queued
  end
  local start_idx = #queued - visible_queued + 1
  for i = start_idx, #queued do
    rows[#rows + 1] = queued_message_row(queued[i], width)
  end
  if show_more then
    rows[#rows + 1] = more_row(hidden, width)
  end
  if smelt.prompt.has_stash() then
    rows[#rows + 1] = stash_row(width)
  end
  local bar_opts = resize_bar_opts("top")
  rows[#rows + 1] = bar.compose(width, indicator_spans(bar_opts), right_spans(bar_opts), bar_opts)
  bar.write_rows(buf, rows, TOP_NS)
end

local function render_aux(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  local queued = smelt.prompt.queued_rows()
  local row = nil
  if should_show_tip(queued) then row = tip_row(width) end
  local blank = { text = string.rep(" ", math.max(width or 0, 0)), highlights = {} }
  bar.write_rows(buf, { row or blank }, AUX_NS)
end

local function render_bottom(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  bar.write_rows(buf, { bar.compose(width, nil, nil, resize_bar_opts("bottom")) }, BOT_NS)
end

-- ── window allocation ───────────────────────────────────────────────

M.aux_win = smelt.win.new(smelt.buf.new({ name = "smelt.prompt_bar.aux" }), {
  name = "smelt.prompt_bar.aux",
  scrollbar = false,
  surface = "selectable_text",
  region = "prompt_aux",
})
M.top_win = smelt.win.new(smelt.buf.new({ name = "smelt.prompt_bar.top" }), {
  name = "smelt.prompt_bar.top",
  scrollbar = false,
  surface = "selectable_text",
  region = "prompt_above",
})
M.bottom_win = smelt.win.new(smelt.buf.new({ name = "smelt.prompt_bar.bottom" }), {
  name = "smelt.prompt_bar.bottom",
  scrollbar = false,
  surface = "inert",
  region = "prompt_below",
})

if M.aux_win then
  M.aux_win:set_renderer(render_aux)
end
if M.top_win then
  M.top_win:set_renderer(render_top)
end
if M.bottom_win then
  M.bottom_win:set_renderer(render_bottom)
end

invalidate_on(M.aux_win, {
  "input_epoch",
  "notification_visible",
  "prompt_queue_revision",
  "work_state",
})
invalidate_on(M.top_win, {
  "auto_continue_status",
  "fast_mode",
  "input_epoch",
  "model",
  "prompt_queue_revision",
  "prompt_resize_active",
  "prompt_resize_chrome",
  "reasoning",
  "session_epoch",
  "tokens_used",
  "work_elapsed_ms",
  "work_label",
  "work_retry_attempt",
  "work_state",
})
invalidate_when_changed(M.top_win, "work_retry_remaining_ms", function(value)
  return math.max(1, math.ceil((tonumber(value) or 0) / 1000))
end)
invalidate_on(M.bottom_win, {
  "prompt_resize_active",
  "prompt_resize_chrome",
})

-- Expose helper for the layout composer so it can compute the top bar's
-- row count from current state (queued messages, stash row, bar row).
-- `max_top_rows` is an optional cap; when omitted the natural row count is
-- returned. The default layout composer passes a cap that preserves at
-- least two transcript rows.
function M.top_rows(max_top_rows)
  local queued = smelt.prompt.queued()
  local rows = 1 + #queued
  if smelt.prompt.has_stash() then rows = rows + 1 end
  if type(max_top_rows) == "number" then
    return math.min(rows, math.max(1, max_top_rows))
  end
  return rows
end

if smelt.win and smelt.win.PROMPT then
  local previous_top_rows = M.top_rows()
  renderer_subscriptions[#renderer_subscriptions + 1] = smelt.win.PROMPT:on(
    "text_changed",
    function()
      if M.aux_win then M.aux_win:invalidate_renderer() end
      if M.top_win then M.top_win:invalidate_renderer() end
      local top_rows = M.top_rows()
      if top_rows ~= previous_top_rows then
        previous_top_rows = top_rows
        smelt.ui.layout.invalidate()
      end
    end)
end

return M
