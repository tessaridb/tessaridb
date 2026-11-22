//! Producing the records a prepared source names.

mod filtered;

use tessari_ql::Select;
use tessari_storage::{RecordAddress, Transaction};
use tessari_types::{RecordId, Value};

use crate::condition::boolean;
use crate::consume::Consumer;
use crate::error::Result;
use crate::outcome::{AccessPath, Note};
use crate::plan;
use crate::plan::Plan;
use crate::search::Searched;
use crate::session::Session;

use super::{
    Asked, Candidates, Prepared, Reached, Reporting, Scope, Testing, Walked, sought, table_named,
};

impl Session<'_> {
    /// The records a source produces, handed over one at a time.
    ///
    /// Returns the path the walk turned out to take. Nothing is returned that a
    /// consumer could have kept — that is the point of the contract (ADR-0014):
    /// the source never learns what the consumer keeps, so a bound reaches it
    /// only through the `Break` the consumer answers with.
    pub(super) async fn produce_source(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        prepared: Prepared<'_>,
        searched: &Searched,
        consumer: &mut impl Consumer,
        reporting: Reporting<'_>,
    ) -> Result<(Plan, usize)> {
        let named = table_named(&select.from);
        // A locking read's claim-taking, built where the source's tenancy and
        // table are known. None is the ordinary read; the Held arm keeps it
        // None, because the parser only lets a locking clause through on a
        // plain table read — a Held source holds values, never claimable rows.
        let mut taking: Option<Claiming<'_>> = None;
        // The table every plan below reports, resolved once: which table a read
        // is over is a property of the statement and not of the walk that turned
        // out to serve it.
        let over = |access: AccessPath| Plan {
            table: named.map(ToOwned::to_owned),
            ..Plan::new(access)
        };
        match prepared {
            Prepared::Held(found, plan) => {
                hand_over(found, transaction, consumer, &mut taking).await?;
                Ok((*plan, 0))
            }
            Prepared::Table(context, id) => {
                let mut took = 0_usize;
                taking = select.locking.as_ref().map(|locking| Claiming {
                    locking,
                    base: (context.namespace, context.database, id),
                    took: &mut took,
                });
                // First: an index leading on the key answers one seek per value.
                // The planner declines every other walk and the bound for such a
                // read (`plan::bound`), so unserved it falls through to the scan.
                if let Some(latest) = &select.latest
                    && let Walked::Served { found, index } =
                        self.walk_latest(transaction, context, id, latest).await?
                {
                    hand_over(found, transaction, consumer, &mut taking).await?;
                    return Ok((
                        Plan {
                            shape: Some("latest"),
                            index: Some(index),
                            ..over(AccessPath::Index)
                        },
                        took,
                    ));
                }
                // A statement that asked for an approximate ordering, over a
                // field carrying a graph built for the distance it named, is the
                // one read in this store an index answers differently from a
                // scan. Every other shape falls through to the scan below, which
                // is exact.
                if let Some(walk) = plan::nearest(select)
                    && let Some((found, index)) = self.walk(transaction, context, id, &walk).await?
                {
                    // The one place the note is not about a cost but about the
                    // answer: these records are the best the graph found, and
                    // nothing in their shape says so.
                    reporting.collected.push(Note::Approximate);
                    hand_over(found, transaction, consumer, &mut taking).await?;
                    return Ok((
                        Plan {
                            index: Some(index),
                            ..over(AccessPath::Approximate)
                        },
                        took,
                    ));
                }
                // A bounded order by distance from a place, over a field
                // carrying a spatial index. Exact rather than approximate, and
                // therefore asking nothing of the statement: a best-first walk
                // ordered by a floor visits every record that could rank above
                // the ones it holds.
                if let Some(closest) = plan::closest(select) {
                    match self
                        .walk_to_place(
                            transaction,
                            context,
                            id,
                            &closest,
                            None,
                            Scope::over(searched, reporting.noticed),
                        )
                        .await?
                    {
                        Walked::Served { found, index } => {
                            hand_over(found, transaction, consumer, &mut taking).await?;
                            return Ok((
                                Plan {
                                    shape: Some("nearest"),
                                    index: Some(index),
                                    ..over(AccessPath::Ordered)
                                },
                                took,
                            ));
                        }
                        Walked::Declined => reporting.collected.push(Note::FellBack {
                            from: AccessPath::Ordered,
                            to: AccessPath::Scan,
                        }),
                        Walked::NotServed => {}
                    }
                }
                // A bounded order by how well a record answers a query, over a
                // field carrying a search index. Exact, and for the same reason
                // the walk above is: the records it does not read are records it
                // has shown cannot reach the answer.
                if let Some(scored) = plan::scored(select) {
                    match self
                        .walk_scored(transaction, context, id, &scored, searched)
                        .await?
                    {
                        Walked::Served { found, index } => {
                            // The page was found below its anchor's score rather
                            // than by reading every record, so the note saying
                            // otherwise — decided before the source ran — goes.
                            if scored.after.is_some() {
                                reporting
                                    .collected
                                    .retain(|note| *note != Note::CursorWalked);
                            }
                            hand_over(found, transaction, consumer, &mut taking).await?;
                            return Ok((
                                Plan {
                                    shape: Some("scored"),
                                    index: Some(index),
                                    ..over(AccessPath::Ordered)
                                },
                                took,
                            ));
                        }
                        Walked::Declined => reporting.collected.push(Note::FellBack {
                            from: AccessPath::Ordered,
                            to: AccessPath::Scan,
                        }),
                        Walked::NotServed => {}
                    }
                }
                // The other shape an index serves without a condition: an order
                // it is already stored in, and a bound to stop at. Exact — the
                // records come back for the ordering stage and `bounded` to
                // shape, the same two the other answers go through.
                if let Some(bound) = plan::ordered(select) {
                    match self.walk_in_order(transaction, context, id, &bound).await? {
                        Walked::Served { found, index } => {
                            hand_over(found, transaction, consumer, &mut taking).await?;
                            return Ok((
                                Plan {
                                    index: Some(index),
                                    ..over(AccessPath::Ordered)
                                },
                                took,
                            ));
                        }
                        // The index holds the order and ran out of entries, so
                        // the records that would fill the rest of the answer are
                        // ones it does not hold. The scan below finds them, and
                        // this is the note saying the index did not earn its
                        // keep on this read.
                        Walked::Declined => reporting.collected.push(Note::FellBack {
                            from: AccessPath::Ordered,
                            to: AccessPath::Scan,
                        }),
                        Walked::NotServed => {}
                    }
                }
                // The bound reaches the source here, and only here, because this
                // is the one arm where the records the source produces are the
                // records the answer holds. `plan::bound` returns nothing for
                // every shape where they differ (ADR-0013).
                let resuming = select.after.as_ref().filter(|_| sought(select));
                let found = match (resuming, plan::bound(select)) {
                    // The seek. A record's key is its table prefix followed by
                    // its identity, so a read whose order is the store's own
                    // begins at a *position* rather than at the table — and the
                    // records before the anchor are never read, which is the
                    // entire difference between a cursor and an offset.
                    (Some(anchor), bound) => {
                        transaction
                            .records_after(
                                context.namespace,
                                context.database,
                                id,
                                anchor.id.fixed(anchor.span)?,
                                bound,
                            )
                            .await?
                    }
                    (None, Some(wanted)) => {
                        transaction
                            .first_records_of(context.namespace, context.database, id, wanted)
                            .await?
                    }
                    (None, None) => {
                        transaction
                            .scan_table(context.namespace, context.database, id)
                            .await?
                    }
                };
                let visible = self.visible_in(transaction, id).await?;
                // Row guards ride the same per-record moment as the claim and
                // the hand-over: a record the guards do not reach is not
                // claimed and not delivered, so a bounded read counts only
                // admissible rows (ADR-0140).
                let rows = self.rows_in(transaction, id).await?;
                // The one place in the store that knows how many records are
                // coming before any of them is decoded. A consumer that holds
                // every record allocates its spine once here instead of doubling
                // its way there; one that holds a bounded few ignores it.
                consumer.expecting(found.len());
                // Decoded one at a time and handed straight over, so the decoded
                // form of the whole table never exists at once. This is where the
                // measured cost was: 43 328 KiB of a 45 822 KiB peak was that
                // vector, and the stored payloads it is decoded from are a tenth
                // of it (ADR-0014, Q-72).
                for (found_id, payload) in found {
                    let record = self.record_of(&payload, &visible)?;
                    // A record the table's row guards do not reach is skipped
                    // before the claim and the hand-over, like a record the
                    // condition refused.
                    let Some(record) = self
                        .permitted(transaction, &found_id, record, &visible, &rows)
                        .await?
                    else {
                        continue;
                    };
                    // A locking read claims the row before handing it over:
                    // NOWAIT refuses on another transaction's claim, SKIP
                    // LOCKED passes the row over and the bound keeps pulling.
                    if let Some(taking) = taking.as_mut()
                        && !claim_row(transaction, taking, &found_id)?
                    {
                        continue;
                    }
                    if consumer
                        .take(transaction, found_id, record)
                        .await?
                        .is_break()
                    {
                        break;
                    }
                }
                Ok((over(AccessPath::Scan), took))
            }
            Prepared::Filtered(context, id, condition) => {
                self.produce_filtered(
                    transaction,
                    select,
                    (context, id, condition),
                    searched,
                    consumer,
                    reporting,
                )
                .await
            }
        }
    }
}

