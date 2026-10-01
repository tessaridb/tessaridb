#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::sync::Arc;

use rust_decimal::Decimal;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_ql::{ExprKind, Span};
use tessari_storage::{Catalog, Store, Window};
use tessari_types::{DatabaseId, NamespaceId, Number, RecordId, Value};

use super::{Folded, Partial, Reduce, merges_exactly, reducing};
use crate::session::Session;

/// A collection of two records, and every one of them as stored.
fn found() -> (Store, Vec<(RecordId, Vec<u8>)>) {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&store)
        .run(
            "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION r; \
             CREATE r:'a' = { n: 1, f: 0.5, s: 'x' }; CREATE r:'b' = { n: 2, f: 1.5, s: 'y' };",
        )
        .unwrap();
    let (namespace, database) = (NamespaceId::new(1), DatabaseId::new(1));
    let mut transaction = store.begin().unwrap();
    let table = Catalog::new(&mut transaction)
        .table_id(namespace, database, "r")
        .unwrap()
        .unwrap();
    let found = transaction
        .records_between(
            namespace,
            database,
            table,
            Window {
                from: None,
                to: None,
            },
            None,
            usize::MAX,
        )
        .unwrap();
    transaction.rollback();
    (store, found)
}

/// Fold `folds` over every record, under `condition` and `visible`.
fn reduced(
    folds: &[(&str, Option<&str>)],
    condition: Option<&str>,
    visible: Option<&[&str]>,
) -> Option<Vec<Partial>> {
    let (store, found) = found();
    let text = |text: &str| (text.to_owned(), tessari_ql::Parameters::new());
    let reduce = Reduce {
        visible: visible.map(|fields| {
            fields
                .iter()
                .map(|field| (*field).to_owned())
                .collect::<BTreeSet<_>>()
        }),
        condition: condition.map(text),
        keys: Vec::new(),
        folds: folds
            .iter()
            .map(|(fold, over)| Folded::named(fold, over.map(text)).unwrap())
            .collect(),
    };
    reducing(&store, &reduce, found).unwrap()
}

fn decimal(held: i64) -> Value {
    Value::Number(Number::Decimal(Decimal::from(held)))
}

#[test]
fn exact_folds_answer_one_state_each_for_the_group() {
    let partials = reduced(
        &[
            ("count", None),
            ("sum", Some("n")),
            ("mean", Some("n")),
            ("min", Some("s")),
            ("max", Some("s")),
        ],
        None,
        None,
    )
    .expect("folded");
    assert_eq!(
        partials,
        vec![Partial {
            key: Vec::new(),
            first: RecordId::from("a"),
            states: vec![
                Value::from(2_i64),
                Value::Array(vec![decimal(3), Value::Bool(true)]),
                Value::Array(vec![decimal(3), Value::from(2_i64)]),
                Value::from("x"),
                Value::from("y"),
            ],
        }]
    );
}

#[test]
fn a_float_offered_to_sum_or_mean_declines() {
    assert_eq!(reduced(&[("mean", Some("f"))], None, None), None);
    assert_eq!(reduced(&[("sum", Some("f"))], None, None), None);
    // `min` and `max` keep a value rather than adding it, so order cannot move them.
    assert!(reduced(&[("min", Some("f"))], None, None).is_some());
}

#[test]
fn a_condition_the_asker_would_doubt_declines() {
    // Not a boolean: the asker would refuse the read.
    assert_eq!(reduced(&[("count", None)], Some("s"), None), None);
    // Two kinds compared: the asker would carry a note.
    assert_eq!(reduced(&[("count", None)], Some("(n > s)"), None), None);
    let kept = reduced(&[("count", None)], Some("(n > 1)"), None).expect("folded");
    assert_eq!(kept[0].states, vec![Value::from(1_i64)]);
    assert_eq!(kept[0].first, RecordId::from("b"));
}

#[test]
fn a_field_the_asker_cannot_see_is_absent_to_the_fold() {
    let partials = reduced(
        &[("count", Some("n")), ("sum", Some("n"))],
        None,
        Some(&["s"]),
    )
    .expect("folded");
    assert_eq!(
        partials[0].states,
        vec![
            Value::from(0_i64),
            Value::Array(vec![decimal(0), Value::Bool(true)])
        ]
    );
}

#[test]
fn the_asker_keeps_its_own_floats_out_of_a_merge_too() {
    let fold = |fold| ExprKind::Fold {
        fold,
        over: None,
        at: None,
        span: Span::new(0, 0),
    };
    let float = Value::Number(Number::float(0.5));
    assert!(!merges_exactly(&fold(tessari_ql::Aggregate::Sum), &float));
    assert!(!merges_exactly(&fold(tessari_ql::Aggregate::Mean), &float));
    assert!(merges_exactly(&fold(tessari_ql::Aggregate::Max), &float));
    assert!(merges_exactly(
        &fold(tessari_ql::Aggregate::Sum),
        &Value::from(1_i64)
    ));
}
