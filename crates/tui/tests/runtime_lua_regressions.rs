const DIALOG_LUA: &str = include_str!("../../../runtime/lua/smelt/dialog.lua");
const PS_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/ps.lua");
const BAR_LUA: &str = include_str!("../../../runtime/lua/smelt/_bar.lua");
const PROMPT_BAR_LUA: &str = include_str!("../../../runtime/lua/smelt/prompt_bar.lua");
const COPY_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/copy.lua");
const LABEL_VALUE_LUA: &str = include_str!("../../../runtime/lua/smelt/label_value.lua");
const SESSION_LUA: &str = include_str!("../../../runtime/lua/smelt/session.lua");
const BANNER_LUA: &str = include_str!("../../../runtime/lua/smelt/banner.lua");
const BANNER_PLUGIN_LUA: &str = include_str!("../../../runtime/lua/smelt/plugins/banner.lua");
const WEB_FETCH_LUA: &str = include_str!("../../../runtime/lua/smelt/tools/web_fetch.lua");
const WEB_SEARCH_LUA: &str = include_str!("../../../runtime/lua/smelt/tools/web_search.lua");
const TRANSCRIPT_DEFAULTS_LUA: &str =
    include_str!("../../../runtime/lua/smelt/transcript/defaults.lua");
const SCROLL_PILLS_LUA: &str = include_str!("../../../runtime/lua/smelt/plugins/scroll_pills.lua");
const NOTIFICATIONS_LUA: &str = include_str!("../../../runtime/lua/smelt/notifications.lua");
const NOTIFY_COMMAND_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/notify.lua");
const TURN_NOTIFICATIONS_LUA: &str =
    include_str!("../../../runtime/lua/smelt/plugins/turn_notifications.lua");
const ARGV_LUA: &str = include_str!("../../../runtime/lua/smelt/argv.lua");
const WORKTREE_LUA: &str = include_str!("../../../runtime/lua/smelt/worktree.lua");
const USAGE_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/usage.lua");
const USAGE_CACHE_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/usage/cache.lua");

