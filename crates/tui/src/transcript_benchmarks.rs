use crate::app::test_harness::*;
use crate::app::AppFocus;
use crate::smelt_edit::VimMode;
use crossterm::event::{KeyCode, KeyModifiers};

fn benchmark_target_enabled() -> bool {
    std::env::var("SMELT_TRANSCRIPT_BENCH_TARGET").as_deref() == Ok("1")
}

fn transcript_row_cursor_row(app: &TestApp) -> crate::smelt_edit::RowIndex {
    app.app
        .transcript_win()
        .row_cursor()
        .expect("row-document transcript cursor")
        .row
}

fn transcript_total_rows(app: &TestApp) -> crate::smelt_edit::RowIndex {
    let win = app.app.transcript_win();
    let buf = app.app.ui.buf(win.buf).expect("transcript buffer");
    win.scroll_row_total(buf)
}

#[test]
fn transcript_layout_projection_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    crate::content::transcript_buf::tests::benchmark_support::run_layout_benchmark();
}

#[test]
fn transcript_true_resume_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    crate::content::transcript_buf::tests::benchmark_support::run_true_resume_benchmark();
}

#[derive(Clone, Copy, Debug)]
struct NavSample {
    search_ms: f64,
    ctrl_d20_ms: f64,
    ctrl_u20_ms: f64,
    gg_ms: f64,
    g_ms: f64,
    previous_user_pill_ms: f64,
    bottom_pill_ms: f64,
    rows: crate::smelt_edit::RowIndex,
}

#[derive(Clone, Copy, Debug)]
struct NavStats {
    mean: f64,
    stddev: f64,
}

impl NavStats {
    fn from(values: &[f64]) -> Self {
        assert!(!values.is_empty(), "benchmark statistics require samples");
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = if values.len() > 1 {
            values
                .iter()
                .map(|value| {
                    let delta = value - mean;
                    delta * delta
                })
                .sum::<f64>()
                / (values.len() - 1) as f64
        } else {
            0.0
        };
        Self {
            mean,
            stddev: variance.sqrt(),
        }
    }

    fn display(self) -> String {
        format!("{:.2}±{:.2}", self.mean, self.stddev)
    }
}

#[derive(Clone, Copy, Debug)]
struct TailStats {
    mean: f64,
    stddev: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

impl TailStats {
    fn from(values: &[f64]) -> Self {
        let basic = NavStats::from(values);
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        Self {
            mean: basic.mean,
            stddev: basic.stddev,
            p50: nearest_rank(&sorted, 50),
            p95: nearest_rank(&sorted, 95),
            p99: nearest_rank(&sorted, 99),
            max: *sorted.last().expect("non-empty benchmark samples"),
        }
    }
}

fn nearest_rank(sorted: &[f64], percentile: usize) -> f64 {
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn elapsed_ms(elapsed: std::time::Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

fn env_positive_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn navigation_bench_runs() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_BENCH_RUNS", 3)
}

fn transcript_navigation_bench_app() -> TestApp {
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    for i in 0..4_000 {
        app.app
            .push_block(smelt_core::transcript_model::Block::User {
                text: format!("navigation bench prompt {i:04}"),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            });
        let marker = if i == 3_777 { " needle-target" } else { "" };
        let content = if i == 3_999 {
            (0..60)
                .map(|line| {
                    format!(
                        "navigation bench final response {line:02}: {}",
                        "alpha beta gamma delta ".repeat(3)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            format!(
                "navigation bench response {i:04}{marker}: {}",
                "alpha beta gamma delta ".repeat(3)
            )
        };
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: content.into(),
            });
    }
    app.render_silent();
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    app
}

fn run_navigation_sample() -> NavSample {
    let mut app = transcript_navigation_bench_app();
    let rows = transcript_total_rows(&app);

    app.type_char('g');
    app.type_char('g');
    let search_start = std::time::Instant::now();
    app.type_char('/');
    app.type_text("needle-target");
    app.press(KeyCode::Enter);
    app.render_silent();
    let search_ms = elapsed_ms(search_start.elapsed());
    assert!(app.app.overlays.search_session().is_some());
    assert!(transcript_row_cursor_row(&app) > rows / 2);

    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let ctrl_d_start = std::time::Instant::now();
    for _ in 0..20 {
        app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL);
        app.render_silent();
    }
    let ctrl_d20_ms = elapsed_ms(ctrl_d_start.elapsed());
    assert!(transcript_row_cursor_row(&app) > 0);

    app.type_char('G');
    app.render_silent();
    let ctrl_u_start = std::time::Instant::now();
    for _ in 0..20 {
        app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL);
        app.render_silent();
    }
    let ctrl_u20_ms = elapsed_ms(ctrl_u_start.elapsed());
    let rows_after_ctrl_u = transcript_total_rows(&app);
    assert!(transcript_row_cursor_row(&app) < rows_after_ctrl_u.saturating_sub(1));

    let gg_start = std::time::Instant::now();
    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let gg_ms = elapsed_ms(gg_start.elapsed());
    assert_eq!(transcript_row_cursor_row(&app), 0);

    let g_start = std::time::Instant::now();
    app.type_char('G');
    app.render_silent();
    let g_ms = elapsed_ms(g_start.elapsed());
    let rows = transcript_total_rows(&app);
    assert!(transcript_row_cursor_row(&app) >= rows.saturating_sub(2));

    let (previous_user_pill_ms, bottom_pill_ms) = measure_scroll_pills(&mut app);

    NavSample {
        search_ms,
        ctrl_d20_ms,
        ctrl_u20_ms,
        gg_ms,
        g_ms,
        previous_user_pill_ms,
        bottom_pill_ms,
        rows,
    }
}

fn search_bench_bytes() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_BENCH_SEARCH_BYTES", 50 * 1024 * 1024)
}

fn transcript_bench_warmup_enabled() -> bool {
    !matches!(
        std::env::var("SMELT_TRANSCRIPT_BENCH_NO_WARMUP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn push_search_bench_transcript(app: &mut TestApp, target_bytes: usize) -> usize {
    let mut approx_bytes = 0usize;
    let mut i = 0usize;
    while approx_bytes < target_bytes {
        let rare = if approx_bytes > target_bytes * 9 / 10 && i.is_multiple_of(17) {
            " needle-target"
        } else {
            ""
        };
        let user = format!(
            "search bench prompt {i}{rare}: {}",
            "common-token transcript search cached navigation ".repeat(10)
        );
        approx_bytes += user.len();
        app.app
            .push_block(smelt_core::transcript_model::Block::User {
                text: user,
                image_labels: vec![],
                command: false,
                sent_at_ms: None,
            });

        let assistant = format!(
            "# Search batch {i}\n\n{}\n\n```rust\nfn search_bench_{i}() {{ println!(\"common-token {i}\"); }}\n```\n\n{}",
            "markdown rendered rows include common-token and wrapping pressure ".repeat(22),
            "tail rows remain searchable through the transcript trigram index ".repeat(18),
        );
        approx_bytes += assistant.len();
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: assistant.into(),
            });

        if i.is_multiple_of(9) {
            let command = format!("python bench_search.py --batch {i}");
            let output = (0..60)
                .map(|j| {
                    format!(
                        "tool result {i}.{j}: {}",
                        "common-token visible materialization exact verification ".repeat(6)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            approx_bytes += command.len() + output.len();
            app.app
                .push_block(smelt_core::transcript_model::Block::Exec {
                    command,
                    output: output.into(),
                });
        }
        i += 1;
    }
    approx_bytes
}

fn transcript_perf_metric(label: &str) -> bool {
    ["search:transcript", "session:", "store:", "transcript:"]
        .iter()
        .any(|prefix| label.starts_with(prefix))
}

fn search_perf_snapshot(label: &str, snapshot: &smelt_perf::perf::Snapshot) {
    for row in snapshot
        .durations
        .iter()
        .filter(|row| transcript_perf_metric(row.label))
    {
        eprintln!(
            "TRANSCRIPT_SEARCH_PERF_DURATION label={} metric={} count={} last_us={} total_us={} p95_us={} max_us={}",
            label, row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
        );
    }
    for row in snapshot
        .values
        .iter()
        .filter(|row| transcript_perf_metric(row.label))
    {
        eprintln!(
            "TRANSCRIPT_SEARCH_PERF_VALUE label={} metric={} count={} last={} total={} p95={} max={}",
            label, row.label, row.count, row.last, row.total, row.p95, row.max
        );
    }
}

fn perf_value_max(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .values
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.max)
        .unwrap_or(0)
}

fn perf_duration_max(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .durations
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.max_us)
        .unwrap_or(0)
}

fn perf_duration_count(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> usize {
    snapshot
        .durations
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.count)
        .unwrap_or(0)
}

fn perf_value_total(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .values
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.total)
        .unwrap_or(0)
}

fn click_named_window(app: &mut TestApp, name: &str) {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let win = app
        .ui_probe()
        .named_win(name)
        .unwrap_or_else(|| panic!("missing benchmark window {name}"));
    let rect = app
        .ui_probe()
        .split_rect(win)
        .or_else(|| {
            app.ui_probe()
                .win(win)
                .and_then(|win| win.viewport.map(|viewport| viewport.rect))
        })
        .unwrap_or_else(|| panic!("missing benchmark window rect {name}"));
    let row = rect.top.saturating_add(rect.height.saturating_sub(1) / 2);
    let column = rect.left.saturating_add(rect.width.saturating_sub(1) / 2);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind,
                row,
                column,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }
}

fn assert_scroll_pill_operation_gates(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    assert_no_full_block_renders_for_scroll(snapshot, label);
    let full_layouts = perf_duration_count(snapshot, "transcript:rebuild_row_index");
    assert_eq!(
        full_layouts, 0,
        "{label} rebuilt the complete transcript height index"
    );
    let exactified_nodes = perf_value_total(snapshot, "transcript:row_index:exactify_missing");
    assert!(
        exactified_nodes <= 128,
        "{label} exactified {exactified_nodes} nodes, expected bounded target and viewport work"
    );
    let materialized_rows = perf_value_total(snapshot, "transcript:collect_nodes_range:rows");
    assert!(
        materialized_rows <= 1_024,
        "{label} materialized {materialized_rows} rows, expected bounded viewport work"
    );
    let reused = perf_value_max(snapshot, "transcript:prepare_row_index:reused_index");
    assert_eq!(reused, 1, "{label} did not reuse the transcript row index");
}

fn measure_scroll_pills(app: &mut TestApp) -> (f64, f64) {
    app.render_silent();
    let tail_scroll = app.app.transcript_win().scroll_top();
    assert!(app
        .ui_probe()
        .named_win("smelt.scroll_pills.top.win")
        .is_some());

    smelt_perf::perf::clear();
    let previous_user_start = std::time::Instant::now();
    click_named_window(app, "smelt.scroll_pills.top.win");
    app.render_silent();
    let previous_user_pill_ms = elapsed_ms(previous_user_start.elapsed());
    let previous_user_snapshot = smelt_perf::perf::snapshot();
    assert_scroll_pill_operation_gates(&previous_user_snapshot, "previous_user_pill");
    assert!(
        app.app.transcript_win().scroll_top() < tail_scroll,
        "previous-user pill did not move above the transcript tail"
    );
    assert!(app
        .ui_probe()
        .named_win("smelt.scroll_pills.bottom.win")
        .is_some());
    app.render_silent();

    smelt_perf::perf::clear();
    let bottom_start = std::time::Instant::now();
    click_named_window(app, "smelt.scroll_pills.bottom.win");
    app.render_silent();
    let bottom_pill_ms = elapsed_ms(bottom_start.elapsed());
    let bottom_snapshot = smelt_perf::perf::snapshot();
    assert_scroll_pill_operation_gates(&bottom_snapshot, "bottom_pill");
    assert!(app.app.transcript_win().is_following_tail());
    assert!(app.app.transcript_win().scroll_top() >= tail_scroll);

    (previous_user_pill_ms, bottom_pill_ms)
}

fn assert_no_full_search_hot_path_reads(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    for metric in [
        "store:history:read_all",
        "store:history:read_all_rows",
        "store:session:load_full_snapshot",
        "store:session:full_snapshot_rows_read",
        "store:transcript:read_records_full",
        "store:transcript:records_full_loaded",
        "transcript:build_from_session:history_items",
    ] {
        let value = perf_value_max(snapshot, metric);
        assert_eq!(
            value, 0,
            "{label} recorded {metric}={value}, expected no full-session search work"
        );
    }
}

fn assert_search_refinement_gates(
    snapshot: &smelt_perf::perf::Snapshot,
    label: &str,
    max_rows_per_refinement: u64,
    max_total_rows: u64,
) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    for metric in [
        "search:transcript:scanned_rows",
        "transcript:display_rows_for_range:rows",
        "transcript:exactified_rows",
    ] {
        let max = perf_value_max(snapshot, metric);
        let total = perf_value_total(snapshot, metric);
        assert!(
            max <= max_rows_per_refinement,
            "{label} recorded {metric} max {max}, expected <= {max_rows_per_refinement}"
        );
        assert!(
            total <= max_total_rows,
            "{label} recorded {metric} total {total}, expected <= {max_total_rows}"
        );
    }
}

fn assert_search_uses_candidate_index(
    snapshot: &smelt_perf::perf::Snapshot,
    label: &str,
    max_index_entries: u64,
) {
    let sqlite_available = perf_value_max(snapshot, "search:transcript:sqlite_available");
    assert_eq!(
        sqlite_available, 1,
        "{label} did not use the persisted transcript search index"
    );
    let manifest_available = perf_value_max(snapshot, "store:lineage:search_manifest_available");
    assert_eq!(
        manifest_available, 1,
        "{label} rebuilt the canonical transcript source list instead of using the search manifest"
    );
    let trigram_build = perf_value_max(snapshot, "search:transcript:index_trigram_build_enabled");
    assert_eq!(
        trigram_build, 0,
        "{label} rebuilt the full transcript trigram index"
    );
    let entries = perf_value_max(snapshot, "search:transcript:index_entries");
    assert!(
        entries <= max_index_entries,
        "{label} indexed {entries} candidate entries, expected <= {max_index_entries}"
    );
}

#[derive(Clone, Copy, Debug)]
struct SearchBenchSample {
    bytes: usize,
    rows: crate::smelt_edit::RowIndex,
    width_resize_ms: f64,
    height_resize_ms: f64,
    theme_color_ms: f64,
    copy_mid_ms: f64,
    nav_ctrl_d20_ms: f64,
    nav_ctrl_u20_ms: f64,
    nav_gg_ms: f64,
    nav_g_ms: f64,
    prod_burst_top_ctrl_d80_ms: f64,
    prod_burst_top_down80_ms: f64,
    prod_burst_bottom_ctrl_u80_ms: f64,
    prod_burst_bottom_up80_ms: f64,
    prod_burst_mid_ctrl_d80_ms: f64,
    prod_burst_mid_down80_ms: f64,
    prod_burst_mid_ctrl_u80_ms: f64,
    prod_burst_mid_up80_ms: f64,
    burst_top_ctrl_d80_ms: f64,
    burst_top_down80_ms: f64,
    burst_bottom_ctrl_u80_ms: f64,
    burst_bottom_up80_ms: f64,
    burst_mid_ctrl_d80_ms: f64,
    burst_mid_down80_ms: f64,
    burst_mid_ctrl_u80_ms: f64,
    burst_mid_up80_ms: f64,
    rare_ms: f64,
    short_common_ms: f64,
    short_absent_ms: f64,
    common_submit_ms: f64,
    next100_ms: f64,
    sparse_rare_ms: f64,
    sparse_common_submit_ms: f64,
    sparse_next100_ms: f64,
    after_append_ms: f64,
}

fn search_bench_metric_values(sample: &SearchBenchSample) -> [(&'static str, f64); 33] {
    [
        ("width_resize", sample.width_resize_ms),
        ("height_resize", sample.height_resize_ms),
        ("theme_color", sample.theme_color_ms),
        ("copy_mid", sample.copy_mid_ms),
        ("nav_ctrl_d20", sample.nav_ctrl_d20_ms),
        ("nav_ctrl_u20", sample.nav_ctrl_u20_ms),
        ("nav_gg", sample.nav_gg_ms),
        ("nav_G", sample.nav_g_ms),
        ("prod_burst_top_ctrl_d80", sample.prod_burst_top_ctrl_d80_ms),
        ("prod_burst_top_down80", sample.prod_burst_top_down80_ms),
        (
            "prod_burst_bottom_ctrl_u80",
            sample.prod_burst_bottom_ctrl_u80_ms,
        ),
        ("prod_burst_bottom_up80", sample.prod_burst_bottom_up80_ms),
        ("prod_burst_mid_ctrl_d80", sample.prod_burst_mid_ctrl_d80_ms),
        ("prod_burst_mid_down80", sample.prod_burst_mid_down80_ms),
        ("prod_burst_mid_ctrl_u80", sample.prod_burst_mid_ctrl_u80_ms),
        ("prod_burst_mid_up80", sample.prod_burst_mid_up80_ms),
        ("burst_top_ctrl_d80", sample.burst_top_ctrl_d80_ms),
        ("burst_top_down80", sample.burst_top_down80_ms),
        ("burst_bottom_ctrl_u80", sample.burst_bottom_ctrl_u80_ms),
        ("burst_bottom_up80", sample.burst_bottom_up80_ms),
        ("burst_mid_ctrl_d80", sample.burst_mid_ctrl_d80_ms),
        ("burst_mid_down80", sample.burst_mid_down80_ms),
        ("burst_mid_ctrl_u80", sample.burst_mid_ctrl_u80_ms),
        ("burst_mid_up80", sample.burst_mid_up80_ms),
        ("rare", sample.rare_ms),
        ("short_common", sample.short_common_ms),
        ("short_absent", sample.short_absent_ms),
        ("common_submit", sample.common_submit_ms),
        ("next100", sample.next100_ms),
        ("sparse_rare", sample.sparse_rare_ms),
        ("sparse_common_submit", sample.sparse_common_submit_ms),
        ("sparse_next100", sample.sparse_next100_ms),
        ("after_append", sample.after_append_ms),
    ]
}

fn assert_no_full_block_renders_for_scroll(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    let metric = "transcript:layout_cache:render_full_to_buffer";
    let full_renders = perf_duration_count(snapshot, metric);
    assert_eq!(
        full_renders, 0,
        "{label} recorded {metric} {full_renders} times while scrolling; row anchors must stay lightweight"
    );
}

fn assert_view_operation_gates(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    let materialized_rows = perf_value_total(snapshot, "transcript:collect_nodes_range:rows");
    assert!(
        materialized_rows <= 1024,
        "{label} materialized {materialized_rows} rows, expected bounded viewport work"
    );
    let prepared = perf_value_max(snapshot, "transcript:prepare_row_index:reused_index");
    assert_eq!(
        prepared, 1,
        "{label} rebuilt the row index instead of reusing it"
    );
    let rebuild_us = perf_duration_max(snapshot, "transcript:prepare_row_index:rebuild_index");
    assert_eq!(
        rebuild_us, 0,
        "{label} rebuilt the full row index in {rebuild_us}us"
    );
    if matches!(label, "nav_ctrl_d20" | "nav_ctrl_u20") {
        assert_no_full_block_renders_for_scroll(snapshot, label);
    }
}

fn assert_copy_operation_gates(
    snapshot: &smelt_perf::perf::Snapshot,
    label: &str,
    max_requested_rows: u64,
) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    let exactified_rows = perf_value_total(snapshot, "transcript:exactified_rows");
    assert!(
        exactified_rows <= max_requested_rows,
        "{label} exactified {exactified_rows} rows, expected <= {max_requested_rows}"
    );
    let materialized_rows = perf_value_total(snapshot, "transcript:collect_nodes_range:rows");
    assert!(
        materialized_rows <= 256,
        "{label} materialized {materialized_rows} rows, expected bounded copy work"
    );
    let prepared = perf_value_max(snapshot, "transcript:prepare_row_index:reused_index");
    assert_eq!(
        prepared, 1,
        "{label} rebuilt the row index instead of reusing it"
    );
    let rebuild_us = perf_duration_max(snapshot, "transcript:prepare_row_index:rebuild_index");
    assert_eq!(
        rebuild_us, 0,
        "{label} rebuilt the full row index in {rebuild_us}us"
    );
}

fn assert_burst_operation_gates(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    let materialized_rows = perf_value_total(snapshot, "transcript:collect_nodes_range:rows");
    assert!(
        materialized_rows <= 4096,
        "{label} materialized {materialized_rows} rows, expected bounded viewport work"
    );
    assert_no_full_block_renders_for_scroll(snapshot, label);
}

fn assert_stream_operation_gates(
    snapshot: &smelt_perf::perf::Snapshot,
    memory: crate::app::transcript::TranscriptMemorySnapshot,
    label: &str,
) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    assert_no_full_block_renders_for_scroll(snapshot, label);
    let repair_metric = "session:catalog:repair";
    let repair_count = perf_duration_count(snapshot, repair_metric);
    assert_eq!(
        repair_count, 0,
        "{label} recorded {repair_metric} {repair_count} times on the streaming hot path"
    );
    for metric in [
        "session:catalog:reconcile_scanned",
        "session:catalog:reconciliation_duration_ms",
    ] {
        assert_eq!(
            perf_value_total(snapshot, metric),
            0,
            "{label} recorded {metric} on the streaming hot path"
        );
    }
    assert!(
        memory.rendered_oversize_debt_bytes <= 4 * 1024 * 1024,
        "{label} accumulated excessive rendered cache oversize debt: {memory:?}"
    );
}

#[derive(Clone, Copy, Debug)]
enum BurstBenchKey {
    CtrlD,
    CtrlU,
    Down,
    Up,
}

impl BurstBenchKey {
    fn press(self, app: &mut TestApp) {
        match self {
            Self::CtrlD => app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Self::CtrlU => app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Self::Down => app.press(KeyCode::Down),
            Self::Up => app.press(KeyCode::Up),
        }
    }

