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

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use tessari_types::TableId;

use crate::catalog::ShardMap;

/// The shard map each table carries, as far as this process has learned.
#[derive(Debug, Default)]
pub(crate) struct ShardRegistry {
    known: RwLock<BTreeMap<TableId, Option<Arc<ShardMap>>>>,
}

impl ShardRegistry {
    /// What this process knows about `table`: the outer `Option` is whether it
    /// has been learned, the inner whether the table is split.
    pub(crate) fn known(&self, table: TableId) -> Option<Option<Arc<ShardMap>>> {
        // A poisoned lock sends the caller to the catalog, which is correct and
        // slow rather than wrong and fast.
        self.known
            .read()
            .ok()
            .and_then(|held| held.get(&table).cloned())
    }

    /// Record a table's map, learned from its declaration or from a read.
    pub(crate) fn learn(&self, table: TableId, shards: Option<&ShardMap>) {
        if let Ok(mut held) = self.known.write() {
            held.insert(table, shards.cloned().map(Arc::new));
        }
    }
}
