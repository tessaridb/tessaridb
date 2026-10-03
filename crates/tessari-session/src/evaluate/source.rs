//! Deciding how a read reaches its records, and refusing the reads a caller may not make.

use tessari_ql::{Expr, Select, Source, TableRef};
use tessari_storage::{Catalog, TableDefinition, Transaction};
use tessari_types::TableId;

use crate::budget::{Ceiling, Deadline};
use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::outcome::AccessPath;
use crate::plan::Plan;
use crate::search::Searched;
use crate::session::Session;

use super::{Part, Prepared, Reporting, Scope, ceiling_reached, node_row, shown};

impl Session<'_> {
    /// What resolving a source establishes before it produces a record.
    ///
    /// Split from producing because the searched context is known **before** the
    /// walk and used to be reported only after it, which forced every consumer to
    /// collect: a sort key is an expression, and one holding a `MATCHES` or a
    /// score means nothing without it. Both table arms compute it from the
    /// schema, and the schema does not change under a read.
    ///
    /// Takes the whole statement rather than only its source, because what the
    /// searched fields need is decided by every expression the read evaluates —
    /// a `SELECT … ORDER BY search::score(body, 'x') FROM notes` searches a
    /// field its source never mentions.
    ///
    /// The access path stays with produce. Which walk serves a table is decided
    /// by whether one *succeeds*, so a prepare that reported a path would be
    /// guessing at what the walk is about to find.
    /// Refuse a `SELECT` whose source is a vault, and name the word that reads one.
    ///
    /// # What this refusal is and is not
    ///
    /// It is **not** a confidentiality control, and treating it as one would be
    /// the mistake. A `SELECT` over a vault that reached the records would
    /// answer with the sealed envelopes, because the envelope *is* the stored
    /// value — the exfiltration survey's whole finding. So nothing leaks if a
    /// path reaches records without passing here.
    ///
    /// What it buys is that the language means something. A caller who writes
    /// `SELECT * FROM team` and receives a column of opaque bytes concludes the
    /// vault is broken; one who is told to use `REVEAL` has learned the feature.
    ///
    /// # Where it is applied, and where it is not
    ///
    /// Here, at the three `SELECT` sources that name a table — which is exactly
    /// the form the design's refusal names. A graph traversal or a `FETCH` that
    /// lands on a vault record still returns ciphertext rather than this
    /// message. That gap is recorded as **Q-416** rather than closed by adding
    /// the call to every site that resolves a table: a refusal maintained by
    /// remembering to call it is the shape of thing this whole feature avoids,
    /// and the mechanical enumeration of read paths belongs to the negative
    /// matrix in W122, where it can be derived rather than recalled.
    pub(crate) fn refuse_reading_a_vault(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        table: &TableRef,
    ) -> Result<()> {
        let definition = Catalog::new(transaction).table(id)?;
        if definition.as_ref().is_some_and(TableDefinition::is_vault) {
            return Err(Error::NotReadBySelect {
                table: table.name.text.clone(),
                span: table.span,
            });
        }
        // A kept view's rows were computed already and cannot be redacted after
        // the fact, so a caller who may read only part of its source is refused
        // rather than answered from fields they may not see (ADR-0109 D7). Here
        // because this is asked at every source that reads a table.
        if let Some(kept) = self.materialized_view(transaction, id)?
            && self.visible_in(transaction, kept.source)?.is_some()
        {
            return Err(Error::MaterializedFromHidden {
                view: table.name.text.clone(),
                table: kept.understood.source.name.text.clone(),
                span: table.span,
            });
        }
        Ok(())
    }

    /// Refuse a read that needs records this node was not served (G031 S3.3).
    ///
    /// Asked beside [`Self::refuse_reading_a_vault`], at the sources that name
    /// a table and do not gather (G033): a join side and a `FETCH`. What is
    /// missing is decided by [`Self::missing`], which the gathered read asks
    /// too, so the two cannot disagree about what this node holds.
    pub(crate) fn refuse_reading_a_part(
        &self,
        transaction: &mut Transaction<'_>,
        id: TableId,
        part: Part<'_>,
    ) -> Result<()> {
        match self.missing(transaction, id, part)? {
            Some(missing) => Err(self.not_held_here(&missing)?),
            None => Ok(()),
        }
    }

    /// The records of `found` the condition keeps, tested as a local read
    /// tests them — over the redacted record, with the searched context and the
    /// read's notes.
    fn kept(
        &self,
        transaction: &mut Transaction<'_>,
        condition: &Expr,
        found: Vec<(tessari_types::RecordId, tessari_types::Value)>,
        searched: &Searched,
        reporting: Reporting<'_>,
    ) -> Result<Vec<(tessari_types::RecordId, tessari_types::Value)>> {
        let mut kept = Vec::new();
        for (record_id, record) in found {
            let held = self.evaluate_in(
                transaction,
                condition,
                Scope::searching(&record, searched)
                    .identified(&record_id)
                    .noticing(reporting.noticed),
            )?;
            if boolean(&held, condition.span)? {
                kept.push((record_id, record));
            }
        }
        Ok(kept)
    }

    pub(super) fn prepare_source<'a>(
        &self,
        transaction: &mut Transaction<'_>,
        select: &'a Select,
        reporting: Reporting<'_>,
        within: Option<Deadline>,
    ) -> Result<(Prepared<'a>, Searched)> {
        match &select.from {
            // A grouped read of a search: the ranked records, held, and folded
            // below like any other source (ADR-0105 D6). An ungrouped one never
            // reaches here — it answers through `search_answer`.
            Source::Search { .. } => {
                let (records, plan) =
                    self.search_records(transaction, select, reporting.noticed)?;
                Ok((Prepared::Held(records, plan), Searched::default()))
            }
            // Resolved from `meta` rather than read from a table, because that
            // is where it is: the identity is deliberately outside the log
            // (ADR-0018 §1). It needs no tenancy, so `$node` answers without a
            // `USE` — a node is not in a database.
            Source::Node => Ok((
                Prepared::Held(
                    vec![node_row(self.store)?],
                    Plan {
                        source: Some("node"),
                        ..Plan::new(AccessPath::Record)
                    },
                ),
                Searched::default(),
            )),
            Source::Record(target) => {
                let (_, address) = self.address(transaction, target)?;
                self.refuse_reading_a_vault(transaction, address.table, &target.table)?;
                if let Some((found, note)) = self.gather_a_part(
                    transaction,
                    address.table,
                    Part::Record(&address.id),
                    None,
                    None,
                    None,
                )? {
                    reporting.collected.push(note);
                    let visible = self.visible_in(transaction, address.table)?;
                    return Ok((
                        Prepared::Held(
                            self.records_of(found, &visible)?,
                            Plan::new(AccessPath::Record).on(target.table.name.text.as_str()),
                        ),
                        Searched::default(),
                    ));
                }
                // One record is still a member of its table's collection, so
                // a score or an explanation over it is measured against that
                // collection exactly as the table read measures it (Q-869).
                let searched = self.searched_for(transaction, address.table, &shown(select))?;
                let visible = self.visible_in(transaction, address.table)?;
                let found = match transaction.get(&address)? {
                    Some(payload) => {
                        vec![(address.id, self.record_of(&payload, &visible)?)]
                    }
                    None => Vec::new(),
                };
                Ok((
                    Prepared::Held(
                        found,
                        Plan::new(AccessPath::Record).on(target.table.name.text.as_str()),
                    ),
                    searched,
                ))
            }
            Source::Table(table) => {
                let (context, id) = self.resolve_readable_table(transaction, table)?;
                self.refuse_reading_a_vault(transaction, id, table)?;
                let searched = self.searched_for(transaction, id, &shown(select))?;
                let visible = self.visible_in(transaction, id)?;
                // ADR-0102: with no condition, a leader ranks exactly the
                // records this node would, so an ordered `LIMIT` travels.
                let ordered = super::shape_rules::travelling_order(select, &visible);
                if let Some((found, note)) = self.gather_a_part(
                    transaction,
                    id,
                    Part::Whole,
                    None,
                    super::shape_rules::held_bound(select),
                    ordered.as_ref(),
                )? {
                    reporting.collected.push(note);
                    return Ok((
                        Prepared::Held(
                            self.records_of(found, &visible)?,
                            Plan::new(AccessPath::Scan).on(table.name.text.as_str()),
                        ),
                        searched,
                    ));
                }
                Ok((Prepared::Table(context, id), searched))
            }
            // A walk between two positions in the table's own keyspace. The
            // records outside the span are not read, not decoded and not
            // tested, which is the whole difference between this and the same
            // question asked as a condition.
            Source::Range {
                table,
                lower,
                upper,
                inclusive,
                span,
            } => {
                let (context, id) = self.resolve_readable_table(transaction, table)?;
                self.refuse_reading_a_vault(transaction, id, table)?;
                let visible = self.visible_in(transaction, id)?;
                let part = Part::Span {
                    lower: lower.fixed(*span)?,
                    upper: upper.fixed(*span)?,
                    inclusive: *inclusive,
                };
                let plan = Plan::new(AccessPath::Span)
                    .on(table.name.text.as_str())
                    .touching(self.shards_touched(transaction, id, part)?);
                if let Some((found, note)) =
                    self.gather_a_part(transaction, id, part, None, None, None)?
                {
                    reporting.collected.push(note);
                    return Ok((
                        Prepared::Held(self.records_of(found, &visible)?, plan),
                        Searched::default(),
                    ));
                }
                let searched = self.searched_for(transaction, id, &shown(select))?;
                let found = transaction.records_in_span(
                    context.namespace,
                    context.database,
                    id,
                    lower.fixed(*span)?,
                    upper.fixed(*span)?,
                    *inclusive,
                )?;
                Ok((
                    Prepared::Held(self.records_of(found, &visible)?, plan),
                    searched,
                ))
            }
            Source::Traverse {
                from,
                direction,
                hops,
                depth,
            } => {
                let found = self.traverse(transaction, from, *direction, hops, *depth)?;
                Ok((
                    Prepared::Held(found, Plan::new(AccessPath::Graph)),
                    Searched::default(),
                ))
            }
            Source::Where { table, condition } => {
                let (context, id) = self.resolve_readable_table(transaction, table)?;
                // Ahead of the analyzer resolution below, so a `WHERE` naming a
                // secret field is refused for being a read of a vault rather
                // than for the shape of its condition — one refusal, and the one
                // that names the statement to use instead.
                self.refuse_reading_a_vault(transaction, id, table)?;
                // Resolved once for the query rather than once per record: which
                // analyzer a field carries is a property of the schema, and the
                // schema does not change under a read; nor does the collection a
                // score is measured against. Read before the access path is
                // chosen, because a search index needs the analyzer to turn the
                // query into the terms it holds.
                let mut expressions: Vec<&Expr> = vec![condition];
                expressions.extend(shown(select));
                let searched = self.searched_for(transaction, id, &expressions)?;
                // ADR-0097: the leader may narrow what it sends by the condition,
                // under this session's visibility. Every record that arrives is
                // still tested below, so the narrowing changes only what travels.
                let visible = self.visible_in(transaction, id)?;
                let pushed =
                    tessari_ql::portable(condition).map(|(condition, parameters)| crate::Pushed {
                        visible: visible.clone(),
                        condition,
                        parameters,
                    });
                // ADR-0096 D3: a condition fixing a partitioned table's field
                // reads that partition's span — here or on the one shard holding
                // it — and is still tested on every record the span holds.
                let region = self.partition_span(transaction, id, condition)?;
                let part = match &region {
                    Some((lower, upper)) => Part::Span {
                        lower,
                        upper,
                        inclusive: false,
                    },
                    None => Part::Whole,
                };
                let plan = match region {
                    Some(_) => Plan::new(AccessPath::Span).touching(self.shards_touched(
                        transaction,
                        id,
                        part,
                    )?),
                    None => Plan::new(AccessPath::Scan),
                }
                .on(table.name.text.as_str());
                if let Some((found, note)) = self.gather_a_part(
                    transaction,
                    id,
                    part,
                    pushed.as_ref(),
                    // Bounded only when the condition went with it: then the
                    // leader keeps exactly what this node keeps, and its first
                    // `n` are this node's first `n`.
                    pushed
                        .as_ref()
                        .and_then(|_| super::shape_rules::held_bound(select)),
                    // Ranked there only when the condition went with it, for
                    // the same reason (ADR-0102 D2).
                    pushed
                        .as_ref()
                        .and_then(|_| super::shape_rules::travelling_order(select, &visible))
                        .as_ref(),
                )? {
                    reporting.collected.push(note);
                    // Narrowed after the records are in hand, over the redacted
                    // record, exactly as a materialised source is: a hidden
                    // field is as absent to this condition as to a local scan.
                    let found = self.records_of(found, &visible)?;
                    let kept = self.kept(transaction, condition, found, &searched, reporting)?;
                    return Ok((Prepared::Held(kept, plan), searched));
                }
                if let Some((lower, upper)) = &region {
                    let found = transaction.records_in_span(
                        context.namespace,
                        context.database,
                        id,
                        lower,
                        upper,
                        false,
                    )?;
                    let found = self.records_of(found, &visible)?;
                    let kept = self.kept(transaction, condition, found, &searched, reporting)?;
                    return Ok((Prepared::Held(kept, plan), searched));
                }
                Ok((Prepared::Filtered(context, id, condition), searched))
            }
            Source::Join {
                left,
                right,
                left_key,
                right_key,
                asof: true,
                condition,
            } => {
                let (found, plan, searched) = self.asof_join(
                    transaction,
                    select,
                    (left, right),
                    (left_key, right_key),
                    condition.as_deref(),
                    reporting,
                )?;
                Ok((Prepared::Held(found, plan), searched))
            }
            Source::Join {
                left,
                right,
                left_key,
                right_key,
                asof: false,
                condition,
            } => {
                let (found, plan, searched) = self.join(
                    transaction,
                    select,
                    left,
                    right,
                    left_key,
                    right_key,
                    condition.as_deref(),
                    reporting,
                    within,
                )?;
                Ok((Prepared::Held(found, plan), searched))
            }
            // The inner read answers first and the outer statement reads what it
            // answered. The path reported is `materialised` and not the inner
            // read's own: the outer statement performed no access, and reporting
            // the inner one said this statement used an index when it read from
            // a held vector. The inner plan is a plan of its own, and one field
            // for it would describe only the shallowest case.
            Source::Subquery { read, condition } => {
                // The ceiling this read runs under, which for a materialised
                // source written by hand is **none**: the grammar refuses one
                // that names no `LIMIT`, so `Ceiling::over` sees the bound the
                // author wrote and declines to add a second.
                //
                // A view reaches here having never passed that rule. It is not
                // written in parentheses — it is a name the session replaced
                // with a read before anything was authorized — so the parser
                // could not have seen it, and a view naming no `LIMIT` would
                // otherwise hold its whole table. `Ceiling::over` is the answer
                // `budget.rs` already gives for the position the grammar cannot
                // reach, and this is the second one: past it the read is
                // **refused**, never truncated, so a view over a growing table
                // fails in a way somebody can see rather than answering a prefix
                // that looks whole.
                let ceiling = Ceiling::over(read);
                let inner = self.read(transaction, read, within, ceiling)?;
                reporting.collected.extend(inner.notes);
                reporting
                    .collected
                    .extend(ceiling_reached(read, inner.records.len()));
                // The ceiling the author did not write, approached rather than
                // reached. Only a read running under one is asked — a view
                // naming its own `LIMIT` runs under none, and a note telling its
                // author about a ceiling that does not apply to them would send
                // them to fix something that is not there.
                reporting
                    .collected
                    .extend(ceiling.and_then(|ceiling| ceiling.nearing(inner.records.len())));
                let (found, plan) = (inner.records, Plan::new(AccessPath::Materialised));
                let Some(condition) = condition else {
                    return Ok((Prepared::Held(found, plan), Searched::default()));
                };
                // Narrowed here rather than by an access path: the records are
                // already in hand, so there is nothing left for an index to
                // choose. That is what lets this condition ask about a value the
                // inner read *produced* — a projected name, a fold's result —
                // which no condition inside it could have named.
                let searched = Searched::default();
                let mut kept = Vec::new();
                for (id, record) in found {
                    let held = self.evaluate_in(
                        transaction,
                        condition,
                        Scope::searching(&record, &searched)
                            .identified(&id)
                            .noticing(reporting.noticed),
                    )?;
                    if boolean(&held, condition.span)? {
                        kept.push((id, record));
                    }
                }
                Ok((Prepared::Held(kept, plan), searched))
            }
        }
    }
}
