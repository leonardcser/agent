-- `/diff-split <filepath>` - side-by-side diff of <filepath> vs `git show HEAD:<filepath>`.
-- Demo of `smelt.render.diff_split` + `smelt.ui.layout.hbox`.
-- Not autoloaded; add `require("smelt.examples.diff")` to init.lua.

local M = {}

local STATE = nil

local function notify_err(msg)
	smelt.notify.error("/diff-split: " .. msg)
end

local function git_root()
	local res, err = smelt.process.run("git", { "rev-parse", "--show-toplevel" })
	if err or not res or res.exit_code ~= 0 then
		return nil, "not in a git repo"
	end
	return (res.stdout or ""):gsub("%s+$", ""), nil
end

local function head_blob(root, rel)
	local res, err = smelt.process.run("git", { "-C", root, "show", "HEAD:" .. rel })
	if err or not res then
		return nil, err or "git show failed"
	end
	if res.exit_code ~= 0 then
		-- New file (no HEAD blob) - treat as empty so the right side shows all-insert.
		return "", nil
	end
	return res.stdout or "", nil
end

local function close()
	if not STATE then
		return
	end
	-- Closing the leaf closes the whole overlay when the leaf belongs to one.
	if STATE.left_win then
		STATE.left_win:close()
	end
	STATE = nil
end

local function open(filepath)
	if STATE then
		close()
	end
	filepath = (filepath or ""):gsub("^%s+", ""):gsub("%s+$", "")
	if filepath == "" then
		notify_err("usage: /diff-split <filepath>")
		return
	end

	local root, gerr = git_root()
	if not root then
		notify_err(gerr)
		return
	end

	local abs = filepath
	if not smelt.path.is_absolute(abs) then
		abs = smelt.path.normalize(smelt.path.join(root, filepath))
	end
	local rel = smelt.path.relative(root, abs)

	local new_text, rerr = smelt.fs.read(abs)
	if not new_text then
		notify_err(rerr or ("could not read " .. abs))
		return
	end
	local old_text, herr = head_blob(root, rel)
	if not old_text then
		notify_err(herr)
		return
	end

	local left_buf = smelt.buf.new()
	local right_buf = smelt.buf.new()

	smelt.render.diff_split(left_buf, right_buf, {
		old = old_text,
		new = new_text,
		path = rel,
	})
	left_buf:readonly(true)
	right_buf:readonly(true)

	local vim = smelt.settings.vim and true or false
	local win_opts = {
		surface = "readonly_text",
		vim_enabled = vim,
		cursor_line = true,
	}
	local left_win = smelt.win.new(left_buf, win_opts)
	local right_win = smelt.win.new(right_buf, win_opts)

	local overlay = smelt.overlay.new({
		title = smelt.dialog.title({
			{ text = " diff ", fg = "green", bold = true },
			{ text = rel .. " ", fg = "white" },
			{ text = "(q / esc to close) ", fg = "grey" },
		}),
		anchor = "center",
		width = "90%",
		height = "85%",
		layout = smelt.ui.layout.hbox({
			{
				smelt.ui.layout.leaf(left_win, { title = smelt.dialog.title({ { text = " HEAD ", fg = "red" } }) }),
				width = "fill",
			},
			{
				smelt.ui.layout.leaf(right_win, { title = smelt.dialog.title({ { text = " working ", fg = "green" } }) }),
				width = "fill",
			},
		}, { gap = 1 }),
		modal = true,
		draggable = true,
		resizable = true,
		keymaps = {
			{ key = "q", on_press = close },
		},
	})

	STATE = { overlay = overlay, left_win = left_win, right_win = right_win }

	left_win:link_scroll(right_win)

	-- No Esc keymap: with `modal = true`, Tier 5 of the key cascade dismisses
	-- the overlay on bare Esc / Ctrl-C - and only when vim is in idle Normal
	-- (so Esc in Visual still exits Visual instead of closing the dialog).
	-- We listen for `close` to keep STATE in sync once the overlay tears down.
	left_win:on("close", function()
		STATE = nil
	end)
	left_win:focus()
end

smelt.cmd.register("diff-split", function(value)
	smelt.spawn(function() open(value) end)
end, { desc = "side-by-side diff of <filepath> vs HEAD (demo)", args = { "<filepath>" } })

return M
