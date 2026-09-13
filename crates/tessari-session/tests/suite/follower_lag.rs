//! What a leader knows about a follower it never calls — G024 S6.1.
//!
//! A follower collects by asking, and the door it asks through is the only way
//! to reach the log for a peer. So the leader has always handled every byte a
//! follower holds; what it never did was keep what it handled. It keeps it now,
//! and publishes two numbers per follower.
//!
//! **Two numbers, because each is blind to a failure the other sees.** A
//! follower that stops collecting while the leader is idle is behind by nothing
//! at all — in sequences it really is level, and only the time since it last
//! asked grows. A follower that keeps asking but cannot keep up is in touch
//! every second, and only the sequence count grows. Two of these tests exist
//! only to hold those two cases apart; delete either number and exactly one of
//! them fails.
//!
//! Nothing here opens a connection to a follower. The follower's own pull is
//! the report, which is what makes the measurement survive the follower being
//! unreachable — the state it most needs to be measurable in.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::{Reach, Store};
use tessari_types::{Sequence, Value};

const PASSWORD: &str = "correct horse battery";

/// One follower and a second one, so that "per follower" can be wrong rather
/// than merely absent: a leader keeping a single global position would pass
/// every test that only ever has one follower in it.
const ONE_FOLLOWER: [u8; 16] = [7; 16];
const ANOTHER_FOLLOWER: [u8; 16] = [9; 16];

/// A leader with a tenant, an owner and an account that may replicate.
fn leader() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut opening = Session::new(&store);
    opening
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders SCHEMALESS;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = signed_in(&store, "root");
    root.run(&format!(
        "DEFINE USER node AUTHORITIES replicate PASSWORD '{PASSWORD}';"
    ))
    .unwrap();
    store
}

/// Signed in, retrying the node's admission bound the way a real client does.
fn signed_in<'store>(store: &'store Store, name: &str) -> Session<'store> {
    let mut session = Session::new(store);
    for _ in 0..1_000 {
        match session.sign_in(name, PASSWORD) {
            Ok(()) => return session,
            Err(Error::SignInThrottled) => std::thread::sleep(Duration::from_millis(2)),
            Err(refused) => panic!("sign-in failed for a reason other than load: {refused}"),
        }
    }
    panic!("the node never admitted a sign-in for {name}");
}

/// Commit `count` records, so the leader's tail moves.
fn writes(store: &Store, count: usize, from: usize) {
    let mut root = signed_in(store, "root");
    root.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    for offset in 0..count {
        root.run(&format!(
            "CREATE orders:{} = {{ total: 1 }};",
            from.saturating_add(offset)
        ))
        .unwrap();
    }
}

/// One follower collects, naming itself, and is told what it is told.
fn collects(store: &Store, node: [u8; 16], from: Sequence, limit: usize) -> usize {
    let mut follower = signed_in(store, "node");
    follower
        .replicate_from(store, node, Reach::Store, from, limit)
        .unwrap()
        .len()
}

/// What one follower row says: `(sequence, behind, quiet_for)`.
struct Reported {
    sequence: i64,
    behind: i64,
    quiet_for: tessari_types::Duration,
    copy_age: Option<tessari_types::Duration>,
}

/// The follower rows `INFO FOR NODE` publishes, by node id.
fn followers(store: &Store) -> Vec<([u8; 16], Reported)> {
    let outcomes = signed_in(store, "root").run("INFO FOR NODE;").unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.first() else {
        panic!("not a report: {outcomes:?}");
    };
    let Some(Value::Object(cluster)) = fields.get("cluster") else {
        panic!("no cluster group: {fields:?}");
    };
    let Some(Value::Array(rows)) = cluster.get("followers") else {
        panic!("no followers list: {cluster:?}");
    };
    rows.iter()
        .map(|row| {
            let Value::Object(row) = row else {
                panic!("not a follower row: {row:?}");
            };
            let Some(Value::Uuid(node)) = row.get("node") else {
                panic!("no node id: {row:?}");
            };
            (
                *node,
                Reported {
                    sequence: number(row.get("sequence")),
                    behind: number(row.get("behind")),
                    quiet_for: duration(row.get("quiet_for")),
                    copy_age: maybe_duration(row.get("copy_age")),
                },
            )
        })
        .collect()
}

fn number(found: Option<&Value>) -> i64 {
    match found {
        Some(Value::Number(tessari_types::Number::Integer(value))) => *value,
        other => panic!("not a whole number: {other:?}"),
    }
}

/// A span that may be absent — `null` where the leader cannot state an age.
///
/// Absent is not zero here, and the distinction is the whole point of the
/// column: zero says *this copy is current*, `null` says *this copy is older
/// than anything I dated*, which is beyond every bound.
fn maybe_duration(found: Option<&Value>) -> Option<tessari_types::Duration> {
    match found {
        Some(Value::Null) => None,
        Some(Value::Duration(span)) => Some(*span),
        other => panic!("neither a span of time nor null: {other:?}"),
    }
}

