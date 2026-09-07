-- Root transcript renderer helpers. Rust owns only the single renderer handle;
-- middleware composition is ordinary Lua around that handle.

smelt.transcript = smelt.transcript or {}

--- Bounded tool output metadata passed to transcript renderers.
---@class smelt.transcript.ToolOutput
---@field content_id integer Stable shared-content id accepted by `smelt.layout.content`.
---@field content_revision integer Monotonic content revision.
---@field content_bytes integer Current output size in bytes.
---@field content_lines integer Current logical line count.
---@field content_preview string Bounded text preview for labels and fallback UI, not complete output.
---@field is_error boolean True when the tool result is an error.
---@field metadata? table Bounded tool-specific structured metadata.
---@field content_fields? table<string, smelt.transcript.ContentMetadata> Named retained payloads referenced by opaque content IDs.

--- Metadata for a retained payload whose complete content remains in Rust and is
--- available to renderers only through retained layout leaves.
---@class smelt.transcript.ContentMetadata
---@field content_id integer Stable shared-content id accepted by retained layout leaves.
---@field content_revision integer Monotonic content revision.
---@field content_bytes integer Current content size in bytes.
---@field content_lines integer Current logical line count.
---@field content_preview string Strictly bounded preview for labels, never the complete retained payload.

--- Opaque top-level string argument passed to transcript renderers. Complete field
--- content remains in Rust and is available only through `smelt.layout.content`.
---@class smelt.transcript.ArgumentField
---@field name string Top-level argument name.
---@field content_id integer Stable shared-content id accepted by `smelt.layout.content`.
---@field content_revision integer Monotonic content revision.
---@field content_bytes integer Current field size in bytes.
---@field content_lines integer Current logical line count.
---@field content_preview string Bounded text preview for labels and fallback UI, not complete content.
---@field complete boolean True when the JSON parser has consumed the complete field value.

--- Renderer context. Width, theme, and scroll state are intentionally absent.
---@class smelt.transcript.Context
---@field view_state "collapsed"|"peek"|"expanded"|"trimmed_head"|"trimmed_tail" Effective view state for the node currently being rendered.
---@field renderer_generation integer Current renderer generation used for cache invalidation.
---@field surface string Rendering surface name, currently `"transcript"`.
---@field limits table Numeric product row budgets such as `tool_output_rows`.
---@field now_ms integer Unix epoch milliseconds shared by the complete top-level render pass.
---@field render fun(node: smelt.transcript.Block, overrides?: { view_state?: string }): smelt.layout.Node Re-enter the composed root renderer for a semantic child.

--- Bounded semantic metadata for one retained group child. Growing content and
--- complete child payloads are never embedded in group renderer input.
---@class smelt.transcript.GroupChild
---@field id integer Stable child block id.
---@field kind string Semantic block kind.
---@field name? string Tool name.
---@field status? "pending"|"confirm"|"ok"|"err"|"denied" Tool status.
---@field summary_text? string Bounded plain-text summary.
---@field called_at_ms? integer Invocation start as Unix epoch milliseconds.
---@field args? table Bounded argument previews used by collapsed labels.
---@field output? { content_lines?: integer, is_error?: boolean } Bounded output metadata.
---@field event? string Process status event type.
---@field event_data? { process_id?: string, exit_code?: integer } Bounded process metadata.
---@field process_id? string Background process id.
---@field exit_code? integer Background process exit code.