fn install_explicit_api_fixtures(lua: &mlua::Lua) {
    lua.load(
        r#"
        function __smelt_notify_stub(notices, errors)
          return {
            info = function(msg) if notices then notices[#notices + 1] = msg end end,
            error = function(msg) if errors then errors[#errors + 1] = msg end end,
            warn = function() end,
            scoped = function(source)
              return {
                info = function(msg) if notices then notices[#notices + 1] = msg end end,
                error = function(msg) if errors then errors[#errors + 1] = msg end end,
                warn = function() end,
              }
            end,
          }
        end

        function __smelt_signal_stub(cells, values)
          values = values or {}
          return {
            get = function(name) return values[name] end,
            set = function(name, value) values[name] = value end,
            subscribe = function(name, fn) cells[name] = fn end,
          }
        end
        "#,
    )
    .exec()
    .expect("install explicit API fixtures");
}

fn bootstrap_fixture_environment(lua: &mlua::Lua, path: &'static str) -> mlua::Table {
    assert!(
        smelt_core::lua::runtime::BOOTSTRAP_CHUNKS
            .iter()
            .any(|chunk| chunk.path == path),
        "missing bootstrap manifest entry for {path}"
    );
    smelt_core::lua::module::bootstrap_chunk_environment(lua, true)
        .unwrap_or_else(|error| panic!("build bootstrap fixture environment for {path}: {error}"))
}

fn load_transcript_defaults(lua: &mlua::Lua) {
    let environment = bootstrap_fixture_environment(lua, "transcript/defaults.lua");
    let internal: mlua::Table = lua
        .load(
            r#"
            return {
              layout = {
                __is_node = function(value)
                  return type(value) == "table" and type(value.kind) == "string"
                end,
              },
            }
            "#,
        )
        .eval()
        .expect("trusted transcript fixture capabilities");
    environment
        .raw_set("__smelt_internal", internal)
        .expect("install transcript fixture capabilities");
    lua.load(TRANSCRIPT_DEFAULTS_LUA)
        .set_name("transcript/defaults.lua")
        .set_environment(environment)
        .exec()
        .expect("load transcript defaults");
}

fn load_web_fetch(lua: &mlua::Lua) {
    let environment = smelt_core::lua::module::bootstrap_chunk_environment(lua, true)
        .expect("build web_fetch fixture environment");
    let internal: mlua::Table = environment
        .raw_get("__smelt_internal")
        .expect("trusted web_fetch fixture capabilities");
    let tools = lua.create_table().expect("web_fetch tool capabilities");
    tools
        .set(
            "with_watchdog",
            lua.create_function(|_, tool: mlua::Table| Ok(tool))
                .expect("web_fetch watchdog fixture"),
        )
        .expect("install web_fetch watchdog fixture");
    internal
        .set("tools", tools)
        .expect("install web_fetch tool capabilities");
    lua.load(WEB_FETCH_LUA)
        .set_name("tools/web_fetch.lua")
        .set_environment(environment)
        .exec()
        .expect("load web_fetch");
}

fn install_notifications_preload(lua: &mlua::Lua) {
    lua.globals()
        .set("__NOTIFICATIONS_LUA", NOTIFICATIONS_LUA)
        .expect("install notifications lua source");
    lua.load(
        r#"
        package.preload["smelt.notifications"] = function()
          return assert(load(__NOTIFICATIONS_LUA, "smelt/notifications.lua"))()
        end
        "#,
    )
    .exec()
    .expect("install notification module preload");
}

#[test]
fn usage_command_renders_codex_reset_times_in_local_time() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.globals()
        .set("__USAGE_CACHE_LUA", USAGE_CACHE_LUA)
        .expect("install usage cache source");
    lua.load(
        r#"
        local last_buf = nil
        local command = nil
        package.preload["smelt.commands.usage.cache"] = function()
          return assert(load(__USAGE_CACHE_LUA, "smelt/commands/usage/cache.lua"))()
        end
        package.loaded["smelt.bar"] = {
          progress = function()
            return { { text = "[bar]" } }
          end,
        }
        package.loaded["smelt.modal"] = { open = function() end }

        smelt = {
          __last_buf = nil,
          __command = nil,
          lifecycle = { on_ready = function() end },
          cmd = { register = function(name, fn) if name == "usage" then command = fn; smelt.__command = fn end end },
          log = { warn = function() end },
          notify = __smelt_notify_stub(nil, nil),
          spawn = function(fn) fn() end,
          task = { external = function(fn) return fn(1) end, resume = function() end },
          model = {
            current = function() return "codex-model" end,
            list = function() return { { key = "codex-model", provider = "codex", api_base = "" } } end,
            pricing = function() return { source = "none" } end,
          },
          session = { cost = function() return 0 end },
          text = { format_cost = function() return "$0.00" end },
          time = { format = function(stamp, fmt) return os.date(fmt, stamp) end },
          auth = {
            request = function(provider, opts)
              assert(provider == "codex")
              assert(opts.path == "/wham/usage")
              return { status = 200, body = "{}" }
            end,
          },
          parse = {
            json = function()
              return {
                rate_limit = {
                  primary_window = {
                    used_percent = 25,
                    limit_window_seconds = 18000,
                    reset_at = 1700000000,
                  },
                },
              }
            end,
          },
          json = { encode = function() return "{}" end },
          buf = {
            new = function()
              local buf = { rows = {} }
              function buf:styled(rows) self.rows = rows end
              last_buf = buf
              smelt.__last_buf = buf
              return buf
            end,
          },
          dialog = {
            content = function(opts) return { buf = opts.buf, on = function() end } end,
            menu = function() return { on = function() end }, { set_items = function() end } end,
            new = function()
              return { win = { on = function() end } }
            end,
          },
        }
        "#,
    )
    .exec()
    .expect("install usage command stubs");
    lua.load(USAGE_LUA).exec().expect("load usage command");

    let (rendered, expected): (String, String) = lua
        .load(
            r#"
            assert(smelt.__command)()
            local parts = {}
            for _, line in ipairs(assert(smelt.__last_buf).rows) do
              for _, span in ipairs(line) do
                parts[#parts + 1] = span.text or ""
              end
              parts[#parts + 1] = "\n"
            end
            return table.concat(parts), "resets " .. os.date("%b %d %H:%M", 1700000000)
            "#,
        )
        .eval()
        .expect("render usage dialog");

    assert!(
        rendered.contains(&expected),
        "rendered usage was:\n{rendered}"
    );
    assert!(!USAGE_LUA.contains("os.date(\"!"));
}

#[test]
fn argv_split_handles_quotes_and_reports_incomplete_input() {
    let lua = mlua::Lua::new();
    let argv: mlua::Table = lua.load(ARGV_LUA).eval().expect("load argv module");

    let (count, first, second, third): (i64, String, String, String) = lua
        .load(r#"local argv = ...; local args = assert(argv.split([[alpha "two words" 'three words']])) return #args, args[1], args[2], args[3]"#)
        .call(argv.clone())
        .expect("split argv input");

    assert_eq!(count, 3);
    assert_eq!(first, "alpha");
    assert_eq!(second, "two words");
    assert_eq!(third, "three words");

    let err: String = lua
        .load(r#"local argv = ...; local args, err = argv.split([[unterminated "]]); assert(args == nil); return err"#)
        .call(argv)
        .expect("report argv error");
    assert_eq!(err, "unterminated quote");
}

#[test]
fn worktree_picker_items_mark_current_and_switch_existing_paths() {
    let lua = mlua::Lua::new();
    let worktree: mlua::Table = lua.load(WORKTREE_LUA).eval().expect("load worktree module");

    let (create_action, current_label, current_desc, other_action, other_desc, other_path): (
        String,
        String,
        String,
        String,
        String,
        String,
    ) = lua
        .load(
            r##"
            local worktree = ...
            local rows = worktree.picker_items({
              info = { cwd = "/repo" },
              accent = "#ffaa00",
              worktrees = {
                { name = "worktree-command", branch = "worktree-command", path = "/repo/.worktrees/worktree-command", current = true },
                { name = "feature", branch = "feature", path = "/repo/.worktrees/feature", current = false },
              },
            })
            return rows[1].action, rows[2].label, rows[2].description, rows[3].action, rows[3].description, rows[3].path
            "##,
        )
        .call(worktree)
        .expect("build worktree picker rows");

    assert_eq!(create_action, "create");
    assert_eq!(current_label, "worktree-command*");
    assert_eq!(current_desc, "/repo/.worktrees/worktree-command");
    assert_eq!(other_action, "switch");
    assert_eq!(other_desc, "/repo/.worktrees/feature");
    assert_eq!(other_path, "/repo/.worktrees/feature");
}

#[test]
fn worktree_picker_items_include_list_error() {
    let lua = mlua::Lua::new();
    let worktree: mlua::Table = lua.load(WORKTREE_LUA).eval().expect("load worktree module");

    let (label, description): (String, String) = lua
        .load(
            r#"
            local worktree = ...
            local rows = worktree.picker_items({ info = { cwd = "/repo" }, list_error = "git failed" })
            return rows[3].label, rows[3].description
            "#,
        )
        .call(worktree)
        .expect("build list error row");

    assert_eq!(label, "could not list worktrees");
    assert_eq!(description, "git failed");
}

#[test]
fn dialog_menu_disabled_items_are_not_selectable_or_submittable() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local next_buf_id = 0
        local last_leaf = nil
        smelt = {
          __last_leaf = nil,
          ns = function(name) return name end,
          log = { error = function() end },
          notify = __smelt_notify_stub(nil, nil),
          text = {
            width = function(s) return #(s or "") end,
            wrap_prefixed = function(text, width, opts)
              opts = opts or {}
              return { (opts.prefix or "") .. (text or "") }
            end,
          },
          buf = {
            new = function()
              next_buf_id = next_buf_id + 1
              local buf = { id = next_buf_id, rows = {}, marks = {} }
              function buf:lines(rows) self.rows = rows; return self end
              function buf:clear_ns() return self end
              function buf:mark(ns, row, col, opts)
                self.marks[#self.marks + 1] = { ns = ns, row = row, col = col, opts = opts }
                return self
              end
              function buf:styled(rows) self.rows = rows; return self end
              function buf:source(text) self.text = text; return self end
              function buf:readonly() return self end
              return buf
            end,
          },
          win = {
            new = function(buf, opts)
              local leaf = { buf = buf, opts = opts or {}, keys = {}, cursor_row = (opts and opts.initial_cursor) or 0 }
              function leaf:key(key, fn) self.keys[key] = fn end
              function leaf:cursor(row)
                if row == nil then return self.cursor_row end
                self.cursor_row = row
              end
              function leaf:reveal(row, opts)
                self.revealed_row = row
                self.reveal_opts = opts
                return self
              end
              function leaf:on() end
              function leaf:set_renderer(fn) self.renderer = fn end
              function leaf:content_width() return 80 end
              function leaf:row_highlights(specs) self.highlights = specs; return self end
              function leaf:focus() end
              function leaf:close() end
              last_leaf = leaf
              smelt.__last_leaf = leaf
              return leaf
            end,
          },
        }
        "#,
    )
    .exec()
    .expect("install dialog stubs");
    lua.load(DIALOG_LUA)
        .set_name("dialog.lua")
        .set_environment(bootstrap_fixture_environment(&lua, "dialog.lua"))
        .exec()
        .expect("load dialog module");

    let submitted: Vec<i64> = lua
        .load(
            r#"
            local submitted = {}
            local leaf, ctrl = smelt.dialog.menu({
              { label = "Refresh" },
              { label = "Redeem reset", disabled = true },
              { label = "Cancel" },
            }, {
              on_submit = function(ctx) submitted[#submitted + 1] = ctx.index end,
            })

            ctrl:cursor(2)
            local cursor_skips_disabled = ctrl:cursor()

            leaf:cursor(1)
            leaf.keys.enter()
            local after_disabled_enter = #submitted

            leaf.keys["2"]()
            local after_disabled_digit = #submitted

            leaf.keys.j()
            leaf.keys.enter()
            return { cursor_skips_disabled, after_disabled_enter, after_disabled_digit, submitted[1] or 0 }
            "#,
        )
        .eval()
        .expect("exercise disabled menu items");

    assert_eq!(submitted, [3, 0, 0, 3]);
}

#[test]
fn copy_command_copies_recent_conversation_messages() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local notices = {}
        local errors = {}
        smelt = {
          __commands = {},
          __copied = nil,
          __notices = notices,
          __errors = errors,
          cmd = {
            register = function(name, fn, opts)
              smelt.__commands[name] = { fn = fn, opts = opts }
            end,
          },
          notify = __smelt_notify_stub(notices, errors),
          clipboard = {
            write = function(text) smelt.__copied = text end,
          },
          session = {
            conversation = function()
              return {
                { role = "user", content = "first user" },
                { role = "assistant", content = "first assistant" },
                { role = "user", content = "second user" },
                { role = "assistant", content = "second assistant\n\n" },
              }
            end,
          },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    lua.load(COPY_LUA).exec().expect("load copy command");

    let (one, two, assistant, yank): (String, String, String, String) = lua
        .load(
            r#"
            smelt.__commands.copy.fn()
            local one = smelt.__copied
            smelt.__commands.copy.fn("2")
            local two = smelt.__copied
            smelt.__commands.copy.fn("--role assistant")
            local assistant = smelt.__copied
            smelt.__commands.yank.fn("--headers 1")
            local yank = smelt.__copied
            return one, two, assistant, yank
            "#,
        )
        .eval()
        .expect("run copy commands");

    assert_eq!(one, "second assistant\n\n");
    assert_eq!(
        two,
        "User:\nsecond user\n\nAssistant:\nsecond assistant\n\n"
    );
    assert_eq!(assistant, "second assistant\n\n");
    assert_eq!(yank, "Assistant:\nsecond assistant\n\n");
}

#[test]
fn copy_command_reports_invalid_arguments() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local errors = {}
        smelt = {
          __commands = {},
          __errors = errors,
          cmd = { register = function(name, fn) smelt.__commands[name] = fn end },
          notify = __smelt_notify_stub(nil, errors),
          clipboard = { write = function() end },
          session = { conversation = function() return {} end },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    lua.load(COPY_LUA).exec().expect("load copy command");

    let err: String = lua
        .load(
            r#"
            smelt.__commands.copy("--role tool")
            return smelt.__errors[1]
            "#,
        )
        .eval()
        .expect("run invalid copy command");

    assert_eq!(err, "usage: /copy [--role user|assistant] [--headers] [N]");
}

#[test]
fn session_tree_orders_nested_forks_and_prefixes() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load("smelt = { session = {} }")
        .exec()
        .expect("init smelt table");
    lua.load(SESSION_LUA)
        .set_name("session.lua")
        .set_environment(bootstrap_fixture_environment(&lua, "session.lua"))
        .exec()
        .expect("load session helpers");

    let rows: mlua::Table = lua
        .load(
            r#"
            local entries = {
              { id = "old", updated_at_ms = 1, created_at_ms = 1 },
              { id = "root", updated_at_ms = 2, created_at_ms = 2 },
              { id = "fork_a", parent_id = "root", updated_at_ms = 3, created_at_ms = 3 },
              { id = "fork_b", parent_id = "root", updated_at_ms = 4, created_at_ms = 4 },
              { id = "nested", parent_id = "fork_b", updated_at_ms = 5, created_at_ms = 5 },
            }
            local out = smelt.session.tree(entries, { order = "asc" })
            local rows = {}
            for i, e in ipairs(out) do
              rows[i] = (e.tree_prefix or "") .. e.id .. ":" .. tostring(e.tree_sort_value)
            end
            return rows
            "#,
        )
        .eval()
        .expect("evaluate tree");
    let got: Vec<String> = rows
        .sequence_values::<String>()
        .collect::<Result<_, _>>()
        .expect("rows");

    assert_eq!(
        got,
        [
            "old:1",
            "root:5",
            "├─ fork_a:3",
            "└─ fork_b:5",
            "   └─ nested:5",
        ]
    );
}

#[test]
fn web_search_auto_prefers_brave_with_stable_transport_options() {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        smelt = {
          settings = { web_search_provider = "auto", brave_search_api_key_env = "BRAVE_SEARCH_API_KEY" },
          tools = { register = function(tool) smelt.__tool = tool end },
          os = { getenv = function() return "secret" end },
          http = {
            get = function(url, opts)
              smelt.__url = url
              smelt.__opts = opts
              return { status = 200, body = [[{"web":{"results":[{"title":"Result","url":"https://example.com","description":"Found"}]}}]] }
            end,
            cache = { read = function() return nil end, write = function() end },
          },
          json = {
            decode = function()
              return { web = { results = { { title = "Result", url = "https://example.com", description = "Found" } } } }
            end,
          },
          html = { parse_ddg_results = function() return {} end },
        }
        "#,
    )
    .exec()
    .expect("install web search fixtures");
    lua.load(WEB_SEARCH_LUA).exec().expect("load web_search");

    let (output, uses_brave, encoding, retries): (String, bool, String, i64) = lua
        .load(
            r#"
            local output = smelt.__tool.execute({ query = "reliable fetch" })
            return output,
              smelt.__url:find("api.search.brave.com", 1, true) ~= nil,
              smelt.__opts.headers["Accept-Encoding"],
              smelt.__opts.max_retries
            "#,
        )
        .eval()
        .expect("search with Brave");

    assert!(output.contains("https://example.com"));
    assert!(uses_brave);
    assert_eq!(encoding, "identity");
    assert_eq!(retries, 2);
}

#[test]
fn web_fetch_renderer_uses_shared_llm_markdown() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local presentations = {}
        smelt = {
          transcript = {
            defaults = {},
            register_tool = function(name, presentation) presentations[name] = presentation end,
            get_tool_presentation = function(name) return presentations[name] end,
          },
          layout = {
            markdown = function(content, opts)
              return { kind = "markdown", content = content, opts = opts or {} }
            end,
            content = function(content_id, opts)
              return { kind = "content", content_id = content_id, opts = opts or {} }
            end,
            text = function(content, opts)
              return { kind = "text", content = content, opts = opts or {} }
            end,
            cap = function(child, opts)
              return { kind = "cap", child = child, opts = opts or {} }
            end,
            vbox = function(items)
              return { kind = "vbox", items = items }
            end,
            gutter = function(child, opts)
              return { kind = "gutter", child = child, opts = opts or {} }
            end,
            runs = function(lines, opts)
              return { kind = "runs", lines = lines, opts = opts or {} }
            end,
            hbox = function(items) return { kind = "hbox", items = items } end,
            line = function(spans) return { kind = "line", spans = spans } end,
            panel = function(child, opts) return { kind = "panel", child = child, opts = opts or {} } end,
            refresh = function(child, opts) return { kind = "refresh", child = child, opts = opts or {} } end,
            separator = function(opts) return { kind = "separator", opts = opts or {} } end,
            code = function(content, opts) return { kind = "code", content = content, opts = opts or {} } end,
          },
          tools = {
            register = function(tool) smelt.__registered_tool = tool end,
          },
        }
        package.loaded["smelt.transcript.defaults"] = smelt.transcript.defaults
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    load_transcript_defaults(&lua);
    load_web_fetch(&lua);

    let (body_kind, output_kind, child_kind, format, dim, rows): (
        String,
        String,
        String,
        String,
        bool,
        i64,
    ) = lua
        .load(
            r###"
            local renderer = assert(smelt.transcript.get_tool_presentation("web_fetch").body)
            local node = renderer({
              args = { prompt = "Summarise" },
              output = { content_id = 42, content_lines = 5, is_error = false },
            }, { limits = { tool_output_rows = 7 } })
            local output = node.items[2]
            return node.kind, output.kind, output.child.kind, output.child.opts.format,
              output.child.opts.dim, output.opts.rows
            "###,
        )
        .eval()
        .expect("render web_fetch body");

    assert_eq!(body_kind, "vbox");
    assert_eq!(output_kind, "cap");
    assert_eq!(child_kind, "content");
    assert_eq!(format, "markdown");
    assert!(dim);
    assert_eq!(rows, 7);
}

#[test]
fn web_fetch_auto_renders_spa_shell_with_installed_browser() {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        local waited = nil
        smelt = {
          settings = {
            web_fetch_render = "auto",
            web_fetch_renderer_command = "chromium",
          },
          transcript = { register_tool = function() end },
          layout = { text = function() return {} end, vbox = function() return {} end },
          tools = {
            register = function(tool) smelt.__tool = tool end,
          },
          http = {
            get = function(url, opts)
              smelt.__http_opts = opts
              return {
                status = 200,
                final_url = url,
                headers = { ["content-type"] = "text/plain" },
                body = '<html><body><div id="root"></div><script src="app.js"></script></body></html>',
              }
            end,
            cache = { read = function() return nil end, write = function() end },
          },
          html = {
            to_text = function(source)
              if source:find("Rendered article", 1, true) then return "Rendered article content" end
              return ""
            end,
            to_markdown = function(source)
              local content = source:find("Rendered article", 1, true) and "Rendered article content" or ""
              return { title = "Page", links = {}, content = content }
            end,
          },
          process = {
            run = function(command, args, opts)
              smelt.__browser = { command = command, args = args, opts = opts }
              return {
                exit_code = 0,
                stdout = "renderer-json",
                stderr = "",
              }
            end,
          },
          __renderer_final_url = "https://example.com/final",
          json = {
            encode = function(value)
              smelt.__renderer_request = value
              return "request-json"
            end,
            decode = function()
              return {
                status = 200,
                final_url = smelt.__renderer_final_url,
                html = "<html><head><title>Page</title></head><body>Rendered article</body></html>",
                truncated = false,
              }
            end,
          },
          time = { monotonic_ms = function() return 1000 end },
          task = {
            alloc = function() return 1 end,
            resume = function(_, value) waited = value end,
            wait = function() return waited end,
          },
          engine = {
            ask = function(opts)
              smelt.__question = opts.question
              opts.on_response({ content = "rendered answer" }, nil)
            end,
          },
          model = { preferred = function() return nil end },
        }
        package.loaded["smelt.transcript.defaults"] = {
          render_llm_markdown_tail = function() return {} end,
        }
        "#,
    )
    .exec()
    .expect("install web fetch fixtures");
    load_web_fetch(&lua);

    let (
        output,
        command,
        max_bytes,
        retries,
        rendered,
        renderer_url,
        stdin,
        renderer_output_bytes,
        rejected,
    ): (String, String, i64, i64, bool, String, String, i64, bool) = lua
        .load(
            r#"
            local output = smelt.__tool.execute({
              url = "https://example.com/app",
              prompt = "What is on the page?",
            })
            smelt.__renderer_final_url = "https://attacker.example/final"
            local rejected = smelt.__tool.execute({
              url = "https://example.com/app",
              prompt = "What is on the page?",
            })
            return output, smelt.__browser.command, smelt.__http_opts.max_response_bytes,
              smelt.__http_opts.max_retries,
              smelt.__question:find("Rendered article content", 1, true) ~= nil,
              smelt.__renderer_request.url, smelt.__browser.opts.stdin,
              smelt.__browser.opts.max_output_bytes,
              rejected.is_error == true and rejected.content:find("redirect crossed domains", 1, true) ~= nil
            "#,
        )
        .eval()
        .expect("fetch rendered SPA");

    assert_eq!(output, "rendered answer");
    assert_eq!(command, "chromium");
    assert_eq!(max_bytes, 5 * 1024 * 1024);
    assert_eq!(retries, 2);
    assert!(rendered);
    assert_eq!(renderer_url, "https://example.com/app");
    assert_eq!(stdin, "request-json");
    assert_eq!(renderer_output_bytes, 32 * 1024 * 1024);
    assert!(rejected);
}

#[test]
fn structured_tool_output_uses_shared_code_content() {
    let lua = mlua::Lua::new();
    lua.load(
        r#"
        smelt = {
          transcript = { defaults = {} },
          text = { truncate_cells = function(text) return text end },
          layout = {
            text = function(content, opts)
              return { kind = "text", content = content, opts = opts or {} }
            end,
            content = function(content_id, opts)
              return { kind = "content", content_id = content_id, opts = opts or {} }
            end,
            cap = function(child, opts)
              return { kind = "cap", child = child, opts = opts or {} }
            end,
            runs = function(lines, opts)
              return { kind = "runs", lines = lines, opts = opts or {} }
            end,
            code = function(content, opts)
              return { kind = "code", content = content, opts = opts or {} }
            end,
          },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    load_transcript_defaults(&lua);

    let (node_kind, child_kind, format, lang): (String, String, String, String) = lua
        .load(
            r#"
            local defaults = require("smelt.transcript.defaults")
            local node = defaults.render_tool_output_tail({
              content_id = 42,
              content_lines = 3,
              metadata = { syntax = "json" },
            }, { limits = { tool_output_rows = 20 } })
            return node.kind, node.child.kind, node.child.opts.format, node.child.opts.lang
            "#,
        )
        .eval()
        .expect("render structured tool output");

    assert_eq!(node_kind, "cap");
    assert_eq!(child_kind, "content");
    assert_eq!(format, "code");
    assert_eq!(lang, "json");
}

#[test]
fn ps_details_dialog_uses_list_dialog_height() {
    assert!(PS_LUA.contains("local DIALOG_HEIGHT = \"60%\""));
    assert!(PS_LUA.contains("height = DIALOG_HEIGHT"));
    assert!(PS_LUA.contains("height      = DIALOG_HEIGHT"));
    assert!(!PS_LUA.contains("max_height = \"70%\""));
}

fn prompt_bar_lua_fixture() -> mlua::Lua {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local next_buf_id = 0
        smelt = {
          __resize = false,
          __resize_chrome = "",
          __wins = {},
          ns = function(name) return name end,
          settings = { show_tokens = true, show_cost = true },
          model = { current = function() return "model" end },
          reasoning = { current = function() return "off" end },
          signal = {
            get = function(name)
                if name == "prompt_resize_active" then return smelt.__resize end
                if name == "prompt_resize_chrome" then return smelt.__resize_chrome end
                if name == "work_state" then return "working" end
                if name == "work_label" then return "run" end
                if name == "work_elapsed_ms" then return 0 end
                if name == "work_retry_attempt" then return 0 end
                if name == "work_retry_remaining_ms" then return 0 end
                if name == "notification_visible" then return false end
                return nil
            end,
          },
          text = {
            width = function(text)
              local n = 0
              for _ in utf8.codes(text or "") do n = n + 1 end
              return n
            end,
            truncate_cells = function(text, width, opts)
              text = text or ""
              width = math.max(width or 0, 0)
              local chars = {}
              for _, code in utf8.codes(text) do chars[#chars + 1] = utf8.char(code) end
              if #chars <= width then return text end
              local suffix = (opts and opts.suffix) or "…"
              if width <= 0 then return "" end
              local out = {}
              for i = 1, math.max(width - 1, 0) do out[#out + 1] = chars[i] end
              out[#out + 1] = suffix
              return table.concat(out)
            end,
            format_tokens = function(value) return tostring(value) end,
            format_cost = function(value) return string.format("$%.2f", value) end,
            format_duration = function(value) return tostring(value) .. "s" end,
          },
          spinner = {
            glyph = function() return "*" end,
            wave_state = function() return 0, 1, 3 end,
            wave_level_at = function() return 2 end,
            wave_color_at = function() return { 1, 2, 3 } end,
          },
          prompt = {
            queued_rows = function() return {} end,
            queued = function() return {} end,
            has_stash = function() return false end,
            text = function() return "" end,
            is_modal = function() return false end,
          },
          session = {
            context_tokens = function() return 1200 end,
            context_window = function() return 10000 end,
            cost = function() return 0.23 end,
          },
          buf = {
            new = function()
              next_buf_id = next_buf_id + 1
              local b = { id = next_buf_id, _lines = {}, _marks = {} }
              function b:lines(lines) self._lines = lines end
              function b:clear_ns() self._marks = {} end
              function b:mark(ns, row, start_col, opts)
                self._marks[#self._marks + 1] = {
                  ns = ns,
                  row = row,
                  start_col = start_col,
                  end_col = opts.end_col,
                  fg = opts.fg,
                  hl_group = opts.hl_group,
                  bold = opts.bold,
                }
              end
              return b
            end,
          },
          win = {
            new = function(buf, opts)
              local w = { _buf = buf, _opts = opts or {} }
              function w:buf() return self._buf end
              function w:content_width() return 60 end
              function w:rect() return { height = 1 } end
              function w:set_renderer(fn) self.renderer = fn end
              smelt.__wins[w._opts.name] = w
              return w
            end,
          },
        }
        package.loaded["smelt.tips"] = {
          enabled = function() return false end,
          prompt_tip = function() return nil end,
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");

    let bar_src = BAR_LUA.to_string();
    let bar_loader = lua
        .create_function(move |lua, ()| lua.load(&bar_src).eval::<mlua::Value>())
        .expect("bar loader");
    let package: mlua::Table = lua.globals().get("package").expect("package table");
    let preload: mlua::Table = package.get("preload").expect("preload table");
    preload
        .set("smelt._bar", bar_loader)
        .expect("install bar preload");

    lua.load(PROMPT_BAR_LUA).exec().expect("load prompt bar");
    lua
}

#[test]
fn prompt_bar_renders_fast_mode_marker_before_model() {
    let lua = prompt_bar_lua_fixture();

    let (text, color, bold, line): (String, Option<String>, Option<bool>, String) = lua
        .load(
            r#"
            smelt.session.status = function()
              return { model = "model", fast = { supported = true, active = true } }
            end
            local top = assert(smelt.__wins["smelt.prompt_bar.top"])
            top:renderer()
            local buf = top:buf()
            local line = buf._lines[1] or ""
            for _, mark in ipairs(buf._marks) do
              local marked = string.sub(buf._lines[mark.row] or "", mark.start_col + 1, mark.end_col)
              if marked == " >>" then return marked, mark.fg, mark.bold, line end
            end
            return "", nil, nil, line
            "#,
        )
        .eval()
        .expect("render fast mode prompt bar");

    assert_eq!(text, " >>");
    assert_eq!(color.as_deref(), Some("Comment"));
    assert_eq!(bold, Some(true));
    assert!(line.contains(">> model"), "{line}");
    assert!(!line.contains(">>model"), "{line}");
}

#[test]
fn prompt_resize_highlight_overrides_top_bar_chrome_and_separator_dots() {
    let lua = prompt_bar_lua_fixture();

    let (inactive_dots, inactive_bar_dots, active_dots, active_resize_dots, active_dashes, bottom_resize_dashes, bottom_bar_dashes): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = lua
        .load(
            r#"
            local top = assert(smelt.__wins["smelt.prompt_bar.top"])
            local bottom = assert(smelt.__wins["smelt.prompt_bar.bottom"])

            local function marked_text(buf, mark)
              return string.sub(buf._lines[mark.row] or "", mark.start_col + 1, mark.end_col)
            end
            local function count_marks(buf, predicate)
              local n = 0
              for _, mark in ipairs(buf._marks) do
                if predicate(mark, marked_text(buf, mark)) then n = n + 1 end
              end
              return n
            end

            top:renderer()
            local inactive_dots = count_marks(top:buf(), function(_, text) return text == " ·" end)
            local inactive_bar_dots = count_marks(top:buf(), function(mark, text)
              return text == " ·" and mark.fg == "SmeltSeparator" and mark.hl_group == nil
            end)

            smelt.__resize = true
            smelt.__resize_chrome = "top"
            top:renderer()
            bottom:renderer()
            local active_dots = count_marks(top:buf(), function(_, text) return text == " ·" end)
            local active_resize_dots = count_marks(top:buf(), function(mark, text)
              return text == " ·" and mark.hl_group == "SmeltResizeHandle" and mark.fg == nil
            end)
            local active_dashes = count_marks(top:buf(), function(mark, text)
              return text:find("─", 1, true) ~= nil and mark.hl_group == "SmeltResizeHandle" and mark.fg == nil
            end)
            local bottom_resize_dashes = count_marks(bottom:buf(), function(mark, text)
              return text:find("─", 1, true) ~= nil and mark.hl_group == "SmeltResizeHandle" and mark.fg == nil
            end)
            local bottom_bar_dashes = count_marks(bottom:buf(), function(mark, text)
              return text:find("─", 1, true) ~= nil and mark.fg == "SmeltSeparator" and mark.hl_group == nil
            end)
            return inactive_dots, inactive_bar_dots, active_dots, active_resize_dots, active_dashes, bottom_resize_dashes, bottom_bar_dashes
            "#,
        )
        .eval()
        .expect("render prompt bars");

    assert_eq!(inactive_dots, 2);
    assert_eq!(inactive_bar_dots, inactive_dots);
    assert_eq!(active_dots, 2);
    assert_eq!(active_resize_dots, active_dots);
    assert!(active_dashes >= 2);
    assert_eq!(bottom_resize_dashes, 0);
    assert_eq!(bottom_bar_dashes, 1);
}

#[test]
fn prompt_resize_highlight_supports_bottom_and_both_chrome() {
    let lua = prompt_bar_lua_fixture();

    let (bottom_only_top, bottom_only_bottom, both_top, both_bottom): (i64, i64, i64, i64) = lua
        .load(
            r#"
            local top = assert(smelt.__wins["smelt.prompt_bar.top"])
            local bottom = assert(smelt.__wins["smelt.prompt_bar.bottom"])

            local function marked_text(buf, mark)
              return string.sub(buf._lines[mark.row] or "", mark.start_col + 1, mark.end_col)
            end
            local function count_resize_dashes(buf)
              local n = 0
              for _, mark in ipairs(buf._marks) do
                local text = marked_text(buf, mark)
                if text:find("─", 1, true) ~= nil and mark.hl_group == "SmeltResizeHandle" and mark.fg == nil then
                  n = n + 1
                end
              end
              return n
            end

            smelt.__resize = true
            smelt.__resize_chrome = "bottom"
            top:renderer()
            bottom:renderer()
            local bottom_only_top = count_resize_dashes(top:buf())
            local bottom_only_bottom = count_resize_dashes(bottom:buf())

            smelt.__resize_chrome = "both"
            top:renderer()
            bottom:renderer()
            local both_top = count_resize_dashes(top:buf())
            local both_bottom = count_resize_dashes(bottom:buf())

            return bottom_only_top, bottom_only_bottom, both_top, both_bottom
            "#,
        )
        .eval()
        .expect("render prompt resize variants");

    assert_eq!(bottom_only_top, 0);
    assert_eq!(bottom_only_bottom, 1);
    assert!(both_top >= 2);
    assert_eq!(both_bottom, 1);
}

#[test]
fn ps_details_meta_values_are_label_value_rows() {
    assert!(
        PS_LUA.contains("local label_value = smelt.label_value or require(\"smelt.label_value\")")
    );
    assert!(PS_LUA.contains("append_label_value(lines, \"command\""));
    assert!(LABEL_VALUE_LUA.contains("local separator = opts.separator or \"  \""));
    assert!(!PS_LUA.contains("key .. \":\""));
    assert!(!PS_LUA.contains("META_KEY_WIDTH"));
    assert!(!PS_LUA.contains("styled_lines ="));
}

#[test]
fn banner_press_resumes_existing_animation_instead_of_reseeding() {
    // The test inspects a Lua closure upvalue via the standard debug library.
    // It runs only trusted in-repo Lua fixtures in an isolated VM.
    let lua = unsafe { mlua::Lua::unsafe_new() };
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        paint_handlers = {}
        smelt = {
          notify = __smelt_notify_stub(nil, nil),
          reg = { new = function(fn) return { remove = fn } end },
          build = { display = "test" },
          timer = { set = function() return { remove = function() end } end },
          paint = {
            register = function()
              return {
                on = function(_, event, handler)
                  paint_handlers[event] = handler
                end,
              }
            end,
          },
          ui = {
            size = function() return { width = 80, height = 24 } end,
            layout = {
              vbox = function(items) return items end,
              leaf = function(target, opts) return { target = target, opts = opts } end,
            },
          },
          buf = {
            new = function()
              return {
                lines = function() end,
                clear_ns = function() end,
                mark = function() end,
              }
            end,
          },
          win = {
            new = function() return {} end,
            transcript = function()
              return {
                rect = function() return { height = 100 } end,
                on = function() end,
                decorate = function() return { close = function() end } end,
              }
            end,
          },
          overlay = { new = function() return { close = function() end } end },
          ns = function(name) return name end,
          transcript = { is_empty = function() return true end },
          events = { on = function() end },
          signal = __smelt_signal_stub({}, {}),
          lifecycle = {
            on_ready = function(fn) fn() end,
            on_shutdown = function() end,
          },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");

    let loader_src = BANNER_LUA.to_string();
    let loader = lua
        .create_function(move |lua, ()| lua.load(&loader_src).eval::<mlua::Value>())
        .expect("banner loader");
    let package: mlua::Table = lua.globals().get("package").expect("package table");
    let preload: mlua::Table = package.get("preload").expect("preload table");
    preload
        .set("smelt.banner", loader)
        .expect("install banner preload");

    lua.load(BANNER_PLUGIN_LUA)
        .exec()
        .expect("load banner plugin");
    let (first_tick, second_tick): (i64, i64) = lua
        .load(
            r#"
            local press = assert(paint_handlers.press)
            local release = assert(paint_handlers.release)
            local function state_for(fn)
              for i = 1, 20 do
                local name, value = debug.getupvalue(fn, i)
                if name == "state" then return value end
              end
              error("state upvalue not found")
            end
            press()
            local state = state_for(press)
            local first = state.sim.tick
            release()
            press()
            return first, state.sim.tick
            "#,
        )
        .eval()
        .expect("drive banner handlers");

    assert_eq!(first_tick, 1);
    assert_eq!(second_tick, 2);
}

#[test]
fn scroll_pills_use_committed_transcript_views() {
    assert!(
        SCROLL_PILLS_LUA.contains("smelt.transcript.watch_view(refresh)"),
        "scroll pills should observe one committed transcript view"
    );
    assert!(
        SCROLL_PILLS_LUA.contains("view:previous_block({ role = \"user\" })"),
        "top scroll pill should navigate from the committed view"
    );
    assert!(
        SCROLL_PILLS_LUA.contains("smelt.transcript.reveal(state.top_target"),
        "top scroll pill should reveal an opaque semantic target"
    );
    assert!(
        SCROLL_PILLS_LUA.contains("smelt.transcript.follow_tail()"),
        "bottom scroll pill should use the semantic tail command"
    );
    assert!(!SCROLL_PILLS_LUA.contains("signal.subscribe"));
    assert!(!SCROLL_PILLS_LUA.contains("transcript_navigation_generation"));
    assert!(!SCROLL_PILLS_LUA.contains(":scroll("));
}

fn install_scroll_pills_fixture(lua: &mlua::Lua) {
    install_explicit_api_fixtures(lua);
    lua.load(
        r#"
        local active = {}
        local event_handlers = {}
        local handlers = {}
        local win_handlers = {}
        local blocks = {}
        local previous_block_calls = 0
        local previous_block_role = nil
        local row_lookup_calls = 0
        local revealed_block = nil
        local view_handler = nil
        local transcript_win = {}
        local view = {
          window = transcript_win,
          viewport = {
            width = 30,
            height = 5,
            content_width = 29,
            scrollable = true,
            following_tail = false,
            at_top = false,
            at_bottom = false,
          },
          focused = true,
          cursor = { viewport_row = 4 },
        }
        function view:previous_block(opts)
          previous_block_calls = previous_block_calls + 1
          local role = opts and opts.role or nil
          previous_block_role = role
          for i = #blocks, 1, -1 do
            local block = blocks[i]
            if role == nil or block.role == role then return block end
          end
          return nil
        end

        smelt = {
          ns = function(name) return name end,
          text = {
            width = function(text) return #text end,
            fit = function(text) return text end,
          },
          buf = {
            new = function()
              return {
                lines = function() end,
                clear_ns = function() end,
                mark = function() end,
              }
            end,
          },
          win = {
            new = function(_, opts)
              local name = opts and opts.name or "win"
              return {
                on = function(_, event, fn)
                  win_handlers[name] = win_handlers[name] or {}
                  win_handlers[name][event] = fn
                  handlers[event] = fn
                end
              }
            end,
          },
          overlay = {
            new = function(opts)
              active[opts.name] = (active[opts.name] or 0) + 1
              local name = opts.name
              return {
                close = function()
                  active[name] = math.max((active[name] or 0) - 1, 0)
                end,
              }
            end,
          },
          ui = { layout = { leaf = function() return {} end } },
          transcript = {
            watch_view = function(fn)
              view_handler = fn
              fn(view)
              return { remove = function() end }
            end,
            reveal = function(target, opts)
              revealed_block = {
                block_id = target.block_id,
                align = opts and opts.align or nil,
                top_padding = opts and opts.top_padding or nil,
                move_cursor = opts and opts.move_cursor or nil,
              }
              return true
            end,
            follow_tail = function()
              view.viewport.following_tail = true
              view.viewport.at_bottom = true
              if view_handler then view_handler(view) end
            end,
            block_before_or_at_row = function()
              row_lookup_calls = row_lookup_calls + 1
              error("scroll pill should not route semantic navigation through row lookup")
            end,
          },
          events = {
            on = function(name, fn) event_handlers[name] = fn end,
          },
        }

        function __active(name) return (active[name] or 0) > 0 end
        function __set_cursor(row) view.cursor = { viewport_row = row - 10 } end
        function __set_focus(value) view.focused = value == "transcript" end
        function __set_blocks(value) blocks = value end
        function __set_scroll(value)
          view.viewport.height = value.viewport
          view.viewport.scrollable = value.overflow
          view.viewport.following_tail = value.follow
          view.viewport.at_top = value.at_top
          view.viewport.at_bottom = value.at_bottom
        end
        function __previous_block_calls() return previous_block_calls end
        function __previous_block_role() return previous_block_role end
        function __row_lookup_calls() return row_lookup_calls end
        function __revealed_block() return revealed_block end
        function __event(name)
          if name == "scrolled" or name == "focus" or name == "blur" then
            assert(view_handler, "view_handler")(view)
          else
            assert(handlers[name], name)()
          end
        end
        function __win_event(win, event) assert(win_handlers[win] and win_handlers[win][event], win .. ":" .. event)() end
        function __publish() assert(view_handler, "view_handler")(view) end
        "#,
    )
    .exec()
    .expect("install fake smelt api");

    lua.load(SCROLL_PILLS_LUA)
        .exec()
        .expect("load scroll pills plugin");
}

#[test]
fn scroll_pills_hide_when_transcript_cursor_is_under_them() {
    let lua = mlua::Lua::new();
    install_scroll_pills_fixture(&lua);

    let bottom_under_cursor: bool = lua
        .load(r#"return __active("smelt.scroll_pills.bottom")"#)
        .eval()
        .expect("read bottom overlay state");
    assert!(!bottom_under_cursor);

    let (
        top_under_cursor,
        top_after_cursor_move,
        previous_block_calls,
        previous_block_role,
        row_lookup_calls,
        revealed_block_id,
        reveal_align,
        reveal_top_padding,
        reveal_moves_cursor,
    ): (bool, bool, i64, String, i64, i64, String, i64, bool) = lua
        .load(
            r#"
            __set_blocks({ { block_id = 1, role = "user", first_line = "previous message" } })
            __set_cursor(10)
            __event("scrolled")
            local under = __active("smelt.scroll_pills.top")
            __set_cursor(11)
            __publish("cursor_pos")
            __event("press")
            local revealed = __revealed_block()
            return under,
              __active("smelt.scroll_pills.top"),
              __previous_block_calls(),
              __previous_block_role(),
              __row_lookup_calls(),
              revealed.block_id,
              revealed.align,
              revealed.top_padding,
              revealed.move_cursor
            "#,
        )
        .eval()
        .expect("drive top pill refresh");
    assert!(!top_under_cursor);
    assert!(top_after_cursor_move);
    assert!(previous_block_calls > 0);
    assert_eq!(previous_block_role, "user");
    assert_eq!(row_lookup_calls, 0);
    assert_eq!(revealed_block_id, 1);
    assert_eq!(reveal_align, "top");
    assert_eq!(reveal_top_padding, 1);
    assert!(!reveal_moves_cursor);

    let (bottom_after_blur, bottom_after_focus): (bool, bool) = lua
        .load(
            r#"
            __set_cursor(14)
            __event("scrolled")
            __set_focus("prompt")
            __event("blur")
            local after_blur = __active("smelt.scroll_pills.bottom")
            __set_focus("transcript")
            __event("focus")
            return after_blur, __active("smelt.scroll_pills.bottom")
            "#,
        )
        .eval()
        .expect("drive bottom pill focus refresh");
    assert!(bottom_after_blur);
    assert!(!bottom_after_focus);

    let (top_before_bottom_press, bottom_after_bottom_press, top_after_bottom_press): (
        bool,
        bool,
        bool,
    ) = lua
        .load(
            r#"
            local top_before = __active("smelt.scroll_pills.top")
            __win_event("smelt.scroll_pills.bottom.win", "press")
            return top_before,
              __active("smelt.scroll_pills.bottom"),
              __active("smelt.scroll_pills.top")
            "#,
        )
        .eval()
        .expect("press bottom pill");
    assert!(top_before_bottom_press);
    assert!(!bottom_after_bottom_press);
    assert!(top_after_bottom_press);
}

#[test]
fn scroll_pills_refresh_top_on_committed_view_change() {
    let lua = mlua::Lua::new();
    install_scroll_pills_fixture(&lua);

    let top_after_history_append: bool = lua
        .load(
            r#"
            __set_cursor(11)
            __set_blocks({ { block_id = 2, role = "user", first_line = "new previous" } })
            __publish()
            return __active("smelt.scroll_pills.top")
            "#,
        )
        .eval()
        .expect("drive committed-view top pill");
    assert!(top_after_history_append);
}

#[test]
fn scroll_pills_hide_top_at_document_start() {
    let lua = mlua::Lua::new();
    install_scroll_pills_fixture(&lua);

    let top_at_document_start: bool = lua
        .load(
            r#"
            __set_blocks({ { block_id = 1, role = "user", first_line = "first message" } })
            __set_scroll({
              viewport = 5,
              overflow = true,
              follow = false,
              at_top = true,
              at_bottom = false,
            })
            __publish()
            return __active("smelt.scroll_pills.top")
            "#,
        )
        .eval()
        .expect("drive document-start top pill refresh");
    assert!(!top_at_document_start);
}

#[test]
fn scroll_pills_hide_bottom_when_already_at_bottom() {
    let lua = mlua::Lua::new();
    install_scroll_pills_fixture(&lua);

    let bottom_at_bottom_without_follow: bool = lua
        .load(
            r#"
            __set_focus("prompt")
            __set_cursor(12)
            __set_scroll({
              top = 25,
              viewport = 5,
              total = 30,
              max = 25,
              overflow = true,
              follow = false,
              at_top = false,
              at_bottom = true,
              needs_tail_repin = false,
            })
            __event("scrolled")
            return __active("smelt.scroll_pills.bottom")
            "#,
        )
        .eval()
        .expect("drive at-bottom pinned bottom pill refresh");
    assert!(!bottom_at_bottom_without_follow);
}

#[test]
fn turn_notifications_are_disabled_by_default() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = {},
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, {}),
          session = {
            title = { get = function() return "" end },
            slug = { get = function() return "" end },
          },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = false } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function(message, opts) _G.osc9 = { message = message, opts = opts }; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (bell_count, osc9_present, notifications_type): (i64, bool, String) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = false })
            return _G.bell_count or 0, _G.osc9 ~= nil, type(smelt.settings.notifications)
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(bell_count, 0);
    assert!(!osc9_present);
    assert_eq!(notifications_type, "nil");
}

#[test]
fn turn_notifications_choose_osc9_for_supported_terminals() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = { notifications = { turn_end = true } },
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, { task_label = "task-1" }),
          session = {
            title = { get = function() return "Session title" end },
            slug = { get = function() return "slug" end },
          },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = true } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function(message, opts) _G.osc9 = { message = message, opts = opts }; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (message, dcs_passthrough, bell_count): (String, bool, i64) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = false })
            return _G.osc9.message, _G.osc9.opts.dcs_passthrough, _G.bell_count or 0
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(message, "smelt turn complete: Session title");
    assert!(dcs_passthrough);
    assert_eq!(bell_count, 0);
}

#[test]
fn turn_notifications_support_one_shot_and_session_overrides() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = { notifications = { turn_end = false } },
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, {}),
          session = {
            title = { get = function() return "" end },
            slug = { get = function() return "" end },
          },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = false } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function(message, opts) _G.osc9_count = (_G.osc9_count or 0) + 1; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (before, first, second, suppressed, third, cleared): (i64, i64, i64, bool, i64, bool) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = false })
            before = _G.osc9_count or 0
            smelt.notifications.enable_once()
            handlers.turn_end({ cancelled = false })
            first = _G.osc9_count or 0
            handlers.turn_end({ cancelled = false })
            second = _G.osc9_count or 0
            smelt.notifications.enable_session()
            handlers.turn_end({ cancelled = false })
            third = _G.osc9_count or 0
            smelt.notifications.disable_session()
            suppressed = smelt.notifications.status().suppressed
            smelt.notifications.clear_session()
            return before, first, second, suppressed, third, smelt.notifications.status().enabled
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(before, 0);
    assert_eq!(first, 1);
    assert_eq!(second, 1);
    assert!(suppressed);
    assert_eq!(third, 2);
    assert!(!cleared);
}

