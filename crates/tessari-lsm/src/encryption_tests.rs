//! An encrypted store: what it writes to disk, and which key opens it.
//!
//! The plaintext check is measured against a control — the same writes into a
//! store opened without a key, where the value is found on disk — so the
//! assertion that the encrypted store holds no copy is one that can fail.

#![allow(clippy::panic, clippy::indexing_slicing)]

use std::path::Path;

use tessari_kv::{Key, Keyspace, KvBackend, Value, WriteBatch};
use tessari_vault::{AtRestKey, SecretBytes};

use crate::backend::LsmBackend;
use crate::options::{Durability, StoreConfig};

const CANARY: &[u8] = b"plaintext-canary-7f3a9c1e";

fn key(byte: u8) -> AtRestKey {
    AtRestKey::from_key(&SecretBytes::adopt([byte; 32])).expect("a test step")
}

fn config() -> StoreConfig {
    StoreConfig::new(Durability::ProcessCrashSafe)
}

fn write(store: &LsmBackend) {
    for n in 0..200_u32 {
        let mut value = CANARY.to_vec();
        value.extend_from_slice(&n.to_be_bytes());
        store
            .apply(WriteBatch::new().put(
                Keyspace::DATA,
                Key::from_slice(format!("row-{n:04}").as_bytes()),
                Value::from_slice(&value),
            ))
            .expect("a test step");
    }
}

/// Whether any file under `folder` holds `needle`.
fn on_disk(folder: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(folder)
        .expect("a test step")
        .any(|entry| {
            let path = entry.expect("a test step").path();
            path.is_file()
                && std::fs::read(&path)
                    .expect("a test step")
                    .windows(needle.len())
                    .any(|window| window == needle)
        })
}

fn refusal(result: tessari_kv::Result<LsmBackend>) -> String {
    match result {
        Ok(_) => panic!("opened"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn an_encrypted_store_writes_no_plaintext_and_reads_back_what_it_wrote() {
    let root = tempfile::tempdir().expect("a test step");

    // The control: without a key the value is on disk, in the log and then in
    // a table, so the search below finds what it looks for.
    let plain = root.path().join("plain");
    let store = LsmBackend::open(&plain, config()).expect("a test step");
    write(&store);
    assert!(
        on_disk(&plain, CANARY),
        "the control holds no plaintext to find"
    );
    store.close().expect("a test step");

    let path = root.path().join("encrypted");
    let key = key(7);
    let store = LsmBackend::open_with_key(&path, config(), Some(&key)).expect("a test step");
    write(&store);
    assert!(!on_disk(&path, CANARY), "plaintext in the write-ahead log");
    store.compact().expect("a test step");
    assert!(!on_disk(&path, CANARY), "plaintext in a table");
    store.close().expect("a test step");

    let reopened = LsmBackend::open_with_key(&path, config(), Some(&key)).expect("a test step");
    let read = reopened
        .get(Keyspace::DATA, &Key::from_slice(b"row-0042"))
        .expect("a test step")
        .expect("a test step");
    assert!(read.as_slice().starts_with(CANARY));
    assert!(!on_disk(&path, CANARY), "plaintext after reopening");
}

#[test]
fn a_store_opens_only_the_way_it_was_made() {
    let root = tempfile::tempdir().expect("a test step");
    let encrypted = root.path().join("encrypted");
    LsmBackend::open_with_key(&encrypted, config(), Some(&key(7)))
        .expect("a test step")
        .close()
        .expect("a test step");
    let plain = root.path().join("plain");
    LsmBackend::open(&plain, config())
        .expect("a test step")
        .close()
        .expect("a test step");

    let without = refusal(LsmBackend::open(&encrypted, config()));
    assert!(
        without.contains("is encrypted; it opens only with its key"),
        "{without}"
    );

    let other = refusal(LsmBackend::open_with_key(
        &encrypted,
        config(),
        Some(&key(8)),
    ));
    assert!(other.contains("does not open the store"), "{other}");

    let keyed = refusal(LsmBackend::open_with_key(&plain, config(), Some(&key(7))));
    assert!(keyed.contains("is not encrypted"), "{keyed}");

    // And each still opens the way it was made.
    LsmBackend::open_with_key(&encrypted, config(), Some(&key(7)))
        .expect("a test step")
        .close()
        .expect("a test step");
    LsmBackend::open(&plain, config())
        .expect("a test step")
        .close()
        .expect("a test step");
}
