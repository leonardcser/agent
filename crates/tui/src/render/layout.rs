pub use ui::Rect;

#[derive(Clone, Debug)]
pub struct LayoutState {
    pub term_width: u16,
    pub transcript: Rect,
    pub gap: u16,
    pub prompt: Rect,
    pub dialog: Option<DialogLayout>,
    pub floats: Vec<FloatEntry>,
}

#[derive(Clone, Debug)]
pub struct FloatEntry {
    pub rect: Rect,
    pub z: u8,
    pub region: HitRegion,
}

#[derive(Clone, Debug)]
pub struct DialogLayout {
    pub rect: Rect,
    pub status_row: u16,
}

#[derive(Debug)]
pub struct LayoutInput {
    pub term_width: u16,
    pub term_height: u16,
    pub prompt_height: u16,
    pub dialog_height: Option<u16>,
    pub constrain_dialog: bool,
}

impl LayoutState {
    pub fn compute(input: &LayoutInput) -> Self {
        let LayoutInput {
            term_width,
            term_height,
            prompt_height,
            dialog_height,
            constrain_dialog,
        } = *input;

        if let Some(dh) = dialog_height {
            Self::compute_dialog(term_width, term_height, dh, constrain_dialog)
        } else {
            Self::compute_normal(term_width, term_height, prompt_height)
        }
    }

    fn compute_normal(term_width: u16, term_height: u16, prompt_height: u16) -> Self {
        let max_prompt = (term_height / 2).max(3);
        let prompt_height = prompt_height.min(max_prompt);

        let tree = ui::LayoutTree::Split {
            direction: ui::layout::Direction::Vertical,
            children: vec![
                ui::LayoutTree::Leaf {
                    name: "transcript".into(),
                    constraint: ui::Constraint::Fill,
                },
                ui::LayoutTree::Leaf {
                    name: "gap".into(),
                    constraint: ui::Constraint::Fixed(1),
                },
                ui::LayoutTree::Leaf {
                    name: "prompt".into(),
                    constraint: ui::Constraint::Fixed(prompt_height),
                },
            ],
        };

        let area = Rect::new(0, 0, term_width, term_height);
        let regions = ui::layout::resolve_layout(&tree, area);

        let transcript = regions.get("transcript").copied().unwrap_or_default();
        let prompt = regions.get("prompt").copied().unwrap_or_default();

        LayoutState {
            term_width,
            transcript,
            gap: 1,
            prompt,
            dialog: None,
            floats: Vec::new(),
        }
    }

    fn compute_dialog(
        term_width: u16,
        term_height: u16,
        dialog_height: u16,
        constrain: bool,
    ) -> Self {
        let effective_dialog = if constrain {
            let half = term_height / 2;
            let natural = term_height.saturating_sub(2);
            dialog_height.min(half.max(natural))
        } else {
            dialog_height
        };
        // Reserve: dialog + 1 gap + 1 status row.
        let reserved = effective_dialog.saturating_add(2);
        let viewport_rows = term_height.saturating_sub(reserved);
        let dialog_row = viewport_rows;
        let max_avail = term_height.saturating_sub(2 + dialog_row);
        let granted = effective_dialog.min(max_avail);
        let status_row = term_height.saturating_sub(1);

        let transcript = Rect::new(0, 0, term_width, viewport_rows);
        let dialog_layout = if granted > 0 {
            Some(DialogLayout {
                rect: Rect::new(dialog_row, 0, term_width, granted),
                status_row,
            })
        } else {
            None
        };

        LayoutState {
            term_width,
            transcript,
            gap: 0,
            prompt: Rect::default(),
            dialog: dialog_layout,
            floats: Vec::new(),
        }
    }

    pub fn push_float(&mut self, rect: Rect, z: u8, region: HitRegion) {
        self.floats.push(FloatEntry { rect, z, region });
        self.floats.sort_by_key(|f| f.z);
    }

    pub fn viewport_rows(&self) -> u16 {
        self.transcript.height
    }

