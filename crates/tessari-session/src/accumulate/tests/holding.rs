//! The holding folds — `median`, `collect` and the counter folds — reduced in
//! parts and merged in key order answer what one walk answers (ADR-0121).

use tessari_types::Datetime;

use super::{Accumulator, Aggregate, Number, Value, decimal, float, integer, span};

/// The walk: every value offered to one accumulator.
fn walked(aggregate: Aggregate, values: &[Value]) -> Value {
    let mut accumulator = Accumulator::for_aggregate(aggregate, span());
    for value in values {
        accumulator.offer(value).unwrap();
    }
    accumulator.finish().unwrap()
}

/// The parts `values` falls into at `cuts`, in key order.
fn parts<'a>(values: &'a [Value], cuts: &[usize]) -> Vec<&'a [Value]> {
    let mut parts = Vec::new();
    let mut from = 0;
    for &cut in cuts {
        parts.push(&values[from..cut]);
        from = cut;
    }
    parts.push(&values[from..]);
    parts
}

/// One accumulator per part, each reduced on its own, as a leader does.
fn reduced(aggregate: Aggregate, part: &[Value]) -> Accumulator {
    let mut accumulator = Accumulator::for_aggregate(aggregate, span());
    for value in part {
        accumulator.offer(value).unwrap();
    }
    accumulator
}

/// The first part offered here, as this node's own span is; every other part
/// travels as its state and is merged in key order, as a leader's is.
fn merged(aggregate: Aggregate, values: &[Value], cuts: &[usize]) -> Accumulator {
    let parts = parts(values, cuts);
    let mut here = reduced(aggregate, parts[0]);
    for part in &parts[1..] {
        let state = reduced(aggregate, part)
            .state()
            .unwrap_or_else(|| panic!("{aggregate:?} has no state to send"));
        assert!(
            here.merge(&state).unwrap(),
            "{aggregate:?} refused its own state"
        );
    }
    here
}

/// Every two-way cut and every three-way cut of `values`.
fn every_cut(values: &[Value]) -> Vec<Vec<usize>> {
    let mut cuts = Vec::new();
    for first in 0..=values.len() {
        cuts.push(vec![first]);
        for second in first..=values.len() {
            cuts.push(vec![first, second]);
        }
    }
    cuts
}

#[test]
fn a_median_reduced_in_parts_answers_the_walk() {
    let groups = [
        vec![integer(5)],
        vec![integer(3), integer(1), integer(2)],
        // An even count: the exact mean of the two in the middle.
        vec![integer(4), integer(1), integer(3), integer(2)],
        vec![
            decimal("2.50"),
            integer(2),
            decimal("2.5"),
            integer(9),
            decimal("-1.25"),
        ],
        vec![
            float(0.1),
            float(0.2),
            integer(1),
            float(0.3),
            Value::None,
            Value::Null,
        ],
        vec![
            integer(7),
            integer(7),
            integer(7),
            integer(1),
            integer(7),
            integer(9),
        ],
    ];
    for values in &groups {
        let expected = walked(Aggregate::Median, values);
        for cuts in every_cut(values) {
            let answer = merged(Aggregate::Median, values, &cuts).finish().unwrap();
            assert_eq!(answer, expected, "median of {values:?} cut at {cuts:?}");
        }
    }
}

#[test]
fn a_median_of_parts_that_hold_nothing_is_nothing() {
    let answer = merged(Aggregate::Median, &[Value::None, Value::Null], &[1])
        .finish()
        .unwrap();
    assert_eq!(answer, Value::None);
}

#[test]
fn a_collect_reduced_in_parts_keeps_the_walks_order() {
    let values = vec![
        integer(3),
        Value::from("b"),
        Value::None,
        decimal("1.50"),
        Value::Null,
        Value::from("a"),
        integer(3),
        float(0.5),
    ];
    let expected = walked(Aggregate::Collect, &values);
    for cuts in every_cut(&values) {
        let answer = merged(Aggregate::Collect, &values, &cuts).finish().unwrap();
        assert_eq!(answer, expected, "collect cut at {cuts:?}");
    }
}

/// A counter sample as the executor offers it: `[value, instant]`.
fn sample(value: Value, second: i64) -> Value {
    Value::Array(vec![value, Value::Datetime(Datetime::from_seconds(second))])
}

