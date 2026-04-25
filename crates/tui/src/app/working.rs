//! Turn state for the status bar: a running "live" turn (animating
//! spinner, elapsed clock, tokens-per-second accumulator) and/or the
//! "last" archived turn result displayed until the next turn begins.
//!
//! Splitting these into separate slots — instead of one `throbber`
//! that carries both "animating" and "display-done" semantics — makes
//! it impossible for the render loop to mistake a completed turn for
//! a frame-by-frame animation (which used to keep CPU pegged at
//! 60 Hz after every completed turn).

use crate::content::{BarSpan, SPINNER_FRAMES};
use crate::theme;
use crate::utils::format_duration;
use crossterm::style::Color;
use protocol::TurnMeta;
use std::time::{Duration, Instant};

/// Phase of the currently-running turn. The spinner animates based on
/// `since` and the phase; `Retrying` additionally carries a delay and
/// attempt counter for the countdown display.
#[derive(Clone, Copy, PartialEq)]
pub enum TurnPhase {
    Working,
    Compacting,
    Retrying { delay: Duration, attempt: u32 },
}

/// Outcome of a completed turn.
#[derive(Clone, Copy, PartialEq)]
pub enum TurnOutcome {
    Done,
    Interrupted,
}

/// A turn that is currently running.
pub struct LiveTurn {
    pub phase: TurnPhase,
    pub since: Instant,
    /// Absolute time at which a `Retrying` phase ends. `None` for
    /// other phases.
    pub retry_deadline: Option<Instant>,
    pub tps_samples: Vec<f64>,
    pub last_spinner_frame: usize,
}

/// Archived metadata from the last completed turn. Shown in the
/// status bar until the next `begin()`.
pub struct LastTurn {
    pub outcome: TurnOutcome,
    pub elapsed: Duration,
    pub avg_tps: Option<f64>,
}

#[derive(Default)]
pub struct WorkingState {
    live: Option<LiveTurn>,
    last: Option<LastTurn>,
}

impl WorkingState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new live turn, or update the phase of the currently-
    /// running one (keeps `since` and accumulated `tps_samples`).
    pub fn begin(&mut self, phase: TurnPhase) {
        let retry_deadline = match phase {
            TurnPhase::Retrying { delay, .. } => Some(Instant::now() + delay),
            _ => None,
        };
        match self.live.as_mut() {
            Some(live) => {
                live.phase = phase;
                live.retry_deadline = retry_deadline;
            }
            None => {
                self.live = Some(LiveTurn {
                    phase,
                    since: Instant::now(),
                    retry_deadline,
                    tps_samples: Vec::new(),
                    last_spinner_frame: usize::MAX,
                });
                self.last = None;
            }
        }
    }

    /// Archive the live turn's metadata as `last` and clear live.
    pub fn finish(&mut self, outcome: TurnOutcome) {
        let (elapsed, avg_tps) = match self.live.take() {
            Some(live) => (live.since.elapsed(), avg(&live.tps_samples)),
            None => (Duration::ZERO, None),
        };
        self.last = Some(LastTurn {
            outcome,
            elapsed,
            avg_tps,
        });
    }

    pub fn clear(&mut self) {
        self.live = None;
        self.last = None;
    }

    /// Whether the status bar currently displays a frame-by-frame
    /// animation (spinner glyph rotating, elapsed clock ticking).
    /// `false` once a turn has completed — the archived result is
    /// static text.
    pub fn is_animating(&self) -> bool {
        self.live.is_some()
    }

    pub fn is_compacting(&self) -> bool {
        matches!(
            self.live.as_ref().map(|l| l.phase),
            Some(TurnPhase::Compacting)
        )
    }

    pub fn record_tokens_per_sec(&mut self, tps: f64) {
        if let Some(live) = self.live.as_mut() {
            live.tps_samples.push(tps);
        }
    }

    /// Elapsed time for the display — `since` for a live turn,
    /// archived `elapsed` otherwise.
    pub fn elapsed(&self) -> Option<Duration> {
        if let Some(live) = self.live.as_ref() {
            Some(live.since.elapsed())
        } else {
            self.last.as_ref().map(|l| l.elapsed)
        }
    }

    pub fn last_spinner_frame(&self) -> Option<usize> {
        self.live.as_ref().map(|l| l.last_spinner_frame)
    }

    pub fn set_last_spinner_frame(&mut self, frame: usize) {
        if let Some(live) = self.live.as_mut() {
            live.last_spinner_frame = frame;
        }
    }

    pub fn turn_meta(&self) -> Option<TurnMeta> {
        if let Some(live) = self.live.as_ref() {
            return Some(TurnMeta {
                elapsed_ms: live.since.elapsed().as_millis() as u64,
                avg_tps: avg(&live.tps_samples),
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
                agent_blocks: std::collections::HashMap::new(),
            });
        }
        self.last.as_ref().map(|last| TurnMeta {
            elapsed_ms: last.elapsed.as_millis() as u64,
            avg_tps: last.avg_tps,
            interrupted: matches!(last.outcome, TurnOutcome::Interrupted),
            tool_elapsed: std::collections::HashMap::new(),
            agent_blocks: std::collections::HashMap::new(),
        })
    }

    pub fn restore_from_turn_meta(&mut self, meta: &TurnMeta) {
        self.live = None;
        self.last = Some(LastTurn {
            outcome: if meta.interrupted {
                TurnOutcome::Interrupted
            } else {
                TurnOutcome::Done
            },
            elapsed: Duration::from_millis(meta.elapsed_ms),
            avg_tps: meta.avg_tps,
        });
    }

    /// Spinner glyph for the active live turn, or `None` when
    /// nothing is animating.
    pub fn spinner_char(&self) -> Option<&'static str> {
        let live = self.live.as_ref()?;
        Some(SPINNER_FRAMES[crate::content::spinner_frame_index(live.since.elapsed())])
    }

    pub(crate) fn throbber_spans(&self, show_tps: bool) -> Vec<BarSpan> {
        if let Some(live) = self.live.as_ref() {
            return live_spans(live, show_tps);
        }
        if let Some(last) = self.last.as_ref() {
            return last_spans(last, show_tps);
        }
        Vec::new()
    }
}

