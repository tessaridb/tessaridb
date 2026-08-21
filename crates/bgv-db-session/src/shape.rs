//! Giving an answer the shape the statement asked for.
//!
//! Sorting and bounding, and nothing else. Separate from evaluating an
//! expression and from reading records because it is a third question — *which
//! of these, in what order* — and because the two rules it holds are the ones a
//! reader comes looking for.

use bgv_db_ql::Ordering;
use bgv_db_types::{Number, RecordId, Value};

/// The records in the order the statement asked for.
///
/// **The order is the value system's** (`docs/value-system.md` §3),
/// including across types and including the absences: `none` sorts below
/// `null` sorts below every present value. That is the opposite of what a
/// *comparison* does with them — `age < 18` is false for a record with no
/// age — and deliberately so: a comparison against a non-value has no
/// answer, while a sort has to put every row somewhere, and "somewhere" is
/// better stated than left to whichever row the scan reached first.
///
/// **Ties are broken by the record's id**, which is unique, so the answer is
/// the same every time whatever access path ran. Without that, adding an
/// index would reorder equal rows — an answer that changes when an index
/// appears, which is the shape this store keeps refusing.
/// The keys are evaluated **once per record** by the caller rather than inside
/// the comparison, because a sort compares a record many times and an expression
/// is evaluated every time it is asked for.
///
/// # Why the keys are projected before they are sorted
///
/// The value system's order across numbers is defined by their decimal
/// projections, so `Number::cmp` calls `as_decimal()` on **both** sides of every
/// comparison — and for a float that is a real conversion, not an accessor. A
/// sort compares each key about `log n` times, so a two-thousand-record sort on
/// float keys performs some forty thousand conversions to answer twenty-two
/// thousand questions.
///
/// The benchmark harness found this on its first day: a nearest-neighbour read
/// over 2000 records cost 12.5 ms, of which about 10 ms was here rather than in
/// the distance it was ordering by. The give-away was that the cost did not move
/// when the query vector went from one component to eight — a length mismatch
/// short-circuits the distance, so every record scored the same and the sort had
/// nothing to order — and then jumped fivefold at the width where the keys
/// finally differed.
///
/// [`projected`] does the conversion **once per key** and hands the comparator
/// the same decimals it would have computed. It is not an approximation of the
/// order; it is the order's own definition, evaluated eagerly.
pub(crate) fn sorted(
    keyed: Vec<(Vec<Value>, RecordId, Value)>,
    order: &[Ordering],
) -> Vec<(RecordId, Value)> {
    let mut keyed: Vec<(Vec<Value>, RecordId, Value)> = keyed
        .into_iter()
        .map(|(keys, id, record)| (keys.into_iter().map(projected).collect(), id, record))
        .collect();
    keyed.sort_by(|left, right| {
        for (position, key) in order.iter().enumerate() {
            let Some((held, other)) = left.0.get(position).zip(right.0.get(position)) else {
                continue;
            };
            let ordered = if key.descending {
                other.cmp(held)
            } else {
                held.cmp(other)
            };
            if ordered != core::cmp::Ordering::Equal {
                return ordered;
            }
        }
        left.1.cmp(&right.1)
    });
    keyed
        .into_iter()
        .map(|(_, id, record)| (id, record))
        .collect()
}

/// A value with every finite float replaced by the decimal it compares as.
///
/// **This cannot change an order, and the reason is worth stating precisely.**
/// `Number::cmp` decides between two finite numbers by comparing
/// `as_decimal()` of each; `as_decimal()` of a decimal is the decimal itself.
/// So replacing a float by its own projection leaves every comparison with the
/// identical pair of operands it would have computed anyway.
///
/// A float that has **no** decimal projection — infinite, not-a-number, or
/// finite but beyond the range a decimal holds — is left exactly as it is,
/// because those are the three cases the comparator answers *without* a
/// projection, and rewriting them would be changing an answer rather than
/// precomputing one.
///
/// Arrays and objects are walked, since a sort key may be either.
fn projected(value: Value) -> Value {
    match value {
        Value::Number(held) => match held.as_decimal() {
            Some(exact) if held.position_is_finite() => Value::Number(Number::Decimal(exact)),
            _ => Value::Number(held),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(projected).collect()),
        Value::Set(items) => Value::Set(items.into_iter().map(projected).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, held)| (name, projected(held)))
                .collect(),
        ),
        other => other,
    }
}

/// The window a `START` and a `LIMIT` ask for.
///
/// Applied **after** ordering, always — including when no `ORDER BY` was
/// written, because otherwise `LIMIT 10` means "the first ten the scan happened
/// to reach", which is a different answer on a replica.
pub(crate) fn bounded(
    records: Vec<(RecordId, Value)>,
    start: Option<u64>,
    limit: Option<u64>,
) -> Vec<(RecordId, Value)> {
    let mut records = records;
    if let Some(start) = start {
        // A start past the end answers with nothing rather than failing: asking
        // for page nine of an eight-page result is a state, not a mistake.
        let skip = usize::try_from(start).unwrap_or(usize::MAX);
        records = records.into_iter().skip(skip).collect();
    }
    if let Some(limit) = limit {
        let keep = usize::try_from(limit).unwrap_or(usize::MAX);
        records.truncate(keep);
    }
    records
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_types::{Number, RecordId, Value};

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
        let nested = Value::Array(vec![Value::Number(Number::float(1.5))]);
        assert_eq!(
            projected(nested),
            Value::Array(vec![Value::Number(Number::Decimal(
                rust_decimal::Decimal::try_from(1.5_f64).expect("a decimal")
            ))])
        );
    }

    #[test]
    fn a_sort_answers_the_same_order_it_did_before_the_projection() {
        use bgv_db_ql::{Expr, ExprKind, Ordering as Order, Span};

        let order = vec![Order {
            key: Expr {
                kind: ExprKind::Literal(Value::None),
                span: Span::new(0, 1),
            },
            descending: false,
        }];
        let keyed: Vec<(Vec<Value>, RecordId, Value)> = awkward()
            .into_iter()
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

        let sorted = super::sorted(keyed, &order);
        let held: Vec<RecordId> = sorted.into_iter().map(|(id, _)| id).collect();
        let wanted: Vec<RecordId> = expected.into_iter().map(|(_, id)| id).collect();
        assert_eq!(held, wanted);
    }
}
