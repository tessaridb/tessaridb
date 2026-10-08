//! Work handed out under a hold that lapses, and why none of it is a lease.
//!
//! # Three properties, and everything else follows from them
//!
//! **A claim is a write.** Taking one commits a record through the ordinary
//! transaction path, so it is sequenced into the log and replicated by the
//! mechanism every other write uses. There is no lease table outside the log, no
//! lock manager and no side channel — a node that has the log has the claims.
//!
//! **A deadline is a value.** The instant a hold lapses is computed **once**, by
//! the session taking the claim, and written into the record. That is the rule
//! `time::now()` already follows and for the same reason: a replica applies what
//! was written rather than asking its own clock and reaching a different answer.
//!
//! **Expiry is a comparison a reader performs.** Nothing sweeps a passed
//! deadline away and nothing raises an event when one arrives; a claim hands out
//! a record whose stored deadline is in the past, and that comparison *is* the
//! mechanism. So there is no in-memory state to rebuild after a restart and
//! nothing to reconcile on a promotion.
//!
//! Together they are why this engine asks nothing of a cluster that an ordinary
//! write does not already ask, and why it could be built before there is one.
//!
//! # Exclusivity is already here
//!
//! Two workers that pick the same record both **write that record**, which is a
//! detected write-write race under the store's snapshot isolation: the first
//! committer wins and the loser writes nothing at all. It is not write skew —
//! the level's one real gap — precisely because both transactions write the same
//! key rather than different ones. So the guarantee needs no new machinery.
//!
//! **The loser is refused, and the retry belongs to the worker.** This module's
//! header claimed the opposite until the multi-process harness was run against
//! it: `commit.rs` builds the write set once, *above* its retry loop, and that
//! loop re-applies the same batch when the committed tail moves — it does not
//! re-run the statement, so nothing re-selects. `check_for_conflicts` returns
//! `Error::Conflict` straight to the caller. Measured in
//! `tessari-cli/tests/queue_broker.rs` with four consumer processes over sixty
//! records: **60 hand-outs, 60 finished, none twice, and about 178 refusals**.
//! Exclusivity and at-least-once are unaffected — a refused claim wrote nothing
//! — but a worker loop has to ask again, and a caller that treats a refusal as a
//! fault will stop on a healthy queue.
//!
//! # What this does not do
//!
//! Nothing happens when a claimant dies. There is no liveness detection, no
//! session-death hook and no heartbeat — the deadline passes and the record
//! becomes claimable. A worker that dies holding a thirty-second claim delays
//! that record by up to thirty seconds, which is what choosing a timeout means.
//!
//! Expiry compares against a **wall clock**, because a monotonic clock is
//! meaningful only inside one process and so cannot be a value in a log a second
//! process reads. A clock stepped forward passes every jumped deadline at once
//! and redelivers the held set; a clock stepped backward stalls the queue until
//! it catches up. Neither is corrected here, and both are written down so that
//! neither is discovered.

mod claims;
mod delay;
mod priority;
use std::collections::BTreeMap;
use std::ops::ControlFlow;

use tessari_constants::MAX_CLAIM_RECORDS;
use tessari_encoding::decode_payload;
use tessari_ql::{RecordTarget, Span, TableRef};
use tessari_storage::{
    QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY, QUEUE_CLAIMED_UNTIL, RecordAddress, Transaction,
};
use tessari_types::{Datetime, Number, RecordId, Value};

use crate::error::{Error, Result};
use crate::outcome::{AccessPath, Outcome};
use crate::plan::Plan;
use crate::session::Session;
pub(crate) use claims::{
    claimable, claimant_of, deadline, hold_engine_fields, later, mark_claimant, nothing_claimed,
    queue_declaration,
};
use delay::delay_field;

