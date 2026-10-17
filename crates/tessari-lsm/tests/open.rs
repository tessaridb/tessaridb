//! Opening a store: what survives a restart, and what is refused.
//!
//! Opening is a decision tree, not a call. A directory already held by another
//! process, and a store whose region set is not the one this build expects, are
//! different situations from a generic failure and each gets its own answer —
//! because the operator does something different about each.

#![allow(clippy::panic, clippy::unwrap_used)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

use tessari_kv::{ErrorCategory, Key, Keyspace, KvBackend as _, Value, WriteBatch};
use tessari_lsm::{Durability, StoreConfig, TessariKvBackend, effective_options_files};

fn config() -> StoreConfig {
    StoreConfig::new(Durability::ProcessCrashSafe)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_was_written_before_a_close_is_there_after_a_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let first = TessariKvBackend::open(&path, config()).await.unwrap();
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
        .await
        .unwrap();
    first.close().await.unwrap();

    let second = TessariKvBackend::open(&path, config()).await.unwrap();
    assert_eq!(
        second
            .get(Keyspace::DATA, &Key::from_slice(b"record"))
            .await
            .unwrap(),
        Some(Value::from_slice(b"payload"))
    );
    assert_eq!(
        second
            .get(Keyspace::META, &Key::from_slice(b"position"))
            .await
            .unwrap(),
        Some(Value::from_slice(b"7"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_directory_already_open_elsewhere_is_refused_by_name_and_not_by_panic() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let held = TessariKvBackend::open(&path, config()).await.unwrap();
    let error = TessariKvBackend::open(&path, config()).await.unwrap_err();

    assert_eq!(error.category(), ErrorCategory::Unavailable);
    let text = error.to_string();
    assert!(text.contains("locked by another open database"), "{text}");
    assert!(
        error.is_retryable(),
        "the owner may hand the directory back; the caller decides whether to wait"
    );
    drop(held);
}

// One thread: once `close` answers, nothing else runs until this test awaits
// again, so a close left running in the background cannot finish in between.
#[tokio::test(flavor = "current_thread")]
async fn a_closed_store_hands_its_directory_back_before_close_returns() {
    // Another process cannot wait for this one's background close, so the
    // directory must be free the moment `close` answers. The probe takes the
    // engine's lock file the way another process would, without yielding.
    // A durability that syncs puts the write gate's view in front of the
    // engine, so both shapes of the store are closed.
    for durability in [Durability::ProcessCrashSafe, Durability::PowerLossSafe] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("store");
        let backend = TessariKvBackend::open(&path, StoreConfig::new(durability))
            .await
            .unwrap();
        let store = tessari_storage::Store::open(std::sync::Arc::new(backend))
            .await
            .unwrap();
        store.close().await.unwrap();

        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join("LOCK"))
            .unwrap();
        assert!(
            lock.try_lock().is_ok(),
            "{durability:?}: the directory was still held after close returned"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_with_another_handle_alive_is_not_closed_under_it() {
    for durability in [Durability::ProcessCrashSafe, Durability::PowerLossSafe] {
        let root = tempfile::tempdir().unwrap();
        let backend =
            TessariKvBackend::open(root.path().join("store"), StoreConfig::new(durability))
                .await
                .unwrap();
        let store = tessari_storage::Store::open(std::sync::Arc::new(backend))
            .await
            .unwrap();
        let other = store.clone();

        let refused = store.close().await.unwrap_err().to_string();
        assert!(
            refused.contains("the store is still in use"),
            "{durability:?}: {refused}"
        );
        // The handle still held keeps working, and closes once it is the last.
        other.begin().await.unwrap();
        other.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_missing_a_region_is_refused_rather_than_extended_in_place() {
    // A store written by something with a different idea of what it contains is
    // not repaired by adding the region back: starting anyway would serve empty
    // results out of a region that merely looks new.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("partial");

    let mut options = tessarikv::DbOptions::builder()
        .create_if_missing(true)
        .create_missing_keyspaces(true);
    for name in ["meta", "data", "index"] {
        options = options.keyspace(name, tessarikv::KeyspaceOptions::default());
    }
    let partial = tessarikv::Db::open(&path, options.build().unwrap())
        .await
        .unwrap();
    partial.close().await.unwrap();

    let error = TessariKvBackend::open(&path, config()).await.unwrap_err();
    assert_eq!(error.category(), ErrorCategory::Validation);
    let text = error.to_string();
    assert!(
        text.contains("the options name a keyspace it does not hold"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_store_records_the_options_it_is_actually_running_with() {
    // The configuration struct records what the code meant to set. This file is
    // what the engine actually did, and an audit reads this one.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let store = TessariKvBackend::open(&path, config()).await.unwrap();
    let written = effective_options_files(&path).unwrap();
    assert!(
        !written.is_empty(),
        "every open writes the effective option set"
    );

    let text = std::fs::read_to_string(&written[0]).unwrap();
    // A precondition is trusted only inside a serializable transaction.
    assert!(
        text.contains("serializable_transactions: true"),
        "serializable transactions are not set: {text}"
    );
    for keyspace in Keyspace::ALL {
        assert!(
            text.contains(&format!("\"{}\"", keyspace.name())),
            "no entry for region {keyspace}"
        );
    }
    drop(store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_healthy_store_reports_no_failed_background_work() {
    let root = tempfile::tempdir().unwrap();
    let store = TessariKvBackend::open(root.path().join("store"), config())
        .await
        .unwrap();
    assert_eq!(store.background_errors().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_declared_durability_level_is_visible_on_the_store() {
    let root = tempfile::tempdir().unwrap();
    let store = TessariKvBackend::open(
        root.path().join("store"),
        StoreConfig::new(Durability::PowerLossSafe),
    )
    .await
    .unwrap();
    assert_eq!(store.durability(), Durability::PowerLossSafe);
    assert_eq!(store.durability().name(), "power-loss-safe");
}
