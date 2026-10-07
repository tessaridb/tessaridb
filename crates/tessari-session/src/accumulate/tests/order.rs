use super::*;

/// The two ends of `median`, and the empty group.
#[test]
fn the_middle_is_the_middle_number_and_the_mean_of_two_when_there_are_two() {
    let odd = incrementally(Aggregate::Median, &[integer(9), integer(1), integer(5)]).unwrap();
    assert_eq!(format!("{odd:?}"), format!("{:?}", decimal("5")));

    let even = incrementally(
        Aggregate::Median,
        &[integer(9), integer(1), integer(5), integer(3)],
    )
    .unwrap();
    assert_eq!(format!("{even:?}"), format!("{:?}", decimal("4")));

    assert_eq!(incrementally(Aggregate::Median, &[]).unwrap(), Value::None);
}

/// The property the exact-and-normalised answer was chosen for.
///
/// Three values that compare **equal** and are three different answers on
/// the wire. Answering with the middle value as written makes the result
/// depend on where the sort left them, which is not a property of the data;
/// the corpus row that says so is what caught the first version of this
/// fold. Here the same multiset is offered in every order it has, and the
/// answer has to be one answer.
#[test]
fn the_middle_does_not_depend_on_the_order_the_records_arrived_in() {
    let equal = [integer(3), float(3.0), decimal("3.0")];
    let orders = [
        [0_usize, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut answers: Vec<String> = Vec::new();
    for order in orders {
        let offered: Vec<Value> = order
            .iter()
            .map(|at| equal.get(*at).unwrap().clone())
            .collect();
        let answered = incrementally(Aggregate::Median, &offered).unwrap();
        answers.push(format!("{answered:?}"));
    }
    answers.dedup();
    assert_eq!(
        answers.len(),
        1,
        "the same three numbers gave more than one median: {answers:?}"
    );
}

/// `collect` answers an empty array and never `NONE`.
///
/// The rule `sum` follows for the same reason: an answer every caller has to
/// write `?? []` after is the wrong answer.
#[test]
fn collecting_nothing_answers_an_empty_array() {
    assert_eq!(
        incrementally(Aggregate::Collect, &[]).unwrap(),
        Value::Array(Vec::new())
    );
    assert_eq!(
        incrementally(Aggregate::Collect, &[Value::None, integer(3), Value::Null]).unwrap(),
        Value::Array(vec![integer(3)]),
        "collect skipped nothing, where every other fold passes over absent values"
    );
}

/// The claim this module exists for, now asserted per class.
///
/// Weakening it to `held() <= n` when `collect` and `median` arrived would
/// have deleted the guarantee for the seven folds that still have it. So the
/// exception is **named** — [`Aggregate::retention`] — and the assertion
/// splits along it.
#[test]
fn a_constant_space_fold_holds_at_most_one_value_however_many_it_is_offered() {
    const OFFERED: i64 = 50_000;
    for aggregate in EVERY
        .iter()
        .filter(|fold| fold.retention() == Retention::Constant)
    {
        let mut accumulator = Accumulator::for_aggregate(*aggregate, span());
        for held in 0..OFFERED {
            // A quantile is offered each value with its rank.
            let offered = if *aggregate == Aggregate::ApproxQuantile {
                Value::Array(vec![integer(held), float(0.5)])
            } else {
                integer(held)
            };
            accumulator.offer(&offered).unwrap();
            assert!(
                accumulator.held() <= 1,
                "{aggregate:?} held {} values after {} offers",
                accumulator.held(),
                held.saturating_add(1)
            );
        }
        // The answer is still right, so the retention is not bought by
        // dropping what it was offered.
        assert!(
            accumulator.finish().is_ok(),
            "{aggregate:?} could not answer"
        );
    }
}

/// The other half, which is what stops the classification being an escape.
///
/// A fold could be listed as whole-group and quietly hold nothing, and the
/// test above would pass while the memory ceiling refused reads for a cost
/// they no longer pay. So a whole-group fold is required to actually hold
/// its group: the label has to be *earned* in both directions.
#[test]
fn a_whole_group_fold_holds_exactly_what_it_was_offered() {
    const OFFERED: i64 = 1_000;
    for aggregate in EVERY
        .iter()
        .filter(|fold| fold.retention() == Retention::WholeGroup)
    {
        let mut accumulator = Accumulator::for_aggregate(*aggregate, span());
        for held in 0..OFFERED {
            // A counter fold is offered each value with its instant.
            let offered = if aggregate.takes_an_instant() {
                Value::Array(vec![
                    integer(held),
                    Value::Datetime(tessari_types::Datetime::from_seconds(held)),
                ])
            } else {
                integer(held)
            };
            accumulator.offer(&offered).unwrap();
        }
        assert_eq!(
            accumulator.held(),
            usize::try_from(OFFERED).unwrap(),
            "{aggregate:?} is classified as holding its group and does not"
        );
        assert!(
            accumulator.finish().is_ok(),
            "{aggregate:?} could not answer"
        );
    }
}

/// Every fold is exercised by the corpus tests, which `EVERY` is the list for.
///
/// `EVERY` is written out rather than aliased to `Aggregate::ALL` so that a
/// reader sees the set; this keeps the two from drifting, which is the only
/// way a new fold could reach the store untested by any of the above.
#[test]
fn the_corpus_exercises_every_fold_the_language_has() {
    assert_eq!(EVERY, Aggregate::ALL);
}

#[test]
fn a_deferred_failure_is_discarded_when_the_other_total_is_the_answer() {
    // The exact total goes out of range on the second value, and a float
    // arrives after it — so the answer comes from the float total and the
    // exact failure is never read.
    let values = vec![beyond_exact(), beyond_exact(), float(1.0)];
    let running = incrementally(Aggregate::Sum, &values).unwrap();
    assert!(
        matches!(running, Value::Number(Number::Float(_))),
        "a float in the group must make the answer a float, got {running:?}"
    );
    assert_eq!(running, fold(Aggregate::Sum, &values, span()).unwrap());
}

#[test]
fn a_value_of_the_wrong_kind_outranks_a_number_out_of_range() {
    // The out-of-range number comes first and the string second; the batch
    // fold type-checks the whole group before any arithmetic, so it reports
    // the string. Deferring the arithmetic failure is what keeps that true.
    let values = vec![
        beyond_exact(),
        beyond_exact(),
        Value::String("not a number".to_owned()),
    ];
    let batch = fold(Aggregate::Sum, &values, span()).unwrap_err();
    let running = incrementally(Aggregate::Sum, &values).unwrap_err();
    assert_eq!(format!("{batch}"), format!("{running}"));
}
