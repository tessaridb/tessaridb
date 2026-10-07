//! The two sketches on their own (ADR-0122 C2, C3): their bound across the
//! range, one state for every merge order, the collapse, and the bytes.
//!
//! Expectations come from the values themselves — a set's size, a sorted
//! vector's element — never from a second sketch.

use super::super::distinct::{Distinct, SMALL};
use super::super::quantile::{MOST, Quantile};
use super::Value;
use tessari_types::Number;

fn whole(at: i64) -> Value {
    Value::Number(Number::Integer(at))
}

fn counted(sketch: &Distinct) -> i64 {
    match sketch.answer() {
        Value::Number(Number::Integer(held)) => held,
        other => panic!("not a count: {other:?}"),
    }
}

fn float(value: &Value) -> f64 {
    match value {
        Value::Number(Number::Float(held)) => *held,
        other => panic!("not a float: {other:?}"),
    }
}

/// Every order three parts can be merged in.
const ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

#[test]
fn a_distinct_count_is_exact_to_the_small_set_and_within_its_bound_past_it() {
    for size in [1, 500, 1024, 1025, 2_000, 20_000, 60_000, 200_000] {
        let mut sketch = Distinct::default();
        for at in 0..size {
            sketch.offer(&whole(at));
            // Offered twice: a second sighting is not a second value.
            sketch.offer(&whole(at));
        }
        let estimate = counted(&sketch);
        if usize::try_from(size).unwrap() <= SMALL {
            assert_eq!(estimate, size, "exact below the small set");
        } else {
            let error = (estimate - size).abs();
            // 2.5 % of the size, in whole values.
            assert!(error * 40 <= size, "{estimate} for {size}");
        }
    }
}

#[test]
fn a_distinct_count_merged_in_any_order_reaches_the_state_one_walk_reaches() {
    // Overlapping parts: one stays small, two pass the small set together.
    let ranges = [0..300, 200..1_100, 1_000..6_000];
    let parts: Vec<Distinct> = ranges
        .iter()
        .map(|range| {
            let mut part = Distinct::default();
            for at in range.clone() {
                part.offer(&whole(at));
            }
            part
        })
        .collect();
    let mut walked = Distinct::default();
    for at in 0..6_000 {
        walked.offer(&whole(at));
    }
    for order in ORDERS {
        let mut merged = Distinct::default();
        for at in order {
            merged.absorb(&parts[at]);
        }
        assert_eq!(merged, walked, "{order:?}");
        assert_eq!(merged.state(), walked.state(), "{order:?}");
    }
    // Merging a part twice changes nothing.
    let mut twice = walked.clone();
    twice.absorb(&parts[2]);
    assert_eq!(twice, walked);
}

/// `count` values `base^k`, k = 0.., spread over three parts in turn.
fn spread(base: f64, count: i32) -> (Vec<f64>, [Quantile; 3]) {
    let mut values = Vec::new();
    let mut parts = [
        Quantile::default(),
        Quantile::default(),
        Quantile::default(),
    ];
    for k in 0..count {
        let value = base.powi(k);
        values.push(value);
        parts[usize::try_from(k % 3).unwrap()].offer(value);
    }
    (values, parts)
}

#[test]
fn a_quantile_merged_in_any_order_reaches_the_state_one_walk_reaches() {
    // 1.03 is wider than a bucket, so each value has a bucket of its own and
    // 2 200 of them pass the 2 048 a sign keeps: the sketch collapses.
    let (values, parts) = spread(1.03, 2_200);
    let mut walked = Quantile::default();
    for value in &values {
        walked.offer(*value);
    }
    assert!(walked.collapsed());
    for order in ORDERS {
        let mut merged = Quantile::default();
        for at in order {
            merged.absorb(&parts[at]);
        }
        assert_eq!(merged, walked, "{order:?}");
        assert_eq!(merged.state(), walked.state(), "{order:?}");
    }
    // The ranks above the collapsed buckets are still within α.
    let mut sorted = values;
    sorted.sort_by(f64::total_cmp);
    for rank in [0.5, 0.9, 1.0] {
        let at = (rank * 2_199.0_f64).floor();
        let expected =
            sorted[usize::try_from(Number::float(at).as_exact_integer().unwrap()).unwrap()];
        let answer = float(&walked.answer(rank));
        assert!(
            ((answer - expected) / expected).abs() <= 0.01,
            "rank {rank}"
        );
    }
}

#[test]
fn a_quantile_short_of_its_bucket_limit_does_not_collapse() {
    let (values, _) = spread(1.03, i32::try_from(MOST).unwrap());
    let mut walked = Quantile::default();
    for value in &values {
        walked.offer(*value);
    }
    assert!(!walked.collapsed());
}

#[test]
fn a_quantile_is_within_its_relative_error_at_every_rank_over_mixed_signs() {
    // A deterministic spread over twelve orders of magnitude, both signs, and zeros.
    let mut values = Vec::new();
    let mut quantile = Quantile::default();
    for k in 0..100_000_i32 {
        let magnitude = 10.0_f64.powf(f64::from(k % 1_200) / 100.0 - 6.0);
        let value = match k % 7 {
            0 => 0.0,
            1 | 2 => -magnitude,
            _ => magnitude,
        };
        values.push(value);
        quantile.offer(value);
    }
    values.sort_by(f64::total_cmp);
    for step in 0..=100 {
        let rank = f64::from(step) / 100.0;
        let at = Number::float((rank * 99_999.0).floor())
            .as_exact_integer()
            .unwrap();
        let expected = values[usize::try_from(at).unwrap()];
        let answer = float(&quantile.answer(rank));
        if expected == 0.0 {
            assert_eq!(answer, 0.0, "rank {rank}");
        } else {
            assert!(
                ((answer - expected) / expected).abs() <= 0.01,
                "rank {rank}: {answer} for {expected}"
            );
        }
    }
}

#[test]
fn a_sketch_reads_back_from_its_bytes_and_refuses_bytes_that_are_not_one() {
    let mut small = Distinct::default();
    let mut large = Distinct::default();
    for at in 0..5_000 {
        if at < 10 {
            small.offer(&whole(at));
        }
        large.offer(&whole(at));
    }
    for sketch in [small, large] {
        assert_eq!(Distinct::from_state(&sketch.state()), Some(sketch));
    }
    let (_, parts) = spread(1.5, 40);
    let mut quantile = parts[0].clone();
    quantile.offer(-2.0);
    quantile.offer(0.0);
    assert_eq!(
        Quantile::from_state(&quantile.state()),
        Some(quantile.clone())
    );

    let Value::Bytes(bytes) = quantile.state() else {
        panic!("a sketch's state is bytes");
    };
    // Cut short, and a tag that is not this sketch's.
    let cut = Value::Bytes(bytes[..bytes.len() - 1].to_vec());
    assert_eq!(Quantile::from_state(&cut), None);
    let mut other = bytes.clone();
    other[0] = 2;
    assert_eq!(Quantile::from_state(&Value::Bytes(other)), None);
    assert_eq!(Distinct::from_state(&Value::Bytes(vec![2, 0, 0])), None);
    assert_eq!(Distinct::from_state(&Value::Bytes(vec![1, 0, 0])), None);
    assert_eq!(Distinct::from_state(&whole(3)), None);
}
