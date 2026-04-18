-- Vim-style leader keymap: <Space>nn notifies, <Space>ll lists commands.
-- Drop this into ~/.config/smelt/init.lua to try.

smelt.keymap("n", "<Space>nn", function()
  smelt.notify("hello from lua leader")
end)

smelt.keymap("n", "<Space>ll", function()
  smelt.notify("commands registered via lua are live")
end)
