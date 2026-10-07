//! G069 C4 — a rollup keeps `approx_distinct` and `approx_quantile` (ADR-0122
//! C5): the row answers the estimate, and folding the column merges the
//! sketches kept beside the rows, so the rollup answers what the raw series
//! answers — bit for bit, because both merges are order-independent.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::sync::Arc;

use tessari_encoding::{FormatVersion, FormatVersionKey, StoreKey, StoreValue};
use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
use tessari_session::{Error, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

const DEFINE: &str = "DEFINE ROLLUP hourly FROM visits WINDOW 1h BY page COMPUTE count(*) AS n, \
                      approx_distinct(visitor) AS users, approx_quantile(ms, 0.99) AS p99 \
                      RETAIN 36500d;";

/// `visits` over two hours and two pages: in hour 10, page `a` sees 1 100
/// distinct visitors (each twice), past a sketch's exact small set, and page
/// `b` 200; in hour 11, page `a` sees 300, of whom 200 were there in hour 10.
/// `ms` runs from 1 to 2 700.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE SERIES visits RETAIN 36500d TIME at;",
        )
        .unwrap();
    session.run(DEFINE).unwrap();
    let mut script = String::from("BEGIN;\n");
    let mut ms = 0;
    let mut visit = |script: &mut String, page: &str, user: i64, minute: i64| {
        ms += 1;
        script.push_str(&format!(
            "CREATE visits = {{ page: '{page}', visitor: 'u{user}', ms: {ms}, \
             at: datetime '2026-10-07T{:02}:{:02}:00Z' }};\n",
            10 + minute / 60,
            minute % 60,
        ));
    };
    for user in 0..1_100 {
        visit(&mut script, "a", user, user % 60);
        visit(&mut script, "a", user, (user + 7) % 60);
    }
    for user in 0..200 {
        visit(&mut script, "b", user, user % 60);
    }
    for user in 900..1_200 {
        visit(&mut script, "a", user, 60 + user % 60);
    }
    script.push_str("COMMIT;");
    session.run(&script).unwrap();
    session
}

fn records(session: &mut Session<'_>, read: &str) -> Vec<(RecordId, Value)> {
    let outcomes = session.run(read).unwrap();
    outcomes.last().unwrap().records().unwrap().to_vec()
}

/// The answer's records with their identities taken off, which a rollup's and
/// a series' grouped answers do not share.
fn rows(session: &mut Session<'_>, read: &str) -> Vec<Value> {
    records(session, read)
        .into_iter()
        .map(|(_, row)| row)
        .collect()
}

#[test]
fn a_rollup_row_answers_each_sketch_with_its_estimate() {
    let store = store();
    let mut session = ready(&store);
    let kept = rows(
        &mut session,
        "SELECT page, users, p99 FROM hourly WHERE window = datetime '2026-10-07T10:00:00Z' \
         AND page = 'a';",
    );
    let raw = rows(
        &mut session,
        "SELECT page, approx_distinct(visitor) AS users, approx_quantile(ms, 0.99) AS p99 \
         FROM visits WHERE at >= datetime '2026-10-07T10:00:00Z' \
         AND at < datetime '2026-10-07T11:00:00Z' AND page = 'a' GROUP BY page;",
    );
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept, raw);
    let Value::Object(fields) = &kept[0] else {
        panic!("not a row: {kept:?}");
    };
    // 1 100 distinct visitors: an estimate within 2.5 %, read as a number.
    let Some(Value::Number(Number::Integer(users))) = fields.get("users") else {
        panic!("not an estimate: {fields:?}");
    };
    assert!((users - 1_100).abs() <= 27, "{users}");
    assert!(matches!(
        fields.get("p99"),
        Some(Value::Number(Number::Float(_)))
    ));
}

#[test]
fn folding_a_sketch_column_merges_the_rows_and_answers_what_the_series_answers() {
    let store = store();
    let mut session = ready(&store);
    let merged = "SELECT page, approx_distinct(users) AS u, approx_quantile(p99, 0.5) AS m, \
                  sum(n) AS c FROM hourly GROUP BY page;";
    let raw = "SELECT page, approx_distinct(visitor) AS u, approx_quantile(ms, 0.5) AS m, \
               count(*) AS c FROM visits GROUP BY page;";
    let expected = rows(&mut session, raw);
    assert_eq!(expected.len(), 2, "{expected:?}");
    assert_eq!(rows(&mut session, merged), expected);
    // Page `a` saw 1 200 distinct visitors across both hours, not 1 100 + 300.
    let Value::Object(fields) = &expected[0] else {
        panic!("not a row: {expected:?}");
    };
    let Some(Value::Number(Number::Integer(users))) = fields.get("u") else {
        panic!("not an estimate: {fields:?}");
    };
    assert!((users - 1_200).abs() <= 30, "{users}");
    // The merged answer carries the note the raw one does.
    let outcomes = session.run(merged).unwrap();
    assert!(
        outcomes
            .last()
            .unwrap()
            .notes()
            .iter()
            .any(|note| note.kind() == "estimated")
    );
}

