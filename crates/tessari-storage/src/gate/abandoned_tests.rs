//! A landing whose future is dropped part way does not hold up the commits after it.
//!
//! A commit is a future, and a future can be dropped at any await. Dropped while
//! its group is on the way to the engine, it would leave the landing marked as
//! running, and every later commit would wait for an answer nobody is left to
//! record.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::FutureExt as _;
use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, KvFuture, MemoryBackend, Result, ScanRequest, Value,
    WriteBatch,
};

use tessari_types::Sequence;

use super::pending::Pending;

/// A memory backend whose first group write never finishes.
#[derive(Debug)]
struct StallsOnce {
    inner: MemoryBackend,
    stalled: AtomicBool,
}

impl KvBackend for StallsOnce {
    fn name(&self) -> &'static str {
        "stalls-once"
    }

    fn get<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, Option<Value>> {
        KvBackend::get(&self.inner, keyspace, key)
    }

    fn scan<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        KvBackend::scan(&self.inner, request)
    }

    fn sweep<'a>(&'a self, request: &'a ScanRequest) -> KvFuture<'a, Vec<(Key, Value)>> {
        KvBackend::sweep(&self.inner, request)
    }

    fn first_of_each<'a>(
        &'a self,
        keyspace: Keyspace,
        ranges: &'a [KeyRange],
    ) -> KvFuture<'a, Vec<Option<(Key, Value)>>> {
        KvBackend::first_of_each(&self.inner, keyspace, ranges)
    }

    fn count<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, u64> {
        KvBackend::count(&self.inner, keyspace, range)
    }

    fn apply(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        KvBackend::apply(&self.inner, batch)
    }

    fn apply_unsynced(&self, batch: WriteBatch) -> KvFuture<'_, ()> {
        KvBackend::apply_unsynced(&self.inner, batch)
    }

    fn sync_applied(&self) -> KvFuture<'_, ()> {
        KvBackend::sync_applied(&self.inner)
    }

    fn apply_group(
        &self,
        batches: Vec<WriteBatch>,
    ) -> Pin<Box<dyn Future<Output = (usize, Result<()>)> + Send + '_>> {
        Box::pin(async move {
            if self.stalled.swap(true, Ordering::Relaxed) {
                KvBackend::apply_group(&self.inner, batches).await
            } else {
                std::future::pending().await
            }
        })
    }

    fn groups_writes(&self) -> bool {
        true
    }

    fn background_errors(&self) -> KvFuture<'_, u64> {
        KvBackend::background_errors(&self.inner)
    }

    fn contains<'a>(&'a self, keyspace: Keyspace, key: &'a Key) -> KvFuture<'a, bool> {
        KvBackend::contains(&self.inner, keyspace, key)
    }

    fn delete_range<'a>(&'a self, keyspace: Keyspace, range: &'a KeyRange) -> KvFuture<'a, ()> {
        KvBackend::delete_range(&self.inner, keyspace, range)
    }
}

fn batch(n: u8) -> WriteBatch {
    WriteBatch::new().put(Keyspace::DATA, Key::new(vec![n]), Value::new(vec![n]))
}

#[test]
fn a_landing_dropped_part_way_leaves_the_next_one_free_to_land() {
    let pending = Pending::default();
    let backend = Arc::new(StallsOnce {
        inner: MemoryBackend::new(),
        stalled: AtomicBool::new(false),
    });

    let first = pending.stage(batch(1), Sequence::new(1));
    let mut abandoned = Box::pin(pending.land(first, backend.as_ref()));
    // Polled once: it takes the group and waits on the engine for good.
    assert!((&mut abandoned).now_or_never().is_none());
    drop(abandoned);

    let second = pending.stage(batch(2), Sequence::new(2));
    let landed = pending
        .land(second, backend.as_ref())
        .now_or_never()
        .expect("a later landing waited for one that was dropped");
    assert!(landed.is_ok(), "{landed:?}");
    assert!(pending.quiet(), "the dropped group was left pending");
}

/// A commit whose waiter is gone by the time another commit lands its batch has
/// still landed: the newest version landed counts it, so a transaction that
/// begins next reads it.
#[test]
fn a_batch_landed_for_a_waiter_that_is_gone_is_counted_as_landed() {
    let pending = Pending::default();
    let backend = MemoryBackend::new();

    let lower = pending.stage(batch(1), Sequence::new(1));
    // Its waiter is dropped before it asks for its answer.
    let _gone = pending.stage(batch(2), Sequence::new(2));
    let landed = pending
        .land(lower, &backend)
        .now_or_never()
        .expect("a memory backend lands at once");
    assert!(landed.is_ok(), "{landed:?}");

    assert_eq!(
        pending.landed_version(),
        Sequence::new(2),
        "the batch landed for a waiter that was gone is not counted as landed"
    );
}

/// An answer nobody will collect is not kept: a commit whose waiter is gone
/// leaves nothing behind once its group has landed.
#[test]
fn an_answer_for_a_waiter_that_is_gone_is_not_kept() {
    let pending = Pending::default();
    let backend = MemoryBackend::new();

    let lower = pending.stage(batch(1), Sequence::new(1));
    let gone = pending.stage(batch(2), Sequence::new(2));
    drop(gone);
    let landed = pending
        .land(lower, &backend)
        .now_or_never()
        .expect("a memory backend lands at once");
    assert!(landed.is_ok(), "{landed:?}");

    assert_eq!(
        pending.held_answers(),
        0,
        "the answer of a commit whose waiter is gone is kept for ever"
    );
}
