//! `UNION [ALL]`, `INTERSECT` and `EXCEPT` (ADR-0134 D6).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Two lists of cities, `paris` on both and written once as `Paris`.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION customers; DEFINE COLLECTION suppliers;\n\
             CREATE customers:1 = { city: 'paris' };\n\
             CREATE customers:2 = { city: 'london' };\n\
             CREATE customers:3 = { city: 'london' };\n\
             CREATE suppliers:1 = { city: 'paris' };\n\
             CREATE suppliers:2 = { city: 'rome' };",
        )
        .unwrap();
    session
}

/// The cities a read answers, in answer order.
fn cities(session: &mut Session<'_>, statement: &str) -> Vec<String> {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records
            .into_iter()
            .map(|(_, row)| match row {
                Value::Object(fields) => match fields.get("city") {
                    Some(Value::String(city)) => city.clone(),
                    other => panic!("a city, got {other:?}"),
                },
                other => panic!("a row, got {other:?}"),
            })
            .collect(),
        other => panic!("records, got {other:?}"),
    }
}

#[test]
fn union_answers_each_value_once() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        cities(
            &mut session,
            "SELECT city FROM customers UNION SELECT city FROM suppliers;"
        ),
        ["paris", "london", "rome"]
    );
}

#[test]
fn union_all_keeps_every_record() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        cities(
            &mut session,
            "SELECT city FROM customers UNION ALL SELECT city FROM suppliers;"
        ),
        ["paris", "london", "london", "paris", "rome"]
    );
}

#[test]
fn intersect_answers_what_both_answer() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        cities(
            &mut session,
            "SELECT city FROM customers INTERSECT SELECT city FROM suppliers;"
        ),
        ["paris"]
    );
}

#[test]
fn except_answers_what_only_the_first_answers() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        cities(
            &mut session,
            "SELECT city FROM customers EXCEPT SELECT city FROM suppliers;"
        ),
        ["london"]
    );
}

#[test]
fn the_combinations_apply_left_to_right() {
    let store = store();
    let mut session = ready(&store);

    // (customers ∪ suppliers) − suppliers, not customers ∪ (suppliers − suppliers).
    assert_eq!(
        cities(
            &mut session,
            "SELECT city FROM customers UNION SELECT city FROM suppliers \
             EXCEPT SELECT city FROM suppliers;"
        ),
        ["london"]
    );
}

#[test]
fn a_trailing_limit_is_the_last_reads_own() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        cities(
            &mut session,
            "SELECT city FROM customers UNION ALL SELECT city FROM suppliers LIMIT 1;"
        ),
        ["paris", "london", "london", "paris"]
    );
}

#[test]
fn a_combined_read_orders_and_bounds_as_a_source() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        cities(
            &mut session,
            "SELECT * FROM (SELECT city FROM customers LIMIT 10 UNION SELECT city FROM suppliers LIMIT 10) \
             ORDER BY city LIMIT 2;"
        ),
        ["london", "paris"]
    );
}

#[test]
fn all_follows_union_only() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("SELECT city FROM customers INTERSECT ALL SELECT city FROM suppliers;")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("only `UNION` makes them"), "{refused}");
}

/// How many identities an insert answered.
fn inserted(session: &mut Session<'_>, statement: &str) -> usize {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Keys(identities) => identities.len(),
        other => panic!("identities, got {other:?}"),
    }
}

#[test]
fn insert_select_writes_one_record_per_record_read() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("DEFINE COLLECTION places;").unwrap();

    assert_eq!(
        inserted(
            &mut session,
            "INSERT INTO places (name) SELECT city FROM customers UNION SELECT city FROM suppliers;"
        ),
        3
    );
    // By position: `city` became `name`.
    let mut names = Vec::new();
    match session
        .run_now("SELECT * FROM places;")
        .unwrap()
        .pop()
        .unwrap()
    {
        Outcome::Records { records, .. } => {
            for (_, record) in records {
                let Value::Object(fields) = record else {
                    panic!("a record");
                };
                assert!(!fields.contains_key("city"), "{fields:?}");
                if let Some(Value::String(name)) = fields.get("name") {
                    names.push(name.clone());
                }
            }
        }
        other => panic!("records, got {other:?}"),
    }
    names.sort();
    assert_eq!(names, ["london", "paris", "rome"]);
}

#[test]
fn insert_select_into_the_table_it_reads_does_not_read_its_own_inserts() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        inserted(
            &mut session,
            "INSERT INTO customers (city) SELECT city FROM customers;"
        ),
        3
    );
    assert_eq!(cities(&mut session, "SELECT city FROM customers;").len(), 6);
}

#[test]
fn insert_select_needs_one_value_per_column() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("INSERT INTO customers (city, country) SELECT city FROM suppliers;")
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("holds 1 values, and the field list names 2"),
        "{refused}"
    );
    assert!(
        session
            .run_now("INSERT INTO customers (city) SELECT * FROM suppliers;")
            .unwrap_err()
            .to_string()
            .contains("not `*`")
    );
}