/// Series in time order, as a table split by time-ordered identities holds
/// them: a reset in the middle, a tie on an instant, integers, decimals and
/// floats.
fn series() -> Vec<Vec<Value>> {
    vec![
        vec![
            sample(integer(1), 10),
            sample(integer(4), 20),
            sample(integer(9), 30),
            // A reset, wherever a cut falls around it.
            sample(integer(2), 40),
            sample(integer(5), 50),
            sample(integer(5), 50),
            sample(integer(8), 60),
        ],
        vec![
            sample(decimal("0.5"), 1),
            sample(decimal("1.25"), 2),
            sample(decimal("0.75"), 3),
            sample(integer(2), 4),
        ],
        vec![
            sample(float(0.1), 1),
            sample(float(0.3), 2),
            sample(integer(1), 3),
            sample(float(0.2), 4),
            sample(float(1e16), 5),
            sample(float(1e16 + 2.0), 6),
        ],
        vec![sample(integer(3), 7)],
    ]
}

#[test]
fn a_counter_reduced_in_parts_that_follow_one_another_answers_the_walk() {
    for aggregate in [Aggregate::Increase, Aggregate::Rate, Aggregate::Delta] {
        for values in series() {
            let expected = walked(aggregate, &values);
            for cuts in every_cut(&values) {
                let here = merged(aggregate, &values, &cuts);
                assert!(
                    !here.needs_samples(),
                    "{aggregate:?} cut at {cuts:?} chains"
                );
                assert_eq!(
                    here.finish().unwrap(),
                    expected,
                    "{aggregate:?} of {values:?} cut at {cuts:?}"
                );
            }
        }
    }
}

#[test]
fn a_counter_answers_what_its_samples_say_counted_by_hand() {
    // 1, 4, 9, then a reset to 2, then 5, 5, 8: rises 3 + 5 + 2 + 3 + 0 + 3,
    // over the fifty seconds from the first sample to the last.
    let values = &series()[0];
    assert_eq!(walked(Aggregate::Increase, values), integer(16));
    assert_eq!(walked(Aggregate::Delta, values), integer(7));
    assert_eq!(walked(Aggregate::Rate, values), decimal("0.32"));
    // The same with the cut on the reset, the rise across it the merge's own.
    let here = merged(Aggregate::Increase, values, &[3]);
    assert_eq!(here.finish().unwrap(), integer(16));
}

#[test]
fn a_counter_whose_parts_overlap_in_time_asks_for_the_samples() {
    // Two shards whose instants interleave: their summaries cannot be put
    // end to end.
    let near = vec![sample(integer(1), 10), sample(integer(9), 30)];
    let far = vec![sample(integer(4), 20), sample(integer(12), 40)];
    let walk: Vec<Value> = near.iter().chain(&far).cloned().collect();
    for aggregate in [Aggregate::Increase, Aggregate::Rate, Aggregate::Delta] {
        let expected = walked(aggregate, &walk);
        let mut here = reduced(aggregate, &near);
        assert!(
            here.merge(&reduced(aggregate, &far).state().unwrap())
                .unwrap()
        );
        assert!(
            here.needs_samples(),
            "{aggregate:?} overlapping parts chained"
        );
        // Asked again for the samples, the same parts answer the walk.
        let mut again = reduced(aggregate, &near);
        assert!(
            again
                .merge(&reduced(aggregate, &far).samples().unwrap())
                .unwrap()
        );
        assert!(!again.needs_samples());
        assert_eq!(
            again.finish().unwrap(),
            expected,
            "{aggregate:?} from samples"
        );
    }
}

#[test]
fn a_counters_float_rises_are_summed_exactly_and_rounded_once() {
    // Rises of 0.1, 0.2 and 0.3: in floats added in order 0.6000000000000001,
    // and 0.6 summed exactly.
    let values = vec![
        sample(float(0.0), 1),
        sample(float(0.1), 2),
        sample(float(0.0), 3),
        sample(float(0.2), 4),
        sample(float(0.0), 5),
        sample(float(0.3), 6),
    ];
    let answer = walked(Aggregate::Increase, &values);
    assert_eq!(answer, Value::Number(Number::float(0.6)));
}

