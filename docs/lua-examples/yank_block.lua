-- Bind <Space>y to yank the block under the cursor (normal mode only).
-- Loads the optional /yank-block plugin, which copies the selectable
-- text of the block at the transcript cursor to the system clipboard.

require("smelt.plugins.yank_block")

smelt.keymap.set("n", "<Space>y", function()
    smelt.cmd.run("/yank-block")
end)
