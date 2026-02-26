use std::process::Command;

const MAX_RESULTS: usize = 5;

pub struct FileCompleter {
    /// Byte offset in the buffer where the '@' starts.
    pub anchor: usize,
    /// Current query (text after '@').
    pub query: String,
    /// Filtered results.
    pub results: Vec<String>,
    /// Selected index in results.
    pub selected: usize,
    /// Full file list (cached on activation).
    all_files: Vec<String>,
}

impl FileCompleter {
    pub fn new(anchor: usize) -> Self {
        let all_files = git_files();
        let results = all_files.iter().take(MAX_RESULTS).cloned().collect();
        Self {
            anchor,
            query: String::new(),
            results,
            selected: 0,
            all_files,
        }
    }

    pub fn update_query(&mut self, query: String) {
        self.query = query;
        self.filter();
    }

    fn filter(&mut self) {
        if self.query.is_empty() {
            self.results = self.all_files.iter().take(MAX_RESULTS).cloned().collect();
        } else {
            let q = self.query.to_lowercase();
            self.results = self
                .all_files
                .iter()
                .filter(|f| fuzzy_match(f, &q))
                .take(MAX_RESULTS)
                .cloned()
                .collect();
        }
        if self.selected >= self.results.len() {
            self.selected = 0;
        }
    }

    pub fn move_up(&mut self) {
        if !self.results.is_empty() {
            self.selected = if self.selected == 0 {
                self.results.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn move_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
        }
    }

    pub fn accept(&self) -> Option<&str> {
        self.results.get(self.selected).map(|s| s.as_str())
    }
}

/// Fuzzy match: all query chars appear in order in the path (case-insensitive).
fn fuzzy_match(path: &str, query: &str) -> bool {
    let lower = path.to_lowercase();
    let mut hay = lower.chars().peekable();
    for qc in query.chars() {
        loop {
            match hay.next() {
                Some(pc) if pc == qc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// Get tracked + untracked (but not ignored) files via git.
fn git_files() -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let mut files: Vec<String> = s.lines().filter(|l| !l.is_empty()).map(String::from).collect();
            files.sort();
            files
        }
        Err(_) => Vec::new(),
    }
}
