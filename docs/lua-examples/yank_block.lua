-- Bind <Space>y to yank the block under the cursor (normal mode only).
-- Uses the built-in /yank-block command which copies the selectable
-- text of the block at the transcript cursor to the system clipboard.

smelt.keymap("n", "<Space>y", function()
    smelt.api.cmd.run("/yank-block")
end)
