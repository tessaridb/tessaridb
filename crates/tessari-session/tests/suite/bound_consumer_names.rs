//! A consumer or group name may be a bound parameter (ADR-0127).
//!
//! The name is a quoted string — the client's own data, not a catalog object —
//! so a parameter may stand where it stands, and after binding the statement is
//! the one the literal would have written. Each test drives one site with a
//! parameter and reads the effect back with the literal.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Parameters, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A session in `prod`/`shop` with a queue `jobs` of three records and a
/// topic `events` of three messages.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE QUEUE jobs TIMEOUT 1m;\n\
             CREATE jobs:1 = { url: 'a' };\n\
             CREATE jobs:2 = { url: 'b' };\n\
             CREATE jobs:3 = { url: 'c' };\n\
             DEFINE TOPIC events;\n\
             CREATE events:'m1' = { n: 1 };\n\
             CREATE events:'m2' = { n: 2 };\n\
             CREATE events:'m3' = { n: 3 };",
        )
        .unwrap();
    session
}

fn named(name: Value) -> Parameters {
    let mut supplied = Parameters::new();
    supplied.insert("name".to_owned(), name);
    supplied
}

fn billing() -> Parameters {
    named(Value::String("billing".to_owned()))
}

/// The last statement's outcome.
fn last(session: &mut Session<'_>, script: &str, supplied: &Parameters) -> Outcome {
    session.run_with(script, supplied).unwrap().pop().unwrap()
}

fn ids(outcome: &Outcome) -> Vec<String> {
    match outcome {
        Outcome::Records { records, .. } => records.iter().map(|(id, _)| id.to_string()).collect(),
        other => panic!("expected records, got {other:?}"),
    }
}

/// The consumer a queued record's hold is signed with, or `none`.
fn holder(session: &mut Session<'_>, record: &str) -> String {
    let outcome = last(
        session,
        &format!("SELECT * FROM {record};"),
        &Parameters::new(),
    );
    let Outcome::Records { records, .. } = outcome else {
        panic!("expected records");
    };
    let Some((_, Value::Object(fields))) = records.first() else {
        panic!("expected one record");
    };
    match fields.get("claimed_by") {
        Some(Value::Object(held)) => match held.get("consumer") {
            Some(Value::String(name)) => name.clone(),
            other => panic!("consumer is {other:?}"),
        },
        None => "none".to_owned(),
        other => panic!("claimed_by is {other:?}"),
    }
}

fn integer(outcome: &Outcome) -> i64 {
    match outcome {
        Outcome::Value(Value::Number(Number::Integer(held))) => *held,
        other => panic!("expected a number, got {other:?}"),
    }
}

#[test]
fn a_worker_named_by_a_parameter_signs_and_releases_its_holds_as_the_literal_would() {
    let store = store();
    let mut session = ready(&store);
    let claimed = last(
        &mut session,
        "USE CONSUMER $name; CLAIM 2 FROM jobs;",
        &billing(),
    );
    assert_eq!(ids(&claimed), vec!["1", "2"]);
    assert_eq!(holder(&mut session, "jobs:1"), "billing");
    // The group's hold, named by the parameter, is what the release clears.
    let freed = last(
        &mut session,
        "RELEASE jobs:1 FOR CONSUMER $name;",
        &billing(),
    );
    assert_eq!(freed, Outcome::Done);
    assert_eq!(holder(&mut session, "jobs:1"), "none");
    let freed = last(
        &mut session,
        "RELEASE ALL FROM jobs FOR CONSUMER $name;",
        &billing(),
    );
    assert_eq!(ids(&freed), vec!["2"]);
}

#[test]
fn a_reader_named_by_a_parameter_keeps_the_literal_readers_position() {
    let store = store();
    let mut session = ready(&store);
    let read = last(
        &mut session,
        "READ FROM events FOR CONSUMER $name LIMIT 2;",
        &billing(),
    );
    assert_eq!(ids(&read).len(), 2);
    // The literal reader is the same reader: it carries on after the second.
    let rest = last(
        &mut session,
        "READ FROM events FOR CONSUMER 'billing' LIMIT 10;",
        &Parameters::new(),
    );
    assert_eq!(ids(&rest).len(), 1);
}

#[test]
fn a_group_named_by_a_parameter_is_the_group_the_literal_names() {
    let store = store();
    let mut session = ready(&store);
    last(
        &mut session,
        "DEFINE GROUP $name ON TOPIC events ACK DEADLINE 30s IN FLIGHT 3;",
        &billing(),
    );
    let given = last(
        &mut session,
        "READ FROM events FOR CONSUMER 'billing' LIMIT 10;",
        &Parameters::new(),
    );
    assert_eq!(ids(&given).len(), 3);
    let acknowledged = last(
        &mut session,
        "ACK events FOR CONSUMER $name AT 1;",
        &billing(),
    );
    assert_eq!(integer(&acknowledged), 1);
    let returned = last(
        &mut session,
        "NACK events FOR CONSUMER $name AT 2;",
        &billing(),
    );
    assert_eq!(integer(&returned), 1);
    last(
        &mut session,
        "ALTER GROUP $name ON TOPIC events START AT 3;",
        &billing(),
    );
    let after = last(
        &mut session,
        "READ FROM events FOR CONSUMER 'billing' LIMIT 10;",
        &Parameters::new(),
    );
    assert_eq!(ids(&after).len(), 0);
    last(
        &mut session,
        "DROP GROUP $name ON TOPIC events;",
        &billing(),
    );
    let gone = session.run("ACK events FOR CONSUMER 'billing' AT 3;");
    assert!(
        gone.is_err(),
        "the group outlived DROP GROUP $name: {gone:?}"
    );
}

#[test]
fn a_name_bound_to_anything_but_a_string_with_something_in_it_is_refused() {
    let store = store();
    let mut session = ready(&store);
    for held in [
        Value::Number(Number::Integer(7)),
        Value::String(String::new()),
        Value::None,
    ] {
        let refused = session.run_with("USE CONSUMER $name;", &named(held.clone()));
        assert!(
            matches!(
                refused,
                Err(Error::Script(tessari_ql::Error::NotAConsumerName { .. }))
            ),
            "{held:?} was taken as a consumer name: {refused:?}"
        );
    }
    let unbound = session.run("READ FROM events FOR CONSUMER $name;");
    assert!(
        matches!(
            unbound,
            Err(Error::Script(tessari_ql::Error::UnboundParameter { .. }))
        ),
        "an unbound name was taken: {unbound:?}"
    );
}

#[test]
fn a_name_a_let_binds_is_taken_when_the_let_runs() {
    let store = store();
    let mut session = ready(&store);
    let claimed = last(
        &mut session,
        "LET $worker = 'billing'; USE CONSUMER $worker; CLAIM FROM jobs;",
        &Parameters::new(),
    );
    assert_eq!(ids(&claimed), vec!["1"]);
    assert_eq!(holder(&mut session, "jobs:1"), "billing");
}
