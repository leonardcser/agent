use super::*;
use crate::smelt_edit::Rect;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

fn paint(app: &mut TestApp) {
    for _ in 0..3 {
        app.render_silent();
        app.dispatch_ui_window_events(false);
    }
}

fn window(app: &TestApp, name: &str) -> WinId {
    app.ui_probe().named_win(name).unwrap()
}

fn rect(app: &TestApp, name: &str) -> Rect {
    app.ui_probe()
        .win(window(app, name))
        .unwrap()
        .viewport
        .unwrap()
        .rect
}

fn mouse(app: &mut TestApp, kind: MouseEventKind, row: u16, col: u16) {
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind,
        row,
        column: col,
        modifiers: KeyModifiers::NONE,
    })));
    paint(app);
}

fn chord(app: &mut TestApp, key: char) {
    app.press_mod(KeyCode::Char('w'), KeyModifiers::CONTROL);
    app.type_char(key);
    paint(app);
}

fn open(app: &mut TestApp, placement: &str) {
    app.set_terminal_size(140, 44);
    app.run_lua_result(r#"
        local layout = smelt.ui.layout
        local function window(name)
            local buf = smelt.buf.new()
            buf:lines({'one', 'two', 'three', 'four', 'five'})
            return smelt.win.new(buf, { name = name, surface = 'readonly_text', vim_enabled = true, wrap = false })
        end
        a, b, c = window('resize.a'), window('resize.b'), window('resize.c')
        split_layout = layout.hsplit(layout.leaf(a),
            layout.vsplit(layout.leaf(b), layout.leaf(c), { size = '50%', min_first = 4, min_second = 5 }),
            { size = 40, min_first = 15, min_second = 18 })
        local windows = layout.windows(split_layout)
        assert(#windows == 3 and tostring(windows[1]) == tostring(a) and tostring(windows[3]) == tostring(c))
    "#).unwrap();
    match placement {
        "root" => app.run_lua_result("smelt.ui.layout.set(function() return split_layout end)").unwrap(),
        "overlay" => app.run_lua_result("surface = smelt.overlay.new({layout = split_layout, anchor = 'center', width = 120, height = 30, border = 'none'})").unwrap(),
        "dialog" => app.run_lua_result("surface = smelt.dialog.new({layout = split_layout, height = 28, focus = a})").unwrap(),
        _ => unreachable!(),
    }
    drive_lua_tasks(app);
    paint(app);
    app.run_lua_result("a:focus()").unwrap();
    paint(app);
}

#[test]
fn split_layout_resolution_storage_scales_linearly() {
    smelt_perf::alloc::enable();
    use smelt_term::{Axis, LayoutTree, NoopSizer, PaintId, Split, SplitOptions};
    fn allocated(count: u64) -> u64 {
        let mut tree = LayoutTree::leaf(PaintId(1));
        for index in 2..=count {
            tree = LayoutTree::split(
                Split::new(Axis::Horizontal, SplitOptions::default()),
                tree,
                LayoutTree::leaf(PaintId(index)),
            );
        }
        let before = smelt_perf::alloc::thread_snapshot().1;
        let resolved = tree.resolve(Rect::new(0, 0, 200, 40), &NoopSizer);
        let bytes = smelt_perf::alloc::thread_snapshot().1 - before;
        assert_eq!(resolved.leaves().count(), count as usize);
        bytes
    }
    let small = allocated(256);
    let large = allocated(1024);
    assert!(
        large < small * 5,
        "superlinear layout allocations: {small} -> {large}"
    );
    assert!(
        large < 2_000_000,
        "1024-pane resolution allocated {large} bytes"
    );
}

#[test]
fn retained_splits_reject_invalid_minima_misplaced_options_and_duplicate_mounts() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(
        r#"
        local layout = smelt.ui.layout
        local a = smelt.win.new(smelt.buf.new())
        local b = smelt.win.new(smelt.buf.new())
        local c = smelt.win.new(smelt.buf.new())
        for _, value in ipairs({-1, 65536, 1.5, '2'}) do
            assert(not pcall(layout.split, 'horizontal', {min_first = value}))
            assert(not pcall(layout.hsplit, layout.leaf(a), layout.leaf(b), {min_second = value}))
        end
        for _, options in ipairs({{border = 'none'}, {padding = 1}, {title = 'title'}}) do
            assert(not pcall(layout.split, 'horizontal', options))
        end
        local split = layout.split('horizontal', {min_first = 0, min_second = 65535})
        assert(not pcall(function() split:layout(layout.leaf(a), layout.leaf(b), {size = 20}) end))
        local duplicate = layout.hsplit(split:layout(layout.leaf(a), layout.leaf(b)),
            split:layout(layout.leaf(a), layout.leaf(c)))
        assert(#layout.windows(duplicate) == 3)
        local ok, err = pcall(smelt.overlay.new, {layout = duplicate})
        assert(not ok and tostring(err):find('only once'), tostring(err))
    "#,
    )
    .unwrap();
}

#[test]
fn retained_splits_reject_cross_surface_mounts_and_allow_named_refresh() {
    for placement in ["overlay", "root", "dialog", "decoration"] {
        let mut app = TestApp::builder().build();
        app.run_lua_result(r#"
            layout = smelt.ui.layout
            a = smelt.win.new(smelt.buf.new())
            b = smelt.win.new(smelt.buf.new())
            c = smelt.win.new(smelt.buf.new())
            d = smelt.win.new(smelt.buf.new())
            split = layout.split('horizontal', {size = 20})
            function pair(first, second) return split:layout(layout.leaf(first), layout.leaf(second)) end
            surface = smelt.overlay.new({name = 'shared-split', layout = pair(a, b)})
            refreshed = smelt.overlay.new({name = 'shared-split', layout = pair(a, b)})
            assert(tostring(surface) == tostring(refreshed))
        "#).unwrap();
        let mount = match placement {
            "overlay" => "smelt.overlay.new({layout = pair(c, d)})",
            "root" => "smelt.ui.layout.set(function() return pair(c, d) end)",
            "dialog" => "smelt.dialog.new({layout = pair(c, d), focus = c})",
            "decoration" => "a:decorate({layout = pair(c, d)})",
            _ => unreachable!(),
        };
        if placement == "root" {
            app.clear_lua_messages();
            app.run_lua_result(mount).unwrap();
            paint(&mut app);
            assert!(
                app.lua_probe()
                    .core_shared()
                    .messages
                    .lock()
                    .unwrap()
                    .entries()
                    .iter()
                    .any(|entry| entry.full.contains("only once")),
                "{placement}"
            );
            app.run_lua_result("smelt.ui.layout.set(nil)").unwrap();
        } else {
            app.run_lua_result(&format!(
                "local ok, err = pcall(function() {mount} end); assert(not ok and tostring(err):find('only once'), tostring(err))"
            )).unwrap();
        }
        app.run_lua_result("surface:close(); remounted = smelt.overlay.new({layout = pair(c, d)})")
            .unwrap();
    }
}

#[test]
fn retained_splits_reject_duplicate_mounts_inside_dialog_stages() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(
        r#"
        local layout = smelt.ui.layout
        local a = smelt.win.new(smelt.buf.new())
        local b = smelt.win.new(smelt.buf.new())
        local c = smelt.win.new(smelt.buf.new())
        local split = layout.split('horizontal')
        smelt.dialog.new({layout = split:layout(layout.leaf(a), layout.leaf(b)), focus = a})
        smelt.ui.layout.set(function(state)
            return split:layout(state.dialog, layout.leaf(c))
        end)
    "#,
    )
    .unwrap();
    paint(&mut app);
    assert!(app
        .lua_probe()
        .core_shared()
        .messages
        .lock()
        .unwrap()
        .entries()
        .iter()
        .any(|entry| entry.full.contains("only once")));
}

#[test]
fn resizable_layouts_compose_in_root_overlay_and_dialog() {
    for placement in ["root", "overlay", "dialog"] {
        let mut app = TestApp::builder().build();
        open(&mut app, placement);
        let initial = rect(&app, "resize.a");
        let right = rect(&app, "resize.b");
        chord(&mut app, '>');
        assert_eq!(
            rect(&app, "resize.a").width,
            initial.width + 4,
            "{placement}"
        );
        assert_eq!(rect(&app, "resize.b").width, right.width - 4, "{placement}");
        assert_eq!(app.ui_probe().focus(), Some(window(&app, "resize.a")));
        chord(&mut app, '<');
        assert_eq!(rect(&app, "resize.a"), initial);

        app.run_lua_result("b:focus()").unwrap();
        chord(&mut app, '>');
        assert_eq!(rect(&app, "resize.b").width, right.width + 4, "{placement}");
        let before = rect(&app, "resize.b");
        chord(&mut app, '+');
        assert_eq!(
            rect(&app, "resize.b").height,
            before.height + 1,
            "{placement}"
        );
        chord(&mut app, '-');
        assert_eq!(rect(&app, "resize.b").height, before.height);
        app.run_lua_result("assert(c:resize('height', 2))").unwrap();
        paint(&mut app);
        assert_eq!(rect(&app, "resize.b").height, before.height - 2);

        chord(&mut app, '=');
        let left = rect(&app, "resize.a");
        let upper = rect(&app, "resize.b");
        let lower = rect(&app, "resize.c");
        assert!(left.width.abs_diff(upper.width) <= 1);
        assert!(upper.height.abs_diff(lower.height) <= 1);
        let border = upper.top + upper.height;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            border,
            upper.left + 3,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            border + 3,
            upper.left + 3,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            border + 3,
            upper.left + 3,
        );
        assert_eq!(rect(&app, "resize.b").height, upper.height + 3);
        assert_eq!(rect(&app, "resize.c").height, lower.height - 3);
        assert_eq!(app.ui_probe().focus(), Some(window(&app, "resize.b")));
        assert!(app.ui_probe().capture().is_none());
    }
}

#[test]
fn split_drag_clamps_and_keyboard_cancels_capture() {
    let mut app = TestApp::builder().build();
    open(&mut app, "overlay");
    let left = rect(&app, "resize.a");
    let col = left.left + left.width;
    mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        left.top + 3,
        col,
    );
    mouse(
        &mut app,
        MouseEventKind::Drag(MouseButton::Left),
        left.top + 3,
        u16::MAX,
    );
    assert_eq!(rect(&app, "resize.b").width, 18);
    mouse(
        &mut app,
        MouseEventKind::Drag(MouseButton::Left),
        left.top + 3,
        0,
    );
    assert_eq!(rect(&app, "resize.a").width, 15);
    chord(&mut app, '=');
    assert!(app.ui_probe().capture().is_none());
    let balanced = rect(&app, "resize.a");
    mouse(
        &mut app,
        MouseEventKind::Drag(MouseButton::Left),
        left.top + 3,
        u16::MAX,
    );
    assert_eq!(rect(&app, "resize.a"), balanced);
    mouse(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        left.top + 3,
        u16::MAX,
    );

    let col = balanced.left + balanced.width;
    mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        balanced.top + 3,
        col,
    );
    assert!(app.ui_probe().capture().is_some());
    app.run_lua_result("surface:close()").unwrap();
    paint(&mut app);
    assert!(app.ui_probe().capture().is_none());
}

