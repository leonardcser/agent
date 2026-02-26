use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn log_path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        let dir = dirs();
        let _ = fs::create_dir_all(&dir);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        dir.join(format!("{ts}.jsonl"))
    })
}

fn dirs() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agent").join("logs")
}

pub fn path() -> &'static PathBuf {
    log_path()
}

pub fn entry(event: &str, data: &impl Serialize) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let payload = serde_json::json!({
        "ts": ts,
        "event": event,
        "data": data,
    });

    let Ok(line) = serde_json::to_string(&payload) else {
        return;
    };

    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    else {
        return;
    };

    let _ = writeln!(f, "{line}");
}
