//! Work given up on, moved to the queue's dead letter (ADR-0126 D2).
//!
//! A record whose attempts are spent and whose last hold has lapsed is met by
//! a claim's walk like any other. With `DEAD LETTER TO t` declared, that claim
//! appends it to the topic and deletes it in its own transaction, so a spent
//! record stops costing every later claim a read and the work is kept where a
//! reader can take it, re-queue it, or let retention end it. Without the
//! clause nothing is moved: the record stays where it is, as it always did.

use std::collections::BTreeMap;

use tessari_ql::{Span, TableRef};
use tessari_storage::{
    Catalog, QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY, QUEUE_CLAIMED_UNTIL, QueueDeclaration,
    RecordAddress, TableKind, Transaction,
};
use tessari_types::{Datetime, RecordId, TableId, Value};

use super::attempts_of;
use crate::context::Context;
use crate::error::Result;
use crate::session::Session;

/// A spent record a walk met, with its fields as stored.
pub(super) type Spent = (RecordId, BTreeMap<String, Value>);

/// Whether a record has had every attempt it will get and nobody holds it.
///
/// Held on its last attempt is not spent — that worker may still finish it —
/// and a hold this build cannot read counts as held, on the rule `claimable`
/// keeps for the same field.
pub(super) fn spent(
    fields: &BTreeMap<String, Value>,
    now: Datetime,
    declared: &QueueDeclaration,
) -> bool {
    let Some(ceiling) = declared.attempts else {
        return false;
    };
    if attempts_of(fields) < i64::from(ceiling) {
        return false;
    }
    match fields.get(QUEUE_CLAIMED_UNTIL) {
        None | Some(Value::None) => true,
        Some(Value::Datetime(until)) => *until <= now,
        Some(_) => false,
    }
}

impl Session<'_> {
    /// Append each spent record to the dead letter and delete it from the queue.
    ///
    /// A dead letter dropped since the declaration moves nothing: the work
    /// stays where it is rather than going nowhere.
    ///
    /// # Errors
    ///
    /// Whatever writing the letter or deleting the record refuses.
    pub(super) fn bury(
        &self,
        transaction: &mut Transaction<'_>,
        (context, queue, named): (Context, TableId, &TableRef),
        declared: &QueueDeclaration,
        spent: Vec<Spent>,
        span: Span,
    ) -> Result<()> {
        let Some(letters) = declared.dead_letter else {
            return Ok(());
        };
        if spent.is_empty() {
            return Ok(());
        }
        let is_topic = Catalog::new(transaction)
            .table(letters)?
            .is_some_and(|found| matches!(found.kind, TableKind::Topic(_)));
        if !is_topic {
            return Ok(());
        }
        for (id, mut fields) in spent {
            let attempts = attempts_of(&fields);
            // The engine's bookkeeping stays behind: the letter's value is the
            // work, and its attempt count is said once, beside it.
            for engine in [QUEUE_ATTEMPTS, QUEUE_CLAIMED_UNTIL, QUEUE_CLAIMED_BY] {
                fields.remove(engine);
            }
            let letter = Value::Object(BTreeMap::from([
                ("queue".to_owned(), Value::from(named.name.text.as_str())),
                ("id".to_owned(), crate::info::id_value(id.clone())),
                ("attempts".to_owned(), Value::from(attempts)),
                ("value".to_owned(), Value::Object(fields)),
            ]));
            let at = self.free_identity(transaction, &context, letters, &letter, span)?;
            self.put_record(
                transaction,
                RecordAddress::new(context.namespace, context.database, letters, at),
                letter,
                span,
            )?;
            self.delete_record(
                transaction,
                RecordAddress::new(context.namespace, context.database, queue, id),
                span,
            )?;
        }
        Ok(())
    }
}
