//! A feed over several of one writer's logs, in the order it committed them
//! (G037, Q-791).
//!
//! # Why one position is not enough
//!
//! A split table's single-shard commits are filed in its shards' logs and a
//! commit touching two shards in its database's, so its changes are spread over
//! several logs whose positions count separately. A cursor over them is a
//! position per log, and the order they are delivered in is the writer's, which
//! every record carries (ADR-0084).
//!
//! # What a round may deliver
//!
//! The logs are read one after another, and a commit can land in one already
//! read while another is being read. So a round first reads how far this store
//! has committed, and delivers nothing ordered past it: every commit at or below
//! that version is already in its log, because the version is allocated against
//! the committed one and written in the same batch as the record. A full page
//! proves its log only up to its last record. The rule is the follower's
//! ([`crate::in_writer_order`]); only the source of the bound is local.
//!
//! # Whose logs
//!
//! One writer's. Its order says nothing about another writer's, so a caller
//! merging two writers' logs here would be ordering by two unrelated counters —
//! the caller refuses that rather than this type guessing.

use tessari_encoding::{LogId, LogRecord};
use tessari_types::Sequence;

use crate::error::Result;
use crate::feed::{Change, Watch, changes_in};
use crate::ordering::{Horizon, Page, in_writer_order};
use crate::store::Store;

/// A cursor over several of one writer's logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    logs: Vec<(LogId, Sequence)>,
    watch: Watch,
}

impl Merged {
    /// Follow `logs`, each from its own position, inclusive.
    #[must_use]
    pub const fn new(logs: Vec<(LogId, Sequence)>, watch: Watch) -> Self {
        Self { logs, watch }
    }

    /// Where each log's next read starts.
    #[must_use]
    pub fn positions(&self) -> &[(LogId, Sequence)] {
        &self.logs
    }

    /// The next watched changes, each with the log it was read out of, in the
    /// writer's order.
    ///
    /// Each log advances past every record delivered from it, matched or not,
    /// and past nothing the round held back.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a record cannot be decoded.
    pub fn poll(&mut self, store: &Store, limit: usize) -> Result<Vec<(LogId, Change)>> {
        // Read before the pages: a commit that lands after this is ordered past
        // it and waits for the next round.
        let committed = store.committed_version()?;
        let mut fetched: Vec<Vec<(Sequence, LogRecord)>> = Vec::with_capacity(self.logs.len());
        for (log, from) in &self.logs {
            fetched.push(store.log_records(*log, *from, limit)?);
        }
        let pages: Vec<Page<'_>> = self
            .logs
            .iter()
            .zip(&fetched)
            .map(|((log, _), records)| Page {
                log: *log,
                previous: tessari_types::Epoch::ZERO,
                records,
                horizon: if records.len() >= limit {
                    Horizon::Full
                } else {
                    Horizon::Level(committed)
                },
            })
            .collect();
        let mut found = Vec::new();
        for (index, position) in in_writer_order(&pages) {
            let (Some((log, next)), Some((at, record))) = (
                self.logs.get_mut(index),
                fetched.get(index).and_then(|records| records.get(position)),
            ) else {
                continue;
            };
            for change in changes_in(*at, record)? {
                if self.watch.covers(&change) {
                    found.push((*log, change));
                }
            }
            *next = Sequence::new(at.get().saturating_add(1));
        }
        Ok(found)
    }
}
