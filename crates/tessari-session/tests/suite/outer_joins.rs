//! `LEFT`, `RIGHT` and `FULL` joins (ADR-0134 D4): a record nothing matched
//! keeps its row, alone, with the other side absent.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Two users and three orders: ada ordered twice, grace never, and one order is
/// for nobody.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users; DEFINE COLLECTION orders;\n\
             CREATE users:1 = { name: 'ada' };\n\
             CREATE users:2 = { name: 'grace' };\n\
             CREATE orders:1 = { who: 'ada', total: 3 };\n\
             CREATE orders:2 = { who: 'ada', total: 7 };\n\
             CREATE orders:3 = { who: 'nobody', total: 1 };",
        )
        .unwrap();
    session
}

/// Each row as its identity and the sides it holds, in answer order.
fn rows(session: &mut Session<'_>, statement: &str) -> Vec<(RecordId, Vec<String>)> {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records
            .into_iter()
            .map(|(id, row)| match row {
                Value::Object(sides) => (id, sides.keys().cloned().collect()),
                other => panic!("a row, got {other:?}"),
            })
            .collect(),
        other => panic!("records, got {other:?}"),
    }
}

fn both() -> Vec<String> {
    vec!["orders".to_owned(), "users".to_owned()]
}

fn only(side: &str) -> Vec<String> {
    vec![side.to_owned()]
}

#[test]
fn a_left_join_keeps_the_user_nothing_matched_alone() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM users LEFT JOIN orders ON users.name = orders.who;"
        ),
        [
            (RecordId::Int(1), both()),
            (RecordId::Int(1), both()),
            (RecordId::Int(2), only("users")),
        ]
    );
}

#[test]
fn a_left_join_through_an_index_answers_the_same() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE INDEX by_who ON orders FIELDS who;")
        .unwrap();

    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM users LEFT OUTER JOIN orders ON users.name = orders.who;"
        ),
        [
            (RecordId::Int(1), both()),
            (RecordId::Int(1), both()),
            (RecordId::Int(2), only("users")),
        ]
    );
}

#[test]
fn a_right_join_drives_from_the_right_under_its_identities() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM users RIGHT JOIN orders ON users.name = orders.who;"
        ),
        [
            (RecordId::Int(1), both()),
            (RecordId::Int(2), both()),
            (RecordId::Int(3), only("orders")),
        ]
    );
}

#[test]
fn a_full_join_answers_both_kinds_of_unmatched() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM users FULL JOIN orders ON users.name = orders.who;"
        ),
        [
            (RecordId::Int(1), both()),
            (RecordId::Int(1), both()),
            (RecordId::Int(2), only("users")),
            (RecordId::Int(3), only("orders")),
        ]
    );
}

#[test]
fn the_where_is_asked_of_the_joined_rows() {
    let store = store();
    let mut session = ready(&store);

    // grace's row has no order, so `orders.total > 5` is false for it like any
    // comparison against an absence.
    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM users LEFT JOIN orders ON users.name = orders.who \
             WHERE orders.total > 5;"
        ),
        [(RecordId::Int(1), both())]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM users LEFT JOIN orders ON users.name = orders.who \
             WHERE orders.total = NONE;"
        ),
        [(RecordId::Int(2), only("users"))]
    );
}

#[test]
fn inner_join_is_the_bare_join_spelled_out() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM users INNER JOIN orders ON users.name = orders.who;"
        ),
        rows(
            &mut session,
            "SELECT * FROM users JOIN orders ON users.name = orders.who;"
        ),
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM users JOIN orders ON users.name = orders.who;"
        )
        .len(),
        2
    );
}

#[test]
fn an_asof_join_takes_no_outer_word() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("SELECT * FROM users LEFT ASOF JOIN orders ON users.name = orders.who;")
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("`JOIN` after the kind of join"),
        "{refused}"
    );
}
