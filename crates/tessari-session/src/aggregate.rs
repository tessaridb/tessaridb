use std::collections::BTreeMap;

use tessari_ql::{Expr, ExprKind, Projected, Retention};
use tessari_storage::Transaction;
use tessari_types::{Number, RecordId, Value};

// The reference implementation this module keeps for the accumulator to be
// tested against needs a little more vocabulary than the executor does.
#[cfg(test)]
use rust_decimal::Decimal;
#[cfg(test)]
use tessari_ql::{Aggregate, Span};

#[cfg(test)]
use crate::error::Error;

use crate::accumulate::Accumulator;
use crate::error::Result;
use crate::evaluate::Scope;
use crate::session::Session;

/// Whether a projection folds many records into one.
pub(crate) fn folds(wanted: &[Projected]) -> bool {
    wanted.iter().any(|value| holds_a_fold(&value.value))
}

/// Whether this expression holds a fold anywhere inside it.
fn holds_a_fold(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Fold { .. }) || children(expr).into_iter().any(holds_a_fold)
}

/// The first fold in this projection that holds its whole group, if any.
///
/// Read by the memory ceiling, which exempts a folding read on the stated
/// grounds that its answer does not grow with the table. That is a claim about
/// the fold and not about folding, and two folds falsify it — so the exemption
/// asks this rather than assuming it (Q-227). The spelling comes back so the
/// refusal can say which fold it is about.
pub(crate) fn retains_its_group(wanted: &[Projected]) -> Option<&'static str> {
    wanted.iter().find_map(|value| growing_fold(&value.value))
}

/// The first whole-group fold anywhere inside this expression.
fn growing_fold(expr: &Expr) -> Option<&'static str> {
    if let ExprKind::Fold { fold, .. } = expr.kind
        && fold.retention() == Retention::WholeGroup
    {
        return Some(fold.spelling());
    }
    children(expr).into_iter().find_map(growing_fold)
}

/// The expressions one expression is built out of.
fn children(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::Fold { over, .. } => over.as_deref().into_iter().collect(),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => vec![inner],
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => vec![left, right],
        ExprKind::Call { arguments, .. } => arguments.iter().collect(),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().collect(),
        ExprKind::Object(fields) => fields.iter().map(|field| &field.value).collect(),
        ExprKind::Range(range) => vec![&range.start, &range.end],
        _ => Vec::new(),
    }
}

/// Every fold in this expression, in the order a walk meets them.
///
/// The **order is the identity**: what a fold collected and what it computed are
/// matched up by position, so the collecting walk and the substituting walk have
/// to meet the folds the same way. They do because both use this function, which
/// is why it exists rather than each walking the tree in its own words.
fn folds_in<'a>(expr: &'a Expr, found: &mut Vec<&'a Expr>) {
    if matches!(expr.kind, ExprKind::Fold { .. }) {
        found.push(expr);
    }
    for child in children(expr) {
        folds_in(child, found);
    }
}

/// The same expression with each fold replaced by the value it produced.
///
/// A fold's value is **constant within its group**, and this makes that
/// literally true rather than a claim about the implementation: what the
/// ordinary evaluator then meets is arithmetic over a literal, and it needs to
/// know nothing about folding at all.
///
/// `taken` is consumed in walk order, which is the order [`folds_in`] produced
/// the collections in.
fn substituted(expr: &Expr, taken: &mut std::vec::IntoIter<Value>) -> Expr {
    if matches!(expr.kind, ExprKind::Fold { .. }) {
        return Expr {
            kind: ExprKind::Literal(taken.next().unwrap_or(Value::None)),
            span: expr.span,
        };
    }
    let kind = match &expr.kind {
        ExprKind::Not(inner) => ExprKind::Not(Box::new(substituted(inner, taken))),
        ExprKind::Negate(inner) => ExprKind::Negate(Box::new(substituted(inner, taken))),
        ExprKind::And(left, right) => ExprKind::And(
            Box::new(substituted(left, taken)),
            Box::new(substituted(right, taken)),
        ),
        ExprKind::Or(left, right) => ExprKind::Or(
            Box::new(substituted(left, taken)),
            Box::new(substituted(right, taken)),
        ),
        ExprKind::Arithmetic { op, left, right } => ExprKind::Arithmetic {
            op: *op,
            left: Box::new(substituted(left, taken)),
            right: Box::new(substituted(right, taken)),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: *op,
            left: Box::new(substituted(left, taken)),
            right: Box::new(substituted(right, taken)),
        },
        ExprKind::Call {
            function,
            arguments,
            span,
        } => ExprKind::Call {
            function: *function,
            arguments: arguments
                .iter()
                .map(|argument| substituted(argument, taken))
                .collect(),
            span: *span,
        },
        ExprKind::Array(items) => {
            ExprKind::Array(items.iter().map(|item| substituted(item, taken)).collect())
        }
        ExprKind::Set(items) => {
            ExprKind::Set(items.iter().map(|item| substituted(item, taken)).collect())
        }
        // Nothing else can hold a fold: an object's values, a range's ends and a
        // nested read are all refused a fold by the grouping rule, so leaving
        // them as written is what they are.
        other => other.clone(),
    };
    Expr {
        kind,
        span: expr.span,
    }
}

