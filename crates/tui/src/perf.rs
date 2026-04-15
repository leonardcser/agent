use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Global per-label samples (self-time durations).
fn samples() -> &'static Mutex<HashMap<&'static str, Vec<Duration>>> {
    static S: OnceLock<Mutex<HashMap<&'static str, Vec<Duration>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Global per-label value samples (arbitrary u64, e.g. byte counts).
fn value_samples() -> &'static Mutex<HashMap<&'static str, Vec<u64>>> {
    static V: OnceLock<Mutex<HashMap<&'static str, Vec<u64>>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a raw numeric sample under `label`. Used for things that
/// aren't durations — byte counts, cache sizes, etc. Printed in the
/// summary alongside the duration table.
pub fn record_value(label: &'static str, value: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut m) = value_samples().lock() {
        m.entry(label).or_default().push(value);
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Enable bench-mode collection. Idempotent.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Returns an RAII guard that records a self-time sample for `label` when
/// dropped. Also records allocation count and bytes delta if the counting
/// allocator is enabled. Cheap no-op if bench mode is off.
pub fn begin(label: &'static str) -> Option<Guard> {
    if !enabled() {
        return None;
    }
    Some(Guard {
        label,
        start: Instant::now(),
        allocs_start: crate::alloc::snapshot(),
    })
}

pub struct Guard {
    label: &'static str,
    start: Instant,
    allocs_start: (u64, u64),
}

impl Drop for Guard {
    fn drop(&mut self) {
        let dur = self.start.elapsed();
        if let Ok(mut s) = samples().lock() {
            s.entry(self.label).or_default().push(dur);
        }
        if crate::alloc::enabled() {
            let (c1, b1) = crate::alloc::snapshot();
            let (c0, b0) = self.allocs_start;
            if let Ok(mut m) = alloc_samples().lock() {
                m.entry(self.label)
                    .or_default()
                    .push((c1.saturating_sub(c0), b1.saturating_sub(b0)));
            }
        }
    }
}

/// Per-label alloc samples: `(count, bytes)` deltas captured by `Guard`.
type AllocSamples = Mutex<HashMap<&'static str, Vec<(u64, u64)>>>;

fn alloc_samples() -> &'static AllocSamples {
    static A: OnceLock<AllocSamples> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── summary printing ─────────────────────────────────────────────────────

// Total visual width of the table:
// 30 (label) + 1 + 8 (calls) + 6 * (1 + 10) = 30 + 1 + 8 + 66 = 105
const TABLE_WIDTH: usize = 105;

/// Print a summary table of all recorded timings to stdout.
pub fn print_summary() {
    if !enabled() {
        return;
    }
    let map = samples().lock().unwrap();
    if map.is_empty() {
        return;
    }
    let mut groups: Vec<(&'static str, Vec<Duration>)> =
        map.iter().map(|(k, v)| (*k, v.clone())).collect();
    drop(map);
    // Sort by total time descending so worst offenders are at the top.
    groups.sort_by(|a, b| {
        let ta: Duration = a.1.iter().sum();
        let tb: Duration = b.1.iter().sum();
        tb.cmp(&ta)
    });
    let max_total: Duration = groups
        .iter()
        .map(|(_, ds)| ds.iter().sum::<Duration>())
        .max()
        .unwrap_or_default();

    let bar = "─".repeat(TABLE_WIDTH);
    let title = "── bench ";
    let title_bar = format!(
        "{}{}",
        title,
        "─".repeat(TABLE_WIDTH - title.chars().count())
    );
    println!("\n{}", title_bar);
    print_header("function", &bar);
    for (label, mut durs) in groups {
        durs.sort();
        let total: Duration = durs.iter().sum();
        let avg = total / durs.len() as u32;
        let row = format_row(label, &durs, total, avg, fmt_dur);
        println!("{}", colorize_row(&row, total, max_total));
    }
    println!("{}", bar);

    // Allocation samples (count + bytes deltas per Guard scope).
    let alloc_map = alloc_samples().lock().unwrap();
    if !alloc_map.is_empty() {
        let mut agroups: Vec<(&'static str, Vec<(u64, u64)>)> =
            alloc_map.iter().map(|(k, v)| (*k, v.clone())).collect();
        drop(alloc_map);
        // Sort by total bytes descending.
        agroups.sort_by(|a, b| {
            let ta: u64 = a.1.iter().map(|(_, b)| *b).sum();
            let tb: u64 = b.1.iter().map(|(_, b)| *b).sum();
            tb.cmp(&ta)
        });
        print_header("allocs", &bar);
        for (label, samples) in agroups {
            let mut counts: Vec<u64> = samples.iter().map(|(c, _)| *c).collect();
            let mut bytes: Vec<u64> = samples.iter().map(|(_, b)| *b).collect();
            counts.sort();
            bytes.sort();
            let total_bytes: u64 = bytes.iter().sum();
            let avg_bytes = total_bytes / bytes.len() as u64;
            let total_count: u64 = counts.iter().sum();
            let avg_count = total_count / counts.len() as u64;
            // Two rows per label: one for count, one for bytes.
            println!(
                "{}",
                format_row(
                    &format!("{label}  (n)"),
                    &counts,
                    total_count,
                    avg_count,
                    |v| v.to_string(),
                )
            );
            println!(
                "{}",
                format_row(
                    &format!("{label}  (bytes)"),
                    &bytes,
                    total_bytes,
                    avg_bytes,
                    fmt_bytes,
                )
            );
        }
        println!("{}", bar);
    } else {
        drop(alloc_map);
    }

    // Value samples (byte counts etc.) in a smaller table below.
    let value_map = value_samples().lock().unwrap();
    if !value_map.is_empty() {
        let mut vgroups: Vec<(&'static str, Vec<u64>)> =
            value_map.iter().map(|(k, v)| (*k, v.clone())).collect();
        drop(value_map);
        vgroups.sort_by_key(|(k, _)| *k);
        print_header("value", &bar);
        for (label, mut vs) in vgroups {
            vs.sort();
            let total: u64 = vs.iter().sum();
            let avg = total / vs.len() as u64;
            println!("{}", format_row(label, &vs, total, avg, fmt_bytes));
        }
        println!("{}", bar);
    }
}

fn print_header(first: &str, bar: &str) {
    println!(
        "{:<30} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        first, "count", "total", "avg", "p50", "p95", "p99", "max"
    );
    println!("{}", bar);
}

/// Format one row of a samples table. `samples` must already be sorted
/// ascending. Generic over the sample value type so both the duration
/// and byte-count tables share the same column layout and percentile
/// math.
fn format_row<T, F>(label: &str, samples: &[T], total: T, avg: T, fmt: F) -> String
where
    T: Copy,
    F: Fn(T) -> String,
{
    let count = samples.len();
    let pct = |p: usize| -> T {
        let idx = ((count * p) / 100).min(count - 1);
        samples[idx]
    };
    let max = *samples.last().unwrap();
    format!(
        "{:<30} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        label,
        count,
        fmt(total),
        fmt(avg),
        fmt(pct(50)),
        fmt(pct(95)),
        fmt(pct(99)),
        fmt(max),
    )
}

/// Colorize a formatted row based on the row's `total` time relative to
/// the worst row's total. Uses a log-scaled gradient from dim → red.
fn colorize_row(row: &str, total: Duration, max_total: Duration) -> String {
    let code = severity_color(total, max_total);
    format!("\x1b[{}m{}\x1b[0m", code, row)
}

/// Map a (total, max_total) pair to an ANSI SGR color code.
/// Log-scaled so that the totals (which span ~5 orders of magnitude in
/// practice) get spread across the gradient instead of all clumping at
/// the bottom.
fn severity_color(total: Duration, max_total: Duration) -> &'static str {
    let t = total.as_secs_f64();
    let m = max_total.as_secs_f64().max(1e-9);
    // Normalize on a log scale: ratio = log(t)/log(m), clamped to [0, 1].
    // Using log(1 + x * 1000) to avoid -inf at zero and to compress.
    let ratio = (1.0 + t * 1000.0).ln() / (1.0 + m * 1000.0).ln();
    let ratio = ratio.clamp(0.0, 1.0);

    match ratio {
        r if r >= 0.85 => "1;91", // bold bright red
        r if r >= 0.65 => "91",   // bright red
        r if r >= 0.45 => "33",   // yellow
        r if r >= 0.25 => "36",   // cyan
        r if r >= 0.10 => "37",   // white
        _ => "2;37",              // dim
    }
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1_000 {
        format!("{}µs", us)
    } else if us < 1_000_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{}B", n)
    } else if n < 1024 * 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{:.2}MB", n as f64 / (1024.0 * 1024.0))
    }
}
