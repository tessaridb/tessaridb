//! A hold that lapses, and the four things it must be true of.
//!
//! The properties asserted here are the ones the design document's guarantee
//! section states, in the same words, because a guarantee nobody tested is a
//! sentence rather than a property:
//!
//! - a claim holds a record, so a second claim does not see it;
//! - a hold **lapses**, and it lapses because a reader compares rather than
//!   because anything sweeps;
//! - the attempt count is taken at the hand-out, and a record that reaches the
//!   declared ceiling stops being handed out while staying readable;
//! - a caller cannot write the two fields the engine writes.
//!
//! The expiry test uses a queue whose timeout is a **negative-length** hold —
//! not to be clever, but because it is the only way to observe the comparison
//! without spending real time in a test. A deadline computed as `now + 0s` is
//! already in the past on the very next read, which is exactly the state a
//! lapsed hold is in, and asserting on it exercises the same branch a
//! thirty-second timeout reaches thirty seconds later.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A queue holding three pieces of work, under the declaration the test names.
fn ready<'a>(store: &'a Store, declaration: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE QUEUE jobs {declaration};\n\
             CREATE jobs:1 = {{ url: 'a' }};\n\
             CREATE jobs:2 = {{ url: 'b' }};\n\
             CREATE jobs:3 = {{ url: 'c' }};"
        ))
        .unwrap();
    session
}

/// The identities one statement answered with, in the order it answered.
fn claimed(outcome: &Outcome) -> Vec<String> {
    match outcome {
        Outcome::Records { records, .. } => records.iter().map(|(id, _)| id.to_string()).collect(),
        other => panic!("expected records, got {other:?}"),
    }
}

/// One statement's outcome.
fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session.run(script).unwrap().pop().unwrap()
}

#[test]
fn a_claim_takes_the_first_record_and_a_second_claim_does_not_see_it() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let first = run(&mut session, "CLAIM FROM jobs;");
    let second = run(&mut session, "CLAIM FROM jobs;");

    // Identity order is arrival order, because both identity kinds this store
    // issues are time-ordered — so the queue is first-in-first-out with no
    // ordering state of its own.
    assert_eq!(claimed(&first), vec!["1"], "the first claim takes the head");
    assert_eq!(
        claimed(&second),
        vec!["2"],
        "the held record is passed over rather than handed out twice"
    );
}

#[test]
fn a_claim_takes_at_most_the_number_it_asked_for() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let taken = run(&mut session, "CLAIM 2 FROM jobs;");

    assert_eq!(claimed(&taken), vec!["1", "2"]);
}

#[test]
fn an_empty_queue_answers_no_records_and_is_not_an_error() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    run(&mut session, "CLAIM 3 FROM jobs;");
    let nothing = run(&mut session, "CLAIM FROM jobs;");

    // A worker polls, so an empty queue is the ordinary case rather than a
    // fault. A refusal here would make the normal state of a caught-up queue
    // indistinguishable from a broken one.
    assert!(claimed(&nothing).is_empty());
}

#[test]
fn a_hold_lapses_and_the_record_is_handed_out_again() {
    let store = store();
    // A hold of no length: the deadline is `now`, which every later read sees as
    // passed. This is the comparison that *is* the expiry mechanism — nothing
    // sweeps, nothing fires, and there is no reaper to have failed to start.
    let mut session = ready(&store, "TIMEOUT 1ns");

    let first = run(&mut session, "CLAIM FROM jobs;");
    let again = run(&mut session, "CLAIM FROM jobs;");

    assert_eq!(claimed(&first), vec!["1"]);
    assert_eq!(
        claimed(&again),
        vec!["1"],
        "a lapsed record is claimable again, and keeps its place rather than \
         going to the back"
    );
}

#[test]
fn the_attempt_count_is_taken_at_the_hand_out() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 1ns");

    run(&mut session, "CLAIM FROM jobs;");
    run(&mut session, "CLAIM FROM jobs;");
    let read = run(&mut session, "SELECT attempts FROM jobs:1;");

    let Outcome::Records { records, .. } = read else {
        panic!("expected records");
    };
    let Value::Object(fields) = &records[0].1 else {
        panic!("expected an object");
    };
    // Two hand-outs, whatever the worker did with them — which is the point of
    // counting here: how many times a record was handed out is a fact the store
    // can observe, and how many times the work failed is not.
    assert_eq!(fields.get("attempts"), Some(&Value::from(2_i64)));
}

#[test]
fn a_record_that_reaches_the_ceiling_stops_being_handed_out_and_stays_readable() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 1ns ATTEMPTS 2");

    // Three claims against a lapsing hold: the head is handed out twice and then
    // never again, so the third claim reaches the record behind it.
    let first = run(&mut session, "CLAIM FROM jobs;");
    let second = run(&mut session, "CLAIM FROM jobs;");
    let third = run(&mut session, "CLAIM FROM jobs;");

    assert_eq!(claimed(&first), vec!["1"]);
    assert_eq!(claimed(&second), vec!["1"]);
    assert_eq!(
        claimed(&third),
        vec!["2"],
        "a record whose attempts are spent is passed over, not refused"
    );

    // The dead letter is a predicate rather than a second table — this read is
    // the whole of it, and the record still carries its payload.
    let dead = run(&mut session, "SELECT url FROM jobs WHERE attempts >= 2;");
    let Outcome::Records { records, .. } = dead else {
        panic!("expected records");
    };
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0.to_string(), "1");
}

