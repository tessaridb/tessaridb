//! Every integration test of this crate, in one binary.
//!
//! Cargo builds one test binary per file directly under `tests/`, and each one
//! links the whole workspace again. A subdirectory carrying a `main.rs` is one
//! target instead, so the cases sit beside this file and the crate pays that
//! link once rather than once per case.

mod catalog;
mod counted_reads;
mod descending_order;
mod expansion_bound;
mod feed;
mod index_maintenance;
mod index_sweep;
mod isolation;
mod queue_definitions;
mod range_batches;
mod record_counts;
mod replication;
mod retention;
mod schema;
mod sealing;
mod spatial_nearest;
mod spatial_region;
mod term_dictionary;
mod values_in_records;
mod walking_a_table;
