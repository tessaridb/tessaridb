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

use tessari_encoding::Roles;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Currency, Error, LEASE_GUARD, Lease, RecordAddress, Store};
use tessari_types::{DatabaseId, Epoch, NamespaceId, RecordId, Sequence, TableId};

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

#[test]
fn a_node_nobody_made_a_leader_publishes_no_remainder_at_all() {
    // `None` rather than zero, and the distinction is the whole reason this is
    // an `Option`: a store standing alone is not a leader whose time has run
    // out, and reporting it as one would leave every single-node deployment
    // permanently at the alarm value.
    let store = store();
    assert!(store.health().unwrap().lease_remaining.is_none());
}

#[test]
fn a_lease_with_room_in_it_publishes_how_much() {
    let store = store();
    let ttl = LEASE_GUARD.saturating_add(Duration::from_secs(60));
    store.hold_lease(ttl);
    let left = store
        .health()
        .unwrap()
        .lease_remaining
        .expect("a lease was granted");
    // Bounded on both sides rather than compared to one number: the remainder
    // is measured to the fence, so it can never exceed `ttl - δ`, and it should
    // not have lost a whole second to the work between the two calls.
    assert!(
        left <= ttl.saturating_sub(LEASE_GUARD),
        "the remainder {left:?} reaches past the fence"
    );
    assert!(
        left >= ttl
            .saturating_sub(LEASE_GUARD)
            .saturating_sub(Duration::from_secs(1)),
        "the remainder {left:?} is implausibly short for a {ttl:?} lease"
    );
}

#[test]
fn the_remainder_runs_out_at_the_fence_and_not_at_the_expiry() {
    // The sharp property, against a stated instant so it needs no clock: at the
    // fence there is nothing left, while the grant itself still has δ to run.
    // A remainder measured to the expiry would show a writable window this node
    // is already forbidden to use.
    let taken = Instant::now();
    for seconds in [3_u64, 5, 10, 30, 300] {
        let ttl = Duration::from_secs(seconds);
        let lease = Lease::taken_at(taken, ttl);
        assert_eq!(
            lease.left(lease.fence()),
            Duration::ZERO,
            "a {ttl:?} lease still reports time left at its own fence"
        );
        assert!(
            lease.left(taken) <= ttl.saturating_sub(LEASE_GUARD),
            "a {ttl:?} lease reports a window wider than the fence allows"
        );
        assert!(
            lease.expiry() > lease.fence(),
            "a {ttl:?} lease has nothing between its fence and its expiry"
        );
    }
}

#[test]
fn the_remainder_and_the_refusal_are_one_question_asked_twice() {
    // They are derived from the same `fenced(now)`, and they must never
    // disagree: a diagnostic reporting time left while the commit is already
    // refusing is worse than no diagnostic, because it sends whoever reads it
    // to look for a bug in the write path.
    let spent = store();
    spent.hold_lease(Duration::ZERO);
    assert_eq!(
        spent.health().unwrap().lease_remaining,
        Some(Duration::ZERO)
    );
    assert!(spent.lease_spent().is_some());
    assert!(write(&spent, "refused").is_err());

    let live = store();
    live.hold_lease(LEASE_GUARD.saturating_add(Duration::from_secs(60)));
    assert!(live.health().unwrap().lease_remaining > Some(Duration::ZERO));
    assert!(live.lease_spent().is_none());
    assert!(write(&live, "taken").is_ok());
}

#[test]
fn a_node_whose_lease_lapsed_stops_reporting_that_it_may_write() {
    // §6.1 says effective role *is* the lease. Before this, a node whose lease
    // had lapsed refused every write and went on reporting `writable` — the
    // behaviour was already the lease and only the report disagreed.
    let store = store();
    let adopted = store.node_identity().unwrap().roles;
    assert!(
        adopted.has(Roles::WRITABLE),
        "a fresh store adopts a writable role, which is what makes this test mean anything"
    );

    store.hold_lease(Duration::ZERO);
    assert!(!store.effective_roles().unwrap().has(Roles::WRITABLE));
    // The adopted set is untouched: what the lease takes away is what this node
    // is *serving under*, not what it was configured to be.
    assert_eq!(store.node_identity().unwrap().roles, adopted);
}

