//! Opening a transaction, and reading one record through its snapshot.
//!
//! Every read here resolves the same way — seek to the snapshot sequence, take
//! the newest version at or before it — and the transaction's own writes are
//! consulted first, so it sees what it has done.

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
            if let RecordValue::Present(payload) =
                StampedValue::decode(value.as_slice())?.into_value()
            {
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
        Ok(self
            .read_newest_stamped(address)?
            .map(|(version, stamped)| (version, stamped.into_value())))
    }

    /// The same version, with the causal context its writer had seen.
    ///
    /// What the commit path's stamp producer reads. A write carries forward
    /// every count the version it replaces held and raises only its own, and
    /// that carrying is the entire reason a later comparison can tell ignorance
    /// from sequence — a producer that started from an empty stamp would make
    /// every write concurrent with every other one.
    pub(super) fn read_newest_stamped(
        &self,
        address: &RecordAddress,
    ) -> Result<Option<(Sequence, StampedValue)>> {
        let prefix = address.versions_prefix();
        self.first_in_range(KeyRange::prefix(&prefix))
    }

    /// The versions of a record that nothing has superseded, newest first.
    ///
    /// One whenever the record is settled, which is every record on a
    /// single-leader range and most records on a multi-master one. More than
    /// one means two nodes wrote it without seeing each other, and that is the
    /// state a commit is refused on rather than resolved (ADR-0075, G027 S3.1).
    ///
    /// # Every version, and not a walk that stops early
    ///
    /// Stopping at the first version the newest descends is O(1) and **wrong**:
    /// with versions `{B:1}`, `{A:1}`, `{A:2}` the newest descends the one
    /// before it and the walk stops, while `{B:1}` is concurrent with it and
    /// survives. The scan is bounded by the versions above the reclaim floor —
    /// which `crate::reclaim` already manages and which is a function of
    /// snapshot lifetime, not of how long the record has existed.
    ///
    /// # Supersession is decided in one place
    ///
    /// [`CausalVersions::record`] owns the rule and this folds every stamp
    /// through it, then maps the survivors back to the versions they came from.
    /// Re-implementing the two lines of that rule here is how two routines
    /// answering one question come to disagree, and the disagreement would be a
    /// live version quietly dropped or a superseded one quietly refused over.
    ///
    /// An unstamped version carries the empty stamp, and two empty stamps are
    /// `Same` — so a store written before stamps existed folds to exactly one
    /// survivor and is never contested.
    pub(super) fn surviving_versions(
        &self,
        address: &RecordAddress,
    ) -> Result<Vec<(Sequence, CausalStamp)>> {
        let held = self.held_versions(address)?;
        let mut surviving = CausalVersions::new();
        for (_, stamp) in &held {
            surviving.record(stamp.clone());
        }
        Ok(surviving
            .stamps()
            .iter()
            .filter_map(|stamp| {
                held.iter()
                    .find(|(_, candidate)| candidate == stamp)
                    .map(|(version, _)| (*version, stamp.clone()))
            })
            .collect())
    }

    /// Every surviving version of a record, each with the node that wrote it,
    /// and whether they are contested (G027 S4.3).
    ///
    /// # The path that returns a version a read resolved away
    ///
    /// A record with two survivors answers every ordinary read with the newest
    /// of them, and the other is still on disk, byte-intact, reachable by
    /// nothing. An operator auditing for data loss finds both versions and
    /// concludes nothing was lost — the bytes are there, and what is missing is
    /// any path that returns them. This is that path.
    ///
    /// # Which node wrote a version, and why it cannot be read off one stamp
    ///
    /// A stamp counts writes per node, so it says what a version has SEEN and
    /// not who wrote it. `{A:5, B:1}` was written by B, which had seen all five
    /// of A's writes, and the largest entry is A's — so "the node with the
    /// highest count" is wrong in exactly the two-master case this exists for.
    ///
    /// The derivation is comparative: the writer of a version is the node whose
    /// count exceeds its count in the newest version this one descends. A first
    /// version descends nothing, and its writer is the only node with a count at
    /// all. This is the same comparison `refuse_a_contested_record` makes to
    /// name the unseen node, so the report and the refusal cannot name different
    /// nodes for one version.
    ///
    /// # The contested flag is not computed here
    ///
    /// [`CausalVersions::is_contested`] answers it, for the reason
    /// [`Self::surviving_versions`] folds through [`CausalVersions::record`]:
    /// two routines answering one question come to disagree, and here the
    /// disagreement would be a contested record reported as settled.
    pub fn surviving_writers(
        &self,
        address: &RecordAddress,
    ) -> Result<(Vec<WrittenVersion>, bool)> {
        let held = self.held_versions(address)?;
        let mut surviving = CausalVersions::new();
        for (_, stamp) in &held {
            surviving.record(stamp.clone());
        }
        let mut answered = Vec::new();
        for stamp in surviving.stamps() {
            let Some((version, _)) = held.iter().find(|(_, candidate)| candidate == stamp) else {
                continue;
            };
            answered.push((*version, writer_of(stamp, &held, *version)));
        }
        answered.sort_by_key(|(at, _)| Reverse(*at));
        Ok((answered, surviving.is_contested()))
    }

    /// Every version of a record above the reclaim floor, with its stamp.
    ///
    /// Extracted so that the survivor walk and the writer walk read one scan
    /// rather than two that could drift apart.
    fn held_versions(&self, address: &RecordAddress) -> Result<Vec<(Sequence, CausalStamp)>> {
        let prefix = address.versions_prefix();
        let request = ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };
        let mut held: Vec<(Sequence, CausalStamp)> = Vec::new();
        for (key, value) in self.store.backend().scan(&request)? {
            let version = RecordKey::decode(key.as_slice())?.version;
            let stamp = StampedValue::decode(value.as_slice())?.stamp().clone();
            held.push((version, stamp));
        }
        Ok(held)
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
        Ok(self
            .first_in_range(range)?
            .map(|(version, stamped)| (version, stamped.into_value())))
    }

    /// The newest entry in a span of one record's versions, stamp and all.
    ///
    /// Decodes as [`StampedValue`] rather than as `RecordValue` because that is
    /// what the store holds: `RecordValue::decode` allows only the tombstone
    /// flag and *refuses* a stamped value outright, so a reader that took the
    /// narrower type would start failing the day commits began carrying a
    /// stamp. One decode site for the reason the codec gives for its splitters
    /// — two readings of one byte string is a thing that can come to disagree.
    fn first_in_range(&self, range: KeyRange) -> Result<Option<(Sequence, StampedValue)>> {
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
        let decoded_value = StampedValue::decode(value.as_slice())?;
        Ok(Some((decoded_key.version, decoded_value)))
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
