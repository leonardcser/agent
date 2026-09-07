-- Compacts older history while preserving a live recent suffix. Owns the
-- /compact command and the post-turn auto-compact subscription. When the
-- context window is filling up, it runs inherited-session summarisation over
-- the older history with a fixed structured prompt while preserving a live
-- suffix verbatim.
--
-- Compaction is visible to the user via `smelt.work.busy("compacting")`
-- and emits structured `compaction` log events with before/after token
-- counts.

-- Task instruction appended as the FINAL user message of the summariser
-- request. Everything before it (system, tools, prior messages) mirrors
-- the main session, so the request hits the same Anthropic prefix cache
-- slot. Only this trailing instruction is fresh on each compaction.
local SUMMARY_TASK = [[
The conversation above is becoming long. Stop the current task and instead produce a CONTEXT CHECKPOINT COMPACTION: a structured handoff summary that another instance of yourself will read to resume the task without losing critical context.

Reply in this exact Markdown structure. Omit a section only if the conversation truly contains nothing for it; never invent details. Respond with ONLY the Markdown document - no preamble, no apology. Under no circumstances use tools; any tool call will be denied and you must answer with the Markdown summary only.

# Goal
Overall objective the user ultimately wants accomplished. Distinguish this from the narrower current focus when they differ.

# Current focus
What the assistant is currently working on, including whether it is complete and how it relates to the overall objective.

# Constraints
Hard limits, style rules, environment facts, anything the next instance must respect.

# Progress
What has already been done. Concrete, specific, in completion order.

# Decisions
Choices that were made and the rationale, when the rationale matters for what comes next.

# Next steps
Ordered, concrete actions the next instance should take. If the current focus is complete, explicitly return to the overall objective.

# Critical context
File contents, error messages, command output, exact identifiers, etc. that the next instance will need verbatim. Quote precisely.

# Relevant files
Bullet list of file paths that were touched or are about to be touched, with a one-line note on why each matters.
]]

local SUMMARY_PREFIX = smelt.engine.summary_prefix()

local INSTRUCTIONS_PREAMBLE = "The user has asked you to pay special attention to the following when summarizing:"

-- How many times to re-issue the summarizer call when the model returns
-- an empty response before giving up.
local MAX_EMPTY_RETRIES = 2

-- Tool use is forbidden during compaction even though the inherited
-- request keeps the live tool schema for KV-cache reuse. If the model
-- calls a tool anyway, we append denial tool results and retry the same
-- boundary. A repeated tool-call response restarts that boundary once
-- from the original request shape before giving up.
local MAX_TOOL_CALL_RESTARTS = 2

local TOOL_DENIED_MESSAGE = "Tool use is not allowed during compaction. Respond with text only."

-- Byte caps keep the extra intent anchors bounded without adding token-estimation
-- plumbing to the Lua compaction path.
local RECENT_USER_MESSAGE_LIMIT = 3
local RECENT_USER_MESSAGE_BYTE_BUDGET = 12000
local RECENT_USER_MESSAGE_MAX_BYTES = 6000

-- Circuit breaker. After this many consecutive failed compactions, the
-- plugin stops auto-firing for the rest of the session to avoid burning
-- tokens in a loop. /compact still works (manual override).
local MAX_CONSECUTIVE_FAILURES = 3
local compact_state = smelt.state.get("compact")
local consecutive_failures = compact_state.consecutive_failures or 0

local function set_consecutive_failures(n)
	consecutive_failures = n
	compact_state.consecutive_failures = n
end

local function record_compaction(kind, phase, before_tokens, first_live_message_index)
	compact_state.total = (compact_state.total or 0) + 1
	compact_state[kind] = (compact_state[kind] or 0) + 1
	compact_state.last_phase = phase
	compact_state.last_tokens_before = before_tokens
	compact_state.last_first_live_message_index = first_live_message_index
	set_consecutive_failures(0)
end

local function record_failure()
	compact_state.failures = (compact_state.failures or 0) + 1
	set_consecutive_failures(consecutive_failures + 1)
end

local function trip_circuit_breaker()
	compact_state.failures = (compact_state.failures or 0) + 1
	set_consecutive_failures(MAX_CONSECUTIVE_FAILURES)
end

local function is_terminal_provider_error(err)
	return err and (err.kind == "quota" or err.kind == "rate_limited")
end

local function set_compaction_preview(summary)
	local transcript = __smelt_internal.transcript
	if transcript and transcript._set_compaction_preview then
		transcript._set_compaction_preview(summary)
	end
