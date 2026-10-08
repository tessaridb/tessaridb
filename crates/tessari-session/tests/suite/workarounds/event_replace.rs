//! `DEFINE EVENT OR REPLACE` — an event's body changed in one statement
//! (ADR-0124 D1).

use tessari_session::Error;
use tessari_types::{Number, Value};

use super::{inside, refused, rows, run, store, value};

/// The `v` each log row carries, in identity order.
fn logged(session: &mut tessari_session::Session<'_>) -> Vec<i64> {
    rows(session, "SELECT v FROM log;")
        .iter()
        .map(|row| match row.get("v") {
            Some(Value::Number(Number::Integer(held))) => *held,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn or_replace_swaps_the_body_and_the_next_write_runs_the_new_one() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE COLLECTION orders; DEFINE COLLECTION log; \
         DEFINE EVENT audit ON orders THEN CREATE log = { v: 1 };",
    );
    run(&mut session, "CREATE orders:1 = { n: 1 };");
    run(
        &mut session,
        "DEFINE EVENT OR REPLACE audit ON orders FOR CREATE THEN CREATE log = { v: 2 };",
    );
    run(&mut session, "CREATE orders:2 = { n: 2 };");
    // FOR CREATE now: an update runs nothing.
    run(&mut session, "UPDATE orders:2 MERGE { n: 3 };");
    let mut seen = logged(&mut session);
    seen.sort_unstable();
    assert_eq!(
        seen,
        vec![1, 2],
        "one write under each body, none under both"
    );
    let info = value(&mut session, "INFO FOR TABLE orders;");
    let Value::Object(fields) = info else {
        panic!("{info:?}")
    };
    let Some(Value::Array(events)) = fields.get("events") else {
        panic!("{fields:?}")
    };
    assert_eq!(
        events,
        &vec![Value::from(
            "DEFINE EVENT audit ON orders FOR CREATE THEN CREATE log = { v: 2 };"
        )],
        "one event, holding the new definition"
    );
}

#[test]
fn or_replace_defines_an_event_that_was_not_there() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE COLLECTION orders; DEFINE COLLECTION log; \
         DEFINE EVENT OR REPLACE audit ON orders THEN CREATE log = { v: 7 };",
    );
    run(&mut session, "CREATE orders:1 = { n: 1 };");
    assert_eq!(logged(&mut session), vec![7]);
}

#[test]
fn replacing_inside_a_transaction_that_writes_runs_the_new_body_for_that_write() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE COLLECTION orders; DEFINE COLLECTION log; \
         DEFINE EVENT audit ON orders THEN CREATE log = { v: 1 };",
    );
    run(
        &mut session,
        "BEGIN; DEFINE EVENT OR REPLACE audit ON orders THEN CREATE log = { v: 2 }; \
         CREATE orders:1 = { n: 1 }; COMMIT;",
    );
    assert_eq!(logged(&mut session), vec![2]);
}

#[test]
fn keep_and_replace_cannot_both_be_meant() {
    let store = store();
    let mut session = inside(&store);
    run(&mut session, "DEFINE COLLECTION orders;");
    let error = refused(
        &mut session,
        "DEFINE EVENT IF NOT EXISTS OR REPLACE audit ON orders THEN THROW 'x';",
    );
    assert!(matches!(error, Error::Script(_)), "{error:?}");
    let said = error.to_string();
    assert!(
        said.contains("IF NOT EXISTS") && said.contains("OR REPLACE"),
        "{said}"
    );
}
