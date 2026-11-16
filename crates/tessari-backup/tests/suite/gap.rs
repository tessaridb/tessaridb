//! A log that no longer reaches back to the follower is refused, not skipped.
//!
//! The three guards over this failure sit at different layers and it matters
//! which one answers here. `WrongBase` compares the *header's* `from` with the
//! store's position — and `write_from` writes the `from` it was **asked for**,
//! not the first sequence it actually found. So over a cut log the header still
//! matches the follower exactly, `WrongBase` passes, and the gap survives to
//! `Store::apply_record`, which requires `at == committed_tail + 1` and raises
//! `Error::LogGap { expected, found }` otherwise.
//!
//! That holds wherever the hole is. Moving it to the head of the stream was
//! tried and still yields `LogGap`, so the two guards do not divide into
//! head-gap and middle-gap: `WrongBase` answers when the stream begins somewhere
//! the store is not, and `LogGap` when the stream skips **within itself**. Wave
//! 66 covers the first; this file covers the second.
//!
//! The hole is nonetheless cut in the middle, for a reason the head case cannot
//! show: only then does the follower **apply something and then stop**, which is
//! what makes the position assertion below mean anything.
//!
//! The skip begins earlier than one might expect, and not in the transfer:
//! `Store::log_records(from, limit)` is a range scan from `LogKey::new(from)`
//! forward, so over a log with a hole it simply returns the first surviving
//! records after it. Nothing there notices. That is precisely why the refusal
//! has to exist further down.
//!
//! Nothing in the store truncates or prunes the log today, so the hole is made
//! here directly through the backend. The criterion is about the **refusal**,
//! not about the pruning that will one day cause it.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::RunNow;
use tessari_storage::Now;

use tessari_encoding::{AppliedPositionKey, LogId, LogKey, StoreKey, StoreValue};
use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::{Reach, Sequence};

/// The same fixture the other files in this crate use, so the sequences stay
/// comparable when one of them moves.
///
/// The mapping matters here in a way it does not elsewhere, because this file
/// addresses records by number — and the numbers now count in **two** logs
/// rather than one (S6.2). `USE` writes nothing, so the store's own log holds
/// 1 `DEFINE NAMESPACE`, 2 `DEFINE DATABASE`, 3 `DEFINE TABLE`, 4 `DEFINE
/// FIELD`, 5 `DEFINE INDEX` — the schema is carried to the whole store — and the
/// database's own log holds only the records: 1 `CREATE people:1`,
/// 2 `CREATE people:2`, **3 `DELETE people:2`**, 4 `CREATE people:3`.
///
/// Everything below counts in the database's log, and [`data_log`] is how the
/// tests name it. The guards do not trust this comment — `leader` asserts the
/// two logs and their lengths, so a change to the fixture fails here with the
/// numbers rather than somewhere further down without them.
const LEADER: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
DEFINE DATABASE orders; USE DATABASE orders;\n\
DEFINE TABLE people SCHEMALESS;\n\
DEFINE FIELD name ON people TYPE string;\n\
DEFINE INDEX by_email ON people FIELDS email UNIQUE;\n\
CREATE people:1 = { name: 'ada', email: 'a@x' };\n\
CREATE people:2 = { name: 'grace', email: 'b@x' };\n\
DELETE people:2;\n\
CREATE people:3 = { name: 'edith', email: 'c@x' };";

/// The record deliberately removed from the middle of the leader's data log.
///
/// Chosen as the `DELETE` rather than an arbitrary record because it makes the
/// cost of a silent skip observable: skipping it leaves the follower holding a
/// row the leader has removed, so the two stores would disagree about a record
/// that neither of them reports as missing.
const MISSING: u64 = 3;

/// Where the follower stands, in the data log, when the truncated stream
/// reaches it.
const FOLLOWER_STOPS_AT: u64 = 1;

/// How long the data log is when the fixture has run.
const DATA_RECORDS: u64 = 4;

