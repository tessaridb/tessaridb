//! Deciding how a read reaches its records, and refusing the reads a caller may not make.

use tessari_ql::{Expr, Select, Source, TableRef};
use tessari_storage::{Catalog, Transaction};
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
        if Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| definition.is_vault())
        {
            return Err(Error::NotReadBySelect {
                table: table.name.text.clone(),
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
            Some(missing) => Err(missing.refusal()),
            None => Ok(()),
        }
    }

    pub(super) fn prepare_source<'a>(
        &self,
        transaction: &mut Transaction<'_>,
        select: &'a Select,
        reporting: Reporting<'_>,
        within: Option<Deadline>,
    ) -> Result<(Prepared<'a>, Searched)> {
        match &select.from {
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
                if let Some((found, note)) =
                    self.gather_a_part(transaction, address.table, Part::Record(&address.id))?
                {
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
                    Searched::default(),
                ))
            }
            Source::Table(table) => {
                let (context, id) = self.resolve_table(transaction, table)?;
                self.refuse_reading_a_vault(transaction, id, table)?;
                let searched = self.searched_for(transaction, id, &shown(select))?;
                if let Some((found, note)) = self.gather_a_part(transaction, id, Part::Whole)? {
                    reporting.collected.push(note);
                    let visible = self.visible_in(transaction, id)?;
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
                let (context, id) = self.resolve_table(transaction, table)?;
                self.refuse_reading_a_vault(transaction, id, table)?;
                let visible = self.visible_in(transaction, id)?;
                if let Some((found, note)) = self.gather_a_part(
                    transaction,
                    id,
                    Part::Span {
                        lower: lower.fixed(*span)?,
                        upper: upper.fixed(*span)?,
                        inclusive: *inclusive,
                    },
                )? {
                    reporting.collected.push(note);
                    return Ok((
                        Prepared::Held(
                            self.records_of(found, &visible)?,
                            Plan::new(AccessPath::Span).on(table.name.text.as_str()),
                        ),
                        Searched::default(),
                    ));
                }
                let found = transaction.records_in_span(
                    context.namespace,
                    context.database,
                    id,
                    lower.fixed(*span)?,
                    upper.fixed(*span)?,
                    *inclusive,
                )?;
                Ok((
                    Prepared::Held(
                        self.records_of(found, &visible)?,
                        Plan::new(AccessPath::Span).on(table.name.text.as_str()),
                    ),
                    Searched::default(),
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
                let (context, id) = self.resolve_table(transaction, table)?;
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
                if let Some((found, note)) = self.gather_a_part(transaction, id, Part::Whole)? {
                    reporting.collected.push(note);
                    let visible = self.visible_in(transaction, id)?;
                    // Narrowed after the records are in hand, over the redacted
                    // record, exactly as a materialised source is: a hidden
                    // field is as absent to this condition as to a local scan.
                    let mut kept = Vec::new();
                    for (record_id, record) in self.records_of(found, &visible)? {
                        let held = self.evaluate_in(
                            transaction,
                            condition,
                            Scope::searching(&record, &searched)
                                .identified(&record_id)
                                .noticing(reporting.noticed),
                        )?;
                        if boolean(&held, condition.span)? {
                            kept.push((record_id, record));
                        }
                    }
                    return Ok((
                        Prepared::Held(
                            kept,
                            Plan::new(AccessPath::Scan).on(table.name.text.as_str()),
                        ),
                        searched,
                    ));
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