#[test]
fn notify_command_controls_transient_notifications() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local notices = {}
        local errors = {}
        smelt = {
          __commands = {},
          __notices = notices,
          __errors = errors,
          cmd = { register = function(name, fn, opts) smelt.__commands[name] = { fn = fn, opts = opts } end },
          notify = __smelt_notify_stub(notices, errors),
          settings = { notifications = { turn_end = true } },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = false } end,
            bell = function() return true end,
            osc9_notify = function() return true end,
          },
        }
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(NOTIFY_COMMAND_LUA)
        .exec()
        .expect("load notify command");

    let (once, session, suppressed, cleared, desc, err): (bool, bool, bool, String, String, String) = lua
        .load(
            r#"
            smelt.__commands.notify.fn()
            once = smelt.notifications.status().once
            smelt.__commands.notify.fn("on")
            session = smelt.notifications.status().session
            smelt.__commands.notify.fn("off")
            suppressed = smelt.notifications.status().suppressed
            smelt.__commands.notify.fn("clear")
            cleared = smelt.notifications.status().mode
            smelt.__commands.notify.fn("bad")
            return once, session, suppressed, cleared, smelt.__commands.notify.opts.desc, smelt.__errors[1] or ""
            "#,
        )
        .eval()
        .expect("run notify command");

    assert!(once);
    assert!(session);
    assert!(suppressed);
    assert_eq!(cleared, "config");
    assert_eq!(desc, "override turn-end notifications for this session");
    assert_eq!(err, "usage: /notify [once|on|off|clear|status]");
}

