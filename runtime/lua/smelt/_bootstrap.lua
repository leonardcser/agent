-- Task-yielding primitives. Each checks `coroutine.isyieldable()` so
-- calls from a non-task context raise a clear error instead of yielding
-- into the void. Autoloaded before user init.lua so every plugin sees
-- `smelt.sleep`, and before `smelt.dialog` / `smelt.picker` so those
-- runtime files can reference yield helpers safely.

function smelt.sleep(ms)
  if not coroutine.isyieldable() then
    error("smelt.sleep: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  return coroutine.yield({ __yield = "sleep", ms = ms })
end

-- Park the running task until `smelt.task.resume(id, value)` fires (from a
-- Rust callback, a keymap, another task, …). Returns the resumed value.
-- Sugar over `coroutine.yield({__yield="external", id=...})` so plugins
-- don't spell the sentinel by hand.
function smelt.task.wait(id)
  if not coroutine.isyieldable() then
    error("smelt.task.wait: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  return coroutine.yield({ __yield = "external", id = id })
end

-- Side-call from a plugin tool's `execute` into a core (or another plugin)
-- tool. Suspends the running coroutine until the engine returns the
-- result. Pass `parent_call_id` (from the `ctx` table) so streamed output
-- groups under the visible plugin invocation. Returns the result table
-- `{ content, is_error, metadata? }`.
function smelt.tools.call(name, args, parent_call_id)
  if not coroutine.isyieldable() then
    error("smelt.tools.call: call from inside tool.execute", 2)
  end
  local id = smelt.task.alloc()
  smelt.tools.__send_call(id, parent_call_id or "", name, args or {})
  return coroutine.yield({ __yield = "external", id = id })
end

-- Fuzzy-rank a list against `query`. Returns an array of 1-based indices
-- into `items`, best matches first. `key_fn(item) -> haystack_string` is
-- optional; omit to score the raw item (must be a string). An empty query
-- returns the original ordering.
function smelt.fuzzy.rank(items, query, key_fn)
  if query == nil or query == "" then
    local all = {}
    for i = 1, #items do all[i] = i end
    return all
  end
  local scored = {}
  for i, it in ipairs(items) do
    local hay
    if key_fn then
      hay = key_fn(it)
    elseif type(it) == "string" then
      hay = it
    else
      hay = (it.label or "") .. " " .. (it.description or "") .. " " .. (it.search_terms or "")
    end
    local s = smelt.fuzzy.score(hay, query)
    if s ~= nil then
      scored[#scored + 1] = { score = s, idx = i }
    end
  end
  table.sort(scored, function(a, b)
    if a.score ~= b.score then return a.score < b.score end
    return a.idx < b.idx
  end)
  local out = {}
  for _, r in ipairs(scored) do out[#out + 1] = r.idx end
  return out
end
