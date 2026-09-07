use super::*;
use std::sync::Arc;

fn preview(app: &TestApp) -> WinId {
    app.ui_probe()
        .named_win("smelt.diff.preview")
        .expect("diff preview")
}

fn open_fixture(app: &mut TestApp, lines: usize) {
    assert!(app.run_lua(&format!(r#"
        local patch = 'diff --git a/first.rs b/first.rs\n@@ -0,0 +1,{lines} @@\n'
          .. string.rep('+abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789\n', {lines})
          .. 'diff --git a/last.rs b/last.rs\n@@ -1 +1 @@\n-old\n+last change\n'
        smelt.git.diff = function() return {{ branch = 'feature/test', root = '.', diff = smelt.diff.parse(patch) }} end
        smelt.cmd.run('diff')
    "#)));
    drive_lua_tasks(app);
    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("first.rs"),
        "{frame}\nLua messages: {:?}",
        app.app.lua.core_shared().messages.lock().unwrap().entries()
    );
    assert!(!frame.contains("feature/test"), "{frame}");
    assert!(app.ui_probe().named_win("smelt.diff.header").is_none());
    assert!(app.ui_probe().named_win("smelt.diff.footer").is_none());
    assert_eq!(
        app.ui_probe()
            .win(preview(app))
            .unwrap()
            .row_source()
            .unwrap()
            .snapshot()
            .total_rows,
        lines as u64 + 8
    );
}

#[test]
fn diff_plugin_continuous_navigation_horizontal_scroll_sidebar_and_close() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    open_fixture(&mut app, 1000);
    let win = preview(&app);
    let total = app
        .ui_probe()
        .win(win)
        .unwrap()
        .row_source()
        .unwrap()
        .snapshot()
        .total_rows;
    assert_eq!(total, 1008);
    let buf = app.ui_probe().win(win).unwrap().buf;
    assert!(app.ui_probe().buf(buf).unwrap().line_count() < 30);

    app.type_char('G');
    let frame = app.render_to_frame().text();
    assert!(frame.contains("last change"), "{frame}");
    assert!(app.ui_probe().win(win).unwrap().scroll_top() > 980);
    app.dispatch_ui_window_events(false);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_row(), 2);
    app.type_text("gg");
    app.render_silent();
    app.dispatch_ui_window_events(false);
    assert_eq!(app.ui_probe().win(win).unwrap().scroll_top(), 0);
    app.type_char('L');
    app.render_silent();
    assert_eq!(app.ui_probe().win(win).unwrap().scroll_left, 4);
    app.type_char('H');
    app.render_silent();
    assert_eq!(app.ui_probe().win(win).unwrap().scroll_left, 0);
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    app.type_char('j');
    let frame = app.render_to_frame().text();
    assert!(frame.contains("last change"), "{frame}");
    app.press(KeyCode::Esc);
    assert!(app.ui_probe().named_win("smelt.diff.preview").is_none());
}

fn click_diff_row(app: &mut TestApp, win: WinId, row: u16) {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let rect = app.ui_probe().win(win).unwrap().viewport.unwrap().rect;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        // Match the terminal loop: observe native focus before flushing Lua callbacks.
        app.app.dispatch_terminal_event(Event::Mouse(MouseEvent {
            kind,
            row: rect.top + row,
            column: rect.left + 3,
            modifiers: KeyModifiers::NONE,
        }));
        app.dispatch_ui_window_events(false);
        app.render_silent();
    }
    app.render_silent();
}

fn drag_diff_divider(app: &mut TestApp, delta: i16) {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let rect = app
        .ui_probe()
        .win(preview(app))
        .unwrap()
        .viewport
        .unwrap()
        .rect;
    for (kind, delta) in [
        (MouseEventKind::Down(MouseButton::Left), 0),
        (MouseEventKind::Drag(MouseButton::Left), delta),
        (MouseEventKind::Up(MouseButton::Left), delta),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: rect.top + 4,
            column: (rect.left - 1).saturating_add_signed(delta),
            modifiers: KeyModifiers::NONE,
        })));
        paint_diff(app);
    }
}

#[test]
fn diff_split_drag_preserves_selection_and_scroll() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(140, 32);
    open_fixture(&mut app, 1000);
    let win = preview(&app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    app.type_text("500G");
    app.type_char('L');
    paint_diff(&mut app);
    let width = app
        .ui_probe()
        .win(sidebar)
        .unwrap()
        .viewport
        .unwrap()
        .rect
        .width;
    let cursor = app.ui_probe().win(win).unwrap().cursor_abs_row();
    let scroll = app.ui_probe().win(win).unwrap().scroll_top();
    let selected = app.ui_probe().win(sidebar).unwrap().cursor_abs_row();
    drag_diff_divider(&mut app, 12);
    assert_eq!(
        app.ui_probe()
            .win(sidebar)
            .unwrap()
            .viewport
            .unwrap()
            .rect
            .width,
        width + 12,
        "dragging the shared border should grow the sidebar"
    );
    assert_eq!(app.ui_probe().focus(), Some(win));
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), cursor);
    assert_eq!(app.ui_probe().win(win).unwrap().scroll_top(), scroll);
    assert_eq!(app.ui_probe().win(win).unwrap().scroll_left, 4);
    assert_eq!(
        app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
        selected
    );
    for focus in [sidebar, win] {
        if app.ui_probe().focus() != Some(focus) {
            app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
        }
        let before = app
            .ui_probe()
            .win(focus)
            .unwrap()
            .viewport
            .unwrap()
            .rect
            .width;
        app.press_mod(KeyCode::Char('w'), KeyModifiers::CONTROL);
        app.type_char('>');
        paint_diff(&mut app);
        assert_eq!(
            app.ui_probe()
                .win(focus)
                .unwrap()
                .viewport
                .unwrap()
                .rect
                .width,
            before + 4
        );
        assert_eq!(app.ui_probe().focus(), Some(focus));
        app.press_mod(KeyCode::Char('w'), KeyModifiers::CONTROL);
        app.type_char('<');
        paint_diff(&mut app);
        assert_eq!(
            app.ui_probe()
                .win(focus)
                .unwrap()
                .viewport
                .unwrap()
                .rect
                .width,
            before
        );
        assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), cursor);
        assert_eq!(app.ui_probe().win(win).unwrap().scroll_top(), scroll);
        assert_eq!(app.ui_probe().win(win).unwrap().scroll_left, 4);
    }
    app.type_char('r');
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    assert_eq!(
        app.ui_probe()
            .win(sidebar)
            .unwrap()
            .viewport
            .unwrap()
            .rect
            .width,
        width + 12
    );
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), cursor);
    assert_eq!(app.ui_probe().win(win).unwrap().scroll_left, 4);
}

#[test]
fn diff_sidebar_counts_follow_labels_when_resized() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(140, 32);
    open_fixture(&mut app, 1000);
    for delta in [0, 12, -12] {
        drag_diff_divider(&mut app, delta);
        let frame = app.render_to_frame().text();
        for label in [
            "unstaged (2) +1001 -1",
            "M first.rs +1000",
            "M last.rs +1 -1",
        ] {
            assert!(frame.contains(label), "missing {label:?}:\n{frame}");
        }
    }
}

#[test]
fn diff_narrow_split_keeps_group_counts_readable() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 18);
    open_fixture(&mut app, 1);
    paint_diff(&mut app);
    let frame = app.render_to_frame().text();
    for label in ["unstaged (2) +2 -1", "M first.rs +1", "M last.rs +1 -1"] {
        assert!(frame.contains(label), "missing {label:?}:\n{frame}");
    }
}

#[test]
fn diff_file_click_jumps_from_preview_and_repeated_click_returns_to_header() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    open_fixture(&mut app, 100);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let win = preview(&app);
    click_diff_row(&mut app, sidebar, 2);
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), 104);
    assert!(app.render_to_frame().text().contains("last change"));
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    app.type_char('G');
    app.render_silent();
    app.dispatch_ui_window_events(false);
    click_diff_row(&mut app, sidebar, 2);
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), 104);
}

