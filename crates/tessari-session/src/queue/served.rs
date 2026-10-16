//! A claim's condition served by the index that covers it (G073 C1).
//!
//! The planner a read uses chooses the index and names the candidates, so a
//! claim and a `SELECT` with the same condition reach the same records by the
//! same path. The candidates are put in identity order — the order the walk meets
//! records in, which is arrival order — and each is then judged exactly as the
//! walk judges it: spent, claimable, the condition re-tested, the register asked.
//! An index changes what a claim costs, never which records it takes.

use tessari_ql::{Expr, TableRef};
use tessari_storage::{RecordAddress, Transaction};
use tessari_types::{RecordId, TableId};

use crate::context::Context;
use crate::error::Result;
use crate::evaluate::{Asked, Candidates};
use crate::plan::Plan;
use crate::session::Session;

/// The records an index offers, as the store holds them, and how they were reached.
type Served = (Vec<(RecordId, Vec<u8>)>, Plan);

impl Session<'_> {
    /// The candidates for a claim's condition, in identity order, when an index
    /// serves it; `None` when nothing does and the claim walks the queue.
    ///
    /// # Errors
    ///
    /// Whatever choosing or reading the index refuses.
    pub(super) async fn served_candidates(
        &self,
        transaction: &mut Transaction<'_>,
        (context, table): (Context, TableId),
        named: &TableRef,
        condition: &Expr,
    ) -> Result<Option<Served>> {
        let searched = self.searched_for(transaction, table, &[condition]).await?;
        let asked = Asked {
            named: Some(named.name.text.as_str()),
            lift_scan_guard: false,
        };
        let Some(reached) = self
            .candidates(transaction, table, context, condition, &searched, asked)
            .await?
        else {
            return Ok(None);
        };
        // The stored record rather than the redacted one a read answers with:
        // a claim writes the record back, so it must hold every field.
        let mut found = match reached.records {
            Candidates::Range {
                index,
                fixed,
                lower,
                upper,
            } => {
                transaction
                    .records_in_range(&index, &fixed, lower.as_ref(), upper.as_ref())
                    .await?
            }
            // Read again, a second decode per candidate: the read path hands a
            // held set over redacted, and a redacted record written back would
            // lose the fields this caller may not see.
            Candidates::Held(held) => {
                let mut stored = Vec::with_capacity(held.len());
                for (id, _) in held {
                    let address =
                        RecordAddress::new(context.namespace, context.database, table, id.clone());
                    if let Some(bytes) = transaction.get(&address).await? {
                        stored.push((id, bytes));
                    }
                }
                stored
            }
        };
        // A range already arrives in record order; a held set is ordered here
        // rather than trusted to be, because the claim's order is a promise.
        found.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(Some((found, reached.plan)))
    }
}
