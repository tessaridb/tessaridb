//! What the turn's holder reads through the overlay is what the engine would
//! answer with the pending writes applied; what anyone else reads is the engine.
//!
//! Differential: the reference is a second store holding the same pairs with
//! the pending batch applied to it, so every merge — limits, pending deletes
//! hiding engine pairs, pending writes replacing them, reverse walks, bounds
//! that fall between keys — is judged against an answer computed without the
//! overlay's code.

#![allow(clippy::unwrap_used)]

use std::ops::Bound;
use std::sync::Arc;

use tessari_kv::{
    AsyncKvBackend as _, Key, KeyRange, Keyspace, KvBackend, MemoryBackend, ScanRequest, Value,
    WriteBatch,
};

use crate::now::Now;

use super::{Overlaid, WriteGate};

const SPACE: Keyspace = Keyspace::META;

fn key(n: u32) -> Key {
    Key::new(format!("k{n:02}").into_bytes())
}

fn stored() -> MemoryBackend {
    let backend = MemoryBackend::new();
    let mut batch = WriteBatch::new();
    for n in (0..20).step_by(2) {
        batch = batch.put(SPACE, key(n), Value::new(format!("old{n}").into_bytes()));
    }
    KvBackend::apply(&backend, batch).unwrap();
    backend
}

/// Overwrites, deletes of stored and of absent keys, and new keys between.
fn pending() -> WriteBatch {
    WriteBatch::new()
        .put(SPACE, key(4), Value::new(b"new4".to_vec()))
        .delete(SPACE, key(6))
        .delete(SPACE, key(8))
        .delete(SPACE, key(7))
        .put(SPACE, key(9), Value::new(b"new9".to_vec()))
        .put(SPACE, key(21), Value::new(b"new21".to_vec()))
        .delete(SPACE, key(0))
}

fn ranges() -> Vec<KeyRange> {
    vec![
        KeyRange::all(),
        KeyRange::prefix(b"k0"),
        KeyRange::between(key(5), key(12)),
        KeyRange::from_bounds(Bound::Excluded(key(4)), Bound::Included(key(9))),
        KeyRange::from_bounds(Bound::Excluded(key(9)), Bound::Included(key(9))),
        KeyRange::from_bounds(Bound::Unbounded, Bound::Excluded(key(1))),
    ]
}

fn requests() -> Vec<ScanRequest> {
    let mut all = Vec::new();
    for range in ranges() {
        for limit in [None, Some(1), Some(2), Some(3), Some(7), Some(50)] {
            let forward = ScanRequest {
                limit,
                ..ScanRequest::new(SPACE, range.clone())
            };
            all.push(forward.clone().reversed());
            all.push(forward);
        }
    }
    all
}

#[test]
fn the_holder_reads_the_engine_with_the_pending_writes_applied() {
    let gate = Arc::new(WriteGate::default());
    let overlay = Overlaid::new(Arc::new(stored()), Arc::clone(&gate));
    let reference = stored();
    KvBackend::apply(&reference, pending()).unwrap();

    let turn = gate.hold().now();
    let _ticket = gate.stage(pending());
    for request in requests() {
        assert_eq!(
            overlay.scan(&request).now().unwrap(),
            KvBackend::scan(&reference, &request).unwrap(),
            "{request:?}"
        );
        assert_eq!(
            overlay.sweep(&request).now().unwrap(),
            KvBackend::sweep(&reference, &request).unwrap(),
            "{request:?}"
        );
    }
    for range in ranges() {
        assert_eq!(
            overlay.count(SPACE, &range).now().unwrap(),
            KvBackend::count(&reference, SPACE, &range).unwrap(),
            "{range:?}"
        );
    }
    let firsts = ranges();
    assert_eq!(
        overlay.first_of_each(SPACE, &firsts).now().unwrap(),
        KvBackend::first_of_each(&reference, SPACE, &firsts).unwrap()
    );
    for n in 0..24 {
        assert_eq!(
            overlay.get(SPACE, &key(n)).now().unwrap(),
            KvBackend::get(&reference, SPACE, &key(n)).unwrap(),
            "k{n}"
        );
        assert_eq!(
            overlay.contains(SPACE, &key(n)).now().unwrap(),
            KvBackend::contains(&reference, SPACE, &key(n)).unwrap(),
            "k{n}"
        );
    }
    drop(turn);
}

#[test]
fn a_thread_without_the_turn_reads_the_engine_alone() {
    // The control arm: the pending writes are not durable, so nobody but the
    // turn's holder may read them.
    let gate = Arc::new(WriteGate::default());
    let overlay = Overlaid::new(Arc::new(stored()), Arc::clone(&gate));
    let engine = stored();

    let turn = gate.hold().now();
    let _ticket = gate.stage(pending());
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                for request in requests() {
                    assert_eq!(
                        overlay.scan(&request).now().unwrap(),
                        KvBackend::scan(&engine, &request).unwrap(),
                        "{request:?}"
                    );
                }
                assert_eq!(
                    overlay.get(SPACE, &key(4)).now().unwrap(),
                    KvBackend::get(&engine, SPACE, &key(4)).unwrap()
                );
            })
            .join()
            .unwrap();
    });
    drop(turn);
    // And the holder, once it has let go, is anybody else.
    assert_eq!(
        overlay.get(SPACE, &key(4)).now().unwrap(),
        KvBackend::get(&engine, SPACE, &key(4)).unwrap()
    );
}

/// The turn belongs to the task that took it, not to the thread it ran on.
///
/// Two tasks on one thread: the first holds the turn with a write staged and
/// yields; the second, which holds nothing, reads that key. It must read the
/// engine alone. A holder recorded per thread shows it the staged write.
#[test]
fn a_task_that_shares_the_holders_thread_does_not_read_its_pending_writes() {
    let gate = Arc::new(WriteGate::default());
    let overlay = Arc::new(Overlaid::new(Arc::new(stored()), Arc::clone(&gate)));
    let seen = Arc::new(std::sync::Mutex::new(None));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let tasks = tokio::task::LocalSet::new();
    tasks.block_on(&runtime, async {
        let holding = {
            let gate = Arc::clone(&gate);
            tasks.spawn_local(async move {
                let turn = gate.hold().await;
                let _ticket = gate.stage(pending());
                tokio::task::yield_now().await;
                drop(turn);
            })
        };
        let reading = {
            let (overlay, seen) = (Arc::clone(&overlay), Arc::clone(&seen));
            tasks.spawn_local(async move {
                *seen.lock().unwrap() = Some(overlay.get(SPACE, &key(4)).await.unwrap());
            })
        };
        holding.await.unwrap();
        reading.await.unwrap();
    });
    assert_eq!(
        seen.lock().unwrap().take(),
        Some(Some(Value::new(b"old4".to_vec()))),
        "a task holding no turn read a write still pending behind another's"
    );
}
