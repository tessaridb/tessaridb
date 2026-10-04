use tessari_encoding::IndexValues;
use tessari_types::Value;

use super::{Walk, estimate_equality, estimate_range};

/// A one-field index over `n % 10` for five thousand records, walked in
/// index order as the entries would be.
fn walked(values: impl Iterator<Item = i64>) -> tessari_encoding::IndexStatistics {
    let mut sorted: Vec<i64> = values.collect();
    sorted.sort_unstable();
    let mut walk = Walk::new(1);
    for value in sorted {
        walk.take(&IndexValues::of(&[Value::from(value)]))
            .expect("an encoding this test wrote");
    }
    walk.finish(5_000, 0)
}

#[test]
fn a_walk_counts_entries_and_distinct_values() {
    let statistics = walked((0..5_000).map(|n| n % 10));
    assert_eq!(statistics.entries, 5_000);
    assert_eq!(statistics.distinct, vec![10]);
    assert_eq!(statistics.common.len(), 10);
    assert!(statistics.bounds.len() > 2 && statistics.bounds.len() <= 129);
}

#[test]
fn a_common_value_is_estimated_from_its_own_count() {
    // Two in five records hold zero; the rest spread over 997 values.
    let skewed = |n: i64| if n % 5 < 2 { 0 } else { n % 997 };
    let statistics = walked((0..5_000).map(skewed));
    let zeros = u64::try_from((0..5_000).filter(|n| skewed(*n) == 0).count()).unwrap_or(0);
    assert_eq!(estimate_equality(&statistics, &[Value::from(0_i64)]), zeros);
    let rare = estimate_equality(&statistics, &[Value::from(17_i64)]);
    assert!((1..=6).contains(&rare), "a rare value estimated at {rare}");
}

#[test]
fn a_value_beyond_every_bound_is_estimated_at_nothing() {
    let statistics = walked(0..5_000);
    assert_eq!(estimate_equality(&statistics, &[Value::from(9_999_i64)]), 0);
    assert_eq!(estimate_equality(&statistics, &[Value::from(-1_i64)]), 0);
}

#[test]
fn a_range_is_estimated_within_a_bucket_at_each_end() {
    let statistics = walked(0..5_000);
    let bucket = 5_000 / u64::try_from(statistics.bounds.len() - 1).unwrap_or(1);
    for (lower, upper, actual) in [
        (Some(4_900_i64), None, 100_u64),
        (Some(4_000), None, 1_000),
        (Some(1_000), None, 4_000),
        (None, Some(2_499), 2_500),
        (Some(1_000), Some(1_999), 1_000),
    ] {
        let estimate = estimate_range(
            &statistics,
            lower.map(Value::from).as_ref(),
            upper.map(Value::from).as_ref(),
        );
        assert!(
            estimate.abs_diff(actual) <= bucket * 2,
            "{lower:?}..{upper:?} estimated {estimate}, actual {actual}, bucket {bucket}"
        );
    }
}
