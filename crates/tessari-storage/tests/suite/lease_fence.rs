//! A leader that cannot renew stops writing on its own — G024 S5.1, local half.
//!
//! The criterion's full validation partitions a leader from a majority and times
//! the refusal, which needs peers. The half that decides whether the mechanism
//! is *correct* needs none: "cannot renew" is, locally, simply *has not been
//! renewed*, and a test produces that by not renewing.
//!
//! Two properties carry the whole design and each has a test whose only subject
//! it is. **The fence closes strictly before the grant opens** — the holder
//! stops writing at `T` while the cluster may not give the leadership away until
//! `T + δ`, so a holder whose clock runs slow is never still writing when
//! somebody else is told to start. And **a store nobody granted a lease is not
//! fenced** — a node without leadership is not a leader running out of it, and
//! this is the property every other test in the workspace depends on without
//! saying so.
//!
//! The arithmetic is asserted against a stated instant rather than against the
//! clock, so the sharp property is proved without waiting and without a test
//! that fails on a slow machine.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Error, LEASE_GUARD, Lease, RecordAddress, Store};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn at(id: &str) -> RecordAddress {
    RecordAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from(id),
    )
}

/// One write, returning whatever the commit said.
fn write(store: &Store, id: &str) -> Result<(), Error> {
    let mut transaction = store.begin().unwrap();
    transaction.put(at(id), b"{}".to_vec());
    transaction.commit().map(|_| ())
}

/// The same write, rehearsed rather than applied.
fn rehearse(store: &Store, id: &str) -> Result<(), Error> {
    let mut transaction = store.begin().unwrap();
    transaction.put(at(id), b"{}".to_vec());
    transaction.dry_run()
}

#[test]
fn a_store_nobody_gave_a_lease_writes() {
    // The control, and the property every other test in this workspace leans on
    // without mentioning it. A node that was never granted leadership is not a
    // leader whose leadership has run out.
    let store = store();

    write(&store, "one").expect("no lease was ever taken, so there is no fence");
    assert!(store.lease_spent().is_none());
}

#[test]
fn a_lease_with_room_in_it_lets_the_write_through() {
    let store = store();
    store.hold_lease(LEASE_GUARD.saturating_add(Duration::from_secs(60)));

    assert!(store.lease_spent().is_none(), "the fence is still open");
    write(&store, "one").expect("the lease has a minute of room in it");
}

#[test]
fn a_lease_with_no_room_in_it_refuses_the_very_next_write() {
    // A TTL at or below the guard buys no writable window at all, so the lease
    // is spent at the instant it is taken. Deterministic: nothing waits.
    let store = store();
    store.hold_lease(Duration::ZERO);

    let refusal = write(&store, "one").unwrap_err();
    assert!(
        matches!(refusal, Error::LeaseSpent { .. }),
        "refused, and refused by name: {refusal}"
    );
}

#[test]
fn the_refusal_says_it_is_the_cluster_and_not_the_write_that_is_wrong() {
    // The category is what a client routes on. `Conflict` would tell a caller to
    // re-read and decide again, `Busy` to retry here — and both are false. The
    // write was fine and this node is the wrong node.
    let store = store();
    store.hold_lease(Duration::ZERO);

    let refusal = write(&store, "one").unwrap_err();
    assert_eq!(refusal.category().code(), "unavailable");
    assert!(
        refusal.to_string().contains("lease"),
        "an operator reading this at three in the morning is told what ran out: {refusal}"
    );
}

#[test]
fn the_holder_stops_writing_strictly_before_the_cluster_calls_the_lease_dead() {
    // The property δ exists for, asserted against a stated instant so it needs
    // no clock: if the two coincided, a holder whose clock ran slow would still
    // be writing at the moment somebody else was granted the same leadership.
    let taken_at = Instant::now();
    for seconds in [3_u64, 5, 10, 30, 300] {
        let ttl = Duration::from_secs(seconds);
        let lease = Lease::taken_at(taken_at, ttl);

        assert!(
            lease.fence() < lease.expiry(),
            "the fence must close before the grant opens, at a {seconds}s lease"
        );
        assert_eq!(
            lease.expiry().saturating_duration_since(lease.fence()),
            LEASE_GUARD,
            "and the gap between them is exactly the guard, at a {seconds}s lease"
        );
    }
}

#[test]
fn a_lease_too_short_to_be_useful_is_spent_where_it_is_taken() {
    // The floor, and it fails in the safe direction: a lease with no room grants
    // no window rather than a window running backwards.
    let taken_at = Instant::now();
    for ttl in [Duration::ZERO, Duration::from_millis(1), LEASE_GUARD] {
        let lease = Lease::taken_at(taken_at, ttl);
        assert!(
            lease.fenced(taken_at),
            "a lease of {ttl:?} is spent at the instant it is taken"
        );
    }
}

#[test]
fn renewing_reopens_the_fence() {
    let store = store();
    store.hold_lease(Duration::ZERO);
    write(&store, "one").unwrap_err();

    store.hold_lease(LEASE_GUARD.saturating_add(Duration::from_secs(60)));

    assert!(store.lease_spent().is_none());
    write(&store, "two").expect("renewal is what a leader does to keep writing");
}

#[test]
fn a_rehearsal_meets_the_fence_the_commit_would() {
    // `VERIFY` runs every check a commit runs. A fence it could not see would be
    // a refusal an operator met for the first time in production.
    let store = store();
    store.hold_lease(Duration::ZERO);

    let refusal = rehearse(&store, "one").unwrap_err();
    assert!(matches!(refusal, Error::LeaseSpent { .. }), "{refusal}");
}

#[test]
fn a_transaction_that_writes_nothing_is_not_fenced() {
    // There is nothing to fence. Refusing it would make a node that has lost its
    // leadership fail the commits of transactions that only read.
    let store = store();
    store.hold_lease(Duration::ZERO);

    let transaction = store.begin().unwrap();
    transaction
        .commit()
        .expect("an empty commit writes nothing, so there is nothing to refuse");
}
