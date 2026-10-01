//! Deleting what a condition or a span selects.

use tessari_ql::{DeleteBound, Expr, TableRef};
use tessari_storage::{RecordAddress, Transaction};

use crate::condition::boolean;
use crate::error::Result;
use crate::session::Session;

use super::{Asked, Candidates, IdentitySpan, Scope};

impl Session<'_> {
    /// Remove every record a condition holds for.
    ///
    /// # It is a read and then a write, and the read is the ordinary one
    ///
    /// The records are found by the same path a `SELECT … WHERE` uses, so an
    /// index serves the condition when one exists — a retention statement over
    /// an indexed timestamp is a bounded scan rather than a walk of the table.
    /// Building a second way to find records would be a second place for the
    /// answer to differ.
    ///
    /// # Everything it removes is in one transaction
    ///
    /// The deletes join whatever transaction the statement is running in, so a
    /// retention run is one commit: it removes all of it or none, and a reader
    /// at a snapshot either sees the table before or after. A statement that
    /// deleted in batches would leave a window in which half a policy had been
    /// applied, and nothing would say which half.
    ///
    /// **What that costs is worth stating**: the whole matched set is held and
    /// committed at once, so a retention statement that matches a very large
    /// table is a very large commit. `LIMIT n` is how a caller keeps that commit
    /// to a size they chose; `LIMIT ALL` is how they say the whole table is what
    /// they meant.
    ///
    /// # The bound counts survivors, not candidates
    ///
    /// It is applied *after* the condition decides, which is the only placement
    /// that makes the statement mean one thing. A bound on candidates would stop
    /// the walk after `n` records had been *examined*, so the same statement
    /// against the same data would remove a different set depending on which
    /// index answered it and in what order that index happened to be walked.
    pub(crate) fn delete_where(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        condition: &Expr,
        limit: DeleteBound,
    ) -> Result<crate::outcome::Outcome> {
        let ceiling = match limit {
            DeleteBound::AtMost(count) => count,
            DeleteBound::All => u64::MAX,
        };
        let (context, id) = self.resolve_table(transaction, table)?;
        // A node holding part of a split table would find only its own part's
        // records, remove them, and report the count as the statement's (G051
        // C4): the condition is about the table, so the read refuses as a read
        // of the whole table does.
        self.refuse_reading_a_part(transaction, id, super::Part::Whole)?;
        let searched = self.searched_for(transaction, id, &[condition])?;
        // No plan is reported: a delete answers with a count and has no plan to
        // carry one on, so the table it would name is not asked for.
        //
        // Nor is the read's own verdict on whether it answered the condition.
        // The same argument would hold here, but nothing has measured the
        // re-test as a delete's cost — what a delete spends is in the writes
        // that follow — and a statement that removes records is the last place
        // to take an untested shortcut.
        //
        // And where no index serves the condition this reads the table whole
        // rather than walking it, which a read no longer does. Deliberate: the
        // walk hands records over while the transaction is being written to, and
        // a statement that removes records is the last place to find out what
        // that means. The read's saving does not arise here in any case — a
        // delete has no `LIMIT` that stops the source, it has a ceiling on how
        // many it removes, and it must see every candidate to know it is done.
        let candidates =
            // `false`: a delete carries no read tail, so there is no clause to lift
            // the guard and nothing here to read one from.
            match self.candidates(transaction, id, context, condition, &searched, Asked::nothing())? {
                Some(reached) => match reached.records {
                    Candidates::Held(held) => held,
                    // Read whole here, deliberately, for the reason the scan
                    // above is read whole: a delete has no bound that stops the
                    // source, and it must see every candidate to know it is
                    // done. The walk exists for a read that can stop.
                    Candidates::Range {
                        index,
                        fixed,
                        lower,
                        upper,
                    } => {
                        let visible = self.visible_in(transaction, id)?;
                        let found = transaction.records_in_range(
                            &index,
                            &fixed,
                            lower.as_ref(),
                            upper.as_ref(),
                        )?;
                        self.records_of(found, &visible)?
                    }
                },
                None => {
                    let visible = self.visible_in(transaction, id)?;
                    let scanned =
                        transaction.scan_table(context.namespace, context.database, id)?;
                    self.records_of(scanned, &visible)?
                }
            };

        let mut removed = 0_u64;
        for (record_id, record) in candidates {
            if removed >= ceiling {
                break;
            }
            // Tested against the whole condition, exactly as a read is: the
            // index narrowed, and the condition decides. A delete that trusted
            // the narrowing would remove records the statement did not name.
            let held = self.evaluate_in(
                transaction,
                condition,
                Scope::searching(&record, &searched).identified(&record_id),
            )?;
            if !boolean(&held, condition.span)? {
                continue;
            }
            self.delete_record(
                transaction,
                RecordAddress::new(context.namespace, context.database, id, record_id),
                condition.span,
            )?;
            removed = removed.saturating_add(1);
        }
        Ok(crate::outcome::Outcome::Removed { count: removed })
    }

    /// Every record in a span of identities, removed.
    ///
    /// # Why there is no re-test here, where the conditional delete has one
    ///
    /// `delete_where` tests every candidate against the whole condition,
    /// because an index **narrows** and the condition decides — a delete that
    /// trusted the narrowing would remove records the statement did not name.
    /// A span narrows nothing. It *is* the set the statement named, so there is
    /// no second question and nothing to re-test, and the absence of the re-test
    /// is the property rather than an omission.
    ///
    /// # What it costs
    ///
    /// The span, and not the table. The walk reaches the records between two
    /// positions in the table's own key order, which is what makes this usable
    /// as a retention pass over a table that has grown — the conditional form
    /// reads every record it is going to keep, once per run, forever.
    ///
    /// # Errors
    ///
    /// Returns an error when a bound is an unbound parameter, the table cannot
    /// be resolved, or the store cannot be read.
    pub(crate) fn delete_span(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        span: IdentitySpan<'_>,
        limit: DeleteBound,
    ) -> Result<crate::outcome::Outcome> {
        let ceiling = match limit {
            DeleteBound::AtMost(count) => count,
            DeleteBound::All => u64::MAX,
        };
        let (context, id) = self.resolve_table(transaction, table)?;
        let lower = span.lower.fixed(span.at)?;
        let upper = span.upper.fixed(span.at)?;
        // As in `delete_where`: a span reaching a shard this node lacks would
        // remove only the records it holds (G051 C4).
        self.refuse_reading_a_part(
            transaction,
            id,
            super::Part::Span {
                lower,
                upper,
                inclusive: span.inclusive,
            },
        )?;
        let found = transaction.records_in_span(
            context.namespace,
            context.database,
            id,
            lower,
            upper,
            span.inclusive,
        )?;

        let mut removed = 0_u64;
        for (record_id, _) in found {
            if removed >= ceiling {
                break;
            }
            self.delete_record(
                transaction,
                RecordAddress::new(context.namespace, context.database, id, record_id),
                span.at,
            )?;
            removed = removed.saturating_add(1);
        }
        Ok(crate::outcome::Outcome::Removed { count: removed })
    }
}
