//! Per-column table statistics: `ANALYZE TABLE` summarises declared fields,
//! `INFO FOR TABLE` surfaces what is held, and the counts are exact on
//! engineered skews (G083 C1).
//!
//! A statistic answers a cost and never an answer, so these tests assert the
//! counts against brute-force expectations computed from the same data, and
//! never ask the statistic to decide a read.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
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

/// One schemafull table with three declared fields and `count` rows of
/// engineered shape: uniform, wide, zipf-skewed, with nulls and absences.
fn ready(store: &Store, count: i64) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE people SCHEMAFULL;\n\
             DEFINE FIELD band ON people TYPE any;\n\
             DEFINE FIELD rare ON people TYPE any;\n\
             DEFINE FIELD city ON people TYPE any;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 0..count {
        let band = if n % 17 == 0 {
            "null".to_owned()
        } else {
            format!("{}", n % 10)
        };
        let mut body = format!("{{ band: {band}, rare: {} ", n % 500);
        if n % 13 != 0 {
            let city = match n % 100 {
                r if r < 50 => "a",
                r if r < 80 => "b",
                _ => "c",
            };
            body.push_str(&format!(", city: '{city}'"));
        }
        body.push_str(" }");
        script.push_str(&format!("UPSERT people:{n} = {body};\n"));
    }
    session.run_now(&script).unwrap();
    session
}

