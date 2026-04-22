-- Built-in /ps command.
--
-- Lists background processes. Backspace kills the selected row via an
-- `on_press` callback; the callback sets a loop flag, closes the
-- dialog, and the plugin reopens with the refreshed list. Esc / Enter
-- close without looping. Pure Lua over `process.list()` +
-- `process.kill(id)` + `dialog.open` callback keymaps.

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

local function format_proc(p)
  return string.format("%s — %s %s", p.command, format_duration(p.elapsed_secs or 0), p.id)
end

smelt.api.cmd.register("ps", function()
  local procs = smelt.api.process.list()
  if #procs == 0 then
    smelt.api.ui.notify_error("no background processes")
    return
  end

  smelt.task(function()
    while true do
      procs = smelt.api.process.list()
      if #procs == 0 then
        return
      end

      local items = {}
      for _, p in ipairs(procs) do
        table.insert(items, { label = format_proc(p) })
      end

      local snapshot = procs
      local should_reopen = false

      smelt.api.dialog.open({
        title   = "processes",
        panels  = {
          { kind = "options", items = items },
        },
        keymaps = {
          { key = "bs", hint = "\u{232b}: kill selected", on_press = function(ctx)
              if ctx.selected_index then
                local target = snapshot[ctx.selected_index]
                if target then
                  smelt.api.process.kill(target.id)
                  should_reopen = true
                end
              end
              ctx.close()
            end },
        },
      })

      if not should_reopen then
        return
      end
    end
  end)
end, { desc = "manage background processes" })