fn avg(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    Some(samples.iter().sum::<f64>() / samples.len() as f64)
}

fn tps_spans(avg_tps: Option<f64>) -> Vec<BarSpan> {
    let Some(avg) = avg_tps else {
        return Vec::new();
    };
    vec![
        BarSpan {
            text: "·".into(),
            color: theme::muted(),
            bg: None,
            bold: false,
            dim: true,
            priority: 3, // drop first
        },
        BarSpan {
            text: format!(" {:.1} tok/s", avg),
            color: theme::muted(),
            bg: None,
            bold: false,
            dim: true,
            priority: 3,
        },
    ]
}

fn live_spans(live: &LiveTurn, show_tps: bool) -> Vec<BarSpan> {
    let elapsed = live.since.elapsed();
    let idx = crate::content::spinner_frame_index(elapsed);
    match live.phase {
        TurnPhase::Compacting => vec![
            BarSpan {
                text: format!(" {} compacting ", SPINNER_FRAMES[idx]),
                color: Color::Reset,
                bg: None,
                bold: true,
                dim: false,
                priority: 0,
            },
            BarSpan {
                text: format!(" {} ", format_duration(elapsed.as_secs())),
                color: theme::muted(),
                bg: None,
                bold: false,
                dim: true,
                priority: 0,
            },
        ],
        TurnPhase::Working | TurnPhase::Retrying { .. } => {
            let spinner_color = if matches!(live.phase, TurnPhase::Retrying { .. }) {
                theme::muted()
            } else {
                Color::Reset
            };
            let mut spans = vec![
                BarSpan {
                    text: format!(" {} working ", SPINNER_FRAMES[idx]),
                    color: spinner_color,
                    bg: None,
                    bold: true,
                    dim: false,
                    priority: 0,
                },
                BarSpan {
                    text: format!(" {} ", format_duration(elapsed.as_secs())),
                    color: theme::muted(),
                    bg: None,
                    bold: false,
                    dim: true,
                    priority: 0,
                },
            ];
            if show_tps {
                spans.extend(tps_spans(avg(&live.tps_samples)));
            }
            if let TurnPhase::Retrying { delay, attempt } = live.phase {
                let remaining = live
                    .retry_deadline
                    .map(|t| t.saturating_duration_since(Instant::now()))
                    .unwrap_or(delay);
                spans.push(BarSpan {
                    text: format!(" (retrying in {}s #{}) ", remaining.as_secs(), attempt),
                    color: theme::muted(),
                    bg: None,
                    bold: false,
                    dim: true,
                    priority: 0,
                });
            }
            spans
        }
    }
}

fn last_spans(last: &LastTurn, show_tps: bool) -> Vec<BarSpan> {
    match last.outcome {
        TurnOutcome::Done => {
            let mut spans = vec![BarSpan {
                text: format!(" done {} ", format_duration(last.elapsed.as_secs())),
                color: theme::muted(),
                bg: None,
                bold: false,
                dim: true,
                priority: 0,
            }];
            if show_tps {
                spans.extend(tps_spans(last.avg_tps));
            }
            spans
        }
        TurnOutcome::Interrupted => vec![BarSpan {
            text: " interrupted ".into(),
            color: theme::muted(),
            bg: None,
            bold: false,
            dim: true,
            priority: 0,
        }],
    }
}