#[test]
fn a_deleted_or_moved_reading_recomputes_its_rows_sketches() {
    let store = store();
    let mut session = ready(&store);
    // Every visit on page `a` in hour 10 of a visitor sorting below `u2` goes,
    // and one visitor's readings move to page `b`.
    session
        .run(
            "DELETE FROM visits WHERE page = 'a' AND at < datetime '2026-10-07T11:00:00Z' \
             AND visitor < 'u2' LIMIT ALL;",
        )
        .unwrap();
    session
        .run(
            "DELETE FROM visits WHERE page = 'a' AND visitor = 'u1600' LIMIT ALL;\n\
             CREATE visits = { page: 'b', visitor: 'u1600', ms: 9999, \
             at: datetime '2026-10-07T11:40:00Z' };",
        )
        .unwrap();
    let merged = "SELECT page, approx_distinct(users) AS u, approx_quantile(p99, 0.25) AS m \
                  FROM hourly GROUP BY page;";
    let raw = "SELECT page, approx_distinct(visitor) AS u, approx_quantile(ms, 0.25) AS m \
               FROM visits GROUP BY page;";
    assert_eq!(rows(&mut session, merged), rows(&mut session, raw));
}

#[test]
fn a_rollup_keeps_a_sketch_only_as_declared() {
    let store = store();
    let mut session = ready(&store);
    for compute in [
        "approx_quantile(ms) AS q",
        "approx_quantile(ms, 1.5) AS q",
        "approx_distinct(visitor, 0.5) AS q",
        "approx_distinct(*) AS q",
        "count(ms, 0.5) AS q",
    ] {
        let script =
            format!("DEFINE ROLLUP other FROM visits WINDOW 1h COMPUTE {compute} RETAIN 1d;");
        match session.run(&script) {
            Err(Error::RollupFold { .. }) => {}
            // A missing rank is the parser's to refuse: there is nothing to read.
            Err(Error::Script(_)) if compute == "approx_quantile(ms) AS q" => {
                panic!("{script}: the rank is the session's to require")
            }
            other => panic!("{script}: {other:?}"),
        }
    }
}

#[test]
fn a_store_holding_an_older_format_refuses_a_sketch_rollup_until_finalized() {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    drop(Store::open(Arc::clone(&backend)).unwrap());
    let older = FormatVersion::SKETCH_ROLLUP.get().checked_sub(1).unwrap();
    backend
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(older).encode(),
        ))
        .unwrap();
    let store = Store::open(Arc::clone(&backend)).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE SERIES visits RETAIN 36500d TIME at;\n\
             DEFINE ROLLUP counted FROM visits WINDOW 1h COMPUTE count(*) AS n RETAIN 1d;",
        )
        .unwrap();
    assert!(matches!(
        session.run(DEFINE),
        Err(Error::FormatNotFinalized { .. })
    ));
    session.run("ALTER STORE FINALIZE FORMAT;").unwrap();
    session.run(DEFINE).unwrap();
}

/// A reader who may not see a sketch column merges nothing from beside its
/// rows: the state is reached only through the column, so redaction holds.
#[test]
fn a_sketch_column_the_reader_may_not_see_merges_nothing() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut owner = Session::new(&store);
    owner.sign_in("root", "correct horse battery").unwrap();
    owner
        .run(
            "DEFINE USER narrow ON NAMESPACE prod AUTHORITIES read \
             PASSWORD 'correct horse battery';\n\
             USE NAMESPACE prod; USE DATABASE shop; GRANT read ON hourly FIELDS page, n TO narrow;",
        )
        .unwrap();
    let mut narrow = Session::new(&store);
    narrow.sign_in("narrow", "correct horse battery").unwrap();
    narrow
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let answered = rows(
        &mut narrow,
        "SELECT page, approx_distinct(users) AS u, approx_quantile(p99, 0.5) AS m, sum(n) AS c \
         FROM hourly GROUP BY page;",
    );
    assert_eq!(answered.len(), 2, "{answered:?}");
    for row in answered {
        let Value::Object(fields) = row else {
            panic!("not a row");
        };
        // Nothing merged: a count of zero, no quantile; the visible sum still answers.
        assert_eq!(
            fields.get("u"),
            Some(&Value::Number(Number::Integer(0))),
            "{fields:?}"
        );
        assert_eq!(fields.get("m"), None, "{fields:?}");
        assert!(fields.contains_key("c"), "{fields:?}");
    }
}
