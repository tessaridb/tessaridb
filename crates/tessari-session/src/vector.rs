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
//! (`docs/tessariql.md` §5, and deliberately so), which means
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

use tessari_ql::Function;
use tessari_types::{Number, Value};

/// The distance between two values, when both are vectors of one length.
///
/// Anything that is not a pair of same-length vectors is infinitely far; see the
/// module documentation for why that is a distance rather than an absence.
pub(crate) fn distance(function: Function, left: &Value, right: &Value) -> Value {
    let (Value::Array(left), Value::Array(right)) = (left, right) else {
        return unreachable_distance();
    };
    if left.is_empty() || left.len() != right.len() {
        return unreachable_distance();
    }
    // One pass over both, allocating nothing: a nearest-neighbour scan calls
    // this once per record, and collecting each side into a vector of floats
    // first — the query's own side again on every record — was a sixth of the
    // scan (G040 M6). Every sum starts at `-0.0` and adds in component order,
    // which is exactly what `Iterator::sum` does, so the answers are the same
    // floats to the bit.
    let (mut dot, mut squared, mut left_norm, mut right_norm) = (-0.0, -0.0, -0.0, -0.0);
    let (mut manhattan, mut differing, mut both, mut either) = (-0.0, 0_u64, 0_u64, 0_u64);
    for (a, b) in left.iter().zip(right) {
        let (Some(a), Some(b)) = (approximate(a), approximate(b)) else {
            return unreachable_distance();
        };
        dot += a * b;
        squared += (a - b) * (a - b);
        left_norm += a * a;
        right_norm += b * b;
        manhattan += (a - b).abs();
        if a != b {
            differing = differing.saturating_add(1);
        }
        // The supports: the positions where each side is nonzero. Jaccard
        // reads their overlap; a zero is no support, exactly as a zero vector
        // points nowhere for the cosine.
        if a != 0.0 || b != 0.0 {
            either = either.saturating_add(1);
            if a != 0.0 && b != 0.0 {
                both = both.saturating_add(1);
            }
        }
    }
    match function {
        Function::VectorDot => Value::Number(Number::float(dot)),
        Function::VectorEuclidean => Value::Number(Number::float(squared.sqrt())),
        Function::VectorManhattan => Value::Number(Number::float(manhattan)),
        // The one distance in the family that is exactly countable, so it
        // answers an integer rather than a float that could only approximate
        // one.
        Function::VectorHamming => Value::Number(Number::Integer(
            i64::try_from(differing).unwrap_or(i64::MAX),
        )),
        Function::VectorJaccard => {
            if either == 0 {
                // Two zero vectors share no support, so there is nothing for
                // an overlap to be a share of — no distance, by the same rule
                // that gives a zero vector no angle.
                return unreachable_distance();
            }
            let share = crate::accumulate::count(both) / crate::accumulate::count(either);
            Value::Number(Number::float(1.0 - share))
        }
        Function::VectorCosine => {
            let magnitude = left_norm.sqrt() * right_norm.sqrt();
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

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use tessari_ql::Function;
    use tessari_types::{Number, Value};

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

    /// The implementation this one replaced, kept as the oracle: each side
    /// collected into floats, then summed with `Iterator::sum`.
    fn collected(function: Function, left: &Value, right: &Value) -> Value {
        fn numbers(value: &Value) -> Option<Vec<f64>> {
            let Value::Array(items) = value else {
                return None;
            };
            items.iter().map(super::approximate).collect()
        }
        fn norm(vector: &[f64]) -> f64 {
            vector.iter().map(|held| held * held).sum::<f64>().sqrt()
        }
        let (Some(left), Some(right)) = (numbers(left), numbers(right)) else {
            return super::unreachable_distance();
        };
        if left.is_empty() || left.len() != right.len() {
            return super::unreachable_distance();
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
                    return super::unreachable_distance();
                }
                Value::Number(Number::float(1.0 - dot / magnitude))
            }
            _ => super::unreachable_distance(),
        }
    }

    fn bits(value: &Value) -> u64 {
        match value {
            Value::Number(Number::Float(held)) => held.to_bits(),
            other => panic!("a distance answered {other:?}"),
        }
    }

    #[test]
    fn one_pass_answers_the_same_floats_as_collecting_first() {
        // Components drawn from a fixed sequence: floats of every sign and
        // scale, exact decimals, zeros, and now and then something that is not
        // a number, over lengths that sometimes differ.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let component = |next: &mut dyn FnMut() -> u64| -> Value {
            let draw = next();
            let magnitude = f64::from(u32::try_from(draw >> 40).unwrap_or(0)) / 1024.0 - 8192.0;
            match draw % 11 {
                0 => Value::Number(Number::float(0.0)),
                1 => Value::Number(Number::Decimal(rust_decimal::Decimal::new(
                    i64::try_from(draw >> 44).unwrap_or(0) - 500_000,
                    4,
                ))),
                2 if draw.is_multiple_of(7) => Value::from("x"),
                _ => Value::Number(Number::float(magnitude)),
            }
        };
        let mut compared = 0_u32;
        for _ in 0..4_000 {
            let length = usize::try_from(next() % 9).unwrap_or(0);
            let other = if next() % 13 == 0 { length + 1 } else { length };
            let left = Value::Array((0..length).map(|_| component(&mut next)).collect());
            let right = Value::Array((0..other).map(|_| component(&mut next)).collect());
            for function in [
                Function::VectorCosine,
                Function::VectorEuclidean,
                Function::VectorDot,
            ] {
                assert_eq!(
                    bits(&distance(function, &left, &right)),
                    bits(&collected(function, &left, &right)),
                    "{function:?} {left:?} {right:?}"
                );
                compared += 1;
            }
        }
        assert_eq!(compared, 12_000);
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

#[cfg(test)]
mod breadth_tests {
    #![allow(clippy::panic)]

    use tessari_ql::Function;
    use tessari_types::{Number, Value};

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
    fn manhattan_is_the_sum_of_the_absolute_differences() {
        // |1−4| + |2−0| + |−3−(−3)| = 3 + 2 + 0.
        let left = vector(&[1.0, 2.0, -3.0]);
        let right = vector(&[4.0, 0.0, -3.0]);
        assert!(near(
            &distance(Function::VectorManhattan, &left, &right),
            5.0
        ));
        assert!(near(
            &distance(Function::VectorManhattan, &left, &left),
            0.0
        ));
    }

    #[test]
    fn hamming_counts_the_positions_that_differ() {
        let left = vector(&[1.0, 2.0, -3.0]);
        let right = vector(&[1.0, 0.0, 3.0]);
        assert_eq!(
            distance(Function::VectorHamming, &left, &right),
            Value::Number(Number::Integer(2))
        );
        assert_eq!(
            distance(Function::VectorHamming, &left, &left),
            Value::Number(Number::Integer(0))
        );
    }

    #[test]
    fn jaccard_is_the_distance_between_the_supports() {
        // Supports {0, 1} and {1, 2}: overlap 1 of union 3, distance 2/3.
        let left = vector(&[5.0, 7.0, 0.0]);
        let right = vector(&[0.0, 2.0, 9.0]);
        assert!(near(
            &distance(Function::VectorJaccard, &left, &right),
            1.0 - 1.0 / 3.0
        ));
        // Equal supports of any weights: distance zero.
        assert!(near(
            &distance(Function::VectorJaccard, &left, &vector(&[1.0, 9.0, 0.0])),
            0.0
        ));
        // Disjoint supports: distance one.
        assert!(near(
            &distance(
                Function::VectorJaccard,
                &vector(&[1.0, 0.0]),
                &vector(&[0.0, 1.0])
            ),
            1.0
        ));
        // Two zero vectors share no support: no distance, by the zero-vector
        // rule the cosine keeps.
        let zero = vector(&[0.0, 0.0]);
        assert!(matches!(
            distance(Function::VectorJaccard, &zero, &zero),
            Value::Number(Number::Float(held)) if held.is_infinite()
        ));
    }
}
