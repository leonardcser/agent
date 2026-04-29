use super::display::DisplayLine;
use super::to_buffer::{apply_to_buffer, project_display_line, ProjectedLine};
use crate::app::transcript_model::{BlockHistory, LayoutKey, ViewState};
use crate::theme::Theme;
use ui::buffer::Buffer;

pub(crate) struct TranscriptProjection {
    buf: Buffer,
    generation: u64,
    width: u16,
    show_thinking: bool,
}

impl TranscriptProjection {
    pub(crate) fn new(buf: Buffer) -> Self {
        Self {
            buf,
            generation: u64::MAX,
            width: 0,
            show_thinking: false,
        }
    }

    pub(crate) fn buf(&self) -> &Buffer {
        &self.buf
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.buf.line_count()
    }

    pub(crate) fn project(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        ephemeral_lines: &[DisplayLine],
    ) {
        let gen = history.generation();
        if gen == self.generation && width == self.width && show_thinking == self.show_thinking {
            return;
        }

        if width as usize != history.cache_width {
            history.invalidate_for_width(width as usize);
        }

        let key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
        };

        let mut lines: Vec<ProjectedLine> = Vec::new();

        for i in 0..history.len() {
            let rows = history.ensure_rows(i, key);
            let gap = if rows == 0 { 0 } else { history.block_gap(i) };

            for _ in 0..gap {
                lines.push(ProjectedLine::default());
            }

            let id = history.order[i];
            let bkey = history.resolve_key(id, key);
            if let Some(display) = history.artifacts.get(&id).and_then(|a| a.get(bkey)) {
                for dline in &display.lines {
                    lines.push(project_display_line(dline, theme));
                }
            }
        }

        for dline in ephemeral_lines {
            lines.push(project_display_line(dline, theme));
        }

        apply_to_buffer(&mut self.buf, &lines);

        self.generation = gen;
        self.width = width;
        self.show_thinking = show_thinking;
    }
}
