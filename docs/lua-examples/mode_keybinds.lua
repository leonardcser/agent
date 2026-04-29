-- Mode-aware keybind example.
-- <C-y> copies the transcript when in the transcript window,
-- or copies the prompt text when in the prompt window.

smelt.keymap.set("n", "<C-y>", function()
    local win = smelt.win.focus()
    if win == "transcript" then
        local text = smelt.transcript.text()
        if #text > 0 then
            smelt.clipboard(text)
            smelt.notify("transcript copied")
        end
    elseif win == "prompt" then
        local text = smelt.buf.text()
        if #text > 0 then
            smelt.clipboard(text)
            smelt.notify("prompt copied")
        end
    end
end)