impl Session<'_> {
    /// `CLAIM 10 FROM jobs`
    ///
    /// Walks the table in identity order — which is arrival order, because both
    /// identity kinds this store issues are time-ordered — and takes the first
    /// records nothing holds, up to `count`.
    ///
    /// # Errors
    ///
    /// [`Error::NotAQueue`] when the table is not one, [`Error::ClaimAboveCeiling`]
    /// when more records were asked for than one statement may take, and the
    /// mapped storage failure otherwise.
    pub(crate) fn claim(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        count: u64,
        span: Span,
    ) -> Result<Outcome> {
        if count > MAX_CLAIM_RECORDS {
            return Err(Error::ClaimAboveCeiling {
                asked: count,
                ceiling: MAX_CLAIM_RECORDS,
                span,
            });
        }
        let (context, id) = self.resolve_table(transaction, table)?;
        let declared = queue_declaration(transaction, id, &table.name.text, table.span)?;
        // Read once, here, for the whole statement. Every record this claim
        // touches carries the same deadline, so two records claimed together
        // lapse together — and the instant that reaches the log is a value
        // rather than a computation a reader would repeat.
        let now = crate::call::instant(span)?;
        let until = deadline(now, &declared, span)?;
        // Cloned out before the walk: the closure borrows the transaction, so
        // it cannot also borrow the session.
        let claimant = self.consumer.clone();
        // A count wider than `usize` can never be reached by a vector, so it saturates.
        let limit = usize::try_from(count).unwrap_or(usize::MAX);
        if let Some(field) = declared.priority.clone() {
            return self.claim_by_priority(
                transaction,
                (context, id, table),
                &declared,
                &field,
                (limit, now, until),
                span,
            );
        }

        let mut taken: Vec<(RecordId, Value)> = Vec::new();
        let mut writes: Vec<(RecordId, Value)> = Vec::new();
        transaction.walk_table(
            context.namespace,
            context.database,
            id,
            |_, record, bytes| -> Result<ControlFlow<()>> {
                if taken.len() >= limit {
                    return Ok(ControlFlow::Break(()));
                }
                let Value::Object(mut fields) = decode_payload(&bytes)? else {
                    // A queue record that is not an object cannot carry a hold,
                    // so it is passed over rather than refused: one malformed
                    // record must not stop every worker on the table.
                    return Ok(ControlFlow::Continue(()));
                };
                if !claimable(&fields, now, &declared) {
                    return Ok(ControlFlow::Continue(()));
                }
                fields.insert(QUEUE_CLAIMED_UNTIL.to_owned(), Value::Datetime(until));
                fields.insert(
                    QUEUE_ATTEMPTS.to_owned(),
                    Value::Number(Number::Integer(attempts_of(&fields).saturating_add(1))),
                );
                mark_claimant(&mut fields, claimant.as_ref());
                let payload = Value::Object(fields);
                taken.push((record.clone(), payload.clone()));
                writes.push((record, payload));
                Ok(ControlFlow::Continue(()))
            },
        )?;

        // Written after the walk rather than inside it. The walk copies this
        // transaction's pending writes out before it starts, so a record written
        // during it would be merged into an order that had already been decided
        // — and a claim that read its own hold back would count one record
        // twice.
        for (record, payload) in writes {
            let address = RecordAddress::new(context.namespace, context.database, id, record);
            self.put_engine_record(transaction, address, payload, span)?;
        }

        Ok(Outcome::Records {
            records: taken,
            plan: Plan::new(AccessPath::Scan).on(&table.name.text),
            notes: Vec::new(),
            suggestion: None,
            only: false,
        })
    }

    /// `CLAIM jobs:7`
    ///
    /// The hold the caller asked for, on the record the caller named. Everything
    /// a hold is — the deadline computed once, the attempt taken at the hand-out,
    /// the comparison a later reader performs — is the selecting form's, reached
    /// by a key instead of by a walk.
    ///
    /// **Existence raises and contention answers.** A record that is not there is
    /// [`Error::Unknown`], which is what `RELEASE` answers and what a caller who
    /// named a record has to be told. A record somebody holds, or one whose
    /// attempts are spent, answers **no records**: that is the selecting form's
    /// own convention, and a refusal there would make a worker polling for a busy
    /// record see failures on a healthy queue.
    ///
    /// **It reports [`AccessPath::Record`]** rather than the scan the selecting
    /// form reports, because it did not walk anything. The two are different
    /// statements about how the record was reached and only one of them is true
    /// here.
    ///
    /// # Errors
    ///
    /// [`Error::NotAQueue`] when the table is not one, [`Error::Unknown`] when
    /// there is no such record, and the mapped storage failure otherwise.
    pub(crate) fn claim_record(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
    ) -> Result<Outcome> {
        let (_, address) = self.address(transaction, target)?;
        let declared = queue_declaration(
            transaction,
            address.table,
            &target.table.name.text,
            target.span,
        )?;
        let Some(stored) = transaction.get(&address)? else {
            return Err(Error::Unknown {
                entity: "record",
                name: address.id.to_string(),
                span: target.span,
            });
        };
        let now = crate::call::instant(span)?;
        let until = deadline(now, &declared, span)?;

        // A record that is not an object cannot carry a hold. The selecting form
        // steps over one so that a single malformed record does not stop every
        // worker; here the caller named it, so answering nothing says the same
        // thing about the same record without pretending it was a refusal.
        let Value::Object(mut fields) = decode_payload(&stored)? else {
            return Ok(nothing_claimed(&target.table.name.text));
        };
        if !claimable(&fields, now, &declared) {
            return Ok(nothing_claimed(&target.table.name.text));
        }
        fields.insert(QUEUE_CLAIMED_UNTIL.to_owned(), Value::Datetime(until));
        fields.insert(
            QUEUE_ATTEMPTS.to_owned(),
            Value::Number(Number::Integer(attempts_of(&fields).saturating_add(1))),
        );
        mark_claimant(&mut fields, self.consumer.as_ref());
        let payload = Value::Object(fields);
        let record = address.id.clone();
        self.put_engine_record(transaction, address, payload.clone(), span)?;

        // Written before the answer is built, and read back by nothing: a second
        // targeted claim in the same transaction reaches this record through
        // `Transaction::get`, which merges what this transaction has written, so
        // it sees the hold and answers nothing. The selecting form arrives at the
        // same behaviour from the other side — its writes land after its walk so
        // that a claim cannot count one record twice.
        Ok(Outcome::Records {
            records: vec![(record, payload)],
            plan: Plan::new(AccessPath::Record).on(&target.table.name.text),
            notes: Vec::new(),
            suggestion: None,
            only: false,
        })
    }

    /// `RELEASE jobs:7` · `RELEASE jobs:7 FOR CONSUMER 'billing'`
    ///
    /// Clears the hold now rather than at its deadline. The attempt count is not
    /// touched: it was taken at the claim, and a record that was handed out was
    /// handed out whatever happened next.
    ///
    /// Releasing a record nothing holds is **not** an error — it is the state the
    /// statement asked for, and a worker that lost a race to the deadline should
    /// not also get a failure for tidying up.
    ///
    /// **The bare form compares instances and the named form compares groups**,
    /// which is the split [`Self::release_all`] already makes. The named form is
    /// not a convenience: a client holding ONE connection for many logical
    /// callers is minted a fresh instance at every `USE CONSUMER`, so the
    /// instance-strict form cannot express *let go of the record this caller
    /// took*, while the group name can.
    ///
    /// # Errors
    ///
    /// [`Error::NotAQueue`] when the table is not one, [`Error::Unknown`] when
    /// there is no such record, [`Error::HeldByAnother`] when the hold is not
    /// the caller's, and the mapped storage failure otherwise.
    pub(crate) fn release(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        consumer: Option<&str>,
        not_before: Option<Datetime>,
        span: Span,
    ) -> Result<Outcome> {
        let (context, address) = self.address(transaction, target)?;
        let declared = queue_declaration(
            transaction,
            address.table,
            &target.table.name.text,
            target.span,
        )?;
        let delay = delay_field(&declared, not_before, &target.table.name.text, span)?;
        let Some(stored) = transaction.get(&address)? else {
            return Err(Error::Unknown {
                entity: "record",
                name: address.id.to_string(),
                span: target.span,
            });
        };
        let Value::Object(mut fields) = decode_payload(&stored)? else {
            return Ok(Outcome::Done);
        };
        if !fields.contains_key(QUEUE_CLAIMED_UNTIL) {
            return Ok(Outcome::Done);
        }
        // Whose hold this is decides whether the caller may drop it. A record
        // nobody signed stays releasable by the bare form, which is what keeps
        // every caller that predates `USE CONSUMER` working exactly as it did.
        match (claimant_of(&fields), consumer) {
            // A named group asks for that group's hold and for nothing else,
            // and a record somebody else holds says who, so the caller learns
            // who to ask rather than that it failed.
            (Some(holder), Some(named)) if holder.name != named => {
                return Err(Error::HeldByAnother {
                    consumer: holder.name,
                    span: target.span,
                });
            }
            (Some(holder), None)
                if !self
                    .consumer
                    .as_ref()
                    .is_some_and(|mine| mine.instance == holder.instance) =>
            {
                return Err(Error::HeldByAnother {
                    consumer: holder.name,
                    span: target.span,
                });
            }
            // An unsigned hold belongs to no group, so the named form leaves it
            // exactly where `RELEASE ALL ... FOR CONSUMER` leaves it — untouched
            // and still held — rather than acting as a master key over every
            // hold nobody signed.
            (None, Some(_)) => return Ok(Outcome::Done),
            _ => {}
        }
        fields.remove(QUEUE_CLAIMED_UNTIL);
        fields.remove(QUEUE_CLAIMED_BY);
        if let Some((field, instant)) = &delay {
            fields.insert(field.clone(), Value::Datetime(*instant));
        }
        let _ = context;
        self.put_engine_record(transaction, address, Value::Object(fields), span)?;
        Ok(Outcome::Done)
    }

    /// `RELEASE ALL FROM jobs [FOR CONSUMER 'billing']`
    ///
    /// Every hold in one queue that belongs to this session's instance, or to a
    /// named consumer, cleared in one statement.
    ///
    /// **It answers the records it released**, not a count. A caller cannot list
    /// what it holds without reading first, and a session coming back from a
    /// crash is the caller least able to read anything — so a number here would
    /// leave that read exactly where it was.
    ///
    /// # Errors
    ///
    /// [`Error::NotAQueue`] when the table is not one,
    /// [`Error::NoConsumerDeclared`] when the bare form is used by a session
    /// that never said who it is, and the mapped storage failure otherwise.
    pub(crate) fn release_all(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        consumer: Option<&str>,
        not_before: Option<Datetime>,
        span: Span,
    ) -> Result<Outcome> {
        let (context, id) = self.resolve_table(transaction, table)?;
        let declared = queue_declaration(transaction, id, &table.name.text, table.span)?;
        let delay = delay_field(&declared, not_before, &table.name.text, span)?;
        // The bare form is refused rather than answered, because a session with
        // no instance has no *everything mine* to name. Succeeding on nothing
        // would tell a worker its work was freed when it was not, and freeing
        // every unsigned hold would take work from claimants who never asked
        // this session for anything.
        let whose = match (consumer, self.consumer.as_ref()) {
            (Some(named), _) => Whose::Consumer(named.to_owned()),
            (None, Some(mine)) => Whose::Instance(mine.instance.clone()),
            (None, None) => return Err(Error::NoConsumerDeclared { span }),
        };

        let mut freed: Vec<(RecordId, Value)> = Vec::new();
        let mut writes: Vec<(RecordId, Value)> = Vec::new();
        transaction.walk_table(
            context.namespace,
            context.database,
            id,
            |_, record, bytes| -> Result<ControlFlow<()>> {
                let Value::Object(mut fields) = decode_payload(&bytes)? else {
                    return Ok(ControlFlow::Continue(()));
                };
                let Some(holder) = claimant_of(&fields) else {
                    return Ok(ControlFlow::Continue(()));
                };
                if !whose.holds(&holder) {
                    return Ok(ControlFlow::Continue(()));
                }
                fields.remove(QUEUE_CLAIMED_UNTIL);
                fields.remove(QUEUE_CLAIMED_BY);
                if let Some((field, instant)) = &delay {
                    fields.insert(field.clone(), Value::Datetime(*instant));
                }
                // The attempt count is left alone, for `release`'s reason: it
                // was taken at the hand-out, and a record that was handed out
                // was handed out whatever happened next.
                let payload = Value::Object(fields);
                freed.push((record.clone(), payload.clone()));
                writes.push((record, payload));
                Ok(ControlFlow::Continue(()))
            },
        )?;

        // After the walk, for the reason the selecting claim writes after its
        // own: a record written during a walk is merged into an order that was
        // already decided.
        for (record, payload) in writes {
            let address = RecordAddress::new(context.namespace, context.database, id, record);
            self.put_engine_record(transaction, address, payload, span)?;
        }

        Ok(Outcome::Records {
            records: freed,
            plan: Plan::new(AccessPath::Scan).on(&table.name.text),
            notes: Vec::new(),
            suggestion: None,
            only: false,
        })
    }
}

