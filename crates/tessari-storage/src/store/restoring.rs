//! Reading a store's state out, and writing one into an empty store (ADR-0091).
//!
//! The reading is [`crate::state`]'s; the entry point is here, beside the
//! writing, so the one list of what reaches past the session to the store holds
//! both halves.
//!
//! A chunk is written the way an applied log record is: validated against the
//! catalog, given the next version of this store, and derived by the same
//! `maintain` functions in the same order — so indexes, adjacency, counts,
//! expiry, eviction and topic entries are rebuilt rather than carried. What an
//! apply also does and this does not is file the record in a log and move that
//! log's position: a restored state has no log below the positions it came
//! from, and those are written once, at the end, by [`Store::finish_state`].
//!
//! The result is the source store as it would stand pruned at those positions,
//! which is a state this engine already has — so a log backup taken from the
//! source `FROM` the next position applies onto it.

use tessari_encoding::{
    AppliedPositionKey, LogId, LogRecord, LogStartKey, ReclaimFloorKey, StoreKey, StoreValue,
    TopicHeadKey,
};
use tessari_kv::WriteBatch;
use tessari_types::Sequence;

use super::Store;
use crate::error::Result;
use crate::state::TopicHead;

impl Store {
    /// Begin reading this store's current state.
    ///
    /// # Errors
    ///
    /// Returns an error when a position or the version cannot be read.
    pub fn read_state(&self) -> Result<crate::state::StateReader<'_>> {
        crate::state::StateReader::open(self, tessari_types::Reach::Store)
    }

    /// Begin reading the part of this store a peer subscribed at `within` is
    /// given: its records by the rule a collect applies, the logs inside or
    /// above it, and the topics inside it.
    ///
    /// # Errors
    ///
    /// Returns an error when a position or the version cannot be read.
    pub fn read_state_within(
        &self,
        within: tessari_types::Reach,
    ) -> Result<crate::state::StateReader<'_>> {
        crate::state::StateReader::open(self, within)
    }

    /// Whether nothing has ever been written into this store.
    ///
    /// # Errors
    ///
    /// Returns an error when the version or the log list cannot be read.
    pub fn holds_nothing(&self) -> Result<bool> {
        Ok(self.committed_version()? == Sequence::ZERO && self.logs()?.is_empty())
    }

    /// Write one chunk of a restored state.
    ///
    /// # Errors
    ///
    /// Returns the store's refusal when a record does not satisfy the catalog
    /// already restored, and a backend error when the batch cannot land.
    pub fn restore_state_chunk(&self, record: &LogRecord) -> Result<()> {
        let _turn = self.writing.hold();
        self.writing.land_all(self.backend.as_ref());
        crate::schema::validate(self, record)?;
        let version = Sequence::new(self.committed_version()?.get().saturating_add(1));
        self.derive_and_land(record, crate::log::state_batch(version, record), version)
    }

    /// Finish a restored state: where each log stood, where its history
    /// starts, and how far each topic had counted.
    ///
    /// Each log stands at the position the state was read at and starts one
    /// past it, so a reader asking for anything below is refused rather than
    /// told the log is level. History below the restore's own last version is
    /// reclaimed by definition — none of it is the source's history — so the
    /// floor is set there and a historical read below it is refused. A topic's
    /// head is raised to the source's and never lowered, because the messages
    /// restored may be fewer than the topic had given.
    ///
    /// # Errors
    ///
    /// Returns a backend error when the batch cannot land.
    pub fn finish_state(
        &self,
        positions: &[(LogId, Sequence)],
        topics: &[TopicHead],
    ) -> Result<()> {
        let _turn = self.writing.hold();
        self.writing.land_all(self.backend.as_ref());
        let mut batch = WriteBatch::new();
        for (log, at) in positions {
            batch = batch
                .put(
                    AppliedPositionKey::keyspace(),
                    AppliedPositionKey::new(*log).encode(),
                    at.encode(),
                )
                .put(
                    LogStartKey::keyspace(),
                    LogStartKey::new(*log).encode(),
                    Sequence::new(at.get().saturating_add(1)).encode(),
                );
        }
        for topic in topics {
            let key = TopicHeadKey {
                namespace: topic.namespace,
                database: topic.database,
                table: topic.table,
            };
            let held = crate::topic::head(self, (topic.namespace, topic.database, topic.table))?;
            if topic.last > held {
                batch = batch.put(
                    TopicHeadKey::keyspace(),
                    key.encode(),
                    Sequence::new(topic.last).encode(),
                );
            }
        }
        batch = batch.put(
            ReclaimFloorKey::keyspace(),
            ReclaimFloorKey.encode(),
            self.committed_version()?.encode(),
        );
        self.writing.apply(batch, self.backend.as_ref())?;
        Ok(())
    }
}
