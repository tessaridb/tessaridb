//! One record offered to an arrival-ordered claim — by the walk, or by the index
//! that served the claim's condition — and what the claim does with it.

use std::ops::ControlFlow;

use tessari_encoding::decode_payload;
use tessari_storage::{QUEUE_ATTEMPTS, QUEUE_CLAIMED_UNTIL, RecordAddress, Transaction};
use tessari_types::{Datetime, Number, RecordId, Value};

use super::{attempts_of, claimable, mark_claimant};
use crate::error::Result;
use crate::session::Session;

/// What one arrival-ordered claim is taking, as the records are offered to it.
pub(super) struct Taking<'a> {
    pub(super) declared: &'a tessari_storage::QueueDeclaration,
    pub(super) matching: Option<&'a super::Matching<'a>>,
    pub(super) claimant: Option<&'a crate::session::Consumer>,
    /// The instant the claim reads the clock at, and the deadline it writes.
    pub(super) moment: (Datetime, Datetime),
    pub(super) limit: usize,
    pub(super) place: (crate::context::Context, tessari_types::TableId),
    pub(super) taken: Vec<(RecordId, Value)>,
    pub(super) writes: Vec<(RecordId, Value)>,
    pub(super) buried: Vec<super::dead::Spent>,
}

impl Session<'_> {
    /// One record offered to an arrival-ordered claim, by the walk or by the
    /// index that served its condition: taken, buried, or passed over.
    pub(super) fn offer(
        &self,
        transaction: &mut Transaction<'_>,
        taking: &mut Taking<'_>,
        record: RecordId,
        bytes: &[u8],
    ) -> Result<ControlFlow<()>> {
        if taking.taken.len() >= taking.limit {
            return Ok(ControlFlow::Break(()));
        }
        let (now, until) = taking.moment;
        let Value::Object(mut fields) = decode_payload(bytes)? else {
            // A queue record that is not an object cannot carry a hold,
            // so it is passed over rather than refused: one malformed
            // record must not stop every worker on the table.
            return Ok(ControlFlow::Continue(()));
        };
        // Met by the walk whatever the condition asks, because a spent
        // record is nobody's work any more (ADR-0126 D2).
        if taking.declared.dead_letter.is_some()
            && super::dead::spent(&fields, now, taking.declared)
        {
            taking.buried.push((record, fields));
            return Ok(ControlFlow::Continue(()));
        }
        if !claimable(&fields, now, taking.declared)
            || !self.matches(transaction, taking.matching, &record, &fields)?
        {
            return Ok(ControlFlow::Continue(()));
        }
        // Another open claim is taking it: passed over, so this claim
        // takes the next record rather than racing for this one
        // (ADR-0126 D5).
        let (context, table) = taking.place;
        let address =
            RecordAddress::new(context.namespace, context.database, table, record.clone());
        if !transaction.take_for_claim(&address) {
            return Ok(ControlFlow::Continue(()));
        }
        fields.insert(QUEUE_CLAIMED_UNTIL.to_owned(), Value::Datetime(until));
        fields.insert(
            QUEUE_ATTEMPTS.to_owned(),
            Value::Number(Number::Integer(attempts_of(&fields).saturating_add(1))),
        );
        mark_claimant(&mut fields, taking.claimant);
        let payload = Value::Object(fields);
        taking.taken.push((record.clone(), payload.clone()));
        taking.writes.push((record, payload));
        Ok(ControlFlow::Continue(()))
    }
}