/// How long the store's own log is when the fixture has run.
const STORE_RECORDS: u64 = 5;

fn store() -> (Arc<dyn KvBackend>, Store) {
    let backend: Arc<dyn KvBackend> = Arc::new(MemoryBackend::new());
    let store = Store::open(Arc::clone(&backend)).now().unwrap();
    (backend, store)
}

fn session(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    session
}

/// A leader holding the whole fixture, and its backend so the log can be cut.
fn leader() -> (Arc<dyn KvBackend>, Store) {
    let (backend, store) = store();
    {
        let mut opening = Session::new(&store);
        opening.run_now(LEADER).unwrap();
    }
    // The numbers this file addresses records by, asserted rather than trusted:
    // a fixture that grew a statement would otherwise move `MISSING` onto an
    // innocent record and every test below would pass while proving nothing.
    assert_eq!(
        crate::tails(&store),
        vec![
            (
                store.own_log(Reach::Store).now().unwrap(),
                Sequence::new(STORE_RECORDS)
            ),
            (data_log(&store), Sequence::new(DATA_RECORDS)),
        ],
        "the fixture no longer has the shape this file's constants describe"
    );
    (backend, store)
}

/// The log the fixture's records land in — the database's, not the store's.
fn data_log(store: &Store) -> LogId {
    let mut logs = store.logs().now().unwrap();
    logs.retain(|log| log.home != Reach::Store);
    match logs.as_slice() {
        [one] => *one,
        many => panic!("expected one data log, found {}", many.len()),
    }
}

/// A follower whose data log stands at `upto` and no further.
///
/// Two restores rather than one, because `upto` counts in one log (Q-625): the
/// store's own log goes over whole — the namespace and database definitions the
/// records depend on — and the data log stops where the test needs it.
fn follower_at(leader: &Store, upto: u64) -> Store {
    let (_backend, store) = store();
    let home = data_log(leader);

    let mut definitions = Vec::new();
    tessari_backup::write_from(
        leader,
        &mut definitions,
        leader.own_log(Reach::Store).now().unwrap(),
        Sequence::new(1),
    )
    .now()
    .unwrap();
    tessari_backup::read(&store, &mut definitions.as_slice())
        .now()
        .unwrap();

    let mut records = Vec::new();
    tessari_backup::write_from(leader, &mut records, home, Sequence::new(1))
        .now()
        .unwrap();
    tessari_backup::read_until(&store, &mut records.as_slice(), Some(Sequence::new(upto)))
        .now()
        .unwrap();
    assert_eq!(
        store.committed_tail(home).now().unwrap(),
        Sequence::new(upto),
        "the follower did not stop where this test needs it to"
    );
    store
}

/// Remove one record from the middle of a log.
fn cut(backend: &Arc<dyn KvBackend>, home: LogId, sequence: u64) {
    let batch = WriteBatch::new().delete(
        LogKey::keyspace(),
        LogKey::new(home, Sequence::new(sequence)).encode(),
    );
    backend.apply(batch).now().unwrap();
}

fn answers(store: &Store, script: &str) -> String {
    format!("{:?}", session(store).run_now(script).unwrap())
}