#[test]
fn turn_notifications_session_override_can_suppress_config() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = { notifications = { turn_end = true } },
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, {}),
          session = {
            title = { get = function() return "" end },
            slug = { get = function() return "" end },
          },
          terminal = {
            info = function() return { term_program = "WezTerm", tmux = false } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function() _G.osc9_count = (_G.osc9_count or 0) + 1; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (first, suppressed, once, cleared): (i64, i64, i64, i64) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = false })
            first = _G.osc9_count or 0
            smelt.notifications.disable_session()
            handlers.turn_end({ cancelled = false })
            suppressed = _G.osc9_count or 0
            smelt.notifications.enable_once()
            handlers.turn_end({ cancelled = false })
            once = _G.osc9_count or 0
            smelt.notifications.clear()
            handlers.turn_end({ cancelled = false })
            cleared = _G.osc9_count or 0
            return first, suppressed, once, cleared
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(first, 1);
    assert_eq!(suppressed, 1);
    assert_eq!(once, 2);
    assert_eq!(cleared, 3);
}

#[test]
fn turn_notifications_fall_back_to_bel_and_skip_cancelled_or_retrying_turns() {
    let lua = mlua::Lua::new();
    install_explicit_api_fixtures(&lua);
    lua.load(
        r#"
        local handlers = {}
        smelt = {
          settings = { notifications = { turn_end = true } },
          events = { on = function(name, fn) handlers[name] = fn end },
          signal = __smelt_signal_stub({}, {}),
          session = {
            title = { get = function() return "" end },
            slug = { get = function() return "" end },
          },
          terminal = {
            info = function() return { term = "vt100", tmux = false } end,
            bell = function() _G.bell_count = (_G.bell_count or 0) + 1; return true end,
            osc9_notify = function() _G.osc9_count = (_G.osc9_count or 0) + 1; return true end,
          },
          text = { truncate = function(s) return s end },
        }
        _G.handlers = handlers
        "#,
    )
    .exec()
    .expect("install fake smelt api");
    install_notifications_preload(&lua);
    lua.load(TURN_NOTIFICATIONS_LUA)
        .exec()
        .expect("load turn notifications plugin");

    let (bell_count, osc9_count): (i64, i64) = lua
        .load(
            r#"
            handlers.turn_end({ cancelled = true })
            handlers.turn_end({ retry_at_ms = 123 })
            handlers.turn_end({ continuation_token = 9 })
            handlers.turn_end({ cancelled = false })
            return _G.bell_count or 0, _G.osc9_count or 0
            "#,
        )
        .eval()
        .expect("fire turn_end");

    assert_eq!(bell_count, 2);
    assert_eq!(osc9_count, 0);
}