#[test]
fn diff_preview_order_matches_tree_and_scroll_moves_selection_forward() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(r#"
        local patches = {}
        for _, path in ipairs({'README.md', 'src/z.rs', 'tests/test.rs', 'src/nested/b.rs', 'src/nested/a.rs'}) do
            patches[#patches + 1] = 'diff --git a/' .. path .. ' b/' .. path .. '\n@@ -0,0 +1,40 @@\n'
                .. string.rep('+content\n', 40)
        end
        smelt.git.diff = function() return {diff = smelt.diff.parse(table.concat(patches))} end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    let win = preview(&app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let source = Arc::clone(app.ui_probe().win(win).unwrap().row_source().unwrap());
    for (index, (path, tree_row)) in [
        ("src/nested/a.rs", 3),
        ("src/nested/b.rs", 4),
        ("src/z.rs", 5),
        ("tests/test.rs", 7),
        ("README.md", 8),
    ]
    .into_iter()
    .enumerate()
    {
        let row = index as u64 * 44;
        assert!(
            source.rows(row..row + 1, app.ui_probe().theme())[0]
                .text
                .contains(path),
            "preview order diverges at {path}"
        );
        app.type_text(&format!("{}G", row + 1));
        paint_diff(&mut app);
        assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_row(), tree_row);
    }
}

#[test]
fn diff_ctrl_j_k_select_files_from_either_pane_without_changing_focus() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    open_fixture(&mut app, 100);
    let win = preview(&app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    for focus in [win, sidebar] {
        if app.ui_probe().focus() != Some(focus) {
            app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
        }
        for (key, expected) in [('j', 1), ('j', 1), ('k', 0), ('k', 0)] {
            app.press_mod(KeyCode::Char(key), KeyModifiers::CONTROL);
            paint_diff(&mut app);
            assert_eq!(app.ui_probe().focus(), Some(focus));
            assert_eq!(
                app.ui_probe().win(sidebar).unwrap().cursor_row(),
                expected + 1
            );
            assert_eq!(
                app.ui_probe().win(win).unwrap().cursor_abs_row(),
                expected * 104,
                "ctrl-{key}, focus={focus:?}, preview={win:?}: {}",
                app.render_to_frame().text()
            );
        }
    }
}

fn open_sidebar_motion_fixture(app: &mut TestApp) -> WinId {
    app.set_terminal_size(120, 25);
    app.run_lua_result(r#"
        local patches = {}
        for i = 1, 80 do
            local path = string.format('src/file%03d.rs', i)
            patches[i] = string.format('diff --git a/%s b/%s\n@@ -0,0 +1 @@\n+content %d\n', path, path, i)
        end
        smelt.git.diff = function() return {diff = smelt.diff.parse(table.concat(patches))} end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(app);
    paint_diff(app);
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    paint_diff(app);
    app.ui_probe().named_win("smelt.diff.files").unwrap()
}

#[test]
fn diff_sidebar_motion_shift_g_reaches_bottom() {
    let mut app = TestApp::builder().build();
    let sidebar = open_sidebar_motion_fixture(&mut app);
    let window = app.ui_probe().win(sidebar).unwrap();
    let total = window.row_source().unwrap().snapshot().total_rows;
    let height = u64::from(window.viewport.unwrap().rect.height);
    app.press_mod(KeyCode::Char('G'), KeyModifiers::SHIFT);
    paint_diff(&mut app);
    let window = app.ui_probe().win(sidebar).unwrap();
    assert_eq!(window.cursor_abs_row(), total - 1);
    assert_eq!(window.scroll_top(), total - height);
}

#[test]
fn diff_sidebar_motion_g_returns_to_bottom_after_wheel_scroll() {
    let mut app = TestApp::builder().build();
    let sidebar = open_sidebar_motion_fixture(&mut app);
    app.type_char('G');
    paint_diff(&mut app);
    let bottom = app.ui_probe().win(sidebar).unwrap().scroll_top();
    for _ in 0..3 {
        wheel_diff(&mut app, sidebar, false);
    }
    assert!(app.ui_probe().win(sidebar).unwrap().scroll_top() < bottom);
    app.type_char('G');
    paint_diff(&mut app);
    assert_eq!(app.ui_probe().win(sidebar).unwrap().scroll_top(), bottom);
}

#[test]
fn diff_sidebar_motion_counts_apply_to_j_and_k() {
    let mut app = TestApp::builder().build();
    let sidebar = open_sidebar_motion_fixture(&mut app);
    let start = app.ui_probe().win(sidebar).unwrap().cursor_abs_row();
    for (keys, expected) in [
        ("12j", start + 12),
        ("10k", start + 2),
        ("j", start + 3),
        ("03j", start + 6),
        ("999999999999999999999999k", 0),
    ] {
        app.type_text(keys);
        paint_diff(&mut app);
        assert_eq!(
            app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
            expected,
            "{keys}"
        );
        assert_eq!(app.ui_probe().focus(), Some(sidebar));
    }
    let total = app
        .ui_probe()
        .win(sidebar)
        .unwrap()
        .row_source()
        .unwrap()
        .snapshot()
        .total_rows;
    app.type_text("G2k");
    paint_diff(&mut app);
    assert_eq!(
        app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
        total - 4
    );
    app.type_text("2j");
    paint_diff(&mut app);
    assert_eq!(
        app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
        total - 1
    );
    app.type_text("gg12");
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    let row = app.ui_probe().win(sidebar).unwrap().cursor_abs_row();
    app.type_char('j');
    paint_diff(&mut app);
    assert_eq!(
        app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
        row + 1
    );
}

#[test]
fn diff_sidebar_wheel_keeps_selection_attached_to_the_file() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(120, 25);
    app.run_lua_result(r#"
        local patches = {}
        for i = 1, 80 do
            local path = string.format('file%03d.rs', i)
            patches[i] = string.format('diff --git a/%s b/%s\n@@ -0,0 +1 @@\n+content %d\n', path, path, i)
        end
        smelt.git.diff = function() return {diff = smelt.diff.parse(table.concat(patches))} end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let selected = app.ui_probe().win(sidebar).unwrap().cursor_abs_row();
    let preview_row = app.ui_probe().win(preview(&app)).unwrap().cursor_abs_row();
    for _ in 0..8 {
        wheel_diff(&mut app, sidebar, true);
    }
    let window = app.ui_probe().win(sidebar).unwrap();
    assert!(window.scroll_top() > selected);
    assert_eq!(window.cursor_abs_row(), selected);
    assert_eq!(
        app.ui_probe().win(preview(&app)).unwrap().cursor_abs_row(),
        preview_row
    );
    let frame = app.render_to_frame();
    let rect = app.ui_probe().win(sidebar).unwrap().viewport.unwrap().rect;
    let selected_bg = app.ui_probe().theme().clone().get("Visual").bg;
    assert!(selected_bg.is_some());
    for row in rect.top..rect.top + rect.height {
        assert_ne!(
            frame.styles[row as usize][rect.left as usize + 3].bg,
            selected_bg,
            "an unrelated visible file acquired the offscreen selection: {}",
            frame.text()
        );
    }
    app.type_char('j');
    paint_diff(&mut app);
    assert!(app.ui_probe().win(preview(&app)).unwrap().cursor_abs_row() > preview_row);
    assert!(app.render_to_frame().text().contains("content 2"));
}

#[tokio::test]
async fn diff_groups_show_distinct_partial_patches_totals_and_file_dividers() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    for path in ["a.rs", "b.rs"] {
        std::fs::write(dir.path().join(path), "base\n").unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    std::fs::write(dir.path().join("a.rs"), "staged\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "other staged\n").unwrap();
    git(&["add", "."]);
    std::fs::write(dir.path().join("a.rs"), "working\nextra\n").unwrap();
    std::fs::write(dir.path().join("c.rs"), "new\n").unwrap();
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(140, 45);
    app.type_text("/diff");
    app.press(KeyCode::Enter);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.feed_one(SourceEvent::LuaWakeup);
        paint_diff(&mut app);
        if app.render_to_frame().text().contains("working") {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let frame = app.render_to_frame();
    for (section, counts) in [("unstaged (2)", "+3 -1"), ("staged (2)", "+2 -2")] {
        assert!(
            frame
                .rows
                .iter()
                .any(|row| row.contains(section) && row.contains(counts)),
            "{}",
            frame.text()
        );
    }
    let source = app
        .ui_probe()
        .win(preview(&app))
        .unwrap()
        .row_source()
        .unwrap();
    let text = source
        .rows(0..100, app.ui_probe().theme())
        .into_iter()
        .map(|row| row.text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(text.matches("a.rs").count(), 2, "{text}");
    for line in ["- staged", "+ working", "- base", "+ staged"] {
        assert!(text.contains(line), "{text}");
    }
    for header in text
        .lines()
        .filter(|line| line.contains("  unstaged") || line.contains("  staged"))
    {
        assert!(!header.contains("+0") && !header.contains("-0"), "{header}");
    }
    for width in [140, 110] {
        app.set_terminal_size(width, 45);
        app.type_char('L');
        let frame = app.render_to_frame();
        let rect = app
            .ui_probe()
            .win(preview(&app))
            .unwrap()
            .viewport
            .unwrap()
            .rect;
        let dividers: Vec<_> = (rect.top..rect.top + rect.height)
            .filter(|row| {
                frame.rows[*row as usize]
                    .chars()
                    .skip(rect.left as usize)
                    .take(rect.width as usize)
                    .all(|ch| ch == '─')
            })
            .collect();
        assert_eq!(dividers.len(), 3, "{}", frame.text());
        for row in dividers {
            assert!(frame.rows[row as usize - 1]
                .chars()
                .skip(rect.left as usize)
                .take(rect.width as usize)
                .all(|ch| ch == ' '));
            assert!(frame.styles[row as usize][rect.left as usize].dim);
        }
    }
}

#[tokio::test]
async fn diff_merge_conflict_remains_visible_with_untracked_files() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap()
    };
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "test"],
        vec!["config", "user.email", "test@example.invalid"],
    ] {
        assert!(git(&args).status.success());
    }
    let path = dir.path().join("conflict.rs");
    std::fs::write(&path, "base\n").unwrap();
    assert!(git(&["add", "."]).status.success());
    assert!(git(&["commit", "-qm", "base"]).status.success());
    assert!(git(&["branch", "other"]).status.success());
    std::fs::write(&path, "ours\n").unwrap();
    assert!(git(&["commit", "-qam", "ours"]).status.success());
    assert!(git(&["checkout", "-q", "other"]).status.success());
    std::fs::write(&path, "theirs\n").unwrap();
    assert!(git(&["commit", "-qam", "theirs"]).status.success());
    assert!(!git(&["merge", "-"]).status.success());
    std::fs::write(dir.path().join("z-new"), "untracked\n").unwrap();
    let before = std::fs::read(dir.path().join(".git/index")).unwrap();
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(120, 35);
    app.type_text("/diff");
    app.press(KeyCode::Enter);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.feed_one(SourceEvent::LuaWakeup);
        paint_diff(&mut app);
        if app.render_to_frame().text().contains("z-new") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{}",
            app.render_to_frame().text()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let text = app.render_to_frame().text();
    assert!(
        text.contains("U conflict.rs") && text.contains("<<<<<<<"),
        "{text}"
    );
    assert!(
        text.contains("unstaged (2)") && text.contains("staged (0)"),
        "{text}"
    );
    assert_eq!(
        std::fs::read(dir.path().join(".git/index")).unwrap(),
        before
    );
    app.type_char('q');
    std::fs::write(&path, "theirs\n").unwrap();
    app.type_text("/diff");
    app.press(KeyCode::Enter);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.feed_one(SourceEvent::LuaWakeup);
        paint_diff(&mut app);
        if app
            .render_to_frame()
            .text()
            .contains("resolve conflicts, then stage")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{}",
            app.render_to_frame().text()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        std::fs::read(dir.path().join(".git/index")).unwrap(),
        before
    );
    app.type_char('s');
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.feed_one(SourceEvent::LuaWakeup);
        paint_diff(&mut app);
        if app.render_to_frame().text().contains("unstaged (1)") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{}",
            app.render_to_frame().text()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(git(&["ls-files", "--unmerged"]).stdout.is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
}

#[test]
fn diff_sidebar_has_neutral_names_colored_status_and_green_red_counts() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(120, 30);
    app.run_lua_result(r#"
        local patch = 'diff --git a/src/modified.rs b/src/modified.rs\n@@ -1 +1,2 @@\n-old\n+new\n+extra\n'
          .. 'diff --git a/src/added.rs b/src/added.rs\nnew file mode 100644\n@@ -0,0 +1 @@\n+added\n'
          .. 'diff --git a/src/deleted.rs b/src/deleted.rs\ndeleted file mode 100644\n@@ -1 +0,0 @@\n-deleted\n'
        smelt.git.diff = function() return {diff = smelt.diff.parse(patch)} end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let lines = app
        .ui_probe()
        .buf(app.ui_probe().win(sidebar).unwrap().buf)
        .unwrap()
        .lines()
        .to_vec();
    assert!(!lines.join("\n").contains('['));
    assert!(lines[0].contains("unstaged (3)"));
    assert_eq!(lines[1].trim_end(), "▼ src/");
    assert!(!lines
        .iter()
        .any(|line| line.contains("+0") || line.contains("-0")));
    let frame = app.render_to_frame();
    for (name, added, deleted, group) in [
        ("added.rs", "+1", "", "SmeltSuccess"),
        ("deleted.rs", "", "-1", "ErrorMsg"),
        ("modified.rs", "+2", "-1", "WarningMsg"),
    ] {
        let index = lines.iter().position(|line| line.contains(name)).unwrap();
        let line = &lines[index];
        assert!(line.contains(added) && line.contains(deleted), "{line}");
        let window = app.ui_probe().win(sidebar).unwrap();
        let row = usize::from(window.viewport.unwrap().rect.top) + index;
        let name_col =
            smelt_buffer::text::byte_to_cell(&frame.rows[row], frame.rows[row].find(name).unwrap());
        let left = name_col - smelt_buffer::text::byte_to_cell(line, line.find(name).unwrap());
        let theme = app.ui_probe().theme().clone();
        assert_eq!(
            frame.styles[row][name_col].fg,
            theme.resolve(smelt_core::theme::intern("Normal")).fg
        );
        let status_col = left + line.find(|c: char| !c.is_whitespace()).unwrap();
        assert_eq!(
            frame.styles[row][status_col].fg,
            theme.resolve(smelt_core::theme::intern(group)).fg
        );
        for (text, group) in [
            (added, "SmeltDiffAddCount"),
            (deleted, "SmeltDiffDeleteCount"),
        ] {
            if text.is_empty() {
                continue;
            }
            let col = left + smelt_buffer::text::byte_to_cell(line, line.find(text).unwrap());
            assert_eq!(
                frame.styles[row][col].fg,
                theme.get(group).fg,
                "{name}, {text} at {col}: {}",
                frame.rows[row]
            );
        }
    }
}

#[test]
fn diff_sidebar_keeps_tree_layout_and_tab_switches_panes() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(120, 30);
    app.run_lua_result(r#"
        local patch = 'diff --git a/crates/core/src/diff/filename.rs b/crates/core/src/diff/filename.rs\n@@ -0,0 +1 @@\n+content\n'
          .. 'diff --git a/README b/README\n@@ -0,0 +1 @@\n+readme\n'
        smelt.git.diff = function() return {diff = smelt.diff.parse(patch)} end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let source = Arc::clone(app.ui_probe().win(sidebar).unwrap().row_source().unwrap());
    let rows = source.rows(0..100, app.ui_probe().theme());
    let snapshot = source.snapshot();
    let win = preview(&app);
    for key in [KeyCode::Tab, KeyCode::BackTab] {
        for focus in [sidebar, win] {
            app.press_mod(
                key,
                if key == KeyCode::BackTab {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                },
            );
            paint_diff(&mut app);
            assert_eq!(app.ui_probe().focus(), Some(focus));
            assert_eq!(source.snapshot(), snapshot);
            assert!(app.render_to_frame().text().contains(" files ─"));
        }
    }
    let staged = rows
        .iter()
        .position(|row| row.text.starts_with("staged (0)"))
        .unwrap();
    assert!(rows[0].text.starts_with("unstaged (2)"));
    assert!(rows[1].text.starts_with("▼ crates/"));
    assert!(rows[staged - 1].text.trim().is_empty());
    assert_eq!(
        rows.len(),
        staged + 1,
        "empty sections have no placeholder row"
    );
    for row in [0, staged] {
        click_diff_row(&mut app, sidebar, row as u16);
        app.type_text(" hl");
        app.press(KeyCode::Enter);
        paint_diff(&mut app);
        assert_eq!(app.ui_probe().focus(), Some(sidebar));
        assert_eq!(source.snapshot(), snapshot);
    }
    app.type_char('k');
    paint_diff(&mut app);
    assert_eq!(
        app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
        staged as u64 - 2
    );
    app.type_char('j');
    paint_diff(&mut app);
    assert_eq!(
        app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
        staged as u64
    );

    let file = rows
        .iter()
        .position(|row| row.text.contains("filename.rs"))
        .unwrap();
    let frame = app.render_to_frame();
    let rect = app.ui_probe().win(sidebar).unwrap().viewport.unwrap().rect;
    let row = usize::from(rect.top) + file;
    let col = smelt_buffer::text::byte_to_cell(
        &frame.rows[row],
        frame.rows[row].find("filename.rs").unwrap(),
    );
    let theme = app.ui_probe().theme().clone();
    assert_eq!(frame.styles[row][col].fg, theme.get("Normal").fg);
    assert_eq!(
        frame.styles[usize::from(rect.top) + 1][usize::from(rect.left) + 3].fg,
        theme.get("Comment").fg
    );
}

#[tokio::test]
async fn diff_fugitive_keys_move_entries_advance_in_section_and_preserve_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    std::fs::write(dir.path().join("a.rs"), "base\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    std::fs::write(dir.path().join("a.rs"), "staged\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "other staged\n").unwrap();
    git(&["add", "."]);
    std::fs::write(dir.path().join("a.rs"), "working\n").unwrap();
    std::fs::write(dir.path().join("c.rs"), "new\n").unwrap();
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(120, 30);
    app.type_text("/diff");
    app.press(KeyCode::Enter);
    async fn wait(app: &mut TestApp, text: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            app.feed_one(SourceEvent::LuaWakeup);
            paint_diff(app);
            let found = app
                .ui_probe()
                .named_win("smelt.diff.files")
                .is_some_and(|win| {
                    app.ui_probe()
                        .buf(app.ui_probe().win(win).unwrap().buf)
                        .unwrap()
                        .lines()
                        .iter()
                        .any(|line| line.starts_with(text))
                });
            if found {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "waiting for {text}: {}",
                app.render_to_frame().text()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    wait(&mut app, "unstaged (2)").await;
    let win = preview(&app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let source = Arc::clone(app.ui_probe().win(win).unwrap().row_source().unwrap());
    let selected_header = |app: &TestApp| {
        let window = app.ui_probe().win(win).unwrap();
        let row = window.cursor_abs_row();
        window
            .row_source()
            .unwrap()
            .rows(row..row + 1, app.ui_probe().theme())[0]
            .text
            .clone()
    };
    app.type_text("4GL");
    paint_diff(&mut app);
    app.type_char('-');
    wait(&mut app, "unstaged (1)").await;
    assert_eq!(git(&["show", ":a.rs"]), "working\n");
    assert_eq!(git(&["show", ":b.rs"]), "other staged\n");
    assert!(selected_header(&app).contains("c.rs"));
    assert!(selected_header(&app).contains("unstaged"));
    assert_eq!(app.ui_probe().win(win).unwrap().scroll_left, 4);
    assert_eq!(app.ui_probe().focus(), Some(win));
    assert!(!Arc::ptr_eq(
        &source,
        app.ui_probe().win(win).unwrap().row_source().unwrap()
    ));
    assert!(
        source
            .rows(0..100, app.ui_probe().theme())
            .iter()
            .any(|row| row.text == "- staged"),
        "old snapshots remain immutable"
    );

    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    app.type_char('s');
    wait(&mut app, "unstaged (0)").await;
    assert_eq!(git(&["show", ":c.rs"]), "new\n");
    assert!(app.render_to_frame().text().contains("all staged"));
    assert!(app.ui_probe().win(win).unwrap().row_source().is_none());
    let index = git(&["diff", "--cached"]);
    app.type_text("--s");
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    assert_eq!(
        git(&["diff", "--cached"]),
        index,
        "empty section must not toggle a staged entry"
    );
    assert!(app.ui_probe().win(win).unwrap().row_source().is_none());

    app.press_mod(KeyCode::Char('j'), KeyModifiers::CONTROL);
    paint_diff(&mut app);
    assert_eq!(app.ui_probe().focus(), Some(sidebar));
    assert!(selected_header(&app).contains("a.rs"));
    assert!(selected_header(&app).contains("staged"));
    app.type_char('u');
    wait(&mut app, "staged (2)").await;
    assert_eq!(git(&["show", ":a.rs"]), "base\n");
    assert!(selected_header(&app).contains("b.rs"));
    app.type_char('-');
    wait(&mut app, "staged (1)").await;
    assert_eq!(git(&["show", ":b.rs"]), "base\n");
    assert!(selected_header(&app).contains("c.rs"));
    app.type_char('-');
    wait(&mut app, "staged (0)").await;
    assert!(git(&["diff", "--cached", "--name-only"]).is_empty());
    assert!(app.render_to_frame().text().contains("nothing staged"));
    app.type_char('-');
    drive_lua_tasks(&mut app);
    assert!(git(&["diff", "--cached", "--name-only"]).is_empty());

    app.press_mod(KeyCode::Char('k'), KeyModifiers::CONTROL);
    paint_diff(&mut app);
    assert!(selected_header(&app).contains("c.rs"));
    app.type_char('u');
    drive_lua_tasks(&mut app);
    assert!(
        git(&["diff", "--cached", "--name-only"]).is_empty(),
        "u on unstaged is inert"
    );
    app.type_char('s');
    wait(&mut app, "unstaged (2)").await;
    assert!(
        selected_header(&app).contains("b.rs"),
        "last entry falls back to previous file"
    );
    assert_eq!(git(&["diff", "--cached", "--name-only"]), "c.rs\n");
    for (path, body) in [
        ("a.rs", "working\n"),
        ("b.rs", "other staged\n"),
        ("c.rs", "new\n"),
    ] {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path)).unwrap(),
            body
        );
    }
    app.press(KeyCode::Esc);
}

