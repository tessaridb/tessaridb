//! Reads combined by `UNION [ALL]`, `INTERSECT` and `EXCEPT` (ADR-0134 D6).

use std::collections::BTreeSet;

use tessari_ql::{Select, SetOperation};
use tessari_storage::Transaction;
use tessari_types::{RecordId, Value};

use crate::budget::{Ceiling, Deadline};
use crate::error::Result;
use crate::session::Session;

use super::Answered;

impl Session<'_> {
    /// A read, and every read it combines with, left to right.
    ///
    /// Each operand is answered exactly as it would be alone — its own source,
    /// clauses, notes and grants — and then combined. Two records are the same
    /// when their values are equal by the store's `=`, which is why the sets are
    /// ordered by value: a hash would put `3` and `3.0` apart. The answer
    /// carries the first read's plan.
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
        Ok(answered)
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
