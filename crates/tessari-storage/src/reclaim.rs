//! Removing versions no live reader can still need.
//!
//! # Why this is ours to do
//!
//! An engine that carries its own per-key timestamps can be told a watermark and
//! left to drop old versions during compaction — the store advances a number and
//! the engine does the work. This store cannot delegate that, because its version
//! is a **suffix in its own key** (`docs/key-grammar.md` §5): to the engine below,
//! two versions of one record are two unrelated keys. The cost of owning the key
//! grammar is owning the reclamation, and this module is that cost.
//!
//! # The rule, and the one way to get it wrong
//!
//! For each record: keep the newest version at or below the floor, and keep
//! everything newer. Remove what is strictly older.
//!
//! Keeping the newest-at-or-below-floor version is what makes a reader at the
//! floor still resolve correctly — it is the version that reader sees. Removing
//! it is the failure mode, and it raises nothing: the read simply returns an
//! older value, or none.
//!
//! A surviving tombstone is removed too, along with everything under it. A reader
//! that finds a tombstone and a reader that finds nothing both conclude the record
//! is not there, so the answer is unchanged — and a deleted record stops costing
//! space, which is the whole point of deleting it.

use tessari_encoding::{
    ReclaimFloorKey, RecordKey, RecordValue, StampedValue, StoreKey, StoreValue,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::error::Result;
use crate::store::Store;

/// What a reclamation pass removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reclaimed {
    /// Versions removed because something newer is visible at the floor.
    pub versions: usize,
    /// Records whose last trace was a tombstone, now gone entirely.
    pub records: usize,
    /// The floor the pass ran at.
    pub floor: Sequence,
}

impl Store {
    /// Remove the versions of one table that no live reader can still need.
    ///
    /// The floor is [`Store::retention_floor`], so a transaction that is still
    /// reading holds this back — that is the point of the registry, and it means
    /// a long-held snapshot postpones reclamation for the whole store.
    ///
    /// There is no schedule here and no background thread. When to call this is
    /// an operational decision that wants a measurement, not a constant chosen
    /// while writing the code.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored key or value cannot
    /// be decoded.
    pub fn reclaim_table(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Reclaimed> {
        let floor = self.retention_floor()?;
        let prefix = RecordKey::table_prefix(namespace, database, table);
        let request = ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        };

        let mut batch = WriteBatch::new();
        let mut removed = Reclaimed {
            floor,
            ..Reclaimed::default()
        };
        // Versions of one record are adjacent and sort newest-first, so one
        // forward pass sees each record's versions in descending order and the
        // first one at or below the floor is the one a reader there resolves to.
        let mut current: Option<RecordId> = None;
        let mut kept_for_current = false;

        for (key, value) in self.backend().scan(&request)? {
            let decoded = RecordKey::decode(key.as_slice())?;
            if current.as_ref() != Some(&decoded.id) {
                current = Some(decoded.id.clone());
                kept_for_current = false;
            }
            if decoded.version > floor {
                // A reader between this version and the floor still needs it.
                continue;
            }
            if !kept_for_current {
                kept_for_current = true;
                // The version every reader at the floor resolves to. It survives
                // — unless it says the record is gone, in which case removing it
                // gives every one of those readers the same answer for less
                // space.
                if matches!(
                    StampedValue::decode(value.as_slice())?.into_value(),
                    RecordValue::Tombstone
                ) {
                    batch = batch.delete(RecordKey::keyspace(), key);
                    removed.records = removed.records.saturating_add(1);
                    removed.versions = removed.versions.saturating_add(1);
                }
                continue;
            }
            // Strictly older than the version visible at the floor.
            batch = batch.delete(RecordKey::keyspace(), key);
            removed.versions = removed.versions.saturating_add(1);
        }

        if removed.versions > 0 {
            // In the same batch as the removals, for the reason the applied
            // position is in the same batch as the state it describes: a floor
            // written afterwards is a floor that a crash can lose, leaving a
            // store that has forgotten history and does not know it.
            //
            // Raised rather than set. A pass can run at a *lower* floor than an
            // earlier one — a long-held snapshot holds the floor down — and what
            // this records is the oldest sequence still answerable exactly,
            // which only ever moves forward as versions are removed.
            let raised = self.reclaim_floor()?.max(floor);
            batch = batch.put(
                ReclaimFloorKey::keyspace(),
                ReclaimFloorKey.encode(),
                raised.encode(),
            );
            self.backend().apply(batch)?;
        }
        Ok(removed)
    }

    /// The oldest sequence a read can still be answered at exactly.
    ///
    /// [`Sequence::ZERO`] when nothing has ever been reclaimed, which is every
    /// store until reclamation is scheduled — and which is why a historical read
    /// works at all today.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or the stored floor cannot be
    /// decoded.
    pub fn reclaim_floor(&self) -> Result<Sequence> {
        let key = ReclaimFloorKey.encode();
        match self.backend().get(ReclaimFloorKey::keyspace(), &key)? {
            Some(value) => Ok(Sequence::decode(value.as_slice())?),
            None => Ok(Sequence::ZERO),
        }
    }
}