    fn max_rows_per_event(self, viewport_rows: u16) -> crate::smelt_edit::RowIndex {
        match self {
            Self::CtrlD | Self::CtrlU => crate::smelt_edit::RowIndex::from(viewport_rows.max(1)),
            Self::Down | Self::Up => 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum BurstBenchPosition {
    Top,
    Middle,
    Bottom,
}

fn save_bench_fixture(app: &mut TestApp, label: &str) -> smelt_store::SaveReceipt {
    app.app.save_session();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let outcome = app.app.flush_persist();
        match outcome {
            crate::persist::PersistenceFlushOutcome::Durable {
                receipt: Some(receipt),
                ..
            } => break receipt,
            crate::persist::PersistenceFlushOutcome::Deadline { .. }
                if std::time::Instant::now() < deadline => {}
            other => panic!("{label} fixture persistence failed: {other:?}"),
        }
    }
}

fn wait_for_bench_catalog(app: &TestApp, label: &str, receipt: &smelt_store::SaveReceipt) {
    let timeout = std::time::Duration::from_secs(120);
    assert!(
        app.app.core.sessions.wait_for_session_catalog(timeout),
        "{label} catalog worker did not complete queued projection work"
    );

    let session_id = &app.app.conversation.session().id;
    let catalog_path = app.app.core.sessions.layout().catalog_path();
    let expected_revision = receipt.current.revision.get();
    let catalog_current = smelt_store::CatalogReader::open_existing(&catalog_path)
        .ok()
        .flatten()
        .and_then(|catalog| catalog.session(session_id).ok().flatten())
        .is_some_and(|session| session.source_revision == expected_revision);
    assert!(
        catalog_current,
        "{label} catalog row did not reach canonical revision {expected_revision}"
    );
}

fn wait_for_bench_search_projection(app: &TestApp, label: &str) {
    assert!(
        app.app.conversation.request_search_projection(),
        "{label} search projection request was not accepted"
    );
    let address = app
        .app
        .conversation
        .transcript()
        .store_address()
        .expect("benchmark transcript has a store address");
    let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
        &address.sessions_root,
        &address.lineage_id,
        &address.session_id,
    )
    .expect("open benchmark search projection reader");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let status = reader
            .search_projection_status()
            .expect("read benchmark search projection status");
        if status.state == smelt_store::SearchProjectionState::Current {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{label} search projection did not become current: {status:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn wait_for_sparse_reader_metrics(
    app: &mut TestApp,
    label: &str,
) -> crate::app::TranscriptReaderMetrics {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        app.render_silent();
        let metrics = app.app.transcript_reader_metrics_for_harness();
        if metrics.metadata_readers == 1 && metrics.hydration_readers == 1 {
            assert_eq!(metrics.total_readers, 2);
            return metrics;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{label} did not retain both transcript readers: {metrics:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn install_sparse_resume_bench_transcript(app: &mut TestApp) {
    let loaded = crate::app::history::load_transcript_tail_from_sqlite_id(
        &app.app.core.sessions,
        &app.app.conversation.session().id,
        100,
        32,
    )
    .expect("load sparse bench transcript tail");
    app.app.clear_transcript();
    app.app
        .conversation
        .replace_loaded_transcript_for_harness(loaded);
    app.app.handle_resize(100, 32);
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    app.render_silent();
}

fn prepare_burst_bench_position(app: &mut TestApp, position: BurstBenchPosition) {
    match position {
        BurstBenchPosition::Top => {
            app.type_char('g');
            app.type_char('g');
        }
        BurstBenchPosition::Middle => {
            let record = app
                .app
                .conversation
                .transcript()
                .record_total_count()
                .expect("sparse record count")
                / 2;
            let block_id = smelt_core::transcript_model::BlockId::new(record as u64);
            assert!(
                app.app
                    .reveal_transcript_target_at_top(record, block_id, 1, true),
                "middle record reveal failed for record {record} and block {block_id:?}"
            );
        }
        BurstBenchPosition::Bottom => {
            app.type_char('G');
        }
    }
    app.render_silent();
}

fn assert_burst_bench_projection_bounded(
    label: &str,
    frames: &[crate::app::transcript_scroll_trace::TranscriptScrollTraceFrame],
    before_scroll: crate::smelt_edit::RowIndex,
    max_input_rows: crate::smelt_edit::RowIndex,
    max_projection_delta: crate::smelt_edit::RowIndex,
    strict_projection_delta: bool,
) {
    let mut checked = 0usize;
    for frame in frames {
        let crate::app::transcript_scroll_trace::TranscriptScrollIntent::UserDelta { rows } =
            frame.scroll_intent
        else {
            continue;
        };
        let input_rows = rows.unsigned_abs() as crate::smelt_edit::RowIndex;
        assert!(
            input_rows <= max_input_rows,
            "{label} over-accumulated a single burst input: input_rows={input_rows}, max_input_rows={max_input_rows}, frame={frame:?}"
        );
        let Some(target) = frame.projection_target.exact_target_row() else {
            panic!("{label} projected user delta through a non-exact target: {frame:?}");
        };
        if strict_projection_delta {
            let delta = target.abs_diff(before_scroll);
            assert!(
                delta <= max_projection_delta,
                "{label} over-accumulated burst navigation: before_scroll={before_scroll}, target={target}, delta={delta}, max_delta={max_projection_delta}, frame={frame:?}"
            );
        }
        checked += 1;
    }
    assert!(
        checked > 0,
        "{label} produced no user-delta projection frames: {frames:?}"
    );
}

fn measure_transcript_burst_operation(
    app: &mut TestApp,
    label: &'static str,
    report_perf: bool,
    position: BurstBenchPosition,
    key: BurstBenchKey,
) -> f64 {
    const BURST_EVENTS: usize = 80;

    prepare_burst_bench_position(app, position);
    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport")
        .rect
        .height
        .max(1);
    let before_scroll = app.app.transcript_win().scroll_top();
    app.app
        .conversation
        .set_transcript_scroll_trace_timings_for_harness(true);
    app.app
        .conversation
        .take_transcript_scroll_trace_frames_for_harness();
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    for _ in 0..BURST_EVENTS {
        key.press(app);
    }
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        before_scroll,
        "{label} mutated Window::scroll_top before projection"
    );
    app.render_silent();
    let ms = elapsed_ms(start.elapsed());
    let frames = app
        .app
        .conversation
        .take_transcript_scroll_trace_frames_for_harness();
    let max_input_rows = key.max_rows_per_event(viewport_rows);
    let max_projection_delta = max_input_rows
        .saturating_mul(BURST_EVENTS as crate::smelt_edit::RowIndex)
        .saturating_add(crate::smelt_edit::RowIndex::from(viewport_rows).saturating_mul(2));
    assert_burst_bench_projection_bounded(
        label,
        &frames,
        before_scroll,
        max_input_rows,
        max_projection_delta,
        matches!(position, BurstBenchPosition::Top),
    );
    let snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot(label, &snapshot);
    }
    assert_burst_operation_gates(&snapshot, label);
    ms
}

fn measure_transcript_burst_operation_without_trace(
    app: &mut TestApp,
    label: &'static str,
    report_perf: bool,
    position: BurstBenchPosition,
    key: BurstBenchKey,
) -> f64 {
    const BURST_EVENTS: usize = 80;

    prepare_burst_bench_position(app, position);
    let before_scroll = app.app.transcript_win().scroll_top();
    app.app
        .conversation
        .set_transcript_scroll_trace_for_harness(false);
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    for _ in 0..BURST_EVENTS {
        key.press(app);
    }
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        before_scroll,
        "{label} mutated Window::scroll_top before projection"
    );
    app.render_silent();
    let ms = elapsed_ms(start.elapsed());
    let snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot(label, &snapshot);
    }
    assert_burst_operation_gates(&snapshot, label);
    ms
}

fn measure_transcript_view_operation(
    app: &mut TestApp,
    label: &'static str,
    report_perf: bool,
    operation: impl FnOnce(&mut TestApp),
) -> f64 {
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    operation(app);
    app.render_silent();
    let ms = elapsed_ms(start.elapsed());
    let snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot(label, &snapshot);
    }
    assert_view_operation_gates(&snapshot, label);
    ms
}

fn measure_transcript_copy_operation(
    app: &mut TestApp,
    label: &'static str,
    report_perf: bool,
    start_row: crate::smelt_edit::RowIndex,
    copied_rows: crate::smelt_edit::RowIndex,
) -> f64 {
    let end_row = start_row.saturating_add(copied_rows.saturating_sub(1));
    let range = crate::smelt_edit::DocRange {
        start: crate::smelt_edit::DocPosition {
            row: start_row,
            byte_col: 0,
        },
        end: crate::smelt_edit::DocPosition {
            row: end_row,
            byte_col: usize::MAX,
        },
    };
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    let out = app
        .app
        .copy_document_rows(crate::app::TRANSCRIPT_WIN, range)
        .expect("copy transcript rows");
    let ms = elapsed_ms(start.elapsed());
    assert!(!out.clipboard.is_empty());
    assert!(!out.kill_ring.is_empty());
    let snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot(label, &snapshot);
    }
    assert_copy_operation_gates(&snapshot, label, copied_rows.saturating_add(1));
    ms
}

fn measure_sparse_search_navigation(app: &mut TestApp, report_perf: bool) -> (f64, f64, f64) {
    app.app
        .conversation
        .set_transcript_scroll_trace_for_harness(false);
    smelt_perf::perf::clear();
    let rare_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "needle-target".into(),
    );
    app.render_silent();
    let rare_ms = elapsed_ms(rare_start.elapsed());
    let rare_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("sparse_rare_cold", &rare_snapshot);
    }
    assert_no_full_search_hot_path_reads(&rare_snapshot, "sparse_rare_cold");

    smelt_perf::perf::clear();
    let common_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "common-token".into(),
    );
    app.render_silent();
    let common_submit_ms = elapsed_ms(common_start.elapsed());

    let next_start = std::time::Instant::now();
    for _ in 0..100 {
        app.type_char('n');
        app.render_silent();
    }
    let next100_ms = elapsed_ms(next_start.elapsed());
    let common_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("sparse_common_hot_next100", &common_snapshot);
    }
    assert_no_full_search_hot_path_reads(&common_snapshot, "sparse_common_hot_next100");
    (rare_ms, common_submit_ms, next100_ms)
}

fn resumed_wheel_bench_bytes() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_RESUMED_WHEEL_BYTES", 5 * 1024 * 1024)
}

fn resumed_wheel_bench_frames() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_RESUMED_WHEEL_FRAMES", 240)
}

fn resumed_wheel_bench_ticks_per_frame() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_RESUMED_WHEEL_TICKS", 24)
}

fn run_resumed_wheel_scroll_bench_sample(
    target_bytes: usize,
    frames: usize,
    ticks_per_frame: usize,
) {
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let bytes = push_search_bench_transcript(&mut app, target_bytes);
    app.render_silent();
    let receipt = save_bench_fixture(&mut app, "resumed wheel");
    wait_for_bench_catalog(&app, "resumed wheel", &receipt);

    install_sparse_resume_bench_transcript(&mut app);
    app.type_char('G');
    app.render_silent();
    app.app
        .conversation
        .set_transcript_scroll_trace_timings_for_harness(true);
    app.app
        .conversation
        .take_transcript_scroll_trace_frames_for_harness();

    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let start = std::time::Instant::now();
    for _ in 0..frames {
        for _ in 0..ticks_per_frame {
            app.transcript_scroll_probe_wheel(false, 1);
        }
        app.render_silent();
    }
    let ms = elapsed_ms(start.elapsed());
    let snapshot = smelt_perf::perf::snapshot();
    smelt_perf::perf::set_enabled(false);
    let trace_frames = app
        .app
        .conversation
        .take_transcript_scroll_trace_frames_for_harness();
    let record_loads = perf_duration_count(&snapshot, "store:transcript:read_record_slice");
    let record_load_us = duration_total_us(&snapshot, "store:transcript:read_record_slice");
    let row_rebuilds = perf_duration_count(&snapshot, "transcript:prepare_row_index:rebuild_index");
    let row_rebuild_us = duration_total_us(&snapshot, "transcript:prepare_row_index:rebuild_index");
    search_perf_snapshot("resumed_wheel_scroll", &snapshot);
    eprintln!(
        "TRANSCRIPT_RESUMED_WHEEL_SAMPLE bytes={} frames={} ticks_per_frame={} trace_frames={} wall_ms={:.3} record_loads={} record_load_ms={:.3} row_rebuilds={} row_rebuild_ms={:.3}",
        bytes,
        frames,
        ticks_per_frame,
        trace_frames.len(),
        ms,
        record_loads,
        record_load_us as f64 / 1000.0,
        row_rebuilds,
        row_rebuild_us as f64 / 1000.0,
    );
    eprintln!(
        "TRANSCRIPT_RESUMED_WHEEL_JSON {{\"type\":\"resumed_wheel_summary\",\"bytes\":{},\"frames\":{},\"ticks_per_frame\":{},\"trace_frames\":{},\"wall_ms\":{:.3},\"record_loads\":{},\"record_load_ms\":{:.3},\"row_rebuilds\":{},\"row_rebuild_ms\":{:.3}}}",
        bytes,
        frames,
        ticks_per_frame,
        trace_frames.len(),
        ms,
        record_loads,
        record_load_us as f64 / 1000.0,
        row_rebuilds,
        row_rebuild_us as f64 / 1000.0,
    );
}

#[test]
fn transcript_resumed_wheel_scroll_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    run_resumed_wheel_scroll_bench_sample(
        resumed_wheel_bench_bytes(),
        resumed_wheel_bench_frames(),
        resumed_wheel_bench_ticks_per_frame(),
    );
}

fn run_search_bench_sample(target_bytes: usize, report_perf: bool) -> SearchBenchSample {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let bytes = push_search_bench_transcript(&mut app, target_bytes);
    app.render_silent();
    let receipt = save_bench_fixture(&mut app, "search");
    wait_for_bench_catalog(&app, "search", &receipt);
    wait_for_bench_search_projection(&app, "search");
    smelt_perf::perf::clear();
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    let rows = transcript_total_rows(&app);

    let width_resize_ms =
        measure_transcript_view_operation(&mut app, "resize_width", report_perf, |app| {
            app.app.handle_resize(140, 32)
        });
    let height_resize_ms =
        measure_transcript_view_operation(&mut app, "resize_height", report_perf, |app| {
            app.app.handle_resize(140, 48)
        });
    let theme_color_ms =
        measure_transcript_view_operation(&mut app, "theme_color", report_perf, |app| {
            app.app.mutate_theme(|theme| {
                theme.set(
                    "SmeltAccent",
                    smelt_core::style::Style::new().fg(smelt_core::style::Color::Rgb {
                        r: 190,
                        g: 120,
                        b: 255,
                    }),
                );
            });
        });
    app.app.handle_resize(100, 32);
    app.render_silent();

    let copy_start_row = rows.saturating_mul(2) / 3;
    let copy_mid_ms = measure_transcript_copy_operation(
        &mut app,
        "copy_mid_rows",
        report_perf,
        copy_start_row,
        8,
    );

    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let nav_ctrl_d20_ms =
        measure_transcript_view_operation(&mut app, "nav_ctrl_d20", report_perf, |app| {
            for _ in 0..20 {
                app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL);
                app.render_silent();
            }
        });
    assert!(transcript_row_cursor_row(&app) > 0);

    app.type_char('G');
    app.render_silent();
    let nav_ctrl_u20_ms =
        measure_transcript_view_operation(&mut app, "nav_ctrl_u20", report_perf, |app| {
            for _ in 0..20 {
                app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL);
                app.render_silent();
            }
        });
    assert!(transcript_row_cursor_row(&app) < rows.saturating_sub(1));

    let nav_gg_ms = measure_transcript_view_operation(&mut app, "nav_gg", report_perf, |app| {
        app.type_char('g');
        app.type_char('g');
    });
    assert_eq!(transcript_row_cursor_row(&app), 0);

    let nav_g_ms = measure_transcript_view_operation(&mut app, "nav_G", report_perf, |app| {
        app.type_char('G');
    });
    let row = transcript_row_cursor_row(&app);
    let total_rows_after_g = app.app.transcript_total_rows();
    assert_eq!(
        row,
        total_rows_after_g.saturating_sub(1),
        "G should land on the final transcript row"
    );

    app.type_char('g');
    app.type_char('g');
    app.render_silent();

    smelt_perf::perf::clear();
    let rare_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "needle-target".into(),
    );
    app.render_silent();
    let rare_ms = elapsed_ms(rare_start.elapsed());
    assert!(transcript_row_cursor_row(&app) > rows / 2);
    let rare_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("rare_cold", &rare_snapshot);
    }
    assert_search_refinement_gates(&rare_snapshot, "rare_cold", 80, 1024);
    assert_search_uses_candidate_index(&rare_snapshot, "rare_cold", 1024);

    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    smelt_perf::perf::clear();
    let short_common_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "e".into(),
    );
    app.render_silent();
    let short_common_ms = elapsed_ms(short_common_start.elapsed());
    let short_common_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("short_common", &short_common_snapshot);
    }
    assert_search_uses_candidate_index(&short_common_snapshot, "short_common", 512);
    assert_eq!(
        perf_value_max(&short_common_snapshot, "store:transcript:search_fts"),
        0,
        "short_common unexpectedly used FTS"
    );

    smelt_perf::perf::clear();
    let short_absent_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "§".into(),
    );
    app.render_silent();
    let short_absent_ms = elapsed_ms(short_absent_start.elapsed());
    let short_absent_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("short_absent", &short_absent_snapshot);
    }
    assert_search_uses_candidate_index(&short_absent_snapshot, "short_absent", 0);
    assert_eq!(
        perf_value_max(&short_absent_snapshot, "store:transcript:search_fts"),
        0,
        "short_absent unexpectedly used FTS"
    );

    smelt_perf::perf::clear();
    let common_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "common-token".into(),
    );
    app.render_silent();
    let common_submit_ms = elapsed_ms(common_start.elapsed());

    let next_start = std::time::Instant::now();
    for _ in 0..100 {
        app.type_char('n');
        app.render_silent();
    }
    let next100_ms = elapsed_ms(next_start.elapsed());
    let common_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("common_hot_next100", &common_snapshot);
    }
    assert_search_refinement_gates(&common_snapshot, "common_hot_next100", 512, 32_000);
    assert_search_uses_candidate_index(&common_snapshot, "common_hot_next100", 512);
    let scanned_entries = perf_value_total(&common_snapshot, "search:transcript:scanned_entries");
    assert!(
        scanned_entries <= 64,
        "common_hot_next100 scanned {scanned_entries} entries; cached next navigation should not rescan every press"
    );

    smelt_perf::perf::clear();
    app.app
        .push_block(smelt_core::transcript_model::Block::Text {
            content: "append after index common-token incremental-index-probe".into(),
        });
    let after_append_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "incremental-index-probe".into(),
    );
    app.render_silent();
    let after_append_ms = elapsed_ms(after_append_start.elapsed());
    let after_append_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("after_append", &after_append_snapshot);
    }
    assert_search_refinement_gates(&after_append_snapshot, "after_append", 80, 160);
    assert_search_uses_candidate_index(&after_append_snapshot, "after_append", 1);
    let dirty_scanned = perf_value_max(
        &after_append_snapshot,
        "search:transcript:dirty_candidates_scanned",
    );
    assert!(
        dirty_scanned <= 1,
        "after_append scanned {dirty_scanned} dirty blocks, expected only the appended suffix"
    );

    install_sparse_resume_bench_transcript(&mut app);
    let prod_burst_top_ctrl_d80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_top_ctrl_d80",
        report_perf,
        BurstBenchPosition::Top,
        BurstBenchKey::CtrlD,
    );
    let prod_burst_top_down80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_top_down80",
        report_perf,
        BurstBenchPosition::Top,
        BurstBenchKey::Down,
    );
    let prod_burst_bottom_ctrl_u80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_bottom_ctrl_u80",
        report_perf,
        BurstBenchPosition::Bottom,
        BurstBenchKey::CtrlU,
    );
    let prod_burst_bottom_up80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_bottom_up80",
        report_perf,
        BurstBenchPosition::Bottom,
        BurstBenchKey::Up,
    );
    let prod_burst_mid_ctrl_d80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_mid_ctrl_d80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::CtrlD,
    );
    let prod_burst_mid_down80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_mid_down80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::Down,
    );
    let prod_burst_mid_ctrl_u80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_mid_ctrl_u80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::CtrlU,
    );
    let prod_burst_mid_up80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_mid_up80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::Up,
    );

    install_sparse_resume_bench_transcript(&mut app);
    let burst_top_ctrl_d80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_top_ctrl_d80",
        report_perf,
        BurstBenchPosition::Top,
        BurstBenchKey::CtrlD,
    );
    let burst_top_down80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_top_down80",
        report_perf,
        BurstBenchPosition::Top,
        BurstBenchKey::Down,
    );
    let burst_bottom_ctrl_u80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_bottom_ctrl_u80",
        report_perf,
        BurstBenchPosition::Bottom,
        BurstBenchKey::CtrlU,
    );
    let burst_bottom_up80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_bottom_up80",
        report_perf,
        BurstBenchPosition::Bottom,
        BurstBenchKey::Up,
    );
    let burst_mid_ctrl_d80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_mid_ctrl_d80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::CtrlD,
    );
    let burst_mid_down80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_mid_down80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::Down,
    );
    let burst_mid_ctrl_u80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_mid_ctrl_u80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::CtrlU,
    );
    let burst_mid_up80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_mid_up80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::Up,
    );

    install_sparse_resume_bench_transcript(&mut app);
    let (sparse_rare_ms, sparse_common_submit_ms, sparse_next100_ms) =
        measure_sparse_search_navigation(&mut app, report_perf);
    smelt_perf::perf::set_enabled(false);

    SearchBenchSample {
        bytes,
        rows,
        width_resize_ms,
        height_resize_ms,
        theme_color_ms,
        copy_mid_ms,
        nav_ctrl_d20_ms,
        nav_ctrl_u20_ms,
        nav_gg_ms,
        nav_g_ms,
        prod_burst_top_ctrl_d80_ms,
        prod_burst_top_down80_ms,
        prod_burst_bottom_ctrl_u80_ms,
        prod_burst_bottom_up80_ms,
        prod_burst_mid_ctrl_d80_ms,
        prod_burst_mid_down80_ms,
        prod_burst_mid_ctrl_u80_ms,
        prod_burst_mid_up80_ms,
        burst_top_ctrl_d80_ms,
        burst_top_down80_ms,
        burst_bottom_ctrl_u80_ms,
        burst_bottom_up80_ms,
        burst_mid_ctrl_d80_ms,
        burst_mid_down80_ms,
        burst_mid_ctrl_u80_ms,
        burst_mid_up80_ms,
        rare_ms,
        short_common_ms,
        short_absent_ms,
        common_submit_ms,
        next100_ms,
        sparse_rare_ms,
        sparse_common_submit_ms,
        sparse_next100_ms,
        after_append_ms,
    }
}

fn resume_bench_session_count() -> usize {
    env_positive_usize("SMELT_RESUME_BENCH_SESSIONS", 1024)
}

fn resume_bench_preview_bytes() -> usize {
    env_positive_usize("SMELT_RESUME_BENCH_PREVIEW_BYTES", 5 * 1024 * 1024)
}