#[test]
fn retained_lua_split_controls_survive_replacing_children_and_restore_preferences() {
    let mut app = TestApp::builder().build();
    open(&mut app, "root");
    app.run_lua_result(r#"
        local layout = smelt.ui.layout
        retained = layout.split('horizontal', {size = 40, resize = 'cells', min_first = 15, min_second = 18,
            divider = {normal = {fg = '#123456', bg = '#654321'}, active = {fg = '#abcdef'}}})
        child = b
        smelt.ui.layout.set(function()
            return retained:layout(layout.leaf(a), layout.leaf(child))
        end)
    "#).unwrap();
    paint(&mut app);
    app.run_lua_result("assert(retained:resize(10)); assert(retained:size() == 50); child = c; smelt.ui.layout.invalidate()")
        .unwrap();
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 50);
    assert_eq!(rect(&app, "resize.c").width, 89);
    app.set_terminal_size(30, 14);
    paint(&mut app);
    app.run_lua_result("assert(retained:size() == 50)").unwrap();
    app.set_terminal_size(140, 44);
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 50);
    app.run_lua_result("assert(retained:equalize()); assert(type(retained:size()) == 'number'); assert(retained:reset()); assert(retained:size() == 40); assert(retained:set_size('ratio:1/3')); saved = retained:size(); assert(saved == 'ratio:1/3')")
        .unwrap();
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 46);
    app.run_lua_result("assert(retained:set_size(60)); assert(retained:set_size(saved)); assert(not retained:set_size(saved)); assert(not pcall(function() retained:set_size('ratio:2/1') end))")
        .unwrap();
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 46);
}

