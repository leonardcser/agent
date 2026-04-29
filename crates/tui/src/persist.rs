//! Background session persistence.
//!
//! `App::save_session` used to serialize + write the full session on the
//! main loop — easily 1–5 MB of JSON work at every turn boundary. This
//! module moves the serialization and disk I/O to a worker thread. The
//! main loop clones the session + the blob data it needs into a
//! `PersistRequest` and sends it; the worker coalesces adjacent saves for
//! the same session id (drains additional queued requests before writing)
//! and handles the rest.
//!
//! Callers that need the on-disk state to be up-to-date (session load,
//! fork, shutdown) call [`Persister::flush`] to drain the queue first.

use crate::app::transcript_cache::{PersistedLayoutCache, RenderCache};
use crate::session::{self, Session};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// One image blob to write alongside the session.
pub struct Blob {
    pub filename: String,
    pub data_url: String,
}

pub struct PersistRequest {
    pub session: Session,
    pub blobs: Vec<Blob>,
    pub render_cache: Option<RenderCache>,
    pub layout_cache: Option<PersistedLayoutCache>,
}

enum Cmd {
    Save(Box<PersistRequest>),
    Flush(Sender<()>),
}

pub struct Persister {
    tx: Option<Sender<Cmd>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Persister {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("smelt-persist".into())
            .spawn(move || worker_loop(rx))
            .expect("spawn persist worker");
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    pub fn save(&self, req: PersistRequest) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Save(Box::new(req)));
        }
    }

    /// Block until all queued saves have been written. No-op if the worker
    /// has already exited (panic or shutdown).
    pub fn flush(&self) {
        let Some(tx) = &self.tx else { return };
        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let (done_tx, done_rx) = mpsc::channel();
        if tx.send(Cmd::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }
}

impl Drop for Persister {
    fn drop(&mut self) {
        self.flush();
        // Drop the sender so the worker's recv loop exits, then join.
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn worker_loop(rx: Receiver<Cmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Save(mut req) => {
                // Coalesce: drain any already-queued saves for the same
                // session id. Keep only the latest payload for that id;
                // process other sessions in order.
                let mut others: Vec<Box<PersistRequest>> = Vec::new();
                while let Ok(next) = rx.try_recv() {
                    match next {
                        Cmd::Save(r) if r.session.id == req.session.id => req = r,
                        Cmd::Save(r) => others.push(r),
                        Cmd::Flush(done) => {
                            write(&req);
                            for o in others.drain(..) {
                                write(&o);
                            }
                            let _ = done.send(());
                            continue;
                        }
                    }
                }
                write(&req);
                for o in others {
                    write(&o);
                }
            }
            Cmd::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn write(req: &PersistRequest) {
    let session_dir = session::dir_for(&req.session);
    let _ = std::fs::create_dir_all(&session_dir);
    let blob_dir = session_dir.join("blobs");
    let url_to_blob = write_blobs(&blob_dir, &req.blobs);
    session::save_with_blobs(&req.session, &url_to_blob);
    if let Some(cache) = &req.render_cache {
        session::save_render_cache(&req.session, cache);
    }
    if let Some(cache) = &req.layout_cache {
        session::save_layout_cache(&req.session, cache);
    }
}

fn write_blobs(
    blob_dir: &std::path::Path,
    blobs: &[Blob],
) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut url_to_blob = HashMap::new();
    if blobs.is_empty() {
        return url_to_blob;
    }
    let _ = std::fs::create_dir_all(blob_dir);
    for b in blobs {
        let path: PathBuf = blob_dir.join(&b.filename);
        if !path.exists() {
            let _ = std::fs::write(&path, b.data_url.as_bytes());
        }
        url_to_blob.insert(b.data_url.clone(), format!("blob:{}", b.filename));
    }
    url_to_blob
}
