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
use std::collections::{BTreeMap, BinaryHeap};

use tessari_encoding::decode_payload;
use tessari_storage::{QueueDeclaration, RecordAddress, Transaction};
use tessari_types::{Datetime, Path, RecordId, Value};

use super::claimable;
use crate::context::Context;
use crate::error::Result;
use crate::session::Session;

/// The head a claim takes, best first, the index that found it, and the spent
/// records met on the way (ADR-0126 D2).
type Head = (
    Vec<(RecordId, std::collections::BTreeMap<String, Value>)>,
    Option<String>,
    Vec<super::dead::Spent>,
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

/// Where a queued record lives.
fn address(context: Context, table: tessari_types::TableId, id: &RecordId) -> RecordAddress {
    RecordAddress::new(context.namespace, context.database, table, id.clone())
}

/// The record's rank, when it can be claimed now; a spent one goes to `dead`
/// when the queue has a dead letter, keyed so a re-read of the index keeps one.
fn ranked(
    id: RecordId,
    bytes: &[u8],
    (now, declared, field): (Datetime, &QueueDeclaration, &str),
    dead: &mut BTreeMap<RecordId, BTreeMap<String, Value>>,
) -> Result<Option<Candidate>> {
    let Value::Object(fields) = decode_payload(bytes)? else {
        return Ok(None);
    };
    if declared.dead_letter.is_some() && super::dead::spent(&fields, now, declared) {
        dead.insert(id, fields);
        return Ok(None);
    }
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
    pub(crate) async fn priority_head(
        &self,
        transaction: &mut Transaction<'_>,
        context: Context,
        table: tessari_types::TableId,
        (declared, field): (&QueueDeclaration, &str),
        (limit, now): (usize, Datetime),
        matching: Option<&super::Matching<'_>>,
    ) -> Result<Head> {
        if limit == 0 {
            return Ok((Vec::new(), None, Vec::new()));
        }
        let mut dead = BTreeMap::new();
        if let Some((index, _)) = self
            .index_serving_order(transaction, context, table, &Path::field(field), true)
            .await?
        {
            let mut wanted = limit;
            // Asking for more until enough of what the index names can be
            // claimed: held and spent records are in the index too.
            while let Some(found) = transaction
                .records_in_descending_order(&index, 1, wanted)
                .await?
            {
                let mut taken = Vec::new();
                for (id, bytes) in &found {
                    if let Some(candidate) =
                        ranked(id.clone(), bytes, (now, declared, field), &mut dead)?
                        && self
                            .matches(transaction, matching, &candidate.id, &candidate.fields)
                            .await?
                        && !transaction.claimed_elsewhere(&address(context, table, &candidate.id))
                    {
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
                        dead.into_iter().collect(),
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
        let mut walk = transaction
            .walk_table(context.namespace, context.database, table)
            .await?;
        while let Some(round) = walk.next_round(transaction).await? {
            for (id, bytes) in round {
                if let Some(candidate) = ranked(id, &bytes, (now, declared, field), &mut dead)?
                    && self
                        .matches(transaction, matching, &candidate.id, &candidate.fields)
                        .await?
                    && !transaction.claimed_elsewhere(&address(context, table, &candidate.id))
                {
                    if kept.len() < limit {
                        kept.push(Reverse(candidate));
                    } else if kept.peek().is_some_and(|Reverse(worst)| candidate > *worst) {
                        kept.pop();
                        kept.push(Reverse(candidate));
                    }
                }
            }
        }
        let mut taken: Vec<Candidate> = kept.into_iter().map(|Reverse(held)| held).collect();
        taken.sort_by(|left, right| right.cmp(left));
        Ok((
            taken
                .into_iter()
                .map(|held| (held.id, held.fields))
                .collect(),
            None,
            dead.into_iter().collect(),
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
    pub(super) async fn claim_by_priority(
        &self,
        transaction: &mut Transaction<'_>,
        (context, table, named): (Context, tessari_types::TableId, &tessari_ql::TableRef),
        (declared, field): (&QueueDeclaration, &str),
        (limit, now, until): (usize, Datetime, Datetime),
        matching: Option<&super::Matching<'_>>,
        span: tessari_ql::Span,
    ) -> Result<crate::outcome::Outcome> {
        let (head, index, buried) = self
            .priority_head(
                transaction,
                context,
                table,
                (declared, field),
                (limit, now),
                matching,
            )
            .await?;
        let mut taken = Vec::with_capacity(head.len());
        for (id, mut fields) in head {
            let address = address(context, table, &id);
            // Taken here rather than while the head was ranked, so a record
            // ranked and then displaced is never held back from another claim;
            // one taken since by another claim is passed over (ADR-0126 D5).
            if !transaction.take_for_claim(&address) {
                continue;
            }
            super::hold(&mut fields, until, self.consumer.as_ref());
            let payload = Value::Object(fields);
            self.put_engine_record(transaction, address, payload.clone(), span)
                .await?;
            taken.push((id, payload));
        }
        self.bury(transaction, (context, table, named), declared, buried, span)
            .await?;
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
