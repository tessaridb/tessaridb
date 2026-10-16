//! What a removed plain index entry said, kept for the snapshots older than its
//! removal (Q-S3-34, format 8).
//!
//! # The defect this closes
//!
//! A secondary entry carries no version: it describes the committed tail, and
//! the commit that changes a record's value deletes the entry for the value it
//! replaced. A transaction whose snapshot is older than that commit can then no
//! longer find, through the index, a record that held the value at its
//! snapshot. The store answered that by withholding every index from a
//! transaction behind the tail (`Transaction::indexes_are_current`) — correct,
//! and with concurrent writers it meant every multi-statement transaction read
//! by scanning, and every `USING INDEX` in one was refused.
//!
//! # What is kept, and why it is enough
//!
//! The commit that removes a plain entry also writes a **retired** entry: the
//! same values and record under key kind `0x45`, stamped in the key with the
//! version that removed it. For a reader at snapshot `S`, an entry that existed
//! at `S` is now either still live or retired at a version above `S` — its last
//! removal came after `S`, because it existed there. So the live entries plus
//! the retired ones stamped above `S` are a SUPERSET of the records that matched
//! at `S`, and resolving each candidate at `S` and re-testing the condition the
//! read asked narrows it to exactly the scan's answer. What the index changes is
//! the cost, never the answer, which is the store's central invariant.
//!
//! Only a plain index keeps them ([`IndexDefinition::keeps_retired_entries`]).
//! A unique entry's key IS the value it claims, so a removed claim cannot sit
//! beside the live one; search, spatial, containment and vector entries have
//! shapes of their own. Those indexes keep the older guard.
//!
//! # Since when
//!
//! Retired entries are complete only for removals after the store began writing
//! them: a store created at format 8 from its first version, an older store from
//! the commit that finalized it to 8, or the open that raised its stamp there.
//! That version is kept as [`RetiredSinceKey`]; a snapshot older than it, or a
//! store without the key, reads as before.
//!
//! # Removal
//!
//! A retired entry stamped at or below the retention floor — the oldest live
//! snapshot — is needed by no reader and is removed by
//! [`Store::reclaim_retired_entries`], on the node's housekeeping cadence. It is
//! this node's own storage work, like every other derived structure: each node
//! retires and reclaims over its own copy.