/// A sample carrying the prediction horizon as its third element.
fn ahead(value: Value, second: i64, ahead: i64) -> Value {
    Value::Array(vec![
        value,
        Value::Datetime(Datetime::from_seconds(second)),
        integer(ahead),
    ])
}

#[test]
fn resets_count_the_falls_and_a_window_of_one_sample_saw_none() {
    // 1, 4, 9, then a fall to 2, then 5, 5, 8: one fall; the equal pair is
    // not one.
    let values = &series()[0];
    assert_eq!(walked(Aggregate::Resets, values), integer(1));
    assert_eq!(
        walked(Aggregate::Resets, &[sample(integer(7), 10)]),
        integer(0)
    );
    assert_eq!(walked(Aggregate::Resets, &[]), Value::None);
    // The count survives the parts being summarised and merged, the fall at a
    // boundary counted by the merge itself.
    for cuts in every_cut(values) {
        let here = merged(Aggregate::Resets, values, &cuts);
        assert_eq!(here.finish().unwrap(), integer(1), "cut at {cuts:?}");
    }
}

#[test]
fn deriv_is_the_least_squares_slope_per_second() {
    // A straight line of slope 0.2 per second, sampled off the line nowhere.
    let line = [
        sample(integer(1), 100),
        sample(integer(3), 110),
        sample(integer(5), 120),
    ];
    let answered = walked(Aggregate::Deriv, &line);
    assert_eq!(format!("{answered:?}"), format!("{:?}", float(0.2)));
    // One sample, or samples at one instant, name no slope.
    assert_eq!(
        walked(Aggregate::Deriv, &[sample(integer(1), 10)]),
        Value::None
    );
    let flat = [sample(integer(1), 10), sample(integer(2), 10)];
    assert_eq!(walked(Aggregate::Deriv, &flat), Value::None);
    // The sums merge exactly, so a cut anywhere answers the same slope.
    for cuts in every_cut(&line) {
        let here = merged(Aggregate::Deriv, &line, &cuts);
        let answered = here.finish().unwrap();
        assert_eq!(
            format!("{answered:?}"),
            format!("{:?}", float(0.2)),
            "cut at {cuts:?}"
        );
    }
}

#[test]
fn predict_linear_extends_the_line_past_the_last_sample() {
    // The same line, extended thirty seconds past its last sample at t=120:
    // 5 + 0.2 * 30 = 11.
    let line = [
        ahead(integer(1), 100, 30),
        ahead(integer(3), 110, 30),
        ahead(integer(5), 120, 30),
    ];
    let answered = walked(Aggregate::PredictLinear, &line);
    assert_eq!(format!("{answered:?}"), format!("{:?}", float(11.0)));
    assert_eq!(
        walked(Aggregate::PredictLinear, &[ahead(integer(1), 10, 30)]),
        Value::None
    );
}

#[test]
fn downsample_keeps_the_ends_and_the_sharpest_point_between_them() {
    // A zigzag (0,0) (1,10) (2,0) (3,10) (4,0) down to three points keeps the
    // first, the last, and (1,10) — the largest triangle against the closing
    // average.
    let zigzag = [
        sample(integer(0), 0),
        sample(integer(10), 1),
        sample(integer(0), 2),
        sample(integer(10), 3),
        sample(integer(0), 4),
    ];
    let answered = walked_downsample(&zigzag, 3);
    assert_eq!(
        answered,
        Value::Array(vec![
            point(integer(0), 0),
            point(integer(10), 1),
            point(integer(0), 4),
        ])
    );
}

#[test]
fn downsample_with_room_for_everything_answers_everything_in_time_order() {
    let few = [sample(integer(7), 30), sample(integer(1), 10)];
    let answered = walked_downsample(&few, 5);
    assert_eq!(
        answered,
        Value::Array(vec![point(integer(1), 10), point(integer(7), 30)])
    );
}