--- Bounded semantic transcript metadata passed to the root renderer.
---@class smelt.transcript.Block
---@field id integer Stable block id within the session.
---@field index integer Zero-based block index in transcript order.
---@field kind "user"|"assistant"|"thinking"|"tool"|"group"|"code"|"exec"|"mode"|"process_status"|"compacted"|"compaction_preview" Block kind.
---@field text? string User/mode/process text.
---@field user_lines? table User text as styled span lines, including slash/ref/image accents.
---@field sent_at_ms? integer User-message submission time as Unix epoch milliseconds; absent for undated history.
---@field content? string Code content.
---@field content_id? integer Stable shared-content id for assistant and thinking blocks.
---@field content_revision? integer Monotonic shared-content revision.
---@field content_bytes? integer Shared content size in bytes.
---@field content_lines? integer Shared content logical line count.
---@field content_preview? string Bounded preview for labels and fallback UI, not complete content.
---@field title? string Latest structured reasoning-summary title.
---@field summary_titles? string[] Ordered structured reasoning-summary title history.
---@field reasoning_kind? "summary"|"raw" Reasoning source for thinking blocks.
---@field image_labels? string[] User image labels.
---@field icon? string Mode icon.
---@field hl_group? string Mode/process highlight group.
---@field lang? string Code language.
---@field call_id? string Tool call id.
---@field name? string Tool name.
---@field args? table Bounded tool-argument previews and complete non-string structured values.
---@field argument_fields? smelt.transcript.ArgumentField[] Opaque top-level string arguments. Complete field content is never included in renderer metadata.
---@field summary? any Tool styled summary lines or compacted summary text.
---@field summary_text? string Tool summary flattened to plain text.
---@field status? "pending"|"confirm"|"ok"|"err"|"denied" Tool status.
---@field called_at_ms? integer Invocation start as Unix epoch milliseconds.
---@field elapsed_ms? integer Best-known execution duration in milliseconds.
---@field elapsed_active? boolean True only while elapsed time can continue advancing.
---@field thinking_summary? string Folded thinking summary text.
---@field user_message? string Tool user-facing status message.
---@field preview_output? smelt.transcript.ToolOutput Immutable pending output metadata for a promoted finished draft.
---@field output? smelt.transcript.ToolOutput Tool output metadata.
---@field event? string Process status event type, e.g. `"background_process_completed"`.
---@field event_type? string Alias for `event`.
---@field event_data? table Full typed process status event payload.
---@field process_id? string Background process id for process status events.
---@field exit_code? integer Background process exit code when known.
---@field command? string Exec command.
---@field command_spans? table Exec command as one styled span line, including the `!` accent.
---@field group_kind? string Registered semantic group name.
---@field bucket? string Stable planner bucket for a group.
---@field view_state? "collapsed"|"peek"|"expanded" Effective group or child view state.
---@field children? smelt.transcript.GroupChild[] Ordered bounded child presentation metadata for a group.
---@field child_ids? integer[] Ordered stable block ids for a group.
---@field child_count? integer Number of semantic children in a group.

--- Group selector declared through `smelt.transcript.groups.register`.
---@class smelt.transcript.GroupSelector
---@field kind? string Match block kind.
---@field name? string Match one tool name for tool blocks.
---@field names? string[] Match any listed tool name for tool blocks. Cannot be combined with `name`.
---@field terminal? boolean Match terminal/non-terminal blocks.
---@field event? string Match typed process status event type.
---@field event_type? string Alias for `event`.
---@field process_id? string Match typed background process id.
---@field exit_code? integer|string Match typed background process exit code.
---@field fields? table<string,string|integer> Exact block-field matches such as `{ event = "background_process_completed" }`.

--- Declarative transcript group registration. The host owns planning; the root
--- transcript renderer owns presentation for the resulting semantic group node.
---@class smelt.transcript.GroupSpec
---@field name string Unique group name. Registering the same name replaces it.
---@field cache_key? string Persisted layout cache key; omit to opt out while active.
---@field priority? integer Higher priority plans first. Defaults to 0.
---@field min? integer Minimum adjacent matching blocks required. Defaults to 2.
---@field default_view? "collapsed"|"peek"|"expanded" Initial presentation when the group first appears.
---@field selector smelt.transcript.GroupSelector Declarative block matcher.
---@field bucket? string|string[] Stable field names used to split adjacent matching runs.