/// What every fold of a group holds: by projection, then by occurrence within it.
///
/// There is no third dimension, and that is the point. Until wave 39 the
/// innermost entry was one value **per record**, so a group cost what its group
/// cost rather than what its answer did; an accumulator has nowhere to put one.
type Holding = Vec<Vec<Accumulator>>;

/// One group: the identity its answer carries, and what its folds hold.
type Group = (RecordId, Holding);

/// An accumulator per fold occurrence, positions preserved.
fn holding(occurrences: &[Vec<&Expr>]) -> Holding {
    occurrences
        .iter()
        .map(|held| {
            held.iter()
                .map(|fold| Accumulator::of(&fold.kind))
                .collect()
        })
        .collect()
}

impl Session<'_> {
    /// The records folded into one answer per group.
    ///
    /// Groups are held in memory and keyed by the group's values, which is what
    /// makes the order of the groups the value system's order too — a
    /// `BTreeMap` keyed by the same values `ORDER BY` sorts by.
    ///
    /// **What a group holds is set by its folds and not by its records.** Each
    /// fold occurrence keeps one [`Accumulator`] rather than one value per
    /// record, so grouping fifty thousand records into three costs three
    /// answers' worth and not fifty thousand. Every fold this store has is
    /// computable one value at a time, so the case a spill was reserved for does
    /// not arise for them; a fold that is not would bring it back.
    ///
    /// A read with folds and no `GROUP BY` has exactly one group, because
    /// `SELECT count(*) FROM users` should not need a clause that means nothing.
    ///
    /// # Two passes, because a fold answers after the records are gone
    ///
    /// The first pass is per record and offers, to every fold **occurrence** in
    /// every projected expression, what that fold saw in that record. The second
    /// is per group: each occurrence answers with one value, those values are
    /// substituted into the expression, and the ordinary evaluator runs over
    /// what is left. That is the whole of what makes `mean(age) * 2` work — the
    /// composition is evaluated by the same code that evaluates `age * 2`, over
    /// a literal.
    pub(crate) fn grouped(
        &self,
        transaction: &mut Transaction<'_>,
        records: Vec<(RecordId, Value)>,
        wanted: &[Projected],
        group: &[Expr],
    ) -> Result<Vec<(RecordId, Value)>> {
        // Which folds each projection holds, resolved once rather than per
        // record — the tree does not change under a read.
        let occurrences: Vec<Vec<&Expr>> = wanted
            .iter()
            .map(|value| {
                let mut found = Vec::new();
                folds_in(&value.value, &mut found);
                found
            })
            .collect();

        // Keyed by the group's values so the groups come out in the value
        // system's order; the identity of the first record in each group becomes
        // the group's, so an answer still has one.
        //
        // The accumulators are indexed by projection, then by fold occurrence
        // within it — and there it stops.
        let mut groups: BTreeMap<Vec<Value>, Group> = BTreeMap::new();
        for (id, record) in records {
            // Evaluated rather than resolved, so a window — `time::bucket(at,
            // 1h)` — is a key like any other. A bare name still reads as a route
            // into the record, so `GROUP BY city` costs the same walk it always
            // did through one more layer.
            let mut key = Vec::with_capacity(group.len());
            for held in group {
                key.push(self.evaluate_in(transaction, held, Scope::of(&record))?);
            }
            let entry = groups
                .entry(key)
                .or_insert_with(|| (id.clone(), holding(&occurrences)));
            for (position, held) in occurrences.iter().enumerate() {
                for (which, fold) in held.iter().enumerate() {
                    let ExprKind::Fold { over, .. } = &fold.kind else {
                        continue;
                    };
                    let value = match over {
                        // `count(*)` folds over the records themselves, so what
                        // it is offered is one placeholder per record rather
                        // than a value read out of one.
                        None => Value::Bool(true),
                        Some(expr) => self.evaluate_in(transaction, expr, Scope::of(&record))?,
                    };
                    if let Some(accumulator) = entry
                        .1
                        .get_mut(position)
                        .and_then(|held| held.get_mut(which))
                    {
                        accumulator.offer(&value)?;
                    }
                }
            }
        }

        let mut answered = Vec::with_capacity(groups.len());
        for (key, (id, accumulated)) in groups {
            let mut fields = BTreeMap::new();
            for (position, value) in wanted.iter().enumerate() {
                let held = occurrences.get(position).map_or(&[][..], Vec::as_slice);
                let mut computed = Vec::with_capacity(held.len());
                for (which, fold) in held.iter().enumerate() {
                    let ExprKind::Fold { .. } = &fold.kind else {
                        continue;
                    };
                    let Some(accumulator) =
                        accumulated.get(position).and_then(|held| held.get(which))
                    else {
                        continue;
                    };
                    computed.push(accumulator.finish()?);
                }
                let answer = if computed.is_empty() {
                    // No fold in this projection, so it is a group key — and a
                    // key has one value per group by construction. Evaluated
                    // against nothing, because a key's value is the key.
                    key_value(&key, group, &value.value)
                } else {
                    let mut taken = computed.into_iter();
                    let substituted = substituted(&value.value, &mut taken);
                    self.evaluate_in(transaction, &substituted, Scope::none())?
                };
                if answer.is_present() {
                    fields.insert(value.name.text.clone(), answer);
                }
            }
            answered.push((id, Value::Object(fields)));
        }
        Ok(answered)
    }
}

