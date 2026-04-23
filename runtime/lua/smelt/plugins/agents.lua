-- Built-in /agents command.
--
-- Two views stitched together with a single `smelt.spawn`:
--   1. List view  — one row per subagent with live status, tokens, cost.
--                   Backspace kills the selected agent; Enter opens its
--                   detail view.
--   2. Detail view — prompt + tool-call log for one agent, live-updated
--                    via `on_tick` + `smelt.agent.snapshots`.
-- Dismissing the detail view navigates back to the list.

local function format_duration(secs)
  if secs < 60 then
    return string.format("%ds", secs)
  elseif secs < 3600 then
    return string.format("%dm %ds", secs // 60, secs % 60)
  else
    local h = secs // 3600
    local rest = secs % 3600
    return string.format("%dh %dm %ds", h, rest // 60, rest % 60)
  end
end

local function format_tokens(n)
  if n >= 1000000 then
    return string.format("%.1fm", n / 1000000)
  elseif n >= 1000 then
    return string.format("%.1fk", n / 1000)
  else
    return string.format("%d", n)
  end
end

local function format_cost(usd)
  if usd < 0.01 then
    return string.format("$%.4f", usd)
  elseif usd < 1.0 then
    return string.format("$%.3f", usd)
  else
    return string.format("$%.2f", usd)
  end
end

local function find_snapshot(snapshots, agent_id)
  for _, s in ipairs(snapshots) do
    if s.agent_id == agent_id then return s end
  end
  return nil
end

-- Render the agent registry into `list_buf`.
local function refresh_list(list_buf, agents, snapshots)
  if #agents == 0 then
    smelt.buf.set_lines(list_buf, { "  No subagents running" })
    smelt.buf.add_dim(list_buf, 1, 0, #"  No subagents running")
    return
  end

  local name_w = 0
  for _, a in ipairs(agents) do
    if #a.agent_id > name_w then name_w = #a.agent_id end
  end

  local lines = {}
  local status_spans = {}
  for _, a in ipairs(agents) do
    local status_str = (a.status == "working") and "working" or "idle   "
    local name = a.agent_id .. string.rep(" ", name_w - #a.agent_id)
    local line = string.format("  %s  %s", name, status_str)
    local status_start = 2 + name_w + 2
    local status_end = status_start + #status_str
    table.insert(status_spans, { status_start, status_end })
    if a.task_slug and a.task_slug ~= "" then
      line = line .. "  " .. a.task_slug
    end
    local snap = find_snapshot(snapshots, a.agent_id)
    if snap then
      if snap.context_tokens and snap.context_tokens > 0 then
        line = line .. "  " .. format_tokens(snap.context_tokens)
      end
      if snap.cost_usd and snap.cost_usd > 0 then
        line = line .. "  " .. format_cost(snap.cost_usd)
      end
    end
    table.insert(lines, line)
  end
  smelt.buf.set_lines(list_buf, lines)
  for i, span in ipairs(status_spans) do
    smelt.buf.add_dim(list_buf, i, span[1], span[2])
  end
end

-- Compare two agent lists for the fields that drive the rendered row.
-- Tick callbacks refresh the buffer only when something visible changed.
local function agents_changed(a, b)
  if #a ~= #b then return true end
  for i = 1, #a do
    local x, y = a[i], b[i]
    if x.agent_id ~= y.agent_id
        or x.status ~= y.status
        or x.task_slug ~= y.task_slug
        or x.pid ~= y.pid then
      return true
    end
  end
  return false
end

local function refresh_detail_title(title_buf, agent_id)
  local entry
  for _, a in ipairs(smelt.agent.list()) do
    if a.agent_id == agent_id then entry = a; break end
  end
  local line = " " .. agent_id
  local id_end = #line
  if entry then
    if entry.status == "idle" then
      line = line .. " \u{2713}"
    end
    if entry.task_slug and entry.task_slug ~= "" then
      line = line .. " \u{00b7} " .. entry.task_slug
    end
  end
  local snap = find_snapshot(smelt.agent.snapshots(), agent_id)
  if snap then
    if snap.context_tokens and snap.context_tokens > 0 then
      line = line .. "  " .. format_tokens(snap.context_tokens)
    end
    if snap.cost_usd and snap.cost_usd > 0 then
      line = line .. "  " .. format_cost(snap.cost_usd)
    end
  end
  smelt.buf.set_lines(title_buf, { line, "" })
  smelt.buf.add_highlight(title_buf, 1, 1, id_end, { fg = "agent", bold = true })
end

local function split_lines(s)
  local out = {}
  for line in (s .. "\n"):gmatch("(.-)\n") do
    table.insert(out, line)
  end
  while #out > 0 and out[#out] == "" do
    table.remove(out)
  end
  return out
end

local function refresh_detail_body(detail_buf, agent_id)
  local snap = find_snapshot(smelt.agent.snapshots(), agent_id)
  if not snap then
    smelt.buf.set_lines(detail_buf, { "(agent not tracked)" })
    return
  end

  local lines = { "Prompt:" }
  local dim_lines = { 1 }
  for _, raw in ipairs(split_lines(snap.prompt or "")) do
    table.insert(lines, " " .. raw)
  end
  table.insert(lines, "")
  if not snap.tool_calls or #snap.tool_calls == 0 then
    table.insert(lines, "(no tool calls yet)")
  else
    for _, entry in ipairs(snap.tool_calls) do
      local elapsed = ""
      if entry.elapsed_ms and entry.elapsed_ms >= 100 then
        elapsed = "  " .. format_duration(math.floor(entry.elapsed_ms / 1000))
      end
      table.insert(lines, string.format("%s %s%s", entry.tool_name, entry.summary, elapsed))
    end
  end
  smelt.buf.set_lines(detail_buf, lines)
  for _, i in ipairs(dim_lines) do
    local line = lines[i]
    if line then
      smelt.buf.add_dim(detail_buf, i, 0, #line)
    end
  end
end

local function open_detail(agent_id)
  local title_buf = smelt.buf.create()
  local detail_buf = smelt.buf.create()
  refresh_detail_title(title_buf, agent_id)
  refresh_detail_body(detail_buf, agent_id)

  local result = smelt.ui.dialog.open({
    panels = {
      { kind = "content", buf = title_buf, height = 2 },
      { kind = "content", buf = detail_buf, height = "fill",
        focusable = true, pad_left = 2 },
    },
    on_tick = function(ctx)
      refresh_detail_title(title_buf, agent_id)
      refresh_detail_body(detail_buf, agent_id)
    end,
  })
  return result
end

smelt.cmd.register("agents", function()
  smelt.spawn(function()
    while true do
      local agents = smelt.agent.list()
      if #agents == 0 then
        smelt.notify_error("no subagents running")
        return
      end

      local list_buf = smelt.buf.create()
      refresh_list(list_buf, agents, smelt.agent.snapshots())
      local title_buf = smelt.buf.create()
      smelt.buf.set_lines(title_buf, { "agents", "" })
      smelt.buf.add_dim(title_buf, 1, 0, #"agents")

      local result = smelt.ui.dialog.open({
        panels = {
          { kind = "content", buf = title_buf, height = 2 },
          { kind = "list", buf = list_buf, height = "fill" },
        },
        keymaps = {
          { key = "bs", hint = "\u{232b}: kill", on_press = function(ctx)
              local idx = ctx.selected_index
              if idx and agents[idx] then
                smelt.agent.kill(agents[idx].pid)
                agents = smelt.agent.list()
                refresh_list(list_buf, agents, smelt.agent.snapshots())
              end
            end },
        },
        on_tick = function(ctx)
          local fresh = smelt.agent.list()
          if agents_changed(fresh, agents) then
            agents = fresh
            refresh_list(list_buf, agents, smelt.agent.snapshots())
          end
        end,
      })

      if result.action == "dismiss" then return end
      local idx = result.option_index
      if not (idx and agents[idx]) then return end
      local detail = open_detail(agents[idx].agent_id)
      -- Detail dismissed → loop back and reopen the list view with
      -- refreshed data. Any other resolution falls out of the command.
      if detail.action ~= "dismiss" then return end
    end
  end)
end, { desc = "manage running agents" })
