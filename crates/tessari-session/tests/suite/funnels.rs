//! `window_funnel`, `sequence_match`, `sequence_count` and `retention` at
//! statement level — the conditions are ordinary booleans over the record,
//! and every expected number is worked out from the fixture by hand.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

/// Two visitors' event streams. `ada` viewed at :00, added at :30, paid at
/// :90; `bo` viewed at :00 and paid at :2000 with no add between.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION events;\n\
             CREATE events = { who: 'ada', kind: 'view', at: datetime '2026-11-01T10:00:00Z' };\n\
             CREATE events = { who: 'ada', kind: 'add',  at: datetime '2026-11-01T10:00:30Z' };\n\
             CREATE events = { who: 'ada', kind: 'pay',  at: datetime '2026-11-01T10:01:30Z' };\n\
             CREATE events = { who: 'bo',  kind: 'view', at: datetime '2026-11-01T10:00:00Z' };\n\
             CREATE events = { who: 'bo',  kind: 'pay',  at: datetime '2026-11-01T10:33:20Z' };",
        )
        .unwrap();
    session
}

/// One field of each group row, keyed by the group.
fn by_group(session: &mut Session<'_>, statement: &str, field: &str) -> Vec<(String, Value)> {
    let Outcome::Records { records, .. } = session.run_now(statement).unwrap().pop().unwrap()
    else {
        panic!("a read answers with records");
    };
    let mut answered: Vec<(String, Value)> = records
        .into_iter()
        .map(|(_, row)| {
            let Value::Object(fields) = row else {
                panic!("a row is an object");
            };
            let who = match fields.get("who") {
                Some(Value::String(who)) => who.clone(),
                other => panic!("a group key, got {other:?}"),
            };
            let held = fields.get(field).cloned().unwrap_or(Value::None);
            (who, held)
        })
        .collect();
    answered.sort();
    answered
}

#[test]
fn the_funnel_reads_the_longest_prefix_inside_the_window() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = ready(&store);
    let answered = by_group(
        &mut session,
        "SELECT who, window_funnel(at, 300, [kind = 'view', kind = 'add', kind = 'pay']) AS f \
         FROM events GROUP BY who;",
        "f",
    );
    // ada: view, add, pay inside 90 seconds — the whole funnel. bo: view,
    // then pay 2000 seconds later with no add — the prefix stops at 1.
    assert_eq!(
        answered,
        [
            ("ada".to_owned(), Value::Number(Number::Integer(3))),
            ("bo".to_owned(), Value::Number(Number::Integer(1))),
        ]
    );
}

#[test]
fn the_sequence_folds_read_the_pattern_with_its_bounds() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = ready(&store);
    let answered = by_group(
        &mut session,
        "SELECT who, sequence_match(at, '(1)(<600)(3)', [kind = 'view', kind = 'add', kind = 'pay']) AS m \
         FROM events GROUP BY who;",
        "m",
    );
    // ada: pay 90 seconds after the view, inside the bound. bo: 2000
    // seconds, outside it.
    assert_eq!(
        answered,
        [
            ("ada".to_owned(), Value::Number(Number::Integer(1))),
            ("bo".to_owned(), Value::Number(Number::Integer(0))),
        ]
    );
    let answered = by_group(
        &mut session,
        "SELECT who, sequence_count(at, '(1)(2)', [kind = 'view', kind = 'add']) AS c \
         FROM events GROUP BY who;",
        "c",
    );
    // ada holds one view→add chain; bo holds none.
    assert_eq!(
        answered,
        [
            ("ada".to_owned(), Value::Number(Number::Integer(1))),
            ("bo".to_owned(), Value::Number(Number::Integer(0))),
        ]
    );
}

#[test]
fn retention_anchors_every_later_condition_on_the_first() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = ready(&store);
    let answered = by_group(
        &mut session,
        "SELECT who, retention([kind = 'view', kind = 'add', kind = 'pay']) AS r \
         FROM events GROUP BY who;",
        "r",
    );
    // Both anchored on a view; ada added and paid, bo only paid.
    let ones = |met: &[i64]| {
        Value::Array(
            met.iter()
                .map(|held| Value::Number(Number::Integer(*held)))
                .collect(),
        )
    };
    assert_eq!(
        answered,
        [
            ("ada".to_owned(), ones(&[1, 1, 1])),
            ("bo".to_owned(), ones(&[1, 0, 1])),
        ]
    );
}

#[test]
fn what_the_folds_cannot_mean_is_refused_naming_the_fold() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    let mut session = ready(&store);
    for (statement, names) in [
        // A pattern naming a condition the call does not write.
        (
            "SELECT sequence_match(at, '(1)(2)(4)', [kind = 'view', kind = 'pay']) AS m FROM events;",
            "sequence_match",
        ),
        // A pattern that is no pattern at all.
        (
            "SELECT sequence_match(at, 'once', [kind = 'view']) AS m FROM events;",
            "sequence_match",
        ),
        // A window of nothing.
        (
            "SELECT window_funnel(at, 0, [kind = 'view']) AS f FROM events;",
            "window_funnel",
        ),
        // A condition that is a number, not a boolean.
        ("SELECT retention([1]) AS r FROM events;", "retention"),
    ] {
        let refused = session.run_now(statement);
        let Err(error) = refused else {
            panic!("{statement} ran: {refused:?}");
        };
        assert!(
            format!("{error}").contains(names),
            "{statement} refused without naming the fold: {error}"
        );
    }
}