#[tokio::test]
async fn diff_tree_staging_keeps_sidebar_scroll() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for index in 0..160 {
        if index == 80 {
            git(&["add", "."]);
        }
        std::fs::write(dir.path().join(format!("src/file{index:03}.rs")), "new\n").unwrap();
    }
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(110, 24);
    app.type_text("/diff");
    app.press(KeyCode::Enter);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.feed_one(SourceEvent::LuaWakeup);
        paint_diff(&mut app);
        if app.render_to_frame().text().contains("unstaged (80)") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{}",
            app.render_to_frame().text()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let rect = app.ui_probe().win(sidebar).unwrap().viewport.unwrap().rect;
    for (key, counts, wheels) in [('s', [79, 78, 77], 8), ('u', [78, 79, 80], 24)] {
        for _ in 0..wheels {
            wheel_diff(&mut app, sidebar, true);
        }
        click_diff_row(&mut app, sidebar, 6);
        for (pass, remaining) in counts.into_iter().enumerate() {
            let window = app.ui_probe().win(sidebar).unwrap();
            let top = window.scroll_top();
            let cursor = window.cursor_abs_row();
            assert!(top > 0);
            assert_eq!(cursor - top, 6);
            let source = window.row_source().unwrap();
            let first = source.rows(top..top + 1, app.ui_probe().theme())[0]
                .text
                .clone();
            let next = source.rows(cursor + 1..cursor + 2, app.ui_probe().theme())[0]
                .text
                .clone();
            if pass > 0 {
                app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
            }
            app.type_char(key);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                app.feed_one(SourceEvent::LuaWakeup);
                paint_diff(&mut app);
                let window = app.ui_probe().win(sidebar).unwrap();
                let source = window.row_source().unwrap();
                let refreshed = source.rows(0..1, app.ui_probe().theme())[0]
                    .text
                    .contains(&format!("unstaged ({remaining})"));
                // Unstaging inserts a row above the staged section.
                let shift = u64::from(key == 'u' && refreshed);
                assert_eq!(window.viewport.unwrap().rect, rect);
                assert_eq!(
                    window.scroll_top(),
                    top + shift,
                    "{key}: sidebar viewport moved"
                );
                assert_eq!(
                    window.cursor_abs_row(),
                    cursor + shift,
                    "the next file should occupy the same screen row"
                );
                assert_eq!(
                    source.rows(top + shift..top + shift + 1, app.ui_probe().theme())[0].text,
                    first
                );
                if refreshed {
                    assert_eq!(
                        source.rows(cursor + shift..cursor + shift + 1, app.ui_probe().theme())[0]
                            .text,
                        next
                    );
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{}",
                    app.render_to_frame().text()
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
    app.press(KeyCode::Esc);
}

#[tokio::test]
async fn diff_tree_staging_reveals_offscreen_sidebar_selection() {
    for preview_focused in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "-q"]);
        std::fs::create_dir(dir.path().join("src")).unwrap();
        for index in 0..80 {
            if index == 40 {
                git(&["add", "."]);
            }
            std::fs::write(dir.path().join(format!("src/file{index:03}.rs")), "new\n").unwrap();
        }
        let mut app = TestApp::builder().with_cwd(dir.path()).build();
        app.set_terminal_size(110, 24);
        app.type_text("/diff");
        app.press(KeyCode::Enter);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            app.feed_one(SourceEvent::LuaWakeup);
            paint_diff(&mut app);
            if app.render_to_frame().text().contains("unstaged (40)") {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
        let visible = |app: &TestApp| {
            let window = app.ui_probe().win(sidebar).unwrap();
            let top = window.scroll_top();
            let height = u64::from(window.viewport.unwrap().rect.height);
            (top..top + height).contains(&window.cursor_abs_row())
        };
        for (key, name, scroll_down, staged) in [
            ('s', "file050.rs", true, true),
            ('s', "file070.rs", false, true),
            ('u', "file010.rs", true, false),
            ('u', "file020.rs", false, false),
            ('-', "file055.rs", true, true),
            ('-', "file025.rs", false, false),
        ] {
            let source = Arc::clone(app.ui_probe().win(sidebar).unwrap().row_source().unwrap());
            let rows = source.rows(0..source.snapshot().total_rows, app.ui_probe().theme());
            let row = rows.iter().position(|row| row.text.contains(name)).unwrap();
            let next = rows[row + 1].text.clone();
            let screen_row = show_tree_row(&mut app, sidebar, row);
            click_diff_row(&mut app, sidebar, screen_row);
            if preview_focused {
                app.press(KeyCode::Tab);
                paint_diff(&mut app);
            }
            for _ in 0..10 {
                wheel_diff(&mut app, sidebar, scroll_down);
            }
            assert!(!visible(&app), "fixture cursor is still visible: {name}");
            let focus = app.ui_probe().focus();
            let top = app.ui_probe().win(sidebar).unwrap().scroll_top();
            assert_eq!(
                app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
                row as u64
            );
            assert_eq!((row as u64) < top, scroll_down);
            app.type_char(key);
            let visible_on_start = visible(&app);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                app.feed_one(SourceEvent::LuaWakeup);
                paint_diff(&mut app);
                if !Arc::ptr_eq(
                    &source,
                    app.ui_probe().win(sidebar).unwrap().row_source().unwrap(),
                ) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{}",
                    app.render_to_frame().text()
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                visible(&app),
                "{key} {name}, preview_focused={preview_focused}: {}",
                app.render_to_frame().text()
            );
            assert!(
                visible_on_start,
                "{key} must reveal the selected file before waiting for Git"
            );
            assert_eq!(app.ui_probe().focus(), focus);
            let window = app.ui_probe().win(sidebar).unwrap();
            let cursor = window.cursor_abs_row();
            assert_eq!(
                window
                    .row_source()
                    .unwrap()
                    .rows(cursor..cursor + 1, app.ui_probe().theme())[0]
                    .text,
                next
            );
            assert_eq!(
                git(&["ls-files"])
                    .lines()
                    .any(|path| path == format!("src/{name}")),
                staged
            );
        }
        app.press(KeyCode::Esc);
    }
}

