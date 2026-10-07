//! Opening a transaction, and reading one record through its snapshot.
//!
//! Every read here resolves the same way — seek to the snapshot sequence, take
//! the newest version at or before it — and the transaction's own writes are
//! consulted first, so it sees what it has done.

mod versions;

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ops::Bound;

use tessari_encoding::{
    CausalStamp, CausalVersions, NODE_ID_LEN, RecordKey, RecordValue, StampedValue, StoreKey,
    StoreValue,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, Sequence, TableId};

use super::{RecordAddress, Transaction};
use crate::error::Result;
use crate::store::Store;

impl<'a> Transaction<'a> {
    pub(crate) fn new(store: &'a Store, snapshot: Sequence) -> Self {
        // Registered here rather than by the caller, so that a snapshot cannot
        // be read from without the store knowing it is being read from.
        let registered = store.snapshot_registry().register(snapshot);
        Self {
            store,
            snapshot,
            registered,
            writes: BTreeMap::new(),
            expiring: BTreeMap::new(),
            lifetimes: BTreeMap::new(),
            reading_at: std::cell::Cell::new(None),
            floors: std::cell::RefCell::new(BTreeMap::new()),
            guarded: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            across: None,
            decided: std::cell::RefCell::new(BTreeMap::new()),
            asks_leaders: true,
        }
    }

    /// A view that decides every intent from this node's own copy and never
    /// asks a peer — what a commit or an apply derives from, under the write
    /// turn, where a question to a peer would wait on the network while that
    /// peer may be waiting for this very write (ADR-0112 D13d is for readers).
    pub(crate) fn local(mut self) -> Self {
        self.asks_leaders = false;
        self
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

    /// A record as it is **held**, an expired version included (G035).
    ///
    /// For the code that keeps a derived structure — an index, a count, an
    /// adjacency list — in step with the records. Those structures hold entries
    /// for every stored version until the version is removed, so the removal must
    /// be computed against what is stored and never against what a reader is
    /// shown: asking [`Self::get`] would report an expired record as absent, and
    /// overwriting or deleting it would then leave its entries behind for ever.
    /// The series floor still applies, exactly as it does for [`Self::get`].
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn get_held(&self, address: &RecordAddress) -> Result<Option<Vec<u8>>> {
        if self.below_series_floor(address)? {
            return Ok(None);
        }
        if let Some(pending) = self.writes.get(address) {
            return Ok(match pending {
                RecordValue::Present(payload) => Some(payload.clone()),
                RecordValue::Tombstone => None,
            });
        }
        Ok(
            match self.read_stamped_at(address)?.map(StampedValue::into_value) {
                Some(RecordValue::Present(payload)) => Some(payload),
                Some(RecordValue::Tombstone) | None => None,
            },
        )
    }

    /// Whether this transaction has written the record itself.
    pub(crate) fn has_written(&self, address: &RecordAddress) -> bool {
        self.writes.contains_key(address)
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
            let mut stored = StampedValue::decode(value.as_slice())?;
            if self.passes_over(&stored)? {
                // The rare record a transaction across leaders wrote that this
                // one does not see: read the version under, one at a time.
                let Some(under) = self.read_stamped_at(&addresses[index])? else {
                    continue;
                };
                stored = under;
            }
            if let RecordValue::Present(payload) = stored.into_visible_at(self.reading_at()) {
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
    /// Returns an error when the committed version cannot be read or decoded.
    pub fn indexes_are_current(&self) -> Result<bool> {
        // The **version**, not the log position. A snapshot is a record version
        // — this store's own number — and comparing it against a log position
        // was one comparison between two scales that happened to hold the same
        // value while a single leader decided every write (Q-623). It stops
        // holding the moment positions go per-home, and the answer it would give
        // is `false` for every transaction, which silently takes the scan on
        // every index-served read.
        Ok(self.snapshot == self.store.committed_version()?)
    }

    /// Whether an index of one table may answer a read taken through this
    /// transaction: [`Transaction::indexes_are_current`], and no transaction
    /// across leaders part-way in the table here (Q-919).
    ///
    /// Between a committed transaction's parts landing here, an index may hold
    /// a resolution its readers cannot see yet, or miss an intent they already
    /// do; either way it would answer about records the read itself would not
    /// show. The answer is about the present, as the position check is.
    ///
    /// # Errors
    ///
    /// Returns an error when the committed version or the mark cannot be read.
    pub fn indexes_are_current_for(&self, table: TableId) -> Result<bool> {
        Ok(self.indexes_are_current()? && !self.store.across_unsettled(table)?)
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
}

/// One surviving version of a record, and the node that wrote it.
///
/// Named rather than spelled out at the two places it appears, so that the pair
/// reads as one fact instead of as a tuple whose halves a caller has to
/// re-derive the meaning of.
pub type WrittenVersion = (Sequence, [u8; NODE_ID_LEN]);

/// The node that wrote one version, derived against the version it descends.
///
/// See [`Transaction::surviving_writers`] for why this cannot be read off a
/// single stamp. `unwrap_or_default` where no node distinguishes the two is the
/// unstamped case — a store written before stamps existed carries the empty
/// stamp everywhere, and every comparison over it is `Same`.
fn writer_of(
    stamp: &CausalStamp,
    held: &[(Sequence, CausalStamp)],
    version: Sequence,
) -> [u8; NODE_ID_LEN] {
    let base = held
        .iter()
        .filter(|(at, other)| *at < version && other != stamp && stamp.descends(other))
        .max_by_key(|(at, _)| *at)
        .map(|(_, other)| other);
    match base {
        Some(base) => stamp
            .entries()
            .iter()
            .find(|(node, count)| *count > base.count(node))
            .map_or_else(<[u8; NODE_ID_LEN]>::default, |(node, _)| *node),
        None => stamp
            .entries()
            .iter()
            .find(|(_, count)| *count > 0)
            .map_or_else(<[u8; NODE_ID_LEN]>::default, |(node, _)| *node),
    }
}
