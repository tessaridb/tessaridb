//! Joins of more than two sources (ADR-0134 D14).

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

/// Two users, three orders (one for nobody), two items.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users; DEFINE COLLECTION orders; DEFINE COLLECTION items;\n\
             CREATE users:1 = { name: 'ada' };\n\
             CREATE users:2 = { name: 'grace' };\n\
             CREATE orders:1 = { who: 'ada', sku: 'pen' };\n\
             CREATE orders:2 = { who: 'ada', sku: 'ink' };\n\
             CREATE orders:3 = { who: 'nobody', sku: 'pen' };\n\
             CREATE items:1 = { sku: 'pen', price: 2 };\n\
             CREATE items:2 = { sku: 'ink', price: 5 };",
        )
        .unwrap();
    session
}

fn rows(session: &mut Session<'_>, statement: &str) -> Vec<Value> {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records.into_iter().map(|(_, row)| row).collect(),
        other => panic!("records, got {other:?}"),
    }
}

fn names(row: &Value) -> Vec<&str> {
    match row {
        Value::Object(fields) => fields.keys().map(String::as_str).collect(),
        other => panic!("a row, got {other:?}"),
    }
}

#[test]
fn a_third_side_joins_the_rows_so_far_and_the_row_stays_one_object() {
    let store = store();
    let mut session = ready(&store);

    let joined = rows(
        &mut session,
        "SELECT * FROM users JOIN orders ON users.name = orders.who \
         JOIN items ON orders.sku = items.sku;",
    );

    assert_eq!(joined.len(), 2);
    assert_eq!(names(&joined[0]), ["items", "orders", "users"]);
}

#[test]
fn each_side_is_reachable_by_its_name_in_where_and_the_projection() {
    let store = store();
    let mut session = ready(&store);

    let joined = rows(
        &mut session,
        "SELECT users.name AS who, items.price AS price FROM users \
         JOIN orders ON users.name = orders.who JOIN items ON items.sku = orders.sku \
         WHERE items.price > 3;",
    );

    assert_eq!(joined.len(), 1);
    let Value::Object(fields) = &joined[0] else {
        panic!("a row");
    };
    assert_eq!(fields.get("who"), Some(&Value::from("ada")));
    assert_eq!(fields.get("price"), Some(&Value::Number(5_i64.into())));
}

#[test]
fn an_outer_join_in_the_chain_keeps_what_nothing_matched() {
    let store = store();
    let mut session = ready(&store);

    let joined = rows(
        &mut session,
        "SELECT * FROM orders JOIN items ON orders.sku = items.sku \
         LEFT JOIN users ON orders.who = users.name;",
    );

    // Three orders, each with its item; `nobody`'s has no user.
    assert_eq!(joined.len(), 3);
    assert!(joined.iter().any(|row| names(row) == ["items", "orders"]));
}

#[test]
fn a_name_used_twice_in_the_chain_is_refused() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now(
            "SELECT * FROM users JOIN orders ON users.name = orders.who \
             JOIN users ON orders.who = users.name;",
        )
        .unwrap_err()
        .to_string();

    assert!(refused.contains("users"), "{refused}");
}
