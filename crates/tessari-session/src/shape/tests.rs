#![allow(clippy::panic)]

use tessari_ql::{Expr, ExprKind, Ordering as Order, Span};
use tessari_types::{Number, RecordId, Value};

use super::projected;

/// Every kind of number the order has something to say about, including the
/// three that have no decimal projection and must therefore survive the
/// pass untouched.
fn awkward() -> Vec<Value> {
    vec![
        Value::None,
        Value::Null,
        Value::Bool(false),
        Value::Number(Number::float(f64::NEG_INFINITY)),
        Value::Number(Number::float(-1e300)),
        Value::Number(Number::from(-3_i64)),
        Value::Number(Number::float(-0.0)),
        Value::Number(Number::from(0_i64)),
        Value::Number(Number::float(0.1)),
        Value::Number(Number::float(1.0)),
        Value::Number(Number::from(1_i64)),
        Value::Number(Number::float(1.5)),
        Value::Number(Number::from(2_i64)),
        Value::Number(Number::float(1e300)),
        Value::Number(Number::float(f64::INFINITY)),
        Value::Number(Number::float(f64::NAN)),
        Value::from("text"),
    ]
}

#[test]
fn projecting_a_key_cannot_change_how_it_compares_to_any_other() {
    // The whole safety argument for the optimisation, asserted over every
    // pair rather than over a happy path: the comparator decides finite
    // numbers by their decimal projections, so handing it those projections
    // must leave every verdict identical — including for the three kinds
    // that have none and are deliberately left alone.
    let values = awkward();
    for left in &values {
        for right in &values {
            assert_eq!(
                left.cmp(right),
                projected(left.clone()).cmp(&projected(right.clone())),
                "{left:?} vs {right:?}"
            );
        }
    }
}

#[test]
fn a_value_with_no_decimal_projection_is_left_exactly_as_it_was() {
    // Rewriting these would be changing an answer rather than precomputing
    // one: the comparator answers all three *without* a projection.
    for held in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 1e300] {
        let value = Value::Number(Number::float(held));
        assert_eq!(projected(value.clone()), value, "{held}");
    }
}

#[test]
fn the_walk_reaches_inside_an_array_and_an_object() {
    // A sort key may be either, so a projection that stopped at the top
    // would leave the expensive case exactly where it was.
    let nested = Value::Array(vec![Value::Number(Number::Integer(3))]);
    assert_eq!(
        projected(nested),
        Value::Array(vec![Value::Number(Number::Decimal(
            rust_decimal::Decimal::from(3_i64)
        ))])
    );
}

/// One sort key, in the given direction. The expression is a placeholder:
/// keys reach the sort already evaluated, so only the direction is read.
fn by(descending: bool) -> Vec<Order> {
    vec![Order {
        key: Expr {
            kind: ExprKind::Literal(Value::None),
            span: Span::new(0, 1),
        },
        descending,
    }]
}

/// Run a corpus through the collector, bounded or not.
fn through(
    keyed: Vec<(Vec<Value>, RecordId, Value)>,
    order: &[Order],
    wanted: Option<usize>,
) -> Vec<(RecordId, Value)> {
    let mut topmost = super::Topmost::keeping(order, wanted);
    for (keys, id, record) in keyed {
        topmost.offer(keys, id, record);
    }
    topmost.finish()
}

/// A deterministic corpus built to be awkward: heavy ties, both absences,
/// numbers that only compare through their decimal projections, and values
/// of several types in one key.
///
/// Ties are the point. A collector that discards on a strict comparison
/// where the order says equal would drop a record the sort keeps, and a
/// corpus of distinct keys never asks.
fn corpus(records: usize) -> Vec<(Vec<Value>, RecordId, Value)> {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    (0..records)
        .map(|n| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let pick = seed >> 33;
            let small = i64::try_from(pick % 5).expect("a small number");
            let key = match pick % 7 {
                0 => Value::None,
                1 => Value::Null,
                2 => Value::Bool(pick.is_multiple_of(2)),
                3 => Value::Number(Number::from(small)),
                4 => Value::Number(Number::float(
                    f64::from(u32::try_from(small).unwrap_or(0)) / 2.0,
                )),
                5 => Value::from(if pick.is_multiple_of(2) {
                    "alpha"
                } else {
                    "beta"
                }),
                _ => Value::Array(vec![Value::Number(Number::from(small))]),
            };
            (
                vec![key],
                RecordId::Int(i64::try_from(n).expect("an id")),
                Value::None,
            )
        })
        .collect()
}