fn open_folder_click_fixture(app: &mut TestApp) -> WinId {
    app.set_terminal_size(110, 28);
    app.run_lua_result(r#"
        local patch = 'diff --git a/src/nested/a.rs b/src/nested/a.rs\n@@ -0,0 +1,100 @@\n'
            .. string.rep('+abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789\n', 100)
            .. 'diff --git a/src/z.rs b/src/z.rs\n@@ -0,0 +1 @@\n+other source\n'
            .. 'diff --git a/tests/test.rs b/tests/test.rs\n@@ -0,0 +1 @@\n+test\n'
        smelt.git.diff = function() return { diff = smelt.diff.parse(patch) } end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(app);
    app.type_text("70GL");
    paint_diff(app);
    app.ui_probe().named_win("smelt.diff.files").unwrap()
}

#[test]
fn diff_tree_passive_preview_sync_keeps_folders_collapsed() {
    let mut app = TestApp::builder().build();
    let sidebar = open_folder_click_fixture(&mut app);
    let source = Arc::clone(app.ui_probe().win(sidebar).unwrap().row_source().unwrap());
    click_diff_row(&mut app, sidebar, 1);
    let collapsed = source.snapshot().total_rows;
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    paint_diff(&mut app);
    assert_eq!(source.snapshot().total_rows, collapsed);
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 1);
    app.type_text("jk");
    for down in [true, false] {
        let win = preview(&app);
        wheel_diff(&mut app, win, down);
        assert_eq!(source.snapshot().total_rows, collapsed);
        assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 1);
    }
    app.type_char('r');
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    let refreshed = app.ui_probe().win(sidebar).unwrap().row_source().unwrap();
    assert!(!Arc::ptr_eq(&source, refreshed));
    assert_eq!(refreshed.snapshot().total_rows, collapsed);
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 1);
}

#[test]
fn diff_tree_folder_click_selects_on_press_and_toggles_on_release() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    let sidebar = open_folder_click_fixture(&mut app);
    let win = preview(&app);
    let position = |app: &TestApp| {
        let window = app.ui_probe().win(win).unwrap();
        (
            window.cursor_abs_row(),
            window.scroll_top(),
            window.scroll_left,
        )
    };
    let before = position(&app);
    assert_eq!(before.0, 69);
    assert!(before.1 > 0);
    assert_eq!(before.2, 4);
    let source = Arc::clone(app.ui_probe().win(sidebar).unwrap().row_source().unwrap());
    let total = source.snapshot().total_rows;
    let rect = app.ui_probe().win(sidebar).unwrap().viewport.unwrap().rect;
    for (row, hidden) in [(1, 3), (2, 1), (5, 1)] {
        for (kind, expanded) in [
            (MouseEventKind::Down(MouseButton::Left), true),
            (MouseEventKind::Up(MouseButton::Left), false),
            (MouseEventKind::Down(MouseButton::Left), false),
            (MouseEventKind::Up(MouseButton::Left), true),
        ] {
            let generation =
                source.snapshot().generation + u64::from(matches!(kind, MouseEventKind::Up(_)));
            app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                kind,
                row: rect.top + row,
                column: rect.left + 3,
                modifiers: KeyModifiers::NONE,
            })));
            for _ in 0..3 {
                paint_diff(&mut app);
                let frame = app.render_to_frame();
                assert_eq!(app.ui_probe().focus(), Some(sidebar));
                assert_eq!(
                    app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
                    u64::from(row)
                );
                assert_eq!(position(&app), before, "{kind:?}: {}", frame.text());
                assert_eq!(source.snapshot().generation, generation, "{kind:?}");
                assert_eq!(
                    source.snapshot().total_rows,
                    if expanded { total } else { total - hidden },
                    "{kind:?}: {}",
                    frame.text()
                );
                assert_eq!(
                    frame.styles[usize::from(rect.top + row)]
                        [usize::from(rect.left + rect.width - 4)]
                    .bg,
                    app.ui_probe().theme().clone().get("CursorLine").bg,
                    "the pressed folder must stay highlighted: {}",
                    frame.text()
                );
            }
        }
    }
    click_diff_row(&mut app, sidebar, 1);
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    paint_diff(&mut app);
    assert_eq!(app.ui_probe().focus(), Some(win));
    assert_eq!(position(&app), before);
    assert_eq!(source.snapshot().total_rows, total - 3);
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 1);
    app.type_text("jk");
    paint_diff(&mut app);
    assert_eq!(source.snapshot().total_rows, total - 3);
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 1);
    app.press_mod(KeyCode::Char('j'), KeyModifiers::CONTROL);
    paint_diff(&mut app);
    assert_eq!(source.snapshot().total_rows, total);
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 4);
}

#[test]
fn diff_tree_consecutive_folder_clicks_work_after_projection_changes() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(r#"
        local patches = {}
        for _, dir in ipairs({'a', 'b', 'c'}) do
            for i = 1, 30 do
                local path = string.format('%s/file%02d.rs', dir, i)
                patches[#patches + 1] = string.format('diff --git a/%s b/%s\n@@ -0,0 +1 @@\n+content\n', path, path)
            end
        end
        smelt.git.diff = function() return {diff = smelt.diff.parse(table.concat(patches))} end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let source = Arc::clone(app.ui_probe().win(sidebar).unwrap().row_source().unwrap());
    let preview = preview(&app);
    let preview_position = |app: &TestApp| {
        let window = app.ui_probe().win(preview).unwrap();
        (
            window.cursor_abs_row(),
            window.scroll_top(),
            window.scroll_left,
        )
    };
    let position = preview_position(&app);
    for (folder, expanded) in [
        ("a", false),
        ("b", false),
        ("c", false),
        ("c", true),
        ("b", true),
        ("a", true),
    ] {
        let row = source
            .rows(0..100, app.ui_probe().theme())
            .iter()
            .position(|row| row.text.contains(&format!("{folder}/")))
            .unwrap();
        let rect = app.ui_probe().win(sidebar).unwrap().viewport.unwrap().rect;
        let top = app.ui_probe().win(sidebar).unwrap().scroll_top();
        let screen_row = row as u64 - top;
        assert!(screen_row < u64::from(rect.height));
        let generation = source.snapshot().generation;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            // The terminal loop publishes native focus changes before flushing Lua callbacks.
            app.app.dispatch_terminal_event(Event::Mouse(MouseEvent {
                kind,
                row: rect.top + screen_row as u16,
                column: rect.left + 1,
                modifiers: KeyModifiers::NONE,
            }));
            app.dispatch_ui_window_events(false);
            app.render_silent();
            app.feed_one(SourceEvent::Tick(100));
            app.dispatch_ui_window_events(true);
            app.render_silent();
            assert_eq!(app.ui_probe().focus(), Some(sidebar));
            assert_eq!(
                app.ui_probe().win(sidebar).unwrap().cursor_abs_row(),
                row as u64
            );
            assert_eq!(preview_position(&app), position);
        }
        assert_eq!(
            source.snapshot().generation,
            generation + 1,
            "{folder}, expanded={expanded}: {}",
            app.render_to_frame().text()
        );
        let prefix = if expanded { "▼" } else { "▶" };
        assert!(
            source.rows(row as u64..row as u64 + 1, app.ui_probe().theme())[0]
                .text
                .starts_with(&format!("{prefix} {folder}/"))
        );
    }
}

#[test]
fn diff_tree_folder_click_cancels_when_its_target_changes() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    for interruption in [
        "other row",
        "outside",
        "refresh",
        "resize",
        "focus",
        "keyboard",
    ] {
        let mut app = TestApp::builder().build();
        let sidebar = open_folder_click_fixture(&mut app);
        let win = preview(&app);
        let position = |app: &TestApp| {
            let window = app.ui_probe().win(win).unwrap();
            (
                window.cursor_abs_row(),
                window.scroll_top(),
                window.scroll_left,
            )
        };
        let before = position(&app);
        let rect = app.ui_probe().win(sidebar).unwrap().viewport.unwrap().rect;
        let mut event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: rect.top + 1,
            column: rect.left + 3,
            modifiers: KeyModifiers::NONE,
        };
        app.feed_one(SourceEvent::Term(Event::Mouse(event)));
        paint_diff(&mut app);
        match interruption {
            "other row" => event.row += 2,
            "outside" => event.column += rect.width,
            "refresh" => {
                app.type_char('r');
                drive_lua_tasks(&mut app);
            }
            "resize" => app.set_terminal_size(120, 30),
            "focus" => app.press(KeyCode::Tab),
            "keyboard" => app.type_char('h'),
            _ => unreachable!(),
        }
        paint_diff(&mut app);
        let source = Arc::clone(app.ui_probe().win(sidebar).unwrap().row_source().unwrap());
        let snapshot = source.snapshot();
        event.kind = MouseEventKind::Drag(MouseButton::Left);
        app.feed_one(SourceEvent::Term(Event::Mouse(event)));
        paint_diff(&mut app);
        event.kind = MouseEventKind::Up(MouseButton::Left);
        app.feed_one(SourceEvent::Term(Event::Mouse(event)));
        paint_diff(&mut app);
        assert_eq!(source.snapshot(), snapshot, "{interruption}");
        // Resizing may clamp the preview viewport to its larger height.
        if interruption != "resize" {
            assert_eq!(position(&app), before, "{interruption}");
        }
        if matches!(interruption, "other row" | "outside") {
            assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 1);
        }
    }
}

#[test]
fn diff_tree_mouse_folders_keyboard_navigation_and_explicit_file_reveal() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(r#"
        local patch = 'diff --git a/src/z.rs b/src/z.rs\n@@ -0,0 +1,60 @@\n'
            .. string.rep('+let value = 42;\n', 60)
            .. 'diff --git a/src/nested/a.lua b/src/nested/a.lua\n@@ -0,0 +1,2 @@\n+local value = "hello"\n+return value\n'
            .. 'diff --git a/tests/test.py b/tests/test.py\n@@ -0,0 +1 @@\n+assert True\n'
            .. 'diff --git a/README.md b/README.md\n@@ -0,0 +1 @@\n+# Read me\n'
        smelt.git.diff = function() return { diff = smelt.diff.parse(patch) } end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    app.render_silent();
    app.dispatch_ui_window_events(false);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let win = preview(&app);
    let tree = |app: &TestApp| {
        app.ui_probe()
            .buf(app.ui_probe().win(sidebar).unwrap().buf)
            .unwrap()
            .lines()
            .join("\n")
    };
    assert!(tree(&app).contains("\n▼ src/"), "{}", tree(&app));
    assert!(tree(&app).contains("\n  ▼ nested/"));
    assert!(!tree(&app).contains('['));
    assert!(!tree(&app).contains("src/z.rs"));
    click_diff_row(&mut app, sidebar, 3);
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), 0);
    click_diff_row(&mut app, sidebar, 2);
    assert!(!tree(&app).contains("a.lua"));
    assert!(tree(&app).contains("\n  ▶ nested/"));
    click_diff_row(&mut app, sidebar, 2);
    assert!(tree(&app).contains("a.lua"));
    click_diff_row(&mut app, sidebar, 2);
    click_diff_row(&mut app, sidebar, 3);
    click_diff_row(&mut app, sidebar, 1);
    assert!(!tree(&app).contains("z.rs"));
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(app.ui_probe().focus(), Some(win));
    app.type_char('G');
    app.render_silent();
    app.dispatch_ui_window_events(false);
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), 77);
    app.type_text("7G");
    app.render_silent();
    app.dispatch_ui_window_events(false);
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), 6);
    assert!(
        !tree(&app).contains("z.rs"),
        "passive preview navigation reopened src"
    );
    app.press_mod(KeyCode::Char('j'), KeyModifiers::CONTROL);
    app.press_mod(KeyCode::Char('k'), KeyModifiers::CONTROL);
    paint_diff(&mut app);
    assert!(
        tree(&app).contains("z.rs"),
        "{}\n{}",
        tree(&app),
        app.render_to_frame().text()
    );
    assert!(!tree(&app).contains("a.lua"), "unrelated folder reopened");
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_row(), 3);
    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
    app.type_text("hh");
    app.render_silent();
    assert!(!tree(&app).contains("z.rs"));
    app.type_text("ljl");
    app.render_silent();
    assert!(tree(&app).contains("a.lua"));
    click_diff_row(&mut app, sidebar, 1);
    assert!(!tree(&app).contains("a.lua"));
    app.press_mod(KeyCode::Char('j'), KeyModifiers::CONTROL);
    paint_diff(&mut app);
    assert_eq!(app.ui_probe().focus(), Some(sidebar));
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 3);
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), 0);
    app.press_mod(KeyCode::Char('j'), KeyModifiers::CONTROL);
    paint_diff(&mut app);
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_abs_row(), 4);
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), 6);
}