    pub fn hit_test(&self, row: u16, col: u16) -> HitRegion {
        for f in self.floats.iter().rev() {
            if f.rect.contains(row, col) {
                return f.region;
            }
        }
        if let Some(ref d) = self.dialog {
            if row == d.status_row {
                return HitRegion::Status;
            }
            if d.rect.contains(row, col) {
                return HitRegion::Dialog;
            }
        }
        if self.prompt.height > 0 {
            // Status bar is the last row of the prompt area.
            if row == self.prompt.bottom().saturating_sub(1) {
                return HitRegion::Status;
            }
            if self.prompt.contains(row, col) {
                return HitRegion::Prompt;
            }
        }
        if self.transcript.contains(row, col) {
            return HitRegion::Transcript;
        }
        HitRegion::Outside
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitRegion {
    Transcript,
    Prompt,
    Dialog,
    Completer,
    Status,
    Outside,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_layout_splits_term() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 80,
            term_height: 40,
            prompt_height: 5,
            dialog_height: None,
            constrain_dialog: false,
        });
        assert_eq!(layout.transcript.top, 0);
        assert_eq!(layout.transcript.height, 34); // 40 - 5 - 1 gap
        assert_eq!(layout.gap, 1);
        assert_eq!(layout.prompt.top, 35); // 34 + 1 gap
        assert_eq!(layout.prompt.height, 5);
        assert!(layout.dialog.is_none());
    }

    #[test]
    fn prompt_capped_at_half_height() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 80,
            term_height: 20,
            prompt_height: 15,
            dialog_height: None,
            constrain_dialog: false,
        });
        assert_eq!(layout.prompt.height, 10); // capped to 20/2
        assert_eq!(layout.transcript.height, 9); // 20 - 10 - 1
    }

    #[test]
    fn dialog_layout_reserves_gap_and_status() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 80,
            term_height: 40,
            prompt_height: 0,
            dialog_height: Some(10),
            constrain_dialog: false,
        });
        let d = layout.dialog.as_ref().unwrap();
        assert_eq!(layout.transcript.height, 28); // 40 - (10 + 2)
        assert_eq!(d.rect.top, 28);
        assert_eq!(d.rect.height, 10);
        assert_eq!(d.status_row, 39);
    }

    #[test]
    fn hit_test_routes_correctly() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 80,
            term_height: 40,
            prompt_height: 5,
            dialog_height: None,
            constrain_dialog: false,
        });
        assert_eq!(layout.hit_test(0, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(33, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(34, 0), HitRegion::Outside); // gap
        assert_eq!(layout.hit_test(35, 0), HitRegion::Prompt);
        assert_eq!(layout.hit_test(38, 0), HitRegion::Prompt);
        assert_eq!(layout.hit_test(39, 0), HitRegion::Status); // last prompt row
    }

    #[test]
    fn tiny_terminal_still_works() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 40,
            term_height: 3,
            prompt_height: 10,
            dialog_height: None,
            constrain_dialog: false,
        });
        // min cap is 3, so prompt_height = 3, but that's the whole term
        assert!(layout.transcript.height <= 3);
        assert!(layout.prompt.height <= 3);
    }

    #[test]
    fn dialog_hit_test() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 80,
            term_height: 40,
            prompt_height: 0,
            dialog_height: Some(10),
            constrain_dialog: false,
        });
        let d = layout.dialog.as_ref().unwrap();
        assert_eq!(layout.hit_test(0, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(d.rect.top, 0), HitRegion::Dialog);
        assert_eq!(layout.hit_test(d.status_row, 0), HitRegion::Status);
    }

    #[test]
    fn float_takes_precedence_over_docked() {
        let mut layout = LayoutState::compute(&LayoutInput {
            term_width: 80,
            term_height: 40,
            prompt_height: 5,
            dialog_height: None,
            constrain_dialog: false,
        });
        assert_eq!(layout.hit_test(10, 20), HitRegion::Transcript);
        layout.push_float(Rect::new(5, 10, 30, 10), 1, HitRegion::Dialog);
        assert_eq!(layout.hit_test(10, 20), HitRegion::Dialog);
        assert_eq!(layout.hit_test(10, 5), HitRegion::Transcript);
    }
}
