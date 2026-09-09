//! Opening a transaction, and reading one record through its snapshot.
//!
//! Every read here resolves the same way — seek to the snapshot sequence, take
//! the newest version at or before it — and the transaction's own writes are
//! consulted first, so it sees what it has done.

use std::collections::BTreeMap;
use std::ops::Bound;

use tessari_encoding::{RecordKey, RecordValue, StoreKey, StoreValue};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, Sequence, TableId};

use super::{RecordAddress, Transaction};
use crate::error::Result;
use crate::store::Store;

impl<'a> Transaction<'a> {
    pub(crate) fn new(store: &'a Store, snapshot: Sequence) -> Self {
        // Registered here rather than by the caller, so that a snapshot cannot
        // be read from without the store knowing it is being read from.
        store.snapshot_registry().register(snapshot);
        Self {
            store,
            snapshot,
            writes: BTreeMap::new(),
            reading_at: std::cell::Cell::new(None),
            floors: std::cell::RefCell::new(BTreeMap::new()),
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

    /// The store this transaction runs against.
    ///
    /// `pub(crate)` and not part of the public surface: everything above this
    /// crate already holds the store it opened the transaction from, and the one
    /// caller here is the sealing path, which needs the per-process keyring that
    /// lives on the store rather than in the log.
    pub(crate) const fn store(&self) -> &Store {
        self.store
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
        // A series table's floor is applied here rather than in each read that
        // ends in a record, because this is where every one of them ends: a
        // point read, an index-served read, a search, a nearest-first walk and a
        // graph hop all resolve their identities through this method. Gating
        // them one by one is the enforcement-coverage failure where the path
        // added next month is the one nobody remembers.
        if self.below_series_floor(address)? {
            return Ok(None);
        }
        self.get_uncovered(address)
    }

    /// [`Self::get`] without the retention floor.
    ///
    /// Reserved for the catalog, which must be able to read the declaration that
    /// says where the floor is.
    pub(super) fn get_uncovered(&self, address: &RecordAddress) -> Result<Option<Vec<u8>>> {
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

    /// Read several records as of this transaction's snapshot, in one ask.
    ///
    /// Answers exactly as [`Self::get`] called on each address would — pending
    /// writes folded in the same way, tombstones absent the same way — and
    /// returns one entry per address, in order.
    ///
    /// It exists because resolving the records an index names is the place
    /// where the cost of reading one record is multiplied by the size of an
    /// answer. Each record read is a bounded range rather than a point lookup,
    /// because records are versioned and the visible one is the newest at or
    /// below the snapshot; asking for those ranges together lets a backend set
    /// up once instead of once per record.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn get_each(&self, addresses: &[RecordAddress]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut answers: Vec<Option<Vec<u8>>> = vec![None; addresses.len()];
        // Tested before anything is asked of the backend, so a batch of
        // identities entirely below the floor costs no read at all.
        let mut covered = vec![false; addresses.len()];
        for (index, address) in addresses.iter().enumerate() {
            covered[index] = self.below_series_floor(address)?;
        }
        let mut ranges = Vec::new();
        let mut asked = Vec::new();
        for (index, address) in addresses.iter().enumerate() {
            if covered[index] {
                continue;
            }
            if let Some(pending) = self.writes.get(address) {
                if let RecordValue::Present(payload) = pending {
                    answers[index] = Some(payload.clone());
                }
                continue;
            }
            let bounds = KeyRange::prefix(&address.versions_prefix());
            ranges.push(KeyRange::from_bounds(
                Bound::Included(address.key_at(self.snapshot).encode()),
                bounds.end().clone(),
            ));
            asked.push(index);
        }

        let found = self
            .store
            .backend()
            .first_of_each(RecordKey::keyspace(), &ranges)?;
        for (index, pair) in asked.into_iter().zip(found) {
            let Some((_, value)) = pair else { continue };
            if let RecordValue::Present(payload) = RecordValue::decode(value.as_slice())? {
                answers[index] = Some(payload);
            }
        }
        Ok(answers)
    }

    /// Whether an index may answer a read taken through this transaction.
    ///
    /// An index entry is `<kind> <tenancy> <index> <values> <0x00> <record-id>`
    /// and carries **no version**. Entries are derived at commit, so an index
    /// describes the committed tail and nothing else. Consulted from a
    /// transaction whose snapshot is behind that tail, it produces two different
    /// wrong answers from the one cause:
    ///
    /// - a record that matched at the snapshot and has been updated since has no
    ///   entry under its old value, so it is **missing** from the answer;
    /// - a record that matches now but did not then has an entry, is resolved at
    ///   the snapshot, and comes back **not satisfying the condition it was
    ///   selected by**.
    ///
    /// Neither raises anything, which is why this is a method and not a rule
    /// each caller remembers. Every read that would be served from an index asks
    /// here first, and a `false` means take the scan.
    ///
    /// This is about the snapshot's *position*, not about how it was opened: a
    /// transaction begun at the tail that is still running while somebody else
    /// commits has fallen behind, and its indexes are stale in exactly the same
    /// way as a deliberately historical one's.
    ///
    /// # Errors
    ///
    /// Returns an error when the committed tail cannot be read or decoded.
    pub fn indexes_are_current(&self) -> Result<bool> {
        Ok(self.snapshot == self.store.committed_tail()?)
    }

    /// Whether this transaction has written to one table without committing.
    ///
    /// Asked by a read that would otherwise be served from an index: an
    /// uncommitted record has no entry, because entries are derived at commit,
    /// so an ordering served from the index would place it nowhere.
    #[must_use]
    pub fn writes_in(&self, namespace: NamespaceId, database: DatabaseId, table: TableId) -> bool {
        self.writes.keys().any(|address| {
            address.namespace == namespace && address.database == database && address.table == table
        })
    }

    /// The newest version of a record, whatever its sequence.
    pub(super) fn read_newest(
        &self,
        address: &RecordAddress,
    ) -> Result<Option<(Sequence, RecordValue)>> {
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
