-- Built-in /help command.
--
-- Scrollable dialog listing every registered keybinding in two columns
-- (label · detail). Content is generated on-demand from
-- `smelt.keymap.help()` which reflects the active vim
-- setting.

local function build_lines(sections)
  local max_label = 0
  for _, section in ipairs(sections) do
    for _, entry in ipairs(section.entries) do
      if #entry.label > max_label then
        max_label = #entry.label
      end
    end
  end
  local label_col = max_label + 4

  local lines = {}
  for si, section in ipairs(sections) do
    for _, entry in ipairs(section.entries) do
      local padding = string.rep(" ", math.max(0, label_col - #entry.label))
      table.insert(lines, entry.label .. padding .. entry.detail)
    end
    if si < #sections then
      table.insert(lines, "")
    end
  end
  return lines
end

smelt.cmd.register("help", function()
  smelt.spawn(function()
    local sections = smelt.keymap.help()
    local lines = build_lines(sections)
    smelt.ui.dialog.open({
      title   = "help",
      panels  = {
        { kind = "content", text = table.concat(lines, "\n"), height = "fill" },
      },
      keymaps = {
        { key = "q", on_press = function(ctx) ctx.close() end },
        { key = "?", on_press = function(ctx) ctx.close() end },
      },
    })
  end)
end, { desc = "show keybindings" })