use std::collections::BTreeSet;
use tessari_kv::AsyncKvBackend as _;

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{
    FormatVersion, IndexAddress, IndexValues, KeyKind, NoPayload, RetiredIndexKey, RetiredSinceKey,
    StoreKey, StoreValue,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{RecordId, Sequence, TableId};

use crate::catalog::IndexDefinition;
use crate::error::Result;
use crate::store::Store;
use crate::transaction::Transaction;

impl Store {
    /// The version from which this store keeps retired index entries, or `None`
    /// when it keeps none (a store below format 8).
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, or a decoding failure for the value.
    pub async fn retired_since(&self) -> Result<Option<Sequence>> {
        match self
            .backend()
            .get(RetiredSinceKey::keyspace(), &RetiredSinceKey.encode())
            .await?
        {
            Some(bytes) => Ok(Some(Sequence::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// Record that this store keeps retired entries from now on, when it holds
    /// format 8 and has not recorded it yet — a store created by this build, or
    /// one whose stamp was raised at open.
    ///
    /// Asked at the end of every open, so whichever path raised the stamp, the
    /// first commit after it already retires.
    pub(crate) async fn begin_keeping_retired_entries(&self) -> Result<()> {
        if self.held_format()? < FormatVersion::RETIRED_INDEX
            || self.retired_since().await?.is_some()
        {
            return Ok(());
        }
        let since = self.committed_version()?;
        self.backend()
            .apply(WriteBatch::new().put(
                RetiredSinceKey::keyspace(),
                RetiredSinceKey.encode(),
                since.encode(),
            ))
            .await?;
        Ok(())
    }

    /// Remove the retired entries no live reader can still need: those stamped
    /// at or below the retention floor, raising [`RetiredSinceKey`] to it first.
    ///
    /// Answers how many were removed. A pass that runs late, or never, costs
    /// space and never an answer — a surplus retired entry is one more candidate
    /// the re-test discards.
    ///
    /// # Errors
    ///
    /// Returns the substrate's failure, or a decoding failure for a key.
    pub async fn reclaim_retired_entries(&self) -> Result<usize> {
        let Some(since) = self.retired_since().await? else {
            return Ok(0);
        };
        let floor = self.retention_floor()?;
        // First, and before anything is removed: the entries about to go are
        // the ones a snapshot below the floor would need, so from here on a
        // read below the floor — a `VERSION` read, which may open one — is not
        // served from them. Raised before the removals so a crash between the
        // two leaves the conservative state: entries kept, none trusted below.
        if floor > since {
            self.backend()
                .apply(WriteBatch::new().put(
                    RetiredSinceKey::keyspace(),
                    RetiredSinceKey.encode(),
                    floor.encode(),
                ))
                .await?;
        }
        let tag = [KeyKind::RetiredIndex.tag()];
        let mut range = KeyRange::prefix(&tag);
        let mut removed = 0_usize;
        loop {
            let request = ScanRequest {
                keyspace: RetiredIndexKey::keyspace(),
                range: range.clone(),
                direction: ScanDirection::Forward,
                limit: Some(RANGE_SCAN_BATCH_ENTRIES),
            };
            let entries = self.backend().scan(&request).await?;
            let mut batch = WriteBatch::new();
            let mut doomed = 0_usize;
            for (key, _) in &entries {
                if RetiredIndexKey::decode(key.as_slice())?.retired_at <= floor {
                    batch = batch.delete(RetiredIndexKey::keyspace(), key.clone());
                    doomed = doomed.saturating_add(1);
                }
            }
            if doomed > 0 {
                self.backend().apply(batch).await?;
                removed = removed.saturating_add(doomed);
            }
            let Some((last, _)) = entries
                .last()
                .filter(|_| entries.len() >= RANGE_SCAN_BATCH_ENTRIES)
            else {
                return Ok(removed);
            };
            range = range.resuming_after(last);
        }
    }
}

/// The version a commit that applies `record` retires at, when the store keeps
/// retired entries.
pub(crate) async fn retiring_at(store: &Store, version: Sequence) -> Result<Option<Sequence>> {
    Ok(store.retired_since().await?.map(|_| version))
}

/// Keep what a removed plain entry said, stamped with the removing version.
pub(crate) fn retire(
    batch: WriteBatch,
    address: &IndexAddress,
    values: &IndexValues,
    id: &RecordId,
    at: Sequence,
) -> WriteBatch {
    batch.put(
        RetiredIndexKey::keyspace(),
        RetiredIndexKey::new(*address, values.clone(), id.clone(), at).encode(),
        NoPayload.encode(),
    )
}

/// The key that starts keeping retired entries, in the batch of the commit that
/// finalizes a store to format 8 — so the first removal after it is retired.
pub(crate) async fn keep_from(
    store: &Store,
    batch: WriteBatch,
    raised_to: FormatVersion,
    version: Sequence,
) -> Result<WriteBatch> {
    if raised_to < FormatVersion::RETIRED_INDEX || store.retired_since().await?.is_some() {
        return Ok(batch);
    }
    Ok(batch.put(
        RetiredSinceKey::keyspace(),
        RetiredSinceKey.encode(),
        version.encode(),
    ))
}

impl Transaction<'_> {
    /// Whether this transaction's snapshot is behind the committed tail.
    pub(crate) fn behind_the_tail(&self) -> Result<bool> {
        Ok(self.snapshot() < self.store().committed_version()?)
    }

    /// Whether this transaction's index reads merge retired entries — set when
    /// [`Self::plain_indexes_serve_behind_the_tail`] answered yes, and only then.
    pub(crate) fn reads_retired_entries(&self) -> bool {
        self.retired_reads.get()
    }

    /// Whether a PLAIN index of `table` may serve a read taken through this
    /// transaction although its snapshot is behind the tail.
    ///
    /// True when the store has kept retired entries since before this snapshot
    /// and no transaction across leaders is part-way in the table here (Q-919,
    /// which this does not relax). The read must then re-test every candidate:
    /// the entries it walks are a superset of the answer, never the answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored marks cannot be read.
    pub async fn plain_indexes_serve_behind_the_tail(&self, table: TableId) -> Result<bool> {
        let Some(since) = self.store().retired_since().await? else {
            return Ok(false);
        };
        let serves = self.behind_the_tail()?
            && self.snapshot() >= since
            && !self.store().across_unsettled(table).await?;
        if serves {
            self.retired_reads.set(true);
        }
        Ok(serves)
    }

    /// The records a plain index held between two live-entry bounds at this
    /// snapshot and has since lost: retired entries in the same value range,
    /// stamped after the snapshot.
    ///
    /// `start` and `end` are bounds built for the SECONDARY keyspace; the
    /// retired keys share its layout under their own tag, so the same value
    /// range is the same bytes after the first. Empty for a transaction at the
    /// tail and for an index that keeps none — the live entries are the whole
    /// candidate set there.
    pub(crate) async fn retired_candidates(
        &self,
        index: &IndexDefinition,
        start: &[u8],
        end: &[u8],
    ) -> Result<BTreeSet<RecordId>> {
        let mut found = BTreeSet::new();
        if !index.keeps_retired_entries() || !self.reads_retired_entries() {
            return Ok(found);
        }
        let retag = |bytes: &[u8]| {
            let mut owned = bytes.to_vec();
            if let Some(first) = owned.first_mut() {
                *first = KeyKind::RetiredIndex.tag();
            }
            owned
        };
        let mut range = KeyRange::between(Key::from(retag(start)), Key::from(retag(end)));
        loop {
            let request = ScanRequest {
                keyspace: RetiredIndexKey::keyspace(),
                range: range.clone(),
                direction: ScanDirection::Forward,
                limit: Some(RANGE_SCAN_BATCH_ENTRIES),
            };
            let entries = self.store().backend().scan(&request).await?;
            for (key, _) in &entries {
                let retired = RetiredIndexKey::decode(key.as_slice())?;
                if retired.retired_at > self.snapshot() {
                    found.insert(retired.id);
                }
            }
            let Some((last, _)) = entries
                .last()
                .filter(|_| entries.len() >= RANGE_SCAN_BATCH_ENTRIES)
            else {
                return Ok(found);
            };
            range = range.resuming_after(last);
        }
    }
}
