use super::*;

impl Session<'_> {
    /// The groups a grouping read of the whole of table `id` folds into, this
    /// node's records offered and the leaders' partials merged, in key order
    /// (ADR-0097 D2) — or `None` when this node holds the whole table, or a
    /// leader declined and the read gathers records instead.
    ///
    /// The ceiling is on what travels: groups, never the records they fold
    /// (ADR-0097 D3).
    pub(crate) fn gather_folded(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        reduce: &crate::Reduce,
        (select, condition): (&tessari_ql::Select, Option<&tessari_ql::Expr>),
        noticed: &Noticed,
        within: Option<crate::budget::Deadline>,
    ) -> Result<Option<(Groups, Note)>> {
        // The read's `TIMEOUT`, checked between the leaders' folds and this
        // node's own pages: nothing here hands a record to a consumer, so the
        // budget a records path spends per record never sees this work (Q-856).
        let mut folded: u64 = 0;
        let in_time = |folded: u64| within.map_or(Ok(()), |deadline| deadline.check(folded));
        let Some(missing) = self.missing(transaction, id, Part::Whole)? else {
            return Ok(None);
        };
        let (Some(gatherer), Some(map)) = (self.gather.as_ref(), missing.map.as_ref()) else {
            return Err(self.not_held_here(&missing)?);
        };
        let occurrences = occurrences(select.projection.written());
        let mut groups = Groups::new();
        for span in map.spans().filter(|span| missing.needed.contains(&span.id)) {
            let Some(window) = window_of(&span, Part::Whole) else {
                continue;
            };
            in_time(folded)?;
            if missing.lacking.contains(&span.id) {
                let asked = Asked {
                    namespace: missing.namespace,
                    database: missing.database,
                    table: id,
                    shard: span.id,
                    window,
                    most: GATHER_RECORDS,
                    pushed: None,
                    enough: None,
                    reduce: Some(reduce),
                    ordered: None,
                    counting: None,
                };
                let partials = match gatherer.gather(&asked) {
                    Ok(Gathered {
                        reduced: Some(crate::Reduced::Partials(partials)),
                        ..
                    }) => partials,
                    Ok(_) => return Ok(None),
                    Err(why) => return Err(self.unanswered(&missing, span.id, why)?),
                };
                if !merge_partials(&mut groups, &occurrences, partials)? {
                    return Ok(None);
                }
            } else {
                // This node's own span, a page at a time, tested here exactly as
                // a local read tests it — notes and refusals included.
                let mut after: Option<RecordId> = None;
                loop {
                    let page = transaction.records_between(
                        missing.namespace,
                        missing.database,
                        id,
                        window,
                        after.as_ref(),
                        GATHER_PAGE_RECORDS,
                    )?;
                    let full = page.len() == GATHER_PAGE_RECORDS;
                    folded = folded.saturating_add(u64::try_from(page.len()).unwrap_or(u64::MAX));
                    after = page.last().map(|(id, _)| id.clone());
                    let mut kept = Vec::with_capacity(page.len());
                    for (record_id, record) in self.records_of(page, &reduce.visible)? {
                        if let Some(condition) = condition {
                            let held = self.evaluate_in(
                                transaction,
                                condition,
                                Scope::of(&record).identified(&record_id).noticing(noticed),
                            )?;
                            if !boolean(&held, condition.span)? {
                                continue;
                            }
                        }
                        kept.push((record_id, record));
                    }
                    self.fold_into(transaction, &mut groups, kept, &occurrences, &select.group)?;
                    if !full {
                        break;
                    }
                    in_time(folded)?;
                }
            }
            if groups.len() > GATHER_RECORDS {
                return Err(missing.too_much());
            }
        }
        in_time(folded)?;
        // A `0.24` leader sent an exact total without its float form, and a
        // float elsewhere in the group made the float form the answer: only the
        // records can give it.
        if groups
            .values()
            .any(|(_, held)| held.iter().flatten().any(Accumulator::lacks_float))
        {
            return Ok(None);
        }
        Ok(Some((groups, missing.note())))
    }
}

/// The part of `span` that `part` needs, or `None` when they do not meet.
pub(crate) fn window_of<'a>(span: &ShardSpan<'a>, part: Part<'a>) -> Option<Window<'a>> {
    let inside =
        |id: &RecordId| span.from.is_none_or(|from| from <= id) && span.to.is_none_or(|to| id < to);
    match part {
        Part::Whole => Some(Window {
            from: span.from,
            to: span.to.map(|to| (to, false)),
        }),
        Part::Record(id) => inside(id).then_some(Window {
            from: Some(id),
            to: Some((id, true)),
        }),
        Part::Span {
            lower,
            upper,
            inclusive,
        } => {
            let empty = if inclusive {
                lower > upper
            } else {
                lower >= upper
            };
            if empty {
                return None;
            }
            let from = match span.from {
                Some(from) if from > lower => from,
                _ => lower,
            };
            // The span's own end is exclusive; the read's is whatever it said.
            let to = match span.to {
                Some(to) if to < upper || (to == upper && inclusive) => (to, false),
                _ => (upper, inclusive),
            };
            let reaches = if to.1 { from <= to.0 } else { from < to.0 };
            reaches.then_some(Window {
                from: Some(from),
                to: Some(to),
            })
        }
    }
}
