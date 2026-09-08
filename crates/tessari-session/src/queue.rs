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

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use tessari_constants::MAX_CLAIM_RECORDS;
use tessari_encoding::decode_payload;
use tessari_ql::{RecordTarget, Span, TableRef};
use tessari_storage::{
    Catalog, QUEUE_ATTEMPTS, QUEUE_CLAIMED_UNTIL, QueueDeclaration, RecordAddress, TableDefinition,
    TableKind, Transaction,
};
use tessari_types::{Datetime, Number, RecordId, Value};

use crate::error::{Error, Result};
use crate::outcome::{AccessPath, Outcome};
use crate::plan::Plan;
use crate::session::Session;

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
        let until = deadline(now, declared, span)?;

        let mut taken: Vec<(RecordId, Value)> = Vec::new();
        let mut writes: Vec<(RecordId, Value)> = Vec::new();
        transaction.walk_table(
            context.namespace,
            context.database,
            id,
            |_, record, bytes| -> Result<ControlFlow<()>> {
                if taken.len() as u64 >= count {
                    return Ok(ControlFlow::Break(()));
                }
                let Value::Object(mut fields) = decode_payload(&bytes)? else {
                    // A queue record that is not an object cannot carry a hold,
                    // so it is passed over rather than refused: one malformed
                    // record must not stop every worker on the table.
                    return Ok(ControlFlow::Continue(()));
                };
                if !claimable(&fields, now, declared) {
                    return Ok(ControlFlow::Continue(()));
                }
                fields.insert(QUEUE_CLAIMED_UNTIL.to_owned(), Value::Datetime(until));
                fields.insert(
                    QUEUE_ATTEMPTS.to_owned(),
                    Value::Number(Number::Integer(attempts_of(&fields).saturating_add(1))),
                );
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

    /// `RELEASE jobs:7`
    ///
    /// Clears the hold now rather than at its deadline. The attempt count is not
    /// touched: it was taken at the claim, and a record that was handed out was
    /// handed out whatever happened next.
    ///
    /// Releasing a record nothing holds is **not** an error — it is the state the
    /// statement asked for, and a worker that lost a race to the deadline should
    /// not also get a failure for tidying up.
    ///
    /// # Errors
    ///
    /// [`Error::NotAQueue`] when the table is not one, [`Error::Unknown`] when
    /// there is no such record, and the mapped storage failure otherwise.
    pub(crate) fn release(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
        span: Span,
    ) -> Result<Outcome> {
        let (context, address) = self.address(transaction, target)?;
        queue_declaration(
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
        let Value::Object(mut fields) = decode_payload(&stored)? else {
            return Ok(Outcome::Done);
        };
        if fields.remove(QUEUE_CLAIMED_UNTIL).is_none() {
            return Ok(Outcome::Done);
        }
        let _ = context;
        self.put_engine_record(transaction, address, Value::Object(fields), span)?;
        Ok(Outcome::Done)
    }
}

/// The declaration of the queue a statement named, or the refusal.
fn queue_declaration(
    transaction: &mut Transaction<'_>,
    table: tessari_types::TableId,
    named: &str,
    span: Span,
) -> Result<QueueDeclaration> {
    let definition: Option<TableDefinition> = Catalog::new(transaction).table(table)?;
    match definition.map(|found| found.kind) {
        Some(TableKind::Queue(declared)) => Ok(declared),
        _ => Err(Error::NotAQueue {
            table: named.to_owned(),
            span,
        }),
    }
}

/// When a claim taken at `now` lapses.
fn deadline(now: Datetime, declared: QueueDeclaration, span: Span) -> Result<Datetime> {
    let seconds = now
        .seconds()
        .checked_add(declared.timeout.seconds())
        .ok_or(Error::ClaimDeadlineUnreachable { span })?;
    let nanos = now.nanos().saturating_add(declared.timeout.nanos());
    let (seconds, nanos) = if nanos >= 1_000_000_000 {
        (
            seconds
                .checked_add(1)
                .ok_or(Error::ClaimDeadlineUnreachable { span })?,
            nanos.saturating_sub(1_000_000_000),
        )
    } else {
        (seconds, nanos)
    };
    Datetime::new(seconds, nanos).ok_or(Error::ClaimDeadlineUnreachable { span })
}

/// Whether this record may be handed out now.
///
/// Two questions, and the order matters only for readability: a record whose
/// attempts are spent is never claimable however long ago its hold lapsed, and a
/// record still held is not claimable however few attempts it has had.
fn claimable(fields: &BTreeMap<String, Value>, now: Datetime, declared: QueueDeclaration) -> bool {
    if let Some(ceiling) = declared.attempts {
        if attempts_of(fields) >= i64::from(ceiling) {
            return false;
        }
    }
    match fields.get(QUEUE_CLAIMED_UNTIL) {
        // Nothing holds it.
        None | Some(Value::None) => true,
        // Held until the stored instant, which is the whole expiry mechanism:
        // the comparison is the sweep.
        Some(Value::Datetime(until)) => *until <= now,
        // A hold this build cannot read is treated as held rather than as
        // absent. The two failures are not symmetric — reading it as absent
        // hands live work to a second worker, while reading it as held delays
        // the record until somebody looks at it.
        Some(_) => false,
    }
}

/// How many times this record has been handed out.
///
/// Anything that is not a whole number reads as none, because a count that
/// cannot be read is a count nothing was told — and refusing here would let one
/// malformed record stop every worker on the table.
fn attempts_of(fields: &BTreeMap<String, Value>) -> i64 {
    match fields.get(QUEUE_ATTEMPTS) {
        Some(Value::Number(Number::Integer(held))) => *held,
        _ => 0,
    }
}

/// Refuse a caller's write that sets a field only the engine may set.
///
/// Called from the one funnel every caller-driven record write passes through,
/// so this is one rule in one place rather than a check each write path has to
/// remember. The engine's own two writes go through the sibling that skips it,
/// and they are the only two.
///
/// The reasoning is the bucket's, in a second place: engine metadata a caller
/// can write is metadata that can lie, and a hold whose deadline the holder
/// chose is not a hold.
///
/// # Errors
///
/// Returns [`Error::QueueFieldIsTheEngines`] naming the field, so a caller whose
/// payload happens to use one of these names is told exactly what happened
/// rather than silently losing it (Q-461).
pub(crate) fn refuse_engine_fields(
    transaction: &mut Transaction<'_>,
    address: &RecordAddress,
    payload: &Value,
    span: Span,
) -> Result<()> {
    let Value::Object(fields) = payload else {
        return Ok(());
    };
    if !fields.contains_key(QUEUE_CLAIMED_UNTIL) && !fields.contains_key(QUEUE_ATTEMPTS) {
        return Ok(());
    }
    let is_queue = Catalog::new(transaction)
        .table(address.table)?
        .is_some_and(|found| matches!(found.kind, TableKind::Queue(_)));
    if !is_queue {
        return Ok(());
    }
    let field = if fields.contains_key(QUEUE_CLAIMED_UNTIL) {
        QUEUE_CLAIMED_UNTIL
    } else {
        QUEUE_ATTEMPTS
    };
    Err(Error::QueueFieldIsTheEngines { field, span })
}