--- Style attributes accepted on one styled title span.
---@class smelt.transcript.StyledSpanStyle
---@field hl? string Theme highlight group.
---@field fg? string Foreground color.
---@field bg? string Background color.
---@field dim? boolean Whether to dim the text.
---@field bold? boolean Whether to render bold text.
---@field italic? boolean Whether to render italic text.

--- One span in a styled tool title. Style attributes may be supplied directly or through `style`.
---@class smelt.transcript.StyledSpan
---@field text? string Span text. The positional field `[1]` is also accepted.
---@field style? smelt.transcript.StyledSpanStyle Nested style attributes.
---@field syntax? string Syntax language used to highlight the span text.
---@field hl? string Theme highlight group.
---@field fg? string Foreground color.
---@field bg? string Background color.
---@field dim? boolean Whether to dim the text.
---@field bold? boolean Whether to render bold text.
---@field italic? boolean Whether to render italic text.
---@field selectable? boolean Whether copied transcript text includes this span.
---@field title_suffix? boolean Whether this span is transient pending-state title metadata.

--- Options passed to focused tool body and draft callbacks.
---@class smelt.transcript.ToolBodyOptions
---@field gutter? string Prefix rendered before each body line.

--- Options accepted by the default tool header renderer.
---@class smelt.transcript.ToolHeaderOptions
---@field hl? string Status marker highlight group.

--- Public presentation policy for one tool name. A complete `render` callback
--- takes precedence; otherwise the default renderer composes the focused pieces.
--- Registrations are copied and immutable.
---@class smelt.transcript.ToolPresentation
---@field cache_key? string Stable persisted-layout key. Omit for dynamic presentation state.
---@field render? fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context, presentation: smelt.transcript.ToolPresentation): smelt.layout.Node Complete replacement renderer.
---@field title? fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context): string|smelt.transcript.StyledSpan[][]|nil Semantic title after the status marker. Return nil to use the tool summary.
---@field body? fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context, opts?: smelt.transcript.ToolBodyOptions): smelt.layout.Node|nil Expanded body renderer. Return nil to suppress the body.
---@field draft? fun(draft: smelt.transcript.Block, ctx: smelt.transcript.Context, opts?: smelt.transcript.ToolBodyOptions): smelt.layout.Node|nil Draft body renderer. Return nil to suppress the body.
---@field compact? fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context): string|smelt.layout.Node|nil Collapsed detail renderer. Return nil to suppress the detail.

local DEFAULT_RENDERER_CACHE_KEY = "smelt.transcript.defaults:v4"
local transcript = smelt.transcript
local internal_transcript = __smelt_internal.transcript
local base_renderer = internal_transcript.__get_renderer and internal_transcript.__get_renderer() or nil
local base_renderer_cache_key = nil
local extensions = {}
local order = {}
local tool_presentations = {}
local tool_order = {}
local next_token = 0
local tool_presentation_fields = { "cache_key", "render", "title", "body", "draft", "compact" }

local function copy_tool_presentation(presentation)
  local copy = {}
  for _, field in ipairs(tool_presentation_fields) do
    copy[field] = presentation[field]
  end
  return copy
end

local function require_function(name, value)
  if type(value) ~= "function" then
    error("smelt.transcript." .. name .. ": expected function", 3)
  end
end

local function parse_cache_key(name, opts)
  if opts == nil then return nil end
  if type(opts) ~= "table" then
    error("smelt.transcript." .. name .. ": opts must be a table", 3)
  end
  local key = opts.cache_key
  if key == nil or key == false then return nil end
  if type(key) ~= "string" or key == "" then
    error("smelt.transcript." .. name .. ": opts.cache_key must be a non-empty string", 3)
  end
  return key
end

