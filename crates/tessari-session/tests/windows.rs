//! Grouping by a window, which is grouping by an expression.
//!
//! A time-series question is "how many per hour", and until `GROUP BY` took an
//! expression there was no way to say it — a caller had to store the bucket
//! beside the instant and keep the two in step by hand, which is a denormalised
//! column that goes wrong the first time somebody changes the window.
//!
//! Two things make it work, and only one of them is about time. `GROUP BY` takes
//! an expression, so any function of a record can be a key; and `time::bucket`
//! exists because truncating an instant to a multiple of a duration is the one
//! thing the language could not already say.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE orders; USE DATABASE orders;\n\
             DEFINE COLLECTION readings;",
        )
        .unwrap();
    session
}

/// Six readings: three in the first hour of the day, two in the second, one in
/// the fourth — so the answer has three windows of different sizes and a gap.
fn populate(session: &mut Session<'_>) {
    for (n, at) in [
        (1, "2026-03-01T00:05:00Z"),
        (2, "2026-03-01T00:35:00Z"),
        (3, "2026-03-01T00:59:59Z"),
        (4, "2026-03-01T01:00:00Z"),
        (5, "2026-03-01T01:30:00Z"),
        (6, "2026-03-01T03:10:00Z"),
    ] {
        session
            .run(&format!(
                "CREATE readings:{n} = {{ at: datetime '{at}', level: {n} }};"
            ))
            .unwrap();
    }
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Object(object) = value else {
        panic!("not an object: {value:?}");
    };
    object
        .get(name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

fn count_of(value: &Value) -> i64 {
    match field(value, "held") {
        Value::Number(Number::Integer(n)) => *n,
        other => panic!("not a count: {other:?}"),
    }
}

#[test]
fn a_window_is_a_group_key_like_any_other() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    let outcomes = session
        .run(
            "SELECT count(*) AS held, time::bucket(at, 1h) AS window FROM readings \
             GROUP BY time::bucket(at, 1h);",
        )
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 3, "{records:?}");

    // Groups come out in the value system's order, so the windows are in time
    // order without anybody asking for it.
    assert_eq!(count_of(&records[0].1), 3);
    assert_eq!(count_of(&records[1].1), 2);
    assert_eq!(count_of(&records[2].1), 1);
    assert_eq!(
        field(&records[0].1, "window"),
        &Value::Datetime(tessari_types::Datetime::parse_rfc3339("2026-03-01T00:00:00Z").unwrap())
    );
    assert_eq!(
        field(&records[2].1, "window"),
        &Value::Datetime(tessari_types::Datetime::parse_rfc3339("2026-03-01T03:00:00Z").unwrap())
    );
}

#[test]
fn a_window_has_no_row_for_an_hour_nothing_happened_in() {
    // Stated because it surprises people: grouping answers with the groups the
    // data has, and 02:00 has no reading. Filling the gap means knowing the
    // range the caller meant, which the statement does not say — so it would be
    // a guess, and a row nobody wrote is worse than a row nobody sees.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    let outcomes = session
        .run(
            "SELECT count(*) AS held, time::bucket(at, 1h) AS window FROM readings \
             GROUP BY time::bucket(at, 1h);",
        )
        .unwrap();
    assert_eq!(outcomes[0].records().unwrap().len(), 3);
}

#[test]
fn the_window_width_is_the_whole_of_what_changes_the_answer() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    for (width, groups) in [("1h", 3), ("2h", 2), ("1d", 1), ("30m", 5)] {
        let outcomes = session
            .run(&format!(
                "SELECT count(*) AS held, time::bucket(at, {width}) AS window FROM readings \
                 GROUP BY time::bucket(at, {width});"
            ))
            .unwrap();
        assert_eq!(
            outcomes[0].records().unwrap().len(),
            groups,
            "{width} gave the wrong number of windows"
        );
    }
}

#[test]
fn windows_are_anchored_at_the_epoch_and_not_at_the_data() {
    // The property that makes two queries agree. A window anchored at the first
    // record would put the same instant in different windows depending on what
    // else the table held, and neither caller would notice.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE readings:1 = { at: datetime '2026-03-01T00:40:00Z' };\n\
             CREATE readings:2 = { at: datetime '2026-03-01T01:10:00Z' };",
        )
        .unwrap();

    let outcomes = session
        .run("SELECT time::bucket(at, 1h) AS window FROM readings;")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    // 00:40 and 01:10 are forty minutes apart and in different windows, because
    // the boundary is the hour and not the first record.
    assert_ne!(
        field(&records[0].1, "window"),
        field(&records[1].1, "window")
    );
    assert_eq!(
        field(&records[0].1, "window"),
        &Value::Datetime(tessari_types::Datetime::parse_rfc3339("2026-03-01T00:00:00Z").unwrap())
    );
}

