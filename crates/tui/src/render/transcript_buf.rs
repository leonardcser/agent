use super::display::DisplayLine;
use super::history::{BlockHistory, LayoutKey, ViewState};
use super::to_buffer::{apply_to_buffer, project_display_line, ProjectedLine};
use crate::theme::Theme;
use ui::buffer::Buffer;

pub(crate) struct TranscriptProjection {
    buf: Buffer,
    display_lines: Vec<DisplayLine>,
    generation: u64,
    width: u16,
    show_thinking: bool,
}

impl TranscriptProjection {
    pub(crate) fn new(buf: Buffer) -> Self {
        Self {
            buf,
            display_lines: Vec::new(),
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

    pub(super) fn viewport_display_lines(
        &self,
        scroll: u16,
        viewport_rows: u16,
    ) -> Vec<DisplayLine> {
        let start = scroll as usize;
        let end = (start + viewport_rows as usize).min(self.display_lines.len());
        self.display_lines[start..end].to_vec()
    }

    pub(super) fn project(
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
        let mut raw_lines: Vec<DisplayLine> = Vec::new();

        for i in 0..history.len() {
            let rows = history.ensure_rows(i, key);
            let gap = if rows == 0 { 0 } else { history.block_gap(i) };

            for _ in 0..gap {
                lines.push(ProjectedLine::default());
                raw_lines.push(DisplayLine::default());
            }

            let id = history.order[i];
            let bkey = history.resolve_key(id, key);
            if let Some(display) = history.artifacts.get(&id).and_then(|a| a.get(bkey)) {
                for dline in &display.lines {
                    lines.push(project_display_line(dline, theme));
                    raw_lines.push(dline.clone());
                }
            }
        }

        for dline in ephemeral_lines {
            lines.push(project_display_line(dline, theme));
            raw_lines.push(dline.clone());
        }

        apply_to_buffer(&mut self.buf, &lines);
        self.display_lines = raw_lines;

        self.generation = gen;
        self.width = width;
        self.show_thinking = show_thinking;
    }
}
