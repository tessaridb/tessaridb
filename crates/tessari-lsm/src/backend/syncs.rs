//! One sync of the write-ahead log for every write it covers (ADR-0112 D13k).
//!
//! A node writes in two ways: its own commits, synced as they are written,
//! and a follower's applies of its leaders' records, written unsynced and made
//! durable once per round. A node that leads one range and follows another
//! does both at once, and every apply round paid a sync of its own — a second
//! device sync beside the commit's, measured on the caller's path of a
//! transaction across leaders.
//!
//! # Why a sync can be shared, and when
//!
//! A synced write syncs EVERY live WAL file once its own records are appended
//! (`DBImpl::WriteToWAL`, RocksDB 11.8.1: `for (auto& log : logs_) ...
//! Sync(...)`). So a write whose append completed before a synced write BEGAN
//! is durable once that synced write returns — and so is everything a flush of
//! the WAL began after. Writes are counted as their appends complete, under
//! the backend's write lock, so the count is an order: a round needs a flush
//! only when some write it covers landed after the last sync began.
//!
//! # After a failed sync, nothing is durable again
//!
//! A sync that failed may have lost the pages it was given; the device saying
//! yes to the next one proves nothing about them. The failure is kept, and
//! every later round is refused rather than flushed again.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use tessari_kv::{Error, Result};

use crate::error::BACKEND_NAME;

/// What has landed in the WAL, and how much of it is known synced.
#[derive(Debug, Default)]
pub(super) struct WalSyncs {
    /// Writes whose WAL append has completed, counted in order.
    landed: AtomicU64,
    /// The count of landed writes a sync is known to cover.
    synced: AtomicU64,
    /// Held by a round that flushes, so a round arriving meanwhile waits for
    /// that flush and finds itself covered instead of flushing again.
    flushing: Mutex<()>,
    /// Whether a flush has failed; nothing is durable after it.
    failed: AtomicBool,
}

impl WalSyncs {
    /// How many writes have landed so far — read BEFORE a write begins, under
    /// the write lock, so it counts exactly the writes that completed first.
    pub(super) fn landed_so_far(&self) -> u64 {
        self.landed.load(Ordering::Acquire)
    }

    /// Record that a write landed, which began when `before` writes had; a
    /// `synced` write covers every one of those.
    pub(super) fn landed(&self, before: u64, synced: bool) {
        self.landed.fetch_add(1, Ordering::AcqRel);
        if synced {
            self.synced.fetch_max(before, Ordering::AcqRel);
        }
    }

    /// Whether a sync already covers the first `target` landed writes.
    pub(super) fn covered(&self, target: u64) -> bool {
        self.synced.load(Ordering::Acquire) >= target
    }

    /// Make the first `target` landed writes durable, calling `flush` only
    /// when no sync that began after them has already covered them.
    ///
    /// # Errors
    ///
    /// The flush's own failure, and the same refusal for every later round.
    pub(super) fn sync_through(
        &self,
        target: u64,
        flush: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(refused());
        }
        if self.covered(target) {
            return Ok(());
        }
        let _flushing = self.flushing.lock().unwrap_or_else(PoisonError::into_inner);
        if self.failed.load(Ordering::Acquire) {
            return Err(refused());
        }
        // Covered by a flush that ran while this round waited.
        if self.covered(target) {
            return Ok(());
        }
        let covers = self.landed.load(Ordering::Acquire);
        if let Err(failure) = flush() {
            self.failed.store(true, Ordering::Release);
            return Err(failure);
        }
        self.synced.fetch_max(covers, Ordering::AcqRel);
        Ok(())
    }
}

/// The refusal every round meets after a sync has failed.
fn refused() -> Error {
    Error::Backend {
        backend: BACKEND_NAME,
        reason: "a sync of the write-ahead log failed earlier, so nothing written since can \
                 be made durable; restart the node to recover from the log"
            .to_owned(),
        source: None,
    }
}

#[cfg(test)]
mod tests;