fn seed_resume_bench_session(id: String, updated_at_ms: u64, target_text_bytes: usize, cwd: &str) {
    let mut session = smelt_core::session::Session::new(4242, std::path::PathBuf::from(cwd));
    session.id = id.clone();
    session.title = Some(format!("resume bench {id}"));
    session.first_user_message = Some(format!("open resume dialog for {id}"));
    session.created_at_ms = updated_at_ms;
    session.updated_at_ms = updated_at_ms;
    let mut bytes = 0usize;
    let mut turn = 0usize;
    while bytes < target_text_bytes {
        let user = format!("resume bench prompt {turn}: {}", "preview seed ".repeat(8));
        bytes += user.len();
        session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(user)));

        let assistant = format!(
            "# Resume bench reply {turn}\n\n{}\n\n```text\n{}\n```\n",
            "large preview transcript body ".repeat(80),
            "tail preview row ".repeat(80)
        );
        bytes += assistant.len();
        session.history.push(protocol::HistoryItem::Assistant(
            protocol::AssistantStep::terminal(
                Some(protocol::Content::text(assistant)),
                None,
                Vec::new(),
            ),
        ));
        turn += 1;
    }
    smelt_core::session::save(&session);
}

fn seed_resume_bench_sessions(count: usize) {
    let base_time = 1_700_000_000_000u64;
    for i in 0..count {
        let id = format!("{i:064x}");
        seed_resume_bench_session(id, base_time + i as u64, 128, "/resume-bench-other");
    }
}

fn seed_resume_bench_preview_session(
    guard: &smelt_test_support::ProcessEnvironmentGuard,
    preview_bytes: usize,
) -> (String, usize) {
    let mut app = TestApp::builder()
        .with_vim(true)
        .build_with_test_home_guard(guard);
    app.app.handle_resize(120, 32);
    let bytes = push_search_bench_transcript(&mut app, preview_bytes);
    app.render_silent();
    let receipt = save_bench_fixture(&mut app, "resume preview");
    wait_for_bench_catalog(&app, "resume preview", &receipt);
    let id = app.app.conversation.session().id.clone();
    assert!(
        crate::app::history::load_transcript_tail_from_sqlite_id(
            &app.app.core.sessions,
            &id,
            80,
            12,
        )
        .is_some(),
        "resume bench preview session should have sparse transcript records"
    );
    (id, bytes)
}

fn run_resume_command_to_dialog(app: &mut TestApp) -> f64 {
    let start = std::time::Instant::now();
    assert!(app.run_lua(r#"smelt.cmd.run("resume")"#));
    app.settle_lua();
    app.render_silent();
    elapsed_ms(start.elapsed())
}

fn run_resume_preview_timer(app: &mut TestApp) -> f64 {
    let start = std::time::Instant::now();
    app.feed_one(SourceEvent::Tick(50));
    app.app.tick_timers();
    app.settle_lua();
    app.render_silent();
    elapsed_ms(start.elapsed())
}

fn duration_count(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> usize {
    snapshot
        .durations
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.count)
        .unwrap_or(0)
}

fn duration_total_us(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .durations
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.total_us)
        .unwrap_or(0)
}

fn print_resume_perf(label: &str, snapshot: &smelt_perf::perf::Snapshot) {
    for row in snapshot.durations.iter().filter(|row| {
        row.label.starts_with("session:")
            || row.label.starts_with("store:lineage:")
            || row.label == "cmd:dispatch"
            || row.label == "lua:cmd"
            || row.label == "lua:timer"
    }) {
        eprintln!(
            "RESUME_DIALOG_PERF_DURATION label={} metric={} count={} total_us={} p95_us={} max_us={}",
            label, row.label, row.count, row.total_us, row.p95_us, row.max_us
        );
    }
}

#[test]
fn resume_dialog_open_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    if std::env::var("SMELT_RESUME_BENCH").ok().as_deref() != Some("1") {
        eprintln!("RESUME_DIALOG_BENCH_SKIPPED");
        return;
    }

    let guard = test_home_guard();
    let count = resume_bench_session_count();
    let preview_bytes = resume_bench_preview_bytes();
    let (latest_id, seeded_preview_bytes) =
        seed_resume_bench_preview_session(&guard, preview_bytes);
    seed_resume_bench_sessions(count.saturating_sub(1));
    let mut app = TestApp::builder()
        .with_vim(true)
        .build_without_test_home_reset(&guard);
    app.app.handle_resize(120, 32);
    assert!(
        app.app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(120)),
        "resume benchmark catalog did not finish indexing seeded sessions"
    );

    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let open_ms = run_resume_command_to_dialog(&mut app);
    let open_snapshot = smelt_perf::perf::snapshot();
    print_resume_perf("open", &open_snapshot);
    assert!(
        app.state().active_modal.is_some(),
        "resume command did not open a dialog"
    );
    let open_ro = duration_count(&open_snapshot, "store:lineage:open_read_only");
    assert!(
        open_ro <= 8,
        "opening /resume opened {open_ro} read-only sqlite databases for {count} sessions; listing must not be proportional to session count"
    );
    let open_rw = duration_count(&open_snapshot, "store:lineage:open_read_write");
    assert!(
        open_rw <= 8,
        "opening /resume opened {open_rw} read-write sqlite databases for {count} sessions; listing must not backfill sidecars on the foreground path"
    );
    assert_eq!(
        duration_count(&open_snapshot, "session:load_full"),
        0,
        "resume preview must use sparse sqlite transcript records, not full session load"
    );
    assert_eq!(
        duration_count(&open_snapshot, "session:list_page"),
        count.div_ceil(500),
        "resume should load each catalog page exactly once"
    );

    smelt_perf::perf::clear();
    let preview_ms = run_resume_preview_timer(&mut app);
    let preview_snapshot = smelt_perf::perf::snapshot();
    print_resume_perf("preview", &preview_snapshot);
    smelt_perf::perf::set_enabled(false);
    assert_eq!(
        duration_count(&preview_snapshot, "session:load_full"),
        0,
        "delayed resume preview refresh must not full-load the selected session"
    );
    let preview_render_count = duration_count(&open_snapshot, "session:render_preview_into")
        + duration_count(&preview_snapshot, "session:render_preview_into");
    assert!(
        preview_render_count > 0,
        "resume dialog did not render the selected session preview"
    );

    eprintln!(
        "RESUME_DIALOG_BENCH_SUMMARY sessions={} latest_id={} preview_bytes={} open_ms={:.3} preview_ms={:.3} open_db_ro={} open_db_rw={} open_session_list_us={} preview_render_us={}",
        count,
        latest_id,
        seeded_preview_bytes,
        open_ms,
        preview_ms,
        duration_count(&open_snapshot, "store:lineage:open_read_only"),
        duration_count(&open_snapshot, "store:lineage:open_read_write"),
        duration_total_us(&open_snapshot, "session:list_page"),
        duration_total_us(&open_snapshot, "session:render_preview_into")
            + duration_total_us(&preview_snapshot, "session:render_preview_into"),
    );
}

#[test]
fn transcript_layout_search_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    if std::env::var("SMELT_TRANSCRIPT_BENCH_SEARCH")
        .ok()
        .as_deref()
        != Some("1")
    {
        eprintln!("TRANSCRIPT_SEARCH_BENCH_SKIPPED");
        return;
    }
    let runs = navigation_bench_runs();
    let target_bytes = search_bench_bytes();
    if transcript_bench_warmup_enabled() {
        let _warmup = run_search_bench_sample(target_bytes, false);
    }
    let mut samples = Vec::with_capacity(runs);
    for run in 0..runs {
        let sample = run_search_bench_sample(target_bytes, true);
        let metric_text = search_bench_metric_values(&sample)
            .into_iter()
            .map(|(label, value)| format!("{label}_ms={value:.3}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!(
            "TRANSCRIPT_SEARCH_BENCH_SAMPLE run={} bytes={} rows={} {}",
            run + 1,
            sample.bytes,
            sample.rows,
            metric_text,
        );
        samples.push(sample);
    }

    let metric_stats = search_bench_metric_values(&samples[0])
        .into_iter()
        .enumerate()
        .map(|(index, (label, _))| {
            let values = samples
                .iter()
                .map(|sample| search_bench_metric_values(sample)[index].1)
                .collect::<Vec<_>>();
            (label, NavStats::from(&values))
        })
        .collect::<Vec<_>>();
    let summary_metrics = metric_stats
        .iter()
        .flat_map(|(label, stats)| {
            [
                format!("{label}_mean_ms={:.3}", stats.mean),
                format!("{label}_stddev_ms={:.3}", stats.stddev),
            ]
        })
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "TRANSCRIPT_SEARCH_BENCH_SUMMARY runs={} bytes={} rows={} {}",
        samples.len(),
        samples[0].bytes,
        samples[0].rows,
        summary_metrics,
    );
    let json_metrics = metric_stats
        .iter()
        .flat_map(|(label, stats)| {
            [
                format!("\"{label}_mean_ms\":{:.3}", stats.mean),
                format!("\"{label}_stddev_ms\":{:.3}", stats.stddev),
            ]
        })
        .collect::<Vec<_>>()
        .join(",");
    eprintln!(
        "TRANSCRIPT_SEARCH_BENCH_JSON {{\"type\":\"search_summary\",\"runs\":{},\"bytes\":{},\"rows\":{},{}}}",
        samples.len(),
        samples[0].bytes,
        samples[0].rows,
        json_metrics,
    );
}

const TALL_WRITE_SCROLL_FRAMES: usize = 12;

#[derive(Clone, Copy, Debug)]
struct TallWriteSample {
    bytes: usize,
    lines: usize,
    collapsed_scroll_ms: f64,
    expand_ms: f64,
    top_scroll_ms: f64,
    middle_scroll_ms: f64,
    deep_scroll_ms: f64,
}

fn tall_write_line_count() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_TALL_WRITE_LINES", 20_000)
}

fn feed_transcript_wheel(app: &mut TestApp, kind: crossterm::event::MouseEventKind) {
    let rect = app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .expect("transcript rect for tall write benchmark");
    app.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
        crossterm::event::MouseEvent {
            kind,
            row: rect.top.saturating_add(rect.height / 2),
            column: rect.left.saturating_add(rect.width / 2),
            modifiers: KeyModifiers::empty(),
        },
    )));
}

fn assert_tall_write_scroll_gates(
    snapshot: &smelt_perf::perf::Snapshot,
    label: &str,
    expanded: bool,
) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    assert_no_full_block_renders_for_scroll(snapshot, label);
    assert_eq!(
        perf_duration_count(snapshot, "transcript:rebuild_row_index"),
        0,
        "{label} rebuilt the complete transcript height index"
    );
    assert_eq!(
        perf_value_max(snapshot, "transcript:prepare_row_index:reused_index"),
        1,
        "{label} did not reuse the transcript row index"
    );
    let materialized_rows = perf_value_total(snapshot, "transcript:collect_nodes_range:rows");
    assert!(
        materialized_rows <= (TALL_WRITE_SCROLL_FRAMES as u64) * 256,
        "{label} materialized {materialized_rows} rows"
    );
    let prefix_lines = perf_value_max(snapshot, "render:inline_diff_cached:prefix_syntax_lines");
    let source_lines = perf_value_max(snapshot, "render:inline_diff_cached:source_lines");
    if expanded {
        assert!(
            prefix_lines < 128,
            "{label} replayed {prefix_lines} source lines before a visible range"
        );
        assert!(
            source_lines <= 256,
            "{label} highlighted {source_lines} source lines for one viewport frame"
        );
    } else {
        assert_eq!(prefix_lines, 0, "{label} touched hidden source syntax");
        assert_eq!(source_lines, 0, "{label} rendered hidden source lines");
    }
}

fn measure_tall_write_scroll(
    app: &mut TestApp,
    label: &'static str,
    kind: crossterm::event::MouseEventKind,
    expanded: bool,
) -> f64 {
    let before = app.app.transcript_win().scroll_top();
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    for _ in 0..TALL_WRITE_SCROLL_FRAMES {
        feed_transcript_wheel(app, kind);
        app.render_silent();
    }
    let ms = elapsed_ms(start.elapsed());
    let after = app.app.transcript_win().scroll_top();
    match kind {
        crossterm::event::MouseEventKind::ScrollDown => assert!(after > before),
        crossterm::event::MouseEventKind::ScrollUp => assert!(after < before),
        _ => unreachable!("tall write benchmark only sends wheel events"),
    }
    let snapshot = smelt_perf::perf::snapshot();
    assert_tall_write_scroll_gates(&snapshot, label, expanded);
    ms
}

fn jump_to_transcript_row(app: &mut TestApp, row: crate::smelt_edit::RowIndex) {
    app.type_text(&row.saturating_add(1).to_string());
    app.type_char('G');
    app.render_silent();
}

fn run_tall_write_sample(line_count: usize) -> TallWriteSample {
    let mut app = TestApp::builder()
        .with_ephemeral(true)
        .with_vim(true)
        .build();
    app.app.handle_resize(120, 40);
    for i in 0..80 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("collapsed transcript row before tool {i:03}").into(),
            });
    }
    let content = (0..line_count)
        .map(|i| {
            format!("pub fn generated_{i:05}() -> usize {{ {i} }} // tall write file benchmark")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = content.len();
    let tool_record = 80;
    let invocation_id = app.start_tool(
        "tall-write-file".into(),
        "write_file".into(),
        protocol::StyledLines::from_plain("write generated/large.rs"),
        std::collections::HashMap::from([
            ("file_path".into(), serde_json::json!("generated/large.rs")),
            ("content".into(), serde_json::json!(content)),
        ]),
    );
    app.finish_tool(
        invocation_id,
        smelt_core::transcript_model::ToolStatus::Ok,
        None,
        Some(std::time::Duration::from_millis(250)),
    );
    for i in 0..80 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("collapsed transcript row after tool {i:03}").into(),
            });
    }
    app.render_silent();
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    assert!(app.run_lua("smelt.transcript.fold_all('close')"));
    app.type_char('G');
    app.render_silent();

    let collapsed_scroll_ms = measure_tall_write_scroll(
        &mut app,
        "tall_write_collapsed_scroll12",
        crossterm::event::MouseEventKind::ScrollUp,
        false,
    );

    assert!(app.reveal_transcript_record_block(tool_record, 0, true));
    app.render_silent();
    let collapsed_rows = transcript_total_rows(&app);
    smelt_perf::perf::clear();
    let expand_start = std::time::Instant::now();
    app.press(KeyCode::Enter);
    app.render_silent();
    let expand_ms = elapsed_ms(expand_start.elapsed());
    let expanded_rows = transcript_total_rows(&app);
    assert!(
        expanded_rows > collapsed_rows.saturating_add(line_count as u64 / 2),
        "Enter did not expand the tall write_file body: collapsed={collapsed_rows}, expanded={expanded_rows}"
    );
    let tool_top = app.app.transcript_win().scroll_top();
    let transcript_buf = app.app.transcript_win().buf;
    let expanded_lines = app
        .ui_probe()
        .buf(transcript_buf)
        .expect("expanded transcript buffer")
        .lines();
    assert!(
        expanded_lines
            .iter()
            .any(|line| line.contains("generated_")),
        "expanded write_file viewport did not render source rows: {expanded_lines:?}"
    );

    let top_scroll_ms = measure_tall_write_scroll(
        &mut app,
        "tall_write_top_scroll12",
        crossterm::event::MouseEventKind::ScrollDown,
        true,
    );

    jump_to_transcript_row(&mut app, tool_top.saturating_add(line_count as u64 / 2));
    let middle_scroll_ms = measure_tall_write_scroll(
        &mut app,
        "tall_write_middle_scroll12",
        crossterm::event::MouseEventKind::ScrollDown,
        true,
    );

    jump_to_transcript_row(
        &mut app,
        tool_top.saturating_add(line_count as u64 * 9 / 10),
    );
    let deep_scroll_ms = measure_tall_write_scroll(
        &mut app,
        "tall_write_deep_scroll12",
        crossterm::event::MouseEventKind::ScrollDown,
        true,
    );

    TallWriteSample {
        bytes,
        lines: line_count,
        collapsed_scroll_ms,
        expand_ms,
        top_scroll_ms,
        middle_scroll_ms,
        deep_scroll_ms,
    }
}

#[test]
fn transcript_layout_tall_write_file_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    smelt_perf::perf::set_enabled(true);
    let runs = navigation_bench_runs();
    let lines = tall_write_line_count();
    if transcript_bench_warmup_enabled() {
        let _ = run_tall_write_sample(lines);
    }
    let mut samples = Vec::with_capacity(runs);
    for run in 0..runs {
        let sample = run_tall_write_sample(lines);
        eprintln!(
            "TRANSCRIPT_TALL_WRITE_BENCH_SAMPLE run={} bytes={} lines={} collapsed_scroll12_ms={:.3} expand_ms={:.3} top_scroll12_ms={:.3} middle_scroll12_ms={:.3} deep_scroll12_ms={:.3}",
            run + 1,
            sample.bytes,
            sample.lines,
            sample.collapsed_scroll_ms,
            sample.expand_ms,
            sample.top_scroll_ms,
            sample.middle_scroll_ms,
            sample.deep_scroll_ms,
        );
        samples.push(sample);
    }
    let stats = |get: fn(&TallWriteSample) -> f64| {
        TailStats::from(&samples.iter().map(get).collect::<Vec<_>>())
    };
    let collapsed = stats(|sample| sample.collapsed_scroll_ms);
    let expand = stats(|sample| sample.expand_ms);
    let top = stats(|sample| sample.top_scroll_ms);
    let middle = stats(|sample| sample.middle_scroll_ms);
    let deep = stats(|sample| sample.deep_scroll_ms);
    eprintln!(
        "TRANSCRIPT_TALL_WRITE_BENCH_SUMMARY runs={} bytes={} lines={} collapsed_mean_ms={:.3} collapsed_p95_ms={:.3} expand_mean_ms={:.3} expand_p95_ms={:.3} top_mean_ms={:.3} top_p95_ms={:.3} middle_mean_ms={:.3} middle_p95_ms={:.3} deep_mean_ms={:.3} deep_p95_ms={:.3}",
        samples.len(),
        samples[0].bytes,
        samples[0].lines,
        collapsed.mean,
        collapsed.p95,
        expand.mean,
        expand.p95,
        top.mean,
        top.p95,
        middle.mean,
        middle.p95,
        deep.mean,
        deep.p95,
    );
    eprintln!(
        "TRANSCRIPT_TALL_WRITE_BENCH_JSON {{\"type\":\"tall_write_file_summary\",\"runs\":{},\"bytes\":{},\"lines\":{},\"collapsed_mean_ms\":{:.3},\"collapsed_p95_ms\":{:.3},\"expand_mean_ms\":{:.3},\"expand_p95_ms\":{:.3},\"top_mean_ms\":{:.3},\"top_p95_ms\":{:.3},\"middle_mean_ms\":{:.3},\"middle_p95_ms\":{:.3},\"deep_mean_ms\":{:.3},\"deep_p95_ms\":{:.3}}}",
        samples.len(),
        samples[0].bytes,
        samples[0].lines,
        collapsed.mean,
        collapsed.p95,
        expand.mean,
        expand.p95,
        top.mean,
        top.p95,
        middle.mean,
        middle.p95,
        deep.mean,
        deep.p95,
    );
}

#[derive(Clone, Copy, Debug)]
struct TallDiffSample {
    source_bytes: usize,
    lines: usize,
    first_render_ms: f64,
    warm_render_ms: f64,
    first_render_allocs: u64,
    first_render_bytes: u64,
    warm_render_allocs: u64,
    warm_render_bytes: u64,
    process_alloc_bytes: u64,
    process_dealloc_bytes: u64,
    process_retained_bytes: i64,
    live_tool_state_bytes: usize,
    layout_bytes: usize,
    height_index_bytes: usize,
    visible_rows_bytes: usize,
    full_rows_bytes: usize,
    total_rows: crate::smelt_edit::RowIndex,
    metadata_compiles: usize,
    warm_metadata_compiles: usize,
}

fn tall_diff_line_count() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_TALL_DIFF_LINES", 20_000)
}