/// Hand a collection to the consumer, stopping where it says to.
///
/// The count is exact here, so it is passed on: these are the arms that had to
/// build their collection to reach their context, and a consumer that keeps
/// every record can size itself once instead of doubling its way there.
async fn hand_over(
    found: Vec<(RecordId, Value)>,
    transaction: &mut Transaction<'_>,
    consumer: &mut impl Consumer,
    claiming: &mut Option<Claiming<'_>>,
) -> Result<()> {
    consumer.expecting(found.len());
    for (id, record) in found {
        // A locking read claims the row before handing it over: NOWAIT
        // refuses on another transaction's claim, SKIP LOCKED passes over it
        // — a passed-over row never reaches the consumer, so a bound behind
        // it keeps pulling.
        if let Some(taking) = claiming.as_mut()
            && !claim_row(transaction, taking, &id)?
        {
            continue;
        }
        if consumer.take(transaction, id, record).await?.is_break() {
            // Nothing follows in any arm that calls this, so stopping the loop
            // is the whole of honouring the break.
            break;
        }
    }
    Ok(())
}

/// A locking read's claim-taking state: the clause, the table the rows belong
/// to, and the count of rows claimed, reported to the caller as a note.
pub(super) struct Claiming<'a> {
    /// The clause the read carries.
    locking: &'a tessari_ql::Locking,
    /// The tenancy and table every produced row belongs to.
    base: (
        tessari_types::NamespaceId,
        tessari_types::DatabaseId,
        tessari_types::TableId,
    ),
    /// How many rows this read has claimed, counted into the caller's note.
    took: &'a mut usize,
}

/// Claim one produced row for the read's transaction, answering whether the
/// row is kept: `false` is a SKIP LOCKED row another transaction claims,
/// passed over. NOWAIT refuses instead of passing over — the engine never
/// waits, and the refusal names the wait.
pub(super) fn claim_row(
    transaction: &Transaction<'_>,
    claiming: &mut Claiming<'_>,
    id: &RecordId,
) -> Result<bool> {
    let address = RecordAddress::new(
        claiming.base.0,
        claiming.base.1,
        claiming.base.2,
        id.clone(),
    );
    if transaction.claimed_elsewhere(&address) {
        return match claiming.locking.mode {
            tessari_ql::LockMode::SkipLocked => Ok(false),
            tessari_ql::LockMode::Nowait => Err(crate::error::Error::ClaimedByAnother {
                id: id.to_string(),
                span: claiming.locking.span,
            }),
        };
    }
    // `claimed_elsewhere` above established the row is free or already this
    // transaction's, so the take's own answer — the register confirming it —
    // cannot be `false`; it is not read.
    let _ = transaction.take_for_claim(&address);
    *claiming.took = claiming.took.saturating_add(1);
    Ok(true)
}
