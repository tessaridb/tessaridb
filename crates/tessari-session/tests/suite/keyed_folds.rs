//! `sum_by`, `min_by` and `max_by` — the keyed folds, answered as objects —
//! at statement level, every expected value worked out from the fixture.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

/// The object `{ fruit: a, veg: b }`, in the store's own key order.
fn by_label(fruit: i64, veg: i64) -> Value {
    let mut held = std::collections::BTreeMap::new();
    held.insert("fruit".to_owned(), Value::Number(Number::Integer(fruit)));
    held.insert("veg".to_owned(), Value::Number(Number::Integer(veg)));
    Value::Object(held)
}

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Sales under two labels; one record has no amount and one no label.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION sales;\n\
             CREATE sales = { label: 'fruit', amount: 10 };\n\
             CREATE sales = { label: 'fruit', amount: 4 };\n\
             CREATE sales = { label: 'veg', amount: 7 };\n\
             CREATE sales = { label: 'veg', amount: 9 };\n\
             CREATE sales = { label: 'fruit' };\n\
             CREATE sales = { amount: 100 };",
        )
        .unwrap();
    session
}

/// The one answered value of a one-row read.
fn answered(session: &mut Session<'_>, statement: &str) -> Value {
    let Outcome::Records { records, .. } = session.run_now(statement).unwrap().pop().unwrap()
    else {
        panic!("a read answers with records");
    };
    let (_, row) = records.into_iter().next().unwrap();
    let Value::Object(fields) = row else {
        panic!("a row is an object");
    };
    fields.get("held").cloned().unwrap_or(Value::None)
}

#[test]
fn the_keyed_folds_answer_one_object_of_per_key_answers() {
    let store = store();
    let mut session = ready(&store);
    // fruit: 10 and 4 (the record with no amount writes nothing); veg: 7
    // and 9; the record with no label writes no key.
    let summed = answered(
        &mut session,
        "SELECT sum_by(amount, label) AS held FROM sales;",
    );
    assert_eq!(summed, by_label(14, 16), "{summed:?}");
    let smallest = answered(
        &mut session,
        "SELECT min_by(amount, label) AS held FROM sales;",
    );
    assert_eq!(smallest, by_label(4, 7), "{smallest:?}");
    let largest = answered(
        &mut session,
        "SELECT max_by(amount, label) AS held FROM sales;",
    );
    assert_eq!(largest, by_label(10, 9), "{largest:?}");
}

#[test]
fn a_key_that_is_not_a_string_is_refused_naming_the_fold() {
    let store = store();
    let mut session = ready(&store);
    let refused = session.run_now("SELECT sum_by(amount, amount) AS held FROM sales;");
    let Err(error) = refused else {
        panic!("ran: {refused:?}");
    };
    assert!(
        format!("{error}").contains("sum_by"),
        "refused without naming the fold: {error}"
    );
}