/// Whose holds a release is asking about.
enum Whose {
    /// This session's own instance — the safe default.
    Instance(String),
    /// A named consumer, which may be several live sessions at once.
    Consumer(String),
}

impl Whose {
    /// Whether this hold is one of the ones asked for.
    fn holds(&self, holder: &Claimant) -> bool {
        match *self {
            Self::Instance(ref instance) => holder.instance == *instance,
            Self::Consumer(ref name) => holder.name == *name,
        }
    }
}

/// Who holds a record, read back out of it.
pub(crate) struct Claimant {
    /// The name the holder's session declared.
    name: String,
    /// The value the engine minted for that session.
    instance: String,
}

/// How many times this record has been handed out.
///
/// Anything that is not a whole number reads as none, because a count that
/// cannot be read is a count nothing was told — and refusing here would let one
/// malformed record stop every worker on the table.
/// Put a hold on a record's fields: the deadline, one more attempt, and who
/// holds it.
fn hold(
    fields: &mut BTreeMap<String, Value>,
    until: tessari_types::Datetime,
    claimant: Option<&crate::session::Consumer>,
) {
    fields.insert(QUEUE_CLAIMED_UNTIL.to_owned(), Value::Datetime(until));
    fields.insert(
        QUEUE_ATTEMPTS.to_owned(),
        Value::Number(Number::Integer(attempts_of(fields).saturating_add(1))),
    );
    mark_claimant(fields, claimant);
}

fn attempts_of(fields: &BTreeMap<String, Value>) -> i64 {
    match fields.get(QUEUE_ATTEMPTS) {
        Some(Value::Number(Number::Integer(held))) => *held,
        _ => 0,
    }
}
