//! Reads combined by `UNION [ALL]`, `INTERSECT` and `EXCEPT` (ADR-0134 D6).

use std::cmp::Ordering;
use std::collections::BTreeSet;

use tessari_ql::{CompoundTail, Nulls, Select, SetOperation};
use tessari_storage::Transaction;
use tessari_types::{Number, RecordId, Value};

use crate::budget::{Ceiling, Deadline};
use crate::error::Result;
use crate::outcome::AccessPath;
use crate::session::Session;

use super::Answered;

impl Session<'_> {
    /// A read, and every read it combines with, left to right.
    ///
    /// Each operand is answered exactly as it would be alone — its own source,
    /// clauses, notes and grants — and then combined. Two records are the same
    /// when their values are equal by the store's `=`, which is why the sets are
    /// ordered by value: a hash would put `3` and `3.0` apart. A statement-level
    /// compound with a tail sorts the collected answer as one set — the
    /// collection is whole, so the answer is exact and reports `via
    /// materialised`; a read inside parentheses answers under the first
    /// branch's plan, unchanged.
    pub(crate) async fn read(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        within: Option<Deadline>,
        holding: Option<Ceiling>,
    ) -> Result<Answered> {
        let mut answered = self.read_one(transaction, select, within, holding).await?;
        for next in &select.combined {
            let other = Box::pin(self.read_one(transaction, &next.read, within, holding)).await?;
            answered.notes.extend(other.notes);
            answered.records = combined(next.operation, next.all, answered.records, other.records);
        }
        if let Some(tail) = &select.compound_tail {
            answered.records = compound_tail(answered.records, tail);
            answered.plan = crate::plan::Plan::new(AccessPath::Materialised);
        }
        Ok(answered)
    }
}

/// The collected answer under the compound's tail: sorted as one set, the
/// bound applied after the sort.
fn compound_tail(
    mut records: Vec<(RecordId, Value)>,
    tail: &CompoundTail,
) -> Vec<(RecordId, Value)> {
    records.sort_by(|left, right| compare_rows(&left.1, &right.1, tail));
    let skipped = usize::try_from(tail.start.unwrap_or(0)).unwrap_or(usize::MAX);
    let records = records.into_iter().skip(skipped);
    match tail.limit {
        Some(bound) => records
            .take(usize::try_from(bound).unwrap_or(usize::MAX))
            .collect(),
        None => records.collect(),
    }
}

/// Two rows under the tail's keys: each key names a cell — an output column
/// by name, or by ordinal in the row's own field order — and cells compare
/// by the value system's one order. An absent cell is lowest by the declared
/// order unless the key said where its absence rides; a descending key
/// reverses the whole comparison.
fn compare_rows(left: &Value, right: &Value, tail: &CompoundTail) -> Ordering {
    for key in &tail.ordering {
        let (held, found) = match &key.key.kind {
            tessari_ql::ExprKind::Literal(Value::Number(Number::Integer(at))) => {
                let at = usize::try_from(at.saturating_sub(1)).unwrap_or(usize::MAX);
                (cell_at(left, at), cell_at(right, at))
            }
            tessari_ql::ExprKind::Path(path) => {
                let name = path.path.root();
                (cell_named(left, name), cell_named(right, name))
            }
            // The parser lifts names and ordinals alone; anything else was
            // refused before the read ran.
            _ => continue,
        };
        let held = match (&held, &found) {
            (None, None) => continue,
            (None, Some(_)) => absence_before(key),
            (Some(_), None) => absence_before(key).reverse(),
            (Some(held), Some(found)) => held.cmp(found),
        };
        let held = if key.descending { held.reverse() } else { held };
        if held != Ordering::Equal {
            return held;
        }
    }
    Ordering::Equal
}

/// An absent cell rides before every present one unless the key said LAST.
fn absence_before(key: &tessari_ql::Ordering) -> Ordering {
    match key.nulls {
        Some(Nulls::Last) => Ordering::Greater,
        Some(Nulls::First) | None => Ordering::Less,
    }
}

/// The row's cell under a name, when the row holds one.
fn cell_named<'a>(row: &'a Value, name: &str) -> Option<&'a Value> {
    match row {
        Value::Object(fields) => fields.get(name),
        _ => None,
    }
}

/// The row's cell at an ordinal (counted from one), in the row's own field
/// order.
fn cell_at(row: &Value, at: usize) -> Option<&Value> {
    // The ordinal addresses the row, not the name.
    match row {
        Value::Object(fields) => fields.values().nth(at),
        _ => None,
    }
}

/// Two answers combined, the first's order kept and the second's appended.
fn combined(
    operation: SetOperation,
    all: bool,
    first: Vec<(RecordId, Value)>,
    second: Vec<(RecordId, Value)>,
) -> Vec<(RecordId, Value)> {
    match operation {
        SetOperation::Union if all => first.into_iter().chain(second).collect(),
        SetOperation::Union => distinct(first.into_iter().chain(second)),
        SetOperation::Intersect | SetOperation::Except => {
            let other: BTreeSet<Value> = second.into_iter().map(|(_, value)| value).collect();
            let wanted = operation == SetOperation::Intersect;
            distinct(
                first
                    .into_iter()
                    .filter(|(_, value)| other.contains(value) == wanted),
            )
        }
    }
}

/// Each value once, at its first place, under its first record's identity.
fn distinct(records: impl Iterator<Item = (RecordId, Value)>) -> Vec<(RecordId, Value)> {
    let mut seen = BTreeSet::new();
    records
        .filter(|(_, value)| seen.insert(value.clone()))
        .collect()
}
