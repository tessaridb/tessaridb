//! Which tables have a retention floor, held per process.
//!
//! # Why this is a registry and not a catalog read
//!
//! The floor is consulted by every read that resolves a record, which is the
//! hottest path there is. Asking the catalog for the table's declaration each
//! time would put **an extra backend round trip on every point read** — a table
//! that is not a series would pay for a feature it does not have, which is
//! exactly what this engine promised not to do. The store's own round-trip tests
//! caught that cost the first time it was written this way, which is what they
//! are for.
//!
//! So the mapping is held in memory beside the snapshot registry, the consumer
//! registry and the open vault, for the reason those are: two handles to one
//! store must not disagree, and this is per-process state rather than something
//! the log carries.
//!
//! # Why a value that is absent is not the same as a table that is not a series
//!
//! `Some(None)` is "this table was looked up and is not a series", which is
//! immutable — a table's kind is fixed when it is created and there is no
//! statement that changes it. `None` is "never looked up", which sends the
//! caller to the catalog once. The distinction is what lets a store opened over
//! tables some earlier process created still find their floors, without every
//! read paying for the ones that have none.

use std::collections::BTreeMap;
use std::sync::RwLock;

use tessari_types::{Duration, TableId};

use crate::catalog::definition::TableKind;

/// The retention each table carries, as far as this process has learned.
#[derive(Debug, Default)]
pub(crate) struct SeriesRegistry {
    known: RwLock<BTreeMap<TableId, Option<Duration>>>,
}

impl SeriesRegistry {
    /// What this process knows about `table`.
    ///
    /// The outer `Option` is whether it has been learned; the inner is whether
    /// the table is a series.
    pub(crate) fn known(&self, table: TableId) -> Option<Option<Duration>> {
        // A poisoned lock means a thread panicked while holding it. Answering
        // "not learned" sends the caller to the catalog, which is correct and
        // slow rather than wrong and fast.
        self.known
            .read()
            .ok()
            .and_then(|held| held.get(&table).copied())
    }

    /// Record what a table's kind says, learned from a declaration or a read.
    pub(crate) fn learn(&self, table: TableId, kind: &TableKind) {
        let retain = match kind {
            TableKind::Series(declared) => Some(declared.retain),
            _ => None,
        };
        if let Ok(mut held) = self.known.write() {
            held.insert(table, retain);
        }
    }

    /// Forget a table that has been dropped.
    ///
    /// Ids are allocated and never reused, so this is hygiene rather than
    /// correctness: nothing can address the table again.
    pub(crate) fn forget(&self, table: TableId) {
        if let Ok(mut held) = self.known.write() {
            held.remove(&table);
        }
    }
}