local function effective_cache_key()
  if not base_renderer_cache_key then return nil end
  local parts = { "base", base_renderer_cache_key }
  for i = 1, #order do
    local name = order[i]
    local entry = extensions[name]
    if entry then
      if not entry.cache_key then return nil end
      parts[#parts + 1] = name
      parts[#parts + 1] = entry.cache_key
    end
  end
  for i = 1, #tool_order do
    local name = tool_order[i]
    local entry = tool_presentations[name]
    if entry then
      if not entry.presentation.cache_key then return nil end
      parts[#parts + 1] = "tool:" .. name
      parts[#parts + 1] = entry.presentation.cache_key
    end
  end
  return table.concat(parts, "\n")
end

local function rebuild_renderer()
  local renderer = base_renderer
  for i = 1, #order do
    local entry = extensions[order[i]]
    if entry then
      local next_renderer = renderer
      local fn = entry.fn
      renderer = function(block, ctx)
        return fn(next_renderer, block, ctx)
      end
    end
  end
  if renderer then internal_transcript.__set_renderer(renderer, effective_cache_key()) end
end

local function set_base_renderer(renderer, cache_key)
  base_renderer = renderer
  base_renderer_cache_key = cache_key
  rebuild_renderer()
end

--- Replace the base transcript renderer used when the host asks Lua for a
--- transcript block layout. Existing middleware registered with `extend_renderer`
--- remains wrapped around the new base. The renderer must return a `smelt.layout`
--- value; return `smelt.layout.empty()` to hide a block. Omit `opts.cache_key`
--- to opt out of persisted DisplayIR, or bump it whenever renderer output changes
--- across process restarts.
---@type fun(renderer: fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?, opts: table?): nil
function smelt.transcript.set_renderer(renderer, opts)
  require_function("set_renderer", renderer)
  set_base_renderer(renderer, parse_cache_key("set_renderer", opts))
end

--- Return the current composed root transcript renderer, or nil before the
--- default renderer has been installed.
---@type fun(): (fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?)?
function smelt.transcript.get_renderer()
  return internal_transcript.__get_renderer()
end

--- Add or replace named middleware around the root renderer. Later extensions
--- run first. The callback receives `(next, block, ctx)` and may return its own
--- layout or delegate with `next(block, ctx)`. The returned `Reg` removes only
--- this registration instance. Omit `opts.cache_key` to opt out of persisted
--- DisplayIR while the middleware is active.
---@type fun(name: string, renderer: fun(next: fun(block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?, block: smelt.transcript.Block, ctx: smelt.transcript.Context): table?, opts: table?): smelt.Reg
function smelt.transcript.extend_renderer(name, renderer, opts)
  if type(name) ~= "string" or name == "" then
    error("smelt.transcript.extend_renderer: name must be a non-empty string", 2)
  end
  require_function("extend_renderer", renderer)

  next_token = next_token + 1
  local token = next_token
  if not extensions[name] then order[#order + 1] = name end
  extensions[name] = { fn = renderer, token = token, cache_key = parse_cache_key("extend_renderer", opts) }
  rebuild_renderer()

  return smelt.reg.new(function()
    local entry = extensions[name]
    if not entry or entry.token ~= token then return end
    extensions[name] = nil
    for i = #order, 1, -1 do
      if order[i] == name then
        table.remove(order, i)
        break
      end
    end
    rebuild_renderer()
  end)
end

--- Bump the renderer generation after changing closed-over state that affects
--- renderer output without calling `set_renderer`, `extend_renderer`, or a
--- registration's `:remove()`. This also opts out of persisted DisplayIR until
--- the renderer is installed again with a cache key.
---@type fun(): integer
function smelt.transcript.invalidate_renderer()
  return internal_transcript.__invalidate_renderer()
end

--- Register or replace presentation policy for a tool. The supported fields are
--- copied, so later table mutation has no effect; re-register to change behavior
--- or cache keys. The returned registration removes only this exact replacement.
--- Registering presentation changes rebuilds the composed root renderer so all
--- cached standalone and grouped nodes invalidate.
---@type fun(name: string, presentation: smelt.transcript.ToolPresentation): smelt.Reg
function smelt.transcript.register_tool(name, presentation)
  if type(name) ~= "string" or name == "" then
    error("smelt.transcript.register_tool: name must be a non-empty string", 2)
  end
  if type(presentation) ~= "table" then
    error("smelt.transcript.register_tool: presentation must be a table", 2)
  end
  for _, field in ipairs({ "render", "title", "body", "draft", "compact" }) do
    if presentation[field] ~= nil and type(presentation[field]) ~= "function" then
      error("smelt.transcript.register_tool: presentation." .. field .. " must be a function", 2)
    end
  end
  if presentation.cache_key ~= nil
    and (type(presentation.cache_key) ~= "string" or presentation.cache_key == "")
  then
    error("smelt.transcript.register_tool: presentation.cache_key must be a non-empty string", 2)
  end

  local retained = copy_tool_presentation(presentation)
  next_token = next_token + 1
  local token = next_token
  if not tool_presentations[name] then tool_order[#tool_order + 1] = name end
  tool_presentations[name] = { presentation = retained, token = token }
  rebuild_renderer()

  return smelt.reg.new(function()
    local entry = tool_presentations[name]
    if not entry or entry.token ~= token then return end
    tool_presentations[name] = nil
    for i = #tool_order, 1, -1 do
      if tool_order[i] == name then
        table.remove(tool_order, i)
        break
      end
    end
    rebuild_renderer()
  end)
end

--- Return a copy of the current presentation policy for `name`, or nil. Mutating
--- the returned table has no effect; re-register the tool to change its presentation.
---@type fun(name: string): smelt.transcript.ToolPresentation?
function smelt.transcript.get_tool_presentation(name)
  local entry = tool_presentations[name]
  return entry and copy_tool_presentation(entry.presentation) or nil
end

smelt.transcript.groups = smelt.transcript.groups or {}

local function require_table(name, value)
  if type(value) ~= "table" then
    error("smelt.transcript." .. name .. ": expected table", 3)
  end
end

--- Register or replace a declarative transcript group type. This only declares
--- planning metadata; Rust owns deterministic adjacent-run planning and the
--- composed root transcript renderer presents resulting group nodes.
---@type fun(spec: smelt.transcript.GroupSpec): smelt.Reg
function smelt.transcript.groups.register(spec)
  require_table("groups.register", spec)
  if type(spec.name) ~= "string" or spec.name == "" then
    error("smelt.transcript.groups.register: spec.name must be a non-empty string", 2)
  end
  if type(spec.selector) ~= "table" then
    error("smelt.transcript.groups.register: spec.selector must be a table", 2)
  end

  local token = internal_transcript.__register_group(spec)
  local name = spec.name
  return smelt.reg.new(function()
    internal_transcript.__unregister_group(name, token)
  end)
end

--- Return group specs in planner order: higher priority first, then registration order.
---@type fun(): smelt.transcript.GroupSpec[]
function smelt.transcript.groups.list()
  return internal_transcript.__groups()
end

--- Current group-registry generation. Rust render planning uses this to invalidate
--- plan/cache state once group planning is enabled.
---@type fun(): integer
function smelt.transcript.groups.generation()
  return internal_transcript.__groups_generation()
end

--- Current group-registry cache key, or nil when any active group opted out of
--- persisted planning/layout caches.
---@type fun(): integer?
function smelt.transcript.groups.cache_key()
  return internal_transcript.__groups_cache_key()
end

local defaults = require("smelt.transcript.defaults")

set_base_renderer(function(block, ctx)
  return defaults.render(block, ctx)
end, DEFAULT_RENDERER_CACHE_KEY)

require("smelt.transcript.builtins").register()

package.loaded["smelt.transcript"] = smelt.transcript

return smelt.transcript