fn duration(found: Option<&Value>) -> tessari_types::Duration {
    match found {
        Some(Value::Duration(span)) => *span,
        other => panic!("not a span of time: {other:?}"),
    }
}

/// The one row there should be, or a panic naming what was there instead.
fn only(store: &Store) -> Reported {
    let mut rows = followers(store);
    assert_eq!(rows.len(), 1, "expected exactly one follower");
    rows.pop().unwrap().1
}

#[test]
fn a_leader_nobody_has_collected_from_publishes_no_followers_at_all() {
    // Absent rather than zero, and the difference is the same one the desired
    // role draws: *we have never heard from it* and *it is level* are different
    // statements, and a zero row would spell them the same way.
    let store = leader();
    writes(&store, 3, 1);

    assert!(
        followers(&store).is_empty(),
        "a leader publishes followers it has served, not followers it imagines"
    );
}

#[test]
fn collecting_puts_the_follower_on_the_list_at_the_position_it_reached() {
    let store = leader();
    writes(&store, 3, 1);

    let served = collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);
    assert!(served > 0, "there was a log to collect");

    let row = only(&store);
    assert_eq!(row.behind, 0, "it collected everything there was");
    assert!(
        row.sequence > 0,
        "the position it reached is the leader's own tail, not zero"
    );
}

#[test]
fn a_follower_that_stops_collecting_falls_behind_by_every_commit_since() {
    // The busy-leader case: the sequence unit is what sees this one.
    let store = leader();
    writes(&store, 2, 1);
    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);

    writes(&store, 3, 10);

    assert_eq!(
        only(&store).behind,
        3,
        "three commits have happened that this follower has not been given"
    );
}

#[test]
fn on_an_idle_leader_only_the_time_says_a_follower_has_stopped() {
    // The arm that isolates the time unit. Nothing is written after the pull,
    // so in sequences this follower is perfectly well — and it is, which is
    // exactly why the sequence count cannot be the only number published.
    let store = leader();
    writes(&store, 2, 1);
    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);

    std::thread::sleep(Duration::from_millis(5));

    let row = only(&store);
    assert_eq!(row.behind, 0, "an idle leader leaves nobody behind");
    assert!(
        row.quiet_for > tessari_types::Duration::new(0, 0).unwrap(),
        "time has passed since it last collected, and only this says so"
    );
}

#[test]
fn a_follower_that_cannot_keep_up_is_behind_while_still_in_touch() {
    // The arm that isolates the sequence unit. This follower asked a moment
    // ago, so by contact it is healthy; it is short by everything its limit
    // would not carry.
    let store = leader();
    writes(&store, 6, 1);

    let served = collects(&store, ONE_FOLLOWER, Sequence::new(1), 1);
    assert_eq!(served, 1, "it asked for one and was given one");

    let row = only(&store);
    assert!(
        row.behind > 0,
        "it is short of the tail, and only this number says so"
    );
    assert!(
        row.quiet_for < tessari_types::Duration::new(5, 0).unwrap(),
        "it collected a moment ago, so by contact it looks entirely well"
    );
}

#[test]
fn an_empty_collection_is_still_a_collection() {
    // A follower that is level polls and is given nothing. Treating that as
    // silence would report the healthiest follower there is as absent.
    let store = leader();
    writes(&store, 2, 1);
    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);
    let reached = only(&store).sequence;

    let served = collects(
        &store,
        ONE_FOLLOWER,
        Sequence::new(u64::try_from(reached).unwrap().saturating_add(1)),
        256,
    );

    assert_eq!(served, 0, "there was nothing left to give it");
    let row = only(&store);
    assert_eq!(row.sequence, reached, "it still holds what it held");
    assert_eq!(row.behind, 0, "and it is still level");
}

#[test]
fn a_follower_that_asks_again_from_further_back_is_recorded_further_back() {
    // The last thing a follower said about itself is the current answer. A
    // high-water mark would hide a follower rewinding, which is what recovering
    // from a divergence looks like and is the case an operator most needs.
    let store = leader();
    writes(&store, 4, 1);
    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);
    let reached = only(&store).sequence;

    collects(&store, ONE_FOLLOWER, Sequence::new(2), 1);

    let row = only(&store);
    assert!(
        row.sequence < reached,
        "it asked from further back, so that is where it is"
    );
    assert!(row.behind > 0, "and being further back is being behind");
}

