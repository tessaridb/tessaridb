//! Transactions at snapshot isolation.
//!
//! A snapshot is one sequence number. Every read in a transaction seeks to that
//! sequence and takes the newest version at or before it, so the transaction
//! sees one consistent point in the store's history however long it runs.
//!
//! # What this level guarantees, and what it does not
//!
//! Guaranteed: reads are consistent as of the snapshot; a transaction sees its
//! own writes; and on a write-write race the first committer wins while the
//! loser writes nothing at all.
//!
//! **Not** guaranteed, and this is the level's defining limitation:
//!
//! - **Write skew.** Two transactions may each read a set, each find an
//!   invariant satisfied, each write a *different* key, and both commit —
//!   leaving the invariant violated with no conflict raised anywhere. Conflict
//!   detection is over what a transaction *wrote*, not over what it *read*.
//!   A caller that needs such an invariant materialises it into a key that both
//!   transactions write, which turns the skew into an ordinary detected
//!   conflict.
//! - **Phantoms.** Detection is per record, so a predicate re-evaluated later
//!   may match records that did not exist at snapshot time.
//!
//! Both are demonstrated by the test suite rather than described only here.

use std::collections::BTreeMap;
use std::ops::Bound;

use bgv_db_constants::MAX_COMMIT_ATTEMPTS;
use bgv_db_encoding::{AppliedPositionKey, RecordKey, RecordValue, StoreKey, StoreValue};
use bgv_db_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use bgv_db_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::error::{Error, Result};
use crate::store::Store;

/// Where a record lives: its table, and its identity within it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordAddress {
    /// The namespace.
    pub namespace: NamespaceId,
    /// The database within the namespace.
    pub database: DatabaseId,
    /// The table within the database.
    pub table: TableId,
    /// The record's identity within the table.
    pub id: RecordId,
}

impl RecordAddress {
    /// Address a record.
    #[must_use]
    pub const fn new(
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        id: RecordId,
    ) -> Self {
        Self {
            namespace,
            database,
            table,
            id,
        }
    }

    fn key_at(&self, version: Sequence) -> RecordKey {
        RecordKey::new(
            self.namespace,
            self.database,
            self.table,
            self.id.clone(),
            version,
        )
    }

    fn versions_prefix(&self) -> Vec<u8> {
        RecordKey::versions_prefix(self.namespace, self.database, self.table, &self.id)
    }
}

/// A unit of work at a fixed snapshot.
#[derive(Debug)]
pub struct Transaction<'a> {
    store: &'a Store,
    snapshot: Sequence,
    writes: BTreeMap<RecordAddress, RecordValue>,
}

impl<'a> Transaction<'a> {
    pub(crate) const fn new(store: &'a Store, snapshot: Sequence) -> Self {
        Self {
            store,
            snapshot,
            writes: BTreeMap::new(),
        }
    }

    /// The sequence every read in this transaction observes.
    ///
    /// Exposed because a snapshot's lifetime is an operational limit: while one
    /// is held, no version newer than it can be reclaimed.
    #[must_use]
    pub const fn snapshot(&self) -> Sequence {
        self.snapshot
    }

