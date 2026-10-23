//! `WITH name AS (<read>) <read>` — named reads (ADR-0134 D12).

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

/// Three users in two cities, three orders.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users; DEFINE COLLECTION orders; DEFINE COLLECTION paris;\n\
             CREATE users:1 = { name: 'ada', city: 'paris' };\n\
             CREATE users:2 = { name: 'grace', city: 'paris' };\n\
             CREATE users:3 = { name: 'alan', city: 'london' };\n\
             CREATE orders:1 = { who: 'ada', total: 10 };\n\
             CREATE orders:2 = { who: 'ada', total: 30 };\n\
             CREATE orders:3 = { who: 'alan', total: 5 };\n\
             CREATE paris:1 = { name: 'a record of the table, not the named read' };",
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

fn field<'a>(row: &'a Value, name: &str) -> &'a Value {
    match row {
        Value::Object(fields) => fields.get(name).unwrap_or(&Value::None),
        other => panic!("a row, got {other:?}"),
    }
}

fn texts(found: &[Value], name: &str) -> Vec<String> {
    let mut texts: Vec<String> = found
        .iter()
        .map(|row| match field(row, name) {
            Value::String(text) => text.clone(),
            other => panic!("text, got {other:?}"),
        })
        .collect();
    texts.sort();
    texts
}

#[test]
fn a_named_read_stands_where_a_table_stands_and_shadows_one_of_its_name() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "WITH paris AS (SELECT * FROM users WHERE city = 'paris' LIMIT 10) \
         SELECT name FROM paris WHERE name != 'grace';",
    );

    assert_eq!(texts(&found, "name"), ["ada"]);
}

#[test]
fn a_later_named_read_may_read_an_earlier_one() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "WITH paris AS (SELECT * FROM users WHERE city = 'paris' LIMIT 10), \
         first AS (SELECT * FROM paris ORDER BY name LIMIT 1) \
         SELECT name FROM first;",
    );

    assert_eq!(texts(&found, "name"), ["ada"]);
}

#[test]
fn a_named_read_joins_under_its_own_name_or_an_alias() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "WITH big AS (SELECT * FROM orders WHERE total > 8 LIMIT 100) \
         SELECT users.name AS name, big.total AS total FROM users JOIN big ON users.name = big.who;",
    );
    assert_eq!(found.len(), 2);
    assert_eq!(texts(&found, "name"), ["ada", "ada"]);

    let paired = rows(
        &mut session,
        "WITH paris AS (SELECT * FROM users WHERE city = 'paris' LIMIT 10) \
         SELECT a.name AS name FROM paris AS a JOIN paris AS b ON a.city = b.city;",
    );
    assert_eq!(paired.len(), 4);
}

#[test]
fn a_named_read_reaches_a_subquery_inside_the_read() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "WITH paris AS (SELECT * FROM users WHERE city = 'paris' LIMIT 10) \
         SELECT who FROM orders WHERE who IN (SELECT name FROM paris LIMIT 10);",
    );

    assert_eq!(texts(&found, "who"), ["ada", "ada"]);
}

#[test]
fn a_named_read_states_its_ceiling_like_any_materialised_source() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("WITH paris AS (SELECT * FROM users WHERE city = 'paris') SELECT * FROM paris;")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("`LIMIT n` on the inner read"), "{refused}");
}

#[test]
fn a_name_given_twice_in_one_with_is_refused() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now(
            "WITH a AS (SELECT * FROM users LIMIT 1), a AS (SELECT * FROM orders LIMIT 1) \
             SELECT * FROM a;",
        )
        .unwrap_err()
        .to_string();

    assert!(refused.contains("named twice"), "{refused}");
}

#[test]
fn the_name_reaches_only_its_own_statement() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "WITH paris AS (SELECT * FROM users WHERE city = 'paris' LIMIT 10) SELECT * FROM paris; \
         SELECT name FROM paris;",
    );

    assert_eq!(
        texts(&found, "name"),
        ["a record of the table, not the named read"]
    );
}
