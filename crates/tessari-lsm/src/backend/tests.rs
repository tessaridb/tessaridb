// Test assertions are exactly where a panic is the correct outcome.
#![allow(clippy::panic, clippy::unwrap_used)]

use super::*;

#[test]
fn a_panic_while_writing_does_not_leave_the_store_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let backend =
        LsmBackend::open(dir.path(), StoreConfig::new(Durability::ProcessCrashSafe)).unwrap();
    let poisoned = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _writer = backend.write_lock.lock();
                std::panic::resume_unwind(Box::new("a defect while holding the write lock"));
            })
            .join()
    });
    assert!(poisoned.is_err(), "the helper thread must have panicked");
    assert!(backend.write_lock.is_poisoned());

    let key = Key::from_slice(b"after the panic");
    let batch = WriteBatch::default().put(Keyspace::INDEX, key.clone(), Value::new(vec![7]));
    backend.apply(batch).unwrap();
    assert_eq!(
        backend.get(Keyspace::INDEX, &key).unwrap(),
        Some(Value::new(vec![7]))
    );
    backend
        .delete_range(
            Keyspace::INDEX,
            &KeyRange::between(Key::from_slice(b"a"), Key::from_slice(b"z")),
        )
        .unwrap();
    assert_eq!(backend.get(Keyspace::INDEX, &key).unwrap(), None);
}

#[test]
fn a_synced_write_covers_the_applies_that_landed_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let backend =
        LsmBackend::open(dir.path(), StoreConfig::new(Durability::PowerLossSafe)).unwrap();
    let put = |n: u8| {
        WriteBatch::default().put(Keyspace::INDEX, Key::from_slice(&[n]), Value::new(vec![n]))
    };
    backend.apply_unsynced(put(1)).unwrap();
    let applied = backend.syncs.landed_so_far();
    assert!(
        !backend.syncs.covered(applied),
        "an unsynced apply is not synced"
    );
    backend.apply(put(2)).unwrap();
    assert!(
        backend.syncs.covered(applied),
        "a commit synced after the apply landed covers it"
    );
    backend.apply_unsynced(put(3)).unwrap();
    assert!(!backend.syncs.covered(backend.syncs.landed_so_far()));
    backend.sync_applied().unwrap();
    assert!(backend.syncs.covered(backend.syncs.landed_so_far()));
}

#[test]
fn writes_landed_unsynced_are_kept_once_synced_at_either_durability() {
    for durability in [Durability::PowerLossSafe, Durability::ProcessCrashSafe] {
        let dir = tempfile::tempdir().unwrap();
        let backend = LsmBackend::open(dir.path(), StoreConfig::new(durability)).unwrap();
        for n in 0..3_u8 {
            let batch = WriteBatch::default().put(
                Keyspace::INDEX,
                Key::from_slice(&[n]),
                Value::new(vec![n]),
            );
            backend.apply_unsynced(batch).unwrap();
        }
        backend.sync_applied().unwrap();
        backend.close().unwrap();

        let reopened = LsmBackend::open(dir.path(), StoreConfig::new(durability)).unwrap();
        for n in 0..3_u8 {
            assert_eq!(
                reopened
                    .get(Keyspace::INDEX, &Key::from_slice(&[n]))
                    .unwrap(),
                Some(Value::new(vec![n])),
                "{durability:?}"
            );
        }
    }
}

#[test]
fn the_successor_is_greater_than_the_key_and_below_anything_after_it() {
    let key = Key::from_slice(b"ab");
    let next = successor(&key);
    assert!(next.as_slice() > key.as_slice());
    assert!(next.as_slice() < b"ab\x01".as_slice());
    assert!(next.as_slice() < b"ac".as_slice());
}

#[test]
fn the_successor_of_the_empty_key_is_the_first_key_after_it() {
    assert_eq!(successor(&Key::from_slice(b"")), vec![0x00]);
}
