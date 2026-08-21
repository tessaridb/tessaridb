//! Folding many records into one answer.
//!
//! Everything else in the read language answers one row per record — a filter
//! narrows, a projection reshapes, an order arranges. A fold does not, and that
//! difference in **arity** is the whole of why aggregates are their own thing
//! rather than more functions.
//!
//! # What each fold does with the rows that hold nothing
//!
//! The interesting half of an aggregate:
//!
//! - `count(*)` counts records. `count(<expr>)` counts the records where the
//!   expression is present and not null — SQL's rule, and what makes
//!   `count(*)` and `count(email)` two questions worth having both of.
//! - `sum` and `mean` ignore absent and null. **Sum over nothing is zero**, so
//!   no caller has to write the same `?? 0`; **mean over nothing is `NONE`**,
//!   because an average of no numbers is not a number.
//! - `min` and `max` ignore absent and null and use the value system's order —
//!   the same one `ORDER BY` uses, so the minimum of a group is the first row an
//!   order over it would give.
//! - A non-number reaching `sum` or `mean` **fails**, naming the type. That is
//!   the rule arithmetic already follows, and a silent skip would make a wrong
//!   total look like a right one.

use std::collections::BTreeMap;

use bgv_db_ql::{Aggregate, FieldPath, Projectable, Projected, Span};
use bgv_db_storage::Transaction;
use bgv_db_types::{Number, RecordId, Value};
use rust_decimal::Decimal;

use crate::error::{Error, Result};
use crate::session::Session;

/// Whether a projection folds many records into one.
pub(crate) fn folds(wanted: &[Projected]) -> bool {
    wanted
        .iter()
        .any(|value| matches!(value.value, Projectable::Aggregate { .. }))
}

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
    pub(crate) fn grouped(
        &self,
        transaction: &mut Transaction<'_>,
        records: Vec<(RecordId, Value)>,
        wanted: &[Projected],
        group: &[FieldPath],
    ) -> Result<Vec<(RecordId, Value)>> {
        // Keyed by the group's values so the groups come out in the value
        // system's order; the identity of the first record in each group becomes
        // the group's, so an answer still has one.
        let mut groups: BTreeMap<Vec<Value>, (RecordId, Vec<Vec<Value>>)> = BTreeMap::new();
        for (id, record) in records {
            let key: Vec<Value> = group
                .iter()
                .map(|path| path.path.resolve(&record).cloned().unwrap_or(Value::None))
                .collect();
            let entry = groups
                .entry(key)
                .or_insert_with(|| (id.clone(), vec![Vec::new(); wanted.len()]));
            for (position, value) in wanted.iter().enumerate() {
                let held = match &value.value {
                    // `count(*)` folds over the records themselves, so what it
                    // collects is one placeholder per record rather than a value
                    // read out of one.
                    Projectable::Aggregate { over: None, .. } => Value::Bool(true),
                    Projectable::Aggregate {
                        over: Some(expr), ..
                    } => self.evaluate_in(transaction, expr, Some(&record))?,
                    Projectable::Value(expr) => {
                        self.evaluate_in(transaction, expr, Some(&record))?
                    }
                };
                if let Some(collected) = entry.1.get_mut(position) {
                    collected.push(held);
                }
            }
        }

        let mut answered = Vec::with_capacity(groups.len());
        for (_, (id, collected)) in groups {
            let mut fields = BTreeMap::new();
            for (position, value) in wanted.iter().enumerate() {
                let held = collected.get(position).map_or(&[][..], Vec::as_slice);
                let answer = match &value.value {
                    Projectable::Aggregate {
                        fold: aggregate,
                        span,
                        ..
                    } => fold(*aggregate, held, *span)?,
                    // A group key has one value per group by construction, so
                    // the first is the only.
                    Projectable::Value(_) => held.first().cloned().unwrap_or(Value::None),
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