/// The value a projected group key holds for this group.
///
/// The key was evaluated once per record to build the group; every record of the
/// group produced the same value, which is what grouping by it means. So the
/// answer is read back out of the group's own key rather than evaluated again —
/// one place the value comes from, and no chance of the two disagreeing.
fn key_value(key: &[Value], group: &[Expr], expr: &Expr) -> Value {
    group
        .iter()
        .position(|held| held.same_shape(expr))
        .and_then(|at| key.get(at))
        .cloned()
        .unwrap_or(Value::None)
}

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
fn spread(values: &[Value], rooted: bool, span: Span) -> Result<Value> {
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
fn median(values: &[Value], span: Span) -> Result<Value> {
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
fn collect(values: &[Value]) -> Value {
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
fn count(values: &[Value]) -> Result<Value> {
    let held = values.iter().filter(|value| present(value)).count();
    let held = i64::try_from(held).unwrap_or(i64::MAX);
    Ok(Value::Number(Number::Integer(held)))
}

/// The numbers a fold is being given, refusing anything that is not one.
#[cfg(test)]
fn numbers(values: &[Value], fold: &'static str, span: Span) -> Result<Vec<Number>> {
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
fn sum(values: &[Value], span: Span) -> Result<Value> {
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
        let mut total = 0.0_f64;
        for number in &numbers {
            total += approximate(number).ok_or_else(|| failed("a number no float can hold"))?;
        }
        return Ok(Value::Number(Number::float(total)));
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
    {
        if let Ok(whole) = i64::try_from(total) {
            return Ok(Value::Number(Number::Integer(whole)));
        }
    }
    Ok(Value::Number(Number::Decimal(total)))
}

/// A number as a float, for the branch that has already decided to be one.
pub(crate) fn approximate(number: &Number) -> Option<f64> {
    match number {
        Number::Float(held) => Some(*held),
        other => other
            .as_decimal()
            .and_then(|exact| f64::try_from(exact).ok()),
    }
}

/// The average of what is there, or `NONE` when nothing is.
#[cfg(test)]
fn mean(values: &[Value], span: Span) -> Result<Value> {
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
fn extreme(values: &[Value], smallest: bool) -> Value {
    let mut extreme: Option<&Value> = None;
    for value in values.iter().filter(|value| present(value)) {
        let replaces = extreme.is_none_or(|held| (value < held) == smallest);
        if replaces {
            extreme = Some(value);
        }
    }
    extreme.cloned().unwrap_or(Value::None)
}

/// Whether a fold should see this at all.
///
/// Absent and null are both "no value here", and every fold but `count(*)`
/// passes over them. `count(*)` never reaches this, because it folds over the
/// records rather than over a value in them.
pub(crate) fn present(value: &Value) -> bool {
    value.is_present() && *value != Value::Null
}