#[test]
fn a_log_that_no_longer_reaches_the_follower_is_refused_and_names_the_missing_sequence() {
    let (leader_backend, held) = leader();
    let follower = follower_at(&held, FOLLOWER_STOPS_AT);

    let home = data_log(&held);
    cut(&leader_backend, home, MISSING);
    // Vacuity guard: if the cut removed nothing, everything below passes while
    // showing nothing at all.
    let after_the_cut = held
        .log_records(home, Sequence::new(MISSING), 1)
        .now()
        .unwrap();
    assert_ne!(
        after_the_cut[0].0,
        Sequence::new(MISSING),
        "the cut removed nothing, so this test proves nothing"
    );

    // The header is honest — it begins exactly where the follower stands — so
    // the base check passes and the gap must be caught further down.
    let mut truncated = Vec::new();
    tessari_backup::write_from(
        &held,
        &mut truncated,
        home,
        Sequence::new(FOLLOWER_STOPS_AT.saturating_add(1)),
    )
    .now()
    .unwrap();

    let refused = tessari_backup::bootstrap(&follower, &mut truncated.as_slice())
        .now()
        .unwrap_err();

    // Named, not merely refused: the numbers are the point, because "it
    // errored" does not tell an operator which record to go and find.
    match refused {
        tessari_backup::Error::Store(tessari_storage::Error::LogGap { expected, found }) => {
            assert_eq!(expected, Sequence::new(MISSING));
            assert_eq!(found, Sequence::new(MISSING.saturating_add(1)));
        }
        other => panic!("refused for the wrong reason: {other}"),
    }

    // It stopped *at* the gap, having applied what came before it. The position
    // reflects what was actually applied, not what the header promised.
    assert_eq!(
        follower.committed_tail(home).now().unwrap(),
        Sequence::new(MISSING.saturating_sub(1)),
        "the follower's position does not match what it applied"
    );

    // And this is what a silent skip would have cost. The missing record deletes
    // `people:2`; the leader has run it and the follower has not, so the row
    // survives here and is gone there. A follower that skipped the gap would
    // hold a record the leader deleted, and neither store would report anything
    // missing.
    let leader_view = answers(&held, "SELECT * FROM people;");
    let follower_view = answers(&follower, "SELECT * FROM people;");
    assert_ne!(
        leader_view, follower_view,
        "the two agree, so the gap did not actually withhold anything"
    );

    // Nothing after the gap arrived either.
    assert!(
        !follower_view.contains("edith"),
        "a record from beyond the gap was applied"
    );
}

#[test]
fn the_same_truncated_log_is_refused_again_rather_than_accepted_on_retry() {
    let (leader_backend, held) = leader();
    let follower = follower_at(&held, FOLLOWER_STOPS_AT);
    let home = data_log(&held);
    cut(&leader_backend, home, MISSING);

    let mut truncated = Vec::new();
    tessari_backup::write_from(
        &held,
        &mut truncated,
        home,
        Sequence::new(FOLLOWER_STOPS_AT.saturating_add(1)),
    )
    .now()
    .unwrap();

    tessari_backup::bootstrap(&follower, &mut truncated.as_slice())
        .now()
        .unwrap_err();
    let stopped_at = follower.committed_tail(home).now().unwrap();

    // Offering the same stream again must not become an acceptance. A refusal
    // that heals itself on retry is worse than one that never fired, because a
    // caller that retries by default would never learn the log was cut — and
    // under ADR-0021 §6 re-bootstrap is an operator decision precisely because
    // the remedy is destructive.
    //
    // The *variant* changes, and that is the guards working rather than a
    // weakness. The first attempt applied the record before the gap, so the
    // follower now stands one past where the stream still says it begins —
    // which no longer describes this store. `WrongBase` answers first, before
    // the applier ever sees a record. Asserting `LogGap`
    // again would be asserting that the follower had *not* advanced, which is
    // the opposite of what the first test just proved.
    let refused_again = tessari_backup::bootstrap(&follower, &mut truncated.as_slice())
        .now()
        .unwrap_err();
    assert!(
        matches!(
            refused_again,
            tessari_backup::Error::Store(tessari_storage::Error::LogGap { .. })
                | tessari_backup::Error::WrongBase { .. }
        ),
        "the retry was refused, but by neither gap guard: {refused_again}"
    );
    assert_eq!(
        follower.committed_tail(home).now().unwrap(),
        stopped_at,
        "the retry moved the follower"
    );
}