#[test]
fn split_ratio_survives_composer_rebuild_and_terminal_resize() {
    let mut app = TestApp::builder().build();
    open(&mut app, "root");
    app.run_lua_result("assert(a:resize('width', 10))").unwrap();
    paint(&mut app);
    let initial = rect(&app, "resize.a");
    app.run_lua_result("smelt.ui.layout.invalidate()").unwrap();
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a"), initial);
    app.set_terminal_size(30, 14);
    paint(&mut app);
    assert!(rect(&app, "resize.a").width > 0);
    assert!(rect(&app, "resize.b").width > 0);
    app.set_terminal_size(140, 44);
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a"), initial);
}

#[test]
fn window_resize_uses_overlay_bounds_without_a_split() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(140, 44);
    app.run_lua_result(r#"
        a = smelt.win.new(smelt.buf.new(), {name = 'resize.a', surface = 'readonly_text'})
        surface = smelt.overlay.new({layout = smelt.ui.layout.leaf(a), anchor = 'center', width = 60, height = 16,
            min_width = 30, max_width = 90, min_height = 8, max_height = 24})
        a:focus()
    "#).unwrap();
    paint(&mut app);
    let before = rect(&app, "resize.a");
    chord(&mut app, '>');
    assert_eq!(rect(&app, "resize.a").width, before.width + 4);
    app.run_lua_result("assert(a:resize('width', 2147483647)); assert(a:resize('height', -2147483648)); assert(not a:equalize())").unwrap();
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 90 - (60 - before.width));
    assert_eq!(rect(&app, "resize.a").height, 8 - (16 - before.height));
    assert_eq!(app.ui_probe().focus(), Some(window(&app, "resize.a")));
}

