//! Every integration test of this crate, in one binary.
//!
//! Cargo builds one test binary per file directly under `tests/`, and each one
//! links the whole workspace again. A subdirectory carrying a `main.rs` is one
//! target instead, so the cases sit beside this file and the crate pays that
//! link once rather than once per case.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod catalog;
mod counted_reads;
mod descending_order;
mod expansion_bound;
mod feed;
mod index_maintenance;
mod index_sweep;
mod isolation;
mod lease_fence;
mod queue_definitions;
mod range_batches;
mod record_counts;
mod replication;
mod retention;
mod schema;
mod sealing;
mod series_floor;
mod spatial_nearest;
mod spatial_region;
mod superseded_leadership;
mod term_dictionary;
mod two_leaders;
mod values_in_records;
mod view_definitions;
mod walking_a_table;

use tessari_types::{DatabaseId, NamespaceId, Reach};

/// The log the fixtures in this suite write into.
///
/// Every one of them works in namespace 1, database 1, and after the log became
/// per-range that is where their records are filed — so a test reading "the log"
/// has to name it. The few cases that write outside it (a namespace definition,
/// a record carrying nothing) name their own home at the call site.
pub(crate) const FIXTURE_HOME: Reach = Reach::Database(NamespaceId::new(1), DatabaseId::new(1));

/// Replay every log a store holds into another, and answer how many records.
///
/// Every log, in the order `homes()` lists them — the store's own first, so the
/// namespace, database and table definitions a range's records depend on arrive
/// before those records do. A replay that read one log would reproduce part of a
/// store and then be compared against the whole of it (S6.2, Q-620).
pub(crate) fn replay(source: &tessari_storage::Store, target: &tessari_storage::Store) -> usize {
    let mut applied = 0_usize;
    for home in source.homes().unwrap() {
        for (sequence, record) in source
            .log_records(home, tessari_types::Sequence::ZERO, 4096)
            .unwrap()
        {
            target.apply_record(sequence, &record).unwrap();
            applied = applied.saturating_add(1);
        }
    }
    applied
}