#[test]
fn a_node_nobody_granted_a_lease_reports_every_role_it_adopted() {
    // The control the whole workspace leans on. `None` from the lease is not a
    // spent lease, and a node standing alone is not a leader running out of one.
    let store = store();
    assert_eq!(
        store.effective_roles().unwrap(),
        store.node_identity().unwrap().roles
    );
}

#[test]
fn the_role_reported_and_the_write_refused_cannot_disagree() {
    // One derivation, asserted as one. A node saying *you may write* while the
    // next write is refused sends whoever reads it to look for a bug in the
    // write path, which is the most expensive place to look.
    for ttl in [
        None,
        Some(Duration::ZERO),
        Some(LEASE_GUARD),
        Some(LEASE_GUARD.saturating_add(Duration::from_secs(60))),
    ] {
        let store = store();
        if let Some(ttl) = ttl {
            store.hold_lease(ttl);
        }
        let writable = store.effective_roles().unwrap().has(Roles::WRITABLE);
        let refused = write(&store, "probe").is_err();
        assert_eq!(
            writable, !refused,
            "reported writable={writable} while the write refused={refused}, at ttl={ttl:?}"
        );
    }
}

#[test]
fn a_node_that_may_write_is_current_as_of_now() {
    // Currency here is an identity and not a measurement: a node that may write
    // is the origin of what it holds, so there is nothing for it to be stale
    // relative to. The zero is the whole claim.
    let store = store();
    assert_eq!(store.current_as_of().unwrap(), Some(Duration::ZERO));
}

#[test]
fn a_copy_this_node_did_not_write_has_no_known_age() {
    // The refusal §C-05 asks for, at the value. This node has never collected,
    // so its copy has no arrival to be measured from, and `None` is the honest
    // answer rather than a cautious one.
    let store = store();
    store.hold_lease(Duration::ZERO);
    assert_eq!(store.current_as_of().unwrap(), None);
}

#[test]
fn a_level_collection_gives_the_copy_an_age() {
    // The other half of the same rule, and the reason the one above says
    // "never collected" rather than "cannot write". A node that asked and was
    // told there is no more was current at that instant, and its copy has an
    // age from then on.
    let store = store();
    store.hold_lease(Duration::ZERO);
    store.collected(Sequence::new(9), Currency::Level);

    let age = store
        .current_as_of()
        .unwrap()
        .expect("a node that has been level knows how old its copy is");
    assert!(age < Duration::from_secs(1), "{age:?}");
}

#[test]
fn a_bounded_collection_that_filled_its_limit_is_not_a_catch_up() {
    // The distinction this whole registry exists for. A collection that filled
    // the bound it named proves the node ASKED; if the peer held more, the copy
    // is older than the contact. Measuring from the contact would admit exactly
    // the read a staleness bound is written to exclude.
    let store = store();
    store.hold_lease(Duration::ZERO);
    store.collected(Sequence::new(9), Currency::Behind);

    assert_eq!(
        store.current_as_of().unwrap(),
        None,
        "a full answer is contact, not arrival"
    );
    // And the position is still recorded, because *how far it got* and *whether
    // it arrived* are two facts and only one of them is unknown.
    assert_eq!(
        store.collection().expect("a collection happened").reached,
        Sequence::new(9)
    );
}

#[test]
fn an_earlier_arrival_survives_a_later_collection_that_did_not_arrive() {
    // The conservative direction, and it needs saying because the opposite
    // reads as safer. A node that arrived and is now catching up has a copy
    // that was current at the arrival and has only grown older; clearing the
    // instant would report it as unknown forever, and unknown is excluded from
    // every bounded read — so a follower that is steadily keeping up would be
    // permanently unusable.
    let store = store();
    store.hold_lease(Duration::ZERO);
    store.collected(Sequence::new(4), Currency::Level);
    store.collected(Sequence::new(9), Currency::Behind);

    assert!(
        store.current_as_of().unwrap().is_some(),
        "the arrival at 4 still dates this copy"
    );
    assert_eq!(
        store.collection().expect("a collection happened").reached,
        Sequence::new(9),
        "and the position moved on"
    );
}

