//! The distance between two vectors.
//!
//! # A vector is an array of numbers
//!
//! Not a sixteenth value type. The value system's set is fixed (ADR-0002), an
//! array of numbers *is* a vector, and a new type would need an encoding, an
//! ordering, a literal syntax and a migration to buy nothing. What a dimension
//! is, and whether two vectors share one, is a question these functions answer.
//!
//! # Three distances, because three questions
//!
//! Embedding models are trained for one measure or another, and using the wrong
//! one returns plausible neighbours that are not the nearest — silently. So all
//! three exist rather than one being picked: the angle, the distance in space,
//! and the inner product.
//!
//! # Cosine answers a distance, not a similarity
//!
//! `1 - cos θ`, so smaller is nearer and `ORDER BY` reads the way every other
//! ordering reads. A similarity would sort backwards, and every nearest-neighbour
//! query in the language would carry a `DESC` nobody could explain.
//!
//! # What has no distance is infinitely far, not absent
//!
//! Two arrays of different lengths, an empty one, a non-array, a record with no
//! embedding at all: none of those has a distance. They answer `+∞` — which the
//! value system already places above every number — rather than `NONE`.
//!
//! That is not a detail, and a test found it. `NONE` sorts **below** every value
//! (`docs/bgvql.md` §5, and deliberately so), which means
//! `ORDER BY vector::cosine(embedding, …) LIMIT 3` would answer with the records
//! that have *no embedding at all*, in first place, looking exactly like results.
//! A distance function answers a distance, and the distance to something that is
//! not there is unbounded — so the records without one sort last, where they
//! belong, and appear only when there are not enough real neighbours to fill the
//! bound.
//!
//! The two are still told apart, by the thing that tells values apart:
//! `WHERE embedding = NONE`. A distance is not the place to carry that
//! distinction, because sorting is the only thing a distance is for.

use bgv_db_ql::Function;
use bgv_db_types::{Number, Value};

/// The distance between two values, when both are vectors of one length.
///
/// Anything that is not a pair of same-length vectors is infinitely far; see the
/// module documentation for why that is a distance rather than an absence.
pub(crate) fn distance(function: Function, left: &Value, right: &Value) -> Value {
    let (Some(left), Some(right)) = (numbers(left), numbers(right)) else {
        return unreachable_distance();
    };
    if left.is_empty() || left.len() != right.len() {
        return unreachable_distance();
    }

    let dot: f64 = left.iter().zip(right.iter()).map(|(a, b)| a * b).sum();
    match function {
        Function::VectorDot => Value::Number(Number::float(dot)),
        Function::VectorEuclidean => {
            let squared: f64 = left
                .iter()
                .zip(right.iter())
                .map(|(a, b)| (a - b) * (a - b))
                .sum();
            Value::Number(Number::float(squared.sqrt()))
        }
        Function::VectorCosine => {
            let magnitude = norm(&left) * norm(&right);
            if magnitude == 0.0 {
                // A zero vector points nowhere, so there is no angle to it —
                // and no angle is as far away as it gets.
                return unreachable_distance();
            }
            Value::Number(Number::float(1.0 - dot / magnitude))
        }
        _ => unreachable_distance(),
    }
}

/// How far away something with no distance is.
fn unreachable_distance() -> Value {
    Value::Number(Number::float(f64::INFINITY))
}

/// The vector a value holds, if it holds one.
fn numbers(value: &Value) -> Option<Vec<f64>> {
    let Value::Array(items) = value else {
        return None;
    };
    items.iter().map(approximate).collect()
}

/// One component as a float, refusing anything that is not a number.
fn approximate(value: &Value) -> Option<f64> {
    match value {
        Value::Number(Number::Float(held)) => Some(*held),
        Value::Number(other) => other
            .as_decimal()
            .and_then(|exact| f64::try_from(exact).ok()),
        _ => None,
    }
}

fn norm(vector: &[f64]) -> f64 {
    vector.iter().map(|held| held * held).sum::<f64>().sqrt()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use bgv_db_ql::Function;
    use bgv_db_types::{Number, Value};

    use super::distance;

    fn vector(components: &[f64]) -> Value {
        Value::Array(
            components
                .iter()
                .map(|held| Value::Number(Number::float(*held)))
                .collect(),
        )
    }

    fn near(value: &Value, expected: f64) -> bool {
        match value {
            Value::Number(Number::Float(held)) => (held - expected).abs() < 1e-9,
            _ => false,
        }
    }

    #[test]
    fn cosine_is_a_distance_so_smaller_is_nearer() {
        // The decision that keeps every nearest-neighbour query from needing a
        // `DESC` nobody could explain.
        let one = vector(&[1.0, 0.0]);
        assert!(near(&distance(Function::VectorCosine, &one, &one), 0.0));
        assert!(near(
            &distance(Function::VectorCosine, &one, &vector(&[0.0, 1.0])),
            1.0
        ));
        assert!(near(
            &distance(Function::VectorCosine, &one, &vector(&[-1.0, 0.0])),
            2.0
        ));
    }

    #[test]
    fn cosine_ignores_magnitude_and_euclidean_does_not() {
        // Which is the whole reason both exist: an embedding model trained for
        // one gives plausible-looking wrong neighbours under the other.
        let short = vector(&[1.0, 0.0]);
        let long = vector(&[100.0, 0.0]);
        assert!(near(&distance(Function::VectorCosine, &short, &long), 0.0));
        assert!(near(
            &distance(Function::VectorEuclidean, &short, &long),
            99.0
        ));
    }

    #[test]
    fn the_inner_product_is_the_third_question() {
        assert!(near(
            &distance(
                Function::VectorDot,
                &vector(&[1.0, 2.0]),
                &vector(&[3.0, 4.0])
            ),
            11.0
        ));
    }

    fn unreachable(value: &Value) -> bool {
        matches!(value, Value::Number(Number::Float(held)) if held.is_infinite() && *held > 0.0)
    }

    #[test]
    fn what_has_no_distance_is_infinitely_far_rather_than_absent() {
        let two = vector(&[1.0, 2.0]);
        for (left, right) in [
            // Different lengths.
            (two.clone(), vector(&[1.0])),
            // Empty.
            (vector(&[]), vector(&[])),
            // Not an array.
            (Value::from("not a vector"), two.clone()),
            // An array holding something that is not a number.
            (
                Value::Array(vec![Value::from("x"), Value::from("y")]),
                two.clone(),
            ),
            // Absent.
            (Value::None, two.clone()),
        ] {
            // `NONE` would sort **below** every value, so a bounded
            // nearest-neighbour read would answer with the records that have no
            // embedding at all, in first place, looking exactly like results.
            assert!(
                unreachable(&distance(Function::VectorCosine, &left, &right)),
                "{left:?} vs {right:?}"
            );
        }
        // And a zero vector points nowhere, so it has no angle.
        assert!(unreachable(&distance(
            Function::VectorCosine,
            &vector(&[0.0, 0.0]),
            &two
        )));
        // …though it does have a distance in space.
        assert!(matches!(
            distance(Function::VectorEuclidean, &vector(&[0.0, 0.0]), &two),
            Value::Number(_)
        ));
    }
}
