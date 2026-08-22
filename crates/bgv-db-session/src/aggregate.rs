use std::collections::BTreeMap;

use bgv_db_ql::{Aggregate, Expr, ExprKind, Projected, Span};
use bgv_db_storage::Transaction;
use bgv_db_types::{Number, RecordId, Value};
use rust_decimal::Decimal;

use crate::error::{Error, Result};
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

/// What one fold occurrence saw, one entry per record of its group.
type Seen = Vec<Value>;

/// What every fold saw: by projection, then by occurrence within it.
type Collected = Vec<Vec<Seen>>;

/// One group: the identity its answer carries, and what its folds saw.
type Group = (RecordId, Collected);

impl Session<'_> {
    /// The records folded into one answer per group.
    ///
    /// Groups are held in memory and keyed by the group's values, which is what
    /// makes the order of the groups the value system's order too — a
    /// `BTreeMap` keyed by the same values `ORDER BY` sorts by. A store that
    /// must aggregate more than fits needs a spill, and that is a measurement
    /// away rather than a guess away.
    ///
    /// A read with folds and no `GROUP BY` has exactly one group, because
    /// `SELECT count(*) FROM users` should not need a clause that means nothing.
    ///
    /// # Two passes, because a fold answers after the records are gone
    ///
    /// The first pass is per record and collects, for every fold **occurrence**
    /// in every projected expression, what that fold saw in that record. The
    /// second is per group: each occurrence is folded into one value, those
    /// values are substituted into the expression, and the ordinary evaluator
    /// runs over what is left. That is the whole of what makes `mean(age) * 2`
    /// work — the composition is evaluated by the same code that evaluates
    /// `age * 2`, over a literal.
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
        // The collected values are indexed by projection, then by fold
        // occurrence within it, then by record.
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
            let entry = groups.entry(key).or_insert_with(|| {
                (
                    id.clone(),
                    occurrences
                        .iter()
                        .map(|held| vec![Vec::new(); held.len()])
                        .collect(),
                )
            });
            for (position, held) in occurrences.iter().enumerate() {
                for (which, fold) in held.iter().enumerate() {
                    let ExprKind::Fold { over, .. } = &fold.kind else {
                        continue;
                    };
                    let value = match over {
                        // `count(*)` folds over the records themselves, so what
                        // it collects is one placeholder per record rather than
                        // a value read out of one.
                        None => Value::Bool(true),
                        Some(expr) => self.evaluate_in(transaction, expr, Scope::of(&record))?,
                    };
                    if let Some(collected) = entry
                        .1
                        .get_mut(position)
                        .and_then(|held| held.get_mut(which))
                    {
                        collected.push(value);
                    }
                }
            }
        }

        let mut answered = Vec::with_capacity(groups.len());
        for (key, (id, collected)) in groups {
            let mut fields = BTreeMap::new();
            for (position, value) in wanted.iter().enumerate() {
                let held = occurrences.get(position).map_or(&[][..], Vec::as_slice);
                let mut computed = Vec::with_capacity(held.len());
                for (which, fold) in held.iter().enumerate() {
                    let ExprKind::Fold {
                        fold: aggregate,
                        span,
                        ..
                    } = &fold.kind
                    else {
                        continue;
                    };
                    let seen = collected
                        .get(position)
                        .and_then(|held| held.get(which))
                        .map_or(&[][..], Vec::as_slice);
                    computed.push(self::fold(*aggregate, seen, *span)?);
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
pub(crate) fn fold(aggregate: Aggregate, values: &[Value], span: Span) -> Result<Value> {
    match aggregate {
        Aggregate::Count => count(values),
        Aggregate::Sum => sum(values, span),
        Aggregate::Mean => mean(values, span),
        Aggregate::Min => Ok(extreme(values, true)),
        Aggregate::Max => Ok(extreme(values, false)),
    }
}

/// How many of these are values at all.
fn count(values: &[Value]) -> Result<Value> {
    let held = values.iter().filter(|value| present(value)).count();
    let held = i64::try_from(held).unwrap_or(i64::MAX);
    Ok(Value::Number(Number::Integer(held)))
}

/// The numbers a fold is being given, refusing anything that is not one.
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
fn approximate(number: &Number) -> Option<f64> {
    match number {
        Number::Float(held) => Some(*held),
        other => other
            .as_decimal()
            .and_then(|exact| f64::try_from(exact).ok()),
    }
}

/// The average of what is there, or `NONE` when nothing is.
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
fn present(value: &Value) -> bool {
    value.is_present() && *value != Value::Null
}
