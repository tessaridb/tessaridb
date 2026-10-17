//! A store dropped without being closed is closed in the background, and a store opened at the
//! same directory in this process waits for that close.
//!
//! The engine's background work outlives a dropped handle and keeps the directory locked until it
//! ends, so an open right after a drop would be refused as if another process held the store.
//! Closing on drop finishes that work cleanly (flushed and synced), and the wait turns the
//! refusal into an ordered hand-over. Another process is still refused: only this process knows
//! its own stores are closing.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use dashmap::DashMap;
use tessarikv::Db;
use tokio::runtime::Handle;
use tokio::sync::watch;

/// The directories whose dropped store is still closing, each with what says when it has. Read
/// and written only when a store opens or is dropped.
static CLOSING: LazyLock<DashMap<PathBuf, watch::Receiver<bool>>> = LazyLock::new(DashMap::new);

/// One key per directory, however it was spelled.
fn key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Closes `db`, the engine of the store at `path`, on `runtime`, where it was opened.
pub(super) fn close_in_background(runtime: &Handle, path: &Path, db: Db) {
    let (done, closed) = watch::channel(false);
    let key = key(path);
    CLOSING.insert(key.clone(), closed.clone());
    // A runtime that has shut down drops the task, and with it the sender: waiters stop waiting,
    // and the files are what a kill would have left, which the next open recovers.
    drop(runtime.spawn(async move {
        // A close that fails leaves the files a kill would; the next open recovers from them.
        let _ = db.close().await;
        CLOSING.remove_if(&key, |_, waiting| waiting.same_channel(&closed));
        let _ = done.send(true);
    }));
}

/// Waits until no store this process dropped is still closing at `path`.
pub(super) async fn settled(path: &Path) {
    // The map's guard ends with this statement, before anything is awaited.
    let waiting = CLOSING.get(&key(path)).map(|entry| entry.value().clone());
    if let Some(mut closed) = waiting {
        // An error means the close was dropped with its runtime: nothing is left to wait for.
        let _ = closed.wait_for(|done| *done).await;
    }
}
