//! The catalog's name and table rows, held between statements.
//!
//! # Why this exists
//!
//! Every statement that names a table resolves the namespace, the database and
//! the table by name and then reads the table's definition — four versioned
//! reads of rows that almost never change. After the decode itself was shared
//! (`decoded`), those reads were still a third of a point read's time (G040
//! SG6, profiled 2026-09-27).
//!
//! # Why it cannot serve a row the reader's snapshot does not hold
//!
//! The rows of [`system::NAMES`] and [`system::TABLES`] are the same at every
//! snapshot at or above the last version that changed any of them, so a row
//! read at one such snapshot is the answer for all of them. This remembers that
//! version and answers only a reader whose snapshot is at or above it.
//!
//! - **Invalidation.** A commit or a replica's apply that changes one of those
//!   rows says so with [`CatalogRows::changed`] **before** its batch can become
//!   visible, so no reader holds a snapshot at the new version while an older row
//!   is still held. Every held row is dropped and the generation moves on.
//! - **Filling.** Only a reader whose snapshot is at or above that version, and
//!   only if no change arrived while it read — the generation it saw before the
//!   read is compared under the write lock. And never from the thread holding the
//!   write turn, which can read batches staged behind it that have not landed and
//!   may not (`crate::gate`).
//! - **A transaction's own writes** are not held here: the caller reads its own
//!   row through the transaction.
//!
//! # Bound
//!
//! At most [`HELD`] rows; past it the set is dropped and refilled, as in
//! `decoded`.

use std::collections::HashMap;
use std::sync::RwLock;

use tessari_encoding::LogRecord;
use tessari_types::Sequence;

use super::system;
use crate::transaction::RecordAddress;

/// How many rows a process keeps.
const HELD: usize = 1024;

/// Name and table rows, valid for readers at or above the last change.
#[derive(Debug, Default)]
pub(crate) struct CatalogRows {
    /// A `RwLock`: every catalog lookup reads, and `fill`/`changed` write only on
    /// a miss or a catalog change (G040 M4).
    held: RwLock<Held>,
}

#[derive(Debug, Default)]
struct Held {
    changed_at: Sequence,
    generation: u64,
    rows: HashMap<RecordAddress, Option<Vec<u8>>>,
}

/// What a reader at one snapshot may do with one row.
pub(crate) enum Lookup {
    /// The row, or its absence, as the reader's snapshot holds it.
    Held(Option<Vec<u8>>),
    /// Not held. The generation is present when the reader may fill it.
    Missing(Option<u64>),
}

impl CatalogRows {
    /// Whether rows at this address are held here at all.
    pub(crate) fn holds(address: &RecordAddress) -> bool {
        address.namespace == system::SYSTEM_NAMESPACE
            && address.database == system::SYSTEM_DATABASE
            && (address.table == system::NAMES || address.table == system::TABLES)
    }

    /// Whether a record changes a row held here.
    pub(crate) fn changes(record: &LogRecord) -> bool {
        record.mutations().iter().any(|mutation| {
            mutation.namespace == system::SYSTEM_NAMESPACE
                && mutation.database == system::SYSTEM_DATABASE
                && (mutation.table == system::NAMES || mutation.table == system::TABLES)
        })
    }

    pub(crate) fn lookup(&self, address: &RecordAddress, snapshot: Sequence) -> Lookup {
        let Ok(held) = self.held.read() else {
            return Lookup::Missing(None);
        };
        if snapshot < held.changed_at {
            return Lookup::Missing(None);
        }
        match held.rows.get(address) {
            Some(row) => Lookup::Held(row.clone()),
            None => Lookup::Missing(Some(held.generation)),
        }
    }

    /// Keep a row read at a snapshot [`Self::lookup`] allowed to fill, unless the
    /// catalog changed since.
    pub(crate) fn fill(&self, address: RecordAddress, generation: u64, row: Option<Vec<u8>>) {
        let Ok(mut held) = self.held.write() else {
            return;
        };
        if held.generation != generation {
            return;
        }
        if held.rows.len() >= HELD {
            held.rows.clear();
        }
        held.rows.insert(address, row);
    }

    /// A change to these rows becomes visible at `version`; nothing held is valid
    /// for it. Called before the change can be read.
    pub(crate) fn changed(&self, version: Sequence) {
        let mut held = self
            .held
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.changed_at = held.changed_at.max(version);
        held.generation = held.generation.wrapping_add(1);
        held.rows.clear();
    }
}
