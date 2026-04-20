use crate::BufId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufType {
    Normal,
    Nofile,
    Prompt,
    Scratch,
}

#[derive(Clone, Debug)]
pub struct VirtualText {
    pub line: usize,
    pub col: usize,
    pub text: String,
    pub hl_group: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Mark {
    pub line: usize,
    pub col: usize,
}

pub struct BufCreateOpts {
    pub modifiable: bool,
    pub buftype: BufType,
}

impl Default for BufCreateOpts {
    fn default() -> Self {
        Self {
            modifiable: true,
            buftype: BufType::Normal,
        }
    }
}

pub struct Buffer {
    pub(crate) id: BufId,
    lines: Vec<String>,
    modifiable: bool,
    buftype: BufType,
    virtual_text: Vec<VirtualText>,
    marks: std::collections::HashMap<String, Mark>,
    changedtick: u64,
}

impl Buffer {
    pub(crate) fn new(id: BufId, opts: BufCreateOpts) -> Self {
        Self {
            id,
            lines: vec![String::new()],
            modifiable: opts.modifiable,
            buftype: opts.buftype,
            virtual_text: Vec::new(),
            marks: std::collections::HashMap::new(),
            changedtick: 0,
        }
    }

    pub fn id(&self) -> BufId {
        self.id
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn get_lines(&self, start: usize, end: usize) -> &[String] {
        let end = end.min(self.lines.len());
        let start = start.min(end);
        &self.lines[start..end]
    }

    pub fn get_line(&self, idx: usize) -> Option<&str> {
        self.lines.get(idx).map(|s| s.as_str())
    }

    pub fn set_lines(&mut self, start: usize, end: usize, replacement: Vec<String>) {
        if !self.modifiable {
            return;
        }
        let end = end.min(self.lines.len());
        let start = start.min(end);
        self.lines.splice(start..end, replacement);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.changedtick += 1;
    }

    pub fn set_all_lines(&mut self, lines: Vec<String>) {
        if !self.modifiable {
            return;
        }
        self.lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.changedtick += 1;
    }

    pub fn append_line(&mut self, line: String) {
        if !self.modifiable {
            return;
        }
        self.lines.push(line);
        self.changedtick += 1;
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn is_modifiable(&self) -> bool {
        self.modifiable
    }

    pub fn set_modifiable(&mut self, modifiable: bool) {
        self.modifiable = modifiable;
    }

    pub fn buftype(&self) -> &BufType {
        &self.buftype
    }

    pub fn changedtick(&self) -> u64 {
        self.changedtick
    }

    pub fn set_virtual_text(&mut self, line: usize, text: String, hl_group: Option<String>) {
        self.virtual_text.retain(|vt| vt.line != line);
        self.virtual_text.push(VirtualText {
            line,
            col: 0,
            text,
            hl_group,
        });
    }

    pub fn clear_virtual_text(&mut self, line: usize) {
        self.virtual_text.retain(|vt| vt.line != line);
    }

    pub fn virtual_text_at(&self, line: usize) -> Option<&VirtualText> {
        self.virtual_text.iter().find(|vt| vt.line == line)
    }

    pub fn virtual_text(&self) -> &[VirtualText] {
        &self.virtual_text
    }

    pub fn set_mark(&mut self, name: String, line: usize, col: usize) {
        self.marks.insert(name, Mark { line, col });
    }

    pub fn get_mark(&self, name: &str) -> Option<&Mark> {
        self.marks.get(name)
    }

    pub fn delete_mark(&mut self, name: &str) {
        self.marks.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf() -> Buffer {
        Buffer::new(BufId(1), BufCreateOpts::default())
    }

    #[test]
    fn new_buffer_has_one_empty_line() {
        let buf = make_buf();
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some(""));
    }

    #[test]
    fn set_lines_replaces_range() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into(), "b".into(), "c".into()]);
        buf.set_lines(1, 2, vec!["x".into(), "y".into()]);
        assert_eq!(buf.lines(), &["a", "x", "y", "c"]);
    }

    #[test]
    fn set_lines_clamps_range() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into()]);
        buf.set_lines(0, 100, vec!["replaced".into()]);
        assert_eq!(buf.lines(), &["replaced"]);
    }

    #[test]
    fn set_all_lines_empty_keeps_one_line() {
        let mut buf = make_buf();
        buf.set_all_lines(vec![]);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some(""));
    }

    #[test]
    fn readonly_buffer_rejects_mutations() {
        let mut buf = Buffer::new(
            BufId(1),
            BufCreateOpts {
                modifiable: false,
                buftype: BufType::Nofile,
            },
        );
        buf.set_all_lines(vec!["should not change".into()]);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some(""));
    }

    #[test]
    fn changedtick_increments() {
        let mut buf = make_buf();
        let t0 = buf.changedtick();
        buf.set_all_lines(vec!["a".into()]);
        assert!(buf.changedtick() > t0);
        let t1 = buf.changedtick();
        buf.append_line("b".into());
        assert!(buf.changedtick() > t1);
    }

    #[test]
    fn virtual_text_lifecycle() {
        let mut buf = make_buf();
        buf.set_virtual_text(0, "ghost".into(), None);
        assert!(buf.virtual_text_at(0).is_some());
        buf.clear_virtual_text(0);
        assert!(buf.virtual_text_at(0).is_none());
    }

    #[test]
    fn marks_lifecycle() {
        let mut buf = make_buf();
        buf.set_mark("a".into(), 0, 5);
        assert_eq!(buf.get_mark("a").unwrap().col, 5);
        buf.delete_mark("a");
        assert!(buf.get_mark("a").is_none());
    }

    #[test]
    fn text_joins_lines() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        assert_eq!(buf.text(), "hello\nworld");
    }
}