fn analyzed(session: &mut Session<'_>) -> Vec<BTreeMap<String, Value>> {
    let outcomes = session.run_now("ANALYZE TABLE people;").unwrap();
    let Some(Outcome::Value(Value::Array(rows))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    rows.iter()
        .map(|row| match row {
            Value::Object(map) => map.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn count(row: &BTreeMap<String, Value>, key: &str) -> i64 {
    let Some(Value::Number(Number::Integer(held))) = row.get(key) else {
        panic!("{key} missing from {row:?}");
    };
    *held
}

#[test]
fn analyze_reports_one_row_per_declared_field_with_exact_counts() {
    let store = store();
    let mut session = ready(&store, 1_000);
    let rows = analyzed(&mut session);
    let by_field: BTreeMap<String, BTreeMap<String, Value>> = rows
        .iter()
        .filter(|row| row.contains_key("field"))
        .map(|row| {
            let Value::String(name) = &row["field"] else {
                panic!("{row:?}");
            };
            (name.clone(), row.clone())
        })
        .collect();
    assert_eq!(
        by_field.len(),
        3,
        "one row per declared field: {by_field:?}"
    );

    // band: null every 17th record; a value otherwise.
    let band = &by_field["band"];
    let band_nulls = (0..1_000)
        .filter(|n| n % 17 == 0)
        .count()
        .try_into()
        .unwrap_or(i64::MAX);
    assert_eq!(count(band, "nulls"), band_nulls);
    assert_eq!(count(band, "entries"), 1_000 - band_nulls);
    assert_eq!(count(band, "distinct"), 10);

    // rare: a value on every record (band's nulls still hold a rare value).
    let rare = &by_field["rare"];
    assert_eq!(count(rare, "nulls"), 0);
    assert_eq!(count(rare, "entries"), 1_000);
    assert_eq!(count(rare, "distinct"), 500);

    // city: absent every 13th record; three values with a skew.
    let city = &by_field["city"];
    let city_absent = (0..1_000)
        .filter(|n| n % 13 == 0)
        .count()
        .try_into()
        .unwrap_or(i64::MAX);
    assert_eq!(count(city, "entries"), 1_000 - city_absent);
    assert_eq!(count(city, "nulls"), 0);
    assert_eq!(count(city, "distinct"), 3);
    // The skew is engineered: 'a' outnumbers 'c' by at least half again.
    let a = (0..1_000)
        .filter(|n| n % 13 != 0 && n % 100 < 50)
        .count()
        .try_into()
        .unwrap_or(i64::MAX);
    let c = (0..1_000)
        .filter(|n| n % 13 != 0 && n % 100 >= 80)
        .count()
        .try_into()
        .unwrap_or(i64::MAX);
    assert!(a >= c + c / 2, "the engineered skew: a={a} c={c}");
    assert_eq!(count(city, "common"), 3, "three values, all kept");
}

#[test]
fn absent_and_null_are_counted_apart() {
    let store = store();
    let mut session = ready(&store, 100);
    let rows = analyzed(&mut session);
    let band = rows
        .iter()
        .find(|row| row.get("field") == Some(&Value::from("band")))
        .unwrap();
    // Nulls and entries sum to every record: absence is neither.
    assert_eq!(
        count(band, "nulls") + count(band, "entries"),
        100,
        "absence is the remainder, not a third kind"
    );
}

#[test]
fn the_change_counter_moves_with_the_writes_so_staleness_is_measured() {
    let store = store();
    let mut session = ready(&store, 100);
    session.run_now("ANALYZE TABLE people;").unwrap();
    fn age(session: &mut Session<'_>) -> i64 {
        let outcomes = session.run_now("INFO FOR TABLE people;").unwrap();
        let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
            panic!("{outcomes:?}");
        };
        let Some(Value::Array(rows)) = report.get("statistics") else {
            panic!("no statistics section: {report:?}");
        };
        let Some(Value::Object(row)) = rows.iter().find(|row| {
            matches!(row, Value::Object(map) if map.get("field") == Some(&Value::from("rare")))
        }) else {
            panic!("{rows:?}");
        };
        count(row, "age")
    }
    assert_eq!(age(&mut session), 0, "freshly analysed");
    // One commit carrying more changes than the staleness floor: the counter
    // moves in the commit batch, which is what staleness is measured against.
    let mut rewrite = String::new();
    for n in 0..1_100 {
        rewrite.push_str(&format!("UPSERT people:{n} = {{ band: 1, rare: {n} }};\n"));
    }
    session.run_now(&rewrite).unwrap();
    assert!(
        age(&mut session) >= 1_100,
        "the counter counts every write, in the one batch"
    );
}

#[test]
fn info_surfaces_counts_and_never_a_counted_value() {
    let store = store();
    let mut session = ready(&store, 200);
    session.run_now("ANALYZE TABLE people;").unwrap();
    let outcomes = session.run_now("INFO FOR TABLE people;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    let Some(Value::Array(rows)) = report.get("statistics") else {
        panic!("no statistics section: {report:?}");
    };
    assert_eq!(rows.len(), 3);
    for row in rows {
        let Value::Object(map) = row else {
            panic!("{row:?}");
        };
        for key in map.keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "field" | "entries" | "nulls" | "distinct" | "common" | "buckets" | "age"
                ),
                "a statistic answers counts, never values: {key}"
            );
        }
    }
}

#[test]
fn a_table_without_declared_fields_has_no_statistics_rows() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE loose SCHEMALESS;\n\
             UPSERT loose:1 = { whatever: 1 };",
        )
        .unwrap();
    session.run_now("ANALYZE TABLE loose;").unwrap();
    let outcomes = session.run_now("INFO FOR TABLE loose;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    assert!(
        !report.contains_key("statistics"),
        "schemaless fields are unbounded; nothing is summarised: {report:?}"
    );
}

#[test]
fn analyze_cost_is_measured_and_printed() {
    // The C8-honesty pattern: the measurement is the evidence, not a gate.
    // Debug build, in-memory backend, 100k rows — the same shape the write
    // path's cost was published at.
    let store = store();
    let mut session = ready(&store, 100_000);
    let start = std::time::Instant::now();
    session.run_now("ANALYZE TABLE people;").unwrap();
    let took = start.elapsed();
    let per = took / 100_000 / 3;
    println!("field-statistics analyze: {took:?} over 100k rows, 3 fields ({per:?}/row/field)");
}
