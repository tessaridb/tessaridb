use super::*;

/// Fold the values one aggregate saw across a group.
///
/// `values` is what the folded expression produced for each record of the
/// group — for `count(*)` it is one entry per record, holding nothing in
/// particular.
///
/// **The reference implementation.** Since wave 39 the executor folds one value
/// at a time ([`crate::accumulate::Accumulator`]) and this is what that is
/// tested against, over a corpus of mixed kinds. It is kept rather than deleted
/// for exactly that reason: an incremental total that agrees with itself proves
/// nothing about the implementation it replaced.
#[cfg(test)]
pub(crate) fn fold(aggregate: Aggregate, values: &[Value], span: Span) -> Result<Value> {
    match aggregate {
        Aggregate::Count => count(values),
        Aggregate::Sum => sum(values, span),
        Aggregate::Mean => mean(values, span),
        Aggregate::Min => Ok(extreme(values, true)),
        Aggregate::Max => Ok(extreme(values, false)),
        Aggregate::Variance => spread(values, false, span),
        Aggregate::Stddev => spread(values, true, span),
        Aggregate::Median => median(values, span),
        Aggregate::Collect => Ok(collect(values)),
        // No batch twin: the counter folds are checked against a hand oracle
        // in the suite (`counters::`), and the sketches against the exact
        // answer within their bound (`approximate_folds::`), rather than
        // against a second copy.
        Aggregate::Increase
        | Aggregate::Rate
        | Aggregate::Delta
        | Aggregate::ApproxDistinct
        | Aggregate::ApproxQuantile => {
            let mut running = crate::accumulate::Accumulator::for_aggregate(aggregate, span);
            for value in values {
                running.offer(value)?;
            }
            running.finish()
        }
    }
}

/// The sample spread, in two passes over the values.
///
/// **Deliberately not Welford.** The accumulator computes this incrementally,
/// and an oracle that used the same recurrence would only prove the
/// implementation agrees with itself. Two passes — the mean, then the squared
/// deviations from it — is the definition the recurrence is derived from, and
/// the one the textbook `E[x²] − E[x]²` form is *also* derived from while losing
/// every significant digit on data whose spread is small next to its magnitude.
///
/// Two different float algorithms do not agree bit for bit, which is why the
/// equivalence test compares these two folds within a tolerance and the exact
/// folds structurally.
#[cfg(test)]
pub(super) fn spread(values: &[Value], rooted: bool, span: Span) -> Result<Value> {
    let fold = if rooted { "stddev" } else { "variance" };
    let numbers = numbers(values, fold, span)?;
    if numbers.len() < 2 {
        // The spread of one observation is not zero, it is unasked.
        return Ok(Value::None);
    }
    let mut held = Vec::new();
    for number in &numbers {
        held.push(approximate(number).ok_or(Error::NotSummable {
            fold,
            found: "a number no float can hold",
            span,
        })?);
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a count past 2^53 has already made every other number here meaningless"
    )]
    let counted = held.len() as f64;
    let mean = held.iter().sum::<f64>() / counted;
    let m2: f64 = held
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum();
    let variance = m2 / (counted - 1.0);
    Ok(Value::Number(Number::float(if rooted {
        variance.sqrt()
    } else {
        variance
    })))
}

/// The middle number, selected by the **definition** of a rank rather than by
/// sorting.
///
/// The k-th smallest value is the one with at most `k` values below it and more
/// than `k` values at or below it. That holds with duplicates, needs no sort,
/// and shares no line with the accumulator's sort-and-index — which is what
/// makes it worth keeping as an oracle for a fold whose implementation is
/// otherwise too short to be worth checking.
#[cfg(test)]
pub(super) fn median(values: &[Value], span: Span) -> Result<Value> {
    let numbers: Vec<Value> = values
        .iter()
        .filter(|value| present(value))
        .cloned()
        .collect();
    for value in &numbers {
        if !matches!(value, Value::Number(_)) {
            return Err(Error::NotSummable {
                fold: "median",
                found: value.type_name(),
                span,
            });
        }
    }
    let held = numbers.len();
    if held == 0 {
        return Ok(Value::None);
    }
    let ranked = |k: usize| -> Option<&Value> {
        numbers.iter().find(|candidate| {
            let below = numbers.iter().filter(|other| other < candidate).count();
            let upto = numbers.iter().filter(|other| other <= candidate).count();
            below <= k && upto > k
        })
    };
    let failed = |found: &'static str| Error::NotSummable {
        fold: "median",
        found,
        span,
    };
    let decimal = |value: &Value| match value {
        Value::Number(number) => number
            .as_decimal()
            .ok_or_else(|| failed("a number outside the exact range")),
        other => Err(failed(other.type_name())),
    };
    // Exact and normalised, for the reason the accumulator's `middle` gives:
    // "the value as written" is not a function of the data when three equal
    // values are three different answers on the wire.
    if held % 2 == 1 {
        let Some(middle) = ranked(held / 2) else {
            return Ok(Value::None);
        };
        return Ok(Value::Number(Number::Decimal(decimal(middle)?.normalize())));
    }
    let above = held / 2;
    let (Some(lower), Some(upper)) = (ranked(above.saturating_sub(1)), ranked(above)) else {
        return Ok(Value::None);
    };
    let pair = decimal(lower)?
        .checked_add(decimal(upper)?)
        .ok_or_else(|| failed("a total outside the exact range"))?;
    let averaged = pair
        .checked_div(Decimal::from(2))
        .ok_or_else(|| failed("a group of no size"))?;
    Ok(Value::Number(Number::Decimal(averaged.normalize())))
}

