//! `UPDATE t SET … WHERE …` — one edit applied to every record a condition holds
//! for (ADR-0134 D1).

use tessari_encoding::decode_payload;
use tessari_ql::Span;
use tessari_ql::{Answer, Edit, Expr, SetBound, TableRef, WriteExpiry};
use tessari_storage::{Catalog, RecordAddress, Transaction};
use tessari_types::{Number, RecordId, Value};

use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::evaluate::{Matched, Scope};
use crate::outcome::Outcome;
use crate::redact::seen;
use crate::session::Session;

use super::expiry::{self, Settled};

/// What a set update names, gathered so the executor takes one argument rather
/// than seven.
pub(super) struct SetUpdate<'a> {
    pub(super) table: &'a TableRef,
    pub(super) edit: &'a Edit,
    pub(super) condition: &'a Expr,
    pub(super) limit: SetBound,
    pub(super) expire: Option<&'a WriteExpiry>,
    pub(super) answer: Answer,
    pub(super) span: Span,
}

impl Session<'_> {
    /// Change every record a condition holds for, in this one transaction.
    ///
    /// # Found as a set delete finds its records, changed as a record update is
    ///
    /// The candidates come from [`Session::matched`], the path `DELETE FROM …
    /// WHERE` takes, so an index serves the condition and the whole set is held
    /// before the first edit — an edit that moves an indexed field is never met
    /// again by the walk. Each candidate is re-tested against the whole
    /// condition, as the record this session may see, and then changed by
    /// [`Session::edit_stored`], the one path a record update takes too: so
    /// assertions, defaults, field types, the queue's engine fields, events and
    /// expiry cannot hold for one record and differ for many.
    ///
    /// # The edit reads the record as stored
    ///
    /// The condition sees what this session may read; the edit is applied to the
    /// record as it is stored. Applying it to the redacted view would write back
    /// a record missing every field a grant hides — a `SET` of one field that
    /// silently deleted the rest.
    ///
    /// # All of it or none
    ///
    /// A refusal on any record — an assertion, an event, a type — fails the
    /// statement, and the transaction it ran in discards every record already
    /// changed. The count, when it is the answer, is how many were changed.
    pub(super) async fn update_where(
        &self,
        transaction: &mut Transaction<'_>,
        update: SetUpdate<'_>,
    ) -> Result<Outcome> {
        let ceiling = match update.limit {
            SetBound::AtMost(count) => count,
            SetBound::All => u64::MAX,
        };
        let Matched {
            context,
            table,
            plan,
            searched,
            candidates,
        } = self
            .matched(transaction, update.table, update.condition)
            .await?;
        if let Some(found) = Catalog::new(transaction).table(table).await? {
            // The guards `writable` puts in front of a record update, asked of
            // the table rather than of one address.
            if found.is_bucket() {
                return Err(Error::NotWrittenByHand {
                    table: update.table.name.text.clone(),
                    span: update.table.span,
                });
            }
            if found.is_vault() {
                return Err(Error::VaultUpdatedByCondition {
                    table: update.table.name.text.clone(),
                    span: update.table.span,
                });
            }
        }
        let settled = self
            .resolve_write_expiry(transaction, table, update.expire)
            .await?;
        // What an answer carrying records may show, decided once for the set.
        let visible = match update.answer {
            Answer::Before | Answer::After => Some(self.visible_in(transaction, table).await?),
            Answer::Nothing | Answer::Position => None,
        };

        let mut changed: u64 = 0;
        let mut answered: Vec<(RecordId, Value)> = Vec::new();
        for (id, record) in candidates {
            if changed >= ceiling {
                break;
            }
            let held = self
                .evaluate_in(
                    transaction,
                    update.condition,
                    Scope::searching(&record, &searched).identified(&id),
                )
                .await?;
            if !boolean(&held, update.condition.span)? {
                continue;
            }
            let address =
                RecordAddress::new(context.namespace, context.database, table, id.clone());
            // Gone already when an event an earlier record ran removed it, in
            // which case there is nothing left to change.
            let Some(stored) = transaction.get(&address).await? else {
                continue;
            };
            let before = decode_payload(&stored)?;
            let after = self
                .edit_stored(
                    transaction,
                    &address,
                    (update.edit, before.clone()),
                    settled,
                    (update.table.span, update.span),
                )
                .await?;
            changed = changed.saturating_add(1);
            if let Some(visible) = &visible {
                let shown = match update.answer {
                    Answer::Before => before,
                    _ => after,
                };
                answered.push((id, seen(shown, visible)));
            }
        }
        Ok(match update.answer {
            Answer::Before | Answer::After => Outcome::Records {
                records: answered,
                plan,
                notes: Vec::new(),
                suggestion: None,
                only: false,
            },
            // A count of records this statement changed, which cannot exceed
            // what one transaction can hold — far below `i64::MAX`.
            Answer::Nothing | Answer::Position => Outcome::Value(Value::Number(Number::Integer(
                i64::try_from(changed).unwrap_or(i64::MAX),
            ))),
        })
    }

    /// Apply one edit to one stored record and write it, the way every update
    /// does — the record form and the set form both end here.
    ///
    /// `REQUIRED` + `DEFAULT` run over the edited payload, so a field set to
    /// `NONE` that has a default keeps one; the write goes through the sealing
    /// path, which carries the queue's engine fields and runs the table's
    /// events; the expiry the statement settled is applied last.
    pub(super) async fn edit_stored(
        &self,
        transaction: &mut Transaction<'_>,
        address: &RecordAddress,
        (edit, before): (&Edit, Value),
        settled: Option<Settled>,
        (target, span): (Span, Span),
    ) -> Result<Value> {
        let (payload, partial) = self.applied(transaction, edit, before, target).await?;
        // One rule rather than two: the result of either shape is a record
        // being written, so `REQUIRED` + `DEFAULT` keeps meaning "this field
        // always holds a value" even when a caller sets one to `none`.
        let (payload, partial) = self
            .defaults_over(transaction, address.table, payload, partial)
            .await?;
        self.put_record_sealing(
            transaction,
            address.clone(),
            payload.clone(),
            partial.as_ref(),
            span,
        )
        .await?;
        expiry::settle_write_expiry(transaction, address, settled);
        Ok(payload)
    }
}