#[test]
fn fit_sized_split_honors_declared_size_and_minima() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(140, 44);
    app.run_lua_result(r#"
        a = smelt.win.new(smelt.buf.new(), {name = 'resize.a', surface = 'readonly_text', wrap = false})
        b = smelt.win.new(smelt.buf.new(), {name = 'resize.b', surface = 'readonly_text', wrap = false})
        local layout = smelt.ui.layout
        surface = smelt.overlay.new({
            layout = layout.hsplit(layout.leaf(a), layout.leaf(b), {size = 40, min_first = 15, min_second = 18}),
            anchor = 'center', height = 12, border = 'none'
        })
    "#).unwrap();
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 40);
    assert_eq!(rect(&app, "resize.b").width, 18);
    app.run_lua_result("a:equalize()").unwrap();
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 29);
    assert_eq!(rect(&app, "resize.b").width, 29);
}

#[test]
fn split_options_and_dialog_composition_are_validated() {
    let mut app = TestApp::builder().build();
    open(&mut app, "overlay");
    app.run_lua_result(
        r#"
        local layout = smelt.ui.layout
        local first, second = layout.leaf(a), layout.leaf(b)
        for _, size in ipairs({'fit', 'fill', 'ratio:1/0', 'min:5', -1, 65536, 1.5, '101%', 'ratio:2/1'}) do
            assert(not pcall(layout.hsplit, first, second, {size = size}), tostring(size))
        end
        assert(not pcall(layout.hsplit, first, second, {resize = 'unknown'}))
        assert(not pcall(function() a:resize('depth', 1) end))
        assert(not pcall(smelt.dialog.new, {layout = split_layout, panels = {}}))
        assert(not pcall(smelt.dialog.new, {layout = split_layout, bottom_panels = {}}))
        assert(not pcall(smelt.dialog.new, {layout = layout.vbox({})}))
        assert(#layout.windows(layout.hsplit(first, first)) == 1)
    "#,
    )
    .unwrap();
}

#[test]
fn composed_dialog_fits_its_body_and_resizes_without_a_matching_split() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(140, 44);
    app.run_lua_result(r#"
        local layout = smelt.ui.layout
        local buf = smelt.buf.new()
        buf:lines({'one', 'two', 'three', 'four', 'five'})
        a = smelt.win.new(buf, {name = 'resize.a', surface = 'readonly_text'})
        b = smelt.win.new(smelt.buf.new(), {name = 'resize.b', surface = 'readonly_text'})
        surface = smelt.dialog.new({layout = layout.hsplit(layout.leaf(a), layout.leaf(b)), focus = a})
    "#).unwrap();
    drive_lua_tasks(&mut app);
    paint(&mut app);
    let before = rect(&app, "resize.a");
    assert_eq!(before.height, 5);
    chord(&mut app, '+');
    assert_eq!(rect(&app, "resize.a").height, before.height + 1);
    assert_eq!(app.ui_probe().focus(), Some(window(&app, "resize.a")));
}

#[test]
fn frames_preserve_child_chrome_and_natural_size_while_stretching() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(140, 44);
    app.run_lua_result(
        r#"
        local layout = smelt.ui.layout
        a = smelt.win.new(smelt.buf.new(), {name = 'resize.a', surface = 'readonly_text'})
        local body = layout.frame(layout.leaf(a, {border = 'single', measure = {10, 5}}),
            {border = 'single', padding = 1, title = 'frame'})
        surface = smelt.overlay.new({layout = body, anchor = 'center', border = 'none'})
        a:focus()
        assert(#layout.windows(body) == 1)
    "#,
    )
    .unwrap();
    paint(&mut app);
    let before = rect(&app, "resize.a");
    assert_eq!((before.width, before.height), (10, 5));
    chord(&mut app, '>');
    chord(&mut app, '+');
    let after = rect(&app, "resize.a");
    assert_eq!((after.width, after.height), (14, 6));
}

#[test]
fn fixed_cell_split_restores_user_width_after_terminal_clamping() {
    let mut app = TestApp::builder().build();
    open(&mut app, "root");
    app.run_lua_result(
        r#"
        local layout = smelt.ui.layout
        local body = layout.hsplit(layout.leaf(a), layout.leaf(b),
            {size = 40, min_first = 15, min_second = 18, resize = 'cells'})
        smelt.ui.layout.set(function() return body end)
    "#,
    )
    .unwrap();
    paint(&mut app);
    chord(&mut app, '>');
    assert_eq!(rect(&app, "resize.a").width, 44);
    app.set_terminal_size(40, 44);
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 21);
    app.set_terminal_size(140, 44);
    paint(&mut app);
    assert_eq!(rect(&app, "resize.a").width, 44);
}

#[test]
fn closed_windows_cannot_resize_retained_layouts() {
    let mut app = TestApp::builder().build();
    open(&mut app, "root");
    app.run_lua_result("a:close(); assert(not a:resize('width', 4)); assert(not a:equalize())")
        .unwrap();
}

#[test]
fn removing_a_captured_split_restores_content_dragging() {
    let mut app = TestApp::builder().build();
    open(&mut app, "root");
    let initial = rect(&app, "resize.a");
    mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        initial.top + 2,
        initial.left + initial.width,
    );
    assert!(app.ui_probe().capture().is_some());
    app.run_lua_result("smelt.ui.layout.set(function() return smelt.ui.layout.leaf(a) end)")
        .unwrap();
    paint(&mut app);
    assert!(app.ui_probe().capture().is_none());
    let area = rect(&app, "resize.a");
    mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        area.top,
        area.left,
    );
    mouse(
        &mut app,
        MouseEventKind::Drag(MouseButton::Left),
        area.top + 3,
        area.left + 2,
    );
    assert!(app
        .ui_probe()
        .win(window(&app, "resize.a"))
        .unwrap()
        .drag_active());
    mouse(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        area.top + 3,
        area.left + 2,
    );
}
