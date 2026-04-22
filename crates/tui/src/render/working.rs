use super::{BarSpan, Throbber, SPINNER_FRAMES};
use crate::theme;
use crate::utils::format_duration;
use crossterm::style::Color;
use protocol::TurnMeta;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct WorkingState {
    pub since: Option<Instant>,
    pub final_elapsed: Option<Duration>,
    pub throbber: Option<Throbber>,
    pub last_spinner_frame: usize,
    retry_deadline: Option<Instant>,
    tps_samples: Vec<f64>,
}

impl WorkingState {
    pub fn new() -> Self {
        Self {
            since: None,
            final_elapsed: None,
            throbber: None,
            last_spinner_frame: usize::MAX,
            retry_deadline: None,
            tps_samples: Vec::new(),
        }
    }

    pub fn set_throbber(&mut self, state: Throbber) {
        let is_active = matches!(
            state,
            Throbber::Working | Throbber::Retrying { .. } | Throbber::Compacting
        );
        if is_active && self.since.is_none() {
            self.since = Some(Instant::now());
            self.final_elapsed = None;
            self.tps_samples.clear();
        }
        if !is_active {
            self.final_elapsed = self.elapsed();
            self.since = None;
        }
        self.retry_deadline = match state {
            Throbber::Retrying { delay, .. } => Some(Instant::now() + delay),
            _ => None,
        };
        self.throbber = Some(state);
    }

    pub fn record_tokens_per_sec(&mut self, tps: f64) {
        self.tps_samples.push(tps);
    }

    fn avg_tokens_per_sec(&self) -> Option<f64> {
        if self.tps_samples.is_empty() {
            return None;
        }
        let sum: f64 = self.tps_samples.iter().sum();
        Some(sum / self.tps_samples.len() as f64)
    }

    pub fn turn_meta(&self) -> Option<TurnMeta> {
        let throbber = self.throbber?;
        let elapsed = match throbber {
            Throbber::Done | Throbber::Interrupted => self.final_elapsed?,
            _ => self.elapsed()?,
        };
        Some(TurnMeta {
            elapsed_ms: elapsed.as_millis() as u64,
            avg_tps: self.avg_tokens_per_sec(),
            interrupted: matches!(throbber, Throbber::Interrupted),
            tool_elapsed: std::collections::HashMap::new(),
            agent_blocks: std::collections::HashMap::new(),
        })
    }

    pub fn restore_from_turn_meta(&mut self, meta: &TurnMeta) {
        self.final_elapsed = Some(Duration::from_millis(meta.elapsed_ms));
        self.tps_samples.clear();
        if let Some(tps) = meta.avg_tps {
            self.tps_samples.push(tps);
        }
        self.throbber = Some(if meta.interrupted {
            Throbber::Interrupted
        } else {
            Throbber::Done
        });
    }

    /// Elapsed time since the throbber started.
    pub fn elapsed(&self) -> Option<Duration> {
        Some(self.since?.elapsed())
    }

    pub fn clear(&mut self) {
        self.throbber = None;
        self.since = None;
        self.final_elapsed = None;
        self.tps_samples.clear();
    }

    /// Returns the current spinner character if actively working/compacting.
    pub fn spinner_char(&self) -> Option<&'static str> {
        let state = self.throbber?;
        match state {
            Throbber::Working | Throbber::Compacting | Throbber::Retrying { .. } => {
                let elapsed = self.elapsed()?;
                let idx = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
                Some(SPINNER_FRAMES[idx])
            }
            _ => None,
        }
    }

    pub(crate) fn throbber_spans(&self, show_tps: bool) -> Vec<BarSpan> {
        let Some(state) = self.throbber else {
            return vec![];
        };
        match state {
            Throbber::Compacting => {
                let Some(elapsed) = self.elapsed() else {
                    return vec![];
                };
                let idx = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
                vec![
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
                ]
            }
            Throbber::Working | Throbber::Retrying { .. } => {
                let Some(elapsed) = self.elapsed() else {
                    return vec![];
                };
                let idx = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
                let spinner_color = if matches!(state, Throbber::Retrying { .. }) {
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
                    if let Some(avg) = self.avg_tokens_per_sec() {
                        spans.push(BarSpan {
                            text: "·".into(),
                            color: theme::muted(),
                            bg: None,
                            bold: false,
                            dim: true,
                            priority: 3, // drop first
                        });
                        spans.push(BarSpan {
                            text: format!(" {:.1} tok/s", avg),
                            color: theme::muted(),
                            bg: None,
                            bold: false,
                            dim: true,
                            priority: 3, // drop first
                        });
                    }
                }
                if let Throbber::Retrying { delay, attempt } = state {
                    let remaining = self
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
            Throbber::Done => {
                let secs = self.final_elapsed.map(|d| d.as_secs()).unwrap_or(0);
                let mut spans = vec![BarSpan {
                    text: format!(" done {} ", format_duration(secs)),
                    color: theme::muted(),
                    bg: None,
                    bold: false,
                    dim: true,
                    priority: 0,
                }];
                if show_tps {
                    if let Some(avg) = self.avg_tokens_per_sec() {
                        spans.push(BarSpan {
                            text: "·".into(),
                            color: theme::muted(),
                            bg: None,
                            bold: false,
                            dim: true,
                            priority: 3,
                        });
                        spans.push(BarSpan {
                            text: format!(" {:.1} tok/s", avg),
                            color: theme::muted(),
                            bg: None,
                            bold: false,
                            dim: true,
                            priority: 3,
                        });
                    }
                }
                spans
            }
            Throbber::Interrupted => {
                vec![BarSpan {
                    text: " interrupted ".into(),
                    color: theme::muted(),
                    bg: None,
                    bold: false,
                    dim: true,
                    priority: 0,
                }]
            }
        }
    }
}