#[test]
fn native_window_focus_observers_follow_keyboard_and_mouse_without_duplicates() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(
        r#"
        focus_events = {}
        local function pane(name)
            local buf = smelt.buf.new()
            buf:lines({name})
            local win = smelt.win.new(buf, {name = name, surface = 'readonly_text'})
            win:on('focus', function() table.insert(focus_events, name .. ':focus') end)
            win:on('blur', function() table.insert(focus_events, name .. ':blur') end)
            return win
        end
        focus_left, focus_right = pane('focus.left'), pane('focus.right')
        smelt.overlay.new({width = '90%', height = '80%', modal = true,
            layout = smelt.ui.layout.hbox({
                {smelt.ui.layout.leaf(focus_left), width = '50%'},
                {smelt.ui.layout.leaf(focus_right), width = 'fill'},
            })})
        focus_left:focus()
    "#,
    )
    .unwrap();
    paint_diff(&mut app);
    app.run_lua_result("focus_right:focus()").unwrap();
    paint_diff(&mut app);
    app.run_lua_result("focus_right:focus()").unwrap();
    paint_diff(&mut app);
    let left = app.ui_probe().named_win("focus.left").unwrap();
    click_diff_row(&mut app, left, 0);
    assert_eq!(
        app.eval_lua::<String>("return table.concat(focus_events, ',')")
            .unwrap(),
        "focus.left:focus,focus.left:blur,focus.right:focus,focus.right:blur,focus.left:focus"
    );
}

#[test]
fn diff_scrolled_tree_click_targets_the_visible_file() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(
        r#"
        local patches = {}
        for i = 1, 80 do
            local path = string.format('src/file%03d.rs', i)
            patches[i] = string.format('diff --git a/%s b/%s\n@@ -0,0 +1,40 @@\n', path, path)
                .. string.rep('+let value = 42;\n', 40)
        end
        smelt.git.diff = function() return { diff = smelt.diff.parse(table.concat(patches)) } end
        smelt.cmd.run('diff')
    "#,
    )
    .unwrap();
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    let row = show_tree_row(&mut app, sidebar, 73);
    assert!(app.ui_probe().win(sidebar).unwrap().scroll_top() > 0);
    click_diff_row(&mut app, sidebar, row);
    assert_eq!(
        app.ui_probe().win(preview(&app)).unwrap().cursor_abs_row(),
        71 * 44
    );
    assert!(app.render_to_frame().text().contains("src/file072.rs"));
}

#[test]
fn diff_source_rows_have_syntax_foregrounds_over_diff_backgrounds() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(r#"
        smelt.git.diff = function() return { diff = smelt.diff.parse(
            'diff --git a/main.rs b/main.rs\n@@ -1 +1 @@\n-let answer = 41;\n+let answer = "界";\n') } end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    app.wait_for_document("smelt.diff.preview", Duration::from_secs(5));
    let window = app.ui_probe().win(preview(&app)).unwrap();
    let source = Arc::clone(window.row_source().unwrap());
    let generation = source.snapshot().generation;
    let buf = app.ui_probe().buf(window.buf).unwrap();
    let row = buf
        .lines()
        .iter()
        .position(|line| line.contains("+ let answer"))
        .unwrap();
    let spans = buf.highlights_at(row);
    assert!(spans
        .iter()
        .any(|span| span.hl == smelt_core::theme::intern("SmeltDiffAddBg")));
    let syntax: std::collections::HashSet<_> = spans
        .iter()
        .filter(|span| span.col_start >= 2)
        .map(|span| span.hl)
        .collect();
    assert!(
        syntax.len() >= 3,
        "missing keyword/string/token colors: {spans:?}"
    );
    let frame = app.render_to_frame();
    let mut token_cells = Vec::new();
    for (needle, group) in [("+ let", "SmeltDiffAddBg"), ("- let", "SmeltDiffDeleteBg")] {
        let row = frame
            .rows
            .iter()
            .position(|line| line.contains(needle))
            .unwrap();
        let byte = frame.rows[row].find(needle).unwrap() + 2;
        let col = smelt_buffer::text::byte_to_cell(&frame.rows[row], byte);
        let expected = app
            .ui_probe()
            .theme()
            .clone()
            .resolve(smelt_core::theme::intern(group))
            .bg;
        assert!(expected.is_some());
        assert_eq!(frame.styles[row][col].bg, expected, "syntax erased {group}");
        token_cells.push((row, col, group));
    }

    app.run_lua_result(
        r#"
        local groups = smelt.theme.snapshot()
        groups.SmeltDiffAddBg = { bg = { rgb = { 10, 60, 30 } } }
        groups.SmeltDiffDeleteBg = { bg = { rgb = { 70, 30, 20 } } }
        smelt.theme.apply({ syntax = 'Catppuccin Mocha', groups = groups })
    "#,
    )
    .unwrap();
    let repainted = app.render_to_frame();
    assert!(Arc::ptr_eq(
        &source,
        app.ui_probe()
            .win(preview(&app))
            .unwrap()
            .row_source()
            .unwrap()
    ));
    assert_eq!(source.snapshot().generation, generation);
    for (row, col, group) in token_cells {
        assert_ne!(repainted.styles[row][col].fg, frame.styles[row][col].fg);
        assert_eq!(
            repainted.styles[row][col].bg,
            app.ui_probe()
                .theme()
                .clone()
                .resolve(smelt_core::theme::intern(group))
                .bg,
            "cached syntax did not pick up the new {group}"
        );
    }
}

#[test]
fn diff_full_width_backgrounds_markers_and_inline_changes_survive_pan_and_theme() {
    use smelt_core::style::Color;
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(r#"
        smelt.git.diff = function() return {diff = smelt.diff.parse(
            'diff --git a/src/main.rs b/src/main.rs\n@@ -1 +1 @@\n-\tlet message = "hello 界 old world";\n+\tlet message = "hello 界 new world";\n')} end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    app.wait_for_document("smelt.diff.preview", Duration::from_secs(5));
    for pass in 0..3 {
        let frame = app.render_to_frame();
        let rect = app
            .ui_probe()
            .win(preview(&app))
            .unwrap()
            .viewport
            .unwrap()
            .rect;
        let theme = app.ui_probe().theme().clone();
        for (needle, marker, color, base, inline) in [
            (
                "old world",
                "- ",
                Color::Red,
                "SmeltDiffDeleteBg",
                "SmeltDiffDeleteInlineBg",
            ),
            (
                "new world",
                "+ ",
                Color::Green,
                "SmeltDiffAddBg",
                "SmeltDiffAddInlineBg",
            ),
        ] {
            let row = frame
                .rows
                .iter()
                .position(|line| line.contains(needle))
                .unwrap();
            let base = theme.resolve(smelt_core::theme::intern(base)).bg;
            let inline = theme.resolve(smelt_core::theme::intern(inline)).bg;
            let byte = frame.rows[row].find(needle).unwrap();
            // Snapshot rows already contain a cell for each wide-glyph continuation.
            let start = frame.rows[row]
                .char_indices()
                .take_while(|(index, _)| *index < byte)
                .count();
            for col in rect.left as usize..usize::from(rect.left + rect.width) {
                assert_eq!(
                    frame.styles[row][col].bg,
                    if (start..start + 3).contains(&col) {
                        inline
                    } else {
                        base
                    },
                    "pass {pass}, row {row}, col {col}: {}",
                    frame.rows[row]
                );
            }
            assert_eq!(
                frame.styles[row][start].fg,
                frame.styles[row][start + 5].fg,
                "inline changed syntax foreground"
            );
            if pass == 0 {
                let col = smelt_buffer::text::byte_to_cell(
                    &frame.rows[row],
                    frame.rows[row].find(marker).unwrap(),
                );
                assert_eq!(frame.styles[row][col].fg, Some(color));
            }
        }
        if pass == 0 {
            app.type_char('L');
        } else if pass == 1 {
            app.run_lua_result(
                r#"
                local groups = smelt.theme.snapshot()
                groups.SmeltDiffAddBg = {bg = {rgb = {10, 60, 30}}}
                groups.SmeltDiffDeleteBg = {bg = {rgb = {70, 30, 20}}}
                groups.SmeltDiffAddInlineBg = {bg = {rgb = {30, 110, 50}}}
                groups.SmeltDiffDeleteInlineBg = {bg = {rgb = {120, 50, 40}}}
                smelt.theme.apply({syntax = 'Catppuccin Mocha', groups = groups})
            "#,
            )
            .unwrap();
        }
    }
}

#[test]
fn diff_visual_copy_crosses_materialized_rows_and_esc_leaves_selection_first() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    open_fixture(&mut app, 100);
    let win = preview(&app);
    app.type_text("3GV50G");
    paint_diff(&mut app);
    let before = app.ui_probe().win(win).unwrap().viewport.unwrap().rect;
    let cursor = app.ui_probe().win(win).unwrap().cursor_abs_row();
    let scroll = app.ui_probe().win(win).unwrap().scroll_top();
    app.press_mod(KeyCode::Char('w'), KeyModifiers::CONTROL);
    app.type_char('>');
    paint_diff(&mut app);
    assert_eq!(
        app.ui_probe()
            .win(win)
            .unwrap()
            .viewport
            .unwrap()
            .rect
            .width,
        before.width + 4
    );
    drag_diff_divider(&mut app, 4);
    assert_eq!(
        app.ui_probe()
            .win(win)
            .unwrap()
            .viewport
            .unwrap()
            .rect
            .width,
        before.width
    );
    assert_eq!(
        app.ui_probe().win(win).unwrap().vim_mode(),
        VimMode::VisualLine
    );
    assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), cursor);
    assert_eq!(app.ui_probe().win(win).unwrap().scroll_top(), scroll);
    app.type_char('y');
    assert_eq!(
        app.core_probe().clipboard.kill_ring.current(),
        (0..48)
            .map(|_| "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789")
            .collect::<Vec<_>>()
            .join("\n")
    );
    app.type_text("vkk");
    app.render_silent();
    app.press(KeyCode::Esc);
    assert_eq!(app.ui_probe().win(win).unwrap().vim_mode(), VimMode::Normal);
    app.press(KeyCode::Esc);
    assert!(app.ui_probe().named_win("smelt.diff.preview").is_none());
}

#[test]
fn diff_index_shortcuts_do_not_escape_visual_or_pending_vim_input() {
    let mut app = TestApp::builder().build();
    open_fixture(&mut app, 100);
    let win = preview(&app);
    app.run_lua_result("index_actions = {}; smelt.git.index = function(_, _, action) table.insert(index_actions, action); return smelt.git.diff() end").unwrap();
    for prefix in ['v', 'V', 'y', '3'] {
        for key in ['s', 'u', '-'] {
            app.type_text(&format!("{prefix}{key}"));
            paint_diff(&mut app);
            drive_lua_tasks(&mut app);
            assert_eq!(
                app.eval_lua::<usize>("return #index_actions").unwrap(),
                0,
                "{prefix}{key} staged a file"
            );
            let window = app.ui_probe().win(win).unwrap();
            if prefix == '3' {
                assert!(
                    window.vim_state().is_idle(),
                    "invalid counted motion was not canceled"
                );
            }
            if window.vim_mode() != VimMode::Normal || !window.vim_state().is_idle() {
                app.press(KeyCode::Esc);
            }
        }
    }
    for key in ['s', 'u', '-'] {
        app.type_char(key);
        drive_lua_tasks(&mut app);
    }
    assert_eq!(
        app.eval_lua::<String>("return table.concat(index_actions, ',')")
            .unwrap(),
        "stage,toggle"
    );
}

#[test]
fn diff_wheel_scroll_updates_sidebar_in_both_directions() {
    use crossterm::event::{MouseEvent, MouseEventKind};
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    open_fixture(&mut app, 100);
    let win = preview(&app);
    let rect = app.ui_probe().win(win).unwrap().viewport.unwrap().rect;
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    app.type_char('G');
    app.render_silent();
    app.dispatch_ui_window_events(false);
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_row(), 2);
    for _ in 0..8 {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            row: rect.top + 2,
            column: rect.left + 2,
            modifiers: KeyModifiers::NONE,
        })));
        app.render_silent();
        app.dispatch_ui_window_events(false);
    }
    assert_eq!(app.ui_probe().win(sidebar).unwrap().cursor_row(), 1);
}

#[test]
fn diff_closing_reopening_and_reload_release_native_documents() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    open_fixture(&mut app, 100);
    let source = Arc::downgrade(
        app.ui_probe()
            .win(preview(&app))
            .unwrap()
            .row_source()
            .unwrap(),
    );
    let scratch = app.ui_probe().win(preview(&app)).unwrap().buf;
    app.type_char('q');
    assert!(app.run_lua("smelt.git.diff = nil; collectgarbage('collect')"));
    assert!(source.upgrade().is_none(), "closed document retained");
    assert!(
        app.ui_probe().buf(scratch).is_none(),
        "closed projection retained"
    );
    open_fixture(&mut app, 100);
    let source = Arc::downgrade(
        app.ui_probe()
            .win(preview(&app))
            .unwrap()
            .row_source()
            .unwrap(),
    );
    let scratch = app.ui_probe().win(preview(&app)).unwrap().buf;
    app.reload_lua();
    assert!(app.ui_probe().named_win("smelt.diff.preview").is_none());
    assert!(source.upgrade().is_none(), "reloaded document retained");
    assert!(
        app.ui_probe().buf(scratch).is_none(),
        "reloaded projection retained"
    );
    open_fixture(&mut app, 10);
}

