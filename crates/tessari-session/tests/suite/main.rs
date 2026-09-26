//! Every integration test of this crate, in one binary.
//!
//! Cargo builds one test binary per file directly under `tests/`, and each one
//! links the whole workspace again. A subdirectory carrying a `main.rs` is one
//! target instead, so the cases sit beside this file and the crate pays that
//! link once rather than once per case.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod a_catalog_record_is_not_a_users_row;
mod advice;
mod alone;
mod alter_user;
mod analyzer_named_by_a_field;
mod analyzer_redefinition;
mod answered_by;
mod ascending_order;
mod assertions;
mod assertions_over_dropped_declarations;
mod audit;
mod authorities;
mod bindings;
mod boolean;
mod boost;
mod bounded_index_reads;
mod bounded_reads;
mod bucket_ceiling;
mod bucket_grants;
mod composite_index;
mod composite_order;
mod composite_range;
mod conditionals;
mod configuration;
mod consumers;
mod descending;
mod describing_kinds;
mod differential;
mod effect;
mod exactness;
mod explain;
mod fetch;
mod file_ranges;
mod files;
mod folds;
mod follower_lag;
mod full_tuple;
mod fuzzy;
mod gathered_reads;
mod generated;
mod geo_store;
mod geometry_ingest;
mod geometry_query;
mod grants;
mod graph_container;
mod graph_engine;
mod graphs;
mod held;
mod highlighting;
mod history;
mod index_kinds_do_not_leak;
mod info;
mod inserts;
mod instants;
mod join_keys;
mod join_sources;
mod joins;
mod key_value;
mod kinds;
mod management;
mod migration;
mod multikey;
mod node;
mod notes;
mod one_plan;
mod opened;
mod ordered_under_a_where;
mod ordering;
mod own_password;
mod parameters;
mod phrase;
mod plan_invariance;
mod prefix;
mod pruned_ranking;
mod queue_claims;
mod queue_targeted_claims;
mod ranges;
mod ranking;
mod rebuild_index;
mod refusal;
mod resumed;
mod revocation;
mod scan_wins;
mod scoring_source;
mod scripts;
mod search_field_grants;
mod selective_stream;
mod series;
mod several;
mod shaping;
mod sharding;
mod sign_in_throttle;
mod spatial_index;
mod spatial_nearest_reads;
mod spatial_reads;
mod staleness;
mod store_named_records;
mod store_wide;
mod strictness;
mod subquery_grants;
mod suggestion;
mod tickets;
mod tightening;
mod timeout;
mod topic;
mod traversal;
mod trusted_index;
mod unique_within_a_transaction;
mod update_fields;
mod using;
mod vault_audit;
mod vault_exfiltration;
mod vault_language;
mod vault_reach_and_open;
mod vault_recipients;
mod vault_refusals;
mod vault_rollback;
mod vector_field_grants;
mod vector_index;
mod vector_reads;
mod vector_recall;
mod vector_store;
mod vector_walk_freshness;
mod vector_width;
mod views;
mod windows;
mod write_answers;
mod write_bounds;
mod write_shapes;

/// Replay every log a store holds into another, and answer how many records.
///
/// Every log, in the order `homes()` lists them — the store's own first, so the
/// namespace, database and schema definitions a range's records depend on arrive
/// before those records do. A replay that read one log would reproduce part of a
/// store and then be compared against the whole of it (S6.2, Q-620).
pub(crate) fn replay(source: &tessari_storage::Store, target: &tessari_storage::Store) -> usize {
    let mut applied = 0_usize;
    for log in source.logs().unwrap() {
        for (sequence, record) in source
            .log_records(log, tessari_types::Sequence::ZERO, 4096)
            .unwrap()
        {
            target.apply_record(log.writer, sequence, &record).unwrap();
            applied = applied.saturating_add(1);
        }
    }
    applied
}

/// Entries with their version stamps removed, and the set of stamps removed.
pub(crate) type Unversioned = (Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>);

/// Every record entry with the version it is keyed by removed, and the set of
/// those versions.
///
/// A replay across several logs applies records in a different order than the
/// commits that produced them, so a node numbers them differently — a version is
/// the node's own history and not the log's (Q-614, Q-627). What must still hold
/// is that the same records come back with the same bytes at the same keys apart
/// from that stamp, and that the SET of stamps is the same, which is what would
/// show one skipped, duplicated or invented.
pub(crate) fn unversioned(held: &[(tessari_kv::Key, tessari_kv::Value)]) -> Unversioned {
    let mut stamps: Vec<Vec<u8>> = Vec::new();
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (key, value) in held {
        let key = key.as_slice();
        let cut = key.len().saturating_sub(8);
        entries.push((key[..cut].to_vec(), value.as_slice().to_vec()));
        stamps.push(key[cut..].to_vec());
    }
    stamps.sort();
    stamps.dedup();
    (entries, stamps)
}
