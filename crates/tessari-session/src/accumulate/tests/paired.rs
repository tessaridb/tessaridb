use super::*;

/// Offer `[x, y]` pairs the way the parser packs a two-argument fold.
fn paired(aggregate: Aggregate, held: &[(f64, f64)]) -> crate::error::Result<Value> {
    let mut accumulator = Accumulator::for_aggregate(aggregate, span());
    for (x, y) in held {
        accumulator.offer(&Value::Array(vec![float(*x), float(*y)]))?;
    }
    accumulator.finish()
}

/// The float a paired fold answered, for a comparison with a tolerance.
fn answered_float(answered: crate::error::Result<Value>, what: &str) -> f64 {
    let answered = answered.unwrap_or_else(|e| panic!("{what} refused: {e}"));
    let Value::Number(number) = &answered else {
        panic!("{what} answered {answered:?}, not a number");
    };
    crate::aggregate::approximate(number)
        .unwrap_or_else(|| panic!("{what} answered {answered:?}, not a float"))
}

const PAIRED: &[Aggregate] = &[
    Aggregate::Corr,
    Aggregate::Covar,
    Aggregate::RegrSlope,
    Aggregate::RegrIntercept,
    Aggregate::RegrR2,
];

/// x = [1,2,3,4,5] against y = [2,1,4,3,5]: n = 5, Σx = Σy = 15, Σxy = 53,
/// Σx² = Σy² = 55 — so the sample covariance is (53 − 45)/4 = 2, the
/// correlation and the slope are 40/50 = 0.8, the intercept is 0.6 and r²
/// is 0.64. The plausible wrong answers — the population divisor (8/5), an
/// uncentered ratio, the slope with x and y swapped — all land outside the
/// tolerance.
#[test]
fn the_paired_folds_answer_the_hand_checked_values() {
    let held = [(1.0, 2.0), (2.0, 1.0), (3.0, 4.0), (4.0, 3.0), (5.0, 5.0)];
    let close = |aggregate: Aggregate, wanted: f64| {
        let got = answered_float(paired(aggregate, &held), aggregate.spelling());
        assert!(
            (got - wanted).abs() < 1e-9,
            "{} answered {got}, not {wanted}",
            aggregate.spelling()
        );
    };
    close(Aggregate::Corr, 0.8);
    close(Aggregate::Covar, 2.0);
    close(Aggregate::RegrSlope, 0.8);
    close(Aggregate::RegrIntercept, 0.6);
    close(Aggregate::RegrR2, 0.64);
}

/// A perfect line answers exactly: correlation 1, r² 1, and the slope and
/// intercept of the line itself.
#[test]
fn a_perfect_line_answers_its_own_numbers() {
    let held = [(1.0, 3.0), (2.0, 5.0), (3.0, 7.0), (4.0, 9.0)];
    assert_eq!(answered_float(paired(Aggregate::Corr, &held), "corr"), 1.0);
    assert_eq!(answered_float(paired(Aggregate::RegrR2, &held), "r2"), 1.0);
    assert_eq!(
        answered_float(paired(Aggregate::RegrSlope, &held), "slope"),
        2.0
    );
    assert_eq!(
        answered_float(paired(Aggregate::RegrIntercept, &held), "intercept"),
        1.0
    );
}

/// Fewer than two pairs names no correlation, no covariance and no line —
/// `NONE`, by `variance`'s rule over one number: the answer is not zero, it
/// is unasked.
#[test]
fn a_pair_short_of_two_answers_none() {
    for aggregate in PAIRED {
        assert_eq!(
            paired(*aggregate, &[(1.0, 2.0)]).unwrap(),
            Value::None,
            "{aggregate:?} over one pair"
        );
        assert_eq!(
            paired(*aggregate, &[]).unwrap(),
            Value::None,
            "{aggregate:?} over nothing"
        );
    }
}

/// An x that never moves names no slope and no correlation — both
/// denominators are zero, and zero would be a claim the data does not make.
/// The covariance still exists there, and it is zero.
#[test]
fn a_degenerate_x_names_no_line() {
    let held = [(2.0, 1.0), (2.0, 5.0), (2.0, 9.0)];
    for aggregate in [
        Aggregate::Corr,
        Aggregate::RegrSlope,
        Aggregate::RegrIntercept,
        Aggregate::RegrR2,
    ] {
        assert_eq!(
            paired(aggregate, &held).unwrap(),
            Value::None,
            "{aggregate:?} over a constant x"
        );
    }
    assert_eq!(paired(Aggregate::Covar, &held).unwrap(), float(0.0));
}

/// An absent side skips the pair — the record is not a pair — and a side of
/// the wrong kind is refused naming the fold, by `increase`'s rule for a
/// sample that is not a number.
#[test]
fn an_absent_side_skips_and_a_wrong_kind_refuses() {
    let mut accumulator = Accumulator::for_aggregate(Aggregate::Corr, span());
    accumulator
        .offer(&Value::Array(vec![Value::None, float(1.0)]))
        .unwrap();
    accumulator
        .offer(&Value::Array(vec![float(1.0), float(2.0)]))
        .unwrap();
    accumulator
        .offer(&Value::Array(vec![float(2.0), float(4.0)]))
        .unwrap();
    assert_eq!(answered_float(accumulator.finish(), "corr"), 1.0);

    let mut accumulator = Accumulator::for_aggregate(Aggregate::Corr, span());
    let refused = accumulator
        .offer(&Value::Array(vec![
            Value::String("x".to_owned()),
            float(1.0),
        ]))
        .unwrap_err();
    assert!(format!("{refused}").contains("corr"), "{refused}");
}
