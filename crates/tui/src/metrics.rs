use crate::config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Format a USD cost for display.
pub fn format_cost(usd: f64) -> String {
    if usd < 0.01 {
        format!("${:.4}", usd)
    } else if usd < 1.0 {
        format!("${:.3}", usd)
    } else {
        format!("${:.2}", usd)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsEntry {
    pub timestamp_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub model: String,
    /// Cost of this LLM call in USD. Absent in old entries.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u32>,
    #[serde(default)]
    pub cache_write_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

fn metrics_path() -> PathBuf {
    config::state_dir().join("metrics.jsonl")
}

/// Append a single entry to the metrics JSONL file.
pub fn append(entry: &MetricsEntry) {
    let path = metrics_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    if let Ok(line) = serde_json::to_string(entry) {
        let _ = writeln!(f, "{line}");
    }
}

/// Load all metrics entries from disk.
pub fn load() -> Vec<MetricsEntry> {
    let path = metrics_path();
    let Ok(f) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    std::io::BufReader::new(f)
        .lines()
        .filter_map(|line| {
            let line = line.ok()?;
            serde_json::from_str(&line).ok()
        })
        .collect()
}

// ── Aggregation ─────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    crate::session::now_ms()
}

fn day_key(ms: u64) -> u64 {
    ms / (24 * 3600 * 1000)
}

fn hour_key(ms: u64) -> u64 {
    ms / (3600 * 1000)
}

struct ModelStats {
    prompt: u64,
    completion: u64,
    calls: usize,
    cost_usd: f64,
}

impl ModelStats {
    fn total(&self) -> u64 {
        self.prompt + self.completion
    }
}

struct Stats {
    total_calls: usize,
    total_prompt: u64,
    total_completion: u64,
    total_cost_usd: f64,
    by_model: BTreeMap<String, ModelStats>,
    by_day: BTreeMap<u64, u64>,
    by_hour: BTreeMap<u64, u64>,
}

fn aggregate(entries: &[MetricsEntry]) -> Stats {
    let mut stats = Stats {
        total_calls: entries.len(),
        total_prompt: 0,
        total_completion: 0,
        total_cost_usd: 0.0,
        by_model: BTreeMap::new(),
        by_day: BTreeMap::new(),
        by_hour: BTreeMap::new(),
    };

    let h24_ago = now_ms().saturating_sub(24 * 3600 * 1000);

    for e in entries {
        let prompt = e.prompt_tokens as u64;
        let completion = e.completion_tokens as u64;
        let total = prompt + completion;
        let cost = e.cost_usd.unwrap_or(0.0);

        stats.total_prompt += prompt;
        stats.total_completion += completion;
        stats.total_cost_usd += cost;

        let m = stats.by_model.entry(e.model.clone()).or_insert(ModelStats {
            prompt: 0,
            completion: 0,
            calls: 0,
            cost_usd: 0.0,
        });
        m.prompt += prompt;
        m.completion += completion;
        m.calls += 1;
        m.cost_usd += cost;

        *stats.by_day.entry(day_key(e.timestamp_ms)).or_insert(0) += total;

        if e.timestamp_ms >= h24_ago {
            *stats.by_hour.entry(hour_key(e.timestamp_ms)).or_insert(0) += total;
        }
    }

    stats
}

// ── Structured output for the renderer ──────────────────────────────────────

pub enum StatsLine {
    /// Dim label + normal value.
    Kv { label: String, value: String },
    /// Section heading (dim).
    Heading(String),
    /// Sparkline bar characters (rendered in accent).
    SparklineBars(String),
    /// Sparkline legend (rendered dim).
    SparklineLegend(String),
    /// One row of the daily heatmap.
    HeatRow { label: String, cells: Vec<HeatCell> },
    /// Empty separator line.
    Blank,
}

#[derive(Clone, Copy)]
pub enum HeatCell {
    Empty,
    /// Intensity 0..=3 (maps to increasing brightness).
    Level(u8),
}

const SPARKLINE: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn sparkline(values: &[u64]) -> String {
    let max = values.iter().copied().max().unwrap_or(1).max(1);
    values
        .iter()
        .map(|&v| {
            let idx = ((v as f64 / max as f64) * (SPARKLINE.len() - 1) as f64).round() as usize;
            SPARKLINE[idx.min(SPARKLINE.len() - 1)]
        })
        .collect()
}

pub struct StatsOutput {
    pub left: Vec<StatsLine>,
    pub right: Vec<StatsLine>,
}

pub fn render_stats(entries: &[MetricsEntry]) -> StatsOutput {
    if entries.is_empty() {
        return StatsOutput {
            left: vec![StatsLine::Heading("No metrics recorded yet.".into())],
            right: vec![],
        };
    }

    let stats = aggregate(entries);
    let mut left = Vec::new();
    let mut right = Vec::new();
    let total = stats.total_prompt + stats.total_completion;

    if stats.total_cost_usd > 0.0 {
        left.push(StatsLine::Kv {
            label: "total cost".into(),
            value: format_cost(stats.total_cost_usd),
        });
    }
    left.push(StatsLine::Kv {
        label: "calls".into(),
        value: stats.total_calls.to_string(),
    });
    left.push(StatsLine::Kv {
        label: "tokens".into(),
        value: format!(
            "{} ({} prompt + {} completion)",
            fmt(total),
            fmt(stats.total_prompt),
            fmt(stats.total_completion),
        ),
    });
    if stats.total_calls > 0 {
        left.push(StatsLine::Kv {
            label: "avg/call".into(),
            value: format!("{} tokens", fmt(total / stats.total_calls as u64)),
        });
    }

    // Per-model breakdown (sorted by total tokens descending)
    if stats.by_model.len() > 1 {
        left.push(StatsLine::Blank);
        left.push(StatsLine::Heading("per model".into()));
        let mut models: Vec<_> = stats.by_model.iter().collect();
        models.sort_by_key(|b| std::cmp::Reverse(b.1.total()));
        let max_model_len = models.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let max_calls_len = models
            .iter()
            .map(|(_, m)| m.calls.to_string().len())
            .max()
            .unwrap_or(0);
        let max_tokens_len = models
            .iter()
            .map(|(_, m)| fmt(m.total()).len())
            .max()
            .unwrap_or(0);
        let show_cost = models.iter().any(|(_, m)| m.cost_usd > 0.0);
        for (model, m) in &models {
            let model_pad = max_model_len.saturating_sub(model.len()) + 2;
            let calls_str = m.calls.to_string();
            let tokens_str = fmt(m.total());
            let calls_pad = max_calls_len.saturating_sub(calls_str.len());
            let tokens_pad = max_tokens_len.saturating_sub(tokens_str.len());
            let cost_str = if show_cost {
                format!("    {}", format_cost(m.cost_usd))
            } else {
                String::new()
            };
            left.push(StatsLine::Kv {
                label: format!("  {model}{}", " ".repeat(model_pad)),
                value: format!(
                    "{}{calls_str}    {}{tokens_str}{cost_str}",
                    " ".repeat(calls_pad),
                    " ".repeat(tokens_pad),
                ),
            });
        }
    }

    // Last 24h hourly sparkline
    if !stats.by_hour.is_empty() {
        right.push(StatsLine::Heading("last 24 hours".into()));
        let now_hour = hour_key(now_ms());
        let values: Vec<u64> = (0..24)
            .map(|i| {
                let h = now_hour - 23 + i;
                stats.by_hour.get(&h).copied().unwrap_or(0)
            })
            .collect();
        right.push(StatsLine::SparklineBars(sparkline(&values)));
        right.push(StatsLine::SparklineLegend(
            "24h ago ─────────────── now".into(),
        ));
    }

    // Daily heatmap (last 12 weeks)
    if !stats.by_day.is_empty() {
        right.push(StatsLine::Blank);
        right.push(StatsLine::Heading("daily activity (12 weeks)".into()));

        let today = day_key(now_ms());
        let days: Vec<u64> = (0..84).map(|i| today - 83 + i).collect();
        let values: Vec<u64> = days
            .iter()
            .map(|d| stats.by_day.get(d).copied().unwrap_or(0))
            .collect();
        let max = values.iter().copied().max().unwrap_or(1).max(1);

        let day_labels = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
        for (row, label) in day_labels.iter().enumerate() {
            let mut cells = Vec::new();
            for week in 0..12 {
                let idx = week * 7 + row;
                if idx < values.len() {
                    let v = values[idx];
                    if v == 0 {
                        cells.push(HeatCell::Empty);
                    } else {
                        let level = ((v as f64 / max as f64) * 3.0).round() as u8;
                        cells.push(HeatCell::Level(level.min(3)));
                    }
                }
            }
            right.push(StatsLine::HeatRow {
                label: label.to_string(),
                cells,
            });
        }
    }

    StatsOutput { left, right }
}

/// Count visual rows needed to display the stats panels.
/// Accounts for whether side-by-side fits the current terminal width.
pub fn stats_row_count(left: &[StatsLine], right: &[StatsLine]) -> usize {
    if right.is_empty() {
        return left.len();
    }

    let left_lc = label_col_width(left);
    let right_lc = label_col_width(right);
    let left_width = left
        .iter()
        .map(|l| stats_line_visual_width(l, left_lc))
        .max()
        .unwrap_or(0)
        + 2;
    let right_width = right
        .iter()
        .map(|l| stats_line_visual_width(l, right_lc))
        .max()
        .unwrap_or(0);
    let term_width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let gap = 5;

    if left_width + gap + right_width + 2 <= term_width {
        left.len().max(right.len())
    } else {
        // Sequential: left + blank separator + right
        left.len() + 1 + right.len()
    }
}

/// Visual width of a stats line (excluding the 2-char left margin).
/// Minimum gap between label and value columns.
const KV_GAP: usize = 2;

/// Compute the label column width for a set of lines (max label length + gap).
pub fn label_col_width(lines: &[StatsLine]) -> usize {
    lines
        .iter()
        .filter_map(|l| match l {
            StatsLine::Kv { label, .. } => Some(label.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + KV_GAP
}

pub fn stats_line_visual_width(line: &StatsLine, label_col: usize) -> usize {
    match line {
        StatsLine::Kv { label, value } => {
            let col = label_col.max(label.len() + KV_GAP);
            col + value.len()
        }
        StatsLine::Heading(text) | StatsLine::SparklineLegend(text) => text.len(),
        StatsLine::SparklineBars(bars) => bars.chars().count(),
        StatsLine::HeatRow { label, cells } => label.len() + 1 + cells.len() * 2,
        StatsLine::Blank => 0,
    }
}

/// Flatten one `StatsLine` to a plain string. Used by the `/stats` and
/// `/cost` Lua plugins which render through `smelt.ui.dialog.open` and
/// need a textual representation rather than the structured variants.
pub fn stats_line_to_text(line: &StatsLine, label_col: usize) -> String {
    match line {
        StatsLine::Kv { label, value } => {
            let pad = label_col.saturating_sub(label.len());
            format!("{label}{}{value}", " ".repeat(pad))
        }
        StatsLine::Heading(text) => text.clone(),
        StatsLine::SparklineBars(bars) => bars.clone(),
        StatsLine::SparklineLegend(text) => text.clone(),
        StatsLine::HeatRow { label, cells } => {
            let mut out = String::new();
            out.push_str(label);
            out.push(' ');
            for cell in cells {
                out.push_str(match cell {
                    HeatCell::Empty => "·",
                    HeatCell::Level(0) => "░",
                    HeatCell::Level(1) => "▒",
                    HeatCell::Level(2) => "▓",
                    HeatCell::Level(_) => "█",
                });
                out.push(' ');
            }
            out
        }
        StatsLine::Blank => String::new(),
    }
}

/// Render full `/stats` output as a single string. Two-column layout
/// joined row-by-row when both columns are present; falls back to
/// sequential left → blank → right.
pub fn render_stats_text(out: &StatsOutput) -> String {
    let left_col = label_col_width(&out.left);
    let right_col = label_col_width(&out.right);
    if out.right.is_empty() {
        return out
            .left
            .iter()
            .map(|l| stats_line_to_text(l, left_col))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let left_visual = out
        .left
        .iter()
        .map(|l| stats_line_visual_width(l, left_col))
        .max()
        .unwrap_or(0);
    let term_width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let right_visual = out
        .right
        .iter()
        .map(|l| stats_line_visual_width(l, right_col))
        .max()
        .unwrap_or(0);
    let gap = 5;

    if left_visual + gap + right_visual + 2 <= term_width {
        // Side-by-side.
        let rows = out.left.len().max(out.right.len());
        (0..rows)
            .map(|i| {
                let l_text = out
                    .left
                    .get(i)
                    .map(|l| stats_line_to_text(l, left_col))
                    .unwrap_or_default();
                let r_text = out
                    .right
                    .get(i)
                    .map(|l| stats_line_to_text(l, right_col))
                    .unwrap_or_default();
                let pad = (left_visual + gap).saturating_sub(l_text.chars().count());
                format!("{l_text}{}{r_text}", " ".repeat(pad))
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        // Sequential.
        let mut rows: Vec<String> = out
            .left
            .iter()
            .map(|l| stats_line_to_text(l, left_col))
            .collect();
        rows.push(String::new());
        rows.extend(out.right.iter().map(|l| stats_line_to_text(l, right_col)));
        rows.join("\n")
    }
}

/// Render `/cost` output (single column) as a plain string.
pub fn render_cost_text(lines: &[StatsLine]) -> String {
    let col = label_col_width(lines);
    lines
        .iter()
        .map(|l| stats_line_to_text(l, col))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_session_cost(
    cost_usd: f64,
    model: &str,
    turns: usize,
    resolved: &engine::pricing::ResolvedPricing,
) -> Vec<StatsLine> {
    let mut lines = Vec::new();
    let pricing = &resolved.pricing;

    lines.push(StatsLine::Heading("session".into()));
    lines.push(StatsLine::Kv {
        label: "cost".into(),
        value: if cost_usd > 0.0 {
            format_cost(cost_usd)
        } else {
            "$0".into()
        },
    });
    lines.push(StatsLine::Kv {
        label: "model".into(),
        value: model.to_string(),
    });
    lines.push(StatsLine::Kv {
        label: "turns".into(),
        value: turns.to_string(),
    });
    lines.push(StatsLine::Blank);

    let fmt_rate = |rate: f64| -> String {
        if rate == 0.0 {
            return "—".into();
        }
        format_cost(rate)
    };

    lines.push(StatsLine::Heading("pricing (per 1M tokens)".into()));
    lines.push(StatsLine::Kv {
        label: "source".into(),
        value: resolved.source.label().to_string(),
    });
    if !pricing.is_zero() {
        lines.push(StatsLine::Kv {
            label: "input".into(),
            value: fmt_rate(pricing.input),
        });
        lines.push(StatsLine::Kv {
            label: "output".into(),
            value: fmt_rate(pricing.output),
        });
        if pricing.cache_read > 0.0 {
            lines.push(StatsLine::Kv {
                label: "cache read".into(),
                value: fmt_rate(pricing.cache_read),
            });
        }
        if pricing.cache_write > 0.0 {
            lines.push(StatsLine::Kv {
                label: "cache write".into(),
                value: fmt_rate(pricing.cache_write),
            });
        }
    }
    lines
}

fn fmt(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