#[test]
fn downsample_merged_in_parts_answers_the_walk() {
    // A downsampling leader's state is its samples — a summary holds no
    // points — so however the group is cut, the merge picks the same points
    // the one walk picks.
    let zigzag = [
        sample(integer(0), 0),
        sample(integer(10), 1),
        sample(integer(0), 2),
        sample(integer(10), 3),
        sample(integer(0), 4),
        sample(integer(3), 5),
        sample(integer(0), 6),
    ];
    let expected = walked_downsample(&zigzag, 4);
    for cuts in every_cut(&zigzag) {
        let parts = parts(&zigzag, &cuts);
        let mut here = reduced_downsample(parts[0], 4);
        for part in &parts[1..] {
            let state = reduced_downsample(part, 4)
                .state()
                .expect("samples to send");
            assert!(here.merge(&state).unwrap(), "cut at {cuts:?}");
        }
        assert_eq!(here.finish().unwrap(), expected, "cut at {cuts:?}");
    }
}

/// One part reduced with the horizon carried on every offer.
fn reduced_downsample(part: &[Value], n: i64) -> Accumulator {
    let mut accumulator = Accumulator::for_aggregate(Aggregate::Downsample, span());
    for value in part {
        let Value::Array(pair) = value else {
            panic!("a sample")
        };
        let mut triple = pair.clone();
        triple.push(integer(n));
        accumulator.offer(&Value::Array(triple)).unwrap();
    }
    accumulator
}

/// `downsample` over the walk with `n` as the horizon element.
fn walked_downsample(values: &[Value], n: i64) -> Value {
    let mut accumulator = Accumulator::for_aggregate(Aggregate::Downsample, span());
    for value in values {
        let Value::Array(pair) = value else {
            panic!("a sample")
        };
        let mut triple = pair.clone();
        triple.push(integer(n));
        accumulator.offer(&Value::Array(triple)).unwrap();
    }
    accumulator.finish().unwrap()
}

/// One answered point, `{ at, value }`.
fn point(value: Value, second: i64) -> Value {
    let mut held = std::collections::BTreeMap::new();
    held.insert(
        "at".to_string(),
        Value::Datetime(Datetime::from_seconds(second)),
    );
    held.insert("value".to_string(), value);
    Value::Object(held)
}

#[test]
fn decayed_folds_weigh_each_sample_by_its_age_in_half_lives() {
    // Samples 2, 4, 8 at t = 0, 10, 20 with a half-life of ten seconds weigh
    // 1/4, 1/2, 1: sum 10.5, mean 6, max 8, count 1.75.
    let samples = [
        sample(integer(2), 0),
        sample(integer(4), 10),
        sample(integer(8), 20),
    ];
    let cases = [
        (Aggregate::DecayedSum, 10.5_f64),
        (Aggregate::DecayedMean, 6.0),
        (Aggregate::DecayedMax, 8.0),
        (Aggregate::DecayedCount, 1.75),
    ];
    for (fold, expected) in cases {
        let answered = walked_third(fold, &samples, 10);
        assert_eq!(
            format!("{answered:?}"),
            format!("{:?}", float(expected)),
            "{fold:?}"
        );
        // The parts send samples and the half-life, so any cut answers alike.
        for cuts in every_cut(&samples) {
            let parts = parts(&samples, &cuts);
            let mut here = reduced_third(fold, parts[0], 10);
            for part in &parts[1..] {
                let state = reduced_third(fold, part, 10).state().expect("samples");
                assert!(here.merge(&state).unwrap(), "{fold:?} cut at {cuts:?}");
            }
            assert_eq!(
                format!("{:?}", here.finish().unwrap()),
                format!("{:?}", float(expected)),
                "{fold:?} cut at {cuts:?}"
            );
        }
    }
    // No samples, no answer; a half-life that is not positive is refused.
    assert_eq!(walked_third(Aggregate::DecayedSum, &[], 10), Value::None);
}

/// The walk with a third element on every offer.
fn walked_third(fold: Aggregate, values: &[Value], third: i64) -> Value {
    reduced_third(fold, values, third).finish().unwrap()
}

/// One part reduced with the third element carried on every offer.
fn reduced_third(fold: Aggregate, part: &[Value], third: i64) -> Accumulator {
    let mut accumulator = Accumulator::for_aggregate(fold, span());
    for value in part {
        let Value::Array(pair) = value else {
            panic!("a sample")
        };
        let mut triple = pair.clone();
        triple.push(integer(third));
        accumulator.offer(&Value::Array(triple)).unwrap();
    }
    accumulator
}