#[test]
fn native_document_windows_share_storage_not_viewports_and_detach_cleanly() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    app.run_lua_result(r#"
        local source = smelt.document.text(string.rep('alpha 界\n', 100) .. 'last')
        assert(source:row_count() == 101)
        left = smelt.win.new(smelt.buf.new(), { name = 'doc.left', vim_enabled = true })
        right = smelt.win.new(smelt.buf.new(), { name = 'doc.right', vim_enabled = true })
        left:document(source)
        right:document(source)
        smelt.overlay.new({ width = '90%', height = '80%', modal = true,
            layout = smelt.ui.layout.hbox({ { smelt.ui.layout.leaf(left), width = '50%' }, { smelt.ui.layout.leaf(right), width = 'fill' } }) })
        left:focus()
    "#).unwrap();
    app.render_silent();
    let left = app.ui_probe().named_win("doc.left").unwrap();
    let right = app.ui_probe().named_win("doc.right").unwrap();
    assert!(Arc::ptr_eq(
        app.ui_probe().win(left).unwrap().row_source().unwrap(),
        app.ui_probe().win(right).unwrap().row_source().unwrap()
    ));
    app.type_char('G');
    let frame = app.render_to_frame().text();
    assert!(frame.contains("last"), "{frame}");
    assert_eq!(app.ui_probe().win(right).unwrap().scroll_top(), 0);
    assert!(app.ui_probe().win(left).unwrap().scroll_top() > 80);
    app.run_lua_result("right:cursor(99)").unwrap();
    app.render_silent();
    assert_eq!(app.ui_probe().win(right).unwrap().cursor_abs_row(), 99);
    app.run_lua_result("right:move_cursor(-90)").unwrap();
    app.render_silent();
    assert_eq!(app.ui_probe().win(right).unwrap().cursor_abs_row(), 9);
    assert!(app.run_lua("left:document(nil); left:buf():lines({'detached'}); left:cursor(0)"));
    let frame = app.render_to_frame().text();
    assert!(frame.contains("detached"), "{frame}");
    assert!(app.ui_probe().win(left).unwrap().row_source().is_none());
    assert_eq!(
        app.ui_probe()
            .win(right)
            .unwrap()
            .row_source()
            .unwrap()
            .snapshot()
            .total_rows,
        101
    );
}

#[test]
fn diff_context_fold_expands_and_resize_keeps_virtual_projection() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    assert!(app.run_lua(
        r#"
        local patch = 'diff --git a/fold.rs b/fold.rs\n@@ -1,101 +1,101 @@\n'
          .. string.rep(' context\n', 100) .. '-old\n+new\n'
        smelt.git.diff = function() return { branch = 'main', diff = smelt.diff.parse(patch) } end
        smelt.cmd.run('diff')
    "#
    ));
    drive_lua_tasks(&mut app);
    let frame = app.render_to_frame().text();
    assert!(frame.contains("▶ 94 unchanged lines"), "{frame}");
    let win = preview(&app);
    app.type_text("6G");
    app.press(KeyCode::Enter);
    assert!(app
        .render_to_frame()
        .text()
        .contains("▼ 94 unchanged lines"));
    assert_eq!(
        app.ui_probe()
            .win(win)
            .unwrap()
            .row_source()
            .unwrap()
            .snapshot()
            .total_rows,
        105
    );
    app.set_terminal_size(70, 18);
    let frame = app.render_to_frame().text();
    assert!(frame.contains("context"), "{frame}");
    let buf = app.ui_probe().win(win).unwrap().buf;
    assert!(app.ui_probe().buf(buf).unwrap().line_count() < 18);
    app.type_char('q');
    assert!(app.ui_probe().named_win("smelt.diff.preview").is_none());
}

#[tokio::test]
async fn diff_command_loads_real_git_worktree_and_refreshes() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    std::fs::write(root.path().join("untracked.rs"), "first local change\n").unwrap();
    let mut app = TestApp::builder().with_cwd(root.path()).build();
    app.set_terminal_size(110, 28);
    app.type_text("/diff");
    app.press(KeyCode::Enter);
    assert!(app.render_to_frame().text().contains("loading"));
    async fn wait(app: &mut TestApp, text: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            app.feed_one(SourceEvent::LuaWakeup);
            if app.render_to_frame().text().contains(text) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{}",
                app.render_to_frame().text()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    wait(&mut app, "first local change").await;
    std::fs::write(root.path().join("untracked.rs"), "refreshed local change\n").unwrap();
    app.type_char('r');
    wait(&mut app, "refreshed local change").await;
    app.press(KeyCode::Esc);
    assert!(app.ui_probe().named_win("smelt.diff.preview").is_none());
}

#[tokio::test]
async fn diff_large_worktree_is_usable_without_waiting_for_all_syntax() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let source: String = (0..1_000_000)
        .map(|i| format!("let value_{i} = compute({i}, \"hello\");\n"))
        .collect();
    std::fs::write(root.path().join("large.rs"), source).unwrap();
    std::fs::write(
        root.path().join("z_last.py"),
        "result = compute(42, \"last file\")\n",
    )
    .unwrap();
    let mut app = TestApp::builder().with_cwd(root.path()).build();
    app.set_terminal_size(120, 40);
    let start = std::time::Instant::now();
    app.type_text("/diff");
    app.press(KeyCode::Enter);
    let loaded = loop {
        app.feed_one(SourceEvent::LuaWakeup);
        if app.render_to_frame().text().contains("let value_0") {
            break true;
        }
        if start.elapsed() > Duration::from_secs(10) {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    if !loaded {
        app.press(KeyCode::Esc);
    }
    assert!(
        loaded,
        "million-line diff was still loading after {:?}",
        start.elapsed()
    );
    app.type_text("900000G");
    paint_diff(&mut app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    click_diff_row(&mut app, sidebar, 2);
    assert!(app.render_to_frame().text().contains("last file"));
    assert!(app.ui_probe().win(preview(&app)).unwrap().cursor_abs_row() > 1_000_000);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let highlighted = loop {
        app.feed_one(SourceEvent::LuaWakeup);
        app.render_silent();
        let buf = app
            .ui_probe()
            .buf(app.ui_probe().win(preview(&app)).unwrap().buf)
            .unwrap();
        let row = buf
            .lines()
            .iter()
            .position(|line| line.contains("last file"))
            .unwrap();
        if buf.highlights_at(row).len() > 3 {
            break true;
        }
        if std::time::Instant::now() > deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    app.press(KeyCode::Esc);
    assert!(
        highlighted,
        "a distant seek starved syntax for the newly selected file"
    );
}

#[test]
fn diff_ctrl_j_k_show_prefetched_syntax_on_the_first_neighbor_frame() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(
        r#"
        local patch = ''
        for _, file in ipairs({'a.rs', 'b.rs', 'c.rs'}) do
            patch = patch .. 'diff --git a/' .. file .. ' b/' .. file .. '\n@@ -0,0 +1,300 @@\n'
                .. string.rep('+let value = compute(42, "hello");\n', 300)
        end
        local diff = smelt.diff.parse(patch)
        first_source_row = diff:file_row(1) + 2
        last_source_row = diff:file_row(3) + 2
        smelt.git.diff = function() return {diff = diff} end
        smelt.cmd.run('diff')
    "#,
    )
    .unwrap();
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    app.press_mod(KeyCode::Char('j'), KeyModifiers::CONTROL);
    paint_diff(&mut app);
    let source = app
        .ui_probe()
        .win(preview(&app))
        .unwrap()
        .row_source()
        .unwrap()
        .clone();
    let globals = app.lua_probe().lua.globals();
    let first = globals.get::<u64>("first_source_row").unwrap();
    let last = globals.get::<u64>("last_source_row").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    // Cache-only reads cannot request highlighting. Neither neighbor is visible.
    while [first, last].into_iter().any(|row| {
        source.rows(row..row + 1, app.ui_probe().theme())[0]
            .spans
            .len()
            <= 3
    }) {
        assert!(
            std::time::Instant::now() < deadline,
            "adjacent file syntax was not prefetched"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !source.is_pending(),
        "speculative prefetch kept the UI pending"
    );
    for key in ['j', 'k', 'k'] {
        app.press_mod(KeyCode::Char(key), KeyModifiers::CONTROL);
        app.render_silent();
        let buffer = app
            .ui_probe()
            .buf(app.ui_probe().win(preview(&app)).unwrap().buf)
            .unwrap();
        let row = buffer
            .lines()
            .iter()
            .position(|line| line.contains("compute"))
            .unwrap();
        assert!(
            buffer.highlights_at(row).len() > 3,
            "neighbor displayed an unhighlighted first frame"
        );
    }
    app.press(KeyCode::Esc);
}

#[tokio::test]
async fn diff_stage_success_with_failed_refresh_is_stale_not_a_failed_mutation() {
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(root.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q"]);
    std::fs::write(root.path().join("file.rs"), "first version\n").unwrap();
    let mut app = TestApp::builder().with_cwd(root.path()).build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(
        r#"
        local index = smelt.git.index
        index_calls = 0
        smelt.git.index = function(...)
            index_calls = index_calls + 1
            index_result, index_error = index(...)
            index_done = true
            return index_result, index_error
        end
    "#,
    )
    .unwrap();
    app.type_text("/diff");
    app.press(KeyCode::Enter);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.feed_one(SourceEvent::LuaWakeup);
        if app.render_to_frame().text().contains("first version") {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    git(&[
        "config",
        "diff.orderFile",
        root.path().join("missing-order").to_str().unwrap(),
    ]);
    app.type_char('s');
    while !app
        .lua_probe()
        .lua
        .globals()
        .get::<bool>("index_done")
        .unwrap_or(false)
    {
        app.feed_one(SourceEvent::LuaWakeup);
        paint_diff(&mut app);
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(git(&["show", ":file.rs"]), "first version\n");
    app.run_lua_result("assert(index_result and index_result.index_updated and not index_result.diff and index_result.refresh_error, index_error)").unwrap();
    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("first version") && frame.contains("stale"),
        "{frame}"
    );
    std::fs::write(root.path().join("file.rs"), "second version\n").unwrap();
    app.type_text("s-s");
    drive_lua_tasks(&mut app);
    assert_eq!(
        git(&["show", ":file.rs"]),
        "first version\n",
        "stale snapshot permitted another mutation"
    );
    assert_eq!(
        app.lua_probe()
            .lua
            .globals()
            .get::<usize>("index_calls")
            .unwrap(),
        1
    );
    git(&["config", "--unset", "diff.orderFile"]);
    app.type_char('r');
    loop {
        app.feed_one(SourceEvent::LuaWakeup);
        let frame = app.render_to_frame().text();
        if frame.contains("second version") && !frame.contains("stale") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{frame}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    app.press(KeyCode::Esc);
}

#[test]
fn diff_refresh_failure_keeps_the_previous_snapshot_visible() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    open_fixture(&mut app, 100);
    app.run_lua_result("smelt.git.diff = function() return nil, 'refresh unavailable' end")
        .unwrap();
    app.type_char('r');
    drive_lua_tasks(&mut app);
    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("first.rs"),
        "previous snapshot discarded:\n{frame}"
    );
    assert!(
        frame.contains("stale"),
        "stale snapshot is not identified:\n{frame}"
    );
    app.type_char('G');
    assert!(app.render_to_frame().text().contains("last change"));
}

#[test]
fn diff_refresh_preserves_expanded_context_and_source_position() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(
        r#"
        local patch = 'diff --git a/fold.rs b/fold.rs\n@@ -1,101 +1,101 @@\n'
            .. string.rep(' context\n', 100) .. '-old\n+new\n'
        smelt.git.diff = function() return {diff = smelt.diff.parse(patch)} end
        smelt.cmd.run('diff')
    "#,
    )
    .unwrap();
    drive_lua_tasks(&mut app);
    app.type_text("6G");
    app.press(KeyCode::Enter);
    app.type_text("40G");
    let win = preview(&app);
    let before = app.ui_probe().win(win).unwrap().cursor_abs_row();
    app.type_char('r');
    drive_lua_tasks(&mut app);
    app.render_silent();
    let window = app.ui_probe().win(win).unwrap();
    assert_eq!(
        window.row_source().unwrap().snapshot().total_rows,
        105,
        "refresh collapsed expanded context"
    );
    assert_eq!(window.cursor_abs_row(), before);
}

#[test]
fn diff_refresh_preserves_hunk_added_deleted_and_no_newline_rows() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(r#"
        local patch = 'diff --git a/source.rs b/source.rs\n@@ -1,2 +1,3 @@\n first\n-old\n+new\n+added\n\\ No newline at end of file\n'
        smelt.git.diff = function() return {diff = smelt.diff.parse(patch)} end
        smelt.cmd.run('diff')
    "#).unwrap();
    drive_lua_tasks(&mut app);
    let win = preview(&app);
    for row in 0..7 {
        app.type_text(&format!("{}G", row + 1));
        assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), row);
        app.type_char('r');
        drive_lua_tasks(&mut app);
        app.render_silent();
        assert_eq!(
            app.ui_probe().win(win).unwrap().cursor_abs_row(),
            row,
            "refresh moved row {row}"
        );
    }
}

#[test]
fn native_documents_do_not_overwrite_a_shared_backing_buffer() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    app.run_lua_result(r#"
        shared = smelt.buf.new()
        shared:lines({'original buffer'})
        left = smelt.win.new(shared, {name = 'doc.left', vim_enabled = true})
        right = smelt.win.new(shared, {name = 'doc.right', vim_enabled = true})
        local source = smelt.document.text('first row\n' .. string.rep('middle\n', 100) .. 'last row')
        left:document(source)
        right:document(source)
        smelt.overlay.new({width = 90, height = 18, modal = true,
            layout = smelt.ui.layout.hsplit(smelt.ui.layout.leaf(left), smelt.ui.layout.leaf(right))})
        left:focus()
    "#).unwrap();
    app.render_silent();
    app.type_char('G');
    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("first row") && frame.contains("last row"),
        "document viewports overwrite each other:\n{frame}"
    );
    let left = app.ui_probe().named_win("doc.left").unwrap();
    let right = app.ui_probe().named_win("doc.right").unwrap();
    let scratch = [
        app.ui_probe().win(left).unwrap().buf,
        app.ui_probe().win(right).unwrap().buf,
    ];
    assert_ne!(scratch[0], scratch[1]);
    let backing = app.ui_probe().win(left).unwrap().backing_buffer();
    assert_eq!(app.ui_probe().win(right).unwrap().backing_buffer(), backing);
    assert!(
        app.app.ui.buf_destroy(backing).is_none(),
        "destroyed a retained backing buffer"
    );
    assert!(
        app.app.ui.buf_destroy(scratch[0]).is_none(),
        "destroyed a live native projection"
    );
    let config = app.ui_probe().win(left).unwrap().config.clone();
    assert!(
        app.app.ui.win_open_split(scratch[0], config).is_none(),
        "shared native projection storage"
    );
    app.run_lua_result(
        "assert(tostring(left:buf()) == tostring(shared)); left:document(nil); right:document(nil)",
    )
    .unwrap();
    let frame = app.render_to_frame().text();
    assert_eq!(
        frame.matches("original buffer").count(),
        2,
        "detaching did not restore the original buffer:\n{frame}"
    );
    for buf in scratch {
        assert!(
            app.ui_probe().buf(buf).is_none(),
            "detached projection leaked"
        );
    }
    assert!(app.ui_probe().win(left).unwrap().wrap);
    assert!(!app
        .ui_probe()
        .win(left)
        .unwrap()
        .surface()
        .is_readonly_text());
}