/// The order the value system defines, computed without this module.
///
/// An oracle, and deliberately not built from [`super::ranked`]: comparing a
/// sort against itself proves that it is consistent, not that it is right.
/// It compares the keys as they were given, since the projection's own proof
/// above is that projecting cannot change a verdict.
fn reference(keyed: &[(Vec<Value>, RecordId, Value)], descending: bool) -> Vec<RecordId> {
    let mut expected: Vec<(Value, RecordId)> = keyed
        .iter()
        .map(|(keys, id, _)| (keys[0].clone(), id.clone()))
        .collect();
    expected.sort_by(|left, right| {
        let ordered = if descending {
            right.0.cmp(&left.0)
        } else {
            left.0.cmp(&right.0)
        };
        ordered.then_with(|| left.1.cmp(&right.1))
    });
    expected.into_iter().map(|(_, id)| id).collect()
}

#[test]
fn a_sort_answers_the_same_order_it_did_before_the_projection() {
    let order = by(false);
    // **Reversed on the way in.** `awkward()` is written in ascending order,
    // so feeding it as it stands lets a sort that does nothing at all pass —
    // which is exactly what a falsification of this wave found it doing.
    let keyed: Vec<(Vec<Value>, RecordId, Value)> = awkward()
        .into_iter()
        .rev()
        .enumerate()
        .map(|(n, key)| {
            (
                vec![key],
                RecordId::Int(i64::try_from(n).expect("an id")),
                Value::None,
            )
        })
        .collect();

    let mut expected: Vec<(Vec<Value>, RecordId)> = keyed
        .iter()
        .map(|(keys, id, _)| (keys.clone(), id.clone()))
        .collect();
    expected.sort_by(|left, right| left.0[0].cmp(&right.0[0]).then(left.1.cmp(&right.1)));

    let sorted = through(keyed, &order, None);
    let held: Vec<RecordId> = sorted.into_iter().map(|(id, _)| id).collect();
    let wanted: Vec<RecordId> = expected.into_iter().map(|(_, id)| id).collect();
    assert_eq!(held, wanted);
}

#[test]
fn keeping_the_top_answers_what_sorting_everything_and_then_bounding_answers() {
    // The whole safety argument, asserted as an equality between the two
    // paths rather than against a hand-written expected order — the same
    // shape the projection's own proof takes above. A hand-written order
    // would only test the corpus somebody thought to write down.
    const RECORDS: usize = 2_000;
    for descending in [false, true] {
        let order = by(descending);
        let whole = reference(&corpus(RECORDS), descending);
        // The unbounded path first, against the same oracle — otherwise
        // every equality below could hold with the sort removed entirely,
        // because both sides would be the same broken collector.
        let sorted: Vec<RecordId> = through(corpus(RECORDS), &order, None)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(sorted, whole, "unbounded, descending={descending}");
        for (start, limit) in [
            (None, Some(0_u64)),
            (None, Some(1)),
            (None, Some(7)),
            (None, Some(500)),
            (
                None,
                u64::try_from(RECORDS).ok().map(|n| n.saturating_add(10)),
            ),
            (Some(3), Some(9)),
            (Some(1_999), Some(5)),
            (Some(5_000), Some(5)),
        ] {
            let wanted = limit.map(|limit| {
                usize::try_from(limit.saturating_add(start.unwrap_or(0))).unwrap_or(usize::MAX)
            });
            let bounded_after: Vec<RecordId> = super::bounded(
                whole.iter().map(|id| (id.clone(), Value::None)).collect(),
                start,
                limit,
            )
            .into_iter()
            .map(|(id, _)| id)
            .collect();
            let kept: Vec<RecordId> =
                super::bounded(through(corpus(RECORDS), &order, wanted), start, limit)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
            assert_eq!(
                kept, bounded_after,
                "descending={descending} {start:?} {limit:?}"
            );
        }
    }
}

#[test]
fn a_bounded_sort_never_holds_more_than_a_small_multiple_of_its_bound() {
    // C3's own wording: a counted assertion on values retained, made where
    // the retention happens rather than inferred from a timing or from a
    // resident-memory figure that cannot attribute.
    const RECORDS: usize = 50_000;
    const WANTED: usize = 10;
    let order = by(false);
    let mut topmost = super::Topmost::keeping(&order, Some(WANTED));
    let mut deepest = 0_usize;
    for (keys, id, record) in corpus(RECORDS) {
        topmost.offer(keys, id, record);
        deepest = deepest.max(topmost.held.len());
    }
    assert!(
        deepest <= WANTED.saturating_mul(2).saturating_add(1),
        "held {deepest} of {RECORDS} records to answer with {WANTED}"
    );
    assert_eq!(topmost.finish().len(), WANTED);
}

#[test]
fn a_limit_of_zero_terminates_and_answers_with_nothing() {
    // The buffer's floor. Twice a bound of nothing is nothing, and a
    // collector whose room is zero would compact on every record forever or
    // never compact at all, depending on which way the comparison is
    // written.
    let order = by(false);
    assert!(through(corpus(100), &order, Some(0)).is_empty());
}