fn run_tall_diff_sample(line_count: usize) -> TallDiffSample {
    let mut app = TestApp::builder()
        .with_ephemeral(true)
        .with_vim(false)
        .build();
    app.app.handle_resize(120, 40);
    for i in 0..100 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("retained diff history row {i:03}").into(),
            });
    }

    let target_line = line_count / 2;
    let old_line = format!(
        "pub fn retained_target_{target_line:05}() -> usize {{ {target_line} }} // retained diff benchmark"
    );
    let new_line = format!(
        "pub fn retained_target_{target_line:05}() -> usize {{ {} }} // retained diff benchmark",
        target_line + 1
    );
    let old_content = (0..line_count)
        .map(|i| format!("pub fn retained_{i:05}() -> usize {{ {i} }} // retained diff benchmark"))
        .collect::<Vec<_>>()
        .join("\n")
        .replacen(
            &format!("pub fn retained_{target_line:05}() -> usize {{ {target_line} }} // retained diff benchmark"),
            &old_line,
            1,
        );
    let new_content = old_content.replacen(&old_line, &new_line, 1);
    let source_bytes = old_content.len().saturating_add(new_content.len());

    let invocation_id = app.start_tool(
        "tall-retained-diff".into(),
        "edit_file".into(),
        protocol::StyledLines::from_plain("edit generated/large.rs"),
        std::collections::HashMap::from([
            ("file_path".into(), serde_json::json!("generated/large.rs")),
            ("old_string".into(), serde_json::json!(old_line)),
            ("new_string".into(), serde_json::json!(new_line)),
        ]),
    );
    let output = smelt_core::transcript_model::ToolOutput {
        content: "edited generated/large.rs".into(),
        is_error: false,
        metadata: Some(serde_json::json!({ "path": "generated/large.rs" })),
        content_fields: vec![
            smelt_core::transcript_model::ToolOutputContentField {
                name: "old_content".into(),
                content: old_content.into(),
            },
            smelt_core::transcript_model::ToolOutputContentField {
                name: "new_content".into(),
                content: new_content.into(),
            },
        ],
    };
    app.finish_tool(
        invocation_id,
        smelt_core::transcript_model::ToolStatus::Ok,
        Some(Box::new(output)),
        Some(std::time::Duration::from_millis(250)),
    );

    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    smelt_perf::alloc::set_enabled(true);
    let process_before = smelt_perf::alloc::snapshot();
    let (first_allocs_before, first_bytes_before) = smelt_perf::alloc::thread_snapshot();
    let first_start = std::time::Instant::now();
    app.render_silent();
    let first_render_ms = elapsed_ms(first_start.elapsed());
    let (first_allocs_after, first_bytes_after) = smelt_perf::alloc::thread_snapshot();
    let process_after = smelt_perf::alloc::snapshot();
    let process_delta = smelt_perf::alloc::delta(process_before, process_after);
    let first_perf = smelt_perf::perf::snapshot();
    let metadata_compiles =
        perf_duration_count(&first_perf, "transcript:layout_cache:block_render_metadata");

    let transcript_buf = app.app.transcript_win().buf;
    let visible_lines = app
        .ui_probe()
        .buf(transcript_buf)
        .expect("retained diff transcript buffer")
        .lines();
    assert!(
        visible_lines.iter().any(|line| line.contains(&format!(
            "retained_target_{target_line:05}() -> usize {{ {} }}",
            target_line + 1
        ))),
        "retained diff did not render the changed source line: {visible_lines:?}"
    );

    smelt_perf::perf::clear();
    let (warm_allocs_before, warm_bytes_before) = smelt_perf::alloc::thread_snapshot();
    let warm_start = std::time::Instant::now();
    app.render_silent();
    let warm_render_ms = elapsed_ms(warm_start.elapsed());
    let (warm_allocs_after, warm_bytes_after) = smelt_perf::alloc::thread_snapshot();
    let warm_perf = smelt_perf::perf::snapshot();
    let warm_metadata_compiles =
        perf_duration_count(&warm_perf, "transcript:layout_cache:block_render_metadata");
    assert_eq!(
        warm_metadata_compiles, 0,
        "unchanged retained diff recompiled Lua renderer metadata"
    );

    let memory = app.app.conversation.transcript().memory_snapshot();
    let total_rows = transcript_total_rows(&app);
    smelt_perf::alloc::set_enabled(false);
    smelt_perf::perf::set_enabled(false);

    TallDiffSample {
        source_bytes,
        lines: line_count,
        first_render_ms,
        warm_render_ms,
        first_render_allocs: first_allocs_after.saturating_sub(first_allocs_before),
        first_render_bytes: first_bytes_after.saturating_sub(first_bytes_before),
        warm_render_allocs: warm_allocs_after.saturating_sub(warm_allocs_before),
        warm_render_bytes: warm_bytes_after.saturating_sub(warm_bytes_before),
        process_alloc_bytes: process_delta.bytes_allocated,
        process_dealloc_bytes: process_delta.bytes_deallocated,
        process_retained_bytes: process_after.current_bytes as i64
            - process_before.current_bytes as i64,
        live_tool_state_bytes: memory.live_tool_state_bytes,
        layout_bytes: memory.layout_bytes,
        height_index_bytes: memory.height_index_bytes,
        visible_rows_bytes: memory.visible_rows_bytes,
        full_rows_bytes: memory.full_rows_bytes,
        total_rows,
        metadata_compiles,
        warm_metadata_compiles,
    }
}

#[test]
fn transcript_layout_tall_retained_diff_benchmark_suite() {
    if !benchmark_target_enabled()
        || std::env::var("SMELT_TRANSCRIPT_TALL_DIFF").as_deref() != Ok("1")
    {
        return;
    }
    let runs = navigation_bench_runs();
    let lines = tall_diff_line_count();
    if transcript_bench_warmup_enabled() {
        let _ = run_tall_diff_sample(lines);
    }
    let mut samples = Vec::with_capacity(runs);
    for run in 0..runs {
        let sample = run_tall_diff_sample(lines);
        eprintln!(
            "TRANSCRIPT_TALL_DIFF_BENCH_SAMPLE run={} source_bytes={} lines={} first_render_ms={:.3} warm_render_ms={:.3} first_render_allocs={} first_render_bytes={} warm_render_allocs={} warm_render_bytes={} process_alloc_bytes={} process_dealloc_bytes={} process_retained_bytes={} live_tool_state_bytes={} layout_bytes={} height_index_bytes={} visible_rows_bytes={} full_rows_bytes={} total_rows={} metadata_compiles={} warm_metadata_compiles={}",
            run + 1,
            sample.source_bytes,
            sample.lines,
            sample.first_render_ms,
            sample.warm_render_ms,
            sample.first_render_allocs,
            sample.first_render_bytes,
            sample.warm_render_allocs,
            sample.warm_render_bytes,
            sample.process_alloc_bytes,
            sample.process_dealloc_bytes,
            sample.process_retained_bytes,
            sample.live_tool_state_bytes,
            sample.layout_bytes,
            sample.height_index_bytes,
            sample.visible_rows_bytes,
            sample.full_rows_bytes,
            sample.total_rows,
            sample.metadata_compiles,
            sample.warm_metadata_compiles,
        );
        samples.push(sample);
    }

    let first = TailStats::from(
        &samples
            .iter()
            .map(|sample| sample.first_render_ms)
            .collect::<Vec<_>>(),
    );
    let warm = TailStats::from(
        &samples
            .iter()
            .map(|sample| sample.warm_render_ms)
            .collect::<Vec<_>>(),
    );
    eprintln!(
        "TRANSCRIPT_TALL_DIFF_BENCH_JSON {{\"type\":\"tall_retained_diff_summary\",\"runs\":{},\"source_bytes\":{},\"lines\":{},\"first_mean_ms\":{:.3},\"first_p95_ms\":{:.3},\"warm_mean_ms\":{:.3},\"warm_p95_ms\":{:.3},\"first_render_bytes\":{},\"warm_render_bytes\":{},\"process_retained_bytes\":{},\"live_tool_state_bytes\":{},\"layout_bytes\":{},\"height_index_bytes\":{},\"visible_rows_bytes\":{},\"full_rows_bytes\":{},\"total_rows\":{},\"metadata_compiles\":{},\"warm_metadata_compiles\":{}}}",
        samples.len(),
        samples[0].source_bytes,
        samples[0].lines,
        first.mean,
        first.p95,
        warm.mean,
        warm.p95,
        samples[0].first_render_bytes,
        samples[0].warm_render_bytes,
        samples[0].process_retained_bytes,
        samples[0].live_tool_state_bytes,
        samples[0].layout_bytes,
        samples[0].height_index_bytes,
        samples[0].visible_rows_bytes,
        samples[0].full_rows_bytes,
        samples[0].total_rows,
        samples[0].metadata_compiles,
        samples[0].warm_metadata_compiles,
    );
}

#[test]
fn transcript_layout_navigation_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    if std::env::var("SMELT_TRANSCRIPT_BENCH_SKIP_NAV")
        .ok()
        .as_deref()
        == Some("1")
    {
        eprintln!("TRANSCRIPT_LAYOUT_NAV_SKIPPED");
        return;
    }
    smelt_perf::perf::set_enabled(true);
    let runs = navigation_bench_runs();
    let _warmup = run_navigation_sample();
    let mut samples = Vec::with_capacity(runs);
    for run in 0..runs {
        let sample = run_navigation_sample();
        eprintln!(
            "TRANSCRIPT_LAYOUT_NAV_SAMPLE run={} rows={} search_ms={:.3} ctrl_d20_ms={:.3} ctrl_u20_ms={:.3} gg_ms={:.3} G_ms={:.3} previous_user_pill_ms={:.3} bottom_pill_ms={:.3}",
            run + 1,
            sample.rows,
            sample.search_ms,
            sample.ctrl_d20_ms,
            sample.ctrl_u20_ms,
            sample.gg_ms,
            sample.g_ms,
            sample.previous_user_pill_ms,
            sample.bottom_pill_ms,
        );
        samples.push(sample);
    }

    let search = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.search_ms)
            .collect::<Vec<_>>(),
    );
    let ctrl_d = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.ctrl_d20_ms)
            .collect::<Vec<_>>(),
    );
    let ctrl_u = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.ctrl_u20_ms)
            .collect::<Vec<_>>(),
    );
    let gg = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.gg_ms)
            .collect::<Vec<_>>(),
    );
    let g = NavStats::from(&samples.iter().map(|sample| sample.g_ms).collect::<Vec<_>>());
    let previous_user_pill = TailStats::from(
        &samples
            .iter()
            .map(|sample| sample.previous_user_pill_ms)
            .collect::<Vec<_>>(),
    );
    let bottom_pill = TailStats::from(
        &samples
            .iter()
            .map(|sample| sample.bottom_pill_ms)
            .collect::<Vec<_>>(),
    );
    eprintln!(
        "TRANSCRIPT_LAYOUT_NAV_SUMMARY runs={} rows={} search_mean_ms={:.3} search_stddev_ms={:.3} ctrl_d20_mean_ms={:.3} ctrl_d20_stddev_ms={:.3} ctrl_u20_mean_ms={:.3} ctrl_u20_stddev_ms={:.3} gg_mean_ms={:.3} gg_stddev_ms={:.3} G_mean_ms={:.3} G_stddev_ms={:.3} previous_user_pill_mean_ms={:.3} previous_user_pill_stddev_ms={:.3} previous_user_pill_p50_ms={:.3} previous_user_pill_p95_ms={:.3} previous_user_pill_p99_ms={:.3} previous_user_pill_max_ms={:.3} bottom_pill_mean_ms={:.3} bottom_pill_stddev_ms={:.3} bottom_pill_p50_ms={:.3} bottom_pill_p95_ms={:.3} bottom_pill_p99_ms={:.3} bottom_pill_max_ms={:.3}",
        samples.len(),
        samples[0].rows,
        search.mean,
        search.stddev,
        ctrl_d.mean,
        ctrl_d.stddev,
        ctrl_u.mean,
        ctrl_u.stddev,
        gg.mean,
        gg.stddev,
        g.mean,
        g.stddev,
        previous_user_pill.mean,
        previous_user_pill.stddev,
        previous_user_pill.p50,
        previous_user_pill.p95,
        previous_user_pill.p99,
        previous_user_pill.max,
        bottom_pill.mean,
        bottom_pill.stddev,
        bottom_pill.p50,
        bottom_pill.p95,
        bottom_pill.p99,
        bottom_pill.max,
    );
    eprintln!(
        "TRANSCRIPT_LAYOUT_NAV_JSON {{\"type\":\"navigation_summary\",\"runs\":{},\"rows\":{},\"search_mean_ms\":{:.3},\"search_stddev_ms\":{:.3},\"ctrl_d20_mean_ms\":{:.3},\"ctrl_d20_stddev_ms\":{:.3},\"ctrl_u20_mean_ms\":{:.3},\"ctrl_u20_stddev_ms\":{:.3},\"gg_mean_ms\":{:.3},\"gg_stddev_ms\":{:.3},\"G_mean_ms\":{:.3},\"G_stddev_ms\":{:.3},\"previous_user_pill_mean_ms\":{:.3},\"previous_user_pill_stddev_ms\":{:.3},\"previous_user_pill_p50_ms\":{:.3},\"previous_user_pill_p95_ms\":{:.3},\"previous_user_pill_p99_ms\":{:.3},\"previous_user_pill_max_ms\":{:.3},\"bottom_pill_mean_ms\":{:.3},\"bottom_pill_stddev_ms\":{:.3},\"bottom_pill_p50_ms\":{:.3},\"bottom_pill_p95_ms\":{:.3},\"bottom_pill_p99_ms\":{:.3},\"bottom_pill_max_ms\":{:.3}}}",
        samples.len(),
        samples[0].rows,
        search.mean,
        search.stddev,
        ctrl_d.mean,
        ctrl_d.stddev,
        ctrl_u.mean,
        ctrl_u.stddev,
        gg.mean,
        gg.stddev,
        g.mean,
        g.stddev,
        previous_user_pill.mean,
        previous_user_pill.stddev,
        previous_user_pill.p50,
        previous_user_pill.p95,
        previous_user_pill.p99,
        previous_user_pill.max,
        bottom_pill.mean,
        bottom_pill.stddev,
        bottom_pill.p50,
        bottom_pill.p95,
        bottom_pill.p99,
        bottom_pill.max,
    );
    eprintln!(
        "| navigation/search | rows={} | search={}ms | ctrl-d×20={}ms | ctrl-u×20={}ms | gg={}ms | G={}ms | previous-user pill={:.2}ms p95={:.2}ms | bottom pill={:.2}ms p95={:.2}ms |",
        samples[0].rows,
        search.display(),
        ctrl_d.display(),
        ctrl_u.display(),
        gg.display(),
        g.display(),
        previous_user_pill.mean,
        previous_user_pill.p95,
        bottom_pill.mean,
        bottom_pill.p95,
    );
}

#[test]
fn transcript_sparse_watcher_benchmark_suite() {
    if !benchmark_target_enabled()
        || std::env::var("SMELT_TRANSCRIPT_SPARSE_WATCHER_BENCH").as_deref() != Ok("1")
    {
        return;
    }
    let block_count = env_positive_usize("SMELT_TRANSCRIPT_SPARSE_WATCHER_BLOCKS", 5_000);
    let runs = env_positive_usize("SMELT_TRANSCRIPT_SPARSE_WATCHER_RUNS", 1_001);
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    for index in 0..block_count {
        let block = if index.is_multiple_of(10) {
            smelt_core::transcript_model::Block::User {
                text: format!("sparse watcher user {index}"),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }
        } else {
            smelt_core::transcript_model::Block::Text {
                content: format!("sparse watcher response {index}").into(),
            }
        };
        app.app.push_block(block);
    }
    app.render_silent();
    let receipt = save_bench_fixture(&mut app, "sparse watcher");
    wait_for_bench_catalog(&app, "sparse watcher", &receipt);
    install_sparse_resume_bench_transcript(&mut app);
    assert!(app.run_lua(
        r#"
        _G.sparse_watcher_calls = 0
        _G.sparse_watcher_reg = smelt.transcript.watch_view(function(view)
          assert(view.revision ~= nil)
          _G.sparse_watcher_calls = _G.sparse_watcher_calls + 1
        end)
        "#
    ));
    app.render_silent();
    let readers_before = wait_for_sparse_reader_metrics(&mut app, "sparse watcher");
    let calls_before = app
        .lua_int_global("sparse_watcher_calls")
        .unwrap_or_default();

    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    for _ in 0..runs {
        app.transcript_scroll_probe_wheel(false, 1);
        app.render_silent();
    }
    let snapshot = smelt_perf::perf::snapshot();
    smelt_perf::perf::set_enabled(false);

    let dispatch = snapshot
        .durations
        .iter()
        .find(|row| row.label == "compositor:dispatch_committed_transcript_view")
        .expect("committed-view dispatch metric");
    assert!(
        dispatch.count >= runs.min(smelt_perf::perf::RING_CAPACITY),
        "each sparse scroll must publish at least one committed view sample: runs={runs} samples={}",
        dispatch.count
    );
    assert!(
        dispatch.p99_us < 2_000,
        "committed-view watcher dispatch exceeded 2 ms p99: {} us",
        dispatch.p99_us
    );
    assert!(
        app.lua_int_global("sparse_watcher_calls")
            .unwrap_or_default()
            > calls_before,
        "sparse scrolling must publish committed-view revisions"
    );
    let readers_after = app.app.transcript_reader_metrics_for_harness();
    assert_eq!(
        readers_after, readers_before,
        "committed-view watcher dispatch must reuse both retained sparse readers"
    );
    eprintln!(
        "TRANSCRIPT_SPARSE_WATCHER_BENCH_JSON {}",
        serde_json::json!({
            "blocks": block_count,
            "runs": runs,
            "dispatch_samples": dispatch.count,
            "dispatch_p50_us": dispatch.p50_us,
            "dispatch_p95_us": dispatch.p95_us,
            "dispatch_p99_us": dispatch.p99_us,
            "dispatch_max_us": dispatch.max_us,
            "metadata_readers": readers_before.metadata_readers,
            "hydration_readers": readers_before.hydration_readers,
            "total_readers": readers_before.total_readers,
            "metadata_open_attempts": readers_before.metadata_open_attempts,
            "hydration_open_attempts": readers_before.hydration_open_attempts,
            "total_open_attempts": readers_before.total_open_attempts,
        })
    );
}

#[derive(Clone, Copy, Debug)]
struct AutonomousFrameSample {
    frame_total_us: u64,
    frame_p50_us: u64,
    frame_p95_us: u64,
    frame_p99_us: u64,
    frame_max_us: u64,
    wall_us: u64,
    thread_allocs: u64,
    thread_bytes: u64,
    process_alloc_bytes: u64,
    process_dealloc_bytes: u64,
    payloads_loaded: u64,
}

fn render_autonomous_frames(app: &mut TestApp, frames: usize) {
    let mut rendered = 0usize;
    for _ in 0..frames {
        app.clock.advance(std::time::Duration::from_millis(16));
        app.app
            .request_animation_render(std::time::Duration::from_millis(16));
        rendered += usize::from(
            app.app
                .render_requested_transient_frame_to(&mut std::io::sink()),
        );
    }
    assert_eq!(rendered, frames, "every autonomous frame must render");
}

fn begin_autonomous_text_turn(app: &mut TestApp, turn_id: u64, warmup_frames: usize) {
    app.start_turn(turn_id);
    app.app.dispatch_engine_event_in_render_loop_to(
        protocol::EngineEvent::TextDelta { delta: "x".into() },
        &mut std::io::sink(),
        |_| {},
    );
    app.app.request_urgent_render();
    app.render_silent();
    render_autonomous_frames(app, warmup_frames);
}

fn measure_autonomous_frames(app: &mut TestApp, frames: usize) -> AutonomousFrameSample {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    smelt_perf::alloc::set_enabled(true);
    let process_before = smelt_perf::alloc::snapshot();
    let (thread_allocs_before, thread_bytes_before) = smelt_perf::alloc::thread_snapshot();
    let start = std::time::Instant::now();
    render_autonomous_frames(app, frames);
    let wall_us = start.elapsed().as_micros().try_into().unwrap_or(u64::MAX);
    let (thread_allocs_after, thread_bytes_after) = smelt_perf::alloc::thread_snapshot();
    let process_after = smelt_perf::alloc::snapshot();
    let process_delta = smelt_perf::alloc::delta(process_before, process_after);
    let snapshot = smelt_perf::perf::snapshot();
    smelt_perf::alloc::set_enabled(false);
    smelt_perf::perf::set_enabled(false);

    let frame = snapshot
        .durations
        .iter()
        .find(|row| row.label == "app:tick_compositor")
        .expect("autonomous benchmark compositor frames");
    assert_eq!(frame.count as usize, frames);
    AutonomousFrameSample {
        frame_total_us: frame.total_us,
        frame_p50_us: frame.p50_us,
        frame_p95_us: frame.p95_us,
        frame_p99_us: frame.p99_us,
        frame_max_us: frame.max_us,
        wall_us,
        thread_allocs: thread_allocs_after.saturating_sub(thread_allocs_before),
        thread_bytes: thread_bytes_after.saturating_sub(thread_bytes_before),
        process_alloc_bytes: process_delta.bytes_allocated,
        process_dealloc_bytes: process_delta.bytes_deallocated,
        payloads_loaded: perf_value_total(&snapshot, "store:object:payloads_loaded"),
    }
}

fn assert_within_twenty_five_percent(label: &str, sparse: u64, hydrated: u64) {
    assert!(
        u128::from(sparse).saturating_mul(4) <= u128::from(hydrated).saturating_mul(5),
        "sparse {label} {sparse} exceeded hydrated {label} {hydrated} by more than 25 percent"
    );
}

#[test]
fn transcript_sparse_autonomous_frame_benchmark_suite() {
    if !benchmark_target_enabled()
        || std::env::var("SMELT_TRANSCRIPT_SPARSE_AUTONOMOUS_BENCH").as_deref() != Ok("1")
    {
        return;
    }
    let target_bytes =
        env_positive_usize("SMELT_TRANSCRIPT_SPARSE_AUTONOMOUS_BYTES", 5 * 1024 * 1024);
    let frames = env_positive_usize("SMELT_TRANSCRIPT_SPARSE_AUTONOMOUS_FRAMES", 600);
    let warmup_frames = env_positive_usize("SMELT_TRANSCRIPT_SPARSE_AUTONOMOUS_WARMUP", 32);
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let fixture_bytes = push_search_bench_transcript(&mut app, target_bytes);
    app.render_silent();
    let receipt = save_bench_fixture(&mut app, "sparse autonomous frames");
    wait_for_bench_catalog(&app, "sparse autonomous frames", &receipt);

    begin_autonomous_text_turn(&mut app, 42, warmup_frames);
    let hydrated = measure_autonomous_frames(&mut app, frames);
    app.app.dispatch_engine_event_in_render_loop_to(
        protocol::EngineEvent::TurnComplete {
            turn_id: 42,
            history: None,
            meta: None,
        },
        &mut std::io::sink(),
        |_| {},
    );

    install_sparse_resume_bench_transcript(&mut app);
    begin_autonomous_text_turn(&mut app, 43, warmup_frames);
    let readers_before = wait_for_sparse_reader_metrics(&mut app, "sparse autonomous frames");
    let sparse = measure_autonomous_frames(&mut app, frames);
    let readers_after = app.app.transcript_reader_metrics_for_harness();
    assert_eq!(
        readers_after, readers_before,
        "autonomous sparse frames must reuse both retained session readers"
    );
    assert_eq!(
        sparse.payloads_loaded, 0,
        "warmed sparse frames must not deserialize transcript payloads"
    );

    assert_within_twenty_five_percent(
        "compositor CPU total",
        sparse.frame_total_us,
        hydrated.frame_total_us,
    );
    assert_within_twenty_five_percent(
        "thread allocation count",
        sparse.thread_allocs,
        hydrated.thread_allocs,
    );
    assert_within_twenty_five_percent(
        "thread allocation bytes",
        sparse.thread_bytes,
        hydrated.thread_bytes,
    );
    assert_within_twenty_five_percent(
        "process allocation bytes",
        sparse.process_alloc_bytes,
        hydrated.process_alloc_bytes,
    );

    eprintln!(
        "TRANSCRIPT_SPARSE_AUTONOMOUS_BENCH_JSON {}",
        serde_json::json!({
            "fixture_bytes": fixture_bytes,
            "frames": frames,
            "warmup_frames": warmup_frames,
            "metadata_readers": readers_before.metadata_readers,
            "hydration_readers": readers_before.hydration_readers,
            "total_readers": readers_before.total_readers,
            "metadata_open_attempts": readers_before.metadata_open_attempts,
            "hydration_open_attempts": readers_before.hydration_open_attempts,
            "total_open_attempts": readers_before.total_open_attempts,
            "hydrated": {
                "frame_total_us": hydrated.frame_total_us,
                "frame_p50_us": hydrated.frame_p50_us,
                "frame_p95_us": hydrated.frame_p95_us,
                "frame_p99_us": hydrated.frame_p99_us,
                "frame_max_us": hydrated.frame_max_us,
                "wall_us": hydrated.wall_us,
                "thread_allocs": hydrated.thread_allocs,
                "thread_bytes": hydrated.thread_bytes,
                "process_alloc_bytes": hydrated.process_alloc_bytes,
                "process_dealloc_bytes": hydrated.process_dealloc_bytes,
                "payloads_loaded": hydrated.payloads_loaded,
            },
            "sparse": {
                "frame_total_us": sparse.frame_total_us,
                "frame_p50_us": sparse.frame_p50_us,
                "frame_p95_us": sparse.frame_p95_us,
                "frame_p99_us": sparse.frame_p99_us,
                "frame_max_us": sparse.frame_max_us,
                "wall_us": sparse.wall_us,
                "thread_allocs": sparse.thread_allocs,
                "thread_bytes": sparse.thread_bytes,
                "process_alloc_bytes": sparse.process_alloc_bytes,
                "process_dealloc_bytes": sparse.process_dealloc_bytes,
                "payloads_loaded": sparse.payloads_loaded,
            },
            "ratios": {
                "frame_total": sparse.frame_total_us as f64 / hydrated.frame_total_us as f64,
                "thread_allocs": sparse.thread_allocs as f64 / hydrated.thread_allocs as f64,
                "thread_bytes": sparse.thread_bytes as f64 / hydrated.thread_bytes as f64,
                "process_alloc_bytes": sparse.process_alloc_bytes as f64 / hydrated.process_alloc_bytes as f64,
            },
        })
    );
}

