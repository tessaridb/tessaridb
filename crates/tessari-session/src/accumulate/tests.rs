use super::{Accumulator, Aggregate, Decimal, Number, Retention, Span, Value};
use crate::aggregate::fold;

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

#[test]
fn folding_one_value_at_a_time_answers_what_folding_them_all_at_once_answers() {
    for aggregate in EVERY {
        for (what, values) in corpus() {
            let batch = fold(*aggregate, &values, span());
            let running = incrementally(*aggregate, &values);
            if matches!(aggregate, Aggregate::Variance | Aggregate::Stddev)
                && let (Ok(batch), Ok(running)) = (&batch, &running)
            {
                assert!(
                    same_spread(batch, running),
                    "{aggregate:?} over {what} answered {batch:?} in one pass and \
                         {running:?} incrementally"
                );
                continue;
            }
            match (batch, running) {
                // Compared **structurally**, not by value equality. This
                // store's `Value` deliberately orders and equates numbers
                // across kinds — `3`, `3.0` and the decimal `3.0` are all
                // equal, which is what lets a join match what `=` matches.
                // So `assert_eq!` on the values would pass a `sum` that
                // stopped promoting to an integer, and pass an extreme that
                // kept the wrong end of a tie between two kinds. Both are
                // differences a caller sees on the wire, and both are
                // invisible to the comparison this test would otherwise
                // make.
                (Ok(batch), Ok(running)) => assert_eq!(
                    format!("{batch:?}"),
                    format!("{running:?}"),
                    "{aggregate:?} over {what} answered differently"
                ),
                (Err(batch), Err(running)) => assert_eq!(
                    format!("{batch}"),
                    format!("{running}"),
                    "{aggregate:?} over {what} failed differently"
                ),
                (batch, running) => panic!(
                    "{aggregate:?} over {what}: one answered and the other did not — \
                         {batch:?} against {running:?}"
                ),
            }
        }
    }
}

/// The spread folds against numbers whose answer is known independently.
///
/// The equivalence test above compares two implementations to each other,
/// which cannot catch a mistake they share — a population divisor in both
/// would pass it. These are hand-checked: `[2, 4, 4, 4, 5, 5, 7, 9]` has a
/// sample variance of `32/7` and a population variance of `4`, and its
/// population standard deviation is exactly `2`, so the three plausible
/// wrong answers are all far outside the tolerance.
#[test]
fn the_spread_is_the_sample_form_and_not_the_population_one() {
    let values: Vec<Value> = [2, 4, 4, 4, 5, 5, 7, 9]
        .iter()
        .map(|n| integer(*n))
        .collect();

    let Ok(Value::Number(Number::Float(variance))) = incrementally(Aggregate::Variance, &values)
    else {
        panic!("variance did not answer a float")
    };
    assert!(
        (variance - 32.0 / 7.0).abs() < 1e-9,
        "variance answered {variance}, which is the population form (4.0) if it is 4"
    );

    let Ok(Value::Number(Number::Float(deviation))) = incrementally(Aggregate::Stddev, &values)
    else {
        panic!("stddev did not answer a float")
    };
    assert!(
        (deviation - (32.0_f64 / 7.0).sqrt()).abs() < 1e-9,
        "stddev answered {deviation}; the population form is exactly 2 here"
    );
}

/// The exact totals earn their place against the float form.
///
/// `E[x²] − E[x]²` in floats over these three values subtracts two numbers
/// that agree to fifteen significant digits and answers `0` — or a negative
/// number, whose square root is not a number at all. The same subtraction on
/// exact totals loses nothing (ADR-0114 D4), so the answer is the true one,
/// to the bit.
#[test]
fn the_spread_survives_numbers_the_textbook_form_cancels_away() {
    let values = vec![float(1e9 + 4.0), float(1e9 + 7.0), float(1e9 + 13.0)];
    let Ok(Value::Number(Number::Float(variance))) = incrementally(Aggregate::Variance, &values)
    else {
        panic!("variance did not answer a float")
    };
    assert_eq!(
        variance.to_bits(),
        21.0_f64.to_bits(),
        "variance answered {variance}; the deviations are -4, -1 and 5 about a \
             mean of 1e9+8, so the sample variance is 42/2 = 21"
    );
}

/// ADR-0114 — a float fold answers the same bits whatever order its values
/// arrive in and however its group is split and merged, which is what lets
/// it travel to the shards.
#[test]
fn a_float_fold_answers_the_same_bits_in_any_order_and_any_split() {
    let values: Vec<Value> = [0.1, 1e16, 0.2, -1e16, 0.3, 2.5e-7, 123_456.789, -0.05]
        .iter()
        .map(|held| float(*held))
        .chain([integer(7), decimal("0.25")])
        .collect();
    let bits = |answer: &Value| match answer {
        Value::Number(Number::Float(held)) => held.to_bits(),
        other => panic!("not a float: {other:?}"),
    };
    for aggregate in [
        Aggregate::Sum,
        Aggregate::Mean,
        Aggregate::Variance,
        Aggregate::Stddev,
    ] {
        let walked = incrementally(aggregate, &values).unwrap();
        let mut reversed = values.clone();
        reversed.reverse();
        assert_eq!(
            bits(&walked),
            bits(&incrementally(aggregate, &reversed).unwrap()),
            "{aggregate:?} reversed"
        );
        for cut in 0..=values.len() {
            let (left, right) = values.split_at(cut);
            let mut first = Accumulator::for_aggregate(aggregate, span());
            left.iter().for_each(|value| first.offer(value).unwrap());
            let mut second = Accumulator::for_aggregate(aggregate, span());
            right.iter().for_each(|value| second.offer(value).unwrap());
            // The second part travels as its state and is merged first,
            // so the merge also reverses the parts' order.
            let mut merged = Accumulator::for_aggregate(aggregate, span());
            assert!(merged.merge(&second.state().unwrap()).unwrap());
            assert!(merged.merge(&first.state().unwrap()).unwrap());
            assert_eq!(
                bits(&walked),
                bits(&merged.finish().unwrap()),
                "{aggregate:?} split at {cut}"
            );
        }
    }
    // The float total is the exact sum rounded once: 0.1 + 0.2 + 0.3 in
    // floats is 0.6000000000000001 added in order, and 0.6 exactly summed.
    let answer = incrementally(Aggregate::Sum, &[float(0.1), float(0.2), float(0.3)]).unwrap();
    assert_eq!(bits(&answer), 0.6_f64.to_bits());
}

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
            accumulator.offer(&integer(held)).unwrap();
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
