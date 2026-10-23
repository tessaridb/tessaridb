//! `GROUP BY … HAVING` (ADR-0134 D5): a condition over each group, folds and
//! keys included.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Orders from three buyers: ada three times, grace twice, alan once.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             CREATE orders:1 = { who: 'ada', total: 3 };\n\
             CREATE orders:2 = { who: 'ada', total: 7 };\n\
             CREATE orders:3 = { who: 'ada', total: 1 };\n\
             CREATE orders:4 = { who: 'grace', total: 20 };\n\
             CREATE orders:5 = { who: 'grace', total: 5 };\n\
             CREATE orders:6 = { who: 'alan', total: 2 };",
        )
        .unwrap();
    session
}

/// Each row's fields, in answer order.
fn rows(session: &mut Session<'_>, statement: &str) -> Vec<Value> {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records.into_iter().map(|(_, row)| row).collect(),
        other => panic!("records, got {other:?}"),
    }
}

fn who(row: &Value) -> &str {
    match row {
        Value::Object(fields) => match fields.get("who") {
            Some(Value::String(name)) => name,
            other => panic!("a buyer, got {other:?}"),
        },
        other => panic!("a row, got {other:?}"),
    }
}

#[test]
fn having_keeps_the_groups_its_fold_holds_for() {
    let store = store();
    let mut session = ready(&store);

    let kept = rows(
        &mut session,
        "SELECT who, count(*) AS orders FROM orders GROUP BY who HAVING count(*) > 1;",
    );

    assert_eq!(kept.iter().map(who).collect::<Vec<_>>(), ["ada", "grace"]);
}

#[test]
fn the_condition_is_not_in_the_answer() {
    let store = store();
    let mut session = ready(&store);

    let kept = rows(
        &mut session,
        "SELECT who FROM orders GROUP BY who HAVING sum(total) > 20;",
    );

    // grace's 25 only, and her row carries what was projected and nothing else.
    assert_eq!(kept.len(), 1);
    let Value::Object(fields) = &kept[0] else {
        panic!("a row");
    };
    assert_eq!(fields.keys().collect::<Vec<_>>(), ["who"]);
    assert_eq!(who(&kept[0]), "grace");
}

#[test]
fn having_may_name_what_the_projection_answers_under() {
    let store = store();
    let mut session = ready(&store);

    let kept = rows(
        &mut session,
        "SELECT who, sum(total) AS spent FROM orders GROUP BY who HAVING spent < 12;",
    );

    assert_eq!(kept.iter().map(who).collect::<Vec<_>>(), ["ada", "alan"]);
    let Value::Object(fields) = &kept[0] else {
        panic!("a row");
    };
    assert_eq!(
        fields.get("spent"),
        Some(&Value::Number(Number::Integer(11)))
    );
}

#[test]
fn having_may_read_a_key() {
    let store = store();
    let mut session = ready(&store);

    let kept = rows(
        &mut session,
        "SELECT who, count(*) AS n FROM orders GROUP BY who HAVING who != 'ada' AND n > 1;",
    );

    assert_eq!(kept.iter().map(who).collect::<Vec<_>>(), ["grace"]);
}

#[test]
fn having_needs_a_group_by() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("SELECT count(*) AS n FROM orders HAVING count(*) > 1;")
        .unwrap_err()
        .to_string();

    assert!(refused.contains("`GROUP BY` before `HAVING`"), "{refused}");
}

#[test]
fn a_fold_in_where_is_still_refused() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("SELECT who FROM orders WHERE count(*) > 1 GROUP BY who;")
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("write the condition after `GROUP BY … HAVING`"),
        "{refused}"
    );
}
