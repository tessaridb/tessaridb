use super::{Accumulator, Aggregate, Decimal, Number, Retention, Span, Value};
use crate::aggregate::fold;

mod holding;
mod order;
mod sketches;
mod spread;

/// A span the corpus can share; no assertion reads it.
fn span() -> Span {
    Span::new(0, 0)
}

/// Fold these values the way the executor now does — one at a time.
fn incrementally(aggregate: Aggregate, values: &[Value]) -> crate::error::Result<Value> {
    let mut accumulator = Accumulator::for_aggregate(aggregate, span());
    for value in values {
        accumulator.offer(value)?;
    }
    accumulator.finish()
}

/// Every aggregate this store has.
const EVERY: &[Aggregate] = &[
    Aggregate::Count,
    Aggregate::Sum,
    Aggregate::Mean,
    Aggregate::Min,
    Aggregate::Max,
    Aggregate::Variance,
    Aggregate::Stddev,
    Aggregate::Median,
    Aggregate::Increase,
    Aggregate::Rate,
    Aggregate::Delta,
    Aggregate::Collect,
    Aggregate::ApproxDistinct,
    Aggregate::ApproxQuantile,
];

/// A number as a value, spelled once.
fn integer(held: i64) -> Value {
    Value::Number(Number::Integer(held))
}

/// A float as a value.
fn float(held: f64) -> Value {
    Value::Number(Number::float(held))
}

/// An exact decimal as a value.
fn decimal(held: &str) -> Value {
    Value::Number(Number::Decimal(held.parse::<Decimal>().unwrap()))
}

/// The largest exact number there is, so that two of them cannot be added.
///
/// Measured rather than assumed: the first version of the deferred-failure
/// tests used `i64::MAX` twice, which a decimal holds without complaint —
/// so they exercised no failure at all and the falsification that raises one
/// eagerly passed straight through them.
fn beyond_exact() -> Value {
    Value::Number(Number::Decimal(Decimal::MAX))
}

/// Groups a fold has to answer the same way either way.
///
/// Deliberately mixed: kinds that promote differently, values a fold passes
/// over, an order that decides a tie, a group of one and a group of none.
/// The float rows are the ones the design note is about — a total that
/// switched kinds part-way would differ from these in the last bits.
fn corpus() -> Vec<(&'static str, Vec<Value>)> {
    vec![
        ("nothing at all", vec![]),
        ("one integer", vec![integer(7)]),
        ("integers", vec![integer(3), integer(-9), integer(12)]),
        (
            "integers and a decimal",
            vec![integer(3), decimal("0.25"), integer(4)],
        ),
        (
            "a float last, after exact numbers",
            vec![integer(1), decimal("2.5"), float(0.1)],
        ),
        (
            "a float first, before exact numbers",
            vec![float(0.1), integer(1), decimal("2.5")],
        ),
        (
            "floats that do not add associatively",
            vec![
                float(0.1),
                float(0.2),
                float(0.3),
                float(1e16),
                float(-1e16),
            ],
        ),
        (
            "values a fold passes over",
            vec![Value::None, integer(5), Value::Null, integer(5)],
        ),
        ("only values it passes over", vec![Value::None, Value::Null]),
        (
            "equal values of one kind, which a tie cannot tell apart",
            vec![integer(4), integer(4), integer(4)],
        ),
        (
            "values that compare equal and are not the same value",
            // `3`, `3.0` and the decimal `3.0` all order equal, so which one
            // an extreme answers with is decided entirely by whether it
            // replaces on a tie — and the two ends decide it oppositely.
            // Only the structural comparison above can see the difference;
            // measured, not assumed, because the first version of this row
            // asserted value equality and the falsification that reverses
            // the tie rule passed straight through it.
            vec![integer(3), float(3.0), decimal("3.0")],
        ),
        (
            "integers, whose total must still answer as an integer",
            // Guards the promotion, which value equality also cannot see:
            // `Integer(6)` and `Decimal(6)` are equal values and different
            // answers on the wire.
            vec![integer(1), integer(2), integer(3)],
        ),
        (
            "strings, which only the counting folds accept",
            vec![
                Value::String("b".to_owned()),
                Value::String("a".to_owned()),
                Value::String("c".to_owned()),
            ],
        ),
        (
            "kinds the value system orders against each other",
            vec![integer(1), Value::Bool(true), Value::String("a".to_owned())],
        ),
        (
            "an integer total no i64 holds",
            vec![integer(i64::MAX), integer(i64::MAX)],
        ),
        (
            "a total no exact number holds",
            vec![beyond_exact(), beyond_exact()],
        ),
        (
            "a total no exact number holds, with a float after it",
            vec![beyond_exact(), beyond_exact(), float(2.0)],
        ),
    ]
}

/// Whether two answers are the same spread computed two different ways.
///
/// `variance` and `stddev` are the only folds whose reference is a
/// **different algorithm** rather than a different arrangement of the same
/// arithmetic — two passes against exact totals rounded once — and two float
/// algorithms do not agree in the last bits. Demanding that they did would
/// force the oracle to become a copy of the implementation, which proves
/// nothing; so these two are compared within a relative tolerance and every
/// other fold keeps the strict structural comparison below.
///
/// The tolerance is relative and tight: a sign error, a population divisor
/// where a sample one belongs, or a missing square root are all changes of
/// several percent or more on this corpus, and none of them survives it.
fn same_spread(batch: &Value, running: &Value) -> bool {
    const TOLERANCE: f64 = 1e-9;
    match (batch, running) {
        (Value::Number(Number::Float(left)), Value::Number(Number::Float(right))) => {
            let scale = left.abs().max(right.abs()).max(1.0);
            (left - right).abs() / scale < TOLERANCE
        }
        (left, right) => format!("{left:?}") == format!("{right:?}"),
    }
}
