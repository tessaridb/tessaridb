//! A queue that hands out its greatest priority first (G055 C8).
//!
//! # The order
//!
//! Greatest value of the priority field first, in the value system's order —
//! the order `ORDER BY f DESC` and a value index both use, so the two ways of
//! finding the head cannot disagree. Ties go in arrival order, which is
//! identity order. A record without the field comes after every record that
//! has one, as an absent value sorts below every present one.
//!
//! # How the head is found
//!
//! From a value index on the field when one is declared and can be believed —
//! the rules an ordered read keeps: current, visible, and not written by this
//! transaction — asking for more until enough of the records it names are
//! claimable. Otherwise, and when the index runs out, by walking the queue and
//! keeping the best `limit` claimable records. An index changes what the claim
//! costs and never which records it takes.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::ControlFlow;

use tessari_encoding::decode_payload;
use tessari_storage::{QueueDeclaration, Transaction};
use tessari_types::{Datetime, Path, RecordId, Value};

use super::claimable;
use crate::context::Context;
use crate::error::Result;
use crate::session::Session;

/// The head a claim takes, best first, and the index that found it.
type Head = (
    Vec<(RecordId, std::collections::BTreeMap<String, Value>)>,
    Option<String>,
);

/// Where a record stands: its priority (absent lowest), then arrival.
type Rank = (Option<Value>, Reverse<RecordId>);

/// A claimable record and its fields, ranked.
struct Candidate {
    rank: Rank,
    id: RecordId,
    fields: std::collections::BTreeMap<String, Value>,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.rank == other.rank
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank.cmp(&other.rank)
    }
}

/// The record's rank, when it can be claimed now.
fn ranked(
    id: RecordId,
    bytes: &[u8],
    now: Datetime,
    declared: &QueueDeclaration,
    field: &str,
) -> Result<Option<Candidate>> {
    let Value::Object(fields) = decode_payload(bytes)? else {
        return Ok(None);
    };
    if !claimable(&fields, now, declared) {
        return Ok(None);
    }
    let priority = fields
        .get(field)
        .filter(|value| value.is_present())
        .cloned();
    Ok(Some(Candidate {
        rank: (priority, Reverse(id.clone())),
        id,
        fields,
    }))
}

impl Session<'_> {
    /// The `limit` claimable records a priority queue hands out next, best
    /// first, and the index that found them when one did.
    ///
    /// # Errors
    ///
    /// Whatever reading the queue or its index refuses.
    pub(crate) fn priority_head(
        &self,
        transaction: &mut Transaction<'_>,
        context: Context,
        table: tessari_types::TableId,
        declared: &QueueDeclaration,
        field: &str,
        (limit, now): (usize, Datetime),
    ) -> Result<Head> {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }
        if let Some((index, _)) =
            self.index_serving_order(transaction, context, table, &Path::field(field), true)?
        {
            let mut wanted = limit;
            // Asking for more until enough of what the index names can be
            // claimed: held and spent records are in the index too.
            while let Some(found) = transaction.records_in_descending_order(&index, 1, wanted)? {
                let mut taken = Vec::new();
                for (id, bytes) in &found {
                    if let Some(candidate) = ranked(id.clone(), bytes, now, declared, field)? {
                        taken.push(candidate);
                    }
                }
                if taken.len() >= limit {
                    taken.sort_by(|left, right| right.cmp(left));
                    taken.truncate(limit);
                    return Ok((
                        taken
                            .into_iter()
                            .map(|held| (held.id, held.fields))
                            .collect(),
                        Some(index.name),
                    ));
                }
                let Some(more) = wanted.checked_mul(2) else {
                    break;
                };
                wanted = more;
            }
        }
        // The walk: every claimable record, keeping the best `limit` in a heap
        // whose top is the worst kept.
        let mut kept: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(limit);
        transaction.walk_table(
            context.namespace,
            context.database,
            table,
            |_, id, bytes| -> Result<ControlFlow<()>> {
                if let Some(candidate) = ranked(id, &bytes, now, declared, field)? {
                    if kept.len() < limit {
                        kept.push(Reverse(candidate));
                    } else if kept.peek().is_some_and(|Reverse(worst)| candidate > *worst) {
                        kept.pop();
                        kept.push(Reverse(candidate));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )?;
        let mut taken: Vec<Candidate> = kept.into_iter().map(|Reverse(held)| held).collect();
        taken.sort_by(|left, right| right.cmp(left));
        Ok((
            taken
                .into_iter()
                .map(|held| (held.id, held.fields))
                .collect(),
            None,
        ))
    }
}

impl Session<'_> {
    /// `CLAIM n FROM q` on a queue declared `PRIORITY BY f`: the head as
    /// [`Session::priority_head`] finds it, each record held exactly as the
    /// arrival-ordered claim holds one.
    ///
    /// # Errors
    ///
    /// Whatever finding the head or writing a hold refuses.
    pub(super) fn claim_by_priority(
        &self,
        transaction: &mut Transaction<'_>,
        (context, table, named): (Context, tessari_types::TableId, &tessari_ql::TableRef),
        declared: &QueueDeclaration,
        field: &str,
        (limit, now, until): (usize, Datetime, Datetime),
        span: tessari_ql::Span,
    ) -> Result<crate::outcome::Outcome> {
        let (head, index) =
            self.priority_head(transaction, context, table, declared, field, (limit, now))?;
        let mut taken = Vec::with_capacity(head.len());
        for (id, mut fields) in head {
            super::hold(&mut fields, until, self.consumer.as_ref());
            let payload = Value::Object(fields);
            let address = tessari_storage::RecordAddress::new(
                context.namespace,
                context.database,
                table,
                id.clone(),
            );
            self.put_engine_record(transaction, address, payload.clone(), span)?;
            taken.push((id, payload));
        }
        let plan = match index {
            Some(index) => crate::plan::Plan {
                index: Some(index),
                ..crate::plan::Plan::new(crate::outcome::AccessPath::Ordered).on(&named.name.text)
            },
            None => crate::plan::Plan::new(crate::outcome::AccessPath::Scan).on(&named.name.text),
        };
        Ok(crate::outcome::Outcome::Records {
            records: taken,
            plan,
            notes: Vec::new(),
            suggestion: None,
            only: false,
        })
    }
}
