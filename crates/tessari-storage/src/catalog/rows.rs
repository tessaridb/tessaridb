//! The catalog's name, table, index and field rows, held between statements.
//!
//! # Why this exists
//!
//! Every statement that names a table resolves the namespace, the database and
//! the table by name and then reads the table's definition — four versioned
//! reads of rows that almost never change. After the decode itself was shared
//! (`decoded`), those reads were still a third of a point read's time (G040
//! SG6, profiled 2026-09-27).
//!
//! A write reads more of them: its table's indexes and fields, found through
//! the span of names they claim and then read one by one (`children`). Read at
//! every commit and every statement, that was a scan per definition per write —
//! +10…+31 % p50 on a small store from 0.34.1 (Q-G071-2). So the index and
//! field rows are held too, and so is the answer of a span: the ids its names
//! claim.
//!
//! # Why it cannot serve a row the reader's snapshot does not hold
//!
//! The rows of [`system::NAMES`], [`system::TABLES`], [`system::INDEXES`] and
//! [`system::FIELDS`] are the same at every snapshot at or above the last
//! version that changed any of them, so a row — or a span of names — read at
//! one such snapshot is the answer for all of them. This remembers that
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
//! At most [`HELD`] rows and spans together; past it the set is dropped and
//! refilled, as in `decoded`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tessari_encoding::LogRecord;
use tessari_types::{DatabaseId, NamespaceId, Sequence, TableId};

use super::system;
use crate::transaction::RecordAddress;

/// How many rows and spans a process keeps. A table costs its table row, a
/// name and a row per index and field, and a span for each of the two, so this
/// holds a few hundred tables' definitions before it is dropped and refilled.
const HELD: usize = 4096;

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
    /// Shared rather than copied out: a statement reads several of these.
    rows: HashMap<RecordAddress, Option<Arc<[u8]>>>,
    /// The ids claimed under one prefix of `NAMES`, in id order (`children`).
    spans: HashMap<String, Vec<u32>>,
}

/// What a reader at one snapshot may do with one row or span.
pub(crate) enum Lookup<T> {
    /// The row or span as the reader's snapshot holds it.
    Held(T),
    /// Not held. The generation is present when the reader may fill it.
    Missing(Option<u64>),
}

impl CatalogRows {
    /// Whether rows at this address are held here at all.
    pub(crate) fn holds(address: &RecordAddress) -> bool {
        held_table(address.namespace, address.database, address.table)
    }

    /// Whether a record changes a row held here — and so any span, since a
    /// span is read from `NAMES`.
    pub(crate) fn changes(record: &LogRecord) -> bool {
        record
            .mutations()
            .iter()
            .any(|mutation| held_table(mutation.namespace, mutation.database, mutation.table))
    }

    pub(crate) fn lookup(
        &self,
        address: &RecordAddress,
        snapshot: Sequence,
    ) -> Lookup<Option<Arc<[u8]>>> {
        self.find(snapshot, |held| held.rows.get(address).cloned())
    }

    /// The ids claimed under `prefix` of `NAMES`, as [`Self::lookup`] answers a
    /// row.
    pub(crate) fn lookup_span(&self, prefix: &str, snapshot: Sequence) -> Lookup<Vec<u32>> {
        self.find(snapshot, |held| held.spans.get(prefix).cloned())
    }

    fn find<T>(&self, snapshot: Sequence, found: impl FnOnce(&Held) -> Option<T>) -> Lookup<T> {
        let Ok(held) = self.held.read() else {
            return Lookup::Missing(None);
        };
        if snapshot < held.changed_at {
            return Lookup::Missing(None);
        }
        match found(&held) {
            Some(answer) => Lookup::Held(answer),
            None => Lookup::Missing(Some(held.generation)),
        }
    }

    /// Keep a row read at a snapshot [`Self::lookup`] allowed to fill, unless the
    /// catalog changed since.
    pub(crate) fn fill(&self, address: RecordAddress, generation: u64, row: Option<Arc<[u8]>>) {
        self.keep(generation, |held| {
            held.rows.insert(address, row);
        });
    }

    /// Keep a span as [`Self::fill`] keeps a row.
    pub(crate) fn fill_span(&self, prefix: String, generation: u64, ids: Vec<u32>) {
        self.keep(generation, |held| {
            held.spans.insert(prefix, ids);
        });
    }

    fn keep(&self, generation: u64, insert: impl FnOnce(&mut Held)) {
        let Ok(mut held) = self.held.write() else {
            return;
        };
        if held.generation != generation {
            return;
        }
        if held.rows.len().saturating_add(held.spans.len()) >= HELD {
            held.rows.clear();
            held.spans.clear();
        }
        insert(&mut held);
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
        held.spans.clear();
    }
}

/// The catalog tables whose rows are held: the ones a statement resolves names
/// through and the ones a write finds its table's definitions in.
fn held_table(namespace: NamespaceId, database: DatabaseId, table: TableId) -> bool {
    namespace == system::SYSTEM_NAMESPACE
        && database == system::SYSTEM_DATABASE
        && [
            system::NAMES,
            system::TABLES,
            system::INDEXES,
            system::FIELDS,
        ]
        .contains(&table)
}