#[test]
fn an_uncollected_copys_currency_and_the_right_to_write_are_one_answer() {
    // The same shape as `the_role_reported_and_the_write_refused_cannot_disagree`
    // and for the same reason: two derivations of one fact drift, and this pair
    // drifting would serve a bounded read from a node that had stopped being the
    // origin of its own data — which is exactly the read the bound was asked to
    // prevent.
    //
    // RENAMED, because the claim narrowed the moment a follower could report an
    // age of its own. Currency and the right to write are one answer only for a
    // node that has never been level with anybody; for one that has, they are
    // deliberately two — that is what a bounded read routed to a replica is FOR.
    // The pair still cannot disagree here, and here is where every store in this
    // workspace that never collects lives.
    for ttl in [
        None,
        Some(Duration::ZERO),
        Some(LEASE_GUARD),
        Some(LEASE_GUARD.saturating_add(Duration::from_secs(60))),
    ] {
        let store = store();
        if let Some(ttl) = ttl {
            store.hold_lease(ttl);
        }
        let writable = store.effective_roles().unwrap().has(Roles::WRITABLE);
        let current = store.current_as_of().unwrap().is_some();
        assert_eq!(
            writable, current,
            "reported writable={writable} while current_as_of said {current}, at ttl={ttl:?}"
        );
    }
}

#[test]
fn a_node_reports_the_epoch_it_was_granted_and_nothing_before_one() {
    // `None` and `Some(0)` are different statements, exactly as they are for the
    // lease beside it. A node nobody elected is not leading under the first
    // epoch — it is not leading at all, and a greeting that reported the
    // constant zero for it would be telling every peer something about a
    // leadership that does not exist.
    let store = store();
    assert_eq!(
        store.leading(),
        None,
        "a store that never won a round claimed a leadership"
    );

    let span = LEASE_GUARD
        .checked_add(Duration::from_secs(60))
        .expect("representable");
    store.hold(Epoch::new(7), Lease::taken_at(Instant::now(), span));
    assert_eq!(store.leading(), Some(Epoch::new(7)));

    // And a renewal moves it, because the epoch a node writes under is the one
    // it most recently won and not the first one it ever did.
    store.hold(Epoch::new(8), Lease::taken_at(Instant::now(), span));
    assert_eq!(store.leading(), Some(Epoch::new(8)));

    // The local form has no round behind it and therefore no epoch to report.
    // It is the fence alone, which is all a test of the fence ever wanted.
    let alone = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    alone.hold_lease(span);
    assert_eq!(
        alone.leading(),
        None,
        "a lease taken locally invented a leadership nobody granted"
    );
}

#[test]
fn a_lease_installed_whole_keeps_the_instant_it_was_taken_at() {
    // The seam a cluster reaches through. A granted lease is dated from the
    // instant its round OPENED, so installing it must not restart that clock:
    // the collection delay comes out of the holder's own window and never out of
    // the voters'. Passing the whole lease is what carries that instant; a span
    // arriving here could only be measured from now.
    let store = store();
    let span = LEASE_GUARD
        .checked_add(Duration::from_secs(60))
        .expect("representable");
    let opened = Instant::now().checked_sub(span).expect("representable");

    store.hold(Epoch::new(4), Lease::taken_at(opened, span));
    assert!(
        store.lease_spent().is_some(),
        "a lease whose whole span was spent before it arrived is already fenced"
    );
    write(&store, "one").expect_err("and a node past its fence does not write");

    // The control: the same span, taken now. What differs between the two is the
    // instant and nothing else, so the difference is the dating.
    store.hold(Epoch::new(5), Lease::taken_at(Instant::now(), span));
    assert_eq!(store.lease_spent(), None);
    write(&store, "two").expect("inside the window it writes");
}
