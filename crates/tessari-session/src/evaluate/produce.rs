//! Producing the records a prepared source names.

use std::ops::ControlFlow;

use tessari_ql::Select;
use tessari_storage::Transaction;
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
    pub(super) fn produce_source(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        prepared: Prepared<'_>,
        searched: &Searched,
        consumer: &mut dyn Consumer,
        reporting: Reporting<'_>,
    ) -> Result<Plan> {
        let named = table_named(&select.from);
        // The table every plan below reports, resolved once: which table a read
        // is over is a property of the statement and not of the walk that turned
        // out to serve it.
        let over = |access: AccessPath| Plan {
            table: named.map(ToOwned::to_owned),
            ..Plan::new(access)
        };
        match prepared {
            Prepared::Held(found, plan) => {
                hand_over(found, transaction, consumer)?;
                Ok(plan)
            }
            Prepared::Table(context, id) => {
                // First: an index leading on the key answers one seek per value.
                // The planner declines every other walk and the bound for such a
                // read (`plan::bound`), so unserved it falls through to the scan.
                if let Some(latest) = &select.latest
                    && let Walked::Served { found, index } =
                        self.walk_latest(transaction, context, id, latest)?
                {
                    hand_over(found, transaction, consumer)?;
                    return Ok(Plan {
                        shape: Some("latest"),
                        index: Some(index),
                        ..over(AccessPath::Index)
                    });
                }
                // A statement that asked for an approximate ordering, over a
                // field carrying a graph built for the distance it named, is the
                // one read in this store an index answers differently from a
                // scan. Every other shape falls through to the scan below, which
                // is exact.
                if let Some(walk) = plan::nearest(select)
                    && let Some((found, index)) = self.walk(transaction, context, id, &walk)?
                {
                    // The one place the note is not about a cost but about the
                    // answer: these records are the best the graph found, and
                    // nothing in their shape says so.
                    reporting.collected.push(Note::Approximate);
                    hand_over(found, transaction, consumer)?;
                    return Ok(Plan {
                        index: Some(index),
                        ..over(AccessPath::Approximate)
                    });
                }
                // A bounded order by distance from a place, over a field
                // carrying a spatial index. Exact rather than approximate, and
                // therefore asking nothing of the statement: a best-first walk
                // ordered by a floor visits every record that could rank above
                // the ones it holds.
                if let Some(closest) = plan::closest(select) {
                    match self.walk_to_place(transaction, context, id, &closest)? {
                        Walked::Served { found, index } => {
                            hand_over(found, transaction, consumer)?;
                            return Ok(Plan {
                                shape: Some("nearest"),
                                index: Some(index),
                                ..over(AccessPath::Ordered)
                            });
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
                    match self.walk_scored(transaction, context, id, &scored, searched)? {
                        Walked::Served { found, index } => {
                            // The page was found below its anchor's score rather
                            // than by reading every record, so the note saying
                            // otherwise — decided before the source ran — goes.
                            if scored.after.is_some() {
                                reporting
                                    .collected
                                    .retain(|note| *note != Note::CursorWalked);
                            }
                            hand_over(found, transaction, consumer)?;
                            return Ok(Plan {
                                shape: Some("scored"),
                                index: Some(index),
                                ..over(AccessPath::Ordered)
                            });
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
                    match self.walk_in_order(transaction, context, id, &bound)? {
                        Walked::Served { found, index } => {
                            hand_over(found, transaction, consumer)?;
                            return Ok(Plan {
                                index: Some(index),
                                ..over(AccessPath::Ordered)
                            });
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
                    (Some(anchor), bound) => transaction.records_after(
                        context.namespace,
                        context.database,
                        id,
                        anchor.id.fixed(anchor.span)?,
                        bound,
                    )?,
                    (None, Some(wanted)) => transaction.first_records_of(
                        context.namespace,
                        context.database,
                        id,
                        wanted,
                    )?,
                    (None, None) => {
                        transaction.scan_table(context.namespace, context.database, id)?
                    }
                };
                let visible = self.visible_in(transaction, id)?;
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
                    if consumer.take(transaction, found_id, record)?.is_break() {
                        break;
                    }
                }
                Ok(over(AccessPath::Scan))
            }
            Prepared::Filtered(context, id, condition) => {
                // The order first, when an index holds it. A filtered read that
                // narrows and then sorts is correct and costs a sort of
                // everything the condition matched; taking the records in the
                // order they are already stored in costs the bound.
                // Either direction, and the direction travels on the bound:
                // ascending is served only where the ordering field is
                // `REQUIRED`, which is a fact about the schema and is asked for
                // by `index_serving_order` rather than by this call site.
                // The decline is remembered rather than acted on here, because
                // what the read falls back *to* is not known until `candidates`
                // has chosen — an index on the condition serves this read even
                // when no index could serve its order.
                // A nearest read the statement let be approximate: the graph,
                // filtered by the whole condition (`evaluate/nearest.rs`) —
                // unless an index on the condition already narrowed the read to
                // no more records than the walk would visit, which the exact
                // path below answers for the same cost and exactly.
                let mut gave_up = false;
                let mut reached_early = None;
                if let Some(walk) = plan::nearest(select) {
                    let reached = self.candidates(
                        transaction,
                        id,
                        context,
                        condition,
                        searched,
                        Asked {
                            named,
                            lift_scan_guard: select.lift_scan_guard,
                        },
                    )?;
                    let ceiling = tessari_storage::filtered_ceiling(walk.wanted, walk.effort);
                    // Decided from the plan the condition's index carries — the
                    // same question `EXPLAIN` asks — and a range is counted up
                    // to the ceiling rather than built.
                    let few = match &reached {
                        Some(Reached {
                            records,
                            plan: chosen,
                            ..
                        }) => {
                            let range = match records {
                                Candidates::Range {
                                    index,
                                    fixed,
                                    lower,
                                    upper,
                                } => Some((
                                    index.as_ref(),
                                    fixed.as_slice(),
                                    lower.as_ref(),
                                    upper.as_ref(),
                                )),
                                Candidates::Held(_) => None,
                            };
                            plan::narrows_to(transaction, chosen, range, ceiling)?
                        }
                        None => false,
                    };
                    if !few {
                        let testing = Testing {
                            condition,
                            searched,
                            noticed: reporting.noticed,
                        };
                        match self.walk_admitted(transaction, context, id, &walk, testing)? {
                            Walked::Served { found, index } => {
                                reporting.collected.push(Note::Approximate);
                                hand_over(found, transaction, consumer)?;
                                return Ok(Plan {
                                    index: Some(index),
                                    ..over(AccessPath::Approximate)
                                });
                            }
                            Walked::Declined => gave_up = true,
                            Walked::NotServed => {}
                        }
                    }
                    reached_early = Some(reached);
                }
                let mut declined = false;
                if let Some(bound) = plan::ordered(select) {
                    match self.walk_matching(
                        transaction,
                        context,
                        id,
                        &bound,
                        condition,
                        Scope::over(searched, reporting.noticed),
                    )? {
                        Walked::Served { found, index } => {
                            hand_over(found, transaction, consumer)?;
                            return Ok(Plan {
                                index: Some(index),
                                ..over(AccessPath::Ordered)
                            });
                        }
                        Walked::Declined => declined = true,
                        Walked::NotServed => {}
                    }
                }
                let reached = match reached_early {
                    Some(reached) => reached,
                    None => self.candidates(
                        transaction,
                        id,
                        context,
                        condition,
                        searched,
                        Asked {
                            named,
                            lift_scan_guard: select.lift_scan_guard,
                        },
                    )?,
                };
                let Some(Reached {
                    records: candidates,
                    plan,
                    answered,
                }) = reached
                else {
                    // No index serves this condition, so the scan does — and it
                    // is *walked* rather than read whole, because this is the
                    // one source shape whose bound cannot be pushed down. A
                    // `LIMIT` behind a `WHERE` counts records that match and the
                    // source counts records that exist, so the only thing that
                    // can stop it is the `Break` the consumer already returns.
                    // Read whole, that break saved the decode and nothing else:
                    // a match found at the third of a hundred thousand records
                    // cost 83.3 ms and a match at the last cost 82.9 (Q-72).
                    let plan = Plan {
                        table: named.map(ToOwned::to_owned),
                        ..Plan::new(AccessPath::Scan)
                    };
                    if declined {
                        reporting.collected.push(Note::FellBack {
                            from: AccessPath::Ordered,
                            to: plan.access,
                        });
                    }
                    if gave_up {
                        reporting.collected.push(Note::FellBack {
                            from: AccessPath::Approximate,
                            to: plan.access,
                        });
                    }
                    self.scan_matching(
                        transaction,
                        context,
                        id,
                        Testing {
                            condition,
                            searched,
                            noticed: reporting.noticed,
                        },
                        consumer,
                    )?;
                    return Ok(plan);
                };
                if declined {
                    reporting.collected.push(Note::FellBack {
                        from: AccessPath::Ordered,
                        to: plan.access,
                    });
                }
                if gave_up {
                    reporting.collected.push(Note::FellBack {
                        from: AccessPath::Approximate,
                        to: plan.access,
                    });
                }

                // No `expecting` here, deliberately: how many candidates survive
                // the condition is not known until it has been run, and an
                // over-estimate would reserve exactly the memory this contract
                // exists to give back.
                //
                // The candidates are tested against the **whole** condition, not
                // only the conjunct the index answered. That is what makes an
                // index a narrowing device rather than an answer, and it is why
                // adding one still cannot change what a query returns.
                //
                // `answered` is the one read that is not a narrowing: a search
                // index's postings are derived by the same `analyzer.terms` over
                // the same field that the predicate calls, so their intersection
                // *is* "holds all of these terms" rather than an approximation
                // of it, and `Session::trusts` has established that this clause
                // is the whole condition, that the field is not redacted for
                // this session, and that no uncommitted write is missing from
                // the index. Re-testing there re-analyses the record's entire
                // text to reach a verdict already reached — which on a common
                // word is the whole cost of the query.
                //
                // The invariant is unchanged and this is why it survives:
                // believing the read is only permitted where believing it and
                // re-testing it answer the same records, and the tests assert
                // that per operator rather than trusting this paragraph.
                match candidates {
                    Candidates::Held(held) => {
                        for (id, record) in held {
                            if !answered {
                                let passed = self.evaluate_in(
                                    transaction,
                                    condition,
                                    Scope::searching(&record, searched)
                                        .identified(&id)
                                        .noticing(reporting.noticed),
                                )?;
                                if !boolean(&passed, condition.span)? {
                                    continue;
                                }
                            }
                            if consumer.take(transaction, id, record)?.is_break() {
                                break;
                            }
                        }
                    }
                    // The same loop over a source that hands its records over as
                    // it reads them, so a filled bound stops the batch after it
                    // rather than being applied to a set already built. The
                    // condition is re-tested here exactly as above: an index
                    // narrows and never answers, and a record redacted for this
                    // session must fail the test on both paths or the two
                    // answer differently.
                    Candidates::Range {
                        index,
                        fixed,
                        lower,
                        upper,
                    } => {
                        let visible = self.visible_in(transaction, id)?;
                        transaction.walk_records_in_range(
                            &index,
                            &fixed,
                            lower.as_ref(),
                            upper.as_ref(),
                            |transaction, id, payload| {
                                let record = self.record_of(&payload, &visible)?;
                                if !answered {
                                    let passed = self.evaluate_in(
                                        transaction,
                                        condition,
                                        Scope::searching(&record, searched)
                                            .identified(&id)
                                            .noticing(reporting.noticed),
                                    )?;
                                    if !boolean(&passed, condition.span)? {
                                        return Ok(ControlFlow::Continue(()));
                                    }
                                }
                                consumer.take(transaction, id, record)
                            },
                        )?;
                    }
                }
                Ok(plan)
            }
        }
    }
}

/// Hand a collection to the consumer, stopping where it says to.
///
/// The count is exact here, so it is passed on: these are the arms that had to
/// build their collection to reach their context, and a consumer that keeps
/// every record can size itself once instead of doubling its way there.
fn hand_over(
    found: Vec<(RecordId, Value)>,
    transaction: &mut Transaction<'_>,
    consumer: &mut dyn Consumer,
) -> Result<()> {
    consumer.expecting(found.len());
    for (id, record) in found {
        if consumer.take(transaction, id, record)?.is_break() {
            // Nothing follows in any arm that calls this, so stopping the loop
            // is the whole of honouring the break.
            break;
        }
    }
    Ok(())
}
