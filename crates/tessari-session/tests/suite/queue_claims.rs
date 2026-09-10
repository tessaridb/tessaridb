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

// ------------------------------------------------------------- the claimant

/// A second session on the same store, in the same tenancy.
fn beside<'a>(store: &'a Store) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// The `claimed_by` a record carries, as `consumer/instance`, or `none`.
fn holder(session: &mut Session<'_>, record: &str) -> String {
    let outcome = run(session, &format!("SELECT * FROM {record};"));
    let Outcome::Records { records, .. } = outcome else {
        panic!("expected records");
    };
    let Some((_, Value::Object(fields))) = records.first() else {
        panic!("expected one record");
    };
    match fields.get("claimed_by") {
        Some(Value::Object(held)) => format!(
            "{}/{}",
            match held.get("consumer") {
                Some(Value::String(name)) => name.clone(),
                other => panic!("consumer is {other:?}"),
            },
            match held.get("instance") {
                Some(Value::String(id)) => id.clone(),
                other => panic!("instance is {other:?}"),
            }
        ),
        None => "none".to_owned(),
        other => panic!("claimed_by is {other:?}"),
    }
}

#[test]
fn a_session_that_says_who_it_is_signs_the_hold_and_one_that_does_not_signs_nothing() {
    let store = store();
    let mut said = ready(&store, "TIMEOUT 30s");
    said.run("USE CONSUMER 'billing';").unwrap();
    run(&mut said, "CLAIM FROM jobs;");
    let signed = holder(&mut said, "jobs:1");
    assert!(signed.starts_with("billing/"), "{signed}");

    // The absence is the point: it means *nobody said*, which is a true
    // statement, where a default would have been a claim.
    let mut quiet = beside(&store);
    run(&mut quiet, "CLAIM FROM jobs;");
    assert_eq!(holder(&mut quiet, "jobs:2"), "none");
}

#[test]
fn releasing_another_claimants_hold_is_refused_and_names_the_consumer() {
    // The hole this closes: until there was a claimant to compare against, any
    // caller who could write the table could drop anybody's hold, with nothing
    // anywhere saying so.
    let store = store();
    let mut mine = ready(&store, "TIMEOUT 30s");
    mine.run("USE CONSUMER 'billing';").unwrap();
    run(&mut mine, "CLAIM jobs:1;");

    let mut theirs = beside(&store);
    theirs.run("USE CONSUMER 'reports';").unwrap();
    let refused = theirs.run("RELEASE jobs:1;").unwrap_err().to_string();
    assert!(refused.contains("billing"), "{refused}");

    // And my own hold still lets go.
    mine.run("RELEASE jobs:1;").unwrap();
    assert_eq!(holder(&mut mine, "jobs:1"), "none");
}

#[test]
fn an_unsigned_hold_stays_releasable_by_anybody() {
    // What keeps every caller written before `USE CONSUMER` working exactly as
    // it did: nothing signs their holds, so nothing refuses them.
    let store = store();
    let mut first = ready(&store, "TIMEOUT 30s");
    run(&mut first, "CLAIM jobs:1;");
    let mut other = beside(&store);
    other.run("RELEASE jobs:1;").unwrap();
}

#[test]
fn release_all_answers_the_records_it_freed_and_leaves_other_claimants_alone() {
    let store = store();
    let mut mine = ready(&store, "TIMEOUT 30s");
    mine.run("USE CONSUMER 'billing';").unwrap();
    run(&mut mine, "CLAIM 2 FROM jobs;");

    let mut theirs = beside(&store);
    theirs.run("USE CONSUMER 'reports';").unwrap();
    run(&mut theirs, "CLAIM FROM jobs;");

    // The records and not a count: a caller cannot list what it holds without
    // reading first, and a session back from a crash is the one least able to.
    let freed = claimed(&run(&mut mine, "RELEASE ALL FROM jobs;"));
    assert_eq!(freed, vec!["1".to_owned(), "2".to_owned()]);
    assert_eq!(holder(&mut mine, "jobs:1"), "none");
    assert_eq!(holder(&mut mine, "jobs:2"), "none");

    let still = holder(&mut theirs, "jobs:3");
    assert!(still.starts_with("reports/"), "{still}");
}

#[test]
fn release_all_without_a_declared_consumer_is_refused_and_names_the_statement() {
    // Both silent readings are wrong in a way that looks like success:
    // succeeding on nothing tells a worker its work was freed when it was not,
    // and freeing every unsigned hold takes work from claimants who never asked
    // this session for anything.
    let store = store();
    let mut quiet = ready(&store, "TIMEOUT 30s");
    run(&mut quiet, "CLAIM FROM jobs;");
    let refused = quiet.run("RELEASE ALL FROM jobs;").unwrap_err().to_string();
    assert!(refused.contains("USE CONSUMER"), "{refused}");
}

