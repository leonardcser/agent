use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

static NEXT_PROC_ID: AtomicU32 = AtomicU32::new(1);

/// Maximum number of output lines retained per background process.
/// Older lines are dropped once this limit is reached.
const MAX_LINES: usize = 10_000;

struct Process {
    lines: Vec<String>,
    read_cursor: usize,
    finished: bool,
    exit_code: Option<i32>,
    command: String,
    started_at: Instant,
    /// Sends SIGKILL to the child process.
    kill_tx: Option<mpsc::Sender<()>>,
}

/// Info about a running background process, returned by `list()`.
pub struct ProcessInfo {
    pub id: String,
    pub command: String,
    pub started_at: Instant,
}

impl Process {
    fn push_line(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(..drop);
            self.read_cursor = self.read_cursor.saturating_sub(drop);
        }
    }
}

/// Shared registry of background processes.
#[derive(Clone)]
pub struct ProcessRegistry(Arc<Mutex<HashMap<String, Process>>>);

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn a background process. Output is accumulated internally; a
    /// background tokio task reads stdout/stderr and marks the process
    /// finished when it exits.
    pub fn spawn(
        &self,
        id: String,
        command: &str,
        mut child: Child,
        done_tx: mpsc::UnboundedSender<(String, Option<i32>)>,
    ) {
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);

        {
            let mut map = self.0.lock().unwrap();
            map.insert(
                id.clone(),
                Process {
                    lines: Vec::new(),
                    read_cursor: 0,
                    finished: false,
                    exit_code: None,
                    command: command.to_string(),
                    started_at: Instant::now(),
                    kill_tx: Some(kill_tx),
                },
            );
        }

        let registry = self.0.clone();
        let id2 = id.clone();
        tokio::spawn(async move {
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stderr_reader = BufReader::new(stderr).lines();
            let mut stdout_done = false;
            let mut stderr_done = false;

            loop {
                if stdout_done && stderr_done {
                    break;
                }
                tokio::select! {
                    line = stdout_reader.next_line(), if !stdout_done => {
                        match line {
                            Ok(Some(line)) => {
                                let mut map = registry.lock().unwrap();
                                if let Some(p) = map.get_mut(&id2) {
                                    p.push_line(line);
                                }
                            }
                            _ => stdout_done = true,
                        }
                    }
                    line = stderr_reader.next_line(), if !stderr_done => {
                        match line {
                            Ok(Some(line)) => {
                                let mut map = registry.lock().unwrap();
                                if let Some(p) = map.get_mut(&id2) {
                                    p.push_line(line);
                                }
                            }
                            _ => stderr_done = true,
                        }
                    }
                    _ = kill_rx.recv() => {
                        super::kill_process_group(&child);
                        break;
                    }
                }
            }

            let status = child.wait().await;
            let code = status.ok().and_then(|s| s.code());
            {
                let mut map = registry.lock().unwrap();
                if let Some(p) = map.get_mut(&id2) {
                    p.finished = true;
                    p.exit_code = code;
                    p.kill_tx = None;
                }
            }
            let _ = done_tx.send((id2, code));
        });
    }

    /// Read new output since the last read. Returns (new_lines, running, exit_code).
    pub fn read(&self, id: &str) -> Result<(String, bool, Option<i32>), String> {
        let mut map = self.0.lock().unwrap();
        let p = map
            .get_mut(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        let output = std::mem::take(&mut p.lines).join("\n");
        p.read_cursor = 0;
        let running = !p.finished;
        let exit_code = p.exit_code;
        if p.finished {
            map.remove(id);
        }
        Ok((output, running, exit_code))
    }

    /// Stop a background process. Returns its final accumulated output.
    pub async fn stop(&self, id: &str) -> Result<String, String> {
        let kill_tx = {
            let mut map = self.0.lock().unwrap();
            let p = map
                .get_mut(id)
                .ok_or_else(|| format!("no process with id '{id}'"))?;
            p.kill_tx.take()
        };
        if let Some(tx) = kill_tx {
            let _ = tx.try_send(());
        }
        // Poll until the background task marks the process finished,
        // rather than a blind sleep that may be too short or too long.
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let map = self.0.lock().unwrap();
            if map.get(id).is_some_and(|p| p.finished) {
                break;
            }
        }
        let mut map = self.0.lock().unwrap();
        let p = map
            .remove(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        Ok(p.lines.join("\n"))
    }

    pub fn next_id(&self) -> String {
        let n = NEXT_PROC_ID.fetch_add(1, Ordering::Relaxed);
        format!("proc_{n}")
    }

    /// Number of currently running processes.
    pub fn running_count(&self) -> usize {
        let map = self.0.lock().unwrap();
        map.values().filter(|p| !p.finished).count()
    }

    /// List running background processes.
    pub fn list(&self) -> Vec<ProcessInfo> {
        let map = self.0.lock().unwrap();
        let mut procs: Vec<ProcessInfo> = map
            .iter()
            .filter(|(_, p)| !p.finished)
            .map(|(id, p)| ProcessInfo {
                id: id.clone(),
                command: p.command.clone(),
                started_at: p.started_at,
            })
            .collect();
        procs.sort_by(|a, b| a.id.cmp(&b.id));
        procs
    }

    /// Kill all running processes and remove all entries.
    pub fn clear(&self) {
        let mut map = self.0.lock().unwrap();
        for p in map.values_mut() {
            if let Some(tx) = p.kill_tx.take() {
                let _ = tx.try_send(());
            }
        }
        map.clear();
    }
}