    /// Read a record as of this transaction's snapshot.
    ///
    /// Returns `None` when the record does not exist at that point, including
    /// when it was deleted at or before it.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn get(&self, address: &RecordAddress) -> Result<Option<Vec<u8>>> {
        if let Some(pending) = self.writes.get(address) {
            return Ok(match pending {
                RecordValue::Present(payload) => Some(payload.clone()),
                RecordValue::Tombstone => None,
            });
        }
        let found = self.read_at(address, self.snapshot)?;
        Ok(match found {
            Some((_, RecordValue::Present(payload))) => Some(payload),
            Some((_, RecordValue::Tombstone)) | None => None,
        })
    }

    /// Buffer a write. Nothing reaches the store until commit.
    pub fn put(&mut self, address: RecordAddress, payload: Vec<u8>) {
        self.writes.insert(address, RecordValue::Present(payload));
    }

    /// Buffer a delete.
    ///
    /// A delete is a version carrying a tombstone, not an erased key: a reader
    /// at an older snapshot must still see the record.
    pub fn delete(&mut self, address: RecordAddress) {
        self.writes.insert(address, RecordValue::Tombstone);
    }

    /// Discard the transaction.
    ///
    /// Nothing was written, so nothing is undone. Dropping the transaction does
    /// the same thing; this exists to say so at the call site.
    pub fn rollback(self) {
        drop(self);
    }

    /// Commit every buffered write at one new sequence.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Conflict`] when another transaction committed to a
    /// record this one wrote, [`Error::CommitContention`] when every attempt
    /// lost the race for the committed tail, or a substrate error.
    pub fn commit(self) -> Result<Sequence> {
        if self.writes.is_empty() {
            return Ok(self.snapshot);
        }

        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            if attempt > MAX_COMMIT_ATTEMPTS {
                return Err(Error::CommitContention {
                    attempts: MAX_COMMIT_ATTEMPTS,
                });
            }

            let tail = self.store.committed_tail()?;
            self.check_for_conflicts()?;

            let commit_at = Sequence::new(tail.get().saturating_add(1));
            match self
                .store
                .backend()
                .apply(self.commit_batch(tail, commit_at))
            {
                Ok(()) => return Ok(commit_at),
                // The tail moved between reading it and applying, so the
                // conflict check above was made against a stale state and the
                // whole attempt is repeated rather than patched up.
                Err(bgv_db_kv::Error::Conflict { .. }) => continue,
                Err(other) => return Err(other.into()),
            }
        }
    }

    /// Refuse the commit if any written record has moved since the snapshot.
    ///
    /// This is the write-write detection, and it is only sound because the
    /// commit batch asserts the tail has not moved either — together they turn
    /// check-then-write into a compare-and-set over the whole commit.
    fn check_for_conflicts(&self) -> Result<()> {
        for address in self.writes.keys() {
            let Some((version, _)) = self.read_newest(address)? else {
                continue;
            };
            if version > self.snapshot {
                return Err(Error::Conflict {
                    id: address.id.clone(),
                    snapshot: self.snapshot,
                    committed: version,
                });
            }
        }
        Ok(())
    }

    fn commit_batch(&self, expected_tail: Sequence, commit_at: Sequence) -> WriteBatch {
        let applied_key = AppliedPositionKey.encode();
        let mut batch = WriteBatch::new()
            .expect_value(
                AppliedPositionKey::keyspace(),
                applied_key.clone(),
                expected_tail.encode(),
            )
            .put(
                AppliedPositionKey::keyspace(),
                applied_key,
                commit_at.encode(),
            );
        for (address, value) in &self.writes {
            batch = batch.put(
                RecordKey::keyspace(),
                address.key_at(commit_at).encode(),
                value.encode(),
            );
        }
        batch
    }

    /// The newest version of a record, whatever its sequence.
    fn read_newest(&self, address: &RecordAddress) -> Result<Option<(Sequence, RecordValue)>> {
        let prefix = address.versions_prefix();
        self.first_in_range(KeyRange::prefix(&prefix))
    }

    /// The newest version of a record at or before `snapshot`.
    fn read_at(
        &self,
        address: &RecordAddress,
        snapshot: Sequence,
    ) -> Result<Option<(Sequence, RecordValue)>> {
        let prefix = address.versions_prefix();
        let bounds = KeyRange::prefix(&prefix);
        let range = KeyRange::from_bounds(
            Bound::Included(address.key_at(snapshot).encode()),
            bounds.end().clone(),
        );
        self.first_in_range(range)
    }

    fn first_in_range(&self, range: KeyRange) -> Result<Option<(Sequence, RecordValue)>> {
        let request = ScanRequest {
            keyspace: RecordKey::keyspace(),
            range,
            direction: ScanDirection::Forward,
            limit: Some(1),
        };
        let found = self.store.backend().scan(&request)?;
        let Some((key, value)) = found.first() else {
            return Ok(None);
        };
        let decoded_key = RecordKey::decode(key.as_slice())?;
        let decoded_value = RecordValue::decode(value.as_slice())?;
        Ok(Some((decoded_key.version, decoded_value)))
    }
}