/// Move every record of the data log to the end of the store's own log.
///
/// The layout a store keeps when its records were written before each database
/// had a log of its own: the records are filed in the store log, where the
/// writer of the day put them, while today each one derives a database home.
fn file_the_data_in_the_store_log(backend: &Arc<dyn KvBackend>, store: &Store) {
    let data = data_log(store);
    let own = store.own_log(Reach::Store).now().unwrap();
    let mut batch = WriteBatch::new();
    for (at, _) in store.log_records(data, Sequence::new(1), 64).now().unwrap() {
        let body = backend
            .get(LogKey::keyspace(), &LogKey::new(data, at).encode())
            .now()
            .unwrap()
            .unwrap();
        let moved = Sequence::new(STORE_RECORDS.saturating_add(at.get()));
        batch = batch
            .put(LogKey::keyspace(), LogKey::new(own, moved).encode(), body)
            .delete(LogKey::keyspace(), LogKey::new(data, at).encode());
    }
    let tail = Sequence::new(STORE_RECORDS.saturating_add(DATA_RECORDS));
    batch = batch
        .put(
            AppliedPositionKey::keyspace(),
            AppliedPositionKey::new(own).encode(),
            tail.encode(),
        )
        .delete(
            AppliedPositionKey::keyspace(),
            AppliedPositionKey::new(data).encode(),
        );
    backend.apply(batch).now().unwrap();
}

#[test]
fn a_record_is_restored_into_the_log_it_was_backed_up_from() {
    let (backend, held) = leader();
    file_the_data_in_the_store_log(&backend, &held);
    let own = held.own_log(Reach::Store).now().unwrap();
    // Vacuity guard: the store log must now carry records that derive a
    // database home, or this restores nothing the old path refused.
    assert_eq!(
        crate::tails(&held),
        vec![(
            own,
            Sequence::new(STORE_RECORDS.saturating_add(DATA_RECORDS))
        )],
        "the data was not re-filed into the store log"
    );

    let mut whole = Vec::new();
    tessari_backup::write(&held, &mut whole).now().unwrap();
    let (_backend, restored) = store();
    tessari_backup::read(&restored, &mut whole.as_slice())
        .now()
        .unwrap();

    assert_eq!(crate::tails(&restored), crate::tails(&held));
    assert_eq!(
        answers(&restored, "SELECT * FROM people;"),
        answers(&held, "SELECT * FROM people;"),
    );
}

#[test]
fn a_point_in_time_restore_answers_what_the_source_held_at_each_of_three_instants() {
    // C2: three instants of the data log, and each cut store holds exactly what
    // the source held then. The fixture's mid-log DELETE is the discriminator:
    // an instant before it answers with the deleted row, one after without —
    // so a replay that skipped a record would be caught by an answer, not by a
    // position. The names are the fixture's documented sequence map (1 ada,
    // 2 grace, 3 deletes grace, 4 edith), and a twin replay answers identically,
    // which is the determinism the log-is-the-store claim rests on.
    let (_backend, leader) = leader();
    // (instant, names the source held then, names it did not)
    let instants: [(u64, &[&str], &[&str]); 3] = [
        (1, &["ada"], &["grace", "edith"]),
        (2, &["ada", "grace"], &["edith"]),
        (4, &["ada", "edith"], &["grace"]),
    ];
    for (at, held, gone) in instants {
        let cut = follower_at(&leader, at);
        let home = data_log(&cut);
        assert_eq!(
            cut.committed_tail(home).now().unwrap(),
            Sequence::new(at),
            "the cut store's log stands exactly at the instant"
        );
        let mut here = session(&cut);
        let answered = format!("{:?}", here.run_now("SELECT name FROM people;").unwrap());
        for name in held {
            assert!(
                answered.contains(&format!("String(\"{name}\")")),
                "at {at} the source held {name}: {answered}"
            );
        }
        for name in gone {
            assert!(
                !answered.contains(&format!("String(\"{name}\")")),
                "at {at} the source had no {name}: {answered}"
            );
        }

        let twin = follower_at(&leader, at);
        let mut again = session(&twin);
        assert_eq!(
            format!("{:?}", again.run_now("SELECT name FROM people;").unwrap()),
            answered,
            "a twin replay of the same instant answers identically"
        );
    }
}
