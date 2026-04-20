-- Register a custom command and remap a keybind.
-- Drop this into ~/.config/smelt/init.lua to try.

-- /hello — greet with a notification.
smelt.api.cmd.register("hello", function(arg)
  local name = arg or "world"
  smelt.notify("hello, " .. name .. "!")
end)

-- Remap Ctrl-S in normal mode to run /fork.
smelt.keymap("n", "<C-s>", function()
  smelt.api.cmd.run("fork")
  smelt.notify("session forked")
end)
