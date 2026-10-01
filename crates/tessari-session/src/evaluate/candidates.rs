//! The candidates an index offers, and the walks that confirm them.

use std::ops::ControlFlow;

use tessari_ql::Expr;
use tessari_storage::{Catalog, RecordAddress, Transaction};
use tessari_types::{TableId, Value};

use crate::condition::boolean;
use crate::consume::Consumer;
use crate::error::Result;
use crate::plan;
use crate::search::Searched;
use crate::session::Session;

use super::{Approximated, Asked, Candidates, Reached, Scope, Testing, Walked};

impl Session<'_> {
    /// The records worth testing, and how they were reached.
    ///
    /// Three steps that used to be one loop: enumerate every conjunct an index
    /// could serve, choose the one that promises to narrow the most
    /// ([`crate::plan`] owns that rule), and execute only the winner. Taking the
    /// first servable conjunct was never a wrong answer — the candidates are
    /// re-tested against the whole condition below — but it was a wrong cost,
    /// decided by where the author happened to put a clause.
    ///
    /// Which one runs is still decided by what exists and never by how the query
    /// was written; that now includes not being decided by the *order* it was
    /// written in.
    ///
    /// # The third value is whether the condition has already been answered
    ///
    /// `Reached::answered` is `false` for every read this store has ever done:
    /// the records are candidates and the condition decides. `true` is the
    /// narrow case where the read *is* the answer — see [`Session::trusts`] for
    /// what has to hold before it can be said, and `plan::Candidate::answers`
    /// for the part of it the planner contributes.
    pub(super) fn candidates(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        context: crate::context::Context,
        condition: &Expr,
        searched: &Searched,
        asked: Asked<'_>,
    ) -> Result<Option<Reached>> {
        // Once for the statement rather than once per conjunct: which indexes a
        // table carries is one question, and it used to be asked as many times
        // as the condition had clauses.
        //
        // An empty list rather than a check at the choice below, because this is
        // the whole vocabulary the enumeration works from: an index the read
        // cannot use is one this statement does not have, and saying so here
        // keeps the reason in the one place that asks the catalog rather than
        // spread across every candidate that could have been built from it.
        let declared = if transaction.indexes_are_current()? {
            Catalog::new(transaction).field_indexes_on(table)?
        } else {
            Vec::new()
        };
        let offered = self.enumerate(transaction, condition, &declared, searched)?;

        // Resolved before either path, so an index-served read and a scan see
        // the same record — which is what makes candidates re-tested against the
        // whole condition unable to answer what a scan refuses.
        let visible = self.visible_in(transaction, table)?;
        // Ranking says which index narrows most; it does not say whether the
        // winner narrows enough to be worth reading. An index that produces
        // most of the table pays an entry walk on top of a fetch it did not
        // shorten, so the winner is measured against the table before it is
        // served — see `plan::worth_serving`, which `EXPLAIN` asks too so that
        // the reported path is the one the read takes.
        let chosen = match plan::choose(offered) {
            Some(candidate)
                if plan::worth_serving(transaction, table, &candidate, asked.lift_scan_guard)? =>
            {
                Some(candidate)
            }
            _ => None,
        };
        if let Some(chosen) = chosen {
            // Built by the candidate itself, which is the same function
            // `EXPLAIN` calls on the candidate its own `choose` returned. The
            // two report one structure because one function writes it.
            let plan = chosen.plan(asked.named);
            let answered = self.trusts(condition, &chosen, &visible);
            // The field's, resolved once for the statement while the analyzers
            // were being read — the same value the query's terms were built
            // from, which is what keeps the index's half of a search and the
            // predicate's half asking one question.
            let analyzer = chosen
                .index
                .fields
                .first()
                .and_then(|path| searched.analyzer(path));
            // A range is handed back as the range itself rather than as its
            // records, so the caller's `Break` can reach the fetch. Every other
            // shape is served here and built whole — see [`Candidates`] for why
            // the entry walk is never the half that stops.
            let records = if let plan::Served::Range {
                fixed,
                lower,
                upper,
            } = &chosen.served
            {
                Candidates::Range {
                    index: Box::new(chosen.index.clone()),
                    fixed: fixed.clone(),
                    lower: lower.clone(),
                    upper: upper.clone(),
                }
            } else {
                let found = self.serve(transaction, context, table, &chosen, analyzer)?;
                Candidates::Held(self.records_of(found, &visible)?)
            };
            return Ok(Some(Reached {
                records,
                plan,
                answered,
            }));
        }
        // No conjunct serves the condition; every side of an `OR` may
        // (G051 T7.2). The union is deduplicated in identity order, the order a
        // scan would meet the same records in.
        if let Some(sides) = self.union_of(
            transaction,
            table,
            condition,
            (&declared, searched),
            asked.lift_scan_guard,
        )? {
            let mut found = std::collections::BTreeMap::new();
            for side in &sides {
                let analyzer = side
                    .index
                    .fields
                    .first()
                    .and_then(|path| searched.analyzer(path));
                found.extend(self.serve(transaction, context, table, side, analyzer)?);
            }
            return Ok(Some(Reached {
                records: Candidates::Held(self.records_of(found.into_iter().collect(), &visible)?),
                plan: plan::union_plan(&sides, asked.named),
                answered: false,
            }));
        }
        // Nothing serves the condition. The caller scans, and it walks the table
        // rather than reading it — see `Session::scan_matching`, which is where
        // the records would otherwise have been materialised before the first
        // one could be tested.
        Ok(None)
    }

    /// Every record of a table that the condition accepts, walked.
    ///
    /// The scan an unserved condition falls back to, and the one source in this
    /// store that hands records over **as it finds them**. A read whose answer
    /// count is its `LIMIT` pushes that bound into the source (ADR-0013); a read
    /// with a `WHERE` cannot, because the bound counts records that match and
    /// the source counts records that exist. What stops this one is the `Break`
    /// the consumer already returns — the same mechanism, reaching the same
    /// place, by the only route a condition leaves open.
    ///
    /// The answer is still materialised above, so ADR-0013's refusal of
    /// streaming is untouched: no value is handed to a caller while a snapshot
    /// is open, and the snapshot's life is shorter because the walk stops.
    ///
    /// The record is redacted before the condition sees it, which is what makes
    /// a scan and an index-served read answer the same records: a field this
    /// session may not read resolves to `NONE` for both.
    pub(super) fn scan_matching(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        testing: Testing<'_>,
        consumer: &mut dyn Consumer,
    ) -> Result<()> {
        let Testing {
            condition,
            searched,
            noticed,
        } = testing;
        let visible = self.visible_in(transaction, table)?;
        transaction.walk_table(
            context.namespace,
            context.database,
            table,
            |transaction, id, payload| {
                let record = self.record_of(&payload, &visible)?;
                let held = self.evaluate_in(
                    transaction,
                    condition,
                    Scope::searching(&record, searched)
                        .identified(&id)
                        .noticing(noticed),
                )?;
                if !boolean(&held, condition.span)? {
                    return Ok(ControlFlow::Continue(()));
                }
                consumer.take(transaction, id, record)
            },
        )
    }

    /// Whether this read may be believed instead of re-tested.
    ///
    /// The planner said what a candidate *can* promise about one clause
    /// (`plan::Candidate::answers`). Two things it cannot know are decided
    /// here, and each of them is a way the re-test is load-bearing today.
    ///
    /// **The clause has to be the whole condition.** A candidate answers one
    /// conjunct; anything joined to it with `AND` still has to be evaluated, and
    /// evaluating it re-analyses the same text anyway. Span equality is the
    /// test because `plan::conjunct::seekable` descends into `AND`: a clause
    /// under one has a span strictly inside the condition's, and a lone clause
    /// has the condition's own. It is a comparison rather than a re-parse, so
    /// the two sides cannot drift.
    ///
    /// **The field has to be one this session may read.** Field-level redaction
    /// on an index-served read is enforced *by the re-test* — the record the
    /// candidate is re-tested against is the redacted one, so a field nobody
    /// granted resolves to `NONE` and the comparison is false (`crate::redact`).
    /// Believing the index instead would let a reader search a field they cannot
    /// see, and learn its contents one query at a time. This is the condition
    /// that makes the change a security decision rather than a timing one.
    ///
    /// Two more are already true wherever this is reached, and are enforced
    /// where the indexes are enumerated rather than repeated here. An index is
    /// only offered when `Transaction::indexes_are_current` holds — entries
    /// carry no version, so at an older snapshot a posting could describe a
    /// newer value than the reader is entitled to see. And a **search** index is
    /// withheld entirely while this transaction holds writes on the table, so a
    /// candidate reaching this point cannot be answering from a posting the
    /// transaction has already written past.
    pub(super) fn trusts(
        &self,
        condition: &Expr,
        chosen: &plan::Candidate,
        visible: &crate::redact::Visible,
    ) -> bool {
        chosen.answers == Some(condition.span)
            && chosen.index.fields.first().is_some_and(|path| {
                // Top-level, because a redaction is: a grant names a field of a
                // table, and what is removed is the whole field. A route into an
                // object this session may read reaches an object nothing took
                // anything out of.
                visible
                    .as_ref()
                    .is_none_or(|fields| fields.contains(path.root()))
            })
    }

    /// Walk a vector index, when there is one that answers this read.
    ///
    /// `None` is the scan, which is exact, and there are five ways to reach it:
    /// no index on the path, one built for another distance, a query that is not
    /// a vector, a field this caller's grant does not contain, and a table this
    /// transaction has written to without committing.
    ///
    /// A sixth is made one level up and is not repeated here: `index_on_path`
    /// refuses every index while `Transaction::indexes_are_current` is false, so
    /// a reader at an older snapshot never reaches the graph. Entries carry no
    /// version, so being answered from a newer graph is not approximation, it is
    /// reading someone else's present.
    ///
    /// That last refusal is the same one [`Evaluator::index_serving_place`],
    /// [`Evaluator::index_serving_score`] and [`Evaluator::index_serving_order`]
    /// each make first, and it was missing here until W187. A field permission
    /// removes the field *before* anything reads the record, so a caller without
    /// it sorts by `none` and the read is unordered; the graph, asked anyway,
    /// answers with the records nearest a vector that caller may not read. The
    /// harm is larger than the order the other three walks refuse to disclose,
    /// because the `none` key then re-sorts those records into identity order:
    /// what arrives looks like an ordinary unordered result and is a statement
    /// of **membership** about a hidden field, handed over in a single read.
    ///
    /// The uncommitted-write refusal was the last of the four and was added in
    /// W188 for a different reason than the others. `APPROXIMATE` is a contract
    /// the caller opted into, and under it "the graph missed it" is an answer
    /// this walk is allowed to give. "This transaction cannot see its own write"
    /// is not: entries are derived at commit, so a record written here has none,
    /// and the graph answers as though it did not exist — measured as an exact
    /// read of `[99, 39, 38]` against an approximate `[39, 38, 37]` for the same
    /// statement in the same transaction. Isolation is not what `APPROXIMATE`
    /// relaxes.
    pub(super) fn walk(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Nearest<'_>,
    ) -> Result<Option<Approximated>> {
        let Some(index) = self.index_on_path(transaction, table, wanted.path)? else {
            return Ok(None);
        };
        let Some(declared) = index.vector else {
            return Ok(None);
        };
        if !plan::answers(declared, wanted.distance) {
            return Ok(None);
        }
        let Some(query) = tessari_storage::vector_of(&self.evaluate(transaction, wanted.query)?)
        else {
            return Ok(None);
        };
        let visible = self.visible_in(transaction, table)?;
        // Asked before the graph is walked and not after: the redaction below
        // removes the field from the records, and would leave the caller holding
        // the graph's choice of which records to return.
        if visible
            .as_ref()
            .is_some_and(|fields| !fields.contains(wanted.path.root()))
        {
            return Ok(None);
        }
        // The graph is built from committed entries, so a record this
        // transaction has written is not in it and would be missing from this
        // transaction's own read.
        if transaction.writes_in(context.namespace, context.database, table) {
            return Ok(None);
        }
        let mut rows = Vec::new();
        for id in transaction.records_by_vector(&index, &query, wanted.wanted, wanted.effort)? {
            // Resolved at this reader's own snapshot, like every index read, so
            // a node left behind by a deleted record produces nothing.
            let at = RecordAddress::new(context.namespace, context.database, table, id);
            if let Some(payload) = transaction.get(&at)? {
                rows.push((at.id, self.record_of(&payload, &visible)?));
            }
        }
        Ok(Some((rows, index.name)))
    }

    /// Walk a spatial index nearest-first, when there is one that answers this
    /// read.
    ///
    /// `None` is the scan, and every `None` here is a way the order a walk
    /// produces can differ from the order the read must answer in. The first
    /// four are the same four an ordered walk refuses, and for the same reason:
    /// an ordering has nothing to re-test, because the entry's **position** is
    /// the answer rather than a candidate for one.
    ///
    /// - **no spatial index on that field.** An ordered index holds values and a
    ///   vector index holds a graph; neither is stored by place.
    /// - **the field is not visible to this caller.** A field permission removes
    ///   the field before anything reads the record, so a caller without it
    ///   sorts by `none`. An order taken from the index would sort by the
    ///   geometries themselves — the ordering disclosing what the projection
    ///   hides, one comparison at a time.
    /// - **this transaction has written to the table.** Entries are derived at
    ///   commit, so an uncommitted record has none and the walk cannot place it.
    /// - **the snapshot is not the committed tail.** Entries hold the current
    ///   state and carry no version, so a record moved since the snapshot sits
    ///   in the index at a place this reader cannot see, and the answer comes
    ///   back in the **wrong order** rather than short.
    ///
    /// The query position is the last: the walk ranks from a position, and a
    /// larger query shape is measured by the scan. [`Transaction::records_by_place`] owns the three
    /// remaining refusals, which are facts about the records rather than about
    /// the session.
    pub(super) fn walk_to_place(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Closest<'_>,
    ) -> Result<Walked> {
        let Some((index, visible)) =
            self.index_serving_place(transaction, context, table, wanted.path)?
        else {
            return Ok(Walked::NotServed);
        };
        // Not a decline: a query that is not a point, or a point no cell can
        // hold, is a statement this walk does not serve rather than an index
        // that ran out.
        let Value::Geometry(tessari_types::Geometry::Point(position)) =
            self.evaluate(transaction, wanted.query)?
        else {
            return Ok(Walked::NotServed);
        };
        let Ok(target) = tessari_geo::Snapped::of(position) else {
            return Ok(Walked::NotServed);
        };
        let Some(nearby) = transaction.records_by_place(&index, target, wanted.wanted)? else {
            return Ok(Walked::Declined);
        };
        Ok(Walked::Served {
            found: self.records_of(nearby.rows, &visible)?,
            index: index.name,
        })
    }
}
