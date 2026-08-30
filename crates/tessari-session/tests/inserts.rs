//! `INSERT`: one transaction, and identities the store produces.
//!
//! # Why the failing row is in the middle
//!
//! An implementation that writes each row as it reaches it — no transaction, or
//! a commit per row — passes every batch whose only bad row is the last one,
//! because by then there is nothing left to leave behind. It also passes every
//! batch that succeeds. The middle row is the only position that tells the two
//! implementations apart, so it is the one the test uses.
//!
//! # Why the answer is checked against the records rather than counted
//!
//! `keys 3` is satisfied by three identities that point at nothing. What the
//! caller actually needs is that the identity in position *n* addresses the
//! record written from row *n* — that is what makes the answer usable as a
//! foreign key — so each one is matched back to the row it came from.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE COLLECTION users;
DEFINE INDEX by_email ON users FIELDS email UNIQUE;
DEFINE COLLECTION readings;
",
        )
        .unwrap();
    session
}

fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .expect("one outcome")
}

fn field(record: &Value, name: &str) -> Value {
    let Value::Object(fields) = record else {
        panic!("not an object")
    };
    fields.get(name).expect("the field").clone()
}

fn text(word: &str) -> Value {
    Value::String(word.to_owned())
}

/// Every record of `users`, by identity.
fn everyone(session: &mut Session<'_>) -> Vec<(RecordId, Value)> {
    run(session, "SELECT * FROM users;")
        .records()
        .expect("records")
        .to_vec()
}

#[test]
fn the_answer_addresses_the_records_that_were_written_in_the_order_they_were_written() {
    let store = store();
    let mut session = ready(&store);

    let answered = run(
        &mut session,
        "INSERT INTO users (name, email) VALUES
           ('ada', 'ada@example.com'),
           ('grace', 'grace@example.com'),
           ('alan', 'alan@example.com');",
    );
    let keys = answered.keys().expect("keys").to_vec();
    assert_eq!(keys.len(), 3);

    let held = everyone(&mut session);
    assert_eq!(held.len(), 3);

    // Position by position: the identity answered for row *n* addresses a record
    // holding row *n*'s name. A count would pass with the identities shuffled.
    for (key, name) in keys.iter().zip(["ada", "grace", "alan"]) {
        let (_, record) = held
            .iter()
            .find(|(id, _)| id == key)
            .unwrap_or_else(|| panic!("the answered identity {key} addresses no record"));
        assert_eq!(field(record, "name"), text(name));
    }
}

#[test]
fn every_produced_identity_differs_from_every_other() {
    let store = store();
    let mut session = ready(&store);

    // `readings` rather than `users`: these rows carry no `email`, and `users`
    // holds a UNIQUE index on that field — whether two absent values collide
    // there is a question about index semantics, and answering it is not what
    // this test is for.
    let mut script = String::from("INSERT INTO readings (n) VALUES");
    for at in 0..200 {
        script.push_str(if at == 0 { " (" } else { ", (" });
        script.push_str(&at.to_string());
        script.push(')');
    }
    script.push(';');

    let answered = run(&mut session, &script);
    let keys = answered.keys().expect("keys").to_vec();
    assert_eq!(keys.len(), 200);
    assert_eq!(
        keys.iter().cloned().collect::<BTreeSet<RecordId>>().len(),
        200,
        "two rows of one batch were given the same identity"
    );
}

#[test]
fn a_row_that_fails_in_the_middle_leaves_none_of_the_batch_behind() {
    let store = store();
    let mut session = ready(&store);

    run(
        &mut session,
        "INSERT INTO users (name, email) VALUES ('grace', 'grace@example.com');",
    );
    assert_eq!(everyone(&mut session).len(), 1);

    // Row two collides on the unique index. Rows one and three are unobjectionable
    // on their own, and row one is written before the failure is reached.
    let refused = session.run(
        "INSERT INTO users (name, email) VALUES
           ('ada', 'ada@example.com'),
           ('someone', 'grace@example.com'),
           ('alan', 'alan@example.com');",
    );
    assert!(refused.is_err(), "the colliding batch was accepted");

    let held = everyone(&mut session);
    assert_eq!(
        held.len(),
        1,
        "the batch left records behind: {:?}",
        held.iter()
            .map(|(_, r)| field(r, "name"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_batch_written_later_sorts_after_one_written_before_it() {
    // Version 7 puts the millisecond in the top 48 bits and leaves everything
    // below that boundary random, so two identities produced inside the same
    // millisecond sort arbitrarily and only batches the clock separates are
    // ordered against each other. That is also the granularity the key space
    // needs: a batch's writes share a prefix rather than scattering across the
    // tree. Asserting the stronger reading would assert a property the format
    // does not provide, and would fail almost every run.
    let store = store();
    let mut session = ready(&store);

    let earlier = run(&mut session, "INSERT INTO readings (n) VALUES (1);")
        .keys()
        .expect("keys")
        .to_vec();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let later = run(
        &mut session,
        "INSERT INTO readings (n) VALUES (2), (3), (4);",
    )
    .keys()
    .expect("keys")
    .to_vec();

    let first_of_the_later = later.iter().min().expect("the later batch");
    assert!(
        earlier.iter().all(|key| key < first_of_the_later),
        "an identity produced a millisecond earlier did not sort before the batch after it"
    );
}

#[test]
fn an_insert_is_refused_where_a_bucket_would_be_written_by_hand() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE BUCKET files;").unwrap();

    let refused = session.run("INSERT INTO files (path) VALUES ('/a.txt');");
    assert!(refused.is_err(), "a bucket was written by hand");
}
