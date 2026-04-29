-- Register a custom command and remap a keybind.
-- Drop this into ~/.config/smelt/init.lua to try.

-- /hello — greet with a notification.
smelt.cmd.register("hello", function(arg)
  local name = arg or "world"
  smelt.notify("hello, " .. name .. "!")
end)

-- Remap Ctrl-S in normal mode to run /fork.
smelt.keymap.set("n", "<C-s>", function()
  smelt.cmd.run("fork")
  smelt.notify("session forked")
end)