#[test]
fn an_instant_before_the_epoch_lands_in_the_window_that_contains_it() {
    // Truncation toward negative infinity rather than toward zero. Division
    // would put 1969-12-31T23:30 into the window starting at 1970-01-01T00:00 —
    // a window that begins after the instant it is meant to contain, and an
    // off-by-one nobody sees until they query a date before the epoch.
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE readings:1 = { at: datetime '1969-12-31T23:30:00Z' };")
        .unwrap();

    let outcomes = session
        .run("SELECT time::bucket(at, 1h) AS window FROM readings;")
        .unwrap();
    assert_eq!(
        field(&outcomes[0].records().unwrap()[0].1, "window"),
        &Value::Datetime(tessari_types::Datetime::parse_rfc3339("1969-12-31T23:00:00Z").unwrap())
    );
}

#[test]
fn a_group_by_over_a_path_still_means_what_it_always_did() {
    // The clause widened from a path to an expression, and a bare name still
    // reads as a route into the record — the same reading `WHERE` and `ORDER BY`
    // give it. Every statement written before this still says what it said.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "CREATE readings:1 = { city: 'london', level: 1 };\n\
             CREATE readings:2 = { city: 'york', level: 2 };\n\
             CREATE readings:3 = { city: 'london', level: 3 };",
        )
        .unwrap();

    let outcomes = session
        .run("SELECT count(*) AS held, city FROM readings GROUP BY city;")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(count_of(&records[0].1), 2);
    assert_eq!(field(&records[0].1, "city"), &Value::from("london"));
}

#[test]
fn a_projection_that_is_not_a_group_key_is_still_refused() {
    // The rule the widening had to keep: a projected value that is neither a
    // fold nor a key would answer with whichever record the group happened to
    // hold last.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    assert!(
        session
            .run(
                "SELECT count(*) AS held, level FROM readings \
                 GROUP BY time::bucket(at, 1h);"
            )
            .is_err()
    );
}

#[test]
fn a_window_of_nothing_is_refused_rather_than_dividing_by_it() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    for width in ["0s", "500ms"] {
        let refused = session
            .run(&format!(
                "SELECT time::bucket(at, {width}) AS window FROM readings;"
            ))
            .unwrap_err();
        assert!(
            matches!(refused, Error::CallFailed { .. }),
            "{width} gave {refused}"
        );
    }
}

#[test]
fn bucketing_something_that_is_not_an_instant_names_what_it_wanted() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    let refused = session
        .run("SELECT time::bucket(level, 1h) AS window FROM readings;")
        .unwrap_err();
    match refused {
        Error::WrongArgument { expected, .. } => assert_eq!(expected, "a datetime"),
        other => panic!("wrong refusal: {other}"),
    }
}

#[test]
fn a_window_composes_with_a_filter_and_an_ordering() {
    // The point of it being an expression rather than a clause: it is a value,
    // so everything that takes a value takes it.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    let outcomes = session
        .run(
            "SELECT count(*) AS held, time::bucket(at, 1h) AS window FROM readings \
             WHERE at >= datetime '2026-03-01T01:00:00Z' \
             GROUP BY time::bucket(at, 1h) ORDER BY held DESC;",
        )
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(count_of(&records[0].1), 2);
    assert_eq!(count_of(&records[1].1), 1);
}

#[test]
fn a_retention_statement_removes_a_range_and_says_how_much() {
    // How a retention policy is *said*. There is no declared TTL and no
    // background job: a policy is a statement an operator or a schedule runs,
    // which keeps the decision about when it runs where somebody can see it.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    let outcomes = session
        .run("DELETE FROM readings WHERE at < datetime '2026-03-01T01:00:00Z' LIMIT ALL;")
        .unwrap();
    match outcomes[0] {
        tessari_session::Outcome::Removed { count } => assert_eq!(count, 3),
        ref other => panic!("not a removal: {other:?}"),
    }

    let left = session.run("SELECT * FROM readings;").unwrap();
    assert_eq!(left[0].records().unwrap().len(), 3);
}