#[test]
fn two_followers_are_two_rows_and_neither_moves_the_other() {
    let store = leader();
    writes(&store, 4, 1);

    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);
    collects(&store, ANOTHER_FOLLOWER, Sequence::new(1), 1);

    let rows = followers(&store);
    assert_eq!(rows.len(), 2, "two followers collected, so two rows");
    let ahead = rows
        .iter()
        .find(|(node, _)| *node == ONE_FOLLOWER)
        .expect("the one that took everything");
    let behind = rows
        .iter()
        .find(|(node, _)| *node == ANOTHER_FOLLOWER)
        .expect("the one that took a single record");
    assert_eq!(ahead.1.behind, 0, "it collected the whole log");
    assert!(
        behind.1.behind > 0,
        "it collected one record, and its neighbour's progress is not its own"
    );
}

/// Zero, as a span, for comparing a measured one against.
fn no_time() -> tessari_types::Duration {
    tessari_types::Duration::new(0, 0).unwrap()
}

#[test]
fn a_leader_that_has_dated_nothing_states_no_copy_age_at_all() {
    // Unknown rather than zero, for the reason `Store::current_as_of` answers
    // `None`: a leader with no timeline has not measured a current copy, it has
    // measured nothing, and zero would be a claim it cannot support.
    let store = leader();
    writes(&store, 3, 1);
    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);

    assert!(
        only(&store).copy_age.is_none(),
        "a leader that never dated its own tail can date nobody's copy"
    );
}

#[test]
fn a_follower_level_with_a_dated_tail_holds_a_copy_with_no_age_to_speak_of() {
    let store = leader();
    writes(&store, 3, 1);
    store.mark_tail().unwrap();
    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);

    let row = only(&store);
    assert_eq!(row.behind, 0, "it collected everything there was");
    let age = row
        .copy_age
        .expect("the leader dated the tail it served from");
    assert!(
        age < tessari_types::Duration::new(5, 0).unwrap(),
        "a copy taken from a tail dated a moment ago is not old: {age:?}"
    );
}

#[test]
fn the_age_of_a_copy_does_not_grow_while_the_leader_writes_nothing() {
    // The test that holds `copy_age` and `quiet_for` apart, and the reason
    // Q-542 was not answered by accepting `quiet_for`. This follower is level
    // and the leader is idle, so its copy is perfectly current — while the time
    // since it last asked goes on growing, because there is nothing to ask for.
    let store = leader();
    writes(&store, 2, 1);
    store.mark_tail().unwrap();
    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);

    std::thread::sleep(Duration::from_millis(40));
    // The cadence keeps running on an idle leader; this is one of its rounds.
    store.mark_tail().unwrap();

    let row = only(&store);
    let age = row.copy_age.expect("the tail is dated");
    assert!(
        row.quiet_for > age,
        "on an idle leader the silence outgrows the copy's age: quiet_for {:?} against copy_age {age:?}",
        row.quiet_for
    );
    assert!(
        age < tessari_types::Duration::new(1, 0).unwrap(),
        "nothing was written, so the copy did not age: {age:?}"
    );
}

#[test]
fn a_copy_older_than_every_position_the_leader_dated_has_no_age_it_can_state() {
    let store = leader();
    writes(&store, 4, 1);
    let served = collects(&store, ONE_FOLLOWER, Sequence::new(1), 1);
    assert_eq!(served, 1, "it asked for one and was given one");

    // Everything this leader has dated is beyond what that follower holds.
    writes(&store, 4, 10);
    store.mark_tail().unwrap();

    let row = only(&store);
    assert!(row.behind > 0, "it is short of the tail");
    assert!(
        row.copy_age.is_none(),
        "a copy from before the leader's timeline has no age it can state"
    );
}

#[test]
fn two_followers_at_different_positions_are_given_different_ages() {
    // Per follower and not per leader: a single global reading would pass every
    // test above and fail only this one.
    let store = leader();
    writes(&store, 2, 1);
    store.mark_tail().unwrap();
    collects(&store, ONE_FOLLOWER, Sequence::new(1), 256);

    std::thread::sleep(Duration::from_millis(40));
    writes(&store, 2, 10);
    store.mark_tail().unwrap();
    collects(&store, ANOTHER_FOLLOWER, Sequence::new(1), 256);

    let rows = followers(&store);
    assert_eq!(rows.len(), 2, "two followers have collected");
    let older = rows
        .iter()
        .find(|(node, _)| *node == ONE_FOLLOWER)
        .expect("the first follower is listed")
        .1
        .copy_age
        .expect("its position was dated");
    let newer = rows
        .iter()
        .find(|(node, _)| *node == ANOTHER_FOLLOWER)
        .expect("the second follower is listed")
        .1
        .copy_age
        .expect("its position was dated");

    assert!(
        older > newer,
        "the follower holding the earlier tail holds the older copy: {older:?} against {newer:?}"
    );
    assert!(
        older > no_time(),
        "and that age is a measured span, not zero"
    );
}