#[derive(Clone, Copy, Debug)]
struct HotPathCounters {
    history_suffix_rows: u64,
    history_inserted: u64,
    history_deleted: u64,
    record_suffix_rows: u64,
    record_inserted: u64,
    record_deleted: u64,
    read_range_rows: u64,
    cached_read_write_db: u64,
    invariant_history_rows: u64,
    search_blob_rows: u64,
    search_blob_bytes: u64,
    user_turn_blocks_scanned: u64,
    user_turns_cloned: u64,
    user_turn_text_bytes_cloned: u64,
}

impl HotPathCounters {
    fn from(snapshot: &smelt_perf::perf::Snapshot) -> Self {
        Self {
            history_suffix_rows: perf_value_max(snapshot, "store:history:dirty_suffix_rows"),
            history_inserted: perf_value_max(snapshot, "store:session:history_rows_inserted"),
            history_deleted: perf_value_max(snapshot, "store:session:history_rows_deleted"),
            record_suffix_rows: perf_value_max(
                snapshot,
                "store:transcript:dirty_record_suffix_rows",
            ),
            record_inserted: perf_value_max(snapshot, "store:transcript:record_db_rows_inserted"),
            record_deleted: perf_value_max(snapshot, "store:transcript:record_db_rows_deleted"),
            read_range_rows: perf_value_max(snapshot, "store:history:read_range_rows"),
            cached_read_write_db: perf_value_max(snapshot, "store:lineage:cached_read_write"),
            invariant_history_rows: perf_value_max(
                snapshot,
                "store:session:invariant_history_rows",
            ),
            search_blob_rows: perf_value_max(snapshot, "store:transcript:search_blob_rows_read"),
            search_blob_bytes: perf_value_max(snapshot, "store:transcript:search_blob_bytes_read"),
            user_turn_blocks_scanned: perf_value_max(
                snapshot,
                "transcript:last_user_block_index:blocks_scanned",
            )
            .max(perf_value_max(
                snapshot,
                "transcript:user_turns:blocks_scanned",
            )),
            user_turns_cloned: perf_value_max(snapshot, "transcript:user_turns:users_cloned"),
            user_turn_text_bytes_cloned: perf_value_max(
                snapshot,
                "transcript:user_turns:text_bytes_cloned",
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamResumePosition {
    Top,
    Middle,
    Tail,
}

impl StreamResumePosition {
    fn from_env() -> Self {
        match std::env::var("SMELT_TRANSCRIPT_STREAM_RESUMED_POSITION")
            .unwrap_or_else(|_| "tail".into())
            .as_str()
        {
            "top" => Self::Top,
            "middle" => Self::Middle,
            "tail" | "bottom" => Self::Tail,
            value => {
                panic!("unknown resumed stream position {value:?}; expected top, middle, or tail")
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Middle => "middle",
            Self::Tail => "tail",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamWorkload {
    Text,
    Bash,
    Mixed,
    Exec,
    WriteDraft,
    ExploreGroup,
}

impl StreamWorkload {
    fn from_env() -> Self {
        match std::env::var("SMELT_TRANSCRIPT_STREAM_WORKLOAD")
            .unwrap_or_else(|_| "text".into())
            .as_str()
        {
            "text" => Self::Text,
            "bash" | "tool" => Self::Bash,
            "mixed" => Self::Mixed,
            "exec" | "shell" => Self::Exec,
            "write-draft" | "draft" => Self::WriteDraft,
            "explore-group" | "group" => Self::ExploreGroup,
            value => panic!(
                "unknown stream workload {value:?}; expected text, bash, mixed, exec, write-draft, or explore-group"
            ),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Bash => "bash",
            Self::Mixed => "mixed",
            Self::Exec => "exec",
            Self::WriteDraft => "write-draft",
            Self::ExploreGroup => "explore-group",
        }
    }

    fn uses_agent_turn(self) -> bool {
        self != Self::Exec
    }
}

#[derive(Clone, Copy, Debug)]
enum StreamEventKind {
    Text,
    Reasoning,
    ToolDraft,
    ToolOutput,
    ExecOutput,
    Lifecycle,
}

impl StreamEventKind {
    const COUNT: usize = 6;

    fn index(self) -> usize {
        match self {
            Self::Text => 0,
            Self::Reasoning => 1,
            Self::ToolDraft => 2,
            Self::ToolOutput => 3,
            Self::ExecOutput => 4,
            Self::Lifecycle => 5,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Reasoning => "reasoning",
            Self::ToolDraft => "tool_draft",
            Self::ToolOutput => "tool_output",
            Self::ExecOutput => "exec_output",
            Self::Lifecycle => "lifecycle",
        }
    }
}

enum StreamBenchmarkEvent {
    Engine {
        kind: StreamEventKind,
        event: Box<protocol::EngineEvent>,
    },
    ExecStarted {
        command: String,
    },
    ExecOutput {
        chunk: String,
    },
    ExecFinished,
}

impl StreamBenchmarkEvent {
    fn kind(&self) -> StreamEventKind {
        match self {
            Self::Engine { kind, .. } => *kind,
            Self::ExecOutput { .. } => StreamEventKind::ExecOutput,
            Self::ExecStarted { .. } | Self::ExecFinished => StreamEventKind::Lifecycle,
        }
    }
}

#[derive(Debug, Default)]
struct StreamEventAccumulator {
    dispatch_ms: Vec<f64>,
    allocs: u64,
    bytes: u64,
    max_event_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct StreamEventSample {
    kind: StreamEventKind,
    count: usize,
    dispatch: TailStats,
    allocs: u64,
    bytes: u64,
    max_event_bytes: u64,
}

#[derive(Debug)]
struct StreamSample {
    workload: StreamWorkload,
    history_blocks: usize,
    resumed_bytes: usize,
    resumed_position: StreamResumePosition,
    boundary_record_bytes: usize,
    terminal_width: u16,
    terminal_height: u16,
    parallel_tools: usize,
    chunks: usize,
    events: usize,
    final_bytes: usize,
    active_output_bytes: usize,
    scheduled: bool,
    scroll: bool,
    idle_frames: usize,
    total_ms: f64,
    dispatch: TailStats,
    render: TailStats,
    frame: TailStats,
    event_samples: Vec<StreamEventSample>,
    traced_frames: usize,
    request_to_flush_p99_ms: f64,
    thread_allocs: u64,
    thread_bytes: u64,
    process_alloc_bytes: u64,
    process_dealloc_bytes: u64,
    process_retained_bytes: i64,
    metadata_readers: usize,
    hydration_readers: usize,
    total_readers: usize,
    metadata_open_attempts: usize,
    hydration_open_attempts: usize,
    total_open_attempts: usize,
}

fn stream_benchmark_enabled() -> bool {
    matches!(
        std::env::var("SMELT_TRANSCRIPT_STREAM_BENCH").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn stream_benchmark_history_blocks() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_STREAM_HISTORY", 100)
}

fn stream_benchmark_resumed_bytes() -> Option<usize> {
    std::env::var("SMELT_TRANSCRIPT_STREAM_RESUMED_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|bytes| *bytes > 0)
}

fn stream_benchmark_boundary_record_bytes() -> usize {
    std::env::var("SMELT_TRANSCRIPT_STREAM_BOUNDARY_RECORD_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

fn stream_benchmark_chunks() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_STREAM_CHUNKS", 512)
}

fn stream_benchmark_final_bytes() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_STREAM_BYTES", 16 * 1024)
}

fn stream_benchmark_terminal_width() -> u16 {
    env_positive_usize("SMELT_TRANSCRIPT_STREAM_WIDTH", 100)
        .try_into()
        .expect("stream benchmark terminal width must fit in u16")
}

fn stream_benchmark_terminal_height() -> u16 {
    env_positive_usize("SMELT_TRANSCRIPT_STREAM_HEIGHT", 32)
        .try_into()
        .expect("stream benchmark terminal height must fit in u16")
}

fn stream_benchmark_parallel_tools() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_STREAM_PARALLEL_TOOLS", 4)
}

fn stream_benchmark_scheduled() -> bool {
    matches!(
        std::env::var("SMELT_TRANSCRIPT_STREAM_SCHEDULED").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) || stream_benchmark_scroll()
}

fn stream_benchmark_scroll() -> bool {
    matches!(
        std::env::var("SMELT_TRANSCRIPT_STREAM_SCROLL").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn stream_benchmark_idle_frames() -> usize {
    std::env::var("SMELT_TRANSCRIPT_STREAM_IDLE_FRAMES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

fn push_stream_boundary_transcript(app: &mut TestApp, record_bytes: usize) -> usize {
    const RECORD_COUNT: usize = 600;
    const LARGE_RECORDS: [usize; 3] = [0, RECORD_COUNT / 2, RECORD_COUNT - 1];
    let mut generated_bytes = 0usize;
    for index in 0..RECORD_COUNT {
        let target = if LARGE_RECORDS.contains(&index) {
            record_bytes.max(1)
        } else {
            64
        };
        let prefix = format!("stream resume boundary record {index}: ");
        let mut content = String::with_capacity(target.max(prefix.len()));
        content.push_str(&prefix);
        while content.len() < target {
            let remaining = target - content.len();
            let chunk = "deterministic extent boundary payload 0123456789 abcdef\n";
            content.push_str(&chunk[..remaining.min(chunk.len())]);
        }
        generated_bytes = generated_bytes.saturating_add(content.len());
        let block = if index.is_multiple_of(2) {
            smelt_core::transcript_model::Block::User {
                text: content,
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }
        } else {
            smelt_core::transcript_model::Block::Text {
                content: content.into(),
            }
        };
        app.app.push_block(block);
    }
    generated_bytes
}

fn stream_benchmark_app(
    history_blocks: usize,
    resumed_bytes: Option<usize>,
    resumed_position: StreamResumePosition,
    workload: StreamWorkload,
    terminal_width: u16,
    terminal_height: u16,
) -> (TestApp, usize) {
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(terminal_width, terminal_height);
    let resumed_bytes = if let Some(target_bytes) = resumed_bytes {
        let boundary_record_bytes = stream_benchmark_boundary_record_bytes();
        let bytes = if boundary_record_bytes == 0 {
            push_search_bench_transcript(&mut app, target_bytes)
        } else {
            push_stream_boundary_transcript(&mut app, boundary_record_bytes)
        };
        app.render_silent();
        let receipt = save_bench_fixture(&mut app, "resumed stream");
        wait_for_bench_catalog(&app, "resumed stream", &receipt);
        install_sparse_resume_bench_transcript(&mut app);
        match resumed_position {
            StreamResumePosition::Top => {
                prepare_burst_bench_position(&mut app, BurstBenchPosition::Top)
            }
            StreamResumePosition::Middle => {
                prepare_burst_bench_position(&mut app, BurstBenchPosition::Middle)
            }
            StreamResumePosition::Tail => {
                prepare_burst_bench_position(&mut app, BurstBenchPosition::Bottom)
            }
        }
        app.app.handle_resize(terminal_width, terminal_height);
        bytes
    } else {
        for index in 0..history_blocks {
            let block = match index % 4 {
                0 => smelt_core::transcript_model::Block::User {
                    text: format!(
                        "stream benchmark prompt {index}: inspect the command output and summarize failures"
                    ),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                },
                1 => smelt_core::transcript_model::Block::Text {
                    content: format!(
                        "## Earlier result {index}\n\n- parsed output\n- checked status\n\n{}",
                        "alpha beta gamma delta ".repeat(5)
                    )
                    .into(),
                },
                2 => smelt_core::transcript_model::Block::Thinking {
                    title: None,
                    summary_titles: Vec::new(),
                    content: format!("Inspecting earlier result {index} and comparing the logs.")
                        .into(),
                    kind: protocol::ReasoningKind::Raw,
                },
                _ => smelt_core::transcript_model::Block::Text {
                    content: format!(
                        "```text\nfinished previous task {index}\nstatus: ok\n```\n\nReady for the next command."
                    )
                    .into(),
                },
            };
            app.app.push_block(block);
        }
        0
    };
    if workload.uses_agent_turn() {
        app.start_turn(42);
    }
    app.render_silent();
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    (app, resumed_bytes)
}

fn stream_benchmark_chunk_lengths(chunks: usize, final_bytes: usize) -> Vec<usize> {
    const WEIGHTS: [usize; 16] = [1, 1, 3, 8, 2, 1, 13, 2, 5, 1, 21, 3, 1, 8, 2, 34];
    let remaining = final_bytes.saturating_sub(chunks);
    let total_weight = (0..chunks)
        .map(|index| WEIGHTS[index % WEIGHTS.len()])
        .sum::<usize>();
    let mut lengths = (0..chunks)
        .map(|index| {
            let weight = WEIGHTS[index % WEIGHTS.len()];
            1 + ((remaining as u128 * weight as u128) / total_weight as u128) as usize
        })
        .collect::<Vec<_>>();
    let assigned = lengths.iter().sum::<usize>();
    for index in 0..final_bytes.saturating_sub(assigned) {
        lengths[index % chunks] += 1;
    }
    lengths
}

fn stream_benchmark_payload(index: usize, len: usize, bash: bool) -> String {
    let pattern = if bash {
        "\u{1b}[32mPASS\u{1b}[0m crate=smelt-tui test=streaming_output elapsed=12ms path=crates/tui/src/app.rs café 東京\nwarning: retrying deterministic fixture\n"
    } else {
        "## Streaming analysis\n\n- inspect `render_loop`\n- compare **frame tails**\n\n```rust\nlet status = Result::<(), Error>::Ok(());\n```\n\nThe next chunk crosses markdown boundaries.\n"
    };
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(len);
    let mut position = index % chars.len();
    while output.len() < len {
        let character = chars[position];
        if output.len() + character.len_utf8() <= len {
            output.push(character);
        } else {
            output.push('x');
        }
        position = (position + 1) % chars.len();
    }
    output
}

fn stream_benchmark_source(final_bytes: usize) -> String {
    const PATTERN: &str = "pub fn streamed_value(input: usize) -> usize {\n    input.saturating_mul(2).saturating_add(1)\n}\n\n";
    let mut source = PATTERN.repeat(final_bytes.div_ceil(PATTERN.len()));
    source.truncate(final_bytes);
    source
}

fn append_streamed_output(output: &mut String, chunk: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(chunk);
}

fn stream_engine_event(
    kind: StreamEventKind,
    event: protocol::EngineEvent,
) -> StreamBenchmarkEvent {
    StreamBenchmarkEvent::Engine {
        kind,
        event: Box::new(event),
    }
}

fn stream_benchmark_events(
    workload: StreamWorkload,
    chunks: usize,
    final_bytes: usize,
    parallel_tools: usize,
) -> (Vec<StreamBenchmarkEvent>, usize) {
    let lengths = stream_benchmark_chunk_lengths(chunks, final_bytes);
    let mut events = Vec::with_capacity(chunks.saturating_mul(parallel_tools) + 16);
    let invocation_id = protocol::InvocationId::new(1);
    let call_id = "stream-benchmark-bash".to_string();
    let tool_started = || {
        stream_engine_event(
            StreamEventKind::Lifecycle,
            protocol::EngineEvent::ToolStarted {
                invocation_id,
                call_id: call_id.clone(),
                tool_name: "bash".into(),
                args: std::collections::HashMap::from([(
                    "command".into(),
                    serde_json::json!("cargo nextest run --workspace --features smelt-tui/harness"),
                )]),
                called_at_ms: 0,
            },
        )
    };
    let tool_output = |line: String| {
        stream_engine_event(
            StreamEventKind::ToolOutput,
            protocol::EngineEvent::ToolOutput {
                invocation_id,
                call_id: call_id.clone(),
                line,
            },
        )
    };
    let tool_finished = |output: String| {
        stream_engine_event(
            StreamEventKind::Lifecycle,
            protocol::EngineEvent::ToolFinished {
                invocation_id,
                call_id: call_id.clone(),
                result: protocol::ToolOutcome::new(output, false, None),
                elapsed_ms: Some(1_250),
            },
        )
    };

    let active_output_bytes = match workload {
        StreamWorkload::Text => {
            for (index, len) in lengths.into_iter().enumerate() {
                events.push(stream_engine_event(
                    StreamEventKind::Text,
                    protocol::EngineEvent::TextDelta {
                        delta: stream_benchmark_payload(index, len, false),
                    },
                ));
            }
            final_bytes
        }
        StreamWorkload::Bash => {
            events.push(tool_started());
            let mut output = String::with_capacity(final_bytes + chunks);
            for (index, len) in lengths.into_iter().enumerate() {
                let chunk = stream_benchmark_payload(index, len, true);
                append_streamed_output(&mut output, &chunk);
                events.push(tool_output(chunk));
            }
            let active_output_bytes = output.len();
            events.push(tool_finished(output));
            active_output_bytes
        }
        StreamWorkload::Mixed => {
            let reasoning_end = chunks / 8;
            let leading_text_end = chunks * 3 / 8;
            let tool_end = chunks * 7 / 8;
            events.push(stream_engine_event(
                StreamEventKind::Lifecycle,
                protocol::EngineEvent::ReasoningPartStarted {
                    id: "stream-benchmark-reasoning".into(),
                    kind: protocol::ReasoningKind::Raw,
                },
            ));
            let mut tool_started_sent = false;
            let mut output = String::with_capacity(final_bytes / 2 + chunks);
            let mut active_output_bytes = 0usize;
            for (index, len) in lengths.into_iter().enumerate() {
                if index < reasoning_end {
                    events.push(stream_engine_event(
                        StreamEventKind::Reasoning,
                        protocol::EngineEvent::ReasoningPartDelta {
                            id: "stream-benchmark-reasoning".into(),
                            kind: protocol::ReasoningKind::Raw,
                            delta: stream_benchmark_payload(index, len, false),
                            title: None,
                        },
                    ));
                } else if index < leading_text_end {
                    events.push(stream_engine_event(
                        StreamEventKind::Text,
                        protocol::EngineEvent::TextDelta {
                            delta: stream_benchmark_payload(index, len, false),
                        },
                    ));
                } else if index < tool_end {
                    if !tool_started_sent {
                        events.push(stream_engine_event(
                            StreamEventKind::Lifecycle,
                            protocol::EngineEvent::ReasoningPartFinished {
                                id: "stream-benchmark-reasoning".into(),
                                kind: protocol::ReasoningKind::Raw,
                                title: None,
                                content: String::new(),
                            },
                        ));
                        events.push(tool_started());
                        tool_started_sent = true;
                    }
                    let chunk = stream_benchmark_payload(index, len, true);
                    append_streamed_output(&mut output, &chunk);
                    active_output_bytes = output.len();
                    events.push(tool_output(chunk));
                } else {
                    if tool_started_sent {
                        events.push(tool_finished(std::mem::take(&mut output)));
                        tool_started_sent = false;
                    }
                    events.push(stream_engine_event(
                        StreamEventKind::Text,
                        protocol::EngineEvent::TextDelta {
                            delta: stream_benchmark_payload(index, len, false),
                        },
                    ));
                }
            }
            if tool_started_sent {
                events.push(tool_finished(std::mem::take(&mut output)));
            }
            active_output_bytes
        }
        StreamWorkload::Exec => {
            events.push(StreamBenchmarkEvent::ExecStarted {
                command: "cargo nextest run --workspace --features smelt-tui/harness".into(),
            });
            let mut output = String::with_capacity(final_bytes + chunks);
            for (index, len) in lengths.into_iter().enumerate() {
                let chunk = stream_benchmark_payload(index, len, true);
                append_streamed_output(&mut output, &chunk);
                events.push(StreamBenchmarkEvent::ExecOutput { chunk });
            }
            events.push(StreamBenchmarkEvent::ExecFinished);
            output.len()
        }
        StreamWorkload::WriteDraft => {
            let stream_id = "stream-benchmark-write-draft".to_string();
            let call_id = "stream-benchmark-write-call".to_string();
            let arguments = serde_json::to_string(&serde_json::json!({
                "file_path": "/tmp/stream-benchmark.rs",
                "content": stream_benchmark_source(final_bytes),
            }))
            .expect("serialize write_file arguments");
            events.push(stream_engine_event(
                StreamEventKind::Lifecycle,
                protocol::EngineEvent::ToolCallDraftStarted {
                    stream_id: stream_id.clone(),
                    call_id: Some(call_id.clone()),
                    tool_name: Some("write_file".into()),
                },
            ));
            let argument_lengths = stream_benchmark_chunk_lengths(chunks, arguments.len());
            let mut offset = 0usize;
            for len in argument_lengths {
                let end = offset + len;
                events.push(stream_engine_event(
                    StreamEventKind::ToolDraft,
                    protocol::EngineEvent::ToolCallDraftDelta {
                        stream_id: stream_id.clone(),
                        call_id: Some(call_id.clone()),
                        tool_name: Some("write_file".into()),
                        delta: arguments[offset..end].to_string(),
                    },
                ));
                offset = end;
            }
            events.push(stream_engine_event(
                StreamEventKind::Lifecycle,
                protocol::EngineEvent::ToolCallDraftFinished {
                    stream_id,
                    call_id,
                    tool_name: "write_file".into(),
                    arguments: arguments.clone(),
                },
            ));
            arguments.len()
        }
        StreamWorkload::ExploreGroup => {
            const TOOL_NAMES: [&str; 4] = ["read_file", "grep", "glob", "outline"];
            let mut outputs = vec![String::with_capacity(final_bytes + chunks); parallel_tools];
            for tool_index in 0..parallel_tools {
                let invocation_id = protocol::InvocationId::new(tool_index as u64 + 1);
                let call_id = format!("stream-benchmark-explore-{tool_index}");
                let tool_name = TOOL_NAMES[tool_index % TOOL_NAMES.len()];
                let args = std::collections::HashMap::from([
                    (
                        "file_path".into(),
                        serde_json::json!(format!("crates/tui/src/group_{tool_index}.rs")),
                    ),
                    ("path".into(), serde_json::json!("crates/tui/src")),
                    ("pattern".into(), serde_json::json!("render|layout|stream")),
                ]);
                events.push(stream_engine_event(
                    StreamEventKind::Lifecycle,
                    protocol::EngineEvent::ToolStarted {
                        invocation_id,
                        call_id,
                        tool_name: tool_name.into(),
                        args,
                        called_at_ms: tool_index as u64,
                    },
                ));
            }
            for (chunk_index, len) in lengths.into_iter().enumerate() {
                for (tool_index, output) in outputs.iter_mut().enumerate() {
                    let invocation_id = protocol::InvocationId::new(tool_index as u64 + 1);
                    let call_id = format!("stream-benchmark-explore-{tool_index}");
                    let line = stream_benchmark_payload(
                        chunk_index + tool_index.saturating_mul(chunks),
                        len,
                        true,
                    );
                    append_streamed_output(output, &line);
                    events.push(stream_engine_event(
                        StreamEventKind::ToolOutput,
                        protocol::EngineEvent::ToolOutput {
                            invocation_id,
                            call_id,
                            line,
                        },
                    ));
                }
            }
            let active_output_bytes = outputs.iter().map(String::len).sum();
            for (tool_index, output) in outputs.into_iter().enumerate() {
                events.push(stream_engine_event(
                    StreamEventKind::Lifecycle,
                    protocol::EngineEvent::ToolFinished {
                        invocation_id: protocol::InvocationId::new(tool_index as u64 + 1),
                        call_id: format!("stream-benchmark-explore-{tool_index}"),
                        result: protocol::ToolOutcome::new(output, false, None),
                        elapsed_ms: Some(1_250),
                    },
                ));
            }
            active_output_bytes
        }
    };

    if workload.uses_agent_turn() {
        events.push(stream_engine_event(
            StreamEventKind::Lifecycle,
            protocol::EngineEvent::TurnComplete {
                turn_id: 42,
                history: None,
                meta: None,
            },
        ));
    }
    (events, active_output_bytes)
}

fn stream_frame_stats(snapshot: &smelt_perf::perf::Snapshot) -> TailStats {
    let row = snapshot
        .durations
        .iter()
        .find(|row| row.label == "app:tick_compositor")
        .expect("stream benchmark renders at least one compositor frame");
    TailStats {
        mean: row.total_us as f64 / row.count as f64 / 1_000.0,
        stddev: 0.0,
        p50: row.p50_us as f64 / 1_000.0,
        p95: row.p95_us as f64 / 1_000.0,
        p99: row.p99_us as f64 / 1_000.0,
        max: row.max_us as f64 / 1_000.0,
    }
}

fn print_stream_perf(workload: StreamWorkload, snapshot: &smelt_perf::perf::Snapshot) {
    const LABELS: &[&str] = &[
        "tui:dispatch_engine_event",
        "app:tick_compositor",
        "compositor:layout",
        "compositor:project_transcript",
        "compositor:lua_renderers",
        "compositor:input",
        "compositor:render_flush",
        "session:live_store:open",
        "engine:model_history:open_store",
        "transcript:store:open_read_only",
        "store:lineage:open_read_only_located",
        "store:object:hydrate_bytes",
        "store:transcript:read_record_slice",
        "transcript:plan_viewport_projection",
        "transcript:plan_sparse_projection",
        "transcript:hydration:plan_ids",
        "transcript:hydration:projection_plan",
        "transcript:hydration:ensure_ids",
        "transcript:hydration:refine_plan",
        "transcript:sparse:activate_tail_window",
        "transcript:sparse:loaded_row_offset",
        "transcript:sparse:estimated_loaded_rows",
        "transcript:sparse:scrollbar_total_rows",
        "transcript:extent:estimated_rows_before_record",
        "transcript:extent:estimated_rows_for_record_range",
        "transcript:extent:sqlite_estimated_record_rows",
        "transcript:navigation:block_from_anchor",
        "transcript:navigation:record_from_store",
        "transcript:navigation:record_before_kind",
        "store:extent:reader_estimated_rows",
        "store:extent:estimated_rows",
        "transcript:transcript_scene",
        "transcript:prepare_row_index",
        "transcript:prepare_row_index:rebuild_index",
        "transcript:row_index:collect_missing",
        "transcript:measure_block:layout",
        "transcript:project_planned",
        "transcript:layout_cache:ensure_many",
        "transcript:layout_cache:ensure_measurement",
        "transcript:layout_cache:compile_and_insert",
        "transcript:layout_cache:block_render_metadata",
        "transcript:layout_cache:compile_layouts",
        "transcript:layout_cache:render_full_to_buffer",
        "transcript:layout_cache:render_range_to_buffer",
        "transcript:project_visible_range",
        "transcript:project_visible_range:buffer_install",
        "transcript:project_visible_range:clone_display_rows",
        "transcript:collect_nodes_range",
        "transcript:draft:append_json",
        "transcript:stream:append_tool_output",
        "transcript:stream:append_exec_output",
        "transcript:content:append_metadata",
        "transcript:content:append_hash",
        "transcript:content:append_file_layouts",
        "transcript:content:append_ansi_index",
        "transcript:content:append_markdown_index",
        "render:layout",
        "render:layout:cap",
        "render:layout:cap:measure_child",
        "render:layout:cap:select_rows",
        "render:layout:cap:render_rows",
        "render:layout:cap:content",
        "render:layout:cap:runs",
        "render:layout:cap:leaf",
        "render:layout:cap:vbox",
        "render:layout:cap:hbox",
        "render:layout:cap:gutter",
        "render:layout:cap:row_prefix",
        "render:layout:cap:panel",
        "render:layout:cap:style",
        "render:layout:cap:nested",
        "render:layout:cap:refresh",
        "render:layout:cap:empty",
        "render:layout:hbox:prepare",
        "render:layout:hbox:render_columns",
        "render:layout:hbox:runs",
        "render:layout:hbox:line",
        "render:layout:hbox:other",
        "render:layout:hbox:compose_rows",
        "render:layout:inline_syntax",
        "render:layout:measure_content_text",
        "render:layout:render_content_text",
        "render:layout:measure_text",
        "render:layout:render_text",
        "render:markdown",
    ];
    const VALUE_LABELS: &[&str] = &[
        "transcript:hydration:ensure_ids:requested",
        "transcript:hydration:ensure_ids:missing",
        "transcript:hydration:ensure_ids:ranges",
        "transcript:hydration:projection_plan:iterations",
        "transcript:hydration:projection_plan:max_required_ids",
        "transcript:block_cache:hydration_reads",
        "transcript:block_cache:hydration_bytes",
        "transcript:block_cache:hydration_duration_us",
        "transcript:extent:estimated_rows_for_record_range:records",
        "transcript:extent:sqlite_estimated_record_rows:records",
        "store:extent:estimated_rows:records",
        "transcript:pending_work:applied",
        "transcript:stream:tool_output_accumulated_bytes",
        "transcript:stream:exec_output_accumulated_bytes",
        "transcript:layout_cache:requested",
        "transcript:layout_cache:compiled",
        "transcript:layout_cache:key_miss",
        "transcript:layout_cache:entry_miss",
        "transcript:layout_cache:group_miss",
        "transcript:layout_cache:block_miss",
        "transcript:layout_cache:content_key_miss",
        "transcript:layout_cache:sidecar_key_miss",
        "transcript:layout_cache:renderer_key_miss",
        "transcript:layout_cache:context_key_miss",
        "transcript:row_index:exactify_missing",
        "transcript:prepare_row_index:blocks",
        "transcript:prepare_row_index:reused_index",
        "transcript:render_cache:retained_bytes",
        "transcript:render_cache:pinned_bytes",
        "transcript:render_cache:oversize_debt_bytes",
        "render:layout:cap_child_rows",
        "render:layout:measure_text_bytes",
        "render:layout:render_text_bytes",
        "render:layout:render_text_row_start",
    ];
    for row in snapshot
        .durations
        .iter()
        .filter(|row| LABELS.contains(&row.label))
    {
        eprintln!(
            "TRANSCRIPT_STREAM_PERF workload={} metric={} count={} total_us={} p50_us={} p95_us={} p99_us={} max_us={}",
            workload.label(),
            row.label,
            row.count,
            row.total_us,
            row.p50_us,
            row.p95_us,
            row.p99_us,
            row.max_us,
        );
    }
    for row in snapshot
        .allocs
        .iter()
        .filter(|row| LABELS.contains(&row.label))
    {
        eprintln!(
            "TRANSCRIPT_STREAM_PERF_ALLOC workload={} metric={} count={} allocs_total={} bytes_total={} bytes_p95={} bytes_max={}",
            workload.label(),
            row.label,
            row.count,
            row.allocs_total,
            row.bytes_total,
            row.bytes_p95,
            row.bytes_max,
        );
    }
    for row in snapshot
        .durations
        .iter()
        .filter(|row| row.max_us >= 10_000 && !LABELS.contains(&row.label))
    {
        eprintln!(
            "TRANSCRIPT_STREAM_PERF_HOT workload={} metric={} count={} total_us={} p50_us={} p95_us={} p99_us={} max_us={}",
            workload.label(),
            row.label,
            row.count,
            row.total_us,
            row.p50_us,
            row.p95_us,
            row.p99_us,
            row.max_us,
        );
    }
    for row in snapshot
        .allocs
        .iter()
        .filter(|row| row.bytes_max >= 10 * 1024 * 1024 && !LABELS.contains(&row.label))
    {
        eprintln!(
            "TRANSCRIPT_STREAM_PERF_ALLOC_HOT workload={} metric={} count={} allocs_total={} bytes_total={} bytes_p95={} bytes_max={}",
            workload.label(),
            row.label,
            row.count,
            row.allocs_total,
            row.bytes_total,
            row.bytes_p95,
            row.bytes_max,
        );
    }
    for row in snapshot
        .values
        .iter()
        .filter(|row| VALUE_LABELS.contains(&row.label))
    {
        eprintln!(
            "TRANSCRIPT_STREAM_PERF_VALUE workload={} metric={} count={} total={} p95={} p99={} max={}",
            workload.label(),
            row.label,
            row.count,
            row.total,
            row.p95,
            row.p99,
            row.max,
        );
    }
}

fn run_stream_benchmark_sample() -> StreamSample {
    let workload = StreamWorkload::from_env();
    let history_blocks = stream_benchmark_history_blocks();
    let resumed_bytes = stream_benchmark_resumed_bytes();
    let resumed_position = StreamResumePosition::from_env();
    let boundary_record_bytes = stream_benchmark_boundary_record_bytes();
    let chunks = stream_benchmark_chunks();
    let final_bytes = stream_benchmark_final_bytes().max(chunks);
    let terminal_width = stream_benchmark_terminal_width();
    let terminal_height = stream_benchmark_terminal_height();
    let parallel_tools = stream_benchmark_parallel_tools();
    let scheduled = stream_benchmark_scheduled();
    let scroll = stream_benchmark_scroll();
    let idle_frames = stream_benchmark_idle_frames();
    let (events, active_output_bytes) =
        stream_benchmark_events(workload, chunks, final_bytes, parallel_tools);
    let event_count = events.len();
    let (mut app, resumed_bytes) = stream_benchmark_app(
        history_blocks,
        resumed_bytes,
        resumed_position,
        workload,
        terminal_width,
        terminal_height,
    );
    let readers_before = if resumed_bytes > 0 {
        wait_for_sparse_reader_metrics(&mut app, "resumed stream")
    } else {
        app.app.transcript_reader_metrics_for_harness()
    };
    let mut dispatch_ms = Vec::with_capacity(event_count);
    let mut render_ms = Vec::with_capacity(event_count + chunks.div_ceil(8));
    let mut event_accumulators: [StreamEventAccumulator; StreamEventKind::COUNT] =
        std::array::from_fn(|_| StreamEventAccumulator::default());
    for accumulator in &mut event_accumulators {
        accumulator.dispatch_ms.reserve(event_count);
    }
    let mut traced_frames = 0usize;

    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    smelt_perf::alloc::set_enabled(true);
    let process_before = smelt_perf::alloc::snapshot();
    let (allocs_before, bytes_before) = smelt_perf::alloc::thread_snapshot();
    let total_start = std::time::Instant::now();
    for (index, event) in events.into_iter().enumerate() {
        if scheduled && index > 0 && index.is_multiple_of(16) {
            app.clock.advance(std::time::Duration::from_millis(20));
        }
        let kind = event.kind();
        let (event_allocs_before, event_bytes_before) = smelt_perf::alloc::thread_snapshot();
        let dispatch_start = std::time::Instant::now();
        let mut pre_dispatch_frames = 0usize;
        match event {
            StreamBenchmarkEvent::Engine { event, .. } => {
                app.app.dispatch_engine_event_in_render_loop_to(
                    *event,
                    &mut std::io::sink(),
                    |_| pre_dispatch_frames += 1,
                );
            }
            StreamBenchmarkEvent::ExecStarted { command } => app.app.start_exec(command),
            StreamBenchmarkEvent::ExecOutput { chunk } => app.app.append_exec_output(chunk),
            StreamBenchmarkEvent::ExecFinished => app.app.finish_exec(None),
        }
        let dispatch_elapsed_ms = elapsed_ms(dispatch_start.elapsed());
        traced_frames += pre_dispatch_frames;

        if scheduled {
            let render_start = std::time::Instant::now();
            if app
                .app
                .render_requested_transient_frame_to(&mut std::io::sink())
            {
                render_ms.push(elapsed_ms(render_start.elapsed()));
                traced_frames += 1;
            }
        } else {
            let render_start = std::time::Instant::now();
            app.render_silent();
            render_ms.push(elapsed_ms(render_start.elapsed()));
            traced_frames += 1;
        }

        if scroll && index.is_multiple_of(8) {
            let modifiers = if (index / 8).is_multiple_of(2) {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            let key = if modifiers == KeyModifiers::CONTROL {
                KeyCode::Char('u')
            } else {
                KeyCode::Char('G')
            };
            app.press_mod(key, modifiers);
            app.app.request_urgent_render();
            let render_start = std::time::Instant::now();
            app.render_silent();
            render_ms.push(elapsed_ms(render_start.elapsed()));
            traced_frames += 1;
        }

        let (event_allocs_after, event_bytes_after) = smelt_perf::alloc::thread_snapshot();
        let event_allocs = event_allocs_after.saturating_sub(event_allocs_before);
        let event_bytes = event_bytes_after.saturating_sub(event_bytes_before);
        let accumulator = &mut event_accumulators[kind.index()];
        accumulator.dispatch_ms.push(dispatch_elapsed_ms);
        accumulator.allocs = accumulator.allocs.saturating_add(event_allocs);
        accumulator.bytes = accumulator.bytes.saturating_add(event_bytes);
        accumulator.max_event_bytes = accumulator.max_event_bytes.max(event_bytes);
        dispatch_ms.push(dispatch_elapsed_ms);
    }
    if scheduled {
        let mut drain_frames = 0usize;
        while app.app.transcript_work_pending_for_harness()
            || app.app.scheduled_frame_delay().is_some()
        {
            drain_frames = drain_frames.saturating_add(1);
            assert!(
                drain_frames <= 1_024,
                "stream content work did not settle within 1,024 frames"
            );
            app.clock.advance(std::time::Duration::from_millis(20));
            let render_start = std::time::Instant::now();
            if app
                .app
                .render_requested_transient_frame_to(&mut std::io::sink())
            {
                render_ms.push(elapsed_ms(render_start.elapsed()));
                traced_frames += 1;
            }
        }
    }
    for _ in 0..idle_frames {
        app.clock.advance(std::time::Duration::from_millis(16));
        app.app
            .request_animation_render(std::time::Duration::from_millis(16));
        let render_start = std::time::Instant::now();
        if app
            .app
            .render_requested_transient_frame_to(&mut std::io::sink())
        {
            render_ms.push(elapsed_ms(render_start.elapsed()));
            traced_frames += 1;
        }
    }
    let total_ms = elapsed_ms(total_start.elapsed());
    let (allocs_after, bytes_after) = smelt_perf::alloc::thread_snapshot();
    let process_after = smelt_perf::alloc::snapshot();
    let process_delta = smelt_perf::alloc::delta(process_before, process_after);
    let readers_after = app.app.transcript_reader_metrics_for_harness();
    if resumed_bytes > 0 {
        assert_eq!(
            readers_after, readers_before,
            "provider events and frames must reuse both retained session readers"
        );
    }
    let perf_snapshot = smelt_perf::perf::snapshot();
    let memory_snapshot = app
        .app
        .conversation
        .transcript_memory_snapshot_for_harness();
    assert_stream_operation_gates(&perf_snapshot, memory_snapshot, workload.label());
    let frame_latency = perf_snapshot
        .values
        .iter()
        .find(|row| row.label == "frame:request_to_flush:us");
    let request_to_flush_p99_ms = frame_latency.map_or(0.0, |row| row.p99 as f64 / 1_000.0);
    if scheduled {
        let application_batches = perf_snapshot
            .values
            .iter()
            .find(|row| row.label == "transcript:pending_work:applied")
            .expect("scheduled stream records transcript work application batches");
        assert_eq!(
            application_batches.count as usize, traced_frames,
            "pending transcript work must be applied exactly once per compositor frame"
        );
    }
    if scheduled && workload == StreamWorkload::Text {
        let interaction_frames = if scroll { event_count.div_ceil(8) } else { 0 };
        let burst_frames = event_count.div_ceil(16);
        assert!(
            traced_frames <= idle_frames + interaction_frames + burst_frames + 8,
            "text stream scheduler emitted {traced_frames} frames for {event_count} events, {idle_frames} idle frames, and {interaction_frames} urgent interactions"
        );
    }
    print_stream_perf(workload, &perf_snapshot);
    let frame = stream_frame_stats(&perf_snapshot);
    smelt_perf::alloc::set_enabled(false);
    smelt_perf::perf::set_enabled(false);

    let kinds = [
        StreamEventKind::Text,
        StreamEventKind::Reasoning,
        StreamEventKind::ToolDraft,
        StreamEventKind::ToolOutput,
        StreamEventKind::ExecOutput,
        StreamEventKind::Lifecycle,
    ];
    let event_samples = kinds
        .into_iter()
        .filter_map(|kind| {
            let accumulator = &event_accumulators[kind.index()];
            (!accumulator.dispatch_ms.is_empty()).then(|| StreamEventSample {
                kind,
                count: accumulator.dispatch_ms.len(),
                dispatch: TailStats::from(&accumulator.dispatch_ms),
                allocs: accumulator.allocs,
                bytes: accumulator.bytes,
                max_event_bytes: accumulator.max_event_bytes,
            })
        })
        .collect();

    StreamSample {
        workload,
        history_blocks,
        resumed_bytes,
        resumed_position,
        boundary_record_bytes,
        terminal_width,
        terminal_height,
        parallel_tools,
        chunks,
        events: event_count,
        final_bytes,
        active_output_bytes,
        scheduled,
        scroll,
        idle_frames,
        total_ms,
        dispatch: TailStats::from(&dispatch_ms),
        render: TailStats::from(&render_ms),
        frame,
        event_samples,
        traced_frames,
        request_to_flush_p99_ms,
        thread_allocs: allocs_after.saturating_sub(allocs_before),
        thread_bytes: bytes_after.saturating_sub(bytes_before),
        process_alloc_bytes: process_delta.bytes_allocated,
        process_dealloc_bytes: process_delta.bytes_deallocated,
        process_retained_bytes: process_after.current_bytes as i64
            - process_before.current_bytes as i64,
        metadata_readers: readers_after.metadata_readers,
        hydration_readers: readers_after.hydration_readers,
        total_readers: readers_after.total_readers,
        metadata_open_attempts: readers_after.metadata_open_attempts,
        hydration_open_attempts: readers_after.hydration_open_attempts,
        total_open_attempts: readers_after.total_open_attempts,
    }
}

fn print_stream_sample(sample: &StreamSample) {
    for event in &sample.event_samples {
        println!(
            "TRANSCRIPT_STREAM_EVENT workload={} kind={} count={} dispatch_mean_ms={:.3} dispatch_p95_ms={:.3} dispatch_p99_ms={:.3} dispatch_max_ms={:.3} allocs={} bytes={} max_event_bytes={}",
            sample.workload.label(),
            event.kind.label(),
            event.count,
            event.dispatch.mean,
            event.dispatch.p95,
            event.dispatch.p99,
            event.dispatch.max,
            event.allocs,
            event.bytes,
            event.max_event_bytes,
        );
    }
    println!(
        "TRANSCRIPT_STREAM_SAMPLE workload={} history_blocks={} resumed_bytes={} resumed_position={} boundary_record_bytes={} terminal_width={} terminal_height={} parallel_tools={} chunks={} events={} final_bytes={} active_output_bytes={} scheduled={} scroll={} idle_frames={} total_ms={:.3} dispatch_mean_ms={:.3} dispatch_p95_ms={:.3} dispatch_p99_ms={:.3} dispatch_max_ms={:.3} render_mean_ms={:.3} render_p95_ms={:.3} render_p99_ms={:.3} render_max_ms={:.3} frame_mean_ms={:.3} frame_p95_ms={:.3} frame_p99_ms={:.3} frame_max_ms={:.3} traced_frames={} request_to_flush_p99_ms={:.3} thread_allocs={} thread_bytes={} process_alloc_bytes={} process_dealloc_bytes={} process_retained_bytes={} metadata_readers={} hydration_readers={} total_readers={} metadata_open_attempts={} hydration_open_attempts={} total_open_attempts={}",
        sample.workload.label(),
        sample.history_blocks,
        sample.resumed_bytes,
        sample.resumed_position.label(),
        sample.boundary_record_bytes,
        sample.terminal_width,
        sample.terminal_height,
        sample.parallel_tools,
        sample.chunks,
        sample.events,
        sample.final_bytes,
        sample.active_output_bytes,
        sample.scheduled,
        sample.scroll,
        sample.idle_frames,
        sample.total_ms,
        sample.dispatch.mean,
        sample.dispatch.p95,
        sample.dispatch.p99,
        sample.dispatch.max,
        sample.render.mean,
        sample.render.p95,
        sample.render.p99,
        sample.render.max,
        sample.frame.mean,
        sample.frame.p95,
        sample.frame.p99,
        sample.frame.max,
        sample.traced_frames,
        sample.request_to_flush_p99_ms,
        sample.thread_allocs,
        sample.thread_bytes,
        sample.process_alloc_bytes,
        sample.process_dealloc_bytes,
        sample.process_retained_bytes,
        sample.metadata_readers,
        sample.hydration_readers,
        sample.total_readers,
        sample.metadata_open_attempts,
        sample.hydration_open_attempts,
        sample.total_open_attempts,
    );
}

#[test]
fn transcript_stream_benchmark_suite() {
    if !benchmark_target_enabled() || !stream_benchmark_enabled() {
        return;
    }

    let runs = navigation_bench_runs();
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let sample = run_stream_benchmark_sample();
        print_stream_sample(&sample);
        samples.push(sample);
    }
    let totals = samples
        .iter()
        .map(|sample| sample.total_ms)
        .collect::<Vec<_>>();
    let frame_p99 = samples
        .iter()
        .map(|sample| sample.frame.p99)
        .collect::<Vec<_>>();
    let total = TailStats::from(&totals);
    let frame = TailStats::from(&frame_p99);
    println!(
        "TRANSCRIPT_STREAM_SUMMARY runs={} workload={} history_blocks={} resumed_bytes={} resumed_position={} boundary_record_bytes={} terminal_width={} terminal_height={} parallel_tools={} chunks={} final_bytes={} scheduled={} scroll={} idle_frames={} total_mean_ms={:.3} total_p95_ms={:.3} frame_p99_mean_ms={:.3} frame_p99_max_ms={:.3}",
        runs,
        samples[0].workload.label(),
        samples[0].history_blocks,
        samples[0].resumed_bytes,
        samples[0].resumed_position.label(),
        samples[0].boundary_record_bytes,
        samples[0].terminal_width,
        samples[0].terminal_height,
        samples[0].parallel_tools,
        samples[0].chunks,
        samples[0].final_bytes,
        samples[0].scheduled,
        samples[0].scroll,
        samples[0].idle_frames,
        total.mean,
        total.p95,
        frame.mean,
        frame.max,
    );
}

#[derive(Clone, Debug)]
struct HotPathSample {
    operation: &'static str,
    history_len: usize,
    history_item_bytes: usize,
    ms: f64,
    queue_wait_ms: u64,
    submit_turn_us: u64,
    transaction_commit_us: u64,
    begin_turn_us: u64,
    project_context_us: u64,
    thread_allocs: u64,
    thread_bytes_allocated: u64,
    process_bytes_allocated: u64,
    process_bytes_deallocated: u64,
    process_current_bytes_before: usize,
    process_current_bytes_after: usize,
    process_retained_bytes: i64,
    counters: HotPathCounters,
}

const HOT_PATH_OPERATIONS: &[&str] = &[
    "noop_save",
    "request_append",
    "history_appended",
    "turn_complete",
    "rewind_delete_suffix",
    "provider_history_read",
    "provider_history_uncheckpointed_read",
    "engine_request_materialization",
    "submit_enter",
    "submit_first_render",
];

fn hot_path_enabled() -> bool {
    matches!(
        std::env::var("SMELT_TRANSCRIPT_HOT_PATH").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn hot_path_history_len() -> usize {
    std::env::var("SMELT_TRANSCRIPT_HOT_PATH_HISTORY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|rows| *rows >= 4)
        .unwrap_or(1024)
}

fn hot_path_history_item_bytes() -> usize {
    std::env::var("SMELT_TRANSCRIPT_HOT_PATH_ITEM_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

fn hot_path_heterogeneous() -> bool {
    matches!(
        std::env::var("SMELT_TRANSCRIPT_HOT_PATH_HETEROGENEOUS").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn hot_path_operation_filter() -> Option<std::collections::HashSet<String>> {
    let value = std::env::var("SMELT_TRANSCRIPT_HOT_PATH_OPERATIONS").ok()?;
    let operations = value
        .split(',')
        .map(str::trim)
        .filter(|operation| !operation.is_empty())
        .map(str::to_string)
        .collect::<std::collections::HashSet<_>>();
    assert!(!operations.is_empty(), "hot-path operation filter is empty");
    for operation in &operations {
        assert!(
            HOT_PATH_OPERATIONS.contains(&operation.as_str()),
            "unknown hot-path operation {operation:?}; expected one of {}",
            HOT_PATH_OPERATIONS.join(",")
        );
    }
    Some(operations)
}

fn hot_path_fixture_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("SMELT_TRANSCRIPT_HOT_PATH_FIXTURE")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

fn hot_path_fixture_session_id() -> String {
    let session_id = std::env::var("SMELT_TRANSCRIPT_HOT_PATH_SESSION_ID")
        .expect("SMELT_TRANSCRIPT_HOT_PATH_SESSION_ID is required with a fixture");
    smelt_core::session_id::SessionId::parse(&session_id)
        .expect("SMELT_TRANSCRIPT_HOT_PATH_SESSION_ID must be a session ID");
    session_id
}

fn hot_path_session_id(_label: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:032x}{:016x}{counter:016x}", std::process::id())
}

fn hot_path_user(text: &str) -> protocol::HistoryItem {
    protocol::HistoryItem::user(protocol::Content::text(text))
}

fn hot_path_assistant(text: &str) -> protocol::HistoryItem {
    protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
        Some(protocol::Content::text(text)),
        None,
        Vec::new(),
    ))
}

fn hot_path_history_item(idx: usize, target_bytes: usize) -> protocol::HistoryItem {
    let prefix = if idx.is_multiple_of(2) {
        format!("hot path old user {idx}")
    } else {
        format!("hot path old assistant {idx}")
    };
    let mut text = String::with_capacity(prefix.len().max(target_bytes));
    text.push_str(&prefix);
    text.extend(std::iter::repeat_n(
        'x',
        target_bytes.saturating_sub(prefix.len()),
    ));
    if idx.is_multiple_of(2) {
        hot_path_user(&text)
    } else {
        hot_path_assistant(&text)
    }
}

fn heterogeneous_hot_path_history_item(idx: usize, target_bytes: usize) -> protocol::HistoryItem {
    match idx % 4 {
        0 => hot_path_history_item(idx, target_bytes),
        1 => protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text(format!(
                "heterogeneous assistant markdown {idx}\n\n- item one\n- item two\n\n```rust\nfn row_{idx}() {{}}\n```"
            ))),
            Some(format!("reasoning for heterogeneous row {idx}")),
            Vec::new(),
        )),
        2 => protocol::HistoryItem::Assistant(protocol::AssistantStep::with_invocations(
            Some(protocol::Content::text(format!("tool completed for row {idx}"))),
            None,
            Vec::new(),
            vec![protocol::ToolInvocation {
                call_id: format!("heterogeneous-call-{idx}"),
                name: "heterogeneous_tool".into(),
                arguments: format!(r#"{{"row":{idx}}}"#),
                result: protocol::ToolOutcome::new(
                    format!("tool output for row {idx}\n{}", "output ".repeat(256)),
                    false,
                    Some(serde_json::json!({
                        "row": idx,
                        "payload": "metadata ".repeat(2 * 1024),
                        "paths": ["src/main.rs", "Cargo.toml"],
                    })),
                ),
                elapsed_ms: Some(idx as u64),
                called_at_ms: None,
            }],
        )),
        _ => protocol::HistoryItem::note(protocol::HistoryNote::process_status(format!(
            "background process {} finished successfully",
            idx / 4
        ))),
    }
}

fn saved_hot_path_app(
    label: &str,
    history_len: usize,
    checkpoint_first_live: Option<usize>,
) -> TestApp {
    let mut app = TestApp::builder().build();
    let mut session =
        smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
    session.id = hot_path_session_id(label);
    session.first_user_message = Some("hot path old user 0".into());
    let item_bytes = hot_path_history_item_bytes();
    let item = if hot_path_heterogeneous() {
        heterogeneous_hot_path_history_item
    } else {
        hot_path_history_item
    };
    session.history = (0..history_len).map(|idx| item(idx, item_bytes)).collect();
    if let Some(first_live_index) = checkpoint_first_live {
        session.checkpoint = Some(smelt_core::ContextCheckpoint {
            kind: "benchmark".into(),
            summary: "checkpointed benchmark prefix".into(),
            first_live_index: first_live_index.min(history_len),
            created_at_ms: smelt_core::session::now_ms(),
            tokens_before: Some(10_000),
            tokens_after_estimate: Some(1_000),
            tokens_after_estimate_history_len: Some(history_len),
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        });
    }

    app.app.load_session(session);
    app.app.restore_screen();
    let receipt = save_bench_fixture(&mut app, "hot path");
    wait_for_bench_catalog(&app, "hot path", &receipt);
    app
}

fn copied_hot_path_fixture_app(fixture: &std::path::Path) -> TestApp {
    assert!(
        fixture.is_dir(),
        "hot-path fixture sessions root does not exist: {}",
        fixture.display()
    );
    let session_id = hot_path_fixture_session_id();
    let mut app = TestApp::builder().build();
    let destination_root = app.app.core.sessions.sessions_dir();
    let started = std::time::Instant::now();
    let copied_bytes = copy_fixture_tree(fixture, &destination_root);
    eprintln!(
        "TRANSCRIPT_HOT_PATH_FIXTURE_COPY session_id={} bytes={} ms={:.3}",
        session_id,
        copied_bytes,
        elapsed_ms(started.elapsed())
    );
    app.resume_session(&session_id);
    assert_eq!(app.app.conversation.session().id, session_id);
    assert!(
        !app.app.conversation.is_read_only(),
        "copied hot-path fixture opened read-only"
    );
    app
}

fn copy_fixture_tree(source: &std::path::Path, destination: &std::path::Path) -> u64 {
    std::fs::create_dir_all(destination).expect("create fixture destination");
    let mut copied_bytes = 0_u64;
    for entry in std::fs::read_dir(source).expect("read fixture directory") {
        let entry = entry.expect("read fixture entry");
        let file_type = entry.file_type().expect("read fixture entry type");
        assert!(!file_type.is_symlink(), "fixture must not contain symlinks");
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copied_bytes = copied_bytes.saturating_add(copy_fixture_tree(&entry.path(), &target));
        } else if file_type.is_file() {
            copied_bytes = copied_bytes.saturating_add(
                std::fs::copy(entry.path(), target).expect("copy fixture database file"),
            );
        }
    }
    copied_bytes
}

fn assert_no_full_store_hot_path_reads(snapshot: &smelt_perf::perf::Snapshot, operation: &str) {
    for metric in [
        "store:history:read_all",
        "store:session:load_full_snapshot",
        "store:transcript:read_records_full",
    ] {
        let count = perf_duration_count(snapshot, metric);
        assert_eq!(
            count, 0,
            "{operation} recorded {metric} {count} times, expected no full-store hot-path work"
        );
    }
    for metric in [
        "store:history:read_all_rows",
        "store:session:full_snapshot_rows_read",
        "store:transcript:records_full_loaded",
        "transcript:build_from_session:history_items",
    ] {
        let value = perf_value_max(snapshot, metric);
        assert_eq!(
            value, 0,
            "{operation} recorded {metric}={value}, expected no full-store hot-path work"
        );
    }
}

fn assert_no_full_hot_path_reads(snapshot: &smelt_perf::perf::Snapshot, operation: &str) {
    assert_no_full_store_hot_path_reads(snapshot, operation);
    let fingerprint_count =
        perf_duration_count(snapshot, "transcript:transcript_scene:fingerprint");
    assert_eq!(
        fingerprint_count, 0,
        "{operation} rebuilt {fingerprint_count} full transcript transcript-scene fingerprints"
    );
}

fn assert_hot_path_at_most(
    snapshot: &smelt_perf::perf::Snapshot,
    operation: &str,
    metric: &str,
    max: u64,
) {
    let value = perf_value_max(snapshot, metric);
    assert!(
        value <= max,
        "{operation} recorded {metric}={value}, expected <= {max}"
    );
}

fn assert_cached_persist_db(snapshot: &smelt_perf::perf::Snapshot, operation: &str) {
    let open_us = perf_duration_max(snapshot, "store:lineage:open_read_write");
    assert_eq!(
        open_us, 0,
        "{operation} reopened the session database in {open_us}us instead of reusing the persist worker connection"
    );
    assert_eq!(
        perf_value_max(snapshot, "store:lineage:cached_read_write"),
        1,
        "{operation} did not reuse the persist worker database connection"
    );
}

fn capture_hot_path_sample(
    operation: &'static str,
    history_len: usize,
    body: impl FnOnce(),
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let process_before = smelt_perf::alloc::snapshot();
    let (allocs_before, bytes_before) = smelt_perf::alloc::thread_snapshot();
    let start = std::time::Instant::now();
    body();
    let ms = elapsed_ms(start.elapsed());
    let (allocs_after, bytes_after) = smelt_perf::alloc::thread_snapshot();
    let process_after = smelt_perf::alloc::snapshot();
    let process_delta = smelt_perf::alloc::delta(process_before, process_after);
    let snapshot = smelt_perf::perf::snapshot();
    smelt_perf::perf::set_enabled(false);
    let sample = HotPathSample {
        operation,
        history_len,
        history_item_bytes: hot_path_history_item_bytes(),
        ms,
        queue_wait_ms: perf_value_max(&snapshot, "persist:submit_turn:queue_wait_ms"),
        submit_turn_us: perf_duration_max(&snapshot, "persist:submit_turn"),
        transaction_commit_us: perf_duration_max(&snapshot, "store:lineage:transaction_commit"),
        begin_turn_us: perf_duration_max(&snapshot, "agent:begin_turn"),
        project_context_us: perf_duration_max(&snapshot, "agent:project_context"),
        thread_allocs: allocs_after.saturating_sub(allocs_before),
        thread_bytes_allocated: bytes_after.saturating_sub(bytes_before),
        process_bytes_allocated: process_delta.bytes_allocated,
        process_bytes_deallocated: process_delta.bytes_deallocated,
        process_current_bytes_before: process_before.current_bytes,
        process_current_bytes_after: process_after.current_bytes,
        process_retained_bytes: process_after.current_bytes as i64
            - process_before.current_bytes as i64,
        counters: HotPathCounters::from(&snapshot),
    };
    (sample, snapshot)
}

fn read_provider_history_source(
    source: protocol::ModelHistorySource,
    sessions_root: &std::path::Path,
    session_id: &str,
) -> Vec<protocol::HistoryItem> {
    match source {
        protocol::ModelHistorySource::Items { items, .. } => items,
        protocol::ModelHistorySource::Store {
            prefix,
            lineage_id,
            first_live_index,
            end_index,
            suffix,
            ..
        } => {
            smelt_perf::perf::record_value("engine:model_history:source_store", 1);
            smelt_perf::perf::record_value(
                "engine:model_history:first_live_index",
                first_live_index as u64,
            );
            smelt_perf::perf::record_value("engine:model_history:end_index", end_index as u64);
            smelt_perf::perf::record_value(
                "engine:model_history:suffix_items",
                suffix.len() as u64,
            );
            let mut history = prefix;
            if end_index > first_live_index {
                let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
                    sessions_root,
                    lineage_id,
                    session_id,
                )
                .expect("open provider history lineage");
                let mut rows = reader
                    .history_range(first_live_index as u64, end_index as u64)
                    .expect("read provider history rows");
                smelt_perf::perf::record_value("engine:model_history:rows_read", rows.len() as u64);
                history.append(&mut rows);
            }
            history.extend(suffix);
            smelt_perf::perf::record_value("engine:model_history:items", history.len() as u64);
            history
        }
    }
}

fn run_noop_save_hot_path(history_len: usize) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("noop-save", history_len, None);
    let (sample, snapshot) = capture_hot_path_sample("noop_save", history_len, || {
        app.app.save_session();
        app.app.flush_persist();
    });
    assert_eq!(
        perf_value_max(&snapshot, "session:save:skipped_unchanged"),
        1,
        "no-op save did not take the unchanged fast path"
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:transcript:dirty_record_suffix_rows",
        0,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_request_append_hot_path(history_len: usize) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("request-append", history_len, None);
    let (sample, snapshot) = capture_hot_path_sample("request_append", history_len, || {
        let source = app.app.commit_request_history_item(
            hot_path_user("hot path new user"),
            Some(smelt_core::Block::User {
                text: "hot path new user".into(),
                image_labels: vec![],
                command: false,
                sent_at_ms: None,
            }),
        );
        assert!(matches!(
            source,
            protocol::ModelHistorySource::Store { end_index, .. } if end_index == history_len
        ));
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:history_rows_inserted",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:transcript:dirty_record_suffix_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:transcript:record_db_rows_inserted",
        1,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    assert_cached_persist_db(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_history_appended_hot_path(
    history_len: usize,
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("history-appended", history_len, None);
    app.start_turn(1);
    let item = hot_path_assistant("hot path assistant update");
    let (sample, snapshot) = capture_hot_path_sample("history_appended", history_len, || {
        app.app
            .dispatch_engine_event(protocol::EngineEvent::HistoryAppended {
                turn_id: 1,
                delta: protocol::CanonicalHistoryDelta::new(history_len, vec![item]),
            });
        app.app.flush_persist();
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:history_rows_inserted",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "tui:set_history:dirty_prefix_compared",
        0,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    assert_cached_persist_db(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_turn_complete_hot_path(history_len: usize) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("turn-complete", history_len, None);
    app.start_turn(1);
    let meta = protocol::TurnMeta {
        elapsed_ms: 1,
        avg_tps: None,
        display_tps: None,
        interrupted: false,
    };
    let (sample, snapshot) = capture_hot_path_sample("turn_complete", history_len, || {
        app.app
            .dispatch_engine_event(protocol::EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: Some(meta),
            });
        app.app.flush_persist();
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "tui:set_history:dirty_prefix_compared",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "tui:set_history:dirty_from_hint",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "lua:session:conversation_rows_scanned",
        16,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "lua:session:conversation_rows_returned",
        16,
    );
    assert!(
        app.app.conversation.has_document_work(),
        "{} did not defer completion metadata for the next save point",
        sample.operation
    );
    assert_eq!(
        perf_duration_max(&snapshot, "session:save"),
        0,
        "{} saved low-priority completion metadata in the engine-event hot path",
        sample.operation
    );
    assert_eq!(
        perf_duration_max(&snapshot, "persist:write_metadata"),
        0,
        "{} wrote low-priority completion metadata in the engine-event hot path",
        sample.operation
    );
    assert_eq!(
        perf_duration_max(&snapshot, "store:lineage:transaction_commit"),
        0,
        "{} committed low-priority completion metadata in the engine-event hot path",
        sample.operation
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_rewind_delete_hot_path(history_len: usize) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("rewind-delete", history_len, None);
    let transcript = app.app.conversation.transcript().history();
    let rewind_history_idx = transcript
        .order
        .iter()
        .rev()
        .find_map(|block_id| {
            (transcript.block_kind(*block_id) == Some("user"))
                .then(|| transcript.block_origin(*block_id))
                .flatten()
                .and_then(|origin| match origin {
                    smelt_core::BlockOrigin::History(history_idx) => Some(history_idx),
                    smelt_core::BlockOrigin::Checkpoint { .. } => None,
                })
        })
        .expect("rewind benchmark history requires a user block");
    let expected_deleted = history_len.saturating_sub(rewind_history_idx) as u64;
    let (sample, snapshot) = capture_hot_path_sample("rewind_delete_suffix", history_len, || {
        let rewound = app.app.rewind_to_history(rewind_history_idx);
        assert!(
            rewound.is_some(),
            "rewind benchmark target must be a user block"
        );
        app.app.save_session();
        app.app.flush_persist();
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:history_rows_deleted",
        expected_deleted,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    assert_cached_persist_db(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_provider_history_hot_path(
    history_len: usize,
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let first_live = history_len.saturating_sub(32);
    let app = saved_hot_path_app("provider-history", history_len, Some(first_live));
    let source = app.app.model_history_source();
    let sessions_root = app.app.core.sessions.sessions_dir();
    let session_id = app.app.conversation.session().id.clone();
    let (sample, snapshot) = capture_hot_path_sample("provider_history_read", history_len, || {
        let history = read_provider_history_source(source, &sessions_root, &session_id);
        assert_eq!(history.len(), history_len - first_live + 1);
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:read_range_rows",
        (history_len - first_live) as u64,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "engine:model_history:rows_read",
        (history_len - first_live) as u64,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_uncheckpointed_provider_history_hot_path(
    history_len: usize,
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let app = saved_hot_path_app("provider-history-uncheckpointed", history_len, None);
    let source = app.app.model_history_source();
    let sessions_root = app.app.core.sessions.sessions_dir();
    let session_id = app.app.conversation.session().id.clone();
    let (sample, snapshot) =
        capture_hot_path_sample("provider_history_uncheckpointed_read", history_len, || {
            let history = read_provider_history_source(source, &sessions_root, &session_id);
            assert_eq!(history.len(), history_len);
        });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:read_range_rows",
        history_len as u64,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "engine:model_history:rows_read",
        history_len as u64,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_engine_request_materialization_hot_path(
    history_len: usize,
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let app = saved_hot_path_app("engine-request-materialization", history_len, None);
    let source = app.app.model_history_source();
    let sessions_root = app.app.core.sessions.sessions_dir();
    let session_id = app.app.conversation.session().id.clone();
    let (sample, snapshot) =
        capture_hot_path_sample("engine_request_materialization", history_len, || {
            let history = read_provider_history_source(source, &sessions_root, &session_id);
            let mut engine_history = {
                let _perf = smelt_perf::perf::begin("bench:engine_request:install_history");
                let mut installed = Vec::with_capacity(history.len() + 1);
                installed.push(protocol::HistoryItem::system("benchmark system prompt"));
                installed.extend(history);
                installed
            };
            assert_eq!(engine_history.len(), history_len + 1);

            let prepared = {
                let _perf = smelt_perf::perf::begin("bench:engine_request:prepare_messages");
                engine::PreparedRequestMessages::new(
                    protocol::history_to_messages(&engine_history),
                    1,
                )
            };
            {
                let _perf =
                    smelt_perf::perf::begin("bench:engine_request:estimate_tokens_serialize");
                serde_json::to_writer(std::io::sink(), prepared.model())
                    .expect("measure serialized request view");
            }
            smelt_perf::perf::record_value(
                "bench:engine_request:prepare_message_count",
                prepared.model().len() as u64,
            );
            smelt_perf::perf::record_value(
                "bench:engine_request:provider_message_count",
                prepared.wire().len() as u64,
            );
            std::hint::black_box((&mut engine_history, prepared));
        });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:read_range_rows",
        history_len as u64,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_submit_hot_paths(
    configured_history_len: usize,
    fixture: Option<&std::path::Path>,
    include_submit: bool,
    include_render: bool,
) -> Vec<(HotPathSample, smelt_perf::perf::Snapshot)> {
    let mut app = fixture.map_or_else(
        || saved_hot_path_app("submit", configured_history_len, None),
        copied_hot_path_fixture_app,
    );
    let history_len = app.app.session_history_len();
    app.app.handle_resize(100, 32);
    app.render_silent();
    app.type_text("hot path submitted user message");
    app.clear_actions();

    let (submit, submit_snapshot) = capture_hot_path_sample("submit_enter", history_len, || {
        app.press(KeyCode::Enter);
    });
    let started = app.actions().iter().find_map(|action| match action {
        Action::EngineSend(command) => match command.as_ref() {
            protocol::UiCommand::StartTurn(payload) => Some(payload),
            _ => None,
        },
        _ => None,
    });
    let Some(started) = started else {
        print_hot_path_perf(submit.operation, &submit_snapshot);
        let notification = app
            .app
            .overlays
            .notification()
            .map(|notification| notification.summary.as_str());
        panic!(
            "submit_enter did not dispatch StartTurn; notification={notification:?}; actions={:?}",
            app.actions()
        );
    };
    assert!(matches!(
        &started.history,
        protocol::ModelHistorySource::Store { .. }
    ));
    // Enter may append the current session context, a named-context tombstone,
    // and the submitted user item, but it must never rewrite persisted history.
    let max_dirty_history_rows = if fixture.is_some() { 3 } else { 2 };
    assert_hot_path_at_most(
        &submit_snapshot,
        submit.operation,
        "store:history:dirty_suffix_rows",
        max_dirty_history_rows,
    );
    assert_hot_path_at_most(
        &submit_snapshot,
        submit.operation,
        "store:session:history_rows_deleted",
        0,
    );
    assert_hot_path_at_most(
        &submit_snapshot,
        submit.operation,
        "store:transcript:dirty_record_suffix_rows",
        1,
    );
    assert_eq!(
        perf_value_max(&submit_snapshot, "persist:submit_turn:transactions"),
        1,
        "submit_enter did not use exactly one canonical SubmitTurn transaction"
    );
    assert_hot_path_at_most(
        &submit_snapshot,
        submit.operation,
        "store:session:invariant_history_rows",
        0,
    );
    assert_hot_path_at_most(
        &submit_snapshot,
        submit.operation,
        "transcript:record_record_index:entries_scanned",
        2,
    );
    assert_cached_persist_db(&submit_snapshot, submit.operation);

    let mut samples = Vec::with_capacity(2);
    if include_submit {
        samples.push((submit, submit_snapshot));
    }
    if include_render {
        let (render, render_snapshot) =
            capture_hot_path_sample("submit_first_render", history_len, || {
                app.render_silent();
            });
        assert_no_full_store_hot_path_reads(&render_snapshot, render.operation);
        samples.push((render, render_snapshot));
    }
    samples
}

fn print_hot_path_perf(operation: &str, snapshot: &smelt_perf::perf::Snapshot) {
    let included = |label: &str| {
        [
            "agent:",
            "app:",
            "session:",
            "persist:",
            "store:",
            "engine:",
            "provider:",
            "transcript:",
            "tui:",
            "lua:",
            "bench:",
        ]
        .iter()
        .any(|prefix| label.starts_with(prefix))
    };
    for row in snapshot.durations.iter().filter(|row| included(row.label)) {
        eprintln!(
            "TRANSCRIPT_HOT_PATH_PERF_DURATION operation={} metric={} count={} last_us={} total_us={} p95_us={} max_us={}",
            operation, row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
        );
    }
    for row in snapshot.values.iter().filter(|row| included(row.label)) {
        eprintln!(
            "TRANSCRIPT_HOT_PATH_PERF_VALUE operation={} metric={} count={} last={} total={} p95={} max={}",
            operation, row.label, row.count, row.last, row.total, row.p95, row.max
        );
    }
    for row in snapshot.allocs.iter().filter(|row| included(row.label)) {
        eprintln!(
            "TRANSCRIPT_HOT_PATH_PERF_ALLOC operation={} metric={} count={} allocs_last={} allocs_total={} bytes_last={} bytes_total={} bytes_p95={} bytes_max={}",
            operation,
            row.label,
            row.count,
            row.allocs_last,
            row.allocs_total,
            row.bytes_last,
            row.bytes_total,
            row.bytes_p95,
            row.bytes_max
        );
    }
}

fn print_hot_path_sample(run: usize, sample: &HotPathSample) {
    let c = sample.counters;
    eprintln!(
        "TRANSCRIPT_HOT_PATH_BENCH_SAMPLE run={} operation={} history_len={} history_item_bytes={} ms={:.3} queue_wait_ms={} submit_turn_us={} transaction_commit_us={} begin_turn_us={} project_context_us={} thread_allocs={} thread_bytes_allocated={} process_bytes_allocated={} process_bytes_deallocated={} process_current_bytes_before={} process_current_bytes_after={} process_retained_bytes={} history_suffix_rows={} history_inserted={} history_deleted={} record_suffix_rows={} record_inserted={} record_deleted={} read_range_rows={} cached_read_write_db={} invariant_history_rows={} search_blob_rows={} search_blob_bytes={} user_turn_blocks_scanned={} user_turns_cloned={} user_turn_text_bytes_cloned={}",
        run,
        sample.operation,
        sample.history_len,
        sample.history_item_bytes,
        sample.ms,
        sample.queue_wait_ms,
        sample.submit_turn_us,
        sample.transaction_commit_us,
        sample.begin_turn_us,
        sample.project_context_us,
        sample.thread_allocs,
        sample.thread_bytes_allocated,
        sample.process_bytes_allocated,
        sample.process_bytes_deallocated,
        sample.process_current_bytes_before,
        sample.process_current_bytes_after,
        sample.process_retained_bytes,
        c.history_suffix_rows,
        c.history_inserted,
        c.history_deleted,
        c.record_suffix_rows,
        c.record_inserted,
        c.record_deleted,
        c.read_range_rows,
        c.cached_read_write_db,
        c.invariant_history_rows,
        c.search_blob_rows,
        c.search_blob_bytes,
        c.user_turn_blocks_scanned,
        c.user_turns_cloned,
        c.user_turn_text_bytes_cloned,
    );
    eprintln!(
        "TRANSCRIPT_HOT_PATH_BENCH_JSON {{\"type\":\"hot_path_sample\",\"run\":{},\"operation\":\"{}\",\"history_len\":{},\"history_item_bytes\":{},\"ms\":{:.3},\"queue_wait_ms\":{},\"submit_turn_us\":{},\"transaction_commit_us\":{},\"begin_turn_us\":{},\"project_context_us\":{},\"thread_allocs\":{},\"thread_bytes_allocated\":{},\"process_bytes_allocated\":{},\"process_bytes_deallocated\":{},\"process_current_bytes_before\":{},\"process_current_bytes_after\":{},\"process_retained_bytes\":{},\"history_suffix_rows\":{},\"history_inserted\":{},\"history_deleted\":{},\"record_suffix_rows\":{},\"record_inserted\":{},\"record_deleted\":{},\"read_range_rows\":{},\"cached_read_write_db\":{},\"invariant_history_rows\":{},\"search_blob_rows\":{},\"search_blob_bytes\":{},\"user_turn_blocks_scanned\":{},\"user_turns_cloned\":{},\"user_turn_text_bytes_cloned\":{}}}",
        run,
        sample.operation,
        sample.history_len,
        sample.history_item_bytes,
        sample.ms,
        sample.queue_wait_ms,
        sample.submit_turn_us,
        sample.transaction_commit_us,
        sample.begin_turn_us,
        sample.project_context_us,
        sample.thread_allocs,
        sample.thread_bytes_allocated,
        sample.process_bytes_allocated,
        sample.process_bytes_deallocated,
        sample.process_current_bytes_before,
        sample.process_current_bytes_after,
        sample.process_retained_bytes,
        c.history_suffix_rows,
        c.history_inserted,
        c.history_deleted,
        c.record_suffix_rows,
        c.record_inserted,
        c.record_deleted,
        c.read_range_rows,
        c.cached_read_write_db,
        c.invariant_history_rows,
        c.search_blob_rows,
        c.search_blob_bytes,
        c.user_turn_blocks_scanned,
        c.user_turns_cloned,
        c.user_turn_text_bytes_cloned,
    );
}

#[test]
fn transcript_layout_hot_path_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    if !hot_path_enabled() {
        eprintln!("TRANSCRIPT_HOT_PATH_BENCH_SKIPPED set SMELT_TRANSCRIPT_HOT_PATH=1 to run");
        return;
    }

    let runs = navigation_bench_runs();
    let history_len = hot_path_history_len();
    let fixture = hot_path_fixture_dir();
    eprintln!(
        "TRANSCRIPT_HOT_PATH_BENCH_CONFIG runs={} history_len={} history_item_bytes={} heterogeneous={} fixture={}",
        runs,
        history_len,
        hot_path_history_item_bytes(),
        hot_path_heterogeneous(),
        fixture.as_ref().map_or_else(
            || "none".to_string(),
            |path| path.display().to_string()
        )
    );
    let operation_filter = hot_path_operation_filter().unwrap_or_else(|| {
        if fixture.is_some() {
            [
                "submit_enter".to_string(),
                "submit_first_render".to_string(),
            ]
            .into_iter()
            .collect()
        } else {
            HOT_PATH_OPERATIONS
                .iter()
                .map(|operation| (*operation).to_string())
                .collect()
        }
    });
    if fixture.is_some() {
        for operation in &operation_filter {
            assert!(
                matches!(operation.as_str(), "submit_enter" | "submit_first_render"),
                "real-session fixtures support only submit_enter and submit_first_render"
            );
        }
    }
    let enabled = |operation: &str| operation_filter.contains(operation);

    let mut samples = Vec::new();
    for run in 1..=runs {
        let mut run_samples = Vec::new();
        if enabled("noop_save") {
            run_samples.push(run_noop_save_hot_path(history_len));
        }
        if enabled("request_append") {
            run_samples.push(run_request_append_hot_path(history_len));
        }
        if enabled("history_appended") {
            run_samples.push(run_history_appended_hot_path(history_len));
        }
        if enabled("turn_complete") {
            run_samples.push(run_turn_complete_hot_path(history_len));
        }
        if enabled("rewind_delete_suffix") {
            run_samples.push(run_rewind_delete_hot_path(history_len));
        }
        if enabled("provider_history_read") {
            run_samples.push(run_provider_history_hot_path(history_len));
        }
        if enabled("provider_history_uncheckpointed_read") {
            run_samples.push(run_uncheckpointed_provider_history_hot_path(history_len));
        }
        if enabled("engine_request_materialization") {
            run_samples.push(run_engine_request_materialization_hot_path(history_len));
        }
        let include_submit = enabled("submit_enter");
        let include_render = enabled("submit_first_render");
        if include_submit || include_render {
            run_samples.extend(run_submit_hot_paths(
                history_len,
                fixture.as_deref(),
                include_submit,
                include_render,
            ));
        }
        for (sample, snapshot) in run_samples {
            print_hot_path_perf(sample.operation, &snapshot);
            print_hot_path_sample(run, &sample);
            samples.push(sample);
        }
    }

    for operation in HOT_PATH_OPERATIONS {
        let operation_samples = samples
            .iter()
            .filter(|sample| sample.operation == *operation)
            .map(|sample| sample.ms)
            .collect::<Vec<_>>();
        if operation_samples.is_empty() {
            continue;
        }
        let stats = TailStats::from(&operation_samples);
        let sample_history_len = samples
            .iter()
            .find(|sample| sample.operation == *operation)
            .expect("operation has samples")
            .history_len;
        eprintln!(
            "TRANSCRIPT_HOT_PATH_BENCH_SUMMARY operation={} runs={} history_len={} history_item_bytes={} mean_ms={:.3} stddev_ms={:.3} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3}",
            operation,
            operation_samples.len(),
            sample_history_len,
            hot_path_history_item_bytes(),
            stats.mean,
            stats.stddev,
            stats.p50,
            stats.p95,
            stats.p99,
            stats.max,
        );
        eprintln!(
            "TRANSCRIPT_HOT_PATH_BENCH_SUMMARY_JSON {{\"type\":\"hot_path_summary\",\"operation\":\"{}\",\"runs\":{},\"history_len\":{},\"history_item_bytes\":{},\"mean_ms\":{:.3},\"stddev_ms\":{:.3},\"p50_ms\":{:.3},\"p95_ms\":{:.3},\"p99_ms\":{:.3},\"max_ms\":{:.3}}}",
            operation,
            operation_samples.len(),
            sample_history_len,
            hot_path_history_item_bytes(),
            stats.mean,
            stats.stddev,
            stats.p50,
            stats.p95,
            stats.p99,
            stats.max,
        );
    }
}

fn active_memory_bench_bytes() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_ACTIVE_MEMORY_BYTES", 50 * 1024 * 1024)
}

fn linux_memory_bytes(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let value_kib = status
        .lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_ascii_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(value_kib.saturating_mul(1024))
}

#[test]
fn transcript_active_memory_benchmark_suite() {
    if !benchmark_target_enabled() {
        return;
    }
    const BLOCK_BYTES: usize = 32 * 1024;
    const SAVE_BATCH_BLOCKS: usize = 256;
    const SEARCH_TARGET: &str = "active-memory-unique-search-target";

    let target_bytes = active_memory_bench_bytes();
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let process_start = smelt_perf::alloc::snapshot();
    let started_at = std::time::Instant::now();
    let body = "active transcript canonical payload with markdown wrapping and exact hydration. "
        .repeat(BLOCK_BYTES / 72);
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let mut generated_bytes = 0usize;
    let mut block_count = 0usize;
    let mut batch_count = 0usize;
    let mut marker_written = false;

    loop {
        let batch_end = block_count.saturating_add(SAVE_BATCH_BLOCKS);
        while generated_bytes < target_bytes && block_count < batch_end {
            let marker = if !marker_written && generated_bytes >= target_bytes / 2 {
                marker_written = true;
                SEARCH_TARGET
            } else {
                "ordinary-active-memory-block"
            };
            let content = format!("# Active block {block_count}\n\n{marker}\n\n{body}");
            generated_bytes = generated_bytes.saturating_add(content.len());
            app.app
                .push_block(smelt_core::transcript_model::Block::Text {
                    content: content.into(),
                });
            block_count += 1;
        }
        let receipt = save_bench_fixture(&mut app, "active memory");
        while app.app.conversation.drain_transcript_compaction_slice() {}
        batch_count += 1;
        if generated_bytes >= target_bytes {
            wait_for_bench_catalog(&app, "active memory", &receipt);
            wait_for_bench_search_projection(&app, "active memory");
            break;
        }
    }
    assert!(marker_written);
    let seeded_ms = elapsed_ms(started_at.elapsed());
    let after_compaction = app
        .app
        .conversation
        .transcript_memory_snapshot_for_harness();
    assert_eq!(after_compaction.live_blocks, 0);
    assert_eq!(after_compaction.stored_blocks, block_count);
    assert_eq!(after_compaction.hydrated_blocks, 0);
    assert_eq!(
        after_compaction.dematerialized_entries as usize,
        block_count
    );

    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    app.app.transcript_win_mut().set_vim_enabled(true);
    app.app.transcript_win_mut().set_vim_mode(VimMode::Normal);
    let render_started_at = std::time::Instant::now();
    app.render_silent();
    let first_render_ms = elapsed_ms(render_started_at.elapsed());

    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let scroll_started_at = std::time::Instant::now();
    for _ in 0..20 {
        app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL);
        app.render_silent();
    }
    let scroll_20_ms = elapsed_ms(scroll_started_at.elapsed());

    smelt_perf::perf::clear();
    let search_started_at = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        SEARCH_TARGET.to_string(),
    );
    app.render_silent();
    let search_ms = elapsed_ms(search_started_at.elapsed());
    let search_snapshot = smelt_perf::perf::snapshot();
    assert_search_uses_candidate_index(&search_snapshot, "active memory", 1);
    let next_started_at = std::time::Instant::now();
    app.type_char('n');
    app.render_silent();
    let next_ms = elapsed_ms(next_started_at.elapsed());

    let churn_ids = app
        .app
        .conversation
        .transcript()
        .history()
        .order
        .iter()
        .copied()
        .take(1_200)
        .collect::<Vec<_>>();
    let churn_started_at = std::time::Instant::now();
    assert!(app.hydrate_transcript_blocks(&churn_ids));
    let hydration_churn_ms = elapsed_ms(churn_started_at.elapsed());
    let newest_hydrated_id = churn_ids
        .iter()
        .rev()
        .copied()
        .find(|id| {
            app.app
                .conversation
                .transcript()
                .history()
                .is_materialized(*id)
        })
        .expect("hydration churn should leave a bounded working set");
    let reads_before_reuse = app
        .app
        .conversation
        .transcript()
        .memory_snapshot()
        .hydration_reads;
    assert!(app.hydrate_transcript_blocks(&[newest_hydrated_id]));
    let working_set_rereads = app
        .app
        .conversation
        .transcript()
        .memory_snapshot()
        .hydration_reads
        .saturating_sub(reads_before_reuse);
    assert_eq!(working_set_rereads, 0);

    let memory = app
        .app
        .conversation
        .transcript_memory_snapshot_for_harness();
    let process_end = smelt_perf::alloc::snapshot();
    let process_delta = smelt_perf::alloc::delta(process_start, process_end);
    let hydrated_bytes = memory
        .hydrated_block_bytes
        .saturating_add(memory.hydrated_tool_state_bytes);
    let rendered_bytes = memory
        .layout_bytes
        .saturating_add(memory.height_index_bytes)
        .saturating_add(memory.height_index_cache_bytes)
        .saturating_add(memory.visible_rows_bytes)
        .saturating_add(memory.full_rows_bytes);
    assert_eq!(memory.live_block_bytes, 0);
    assert_eq!(memory.live_tool_state_bytes, 0);
    assert!(
        hydrated_bytes
            <= memory
                .hydrated_budget_bytes
                .saturating_add(memory.pinned_hydrated_bytes)
                .saturating_add(memory.hydrated_oversize_debt_bytes),
        "hydrated cache exceeded budget plus pins and oversize debt: {memory:?}"
    );
    assert!(
        memory.record_window_bytes
            <= memory
                .record_budget_bytes
                .saturating_add(memory.record_oversize_debt_bytes),
        "record cache exceeded its measured bound: {memory:?}"
    );
    assert!(
        rendered_bytes
            <= memory
                .rendered_budget_bytes
                .saturating_add(memory.rendered_oversize_debt_bytes),
        "render cache exceeded its measured bound: {memory:?}"
    );
    assert!(
        memory.live_block_bytes.saturating_add(hydrated_bytes) < 64 * 1024 * 1024,
        "full block content grew with committed transcript size: {memory:?}"
    );
    if target_bytes >= 50 * 1024 * 1024 {
        assert!(
            memory.evicted_entries > 0,
            "default hydration budget never evicted"
        );
    }
    if target_bytes >= 450 * 1024 * 1024 {
        assert!(
            memory.live_block_bytes.saturating_add(hydrated_bytes) < 50 * 1024 * 1024,
            "500 MiB session retained a transcript-sized full-content copy: {memory:?}"
        );
    }

    let report = serde_json::json!({
        "type": "active_transcript_memory",
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "target_bytes": target_bytes,
        "generated_bytes": generated_bytes,
        "blocks": block_count,
        "save_batches": batch_count,
        "timings_ms": {
            "seed_persist_compact": seeded_ms,
            "first_render": first_render_ms,
            "scroll_20": scroll_20_ms,
            "search": search_ms,
            "next": next_ms,
            "hydration_churn": hydration_churn_ms,
        },
        "block_counts": {
            "live": memory.live_blocks,
            "stored": memory.stored_blocks,
            "hydrated": memory.hydrated_blocks,
        },
        "budgets": {
            "hydrated": memory.hydrated_budget_bytes,
            "record_windows": memory.record_budget_bytes,
            "rendered": memory.rendered_budget_bytes,
        },
        "retained_bytes": {
            "live_blocks": memory.live_block_bytes,
            "live_tool_states": memory.live_tool_state_bytes,
            "hydrated_blocks": memory.hydrated_block_bytes,
            "hydrated_tool_states": memory.hydrated_tool_state_bytes,
            "compact_records": memory.compact_record_bytes,
            "record_windows": memory.record_window_bytes,
            "tool_state_index": memory.tool_state_index_bytes,
            "block_metadata": memory.block_metadata_bytes,
            "layouts": memory.layout_bytes,
            "active_height_index": memory.height_index_bytes,
            "cached_height_indexes": memory.height_index_cache_bytes,
            "visible_rows": memory.visible_rows_bytes,
            "full_rows": memory.full_rows_bytes,
            "rendered_total": rendered_bytes,
        },
        "pins": {
            "hydrated_bytes": memory.pinned_hydrated_bytes,
            "rendered_bytes": memory.pinned_rendered_bytes,
        },
        "oversize_debt_bytes": {
            "hydrated": memory.hydrated_oversize_debt_bytes,
            "record_windows": memory.record_oversize_debt_bytes,
            "rendered": memory.rendered_oversize_debt_bytes,
        },
        "hydration": {
            "reads": memory.hydration_reads,
            "ranges": memory.hydration_ranges,
            "bytes": memory.hydration_bytes,
            "duration_us": memory.hydration_duration_us,
            "working_set_rereads": working_set_rereads,
        },
        "eviction": {
            "entries": memory.evicted_entries,
            "bytes": memory.evicted_bytes,
        },
        "dematerialization": {
            "entries": memory.dematerialized_entries,
            "bytes": memory.dematerialized_bytes,
        },
        "allocator": {
            "current_bytes_start": process_start.current_bytes,
            "current_bytes_end": process_end.current_bytes,
            "retained_delta_bytes": process_end.current_bytes as i64 - process_start.current_bytes as i64,
            "allocated_bytes": process_delta.bytes_allocated,
            "deallocated_bytes": process_delta.bytes_deallocated,
        },
        "process": {
            "rss_bytes": linux_memory_bytes("VmRSS:"),
            "peak_rss_bytes": linux_memory_bytes("VmHWM:"),
        },
    });
    eprintln!("TRANSCRIPT_ACTIVE_MEMORY_JSON {report}");
    smelt_perf::perf::set_enabled(false);
}
