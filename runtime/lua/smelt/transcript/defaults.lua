smelt.transcript = smelt.transcript or {}
smelt.transcript.defaults = {}

local M = {}

local layout = smelt.layout
local is_layout_node = __smelt_internal.layout.__is_node

local preferences = smelt.settings and smelt.settings.transcript
if preferences and preferences.show_timestamps == nil then
  preferences.show_timestamps = true
end

local status_hl = {
  drafting = "SmeltToolPending",
  pending = "SmeltToolPending",
  ok = "SmeltSuccess",
  err = "ErrorMsg",
  denied = "ErrorMsg",
  confirm = "SmeltAccent",
}

local NO_PRESENTATION = {}

local function tool_presentation(block, presentation)
  if presentation ~= nil then return presentation end
  return smelt.transcript.get_tool_presentation(block.name or "") or NO_PRESENTATION
end

local function presentation_result_error(block, field, expected, value)
  error(
    "smelt.transcript tool presentation `" .. (block.name or "tool") .. "`." .. field
      .. " returned " .. type(value) .. "; expected " .. expected,
    3
  )
end

local function require_layout_result(block, field, value, allow_nil)
  if value == nil and allow_nil then return value end
  if not is_layout_node(value) then
    local expected = "smelt.layout.Node"
    if allow_nil then expected = expected .. " or nil" end
    presentation_result_error(block, field, expected, value)
  end
  return value
end

local title_result_expected = "string, styled-lines table, or nil"

local function require_title_result(block, value)
  if value == nil or type(value) == "string" then return value end
  if type(value) ~= "table" then
    presentation_result_error(block, "title", title_result_expected, value)
  end
  for _, line in ipairs(value) do
    if type(line) ~= "string" and type(line) ~= "table" then
      presentation_result_error(block, "title", title_result_expected, value)
    end
    if type(line) == "table" then
      for _, span in ipairs(line) do
        if type(span) ~= "string" and type(span) ~= "table" then
          presentation_result_error(block, "title", title_result_expected, value)
        end
        if type(span) == "table" then
          local text = span.text
          if text == nil then text = span[1] end
          if text ~= nil and type(text) ~= "string" then
            presentation_result_error(block, "title", title_result_expected, value)
          end
        end
      end
    end
  end
  return value
end

function M.display_count_text(block, opts)
  opts = opts or {}
  local output = block and block.output or {}
  local metadata = output.metadata or {}
  local display_count = metadata.display_count
  if type(display_count) ~= "table" then display_count = {} end
  local count = tonumber(display_count.value)
  if count == nil then count = tonumber(opts.count) or 0 end
  local unit = display_count.unit or opts.unit or "item"
  local plural = display_count.plural or opts.plural
  if plural == nil then
    if unit == "match" then
      plural = "matches"
    else
      plural = unit .. "s"
    end
  end
  return count .. " " .. (count == 1 and unit or plural)
end

function M.render_display_count(block, opts)
  return layout.text(M.display_count_text(block, opts))
end

--- Render any semantic transcript block with the bundled default policy.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render(block, ctx)
  block = block or {}
  if block.kind == "group" then
    return require("smelt.transcript.builtins").render(block, ctx)
  elseif block.kind == "tool" then
    return M.render_tool(block, ctx)
  elseif block.kind == "user" then
    return M.render_user(block, ctx)
  elseif block.kind == "assistant" then
    return M.render_assistant(block, ctx)
  elseif block.kind == "thinking" then
    return M.render_thinking(block, ctx)
  elseif block.kind == "exec" then
    return M.render_exec(block, ctx)
  elseif block.kind == "mode" then
    return M.render_mode(block, ctx)
  elseif block.kind == "process_status" then
    return M.render_process_status(block, ctx)
  elseif block.kind == "compacted" then
    return M.render_compacted(block, ctx)
  elseif block.kind == "compaction_preview" then
    return M.render_compaction_preview(block, ctx)
  elseif block.kind == "code" then
    return M.render_code(block, ctx)
  end
  return M.render_unknown(block, ctx)