#[test]
fn a_retention_statement_is_served_by_an_index_like_any_other_read() {
    // It finds its records the same way a `SELECT … WHERE` does, so a policy
    // over an indexed timestamp is a bounded scan rather than a walk of the
    // table. Building a second way to find records would be a second place for
    // the answer to differ.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);
    session
        .run("DEFINE INDEX by_at ON readings FIELDS at;")
        .unwrap();

    let outcomes = session
        .run("DELETE FROM readings WHERE at < datetime '2026-03-01T01:00:00Z' LIMIT ALL;")
        .unwrap();
    match outcomes[0] {
        tessari_session::Outcome::Removed { count } => assert_eq!(count, 3),
        ref other => panic!("not a removal: {other:?}"),
    }
    // And the index no longer answers for what is gone.
    let left = session
        .run("SELECT * FROM readings WHERE at < datetime '2026-03-01T01:00:00Z';")
        .unwrap();
    assert!(left[0].records().unwrap().is_empty());
}

#[test]
fn a_condition_nothing_satisfies_removes_nothing_and_is_not_an_error() {
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    let outcomes = session
        .run("DELETE FROM readings WHERE at < datetime '2020-01-01T00:00:00Z' LIMIT ALL;")
        .unwrap();
    match outcomes[0] {
        tessari_session::Outcome::Removed { count } => assert_eq!(count, 0),
        ref other => panic!("not a removal: {other:?}"),
    }
    assert_eq!(
        session.run("SELECT * FROM readings;").unwrap()[0]
            .records()
            .unwrap()
            .len(),
        6
    );
}

#[test]
fn a_retention_run_is_one_commit() {
    // All of it or none. A statement that deleted in batches would leave a
    // window in which half a policy had been applied and nothing would say
    // which half — so this is inside a transaction that is then cancelled, and
    // the table is untouched.
    let store = store();
    let mut session = ready(&store);
    populate(&mut session);

    session
        .run(
            "BEGIN;\n\
             DELETE FROM readings WHERE at < datetime '2026-03-01T02:00:00Z' LIMIT ALL;\n\
             CANCEL;",
        )
        .unwrap();
    assert_eq!(
        session.run("SELECT * FROM readings;").unwrap()[0]
            .records()
            .unwrap()
            .len(),
        6,
        "a cancelled retention run removed something"
    );
}

#[test]
fn what_retention_removes_is_reclaimed_rather_than_left_behind() {
    // The other half of the node's name. A delete writes a tombstone, and the
    // space comes back when reclamation passes it — which it can only do once no
    // reader still needs the version. Asserted end to end rather than assumed:
    // the versions are counted on disk before and after.
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    {
        let mut session = ready(&store);
        populate(&mut session);
        session
            .run("DELETE FROM readings WHERE at < datetime '2026-03-01T02:00:00Z' LIMIT ALL;")
            .unwrap();
    }

    let (namespace, database, table) = tenancy(&store);
    let before = versions(&backend, namespace, database, table);
    let reclaimed = store.reclaim_table(namespace, database, table).unwrap();
    let after = versions(&backend, namespace, database, table);

    assert!(
        reclaimed.versions > 0,
        "reclamation removed nothing after a retention run"
    );
    assert!(after < before, "{before} versions before, {after} after");
}

/// The tenancy the fixture built, and its table.
fn tenancy(
    store: &Store,
) -> (
    tessari_types::NamespaceId,
    tessari_types::DatabaseId,
    tessari_types::TableId,
) {
    let mut transaction = store.begin().unwrap();
    let catalog = tessari_storage::Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "orders").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, "readings")
        .unwrap()
        .unwrap();
    (namespace, database, table)
}

/// How many record versions the table holds on disk, live and tombstoned alike.
///
/// Read from the backend the test opened the store over, because a version is a
/// key and only the key layer can count them — a `SELECT` sees the live ones,
/// which is exactly what this has to look past.
fn versions(
    backend: &Arc<dyn KvBackend>,
    namespace: tessari_types::NamespaceId,
    database: tessari_types::DatabaseId,
    table: tessari_types::TableId,
) -> usize {
    let prefix = tessari_encoding::RecordKey::table_prefix(namespace, database, table);
    let request = tessari_kv::ScanRequest {
        keyspace: tessari_kv::Keyspace::DATA,
        range: tessari_kv::KeyRange::prefix(&prefix),
        direction: tessari_kv::ScanDirection::Forward,
        limit: None,
    };
    backend.scan(&request).unwrap().len()
}