#[test]
fn the_same_consumer_name_shares_the_work_and_fences_nobody() {
    // The owner's correction, asserted: a single declared name reads as a
    // GROUP. Five machines writing the same name mean *we are the billing
    // workers*, and fencing them would make the fifth displace the fourth.
    let store = store();
    let mut one = ready(&store, "TIMEOUT 30s");
    one.run("USE CONSUMER 'billing';").unwrap();
    let mut two = beside(&store);
    two.run("USE CONSUMER 'billing';").unwrap();

    assert_eq!(claimed(&run(&mut one, "CLAIM FROM jobs;")), vec!["1"]);
    // The second session under the same name gets DIFFERENT work, and the
    // first one's hold is untouched — no displacement anywhere.
    assert_eq!(claimed(&run(&mut two, "CLAIM FROM jobs;")), vec!["2"]);
    assert!(holder(&mut one, "jobs:1").starts_with("billing/"));
}

#[test]
fn two_sessions_under_one_name_hold_different_instances() {
    // Which is what makes `RELEASE ALL` safe by default: the bare form reaches
    // this session's instance, so a restarted worker cannot drop a live
    // colleague's work by accident.
    let store = store();
    let mut one = ready(&store, "TIMEOUT 30s");
    one.run("USE CONSUMER 'billing';").unwrap();
    let mut two = beside(&store);
    two.run("USE CONSUMER 'billing';").unwrap();
    run(&mut one, "CLAIM jobs:1;");
    run(&mut two, "CLAIM jobs:2;");
    assert_ne!(holder(&mut one, "jobs:1"), holder(&mut two, "jobs:2"));

    let freed = claimed(&run(&mut two, "RELEASE ALL FROM jobs;"));
    assert_eq!(freed, vec!["2".to_owned()], "only its own instance");
    assert!(holder(&mut one, "jobs:1").starts_with("billing/"));
}

#[test]
fn the_named_consumer_form_reaches_the_whole_group() {
    // The operation that CAN take a live colleague's work is the one you have
    // to type. Both holds go, and that is the point of naming it.
    let store = store();
    let mut one = ready(&store, "TIMEOUT 30s");
    one.run("USE CONSUMER 'billing';").unwrap();
    let mut two = beside(&store);
    two.run("USE CONSUMER 'billing';").unwrap();
    run(&mut one, "CLAIM jobs:1;");
    run(&mut two, "CLAIM jobs:2;");

    let freed = claimed(&run(
        &mut two,
        "RELEASE ALL FROM jobs FOR CONSUMER 'billing';",
    ));
    assert_eq!(freed, vec!["1".to_owned(), "2".to_owned()]);
}

#[test]
fn declaring_a_consumer_again_mints_a_new_instance() {
    // A session that says who it is again is a new claimant from the queue's
    // side. Reusing the value would let `RELEASE ALL` reach holds the previous
    // declaration took, which is the reuse the design forbids arriving from
    // inside one session.
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");
    session.run("USE CONSUMER 'billing';").unwrap();
    run(&mut session, "CLAIM jobs:1;");
    let first = holder(&mut session, "jobs:1");

    session.run("USE CONSUMER 'billing';").unwrap();
    run(&mut session, "CLAIM jobs:2;");
    assert_ne!(first, holder(&mut session, "jobs:2"));

    // And the earlier hold is out of reach of the new instance's bare form.
    let freed = claimed(&run(&mut session, "RELEASE ALL FROM jobs;"));
    assert_eq!(freed, vec!["2".to_owned()]);
}

#[test]
fn a_caller_cannot_sign_a_hold_itself() {
    // The strongest of the three engine fields to refuse: the other two are
    // bookkeeping, this one is an assertion about WHO. A caller able to write
    // it could sign a hold with another consumer's name and then have that
    // consumer's `RELEASE ALL` drop it.
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");
    let refused = session
        .run("CREATE jobs:9 = { url: 'd', claimed_by: { consumer: 'billing' } };")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("claimed_by"), "{refused}");
}