end

--- Render a tool block for the current transcript view state.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_tool(block, ctx)
  local presentation = tool_presentation(block)
  if presentation.render then
    local rendered = presentation.render(block, ctx, presentation)
    return require_layout_result(block, "render", rendered, false)
  end
  if ctx and ctx.view_state == "collapsed" then
    return M.render_tool_summary(block, ctx, presentation)
  end
  return M.render_tool_full(block, ctx, presentation)
end

local function render_tool_error_summary(block, ctx, presentation)
  return layout.vbox({
    M.render_tool_header(block, ctx, nil, presentation),
    layout.gutter(
      M.render_tool_output_tail(block.output, ctx, {
        rows = (ctx and ctx.limits and ctx.limits.collapsed_error_rows) or 4,
        keep = "head",
        marker = "below",
      }),
      { text = "  " }
    ),
  })
end

--- Render a compact tool summary: header plus an optional detail line.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, presentation?: smelt.transcript.ToolPresentation): smelt.layout.Node
function M.render_tool_summary(block, ctx, presentation)
  presentation = tool_presentation(block, presentation)
  local output = block.output
  if not presentation.compact and output and output.is_error then
    return render_tool_error_summary(block, ctx, presentation)
  end

  local header = M.render_tool_header(block, ctx, nil, presentation)
  local detail = M.tool_collapsed_detail(block, ctx, presentation)
  if detail == nil or detail == "" then return header end
  local detail_layout = detail
  if type(detail) == "string" then
    detail_layout = layout.runs({ { { text = detail, dim = true } } })
  end
  return layout.vbox({
    header,
    layout.gutter(detail_layout, { text = "  " }),
  })
end

