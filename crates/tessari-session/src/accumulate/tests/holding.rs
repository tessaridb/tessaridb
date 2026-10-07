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
