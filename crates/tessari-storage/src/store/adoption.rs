//! A first leader's own history becomes the start of the line's (ADR-0107 D3).
//!
//! A commit made under no leadership — a store standing alone, a node's own
//! declarations before it joins — files into the node's own log. A commit made
//! under one files into the line's one log, which every later leader continues.
//! The seam between the two is the first lease: whatever the first leader wrote
//! before it led has to reach its followers through the one log they follow, so
//! at that moment its own log of each single-leader home is MOVED into the
//! line's — moved, so the history exists once and in one place.
//!
//! A home whose line log already holds records adopts nothing: that node is a
//! later leader, or a follower that collected the line, and its own pre-cluster
//! records are its own (they never were the cluster's, which is why a joining
//! node's tenancy is refused rather than merged).

use tessari_encoding::{AppliedPositionKey, LogId, LogKey, LogStartKey, StoreKey, StoreValue};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::Sequence;

use crate::error::Result;

use super::Store;

impl Store {
    /// Move this node's own log of every single-leader home whose line log is
    /// still empty into the line's log, and answer how many homes moved.
    ///
    /// Run whenever a lease is taken, and a no-op once it has run: a moved
    /// log is empty, and a line log that holds records is never touched. One
    /// batch per home under the write gate, so a commit or an apply cannot
    /// interleave with a log being renamed under it.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, and a decoding failure for a log key
    /// that does not hold a position.
    pub(crate) fn adopt_own_history(&self) -> Result<usize> {
        let own = self.writer()?;
        let mut moved = 0_usize;
        for log in self.logs()? {
            if log.writer == own && self.adopt_into_line(log)? {
                moved = moved.saturating_add(1);
            }
        }
        Ok(moved)
    }

    /// Move `log` into its home's line log if that line log is empty, and say
    /// whether it moved.
    ///
    /// The follower's half of adoption as well as the leader's: a follower that
    /// has been collecting a leader's own log meets that leader's first line
    /// answer after the leader adopted the same log, and renaming its copy the
    /// same way lets positions continue with no gap on either side.
    ///
    /// # Errors
    ///
    /// The same as [`Self::adopt_own_history`].
    pub fn adopt_into_line(&self, log: LogId) -> Result<bool> {
        let line = LogId::line(log.home);
        if log == line {
            return Ok(false);
        }
        let _turn = self.writing.hold();
        self.writing.land_all(self.backend.as_ref());
        let tail = self.committed_tail(log)?;
        if tail == Sequence::ZERO || self.committed_tail(line)? != Sequence::ZERO {
            return Ok(false);
        }
        // Last, because it is the one question that costs a catalog read: a
        // range that admits two writers keeps one log per writer.
        if self.admits_two_writers(log.home)? {
            return Ok(false);
        }
        self.backend.apply(self.renamed(log, line, tail)?)?;
        Ok(true)
    }

    /// The batch that files every key of `log` under `line` instead.
    fn renamed(&self, log: LogId, line: LogId, tail: Sequence) -> Result<WriteBatch> {
        let mut batch = WriteBatch::new();
        let entries = ScanRequest {
            keyspace: LogKey::keyspace(),
            range: KeyRange::prefix(&LogKey::prefix_for(log)),
            direction: ScanDirection::Forward,
            limit: None,
        };
        for (key, value) in self.backend.scan(&entries)? {
            let at = LogKey::decode(key.as_slice())?.sequence;
            batch = batch.delete(LogKey::keyspace(), key).put(
                LogKey::keyspace(),
                LogKey::new(line, at).encode(),
                value,
            );
        }
        batch = batch
            .delete(
                AppliedPositionKey::keyspace(),
                AppliedPositionKey::new(log).encode(),
            )
            .put(
                AppliedPositionKey::keyspace(),
                AppliedPositionKey::new(line).encode(),
                tail.encode(),
            );
        // A pruned log carries where it now begins, and the line's log begins
        // there too (ADR-0079): leaving it behind would make a follower asking
        // below it read records that are gone as records that were never there.
        let start = LogStartKey::new(log).encode();
        if let Some(begins) = self.backend.get(LogStartKey::keyspace(), &start)? {
            batch = batch.delete(LogStartKey::keyspace(), start).put(
                LogStartKey::keyspace(),
                LogStartKey::new(line).encode(),
                begins,
            );
        }
        Ok(batch)
    }
}
