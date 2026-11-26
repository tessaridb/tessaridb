//! The chunk-head index: sealed chunks' ranges, held in memory, so a pruned
//! read consults memory instead of seeking every chunk's head (ADR-0135,
//! S13-B).
//!
//! # What it is
//!
//! One entry per (table, family, partition) lane: the sealed chunks' local
//! seqs with the [low, high] each chunk's head statistics recorded, and the
//! open seq the set was built against. A read whose lane entry is present
//! and CURRENT — its `open_at` equals the lane pointer's open seq — visits
//! only the entries the predicate can match, plus the open chunk, which no
//! statistic ever covers. A read whose lane entry is absent or stale walks
//! every head, exactly as before, and installs a fresh entry from what it
//! walked.
//!
//! # The soundness rule
//!
//! The index may only ever be the reason a read examines FEWER heads, never
//! the reason a matching row is missed. That holds by three rules:
//!
//! - **Completeness is claimed against one open seq.** An entry built when
//!   the lane's open seq was `n` covers every chunk sealed below `n`. A lane
//!   pointer naming a different open seq makes the entry stale, and stale is
//!   absence.
//! - **A seal extends, anything else drops.** The commit apply appends the
//!   chunks it sealed and moves `open_at` forward — the one transition that
//!   preserves completeness. A copy-on-write rewrite, whose chunk keeps its
//!   seq but changes its range, DROPS the lane: rebuilding costs one
//!   walk-all read, and a stale range that skipped a matching chunk would
//!   cost a wrong answer.
//! - **The open chunk is never in the index**, so it is consulted on every
//!   read whatever the index says — the same rule the lane statistics keep.
//!
//! Nothing here is durable: the heads are the truth, and the index is a
//! memory of having read them.

use std::collections::BTreeMap;

use dashmap::DashMap;
use tessari_encoding::ColumnFamily;
use tessari_types::TableId;

use super::chunk::ChunkStats;

/// One lane's identity: the table, the family, and the partition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct LaneKey {
    /// The table.
    pub(crate) table: TableId,
    /// The column family.
    pub(crate) family: ColumnFamily,
    /// The partition within the table.
    pub(crate) partition: u64,
}

/// What one sealed chunk's head recorded — the same statistics the head
/// itself carries, so the in-memory test is the head test, not a second one.
#[derive(Debug, Clone)]
pub(crate) struct HeadEntry {
    /// The chunk's head statistics.
    pub(crate) stats: ChunkStats,
}

/// One lane's sealed-chunk ranges, complete against one open seq.
#[derive(Debug, Clone)]
pub(crate) struct LaneHeads {
    /// The open seq these entries were complete against: every chunk sealed
    /// below it has an entry.
    pub(crate) open_at: u64,
    /// Local seq → the chunk's range, for every sealed chunk below `open_at`.
    pub(crate) entries: BTreeMap<u64, HeadEntry>,
}

/// Every lane's head index, shared across the store's handles.
#[derive(Debug, Default)]
pub(crate) struct HeadIndex {
    lanes: DashMap<LaneKey, LaneHeads>,
}

impl HeadIndex {
    /// The lane's entries, when they are complete against `open` — the lane
    /// pointer's current open seq. Stale is absence: the caller walks.
    pub(crate) fn current(&self, key: &LaneKey, open: u64) -> Option<LaneHeads> {
        let held = self.lanes.get(key)?;
        (held.open_at == open).then(|| held.clone())
    }

    /// Install a lane's heads, built by a read that walked every chunk.
    pub(crate) fn install(&self, key: LaneKey, heads: LaneHeads) {
        self.lanes.insert(key, heads);
    }

    /// Extend a lane the commit apply sealed chunks into: the new entries
    /// join, and completeness moves to the new open seq. A lane not held, or
    /// held against a different base, is left alone — the next read rebuilds.
    pub(crate) fn sealed(
        &self,
        key: &LaneKey,
        base_open: u64,
        new_open: u64,
        sealed: Vec<(u64, HeadEntry)>,
    ) {
        if let Some(mut held) = self.lanes.get_mut(key)
            && held.open_at == base_open
        {
            for (local, entry) in sealed {
                held.entries.insert(local, entry);
            }
            held.open_at = new_open;
        }
    }

    /// A copy-on-write rewrite replaced a sealed chunk: its entry takes the
    /// rewritten chunk's own statistics, so the lane stays complete and the
    /// next read still seeks by memory.
    ///
    /// Updating ahead of the batch landing is sound by the store's own
    /// fail-stop rule: the one outcome that could leave the entry narrower
    /// than the stored chunk is a failed batch write, and a failed sync
    /// stops the store rather than serving another read. A lane not held is
    /// left alone — the next read walks and reinstalls.
    pub(crate) fn rewritten(&self, key: &LaneKey, local: u64, stats: ChunkStats) {
        if let Some(mut held) = self.lanes.get_mut(key)
            && held.entries.contains_key(&local)
        {
            held.entries.insert(local, HeadEntry { stats });
        }
    }
}