--- Render a full tool block using the current generic primitives and explicit item
--- construction.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, presentation?: smelt.transcript.ToolPresentation): smelt.layout.Node
function M.render_tool_full(block, ctx, presentation)
  presentation = tool_presentation(block, presentation)
  local items = {}
  items[#items + 1] = M.render_tool_header(block, ctx, nil, presentation)

  if block.user_message and block.user_message ~= "" then
    items[#items + 1] = layout.gutter(
      layout.text(block.user_message),
      { text = "  " }
    )
  end

  local body = M.render_tool_body(block, ctx, nil, presentation)
  if body then items[#items + 1] = body end

  return layout.vbox(items)
end

local tool_header_lines
local tool_header_rest_prefix
local tool_header_prefix

local months = { "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec" }

local function boundary_after_ms(now_ms, year_only)
  local now_seconds = math.floor(now_ms / 1000)
  local current = os.date("*t", now_seconds)
  if type(current) ~= "table" then return nil end
  local boundary
  if year_only then
    boundary = os.time({ year = current.year + 1, month = 1, day = 1, hour = 0 })
  else
    boundary = os.time({ year = current.year, month = current.month, day = current.day + 1, hour = 0 })
  end
  if not boundary or boundary <= now_seconds then return nil end
  return math.max(1, math.floor(boundary * 1000 - now_ms))
end

--- Return the visible timestamp and its next adaptive-date refresh delay.
--- Visibility and formatting are shared by user messages and tool headers.
function M.timestamp(timestamp_ms, now_ms)
  local settings = smelt.settings and smelt.settings.transcript
  if settings and settings.show_timestamps == false then return nil, nil end

  timestamp_ms = tonumber(timestamp_ms)
  local current_ms = tonumber(now_ms)
  if not timestamp_ms or not current_ms then return nil, nil end
  local timestamp = math.floor(timestamp_ms / 1000)
  local now = math.floor(current_ms / 1000)

  local day = smelt.time.format(timestamp, "%Y-%m-%d")
  local current_day = smelt.time.format(now, "%Y-%m-%d")
  if day == current_day then
    return smelt.time.format(timestamp, "%H:%M:%S"), boundary_after_ms(current_ms, false)
  end

  local month = tonumber(smelt.time.format(timestamp, "%m"))
  local day_and_time = smelt.time.format(timestamp, "%d %H:%M:%S")
  if not month or not day_and_time then return nil, nil end
  local date_and_time = months[month] .. " " .. day_and_time
  if smelt.time.format(timestamp, "%Y") == smelt.time.format(now, "%Y") then
    return date_and_time, boundary_after_ms(current_ms, true)
  end
  return smelt.time.format(timestamp, "%Y") .. " " .. date_and_time, nil
end

local function elapsed_text(elapsed_ms)
  local ms = tonumber(elapsed_ms)
  if not ms or ms < 1000 then return nil end
  if ms < 10000 then
    return string.format("%.1fs", ms / 1000)
  end
  local seconds = math.floor(ms / 1000)
  if seconds < 60 then return tostring(seconds) .. "s" end
  if seconds < 3600 then return tostring(math.floor(seconds / 60)) .. "m" .. tostring(seconds % 60) .. "s" end
  return tostring(math.floor(seconds / 3600)) .. "h" .. tostring(math.floor((seconds % 3600) / 60)) .. "m"
end

--- Render the default one-line tool header.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts?: smelt.transcript.ToolHeaderOptions, presentation?: smelt.transcript.ToolPresentation): smelt.layout.Node
function M.render_tool_header(block, ctx, opts, presentation)
  ctx = ctx or {}
  opts = opts or {}
  presentation = tool_presentation(block, presentation)
  local status = block.status or "pending"
  local hl = opts.hl or status_hl[status]
  local lines, tail, has_summary = tool_header_lines(block, status, ctx, presentation)
  local duration = status ~= "confirm" and elapsed_text(block.elapsed_ms) or nil
  if duration then
    local first = lines[1] or {}
    first[#first + 1] = { text = "  " .. duration, selectable = false, dim = true }
    lines[1] = first
  end

  local header = layout.runs(lines)
  local called_at, called_at_refresh = M.timestamp(block.called_at_ms, ctx.now_ms)
  if called_at then
    header = layout.hbox({
      { header, weight = 1, copy_owner = true },
      {
        layout.line({ { text = "  " .. called_at, selectable = false, dim = true } }),
        fit = true,
      },
    })
  end
  header = layout.row_prefix(
    layout.cap(header, {
      rows = (ctx.limits and ctx.limits.tool_header_rows) or 20,
      keep = "head",
      marker = "below",
    }),
    {
      first = tool_header_prefix(block, hl, tail, has_summary),
      rest = tool_header_rest_prefix(block),
    }
  )

  local refresh_after = called_at_refresh
  if block.elapsed_active and status ~= "confirm" then
    refresh_after = refresh_after and math.min(refresh_after, 100) or 100
  end
  if refresh_after then header = layout.refresh(header, { after_ms = refresh_after }) end
  return header
end

local function copy_table(t)
  local out = {}
  for k, v in pairs(t) do
    if type(v) == "table" then
      out[k] = copy_table(v)
    else
      out[k] = v
    end
  end
  return out
end

local function copy_span(span)
  if type(span) == "string" then return { text = span } end
  if type(span) ~= "table" then return { text = tostring(span or "") } end
  return copy_table(span)
end

local function span_text(span)
  if type(span) == "string" then return span end
  if type(span) ~= "table" then return tostring(span or "") end
  return span.text or span[1] or ""
end

local function has_text(spans)
  for _, span in ipairs(spans) do
    if span_text(span) ~= "" then return true end
  end
  return false
end

local function summary_lines(summary)
  if summary == nil then return {} end
  if type(summary) == "string" then
    if summary == "" then return {} end
    local out = {}
    for line in (summary .. "\n"):gmatch("([^\n]*)\n") do
      out[#out + 1] = { { text = line } }
    end
    return out
  end
  if type(summary) ~= "table" then return {} end

  local out = {}
  for _, line in ipairs(summary) do
    local spans = {}
    if type(line) == "string" then
      spans[#spans + 1] = { text = line }
    elseif type(line) == "table" then
      for _, span in ipairs(line) do
        spans[#spans + 1] = copy_span(span)
      end
    end
    out[#out + 1] = spans
  end
  return out
end

local function display_width(s)
  if smelt.text and smelt.text.width then
    local ok, width = pcall(smelt.text.width, s)
    if ok and type(width) == "number" then return width end
  end
  return #s
end

function tool_header_prefix(block, hl, tail, has_summary)
  local tool_name = block.name or "tool"
  local spans = {
    { text = "*", hl = hl },
    { text = " " .. tool_name, dim = true },
  }
  if tail then spans[#spans + 1] = { text = " ", selectable = has_summary } end
  return spans, display_width("* " .. tool_name .. " ")
end

function tool_header_rest_prefix(block)
  local _, width = tool_header_prefix(block, nil, true, false)
  return { { text = string.rep(" ", width), selectable = false, dim = true } }
end

function tool_header_lines(block, status, ctx, presentation)
  local title = block.summary
  if presentation.title then
    title = require_title_result(block, presentation.title(block, ctx))
    if title == nil then title = block.summary end
  end
  local lines = summary_lines(title)
  local suffix = {}

  if lines[1] then
    local first = lines[1]
    local trailing = {}
    while #first > 0 and first[#first].title_suffix do
      local span = table.remove(first)
      local text = span_text(span):gsub("^%s+", ""):gsub("%s+$", "")
      if status == "pending" and text ~= "" then
        span.text = text
        span[1] = nil
        trailing[#trailing + 1] = span
      end
    end
    for i = #trailing, 1, -1 do
      suffix[#suffix + 1] = trailing[i]
    end
  end

  if #lines == 0 then lines[1] = {} end
  local first = lines[1]
  local has_summary = has_text(first)
  local tail = has_summary or #suffix > 0

  if #suffix > 0 then
    if has_summary then
      first[#first + 1] = { text = "  ", selectable = false, dim = true }
    end
    for i, span in ipairs(suffix) do
      if i > 1 then first[#first + 1] = { text = " ", selectable = false, dim = true } end
      local copy = copy_span(span)
      copy.selectable = false
      if copy.dim == nil then copy.dim = true end
      first[#first + 1] = copy
    end
  end

  return lines, tail, has_summary
end

--- Return a compact tool detail for collapsed tool blocks.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, presentation?: smelt.transcript.ToolPresentation): string|smelt.layout.Node|nil
function M.tool_collapsed_detail(block, ctx, presentation)
  presentation = tool_presentation(block, presentation)
  if not presentation.compact then return nil end
  local detail = presentation.compact(block, ctx)
  if detail ~= nil and type(detail) ~= "string" and not is_layout_node(detail) then
    presentation_result_error(block, "compact", "string, smelt.layout.Node, or nil", detail)
  end
  return detail
end

--- Render a tool body. Drafts can provide an explicit best-effort preview;
--- completed tools fall back to raw output or their structured body renderer.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context, opts?: smelt.transcript.ToolBodyOptions, presentation?: smelt.transcript.ToolPresentation): smelt.layout.Node|nil
function M.render_tool_body(block, ctx, opts, presentation)
  opts = opts or {}
  presentation = tool_presentation(block, presentation)
  local renderer = presentation.body
  local draft_renderer = presentation.draft
  if block.draft and draft_renderer then
    local body = require_layout_result(block, "draft", draft_renderer(block, ctx, opts), true)
    if body == nil then return nil end
    return layout.gutter(body, { text = opts.gutter or "  " })
  end

  local output = block.output or block.preview_output or { content_preview = "", is_error = false }
  if renderer then
    local render_block = block
    if not block.output then
      render_block = setmetatable({ output = output }, { __index = block })
    end
    local body = require_layout_result(
      block,
      "body",
      renderer(render_block, ctx, opts),
      true
    )
    if body == nil then return nil end
    return layout.gutter(body, { text = opts.gutter or "  " })
  end

  if block.status == "denied" or not block.output then return nil end
  return M.render_tool_output(block.output, ctx, opts)
end

local function output_total_rows(output)
  local metadata = output and output.metadata
  local display_count = type(metadata) == "table" and metadata.display_count or nil
  if type(display_count) ~= "table" then return nil end
  local unit = display_count.unit
  if unit ~= "line" and unit ~= "lines" then return nil end
  return tonumber(display_count.value)
end

local function output_syntax(output)
  local metadata = output and output.metadata
  if type(metadata) ~= "table" then return nil end
  local syntax = metadata.syntax or metadata.lang or metadata.language
  if type(syntax) ~= "string" or syntax == "" then return nil end
  return syntax
end

--- Render raw tool output without gutter using generic layout primitives: text/runs and
--- a rendered-row cap. Body renderers use this for expanded/tail previews.
---@type fun(output: smelt.transcript.ToolOutput?, ctx: smelt.transcript.Context?, opts: table?): smelt.layout.Node
function M.render_tool_output_tail(output, ctx, opts)
  opts = opts or {}
  ctx = ctx or {}
  local limits = ctx.limits or {}
  local is_error = output and output.is_error == true
  local rows = opts.rows or limits.tool_output_rows or 20
  local hl = opts.hl or opts.hl_group
  if not hl and is_error then hl = "ErrorMsg" end

  local body
  if output and output.content_id then
    body = layout.content(output.content_id, {
      format = output_syntax(output) and not hl and "code" or "text",
      lang = output_syntax(output),
      hl_group = hl,
      ansi = true,
    })
  else
    body = layout.text((output and output.content_preview) or "", {
      hl_group = hl,
      ansi = true,
    })
  end

  return layout.cap(
    body,
    {
      rows = rows,
      keep = opts.keep or "tail",
      marker = opts.marker or "above",
      total_rows = opts.total_rows or output_total_rows(output) or (output and output.content_lines),
    }
  )
end

--- Render raw tool output using generic layout primitives: text, gutter, and a
--- rendered-row cap. Error output uses `ErrorMsg`; success output remains dimmed
--- by the text primitive's no-highlight fallback.
---@type fun(output: smelt.transcript.ToolOutput?, ctx: smelt.transcript.Context?, opts: table?): smelt.layout.Node
function M.render_tool_output(output, ctx, opts)
  opts = opts or {}
  return layout.gutter(
    M.render_tool_output_tail(output, ctx, opts),
    { text = opts.gutter or "  " }
  )
end

--- Render a user panel with a timestamp in its top padding row.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_user(block, ctx)
  local text = M.render_user_text(block, ctx)
  local timestamp, refresh_after = M.timestamp(block.sent_at_ms, ctx and ctx.now_ms)
  if not timestamp then
    return layout.panel(text, { hl = "SmeltUserBg", padding = 1 })
  end

  local empty = layout.line({})
  local header = layout.hbox({
    { empty, weight = 1 },
    {
      layout.line({ { text = timestamp, hl = "SmeltUserTimestamp", selectable = false } }),
      fit = true,
      copy_owner = true,
    },
  })
  local panel = layout.panel(layout.hbox({
    { empty, cols = 1 },
    { layout.vbox({ header, text, empty }), weight = 1, copy_owner = true },
    { empty, cols = 1 },
  }), { hl = "SmeltUserBg", padding = 0 })
  if refresh_after then panel = layout.refresh(panel, { after_ms = refresh_after }) end
  return panel
end

--- Render user text.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_user_text(block, ctx)
  local _ = ctx
  return layout.runs(block.user_lines or block.text or "")
end

--- Render LLM-authored Markdown with the shared transcript Markdown path.
---@type fun(content: string?, opts: table?): smelt.layout.Node
function M.render_llm_markdown(content, opts)
  return layout.markdown(content or "", opts or {})
end

--- Render capped LLM-authored Markdown for tool bodies and other long outputs.
---@type fun(content: string?, ctx: smelt.transcript.Context?, opts: table?): smelt.layout.Node
function M.render_llm_markdown_tail(content, ctx, opts)
  opts = opts or {}
  ctx = ctx or {}
  local limits = ctx.limits or {}
  local markdown_opts = opts.markdown or {
    dim = opts.dim,
    italic = opts.italic,
    hl_group = opts.hl or opts.hl_group,
  }
  return layout.cap(
    M.render_llm_markdown(content, markdown_opts),
    {
      rows = opts.rows or limits.tool_output_rows or 20,
      keep = opts.keep or "tail",
      marker = opts.marker or "above",
      total_rows = opts.total_rows,
    }
  )
end

--- Render assistant text.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_assistant(block, ctx)
  local _ = ctx
  if block.content_id then
    return layout.content(block.content_id, { format = "markdown" })
  end
  return M.render_llm_markdown(block.content_preview or "")
end

--- Render thinking for the current transcript view state.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_thinking(block, ctx)
  if ctx and ctx.view_state == "collapsed" then
    return M.render_thinking_summary(block, ctx)
  elseif ctx and ctx.view_state == "peek" then
    return M.render_thinking_peek(block, ctx)
  end
  return M.render_thinking_full(block, ctx)
end

local function thinking_content_layout(block, include_summary_history)
  local items = {}
  local titles = include_summary_history and block.summary_titles or nil
  if titles and #titles > 0 then
    for _, title in ipairs(titles) do
      items[#items + 1] = layout.markdown("**" .. title .. "**")
    end
  elseif block.title and block.title ~= "" then
    items[#items + 1] = layout.markdown("**" .. block.title .. "**")
  end
  if block.content_id then
    items[#items + 1] = layout.content(block.content_id, { format = "markdown" })
  elseif block.content_preview and block.content_preview ~= "" then
    items[#items + 1] = M.render_llm_markdown(block.content_preview)
  end
  if #items == 0 then
    items[1] = layout.markdown(block.thinking_summary or "thinking (0 lines)", { inline = true })
  end
  return layout.style(layout.vbox(items), {
    dim = true,
    italic = true,
  })
end

--- Render the full thinking block with the current gutter.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_thinking_full(block, ctx)
  local _ = ctx
  return layout.gutter(
    thinking_content_layout(block, true),
    { text = "│ ", styled = true }
  )
end

--- Render a compact live preview of thinking: first rendered row, omitted rows, tail.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_thinking_peek(block, ctx)
  return layout.gutter(
    layout.cap(thinking_content_layout(block, false), {
      rows = (ctx and ctx.limits and ctx.limits.thinking_peek_rows) or 4,
      keep = "head_tail",
      head_rows = (ctx and ctx.limits and ctx.limits.thinking_peek_head_rows) or 1,
      marker = "middle",
      total_rows = block.content_lines,
    }),
    { text = "│ ", styled = true }
  )
end

--- Render a compact thinking summary.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_thinking_summary(block, ctx)
  local _ = ctx
  return layout.gutter(
    layout.style(
      layout.markdown(block.thinking_summary or "thinking (0 lines)", {
        inline = true,
      }),
      {
        dim = true,
        italic = true,
      }
    ),
    { text = "│ ", styled = true }
  )
end

--- Render an exec block.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_exec(block, ctx)
  ctx = ctx or {}
  local items = {}
  local command_spans = block.command_spans or {
    { text = "!", fg = "SmeltExecPrefix", bold = true },
    { text = block.command or "", bold = true },
  }
  items[#items + 1] = layout.panel(
    layout.runs({ command_spans }),
    { hl = "SmeltUserBg", padding = 1 }
  )
  if block.output_id then
    local limits = ctx.limits or {}
    items[#items + 1] = layout.gutter(
      layout.cap(
        layout.content(block.output_id, { format = "text", ansi = true }),
        {
          rows = limits.tool_output_rows or 20,
          keep = "tail",
          marker = "above",
          total_rows = block.output_lines,
        }
      ),
      { text = "  ", styled = true }
    )
  end
  return layout.vbox(items)
end

--- Render a mode note.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_mode(block, ctx)
  local _ = ctx
  local hl = block.hl_group or "SmeltModeDefault"
  return layout.line({
    { text = block.icon or "", fg = hl },
    { text = block.text or "", fg = hl, italic = true },
  })
end

--- Render a process-status note.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_process_status(block, ctx)
  local _ = ctx
  local hl = block.hl_group or "SmeltProcess"
  return layout.runs({ {
    { text = block.text or "", fg = hl, italic = true },
  } })
end

local function render_compaction_summary(label, block, ctx)
  local separator = layout.separator({ label = label, dim = true, selectable = true })
  if ctx and ctx.view_state == "collapsed" then return separator end

  local items = { separator }
  if block.summary and block.summary ~= "" then
    local summary = layout.markdown(block.summary, { dim = true })
    if ctx and ctx.view_state == "peek" then
      summary = layout.cap(summary, {
        rows = (ctx and ctx.limits and ctx.limits.compacted_peek_rows) or 4,
        keep = "tail",
        marker = "above",
      })
    end
    items[#items + 1] = summary
  end
  return layout.vbox(items)
end

--- Render a compacted-history marker.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_compacted(block, ctx)
  return render_compaction_summary(" compacted ", block, ctx)
end

--- Render an in-flight compaction summary preview.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_compaction_preview(block, ctx)
  return render_compaction_summary(" compacting ", block, ctx)
end

--- Render a code block with syntax highlighting.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_code(block, ctx)
  local _ = ctx
  return layout.code(block.content or "", { lang = block.lang or "" })
end

--- Render unknown block kinds without failing the transcript.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_unknown(block, ctx)
  local _ = ctx
  return layout.text(block.content or block.text or block.summary or "")
end

--- Semantic transcript group snapshot passed to default renderer helpers.
---@class smelt.transcript.Group
---@field id? integer Stable render-plan group id.
---@field name? string Group spec name.
---@field title? string Optional display title.
---@field view_state? string Current group view state.
---@field children? smelt.transcript.GroupChild[] Ordered bounded child presentation metadata.

--- Return bounded child presentation metadata for a transcript group.
---@type fun(group: smelt.transcript.Group): smelt.transcript.GroupChild[]
function M.group_children(group)
  return group.children or {}
end

--- True when a grouped child represents a failed or denied tool result.
---@type fun(child: smelt.transcript.GroupChild): boolean
function M.child_failed(child)
  return child.status == "err"
    or child.status == "denied"
    or (child.output and child.output.is_error == true)
end

--- Count failed and denied tool children in a transcript group.
---@type fun(group: smelt.transcript.Group): integer, integer
function M.group_failure_counts(group)
  local errors = 0
  local denied = 0
  for _, child in ipairs(M.group_children(group)) do
    if child.status == "denied" then
      denied = denied + 1
    elseif M.child_failed(child) then
      errors = errors + 1
    end
  end
  return errors, denied
end

--- Compose independently retained child layouts for an expanded group.
---@type fun(group: smelt.transcript.Group, ctx: smelt.transcript.Context): smelt.layout.Node
function M.render_group_children(group, ctx)
  local _ = group
  local _ctx = ctx
  return layout.group_children()
end

--- Render any semantic transcript block with the bundled default policy.
---@type fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node
function smelt.transcript.defaults.render(block, ctx)
  return M.render(block, ctx)
end

--- Render raw tool output with the default gutter and rendered-row cap.
---@type fun(output: smelt.transcript.ToolOutput?, ctx: smelt.transcript.Context?, opts: table?): smelt.layout.Node
function smelt.transcript.defaults.render_tool_output(output, ctx, opts)
  return M.render_tool_output(output, ctx, opts)
end

package.loaded["smelt.transcript.defaults"] = M

return M