/// Every present value, in the order they arrived.
#[cfg(test)]
pub(super) fn collect(values: &[Value]) -> Value {
    Value::Array(
        values
            .iter()
            .filter(|value| present(value))
            .cloned()
            .collect(),
    )
}

/// How many of these are values at all.
#[cfg(test)]
pub(super) fn count(values: &[Value]) -> Result<Value> {
    let held = values.iter().filter(|value| present(value)).count();
    let held = i64::try_from(held).unwrap_or(i64::MAX);
    Ok(Value::Number(Number::Integer(held)))
}

/// The numbers a fold is being given, refusing anything that is not one.
#[cfg(test)]
pub(super) fn numbers(values: &[Value], fold: &'static str, span: Span) -> Result<Vec<Number>> {
    let mut held = Vec::new();
    for value in values.iter().filter(|value| present(value)) {
        let Value::Number(number) = value else {
            return Err(Error::NotSummable {
                fold,
                found: value.type_name(),
                span,
            });
        };
        held.push(number.clone());
    }
    Ok(held)
}

/// The total, in the widest kind the group holds.
///
/// The same promotion arithmetic uses: a group of integers totals to an
/// integer, one holding a decimal totals exactly, and anything touching a float
/// totals to a float and says so.
#[cfg(test)]
pub(super) fn sum(values: &[Value], span: Span) -> Result<Value> {
    let numbers = numbers(values, "sum", span)?;
    let failed = |reason: &'static str| Error::NotSummable {
        fold: "sum",
        found: reason,
        span,
    };

    if numbers
        .iter()
        .any(|number| matches!(number, Number::Float(_)))
    {
        // The exact sum rounded once (ADR-0114), by the fixed-point oracle
        // rather than by the expansion the accumulator holds.
        let mut held = Vec::with_capacity(numbers.len());
        for number in &numbers {
            held.push(approximate(number).ok_or_else(|| failed("a number no float can hold"))?);
        }
        return Ok(Value::Number(Number::float(
            crate::accumulate::exact::oracle::oracle(&held),
        )));
    }

    let mut total = Decimal::ZERO;
    for number in &numbers {
        let exact = number
            .as_decimal()
            .ok_or_else(|| failed("a number outside the exact range"))?;
        total = total
            .checked_add(exact)
            .ok_or_else(|| failed("a total outside the exact range"))?;
    }
    // Over nothing this is zero, deliberately: a sum that answered `NONE` for an
    // empty group would make every caller write the same `?? 0`.
    if numbers
        .iter()
        .all(|number| matches!(number, Number::Integer(_)))
        && let Ok(whole) = i64::try_from(total)
    {
        return Ok(Value::Number(Number::Integer(whole)));
    }
    Ok(Value::Number(Number::Decimal(total)))
}

/// The average of what is there, or `NONE` when nothing is.
#[cfg(test)]
pub(super) fn mean(values: &[Value], span: Span) -> Result<Value> {
    let numbers = numbers(values, "mean", span)?;
    if numbers.is_empty() {
        // An average of no numbers is not a number, and zero would be a claim.
        return Ok(Value::None);
    }
    let failed = |reason: &'static str| Error::NotSummable {
        fold: "mean",
        found: reason,
        span,
    };
    if numbers
        .iter()
        .any(|number| matches!(number, Number::Float(_)))
    {
        let mut held = Vec::with_capacity(numbers.len());
        for number in &numbers {
            held.push(approximate(number).ok_or_else(|| failed("a number no float can hold"))?);
        }
        let total = crate::accumulate::exact::oracle::oracle(&held);
        #[expect(
            clippy::cast_precision_loss,
            clippy::as_conversions,
            reason = "a test-only reference over a corpus of a handful of values"
        )]
        let counted = held.len() as f64;
        return Ok(Value::Number(Number::float(total / counted)));
    }
    let mut total = Decimal::ZERO;
    for number in &numbers {
        let exact = number
            .as_decimal()
            .ok_or_else(|| failed("a number outside the exact range"))?;
        total = total
            .checked_add(exact)
            .ok_or_else(|| failed("a total outside the exact range"))?;
    }
    let count = i64::try_from(numbers.len()).unwrap_or(i64::MAX);
    let count = Decimal::from(count);
    let averaged = total
        .checked_div(count)
        .ok_or_else(|| failed("a group of no size"))?;
    Ok(Value::Number(Number::Decimal(averaged)))
}

/// The smallest or largest value present, in the value system's order.
#[cfg(test)]
pub(super) fn extreme(values: &[Value], smallest: bool) -> Value {
    let mut extreme: Option<&Value> = None;
    for value in values.iter().filter(|value| present(value)) {
        let replaces = extreme.is_none_or(|held| (value < held) == smallest);
        if replaces {
            extreme = Some(value);
        }
    }
    extreme.cloned().unwrap_or(Value::None)
}
