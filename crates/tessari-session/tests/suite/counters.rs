//! `increase`, `rate` and `delta` over samples ordered by their instant
//! (ADR-0088 §5, G044 C6). The oracle is written by hand: five samples forty
//! seconds apart end to end, one of them after a counter reset, written in an
//! order that is not their time order.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Datetime, Number, Value};

/// `(seconds after 10:00, value)`, in the order they are written.
const SAMPLES: [(u32, &str); 5] = [(30, "8"), (0, "10"), (40, "20"), (20, "3"), (10, "15")];

fn answered(numbers: &str) -> Value {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION hits;",
        )
        .unwrap();
    for (second, value) in SAMPLES {
        session
            .run_now(&format!(
                "CREATE hits = {{ n: {value}{numbers}, at: datetime '2026-09-29T10:00:{second:02}Z' }};"
            ))
            .unwrap();
    }
    let Outcome::Records { records, .. } = session
        .run_now(
            "SELECT increase(n, at) AS up, delta(n, at) AS change, rate(n, at) AS per \
             FROM hits;",
        )
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("a read answers with records");
    };
    records.into_iter().next().unwrap().1
}

fn field(row: &Value, name: &str) -> Value {
    let Value::Object(fields) = row else {
        panic!("a row is an object");
    };
    fields.get(name).cloned().unwrap_or(Value::None)
}

#[test]
fn a_fall_is_a_reset_and_the_answer_is_what_the_samples_say() {
    let row = answered("");
    // 10 → 15 (+5) → 3 (reset: +3) → 8 (+5) → 20 (+12).
    assert_eq!(field(&row, "up"), Value::Number(Number::Integer(25)));
    // Last minus first, no reset handling: 20 - 10.
    assert_eq!(field(&row, "change"), Value::Number(Number::Integer(10)));
    // 25 over the 40 seconds between the first and last sample, exactly.
    assert_eq!(field(&row, "per").to_string(), "0.625");
}

#[test]
fn a_float_sample_turns_the_fold_to_floats() {
    let row = answered(".0");
    assert_eq!(field(&row, "up"), Value::Number(Number::Float(25.0)));
    assert_eq!(field(&row, "change"), Value::Number(Number::Float(10.0)));
    assert_eq!(field(&row, "per"), Value::Number(Number::Float(0.625)));
}

#[test]
fn one_sample_is_not_a_change_and_the_instant_is_required() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION hits; CREATE hits = { n: 1, at: datetime '2026-09-29T10:00:00Z' };",
        )
        .unwrap();
    let Outcome::Records { records, .. } = session
        .run_now("SELECT increase(n, at) AS up FROM hits;")
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("a read answers with records");
    };
    assert_eq!(field(&records[0].1, "up"), Value::None);
    let missing = session
        .run_now("SELECT increase(n) AS up FROM hits;")
        .unwrap_err()
        .to_string();
    assert!(
        missing.contains("the instant each value was observed at"),
        "{missing}"
    );
    let text = session
        .run_now("SELECT rate(n, 'x') AS up FROM hits;")
        .unwrap_err()
        .to_string();
    assert!(text.contains("not a datetime"), "{text}");
}

#[test]
fn the_edge_and_instant_folds_answer_the_same_series_by_hand() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION hits;",
        )
        .unwrap();
    for (second, value) in SAMPLES {
        session
            .run_now(&format!(
                "CREATE hits = {{ n: {value}, at: datetime '2026-09-29T10:00:{second:02}Z' }};"
            ))
            .unwrap();
    }
    let Outcome::Records { records, .. } = session
        .run_now(
            "SELECT changes(n, at) AS changed, irate(n, at) AS pace, idelta(n, at) AS step, \
             ts_of_first(n, at) AS first, ts_of_last(n, at) AS last, ts_of_max(n, at) AS top, \
             ts_of_min(n, at) AS bottom, mad(n) AS spread FROM hits;",
        )
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("a read answers with records");
    };
    let row = records.into_iter().next().unwrap().1;
    // Time order 10, 15, 3, 8, 20: every neighbour differs.
    assert_eq!(field(&row, "changed"), Value::Number(Number::Integer(4)));
    // The edge: 8 at :30 to 20 at :40, twelve over ten seconds.
    assert_eq!(field(&row, "step"), Value::Number(Number::Integer(12)));
    assert_eq!(field(&row, "pace").to_string(), "1.2");
    // 2026-09-29T10:00:00Z is 1 790 676 000 seconds.
    let at = |plus: i64| Value::Datetime(Datetime::from_seconds(1_790_676_000 + plus));
    assert_eq!(field(&row, "first"), at(0));
    assert_eq!(field(&row, "last"), at(40));
    // Largest 20 at :40; smallest 3 at :20.
    assert_eq!(field(&row, "top"), at(40));
    assert_eq!(field(&row, "bottom"), at(20));
    // Sorted 3, 8, 10, 15, 20: median 10, distances 0, 2, 5, 7, 10, median 5.
    assert_eq!(field(&row, "spread").to_string(), "5");
}

#[test]
fn the_moving_folds_and_the_string_join_answer_at_statement_level() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION hits;",
        )
        .unwrap();
    for (second, value) in SAMPLES {
        session
            .run_now(&format!(
                "CREATE hits = {{ n: {value}, tag: 's{value}', \
                 at: datetime '2026-09-29T10:00:{second:02}Z' }};"
            ))
            .unwrap();
    }
    let Outcome::Records { records, .. } = session
        .run_now(
            "SELECT moving_sum(n, at, 2) AS pair, moving_mean(n, at, 2) AS pace, \
             string_agg(tag, '+') AS joined FROM hits;",
        )
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("a read answers with records");
    };
    let row = records.into_iter().next().unwrap().1;
    // Time order 10, 15, 3, 8, 20: the last two are 8 and 20.
    assert_eq!(field(&row, "pair"), Value::Number(Number::Integer(28)));
    assert_eq!(field(&row, "pace").to_string(), "14");
    // Joined in the order the records were written, not their time order.
    assert_eq!(
        field(&row, "joined"),
        Value::String("s8+s10+s20+s3+s15".to_owned())
    );
}
