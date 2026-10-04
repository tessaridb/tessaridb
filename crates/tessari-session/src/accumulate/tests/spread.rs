use super::*;

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