#[test]
fn native_view_demand_tracks_mounts_and_copy_uses_each_window_width() {
    use crate::smelt_edit::{
        BufferDisplayDocument, DisplayDocument, DocPosition, DocRange, TextRange,
    };
    use smelt_buffer::document::{
        DocumentRow, DocumentSnapshot, DocumentViewport, RowSource, ViewId,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Source(Mutex<HashMap<ViewId, DocumentViewport>>);
    impl RowSource for Source {
        fn snapshot(&self) -> DocumentSnapshot {
            DocumentSnapshot {
                generation: 0,
                total_rows: 1000,
            }
        }
        fn rows(&self, _: std::ops::Range<u64>, _: &smelt_core::theme::Theme) -> Vec<DocumentRow> {
            panic!("window width missing")
        }
        fn prepare(&self, id: ViewId, viewport: &DocumentViewport) {
            self.0.lock().unwrap().insert(id, viewport.clone());
        }
        fn viewport_rows(
            &self,
            viewport: &DocumentViewport,
            _: &smelt_core::theme::Theme,
        ) -> Vec<DocumentRow> {
            (viewport.rows.start..viewport.rows.end.min(1000))
                .map(|_| DocumentRow {
                    text: format!("width {}", viewport.width),
                    ..Default::default()
                })
                .collect()
        }
        fn release(&self, id: ViewId) {
            self.0.lock().unwrap().remove(&id);
        }
        fn line_number_bounds(&self) -> Option<smelt_buffer::buffer::SourceLine> {
            Some(smelt_buffer::buffer::SourceLine::Linear { lineno: 1000 })
        }
    }
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    app.run_lua_result(r#"
        left = smelt.win.new(smelt.buf.new(), {name = 'doc.left', gutter = 'line_numbers'})
        right = smelt.win.new(smelt.buf.new(), {name = 'doc.right', gutter = 'line_numbers'})
        local layout = smelt.ui.layout
        function mount(mode)
            local body = mode == 'right' and layout.leaf(right)
                or mode == 'zero' and layout.vbox({{layout.leaf(left), height = 0}, {layout.leaf(right), height = 'fill'}})
                or layout.hsplit(layout.leaf(left), layout.leaf(right), {size = 25})
            surface = smelt.overlay.new({name = 'documents', width = 90, height = 18, layout = body})
        end
        mount('both')
    "#).unwrap();
    let left = app.ui_probe().named_win("doc.left").unwrap();
    let right = app.ui_probe().named_win("doc.right").unwrap();
    let source = Arc::new(Source::default());
    app.app.ui.set_window_document(left, Some(source.clone()));
    app.app.ui.set_window_document(right, Some(source.clone()));
    app.render_silent();
    let demands = source.0.lock().unwrap().clone();
    assert_eq!(demands.len(), 2);
    for win in [left, right] {
        let width = app
            .ui_probe()
            .win(win)
            .unwrap()
            .viewport
            .unwrap()
            .content_width;
        let window = app.ui_probe().win(win).unwrap();
        assert_eq!(window.viewport.unwrap().gutter_width, 6);
        assert_eq!(
            app.ui_probe().buf(window.buf).unwrap().lines()[0],
            format!("width {width}"),
            "first frame used the width before reserving the gutter"
        );
        assert!(demands.values().any(|viewport| viewport.width == width));
        let copy = BufferDisplayDocument::new(&mut app.app.ui, win)
            .copy_range(TextRange::Rows(DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 0,
                },
                end: DocPosition {
                    row: 700,
                    byte_col: 0,
                },
            }))
            .unwrap();
        assert_eq!(
            copy.kill_ring,
            vec![format!("width {width}"); 700].join("\n")
        );
    }
    assert_eq!(
        *source.0.lock().unwrap(),
        demands,
        "copy changed syntax demand"
    );
    for mode in ["right", "both", "zero", "both"] {
        app.run_lua_result(&format!("mount('{mode}')")).unwrap();
        app.render_silent();
        assert_eq!(
            source.0.lock().unwrap().len(),
            if mode == "both" { 2 } else { 1 },
            "{mode}"
        );
    }
    app.run_lua_result("surface:close(); left:close(); right:close()")
        .unwrap();
    assert!(source.0.lock().unwrap().is_empty());
}

#[test]
fn diff_tree_handles_have_independent_folds_and_widths() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 28);
    app.run_lua_result(r#"
        local diff = smelt.diff.parse('diff --git a/src/long_filename.rs b/src/long_filename.rs\n@@ -1 +1 @@\n-old\n+new\n')
        tree_left, tree_right = diff:tree(), diff:tree()
        tree_left:width(22)
        tree_right:width(48)
        left = smelt.win.new(smelt.buf.new(), {name = 'tree.left', surface = 'list_inert'})
        right = smelt.win.new(smelt.buf.new(), {name = 'tree.right', surface = 'list_inert'})
        left:document(tree_left:document())
        right:document(tree_right:document())
        smelt.overlay.new({width = 100, height = 18, modal = true,
            layout = smelt.ui.layout.hsplit(smelt.ui.layout.leaf(left), smelt.ui.layout.leaf(right), {size = 30})})
        tree_left:toggle(1)
    "#).unwrap();
    let frame = app.render_to_frame().text();
    assert_eq!(
        frame.matches("long_filename.rs").count(),
        1,
        "collapsing one tree affected the other:\n{frame}"
    );
    app.run_lua_result("assert(#tree_left:collapsed() == 1); assert(#tree_right:collapsed() == 0)")
        .unwrap();
}

#[test]
fn diff_empty_error_and_close_while_loading() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    assert!(app.run_lua(
        r#"
        smelt.git.diff = function() return { branch = 'main', diff = smelt.diff.parse('') } end
        smelt.cmd.run('diff')
    "#
    ));
    drive_lua_tasks(&mut app);
    let frame = app.render_to_frame().text();
    assert!(frame.contains("clean"), "{frame}");
    assert!(!frame.contains("empty"), "{frame}");
    assert_eq!(frame.matches("(0)").count(), 2, "{frame}");
    assert!(frame.contains("r refresh"), "{frame}");
    app.type_char('q');
    assert!(app.run_lua(
        r#"
        smelt.git.diff = function() return nil, 'not a git repository' end
        smelt.cmd.run('diff')
    "#
    ));
    drive_lua_tasks(&mut app);
    assert!(app
        .render_to_frame()
        .text()
        .contains("not a git repository"));
    app.type_char('q');
    assert!(app.run_lua(
        r#"
        smelt.git.diff = function() smelt.sleep(10000); error('closed load resumed') end
        smelt.cmd.run('diff')
    "#
    ));
    drive_lua_tasks(&mut app);
    app.press(KeyCode::Esc);
    app.feed_one(SourceEvent::Tick(20000));
    assert!(app.ui_probe().named_win("smelt.diff.preview").is_none());
}

/// A deterministic allocation guard accompanies the timed benchmark below:
/// million-line navigation must never rebuild a million buffer/Lua rows.
#[test]
fn diff_million_line_viewport_has_bounded_frame_allocations() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(120, 40);
    open_fixture(&mut app, 1_000_000);
    let win = preview(&app);
    for key in ['G', 'k', 'j', 'h', 'l', 'H', 'L', '<', '>', '='] {
        let before = smelt_perf::alloc::thread_snapshot();
        if matches!(key, '<' | '>' | '=') {
            app.press_mod(KeyCode::Char('w'), KeyModifiers::CONTROL);
        }
        app.type_char(key);
        app.render_silent();
        app.dispatch_ui_window_events(false);
        app.render_silent();
        let after = smelt_perf::alloc::thread_snapshot();
        assert!(
            after.1.saturating_sub(before.1) < 2_000_000,
            "{key}: frame allocated {} bytes",
            after.1.saturating_sub(before.1)
        );
        let window = app.ui_probe().win(win).unwrap();
        assert!(app.ui_probe().buf(window.buf).unwrap().line_count() <= 40);
    }
}

#[test]
fn diff_many_file_tree_keeps_folder_frames_bounded() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(120, 40);
    app.run_lua_result(
        r#"
        local patches = {}
        for i = 1, 10000 do
            local path = string.format('group%03d/file%04d.rs', math.floor((i - 1) / 10), i)
            patches[i] = string.format('diff --git a/%s b/%s\n@@ -0,0 +1 @@\n+line\n', path, path)
        end
        smelt.git.diff = function() return {diff = smelt.diff.parse(table.concat(patches))} end
        smelt.cmd.run('diff')
    "#,
    )
    .unwrap();
    drive_lua_tasks(&mut app);
    paint_diff(&mut app);
    let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
    for _ in 0..2 {
        let before = smelt_perf::alloc::thread_snapshot();
        let start = std::time::Instant::now();
        click_diff_row(&mut app, sidebar, 1);
        let allocated = smelt_perf::alloc::thread_snapshot()
            .1
            .saturating_sub(before.1);
        assert!(
            allocated < 2_000_000,
            "folder toggle: {:?}, {allocated} allocated bytes",
            start.elapsed()
        );
        let window = app.ui_probe().win(sidebar).unwrap();
        assert!(app.ui_probe().buf(window.buf).unwrap().line_count() <= 40);
        assert!(window.row_source().unwrap().snapshot().total_rows >= 10000);
    }
}

