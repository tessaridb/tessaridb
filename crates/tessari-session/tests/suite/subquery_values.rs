//! A subquery beside a comparison answers what a SQL reader means by it
//! (ADR-0134 D3): its values for membership, its one value for `=` and the
//! orderings.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Three users, two of them with orders.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users; DEFINE COLLECTION orders;\n\
             CREATE users:1 = { name: 'ada', age: 36 };\n\
             CREATE users:2 = { name: 'grace', age: 85 };\n\
             CREATE users:3 = { name: 'alan', age: 41 };\n\
             CREATE orders:1 = { buyer: 'ada', total: 10 };\n\
             CREATE orders:2 = { buyer: 'alan', total: 30 };",
        )
        .unwrap();
    session
}

/// The names of the users a read answers, in id order.
fn names(session: &mut Session<'_>, statement: &str) -> Vec<String> {
    session
        .run_now(statement)
        .unwrap()
        .pop()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(_, record)| match record {
            Value::Object(fields) => match fields.get("name") {
                Some(Value::String(name)) => name.clone(),
                other => panic!("a name, got {other:?}"),
            },
            other => panic!("a record, got {other:?}"),
        })
        .collect()
}

#[test]
fn in_a_subquery_compares_with_its_values() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM users WHERE name IN (SELECT buyer FROM orders LIMIT 100);"
        ),
        ["ada", "alan"]
    );
}

#[test]
fn not_in_a_subquery_is_the_rest() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM users WHERE NOT (name IN (SELECT buyer FROM orders LIMIT 100));"
        ),
        ["grace"]
    );
}

#[test]
fn a_subquery_compared_with_equals_is_its_one_value() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM users WHERE age = (SELECT max(age) AS oldest FROM users);"
        ),
        ["grace"]
    );
}

#[test]
fn an_ordering_against_a_subquery_reads_its_value() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM users WHERE age < (SELECT mean(age) AS middle FROM users);"
        ),
        ["ada", "alan"]
    );
}

#[test]
fn a_subquery_that_answers_nothing_is_no_value() {
    let store = store();
    let mut session = ready(&store);

    assert!(
        names(
            &mut session,
            "SELECT * FROM users WHERE age = (SELECT age FROM users WHERE age > 100 LIMIT 1);"
        )
        .is_empty()
    );
}

#[test]
fn a_subquery_compared_as_one_value_that_answers_several_is_refused() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("SELECT * FROM users WHERE age = (SELECT age FROM users LIMIT 10);")
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("compared as one value and 3 records answered"),
        "{refused}"
    );
}

#[test]
fn a_subquery_elsewhere_still_answers_its_records() {
    let store = store();
    let mut session = ready(&store);

    let answered = session
        .run_now("RETURN (SELECT buyer FROM orders LIMIT 100);")
        .unwrap()
        .pop()
        .unwrap();

    // An array of the two records, each still an object — not their values.
    match answered.value().unwrap() {
        Value::Array(records) => {
            assert_eq!(records.len(), 2);
            assert!(matches!(&records[0], Value::Object(fields) if fields.contains_key("buyer")));
        }
        other => panic!("an array, got {other:?}"),
    }
}
