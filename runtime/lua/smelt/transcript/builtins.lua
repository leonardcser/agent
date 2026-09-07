-- Built-in transcript group rules and renderers.

local defaults = require("smelt.transcript.defaults")
local layout = smelt.layout

local M = {}
local BUILTIN_GROUP_PRIORITY = -100
local GROUP_LIST_MAX = 5

local function failure_suffix(errors, denied)
  local parts = {}
  if errors > 0 then
    parts[#parts + 1] = tostring(errors) .. (errors == 1 and " error" or " errors")
  end
  if denied > 0 then
    parts[#parts + 1] = tostring(denied) .. " denied"
  end
  if #parts == 0 then return "" end
  return " (" .. table.concat(parts, ", ") .. ")"
end

local function child_status_hl(child)
  if defaults.child_failed(child) then return "ErrorMsg" end
  if child.status == "pending" or child.status == "drafting" then return "SmeltToolPending" end
  return nil
end

local function aggregate_status_hl(group)
  local has_pending = false
  local has_confirm = false
  for _, child in ipairs(defaults.group_children(group)) do
    if defaults.child_failed(child) then return "ErrorMsg" end
    has_pending = has_pending or child.status == "pending" or child.status == "drafting"
    has_confirm = has_confirm or child.status == "confirm"
  end
  if has_pending then return "SmeltToolPending" end
  if has_confirm then return "SmeltAccent" end
  return "SmeltSuccess"
end

local function process_summary_line(text, has_failure)
  return layout.runs({ { {
    text = text,
    fg = has_failure and "ErrorMsg" or "SmeltProcess",
    italic = true,
  } } })
end

local function tool_group_header(name, count, hl, suffix, called_at_ms, ctx)
  local header = layout.runs({ {
    { text = "*", hl = hl },
    { text = " " .. name, dim = true },
    { text = " ×" .. tostring(count), dim = true, selectable = false },
    { text = suffix or "", hl = suffix ~= "" and "ErrorMsg" or nil, selectable = false },
  } })
  local called_at, refresh_after = defaults.timestamp(called_at_ms, ctx and ctx.now_ms)
  if called_at then
    header = layout.hbox({
      { header, weight = 1, copy_owner = true },
      {
        layout.line({ { text = "  " .. called_at, dim = true, selectable = false } }),
        fit = true,
      },
    })
  end
  if refresh_after then header = layout.refresh(header, { after_ms = refresh_after }) end
  return header
end

local function latest_group_call_time(group)
  local latest
  for _, child in ipairs(defaults.group_children(group)) do
    local called_at_ms = tonumber(child.called_at_ms)
    if called_at_ms and (not latest or called_at_ms > latest) then latest = called_at_ms end
  end
  return latest
end

local function display_path(path)
  if type(path) ~= "string" or path == "" then return nil end
  if smelt.path and smelt.path.display then return smelt.path.display(path) end
  return path
end

local function read_file_label(child)
  local args = child.args or {}
  local content_lines = child.output and child.output.content_lines
  if smelt.tools and smelt.tools.read_file_summary and args.file_path then
    return smelt.tools.read_file_summary(args, content_lines)
  end
  return display_path(args.file_path) or child.summary_text or child.name or "read_file"
end

local function grep_label(child)
  local args = child.args or {}
  local pattern = args.pattern or child.summary_text or ""
  local label = pattern ~= "" and ('"' .. tostring(pattern) .. '"') or "grep"
  if args.path and args.path ~= "" then
    label = label .. " in " .. (display_path(args.path) or tostring(args.path))
  elseif args.glob and args.glob ~= "" then
    label = label .. " glob:" .. tostring(args.glob)
  elseif args.type and args.type ~= "" then
    label = label .. " type:" .. tostring(args.type)
  end
  return label
end

local function glob_label(child)
  local args = child.args or {}
  local label = args.pattern or child.summary_text or ""
  if args.path and args.path ~= "" then label = label .. " in " .. (display_path(args.path) or tostring(args.path)) end
  return label
end

local function tool_group_label(child)
  local label
  if child.name == "read_file" then
    label = read_file_label(child)
  elseif child.name == "grep" then
    label = grep_label(child)
  elseif child.name == "glob" then
    label = glob_label(child)
  else
    label = child.summary_text or ""
  end
  local name = child.name or "tool"
  if label == "" or label == name then return name end
  return name .. " " .. label
end

local function render_compact_group_list(group, label)
  local children = defaults.group_children(group)
  local max = math.min(#children, GROUP_LIST_MAX)
  local lines = {}
  local start = math.max(1, #children - max + 1)
  if start > 1 then
    lines[#lines + 1] = { { text = "… " .. tostring(start - 1) .. " above", dim = true, selectable = false } }
  end
  for i = start, #children do
    local child = children[i]
    local span = { text = tostring(label(child)), hl = child_status_hl(child) }
    lines[#lines + 1] = { span }
  end
  if #lines == 0 then return layout.empty() end
  return layout.gutter(layout.runs(lines), { text = "  " })
end

local function render_terminal_tool_group(group, ctx, opts)
  local count = group.child_count or #defaults.group_children(group)
  local errors, denied = defaults.group_failure_counts(group)
  local header = tool_group_header(
    opts.name,
    count,
    aggregate_status_hl(group),
    failure_suffix(errors, denied),
    latest_group_call_time(group),
    ctx
  )
  if group.view_state == "expanded" then
    return defaults.render_group_children(group, ctx)
  end
  return layout.vbox({
    header,
    render_compact_group_list(group, opts.label),
  })
end

local function process_exit_code(child)
  local code = child.exit_code
  if code == nil and type(child.event_data) == "table" then code = child.event_data.exit_code end
  return code
end

local function process_id(child)
  local id = child.process_id
  if id == nil and type(child.event_data) == "table" then id = child.event_data.process_id end
  return id
end

local function process_failed(child)
  local code = tonumber(process_exit_code(child))
  return code ~= nil and code ~= 0
end

local function process_status_fragment(child)
  local id = process_id(child)
  local subject = id and tostring(id) or "process"
  local code = tonumber(process_exit_code(child))
  if code == 0 then return subject .. " finished successfully" end
  if code ~= nil then return subject .. " exited with code " .. tostring(code) end
  return subject .. " exited"
end

local function failed_process_summary(failed)
  if #failed == 0 then return "" end
  if #failed == 1 then return ": " .. process_status_fragment(failed[1]) end

  local max = math.min(#failed, 2)
  local parts = {}
  for i = 1, max do
    parts[#parts + 1] = tostring(process_id(failed[i]) or process_status_fragment(failed[i]))
  end
  if #failed > max then parts[#parts + 1] = "+" .. tostring(#failed - max) .. " more" end
  return ": " .. table.concat(parts, ", ")
end

local function render_background_process_completed_group(group, ctx)
  if group.view_state == "expanded" then return defaults.render_group_children(group, ctx) end

  local children = defaults.group_children(group)
  local count = group.child_count or #children
  local failed = {}
  for _, child in ipairs(children) do
    if process_failed(child) then failed[#failed + 1] = child end
  end

  local text = "background processes finished: " .. tostring(count)
  if #failed > 0 then
    text = text .. ", " .. tostring(#failed) .. " failed" .. failed_process_summary(failed)
  end
  return process_summary_line(text, #failed > 0)
end

function M.render(group, ctx)
  local kind = group.group_kind or group.name
  if kind == "background_process_completed" then
    return render_background_process_completed_group(group, ctx)
  end
  if kind == "explore" or kind == "lsp" or kind == "web" then
    return render_terminal_tool_group(group, ctx, {
      name = kind,
      label = tool_group_label,
    })
  end
  if (ctx and ctx.view_state) == "expanded" or group.view_state == "expanded" then
    return defaults.render_group_children(group, ctx)
  end
  return tool_group_header(
    kind or "group",
    group.child_count or #defaults.group_children(group),
    aggregate_status_hl(group),
    "",
    latest_group_call_time(group),
    ctx
  )
end

function M.register()
  smelt.transcript.groups.register({
    name = "background_process_completed",
    cache_key = "smelt.transcript.group.background_process_completed:v2",
    priority = BUILTIN_GROUP_PRIORITY,
    min = 2,
    default_view = "collapsed",
    selector = { kind = "process_status", event = "background_process_completed" },
  })

  smelt.transcript.groups.register({
    name = "explore",
    cache_key = "smelt.transcript.group.explore:v2",
    priority = BUILTIN_GROUP_PRIORITY,
    min = 2,
    default_view = "collapsed",
    selector = {
      kind = "tool",
      names = {
        "read_file",
        "grep",
        "glob",
        "outline",
        "find_symbol",
      },
    },
  })

  smelt.transcript.groups.register({
    name = "lsp",
    cache_key = "smelt.transcript.group.lsp:v1",
    priority = BUILTIN_GROUP_PRIORITY,
    min = 2,
    default_view = "collapsed",
    selector = {
      kind = "tool",
      names = {
        "inspect_symbol",
        "inspect_symbol_at",
        "find_definition",
        "find_references",
        "diagnostics",
      },
    },
  })

  smelt.transcript.groups.register({
    name = "web",
    cache_key = "smelt.transcript.group.web:v1",
    priority = BUILTIN_GROUP_PRIORITY,
    min = 2,
    default_view = "collapsed",
    selector = {
      kind = "tool",
      names = {
        "web_search",
        "web_fetch",
      },
    },
  })
end

package.loaded["smelt.transcript.builtins"] = M

return M
