use once_cell::sync::Lazy;
use redb::{Database, ReadableDatabase, TableDefinition};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("web_cache");
const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

static DB: Lazy<Option<Database>> = Lazy::new(|| {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    Database::create(&path).ok()
});

fn cache_path() -> PathBuf {
    dirs_cache().join("agent").join("web_cache.redb")
}

fn dirs_cache() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".cache")
        })
}

pub fn get(key: &str) -> Option<String> {
    let db = DB.as_ref()?;
    let tx = db.begin_read().ok()?;
    let table = tx.open_table(TABLE).ok()?;
    let entry = table.get(key).ok()??;
    let bytes = entry.value();
    if bytes.len() < 8 {
        return None;
    }
    let expires = u64::from_be_bytes(bytes[..8].try_into().ok()?);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now > expires {
        return None;
    }
    String::from_utf8(bytes[8..].to_vec()).ok()
}

pub fn put(key: &str, value: &str) {
    put_with_ttl(key, value, DEFAULT_TTL);
}

pub fn put_with_ttl(key: &str, value: &str, ttl: Duration) {
    let Some(db) = DB.as_ref() else { return };
    let Ok(tx) = db.begin_write() else { return };
    {
        let Ok(mut table) = tx.open_table(TABLE) else {
            return;
        };
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + ttl.as_secs();
        let mut buf = Vec::with_capacity(8 + value.len());
        buf.extend_from_slice(&expires.to_be_bytes());
        buf.extend_from_slice(value.as_bytes());
        let _ = table.insert(key, buf.as_slice());
    }
    let _ = tx.commit();
}