#[test]
fn a_release_hands_the_record_back_before_its_deadline() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    run(&mut session, "CLAIM FROM jobs;");
    run(&mut session, "RELEASE jobs:1;");
    let again = run(&mut session, "CLAIM FROM jobs;");

    assert_eq!(
        claimed(&again),
        vec!["1"],
        "released before the deadline, so the head is claimable again"
    );
}

#[test]
fn a_release_does_not_touch_the_attempt_count() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    run(&mut session, "CLAIM FROM jobs;");
    run(&mut session, "RELEASE jobs:1;");
    let read = run(&mut session, "SELECT attempts FROM jobs:1;");

    let Outcome::Records { records, .. } = read else {
        panic!("expected records");
    };
    let Value::Object(fields) = &records[0].1 else {
        panic!("expected an object");
    };
    // The count was taken at the claim. Moving it here would make a deliberate
    // hand-back and a crash count differently for no reason a caller could
    // predict from the statement.
    assert_eq!(fields.get("attempts"), Some(&Value::from(1_i64)));
}

#[test]
fn a_caller_cannot_write_the_fields_the_engine_writes() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let deadline = session.run("CREATE jobs:9 = { claimed_until: 1 };");
    let attempts = session.run("UPDATE jobs:1 SET attempts = 0;");

    // Named rather than silently dropped, so a payload that happens to use one
    // of these names is told exactly what happened (Q-461).
    assert!(
        format!("{:?}", deadline.unwrap_err()).contains("claimed_until"),
        "the refusal names the field"
    );
    assert!(format!("{:?}", attempts.unwrap_err()).contains("attempts"));
}

#[test]
fn the_same_field_names_are_ordinary_on_a_table_that_is_not_a_queue() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION notes;\n\
             CREATE notes:1 = { attempts: 3, claimed_until: 'whenever' };",
        )
        .unwrap();

    // The refusal is about the **kind**, not about the words. A store that
    // already had a table with these column names keeps it.
    let read = run(&mut session, "SELECT attempts FROM notes:1;");
    let Outcome::Records { records, .. } = read else {
        panic!("expected records");
    };
    let Value::Object(fields) = &records[0].1 else {
        panic!("expected an object");
    };
    assert_eq!(fields.get("attempts"), Some(&Value::from(3_i64)));
}

#[test]
fn claiming_from_a_table_that_is_not_a_queue_is_refused() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION notes; CREATE notes:1 = { a: 1 };",
        )
        .unwrap();

    let refused = session.run("CLAIM FROM notes;");

    // Refused rather than answered as an ordinary read: `CLAIM` writes, and a
    // table that gained holds because somebody used the wrong verb would carry
    // two fields nothing maintains.
    assert!(format!("{:?}", refused.unwrap_err()).contains("NotAQueue"));
}

#[test]
fn a_claim_above_the_ceiling_is_refused_by_the_store_and_names_the_number() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let refused = session.run("CLAIM 501 FROM jobs;");

    // The grammar refuses zero because that is a shape mistake; the ceiling is
    // the store's question, and this is where it is answered.
    let reported = format!("{:?}", refused.unwrap_err());
    assert!(reported.contains("ClaimAboveCeiling"), "{reported}");
    assert!(reported.contains("500"), "the refusal names the ceiling");
}

#[test]
fn a_queue_declared_with_no_ceiling_hands_a_record_out_without_end() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 1ns");

    for _ in 0..5 {
        assert_eq!(claimed(&run(&mut session, "CLAIM FROM jobs;")), vec!["1"]);
    }

    // Unlimited has one spelling — leaving the clause out — which is why the
    // grammar refuses `ATTEMPTS 0` rather than reading it as a second one.
    assert!(
        session
            .run("DEFINE QUEUE other TIMEOUT 5s ATTEMPTS 0;")
            .is_err()
    );
}

#[test]
fn dropping_a_queue_takes_its_held_records_with_it() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    run(&mut session, "CLAIM 2 FROM jobs;");
    session.run("DROP QUEUE jobs;").unwrap();

    // A hold is a field on a record rather than a resource somebody else owns,
    // so there is nothing here to wait for and nothing to release first.
    assert!(session.run("SELECT * FROM jobs;").is_err());
}

#[test]
fn drop_queue_refuses_a_table_that_is_not_one() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");
    session.run("DEFINE COLLECTION notes;").unwrap();

    // Reported as unknown rather than as a wrong kind, the shape `DROP VECTOR`
    // already uses: a word that removed a table of another kind would be a
    // second spelling of `DROP TABLE`.
    assert!(session.run("DROP QUEUE notes;").is_err());
}