end

-- ── helpers ────────────────────────────────────────────────────────────

local function trim(s)
	return (s or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local function combine_instructions(a, b)
	a = trim(a)
	b = trim(b)
	if a == "" then
		return b ~= "" and b or nil
	end
	if b == "" then
		return a
	end
	return a .. "\n\n" .. b
end

local function content_text(content)
	if type(content) == "string" then
		return content
	end
	if type(content) ~= "table" then
		return nil
	end

	local parts = {}
	for _, item in ipairs(content) do
		if type(item) == "table" and type(item.text) == "string" then
			table.insert(parts, item.text)
		end
	end

	if #parts == 0 then
		return nil
	end
	return table.concat(parts, "\n")
end

local function is_checkpoint_summary(text)
	return text:find(SUMMARY_PREFIX, 1, true) == 1
end

local function recent_user_intent_instructions(history)
	local selected = {}
	local remaining = RECENT_USER_MESSAGE_BYTE_BUDGET

	for i = #(history or {}), 1, -1 do
		if #selected >= RECENT_USER_MESSAGE_LIMIT or remaining <= 0 then
			break
		end

		local msg = history[i]
		if msg.role == "user" then
			local text = trim(content_text(msg.content))
			if text ~= "" and not is_checkpoint_summary(text) then
				local limit = math.min(RECENT_USER_MESSAGE_MAX_BYTES, remaining)
				local clipped = smelt.text.truncate(text, limit, "\n[truncated]")
				table.insert(selected, 1, clipped)
				remaining = remaining - #clipped
			end
		end
	end

	if #selected == 0 then
		return nil
	end

	local out = {
		"Recent user intent anchors, verbatim. These may include completed requests. Use them as evidence for task priority and return/resume instructions, while distinguishing completed work, the current focus, and the overall objective. Do not assume the latest user message replaces the overall objective.",
	}
	for i, text in ipairs(selected) do
		table.insert(out, string.format("<user_message index=\"%d\">\n%s\n</user_message>", i, text))
	end
	return table.concat(out, "\n\n")
end

-- Compose the trailing user message. Folds optional per-call instructions
-- into the structured-summary spec.
local function build_summary_task(instructions)
	local task = trim(SUMMARY_TASK)
	if instructions then
		local extra = trim(instructions)
		if extra ~= "" then
			task = task .. "\n\n" .. INSTRUCTIONS_PREAMBLE .. "\n" .. extra
		end
	end
	return task
end

local function clone_messages(messages)
	local out = {}
	for i, msg in ipairs(messages or {}) do
		out[i] = msg
	end
	return out
end

local function append_tool_denials(messages, assistant_response)
	local out = clone_messages(messages)
	table.insert(out, assistant_response)
	for _, call in ipairs(assistant_response.tool_calls or {}) do
		table.insert(out, {
			role = "tool",
			tool_call_id = call.id,
			content = TOOL_DENIED_MESSAGE,
			is_error = true,
		})
	end
	return out
end

-- Async summarizer primitive shared by /compact and post-turn auto-compact.
-- Uses `ask_inherited` so the request shares the main turn's
-- system prompt and tool list. `history` may be a strict prefix of the
-- live model-visible conversation; when omitted the full live history is
-- inherited.
local function summarize_messages(history, instructions, current, done)
	if not history or #history == 0 then
		done(nil)
		return
	end
	local task = build_summary_task(instructions)
	local empty_retries = 0
	local boundary_restarts = 0
	local streamed_summary = ""
	local base_messages = clone_messages(history)
	table.insert(base_messages, { role = "user", content = task })

	local function finish(summary, err)
		if not summary and current() then
			set_compaction_preview(nil)
		end
		done(summary, err)
	end

	local function send(messages, retrying_after_tool_denial)
		if not current() then
			finish(nil, { kind = "cancelled", message = "compaction superseded" })
			return
		end
		local response_received = false
		streamed_summary = ""
		set_compaction_preview(streamed_summary)
		smelt.engine.ask_inherited({
			messages = messages,
			model = smelt.model.preferred("compact"),
			visible_retries = true,
			on_delta = function(delta)
				if response_received or not current() then
					return
				end
				streamed_summary = streamed_summary .. delta
				set_compaction_preview(streamed_summary)
			end,
			on_response = function(response, err)
				if response_received then
					return
				end
				response_received = true
				if not current() then
					finish(nil, { kind = "cancelled", message = "compaction superseded" })
					return
				end
				if err then
					if smelt.task.is_cancelled(err) then
						finish(nil, err)
						return
					end
					if is_terminal_provider_error(err) then
						set_consecutive_failures(MAX_CONSECUTIVE_FAILURES)
						finish(nil, err)
						return
					end
					if err.kind ~= "context_window" then
						smelt.notify.error("compaction failed: " .. err.message)
					end
					finish(nil, err)
					return
				end
				if response and response.tool_calls and #response.tool_calls > 0 then
					if not retrying_after_tool_denial then
						send(append_tool_denials(messages, response), true)
						return
					end
					if boundary_restarts < MAX_TOOL_CALL_RESTARTS then
						boundary_restarts = boundary_restarts + 1
						send(base_messages, false)
						return
					end
					finish(nil, {
						kind = "invalid_response",
						message = "model kept requesting tools during compaction",
					})
					return
				end
				local summary = response and response.content or nil
				if not summary or summary == "" then
					if empty_retries < MAX_EMPTY_RETRIES then
						empty_retries = empty_retries + 1
						send(base_messages, false)
						return
					end
					smelt.notify.error("compaction failed: empty summary after retries")
					finish(nil, nil)
					return
				end
				finish(summary, nil)
			end,
		})
	end

	send(base_messages, false)
end

local function build_message_groups(history)
	local groups = {}
	local i = 1
	while i <= #history do
		local msg = history[i]
		if msg.role == "assistant" then
			local j = i
			if msg.tool_calls and #msg.tool_calls > 0 then
				j = i + 1
				while j <= #history and history[j].role == "tool" do
					j = j + 1
				end
				j = j - 1
			end
			table.insert(groups, { start_idx = i, end_idx = j })
			i = j + 1
		else
			table.insert(groups, { start_idx = i, end_idx = i })
			i = i + 1
		end
	end
	return groups
end

local function slice_group_prefix(history, groups, last_group)
	local out = {}
	if last_group <= 0 then
		return out
	end
	for group_idx = 1, last_group do
		local group = groups[group_idx]
		for msg_idx = group.start_idx, group.end_idx do
			table.insert(out, history[msg_idx])
		end
	end
	return out
end

local function suffix_first_live_message_index(history, groups, suffix_start_group)
	if suffix_start_group > #groups then
		return #history
	end
	return groups[suffix_start_group].start_idx - 1
end

local function message_is_checkpoint_summary(msg)
	return msg and msg.role == "user" and is_checkpoint_summary(trim(content_text(msg.content)))
end

local function prefix_group_is_compactable(history, prefix_last_group)
	if prefix_last_group <= 0 then
		return false
	end
	if message_is_checkpoint_summary(history[1]) and prefix_last_group <= 1 then
		return false
	end
	return true
end

local function compaction_boundary(history)
	if not history or #history == 0 then
		return nil
	end
	local groups = build_message_groups(history)
	if #groups == 0 then
		return nil
	end
	local min_postponed_groups = math.max(0, math.floor(smelt.settings.compact_keep_recent_groups or 1))
	local suffix_start_group = math.max(1, (#groups + 1) - math.min(min_postponed_groups, #groups))
	local prefix_last_group = suffix_start_group - 1
	if not prefix_group_is_compactable(history, prefix_last_group) then
		return nil
	end
	return {
		groups = groups,
		suffix_start_group = suffix_start_group,
		prefix_last_group = prefix_last_group,
	}
end

local function has_compactable_group_prefix(history)
	return compaction_boundary(history) ~= nil
end

local function checkpointed_messages_from_boundary(history, summary, first_live_message_index)
	local out = {
		{ role = "user", content = SUMMARY_PREFIX .. "\n" .. summary },
	}
	for i = first_live_message_index + 1, #history do
		table.insert(out, history[i])
	end
	return out
end

local function summarize_by_group_boundary(history, instructions, done)
	if not history or #history == 0 then
		done(nil)
		return
	end

	local boundary = compaction_boundary(history)
	if not boundary then
		done(nil)
		return
	end

	local handle = __smelt_internal.work._context_recalculation("compacting")
	local groups = boundary.groups
	local suffix_start_group = boundary.suffix_start_group
	local summary_instructions = combine_instructions(instructions, recent_user_intent_instructions(history))
	local finished = false
	local work_guard = smelt.work.guard()
	local lifecycle = smelt.lifecycle.guard({ "session", "history" }):latest("compaction")

	local function current()
		return not finished and handle:alive() and lifecycle:alive() and smelt.work.guard_current(work_guard)
	end

	local function finish(summary, err, first_live_message_index)
		if finished then
			return
		end
		finished = true
		local ok, callback_err = pcall(done, summary, err, first_live_message_index)
		handle:remove()
		if not ok then
			error(callback_err)
		end
	end

	local function attempt()
		local prefix_last_group = suffix_start_group - 1
		if not prefix_group_is_compactable(history, prefix_last_group) then
			finish(nil)
			return
		end

		local prefix_messages = slice_group_prefix(history, groups, prefix_last_group)
		summarize_messages(prefix_messages, summary_instructions, current, function(summary, err)
			if summary then
				finish(summary, nil, suffix_first_live_message_index(history, groups, suffix_start_group))
				return
			end
			if smelt.task.is_cancelled(err) then
				finish(nil, err)
				return
			end
			if err and err.kind == "context_window" then
				if prefix_last_group > 1 then
					suffix_start_group = suffix_start_group - 1
					attempt()
					return
				end
				finish(nil, err)
				return
			end
			finish(nil, err)
		end)
	end

	local ok, err = pcall(attempt)
	if not ok then
		handle:remove()
		error(err)
	end
end

local function emit_event(phase, before_tokens, after_tokens, extra)
	local data = { phase = phase }
	if before_tokens then
		data.before_tokens = before_tokens
	end
	if after_tokens then
		data.after_tokens = after_tokens
	end
	if before_tokens and after_tokens then
		data.saved_tokens = math.max(0, before_tokens - after_tokens)
	end
	if extra then
		for k, v in pairs(extra) do
			data[k] = v
		end
	end
	smelt.log.info("compaction", data)
end

local function run_compact(opts)
	local history = smelt.session.model_messages()
	if not history or #history == 0 then
		smelt.notify.error("nothing to compact")
		return
	end
	if not has_compactable_group_prefix(history) then
		smelt.notify.info("nothing old enough to compact")
		return
	end
	-- Use the provider's actual prompt-token count.  If no turn has
	-- completed yet (context_tokens is nil) we skip rather than guess.
	local before_tokens = smelt.session.context_tokens()
	if not before_tokens then
		smelt.notify.error("no token usage available yet; try again after the next turn completes")
		return
	end

	-- Summarise the original history. `ask_inherited` keeps the request
	-- prefix byte-identical to the main turn for cache reuse.
	local guard = smelt.work.guard()
	summarize_by_group_boundary(history, opts and opts.instructions, function(summary, err, first_live_message_index)
		if smelt.task.is_cancelled(err) then
			return
		end
		if is_terminal_provider_error(err) then
			trip_circuit_breaker()
			smelt.notify.error(err.message)
			return
		end
		if not summary then
			record_failure()
			return
		end
		local installed = smelt.session.checkpoint({
			kind = "compaction",
			summary = summary,
			first_live_message_index = first_live_message_index,
			tokens_before = before_tokens,
			guard = guard,
		})
		if not installed then
			return
		end
		record_compaction("manual", "summarize", before_tokens, first_live_message_index)
		emit_event("summarize", before_tokens, nil, {
			first_live_message_index = first_live_message_index,
		})
	end)
end

local function auto_compact_due(tokens)
	if not smelt.settings.auto_compact then
		return false
	end
	if consecutive_failures >= MAX_CONSECUTIVE_FAILURES then
		return false
	end
	local window = smelt.session.context_window()
	if not window then
		return false
	end
	local threshold = smelt.settings.compact_threshold
	if not (threshold > 0 and threshold <= 1) then
		threshold = 0.80
	end
	if not tokens then
		return false
	end
	return tokens >= window * threshold
end

local function is_unchanged_checkpoint_estimate(estimate)
	return estimate
		and estimate.source == "checkpoint_estimate"
		and estimate.estimated_delta_tokens == 0
end

local function compact_live_session(before_tokens, phase, opts, done)
	local history = smelt.session.model_messages()
	if not has_compactable_group_prefix(history) then
		done(nil)
		return
	end
	summarize_by_group_boundary(history, nil, function(summary, err, first_live_message_index)
		if opts and opts.guard and not smelt.work.guard_current(opts.guard) then
			done(nil, nil)
			return
		end
		if smelt.task.is_cancelled(err) then
			done(nil, err)
			return
		end
		if not summary then
			if is_terminal_provider_error(err) then
				trip_circuit_breaker()
			else
				record_failure()
			end
			done(nil, err)
			return
		end
		local installed = smelt.session.checkpoint({
			kind = "compaction",
			summary = summary,
			first_live_message_index = first_live_message_index,
			tokens_before = before_tokens,
			guard = opts and opts.guard or nil,
		})
		if not installed then
			done(nil)
			return
		end
		record_compaction("auto", phase, before_tokens, first_live_message_index)
		emit_event(phase, before_tokens, nil, {
			first_live_message_index = first_live_message_index,
		})
		done(true)
	end)
end

-- ── /compact command ──────────────────────────────────────────────────

smelt.cmd.register("compact", function(arg)
	set_consecutive_failures(0)
	run_compact({ instructions = arg, inject_recent_user_messages = false })
end, { desc = "compact conversation history", args = { "<instructions>" }, busy = "reject" })

-- ── pre-request auto-compaction ───────────────────────────────────────
--
-- Engine invokes this immediately before sending a model request. This
-- is the single normal auto-compact path: it uses the active-context
-- estimate for the request the engine is actually about to send, anchored
-- to the last provider-reported context size when available.

smelt.engine.on_prepare_request(function(request, reply)
	local estimate = request and request.context_estimate
	-- An unchanged checkpoint estimate is the result of this compaction cycle.
	-- Let its immediate retry reach the provider instead of reconsidering the
	-- same history when the estimate remains above the configured threshold.
	if is_unchanged_checkpoint_estimate(estimate) then
		reply(nil)
		return
	end
	local estimated_tokens = request and (request.estimated_context_tokens or request.estimated_tokens)
	if not auto_compact_due(estimated_tokens) then
		reply(nil)
		return
	end
	emit_event("trigger-pre-request", estimated_tokens, nil, {
		estimate_source = estimate and estimate.source or nil,
		provider_context_tokens = estimate and estimate.provider_context_tokens or nil,
		estimated_delta_tokens = estimate and estimate.estimated_delta_tokens or nil,
		snapshot_history_len = estimate and estimate.latest_snapshot_history_len or nil,
		current_history_len = estimate and estimate.current_history_len or nil,
	})
	local guard = smelt.work.guard()
	compact_live_session(estimated_tokens, "summarize-pre-request", { guard = guard }, function(replaced, err)
		if is_terminal_provider_error(err) then
			reply({ action = "abort", message = err.message })
			return
		end
		if not replaced then
			reply(nil)
			return
		end
		reply({ action = "replace", source = "model_history" })
	end)
end)

-- ── mid-turn recovery hook ────────────────────────────────────────────
--
-- Engine invokes this when a provider returns a context-window error
-- mid-turn. Honours `settings.auto_compact`: when disabled, the hook
-- calls `reply(nil)` immediately so the engine aborts the turn with the
-- standard error. When enabled, reruns the same group-boundary compaction
-- algorithm against the failing request snapshot and hands the engine a
-- shortened replacement request: compaction summary + preserved live suffix.

smelt.engine.on_context_limit(function(messages, reply)
	if not smelt.settings.auto_compact then
		reply(nil)
		return
	end

	if not has_compactable_group_prefix(messages) then
		reply(nil)
		return
	end

	local guard = smelt.work.guard()
	summarize_by_group_boundary(messages, nil, function(summary, err, first_live_message_index)
		if not smelt.work.guard_current(guard) then
			reply(nil)
			return
		end
		if smelt.task.is_cancelled(err) then
			reply(nil)
			return
		end
		if is_terminal_provider_error(err) then
			trip_circuit_breaker()
			reply({ action = "abort", message = err.message })
			return
		end
		if not summary then
			record_failure()
			reply(nil)
			return
		end
		local installed = smelt.session.checkpoint({
			kind = "compaction",
			summary = summary,
			first_live_message_index = first_live_message_index,
			guard = guard,
		})
		set_compaction_preview(nil)
		record_compaction("recovery", "summarize-recovery", 0, first_live_message_index)
		emit_event("summarize-recovery", 0, 0, {
			first_live_message_index = first_live_message_index,
		})
		if installed then
			reply({ action = "replace", source = "model_history" })
		else
			-- Detached requests have no canonical session boundary to checkpoint.
			local replacement = checkpointed_messages_from_boundary(messages, summary, first_live_message_index)
			reply({ action = "replace", messages = replacement })
		end
	end)
end)
