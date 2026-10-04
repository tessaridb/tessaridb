use tessari_ql::Expr;
use tessari_types::TableId;

use crate::context::Context;

use super::*;

impl Session<'_> {
    /// The records a table read under a condition produces: an index that
    /// holds the order, then one that serves the condition, then the scan.
    pub(super) fn produce_filtered(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        (context, id, condition): (Context, TableId, &Expr),
        searched: &Searched,
        consumer: &mut dyn Consumer,
        reporting: Reporting<'_>,
    ) -> Result<Plan> {
        let named = table_named(&select.from);
        let over = |access: AccessPath| Plan {
            table: named.map(ToOwned::to_owned),
            ..Plan::new(access)
        };
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
        // A bounded order by distance from a position, under the
        // condition: the spatial walk tests the whole condition on each
        // record it takes and measures it exactly (G058 C1).
        if let Some(closest) = plan::closest(select) {
            match self.walk_to_place(
                transaction,
                context,
                id,
                &closest,
                Some(condition),
                Scope::over(searched, reporting.noticed),
            )? {
                Walked::Served { found, index } => {
                    hand_over(found, transaction, consumer)?;
                    return Ok(Plan {
                        shape: Some("nearest"),
                        index: Some(index),
                        ..over(AccessPath::Ordered)
                    });
                }
                Walked::Declined => declined = true,
                Walked::NotServed => {}
            }
        }
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
