use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static SAMPLES: Mutex<Vec<Sample>> = Mutex::new(Vec::new());

struct Sample {
    label: &'static str,
    dur: Duration,
}

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Returns a guard that records the elapsed time when dropped.
/// If bench mode is disabled, returns None (zero overhead).
pub fn begin(label: &'static str) -> Option<Guard> {
    if !enabled() {
        return None;
    }
    Some(Guard { label, start: Instant::now() })
}

pub struct Guard {
    label: &'static str,
    start: Instant,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let dur = self.start.elapsed();
        if let Ok(mut samples) = SAMPLES.lock() {
            samples.push(Sample { label: self.label, dur });
        }
    }
}

/// Print a summary table of all recorded timings to stderr.
pub fn print_summary() {
    if !enabled() {
        return;
    }
    let samples = SAMPLES.lock().unwrap();
    if samples.is_empty() {
        return;
    }

    // Group by label
    let mut groups: Vec<(&'static str, Vec<Duration>)> = Vec::new();
    for s in samples.iter() {
        if let Some(g) = groups.iter_mut().find(|(l, _)| *l == s.label) {
            g.1.push(s.dur);
        } else {
            groups.push((s.label, vec![s.dur]));
        }
    }
    groups.sort_by_key(|(l, _)| *l);

    eprintln!("\n{:─<72}", "── bench ");
    eprintln!(
        "{:<30} {:>8} {:>10} {:>10} {:>10}",
        "function", "calls", "total", "avg", "max"
    );
    eprintln!("{:─<72}", "");

    for (label, mut durs) in groups {
        durs.sort();
        let count = durs.len();
        let total: Duration = durs.iter().sum();
        let avg = total / count as u32;
        let max = durs.last().copied().unwrap_or_default();
        eprintln!(
            "{:<30} {:>8} {:>10} {:>10} {:>10}",
            label,
            count,
            fmt_dur(total),
            fmt_dur(avg),
            fmt_dur(max),
        );
    }
    eprintln!("{:─<72}", "");
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
