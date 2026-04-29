pub use ui::Rect;

#[derive(Clone, Debug)]
pub struct LayoutState {
    pub transcript: Rect,
    pub prompt: Rect,
}

#[derive(Debug)]
pub struct LayoutInput {
    pub term_width: u16,
    pub term_height: u16,
    pub prompt_height: u16,
}

impl LayoutState {
    pub fn compute(input: &LayoutInput) -> Self {
        let LayoutInput {
            term_width,
            term_height,
            prompt_height,
        } = *input;

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

        LayoutState { transcript, prompt }
    }

    pub fn viewport_rows(&self) -> u16 {
        self.transcript.height
    }

    pub fn hit_test(&self, row: u16, col: u16) -> HitRegion {
        if self.prompt.height > 0 {
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
        });
        assert_eq!(layout.transcript.top, 0);
        assert_eq!(layout.transcript.height, 34);
        assert_eq!(layout.prompt.top, 35);
        assert_eq!(layout.prompt.height, 5);
    }

    #[test]
    fn prompt_capped_at_half_height() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 80,
            term_height: 20,
            prompt_height: 15,
        });
        assert_eq!(layout.prompt.height, 10);
        assert_eq!(layout.transcript.height, 9);
    }

    #[test]
    fn hit_test_routes_correctly() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 80,
            term_height: 40,
            prompt_height: 5,
        });
        assert_eq!(layout.hit_test(0, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(33, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(34, 0), HitRegion::Outside);
        assert_eq!(layout.hit_test(35, 0), HitRegion::Prompt);
        assert_eq!(layout.hit_test(38, 0), HitRegion::Prompt);
        assert_eq!(layout.hit_test(39, 0), HitRegion::Status);
    }

    #[test]
    fn tiny_terminal_still_works() {
        let layout = LayoutState::compute(&LayoutInput {
            term_width: 40,
            term_height: 3,
            prompt_height: 10,
        });
        assert!(layout.transcript.height <= 3);
        assert!(layout.prompt.height <= 3);
    }
}
