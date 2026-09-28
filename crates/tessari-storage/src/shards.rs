//! Which tables are split, and where, held per process (G031, ADR-0080).
//!
//! # Why a registry and not a catalog read
//!
//! Every commit asks which shard each record it writes falls in, so a catalog
//! read per table per commit would put a backend round trip on the write path
//! of every table, split or not — the cost [`crate::series`] was written to
//! avoid on the read path, for the same reason.
//!
//! # Why caching it is safe
//!
//! A table's shard map is fixed when the table is declared: no statement moves a
//! boundary, and a split, when one exists, will mint new shard ids rather than
//! edit a span. Table ids are never reused. So an answer learned once is the
//! answer for the life of the table, and `Some(None)` — *learned, and not split*
//! — is as permanent as `Some(Some(map))`. `None` means *not looked up yet*.
//!
//! **The day `ALTER TABLE … SPLIT AT` exists this stops being true**, and the
//! registry has to be told by the statement that moves the map; that is written
//! here so it is not discovered from a record filed in a retired shard.

use std::sync::Arc;

use dashmap::DashMap;
use tessari_types::TableId;

use crate::catalog::ShardMap;

/// The shard map each table carries, as far as this process has learned.
#[derive(Debug, Default)]
pub(crate) struct ShardRegistry {
    /// A `DashMap`: every statement over a table asks `known` from
    /// every thread, and `learn` changes it only when a table is first seen or
    /// split again.
    known: DashMap<TableId, Option<Arc<ShardMap>>>,
}

impl ShardRegistry {
    /// What this process knows about `table`: the outer `Option` is whether it
    /// has been learned, the inner whether the table is split.
    pub(crate) fn known(&self, table: TableId) -> Option<Option<Arc<ShardMap>>> {
        self.known.get(&table).map(|held| held.clone())
    }

    /// Record a table's map, learned from its declaration or from a read.
    pub(crate) fn learn(&self, table: TableId, shards: Option<&ShardMap>) {
        self.known.insert(table, shards.cloned().map(Arc::new));
    }
}
