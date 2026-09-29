//! `increase`, `rate` and `delta` over samples ordered by their instant
//! (ADR-0088 §5, G044 C6). The oracle is written by hand: five samples forty
//! seconds apart end to end, one of them after a counter reset, written in an
//! order that is not their time order.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

/// `(seconds after 10:00, value)`, in the order they are written.
const SAMPLES: [(u32, &str); 5] = [(30, "8"), (0, "10"), (40, "20"), (20, "3"), (10, "15")];

fn answered(numbers: &str) -> Value {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION hits;",
        )
        .unwrap();
    for (second, value) in SAMPLES {
        session
            .run(&format!(
                "CREATE hits = {{ n: {value}{numbers}, at: datetime '2026-09-29T10:00:{second:02}Z' }};"
            ))
            .unwrap();
    }
    let Outcome::Records { records, .. } = session
        .run(
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
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION hits; CREATE hits = { n: 1, at: datetime '2026-09-29T10:00:00Z' };",
        )
        .unwrap();
    let Outcome::Records { records, .. } = session
        .run("SELECT increase(n, at) AS up FROM hits;")
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("a read answers with records");
    };
    assert_eq!(field(&records[0].1, "up"), Value::None);
    let missing = session
        .run("SELECT increase(n) AS up FROM hits;")
        .unwrap_err()
        .to_string();
    assert!(
        missing.contains("the instant each value was observed at"),
        "{missing}"
    );
    let text = session
        .run("SELECT rate(n, 'x') AS up FROM hits;")
        .unwrap_err()
        .to_string();
    assert!(text.contains("not a datetime"), "{text}");
}
