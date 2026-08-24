//! The persistent backend against the shared contract.
//!
//! This is the same suite the in-memory backend runs, unchanged. That is the
//! point of it: a contract asserted only against the implementation that shaped
//! it is a description, and two implementations passing one suite is what turns
//! it into a contract.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::path::Path;

use tessari_kv::conformance;
use tessari_lsm::{Durability, LsmBackend, StoreConfig};

/// The suite builds a fresh store per check, so no check can be influenced by
/// what another left behind. Every store here is throwaway and never restarted,
/// so it runs at the level that does not sync per commit — the level that *is*
/// synced is proven where it matters, by killing a process.
fn store_at(root: &Path, index: usize) -> LsmBackend {
    let path = root.join(format!("store-{index}"));
    LsmBackend::open(path, StoreConfig::new(Durability::ProcessCrashSafe)).unwrap()
}

#[test]
fn the_persistent_backend_satisfies_the_whole_contract() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().to_path_buf();
    let mut index = 0;

    let results = conformance::run_all(|| {
        index += 1;
        store_at(&base, index)
    });

    assert_eq!(
        results.len(),
        conformance::check_count(),
        "the suite must run every check"
    );
    let failures: Vec<String> = results
        .iter()
        .filter_map(|result| {
            result
                .failure
                .as_ref()
                .map(|reason| format!("{}: {reason}", result.name))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_backend_reports_a_name_that_does_not_leak_the_engine_behind_it() {
    use tessari_kv::KvBackend;

    let root = tempfile::tempdir().unwrap();
    let backend = LsmBackend::open(
        root.path().join("named"),
        StoreConfig::new(Durability::ProcessCrashSafe),
    )
    .unwrap();
    assert_eq!(backend.name(), "lsm");
}