fn paint_diff(app: &mut TestApp) {
    app.render_silent();
    app.dispatch_ui_window_events(false);
    app.render_silent();
}

fn wheel_diff(app: &mut TestApp, win: WinId, down: bool) {
    use crossterm::event::{MouseEvent, MouseEventKind};
    let rect = app.ui_probe().win(win).unwrap().viewport.unwrap().rect;
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        },
        row: rect.top + 2,
        column: rect.left + 3,
        modifiers: KeyModifiers::NONE,
    })));
    paint_diff(app);
}

/// Bring a tree row on screen before timing its actual mouse interaction.
fn show_tree_row(app: &mut TestApp, win: WinId, row: usize) -> u16 {
    let window = app.ui_probe().win(win).unwrap();
    let rect = window.viewport.unwrap().rect;
    let delta = row as isize - window.scroll_top() as isize;
    app.scroll_at(rect.top, rect.left + 3, delta);
    paint_diff(app);
    (row as u64 - app.ui_probe().win(win).unwrap().scroll_top()) as u16
}

#[tokio::test]
#[ignore = "timed benchmark: cargo xtask bench-diff"]
async fn diff_loading_benchmark() {
    for (count, file_count, tracked) in [
        (10_000, 2, false),
        (1_000_000, 2, false),
        (1_000_000, 1000, false),
        (1_000_000, 1000, true),
        (1_000_000, 10_000, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        let paths: Vec<_> = (0..file_count)
            .map(|file| root.path().join(format!("file{file:04}.rs")))
            .collect();
        if tracked {
            git(&["config", "user.name", "test"]);
            git(&["config", "user.email", "test@example.invalid"]);
            for path in &paths {
                std::fs::write(path, "").unwrap();
            }
            git(&["add", "."]);
            git(&["commit", "-qm", "base"]);
        }
        for (file, path) in paths.iter().enumerate() {
            let source: String = (0..count / file_count)
                .map(|line| format!("let value_{file}_{line} = compute({line}, \"hello\");\n"))
                .collect();
            std::fs::write(path, source).unwrap();
        }
        let mut app = TestApp::builder().with_cwd(root.path()).build();
        app.set_terminal_size(120, 40);
        let start = std::time::Instant::now();
        app.type_text("/diff");
        app.press(KeyCode::Enter);
        loop {
            app.feed_one(SourceEvent::LuaWakeup);
            if app.render_to_frame().text().contains("let value_0_0") {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "real Git diff did not become usable"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let usable = start.elapsed();
        app.wait_for_document("smelt.diff.preview", Duration::from_secs(5));
        let highlighted = start.elapsed();
        let buf = app
            .ui_probe()
            .buf(app.ui_probe().win(preview(&app)).unwrap().buf)
            .unwrap();
        let row = buf
            .lines()
            .iter()
            .position(|line| line.contains("let value_0_0"))
            .unwrap();
        assert!(
            buf.highlights_at(row).len() > 3,
            "first viewport has no syntax"
        );
        println!("real Git diff {count} rows {file_count} files tracked={tracked}: usable={usable:?} first_syntax={highlighted:?}");
        let win = preview(&app);
        for (key, action) in [('s', "stage"), ('u', "unstage")] {
            if key == 'u' {
                app.type_char('G');
                paint_diff(&mut app);
                let window = app.ui_probe().win(win).unwrap();
                let row = window.cursor_abs_row();
                assert!(window
                    .row_source()
                    .unwrap()
                    .rows(row..row + 1, app.ui_probe().theme())[0]
                    .text
                    .contains("hello"));
            }
            let tree_rows = file_count as u64 + if key == 's' { 2 } else { 3 };
            let start = std::time::Instant::now();
            app.type_char(key);
            let mut frames = Vec::new();
            let mut max_allocated = 0;
            loop {
                let before = smelt_perf::alloc::thread_snapshot();
                let frame_start = std::time::Instant::now();
                app.feed_one(SourceEvent::LuaWakeup);
                paint_diff(&mut app);
                frames.push(frame_start.elapsed());
                max_allocated = max_allocated.max(
                    smelt_perf::alloc::thread_snapshot()
                        .1
                        .saturating_sub(before.1),
                );
                let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
                let tree = app.ui_probe().win(sidebar).unwrap().row_source().unwrap();
                if tree.snapshot().total_rows == tree_rows {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(15),
                    "{action} did not update sections"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            frames.sort();
            let p95 = frames[frames.len() * 95 / 100];
            println!(
                "  {action}: elapsed={:?} frame_p95={p95:?} max_allocated={max_allocated}",
                start.elapsed()
            );
            assert!(
                p95 < Duration::from_millis(16),
                "index operation blocked a frame"
            );
            assert!(
                max_allocated < 2_000_000,
                "index snapshot swap allocated {max_allocated} bytes"
            );
        }
        app.press(KeyCode::Esc);
        if !cfg!(debug_assertions) {
            assert!(
                usable < Duration::from_secs(1),
                "real Git loading exceeded one second"
            );
        }
    }
}

#[test]
#[ignore = "timed benchmark: cargo xtask bench-diff"]
fn diff_viewer_benchmark() {
    for (count, file_count, replacement) in [
        (10_000, 2, false),
        (1_000_000, 2, false),
        (1_000_000, 1000, false),
        (1_000_000, 10_000, false),
        (1_000_000, 2, true),
        (1_000_000, 10_000, true),
    ] {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(120, 40);
        let start = std::time::Instant::now();
        let per_file = count / file_count;
        app.run_lua_result(&format!(r#"
            local patches = {{}}
            local sources = {{
                {{ 'rs', '+let value = compute(42, "hello 界"); // local change\n' }},
                {{ 'py', '+value = compute(42, "hello 界") # local change\n' }},
                {{ 'ts', '+const value = compute(42, "hello 界"); // local change\n' }},
                {{ 'lua', '+local value = compute(42, "hello 界") -- local change\n' }},
            }}
            for i = 1, {file_count} do
                local source = sources[(i - 1) % #sources + 1]
                local path = string.format('group%03d/file%04d.%s', math.floor((i - 1) / 10), i, source[1])
                if {replacement} then
                    patches[i] = string.format('diff --git a/%s b/%s\n@@ -1,%d +1,%d @@\n', path, path, {per_file} / 2, {per_file} / 2)
                        .. string.rep(source[2]:gsub('^%+', '-'):gsub('local change', 'previous change'), {per_file} / 2)
                        .. string.rep(source[2], {per_file} / 2)
                else
                    patches[i] = string.format('diff --git a/%s b/%s\n@@ -0,0 +1,{per_file} @@\n', path, path)
                        .. string.rep(source[2], {per_file})
                end
            end
            local diff = smelt.diff.parse(table.concat(patches))
            smelt.git.diff = function() return {{ diff = diff }} end
            smelt.cmd.run('diff')
        "#)).unwrap();
        drive_lua_tasks(&mut app);
        paint_diff(&mut app);
        let load = start.elapsed();
        let win = preview(&app);
        let sidebar = app.ui_probe().named_win("smelt.diff.files").unwrap();
        let total = app
            .ui_probe()
            .win(win)
            .unwrap()
            .row_source()
            .unwrap()
            .snapshot()
            .total_rows;
        assert_eq!(total, (count + file_count * 4 - 2) as u64);
        assert!(app.render_to_frame().text().contains("compute"));
        let languages = if file_count == 2 {
            "rs/py"
        } else {
            "rs/py/ts/lua"
        };
        println!(
            "diff {count} rows {file_count} files replacement={replacement}, syntax={languages}: fixture_usable={load:?}"
        );
        for operation in [
            "seek",
            "wheel",
            "file_shortcut",
            "file_click",
            "folder_toggle",
            "split_resize",
            "highlighted_wheel",
        ] {
            if operation == "highlighted_wheel" {
                if app.ui_probe().focus() != Some(win) {
                    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);
                }
                app.type_text("gg");
                paint_diff(&mut app);
                app.wait_for_document("smelt.diff.preview", Duration::from_secs(5));
                let buf = app
                    .ui_probe()
                    .buf(app.ui_probe().win(win).unwrap().buf)
                    .unwrap();
                let row = buf
                    .lines()
                    .iter()
                    .position(|line| line.contains("compute"))
                    .unwrap();
                assert!(
                    buf.highlights_at(row).len() > 3,
                    "warm viewport lacks syntax"
                );
                if replacement {
                    let theme = app.ui_probe().theme().clone();
                    let inline_bg = theme
                        .resolve(smelt_core::theme::intern("SmeltDiffDeleteInlineBg"))
                        .bg;
                    assert!(
                        buf.highlights_at(row)
                            .iter()
                            .any(|span| theme.resolve(span.hl).bg == inline_bg),
                        "warm replacement lacks inline highlights"
                    );
                }
            }
            let mut samples = Vec::new();
            let mut max_allocated = 0;
            let mut collapsed = std::collections::BTreeSet::new();
            for i in 0..200 {
                let target_file = (i * 617 + 37) % file_count + 1;
                let group = (target_file - 1) / 10;
                let tree_row = if operation == "file_click" || operation == "folder_toggle" {
                    // Fixture metadata locates the target before timing a user's
                    // visible click. Offscreen rows are never materialized to search.
                    let row = if operation == "file_click" {
                        target_file + group + 1
                    } else {
                        group * 11 + 1 - collapsed.range(..group).count() * 10
                    };
                    show_tree_row(&mut app, sidebar, row)
                } else {
                    0
                };
                let before = smelt_perf::alloc::thread_snapshot();
                let start = std::time::Instant::now();
                match operation {
                    "seek" => {
                        let row = (i as u64 * 7919 + total / 2) % total;
                        app.type_text(&format!("{}G", row + 1));
                        paint_diff(&mut app);
                        assert_eq!(app.ui_probe().win(win).unwrap().cursor_abs_row(), row);
                    }
                    "wheel" | "highlighted_wheel" => wheel_diff(&mut app, win, i % 20 < 10),
                    "split_resize" => drag_diff_divider(&mut app, if i % 2 == 0 { 4 } else { -4 }),
                    "file_shortcut" => {
                        let focus = app.ui_probe().focus();
                        app.press_mod(
                            KeyCode::Char(if i % 2 == 0 { 'k' } else { 'j' }),
                            KeyModifiers::CONTROL,
                        );
                        paint_diff(&mut app);
                        assert_eq!(app.ui_probe().focus(), focus);
                    }
                    "file_click" => {
                        click_diff_row(&mut app, sidebar, tree_row);
                        assert_eq!(
                            app.ui_probe().win(win).unwrap().cursor_abs_row(),
                            ((target_file - 1) * (per_file + 4)) as u64
                        );
                    }
                    "folder_toggle" => {
                        click_diff_row(&mut app, sidebar, tree_row);
                        if !collapsed.remove(&group) {
                            collapsed.insert(group);
                        }
                        let hidden: usize = collapsed
                            .iter()
                            .map(|group| 10.min(file_count - group * 10))
                            .sum();
                        assert_eq!(
                            app.ui_probe()
                                .win(sidebar)
                                .unwrap()
                                .row_source()
                                .unwrap()
                                .snapshot()
                                .total_rows,
                            (file_count + file_count.div_ceil(10) + 3 - hidden) as u64
                        );
                    }
                    _ => unreachable!(),
                }
                samples.push(start.elapsed());
                let after = smelt_perf::alloc::thread_snapshot();
                max_allocated = max_allocated.max(after.1.saturating_sub(before.1));
                let window = app.ui_probe().win(win).unwrap();
                if operation == "highlighted_wheel" {
                    assert!(!window.row_source().unwrap().is_pending());
                }
                assert!(app.ui_probe().buf(window.buf).unwrap().line_count() <= 40);
            }
            samples.sort();
            println!(
                "  {operation}: p50={:?} p95={:?} max={:?} max_allocated={max_allocated}",
                samples[100], samples[190], samples[199]
            );
            assert!(
                samples[190] < Duration::from_millis(16),
                "{operation} exceeded 60fps budget"
            );
            assert!(
                max_allocated < 2_000_000,
                "{operation} allocated {max_allocated} bytes"
            );
        }
    }
}
