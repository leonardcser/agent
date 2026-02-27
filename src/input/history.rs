use crate::config;
use std::path::PathBuf;

pub struct History {
    entries: Vec<String>,
    cursor: usize,
    draft: String,
    path: PathBuf,
}

const RECORD_SEP: char = '\x1e';

impl History {
    pub fn load() -> Self {
        let path = config::state_dir().join("history");
        let entries = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .split(RECORD_SEP)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        let cursor = entries.len();
        Self {
            entries,
            cursor,
            draft: String::new(),
            path,
        }
    }

    pub fn push(&mut self, entry: String) {
        if !entry.is_empty() && self.entries.last().is_none_or(|last| *last != entry) {
            self.entries.push(entry.clone());
            self.append_to_file(&entry);
        }
        self.reset();
    }

    fn append_to_file(&self, entry: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = write!(f, "{}{}", entry, RECORD_SEP);
        }
    }

    fn reset(&mut self) {
        self.cursor = self.entries.len();
        self.draft.clear();
    }

    pub(super) fn up(&mut self, current_buf: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.cursor == self.entries.len() {
            self.draft = current_buf.to_string();
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            Some(&self.entries[self.cursor])
        } else {
            None
        }
    }

    pub(super) fn down(&mut self) -> Option<&str> {
        if self.cursor >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        if self.cursor == self.entries.len() {
            Some(&self.draft)
        } else {
            Some(&self.entries[self.cursor])
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }
}
