//! Opening a store: what survives a restart, and what is refused.
//!
//! Opening is a decision tree, not a call. A directory already held by another
//! process, and a store whose region set is not the one this build expects, are
//! different situations from a generic failure and each gets its own answer —
//! because the operator does something different about each.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_kv::{ErrorCategory, Key, Keyspace, KvBackend, Value, WriteBatch};
use tessari_lsm::{Durability, LsmBackend, StoreConfig, effective_options_files};

fn config() -> StoreConfig {
    StoreConfig::new(Durability::ProcessCrashSafe)
}

#[test]
fn what_was_written_before_a_close_is_there_after_a_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let first = LsmBackend::open(&path, config()).unwrap();
    first
        .apply(
            WriteBatch::new()
                .put(
                    Keyspace::DATA,
                    Key::from_slice(b"record"),
                    Value::from_slice(b"payload"),
                )
                .put(
                    Keyspace::META,
                    Key::from_slice(b"position"),
                    Value::from_slice(b"7"),
                ),
        )
        .unwrap();
    first.close().unwrap();

    let second = LsmBackend::open(&path, config()).unwrap();
    assert_eq!(
        second
            .get(Keyspace::DATA, &Key::from_slice(b"record"))
            .unwrap(),
        Some(Value::from_slice(b"payload"))
    );
    assert_eq!(
        second
            .get(Keyspace::META, &Key::from_slice(b"position"))
            .unwrap(),
        Some(Value::from_slice(b"7"))
    );
}

#[test]
fn a_directory_already_open_elsewhere_is_refused_by_name_and_not_by_panic() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let held = LsmBackend::open(&path, config()).unwrap();
    let error = LsmBackend::open(&path, config()).unwrap_err();

    assert_eq!(error.category(), ErrorCategory::Unavailable);
    let text = error.to_string();
    assert!(text.contains("another process"), "{text}");
    assert!(
        error.is_retryable(),
        "the owner may hand the directory back; the caller decides whether to wait"
    );
    drop(held);
}

#[test]
fn a_store_missing_a_region_is_refused_rather_than_extended_in_place() {
    // A store written by something with a different idea of what it contains is
    // not repaired by adding the region back: starting anyway would serve empty
    // results out of a region that merely looks new.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("partial");

    let mut options = rocksdb::Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);
    let partial = rocksdb::DB::open_cf(&options, &path, ["meta", "data", "index"]).unwrap();
    drop(partial);

    let error = LsmBackend::open(&path, config()).unwrap_err();
    assert_eq!(error.category(), ErrorCategory::Validation);
    let text = error.to_string();
    assert!(text.contains("log"), "{text}");
}

#[test]
fn the_store_records_the_options_it_is_actually_running_with() {
    // The configuration struct records what the code meant to set. This file is
    // what the engine actually did, and an audit reads this one.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let store = LsmBackend::open(&path, config()).unwrap();
    let written = effective_options_files(&path).unwrap();
    assert!(
        !written.is_empty(),
        "every open writes the effective option set"
    );

    let text = std::fs::read_to_string(&written[0]).unwrap();
    assert!(
        text.contains("atomic_flush=true"),
        "atomic flush is not set"
    );
    for keyspace in Keyspace::ALL {
        assert!(
            text.contains(&format!("[CFOptions \"{}\"]", keyspace.name())),
            "no section for region {keyspace}"
        );
    }
    drop(store);
}

#[test]
fn a_healthy_store_reports_no_failed_background_work() {
    let root = tempfile::tempdir().unwrap();
    let store = LsmBackend::open(root.path().join("store"), config()).unwrap();
    assert_eq!(store.background_errors().unwrap(), 0);
}

#[test]
fn the_declared_durability_level_is_visible_on_the_store() {
    let root = tempfile::tempdir().unwrap();
    let store = LsmBackend::open(
        root.path().join("store"),
        StoreConfig::new(Durability::PowerLossSafe),
    )
    .unwrap();
    assert_eq!(store.durability(), Durability::PowerLossSafe);
    assert_eq!(store.durability().name(), "power-loss-safe");
}
