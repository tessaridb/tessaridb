//! `FILL <mode> FROM <start> TO <end>` — one row per window of a stated range
//! (ADR-0088 §2, G044 C3). The oracle is written by hand: three readings in two
//! of five hourly windows.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Note, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

const READINGS: &str = "CREATE readings:1 = { sensor: 'a', at: datetime '2026-09-29T10:05:00Z', v: 10 };\
     CREATE readings:2 = { sensor: 'a', at: datetime '2026-09-29T10:40:00Z', v: 10 };\
     CREATE readings:3 = { sensor: 'a', at: datetime '2026-09-29T12:10:00Z', v: 30 };\
     CREATE readings:4 = { sensor: 'b', at: datetime '2026-09-29T15:00:00Z', v: 99 };";

const RANGE: &str = "FROM datetime '2026-09-29T09:30:00Z' TO datetime '2026-09-29T14:00:00Z'";

fn session(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION readings;",
        )
        .unwrap();
    session.run(READINGS).unwrap();
    session
}

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// The answer's rows as `(window, n, v)` and its notes.
fn windows(session: &mut Session<'_>, mode: &str) -> (Vec<(String, Value, Value)>, Vec<Note>) {
    let read = format!(
        "SELECT time::bucket(at, 1h) AS w, count(*) AS n, mean(v) AS v FROM readings \
         WHERE sensor = 'a' GROUP BY time::bucket(at, 1h) FILL {mode} {RANGE};"
    );
    let Outcome::Records { records, notes, .. } = session.run(&read).unwrap().pop().unwrap() else {
        panic!("a read answers with records");
    };
    let rows = records
        .into_iter()
        .map(|(_, value)| {
            let Value::Object(fields) = value else {
                panic!("a row is an object");
            };
            (
                fields["w"].to_string(),
                fields["n"].clone(),
                fields.get("v").cloned().unwrap_or(Value::None),
            )
        })
        .collect();
    (rows, notes)
}

fn int(n: i64) -> Value {
    Value::Number(Number::Integer(n))
}

fn float(n: f64) -> Value {
    Value::Number(Number::Float(n))
}

#[test]
fn every_window_of_the_range_answers_once_and_a_filled_count_is_zero() {
    let store = store();
    let mut session = session(&store);
    let (rows, notes) = windows(&mut session, "NULL");
    // 09:00 (the range starts inside it), 10, 11, 12, 13 — and not 14:00.
    assert_eq!(rows.len(), 5, "{rows:?}");
    let counts: Vec<Value> = rows.iter().map(|row| row.1.clone()).collect();
    assert_eq!(counts, vec![int(0), int(2), int(0), int(1), int(0)]);
    let values: Vec<Value> = rows.iter().map(|row| row.2.clone()).collect();
    assert_eq!(
        values,
        vec![Value::Null, int(10), Value::Null, int(30), Value::Null]
    );
    assert!(
        notes
            .iter()
            .any(|note| matches!(note, Note::Filled { windows: 3 })),
        "{notes:?}"
    );
}

#[test]
fn previous_linear_and_a_value_fill_what_their_names_say() {
    let store = store();
    let mut session = session(&store);
    let values = |rows: Vec<(String, Value, Value)>| -> Vec<Value> {
        rows.into_iter().map(|row| row.2).collect()
    };
    assert_eq!(
        values(windows(&mut session, "PREVIOUS").0),
        vec![Value::Null, int(10), int(10), int(30), int(30)]
    );
    assert_eq!(
        values(windows(&mut session, "LINEAR").0),
        vec![Value::Null, int(10), float(20.0), int(30), Value::Null]
    );
    assert_eq!(
        values(windows(&mut session, "0").0),
        vec![int(0), int(10), int(0), int(30), int(0)]
    );
}

#[test]
fn each_group_is_filled_over_the_whole_range() {
    let store = store();
    let mut session = session(&store);
    let read = format!(
        "SELECT sensor, time::bucket(at, 1h) AS w, count(*) AS n FROM readings \
         GROUP BY sensor, time::bucket(at, 1h) FILL NULL {RANGE};"
    );
    let Outcome::Records { records, .. } = session.run(&read).unwrap().pop().unwrap() else {
        panic!("a read answers with records");
    };
    // Two sensors × five windows; sensor b's 15:00 reading is outside the range.
    assert_eq!(records.len(), 10);
    let b: Vec<&Value> = records
        .iter()
        .filter_map(|(_, row)| match row {
            Value::Object(fields) if fields["sensor"] == Value::from("b") => Some(&fields["n"]),
            _ => None,
        })
        .collect();
    assert_eq!(b, vec![&int(0); 5]);
}

#[test]
fn a_fill_that_cannot_say_its_windows_is_refused() {
    let store = store();
    let mut session = session(&store);
    let refusal =
        |session: &mut Session<'_>, read: &str| session.run(read).unwrap_err().to_string();

    let no_window = refusal(
        &mut session,
        &format!("SELECT sensor, count(*) AS n FROM readings GROUP BY sensor FILL NULL {RANGE};"),
    );
    assert!(no_window.contains("one window key"), "{no_window}");
    let no_range = refusal(
        &mut session,
        "SELECT time::bucket(at, 1h) AS w, count(*) AS n FROM readings \
         GROUP BY time::bucket(at, 1h) FILL NULL FROM 1 TO 2;",
    );
    assert!(no_range.contains("both ends to be datetimes"), "{no_range}");
    let too_wide = refusal(
        &mut session,
        "SELECT time::bucket(at, 1s) AS w, count(*) AS n FROM readings \
         GROUP BY time::bucket(at, 1s) FILL NULL \
         FROM datetime '2026-01-01T00:00:00Z' TO datetime '2026-02-01T00:00:00Z';",
    );
    assert!(too_wide.contains("fills at most"), "{too_wide}");
}
