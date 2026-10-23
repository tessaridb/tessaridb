//! `fn(…) OVER ([PARTITION BY …] [ORDER BY …])` — window functions (ADR-0134 D13).

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

/// Five sales over two regions; two north sales tie on amount.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION sales;\n\
             CREATE sales:1 = { region: 'north', day: 1, amount: 10 };\n\
             CREATE sales:2 = { region: 'north', day: 2, amount: 20 };\n\
             CREATE sales:3 = { region: 'north', day: 3, amount: 20 };\n\
             CREATE sales:4 = { region: 'south', day: 1, amount: 5 };\n\
             CREATE sales:5 = { region: 'south', day: 2, amount: 7 };",
        )
        .unwrap();
    session
}

/// The rows of a read, by the record each came from.
fn rows(session: &mut Session<'_>, statement: &str) -> Vec<(String, Value)> {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records
            .into_iter()
            .map(|(id, row)| (id.to_string(), row))
            .collect(),
        other => panic!("records, got {other:?}"),
    }
}

fn field(row: &Value, name: &str) -> Value {
    match row {
        Value::Object(fields) => fields.get(name).cloned().unwrap_or(Value::None),
        other => panic!("a row, got {other:?}"),
    }
}

fn int(value: i64) -> Value {
    Value::Number(Number::Integer(value))
}

/// One field of every row, in the order of the records the rows came from.
fn by_record(found: &[(String, Value)], name: &str) -> Vec<Value> {
    let mut found: Vec<&(String, Value)> = found.iter().collect();
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found.iter().map(|(_, row)| field(row, name)).collect()
}

#[test]
fn row_number_counts_within_each_partition_in_its_order() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT day, row_number() OVER (PARTITION BY region ORDER BY day DESC) AS n FROM sales;",
    );

    assert_eq!(
        by_record(&found, "n"),
        [int(3), int(2), int(1), int(2), int(1)]
    );
}

#[test]
fn rank_leaves_gaps_after_ties_and_dense_rank_does_not() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT rank() OVER (PARTITION BY region ORDER BY amount DESC) AS r, \
         dense_rank() OVER (PARTITION BY region ORDER BY amount DESC) AS d FROM sales;",
    );

    assert_eq!(
        by_record(&found, "r"),
        [int(3), int(1), int(1), int(2), int(1)]
    );
    assert_eq!(
        by_record(&found, "d"),
        [int(2), int(1), int(1), int(2), int(1)]
    );
}

#[test]
fn lag_and_lead_read_a_neighbour_and_fall_back_to_their_default() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT lag(amount) OVER (PARTITION BY region ORDER BY day) AS before, \
         lead(amount, 1, 0) OVER (PARTITION BY region ORDER BY day) AS after FROM sales;",
    );

    assert_eq!(
        by_record(&found, "before"),
        [Value::None, int(10), int(20), Value::None, int(5)]
    );
    assert_eq!(
        by_record(&found, "after"),
        [int(20), int(20), int(0), int(7), int(0)]
    );
}

#[test]
fn a_fold_runs_with_an_order_and_covers_the_partition_without_one() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT sum(amount) OVER (PARTITION BY region ORDER BY day) AS running, \
         sum(amount) OVER (PARTITION BY region) AS whole, count(*) OVER () AS every FROM sales;",
    );

    assert_eq!(
        by_record(&found, "running"),
        [int(10), int(30), int(50), int(5), int(12)]
    );
    assert_eq!(
        by_record(&found, "whole"),
        [int(50), int(50), int(50), int(12), int(12)]
    );
    assert_eq!(by_record(&found, "every"), vec![int(5); 5]);
}

#[test]
fn first_and_last_value_read_the_ends_of_the_frame() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT first_value(day) OVER (PARTITION BY region ORDER BY day) AS first, \
         last_value(day) OVER (PARTITION BY region) AS last FROM sales;",
    );

    assert_eq!(
        by_record(&found, "first"),
        [int(1), int(1), int(1), int(1), int(1)]
    );
    assert_eq!(
        by_record(&found, "last"),
        [int(3), int(3), int(3), int(2), int(2)]
    );
}

#[test]
fn windows_see_the_filtered_records_before_the_read_orders_and_bounds_them() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT day, row_number() OVER (ORDER BY day, amount) AS n FROM sales \
         WHERE amount > 5 ORDER BY n DESC LIMIT 1;",
    );

    assert_eq!(found.len(), 1);
    assert_eq!(field(&found[0].1, "n"), int(4));
    assert_eq!(field(&found[0].1, "day"), int(3));
}

#[test]
fn a_window_stands_beside_the_star() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT *, row_number() OVER (ORDER BY day, amount) AS n FROM sales WHERE region = 'south';",
    );

    assert_eq!(by_record(&found, "n"), [int(1), int(2)]);
    assert_eq!(by_record(&found, "amount"), [int(5), int(7)]);
}

#[test]
fn a_parameter_inside_a_window_is_bound() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "LET $none = -1; SELECT lag(amount, 1, $none) OVER (PARTITION BY region ORDER BY day) AS before \
         FROM sales;",
    );

    assert_eq!(
        by_record(&found, "before"),
        [int(-1), int(10), int(20), int(-1), int(5)]
    );
}

#[test]
fn a_window_beside_a_grouping_is_refused() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now(
            "SELECT region, row_number() OVER (ORDER BY region) AS n FROM sales GROUP BY region;",
        )
        .unwrap_err()
        .to_string();

    assert!(refused.contains("window"), "{refused}");
}

#[test]
fn a_window_answers_under_the_name_it_is_given() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("SELECT row_number() OVER () FROM sales;")
        .unwrap_err()
        .to_string();

    assert!(refused.to_lowercase().contains("as"), "{refused}");
}

#[test]
fn a_bound_never_shortens_what_a_window_counts() {
    let store = store();
    let mut session = Session::new(&store);
    // The index first, so an ordered `LIMIT` could be served by walking it.
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION sales; DEFINE INDEX by_day ON sales FIELDS day;\n\
             CREATE sales:1 = { day: 1, amount: 1 }; CREATE sales:2 = { day: 2, amount: 1 };\n\
             CREATE sales:3 = { day: 3, amount: 1 }; CREATE sales:4 = { day: 4, amount: 1 };\n\
             CREATE sales:5 = { day: 5, amount: 1 };",
        )
        .unwrap();

    for read in [
        "SELECT count(*) OVER () AS every FROM sales LIMIT 1;",
        "SELECT count(*) OVER () AS every FROM sales WHERE amount > 0 LIMIT 1;",
        "SELECT *, count(*) OVER () AS every FROM sales ORDER BY day LIMIT 1;",
    ] {
        let found = rows(&mut session, read);
        assert_eq!(found.len(), 1, "{read}");
        assert_eq!(field(&found[0].1, "every"), int(5), "{read}");
    }
}