#[test]
fn a_group_frees_a_siblings_hold_by_naming_the_record() {
    // The form a client with ONE connection and many logical callers needs. It
    // declares a name per caller, so the engine mints it a fresh instance every
    // time — and the bare `RELEASE` compares instances, which would refuse the
    // caller its own work. The group form is the same permission `RELEASE ALL
    // ... FOR CONSUMER` already grants, asked about one record instead of all
    // of them.
    let store = store();
    let mut first = ready(&store, "TIMEOUT 30s");
    first.run("USE CONSUMER 'billing';").unwrap();
    run(&mut first, "CLAIM jobs:1;");

    let mut second = beside(&store);
    second.run("USE CONSUMER 'billing';").unwrap();

    // The bare form still refuses, because the instance is not this one's —
    // that is the safety this statement is deliberately not weakening.
    let refused = second.run("RELEASE jobs:1;").unwrap_err().to_string();
    assert!(refused.contains("billing"), "{refused}");

    // Naming the group frees it.
    second
        .run("RELEASE jobs:1 FOR CONSUMER 'billing';")
        .unwrap();
    assert_eq!(holder(&mut second, "jobs:1"), "none");
}

#[test]
fn naming_a_group_that_does_not_hold_the_record_is_refused_and_names_the_one_that_does() {
    // Naming a consumer is not a master key: it asks for *that group's* hold,
    // and a record another group holds answers the same refusal the bare form
    // gives, carrying the holder so the caller learns who to ask.
    let store = store();
    let mut mine = ready(&store, "TIMEOUT 30s");
    mine.run("USE CONSUMER 'billing';").unwrap();
    run(&mut mine, "CLAIM jobs:1;");

    let mut theirs = beside(&store);
    let refused = theirs
        .run("RELEASE jobs:1 FOR CONSUMER 'reports';")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("billing"), "{refused}");
    assert!(holder(&mut mine, "jobs:1").starts_with("billing/"));
}

#[test]
fn naming_a_group_leaves_a_hold_nobody_signed_exactly_where_the_sweep_leaves_it() {
    // An unsigned hold belongs to no group, so the named form must not act as a
    // master key over every hold nobody signed. `RELEASE ALL ... FOR CONSUMER`
    // already skips them by construction; this asserts the record form agrees
    // rather than diverging quietly.
    let store = store();
    let mut quiet = ready(&store, "TIMEOUT 30s");
    run(&mut quiet, "CLAIM jobs:1;");
    let deadline = held_until(&mut quiet, "jobs:1");

    let mut named = beside(&store);
    named.run("RELEASE jobs:1 FOR CONSUMER 'billing';").unwrap();
    // Still held, and by the same deadline: the statement did nothing at all.
    assert_eq!(held_until(&mut named, "jobs:1"), deadline);

    // And the bare form goes on freeing it, as it always has.
    named.run("RELEASE jobs:1;").unwrap();
    assert_eq!(holder(&mut named, "jobs:1"), "none");
}

/// The `claimed_until` a record carries, as written, or `none`.
fn held_until(session: &mut Session<'_>, record: &str) -> String {
    let outcome = run(session, &format!("SELECT * FROM {record};"));
    let Outcome::Records { records, .. } = outcome else {
        panic!("expected records");
    };
    let Some((_, Value::Object(fields))) = records.first() else {
        panic!("expected one record");
    };
    fields
        .get("claimed_until")
        .map_or_else(|| "none".to_owned(), |until| format!("{until:?}"))
}

#[test]
fn a_strict_queue_accepts_the_fields_the_engine_writes_and_still_refuses_the_caller() {
    // The first-party consumer declares every table `SCHEMAFULL`, so a claim on
    // a strict queue is the shape it will actually use. The engine's own claim
    // write goes through the same validation a caller's write does, and none of
    // `attempts`, `claimed_until` and `claimed_by` is a field anybody declares —
    // exactly the position the vault's key set was in before it was excused by
    // kind. Before this was excused, the claim was refused with
    // `UndeclaredField { field: "attempts" }` and a strict queue was unusable.
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE QUEUE jobs TIMEOUT 30s;\n\
             ALTER TABLE jobs SET SCHEMAFULL;\n\
             DEFINE FIELD url ON jobs TYPE string REQUIRED;\n\
             CREATE jobs:1 = { url: 'a' };",
        )
        .unwrap();
    session.run("USE CONSUMER 'billing';").unwrap();
    let taken = claimed(&run(&mut session, "CLAIM FROM jobs;"));
    assert_eq!(taken, vec!["1".to_owned()]);
    assert!(holder(&mut session, "jobs:1").starts_with("billing/"));

    // The exemption opens nothing: a caller writing the same field is refused
    // by the guard that says it is the engine's, before strictness is asked.
    let refused = session
        .run("UPDATE jobs:1 SET attempts = 0;")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("written by the store"), "{refused}");
}
