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
//! # Since `ALTER TABLE … SPLIT AT` it is TAUGHT, and when matters
//!
//! A split moves the map, so the registry is taught by the record that carries
//! the new definition — under the write gate, before the batch lands, on the
//! leader's commit and on every follower's apply alike (ADR-0095 D8). A commit
//! re-asks its placement under that same gate, so no commit can be admitted
//! under one map and filed after another. A landing that fails forgets what it
//! taught, so the next placement reads the committed catalog instead.

use std::sync::Arc;

use dashmap::DashMap;
use tessari_encoding::{LogRecord, RecordValue};
use tessari_types::TableId;

use crate::catalog::{DecodedTables, ShardMap, system};
use crate::error::Result;

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

    /// Learn the map of every table definition `record` writes, answering the
    /// tables taught so a landing that fails can [`Self::forget`] them.
    ///
    /// Called under the write gate, before the batch lands (ADR-0095 D8).
    pub(crate) fn teach(
        &self,
        decoded: &DecodedTables,
        record: &LogRecord,
    ) -> Result<Vec<TableId>> {
        let mut taught = Vec::new();
        for mutation in record.mutations() {
            if mutation.namespace != system::SYSTEM_NAMESPACE
                || mutation.database != system::SYSTEM_DATABASE
                || mutation.table != system::TABLES
            {
                continue;
            }
            // A dropped table's id is never reused, so what was learned of it
            // is never asked again and is left where it is.
            let RecordValue::Present(stored) = mutation.value.value() else {
                continue;
            };
            let definition = decoded.definition(stored)?;
            self.learn(definition.id, definition.shards.as_ref());
            taught.push(definition.id);
        }
        Ok(taught)
    }

    /// Forget `tables`, so the next commit reads their maps from the catalog.
    pub(crate) fn forget(&self, tables: &[TableId]) {
        for table in tables {
            self.known.remove(table);
        }
    }
}
