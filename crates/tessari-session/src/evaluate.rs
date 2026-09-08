//! Turning an expression into a value.
//!
//! Two of the forms are reads, and that is the whole of what makes the models
//! compose. A `GET` inside a record statement runs **in the same transaction**,
//! so it sees the same snapshot as the statement around it — two models that
//! cannot share a snapshot are two databases sharing a process.

use core::ops::Bound;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use tessari_constants::ORDERED_FILTER_REACH;
use tessari_encoding::{Direction as AdjacencyDirection, Posting};
use tessari_ql::{
    BinaryOp, DeleteBound, Direction, Expr, ExprKind, FieldPath, Function, Hop, Identity, JoinSide,
    Projected, Projection, RecordTarget, Select, Source, Span, TableRef, Using,
};
use tessari_storage::{
    BUILD_VERSION, Catalog, EDGE_IN, EDGE_OUT, IndexDefinition, RecordAddress, Store, Transaction,
};
use tessari_types::{
    Analyzer, Number, Path, RecordId, RecordRef, Step, TableId, Value, ValueRange, apply,
};

use crate::aggregate::folds;
use crate::arithmetic::{arithmetic, negate};
use crate::budget::{Budget, Ceiling, Deadline};
use crate::call::call;
use crate::condition::boolean;
use crate::consume::Consumer;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::noticed::Noticed;
use crate::outcome::{AccessPath, Note, Suggestion};
use crate::plan;
use crate::plan::Plan;
use crate::rank::{self, Held, score};
use crate::search::{
    Ranked, Searched, marked, matches_fuzzy_terms, matches_prefix_terms, matches_terms,
};
use crate::session::Session;

/// How many of an index's leading values an index-served order compares.
///
/// One, because `plan::ordered` refuses a second sort key — so the field the
/// order names is the only one a tie group may be defined by. It is the width of
/// the comparison rather than a tunable, which is why it lives here beside the
/// reads that use it and not in `tessari-constants`.
///
/// The number matters because it decides what a *tie* is. On a single-field
/// index it is every value the entry holds; on `(last, first)` ordered by `last`
/// it is the first of two, and comparing both instead would make every entry its
/// own group — so nothing would ever drain, and a bound cut inside a group of
/// equal `last` would answer with the wrong members and raise nothing.
const ORDERED_LEADING_FIELDS: usize = 1;

impl Session<'_> {
    /// The value an expression denotes, with no record in scope.
    pub(crate) fn evaluate(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Value> {
        self.evaluate_in(transaction, expr, Scope::default())
    }

    /// The value an expression denotes, against the record being tested.
    ///
    /// The scope is what separates a condition from a value: a path is only
    /// meaningful when there **is** a record, and in a value position there is
    /// not — which is why `CREATE audit:1 = { subject: users }` writes a table
    /// and `WHERE users = 3` reads a field.
    pub(crate) fn evaluate_in(
        &self,
        transaction: &mut Transaction<'_>,
        expr: &Expr,
        scope: Scope<'_>,
    ) -> Result<Value> {
        match &expr.kind {
            ExprKind::Path(field) => {
                let Some(record) = scope.record else {
                    return Err(Error::NoRecordInScope { span: field.span });
                };
                // A route that reaches nothing **is** `none`: the field is not
                // there, which is precisely what `none` says. That is what makes
                // `WHERE email = NONE` find the records without an email
                // without the language needing an `IS NULL` operator at all.
                Ok(field.path.resolve(record).cloned().unwrap_or(Value::None))
            }
            // A fold is replaced by the value it produced before the enclosing
            // expression is evaluated, so one reaching here is one that stood
            // somewhere a fold may not stand. The parser refuses those, which
            // makes this the arm that says the parser is the only gate — and
            // says it out loud rather than by a wildcard that would quietly
            // answer `none`.
            ExprKind::Fold { span, .. } => Err(Error::FoldOutsideAGroup { span: *span }),
            ExprKind::Not(operand) => {
                let held = self.evaluate_in(transaction, operand, scope)?;
                Ok(Value::Bool(!boolean(&held, operand.span)?))
            }
            // Short-circuit: the right side is not evaluated when the left
            // already decides. It is not only a saving — it is what lets
            // `x = NONE OR x.y = 1` be written without the second half having to
            // be meaningful for every record.
            ExprKind::And(left, right) => {
                let held = self.evaluate_in(transaction, left, scope)?;
                if !boolean(&held, left.span)? {
                    return Ok(Value::Bool(false));
                }
                let held = self.evaluate_in(transaction, right, scope)?;
                Ok(Value::Bool(boolean(&held, right.span)?))
            }
            ExprKind::Or(left, right) => {
                let held = self.evaluate_in(transaction, left, scope)?;
                if boolean(&held, left.span)? {
                    return Ok(Value::Bool(true));
                }
                let held = self.evaluate_in(transaction, right, scope)?;
                Ok(Value::Bool(boolean(&held, right.span)?))
            }
            // Only the arm that is taken is evaluated. That is not only a
            // saving — it is what lets the untaken arm be a read, or an
            // arithmetic that would fail on this record, without the statement
            // having to be meaningful for every record it passes over.
            ExprKind::If {
                condition,
                then,
                otherwise,
            } => {
                let held = self.evaluate_in(transaction, condition, scope)?;
                if boolean(&held, condition.span)? {
                    return self.evaluate_in(transaction, then, scope);
                }
                match otherwise {
                    Some(otherwise) => self.evaluate_in(transaction, otherwise, scope),
                    // No `ELSE` answers `none`, which is what a route into a
                    // field the record does not have already answers.
                    None => Ok(Value::None),
                }
            }
            // `NONE` and `NULL` both count as holding nothing here, and this is
            // the one place the language treats them alike — the question `??`
            // asks is whether there is a value to use, and the answer is no in
            // both cases. The right side is evaluated only when it is needed.
            ExprKind::Coalesce(left, right) => {
                let held = self.evaluate_in(transaction, left, scope)?;
                if matches!(held, Value::None | Value::Null) {
                    return self.evaluate_in(transaction, right, scope);
                }
                Ok(held)
            }
            ExprKind::Negate(operand) => {
                let held = self.evaluate_in(transaction, operand, scope)?;
                negate(&held, operand.span)
            }
            ExprKind::Call {
                function,
                arguments,
                span,
            } => {
                // A score is the second thing in this language that needs more
                // than its arguments — the field's analyzer, and what the
                // collection looks like. `call` takes values, and neither of
                // those is one, so it is answered here where the scope is.
                if *function == Function::SearchScore {
                    return self.rank(transaction, arguments, scope, *span);
                }
                // And the third. A highlight needs the field's analyzer and what
                // this read asked of that field — neither of which is a value,
                // and the second of which is the whole point: the marks come
                // from the query the statement ran, not from one the projection
                // repeated.
                if *function == Function::SearchHighlight {
                    return self.highlight(transaction, arguments, scope);
                }
                let arguments = self.values(transaction, arguments, scope)?;
                call(*function, &arguments, *span)
            }
            ExprKind::Arithmetic { op, left, right } => {
                let held = self.evaluate_in(transaction, left, scope)?;
                let other = self.evaluate_in(transaction, right, scope)?;
                arithmetic(*op, &held, &other, expr.span)
            }
            ExprKind::Binary { op, left, right } => {
                let other = self.evaluate_in(transaction, right, scope)?;
                // A route holding `[*]` denotes *the values it reaches* rather
                // than a value, and what a comparison does with several values
                // is the comparison's rule: it holds when **any** of them
                // satisfies it. That is what makes an array queryable at all,
                // and it is why `[*]` needs no `Value` of its own — several-ness
                // belongs to the route.
                //
                // Nothing reached is `false`, not an error: an empty array, a
                // field holding a single value, a record with no such field. A
                // record that does not match is a record that does not match.
                if let ExprKind::Path(field) = &left.kind
                    && field.path.is_several()
                {
                    let Some(record) = scope.record else {
                        return Err(Error::NoRecordInScope { span: field.span });
                    };
                    let analyzer = scope.analyzer(&field.path);
                    let held = field.path.reach(record);
                    return Ok(Value::Bool(held.into_iter().any(|value| match *op {
                        BinaryOp::Matches => matches_terms(analyzer, value, &other),
                        BinaryOp::MatchesPrefix => matches_prefix_terms(analyzer, value, &other),
                        BinaryOp::MatchesFuzzy => matches_fuzzy_terms(analyzer, value, &other),
                        held_op => {
                            scope.compared(value, &other);
                            apply(held_op, value, &other)
                        }
                    })));
                }
                let held = self.evaluate_in(transaction, left, scope)?;
                // A term match is the one test that needs the *schema*: which
                // analyzer turns this field's text into terms is a property of
                // the field, so that both a scan and an index ask the same
                // question of it.
                if matches!(
                    *op,
                    BinaryOp::Matches | BinaryOp::MatchesPrefix | BinaryOp::MatchesFuzzy
                ) {
                    let analyzer = match &left.kind {
                        ExprKind::Path(field) => scope.analyzer(&field.path),
                        _ => None,
                    };
                    return Ok(Value::Bool(match *op {
                        BinaryOp::MatchesPrefix => matches_prefix_terms(analyzer, &held, &other),
                        BinaryOp::MatchesFuzzy => matches_fuzzy_terms(analyzer, &held, &other),
                        _ => matches_terms(analyzer, &held, &other),
                    }));
                }
                // Beside the comparison rather than inside it: what an operator
                // means once both sides are values belongs to `tessari_types`,
                // which the store's `ASSERT` path shares and which has no notes
                // and should not grow any.
                scope.compared(&held, &other);
                Ok(Value::Bool(apply(*op, &held, &other)))
            }
            ExprKind::Literal(value) => Ok(value.clone()),
            // Binding replaces every parameter in a script before its first
            // statement runs, so the only way one arrives here is from an
            // expression that was **stored** — a field's `DEFAULT` — and a
            // stored expression belongs to no call, so nothing could have bound
            // it. Refused with that said, rather than treated as absent.
            ExprKind::Parameter(name) => Err(Error::ParameterHasNoValue {
                name: name.clone(),
                span: expr.span,
            }),
            ExprKind::Table(table) => {
                let (_, id) = self.resolve_table(transaction, table)?;
                Ok(Value::Table(id))
            }
            ExprKind::Record(target) => {
                let (_, address) = self.address(transaction, target)?;
                Ok(Value::Record(RecordRef::new(address.table, address.id)))
            }
            ExprKind::Array(items) => Ok(Value::Array(self.values(transaction, items, scope)?)),
            ExprKind::Set(items) => Ok(Value::Set(
                self.values(transaction, items, scope)?
                    .into_iter()
                    .collect(),
            )),
            ExprKind::Object(fields) => {
                let mut object = BTreeMap::new();
                for field in fields {
                    let value = self.evaluate_in(transaction, &field.value, scope)?;
                    object.insert(field.name.text.clone(), value);
                }
                Ok(Value::Object(object))
            }
            ExprKind::Range(range) => {
                let start = self.evaluate_in(transaction, &range.start, scope)?;
                let end = self.evaluate_in(transaction, &range.end, scope)?;
                let end = if range.inclusive {
                    Bound::Included(end)
                } else {
                    Bound::Excluded(end)
                };
                Ok(Value::Range(Box::new(ValueRange::new(
                    Bound::Included(start),
                    end,
                ))))
            }
            ExprKind::Get(target) => self.read_key(transaction, target),
            ExprKind::Select(select) => self.read_as_value(transaction, select),
        }
    }

    fn values(
        &self,
        transaction: &mut Transaction<'_>,
        items: &[Expr],
        scope: Scope<'_>,
    ) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            values.push(self.evaluate_in(transaction, item, scope)?);
        }
        Ok(values)
    }

    /// Where a record lives, once its table name is resolved.
    pub(crate) fn address(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<(crate::context::Context, RecordAddress)> {
        let (context, table) = self.resolve_table(transaction, &target.table)?;
        Ok((
            context,
            RecordAddress::new(
                context.namespace,
                context.database,
                table,
                target.id.fixed(target.span)?.clone(),
            ),
        ))
    }

    /// The value under a key, or [`Value::None`] when there is nothing there.
    pub(crate) fn read_key(
        &self,
        transaction: &mut Transaction<'_>,
        target: &RecordTarget,
    ) -> Result<Value> {
        let (_, address) = self.address(transaction, target)?;
        let visible = self.visible_in(transaction, address.table)?;
        match transaction.get(&address)? {
            Some(payload) => self.record_of(&payload, &visible),
            None => Ok(Value::None),
        }
    }

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
            match self.candidates(transaction, id, context, condition, &searched, None)? {
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
            transaction.delete(RecordAddress::new(
                context.namespace,
                context.database,
                id,
                record_id,
            ));
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
        let found = transaction.records_in_span(
            context.namespace,
            context.database,
            id,
            span.lower.fixed(span.at)?,
            span.upper.fixed(span.at)?,
            span.inclusive,
        )?;

        let mut removed = 0_u64;
        for (record_id, _) in found {
            if removed >= ceiling {
                break;
            }
            transaction.delete(RecordAddress::new(
                context.namespace,
                context.database,
                id,
                record_id,
            ));
            removed = removed.saturating_add(1);
        }
        Ok(crate::outcome::Outcome::Removed { count: removed })
    }

    /// A read, as the records it found, shaped by what it asked for.
    ///
    /// The projection is applied here — once, above the four sources — so a
    /// record read by identity, by scan, by index and by traversal all answer in
    /// the same shape. Four applications would be four chances for one of them
    /// to differ.
    ///
    /// It does not change the access path. A projection that an index could
    /// answer without touching the record is a covering read, which is a
    /// planner's decision about how to run the statement rather than a change to
    /// what the statement says.
    pub(crate) fn read(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        within: Option<Deadline>,
        holding: Option<Ceiling>,
    ) -> Result<Answered> {
        // The searched context is resolved before the source produces anything,
        // because a sort key is an expression too and one holding a `MATCHES` or
        // a score must mean the same thing there as it does in the `WHERE` that
        // produced them. That is the whole reason the source is split in two: it
        // knew `searched` before it walked, and only reported it on return, so
        // the consumer could not be built until every record already existed.
        // The one note channel for the whole read, threaded rather than kept on
        // the session: a buffer on `self` would outlive the statement that filled
        // it, and a note reported against the *next* answer is worse than no note
        // at all.
        let mut notes = Vec::new();
        // Narrowed by this statement's own clause, and never widened by it: a
        // subquery may set a tighter ceiling than the read holding it and may
        // not set a looser one.
        let within = Deadline::under(within, select.timeout);
        // One budget for the whole read, so a two-stage path counts each record
        // once and the refusal says how far the *read* got. `holding` comes from
        // the caller and never from this statement: how long a read may take is
        // the statement's own business, but whether it is being built into
        // memory for somebody else is a fact only its caller knows.
        let mut budget = Budget::of(within, holding);
        // Owned by the read and borrowed by everything it evaluates, so the
        // lifetime rather than the discipline is what stops a note outliving the
        // statement that earned it.
        let noticed = Noticed::default();
        let (prepared, searched) = self.prepare_source(
            transaction,
            select,
            Reporting {
                collected: &mut notes,
                noticed: &noticed,
            },
            within,
        )?;
        // The bound the sort may keep to. `bounded` is applied to the ordering
        // stage's output below, so keeping only what it will keep is an identity
        // between two adjacent stages rather than a decision about the
        // statement — which is why, unlike the bound handed to the source, this
        // one needs no whitelist of shapes (ADR-0013).
        let bound = order_bound(select);
        // Resolved before the source runs, because the ordering stage has to
        // hold it before the first record arrives — a cursor applied to the
        // finished answer would let the bound keep the records the page has
        // already handed back and then throw them away.
        let resuming = self.resuming(transaction, select)?;
        // The seek reaches the source itself, so only the walk has anything to
        // report. Pushed here rather than beside the seek because it is a
        // property of the statement's shape, which is decided once.
        if select.after.is_some() && !sought(select) {
            notes.push(Note::CursorWalked);
        }

        if streams(select) {
            // The projection folded once, above the records rather than per
            // record: a projection's constant parts are constant across every
            // record it is applied to.
            let wanted = self.shaped(transaction, select)?;
            let keys = self.folded_order(transaction, &select.order)?;
            let mut shaping = crate::consume::Shaping::new(
                self,
                wanted,
                keys,
                &searched,
                crate::shape::Topmost::keeping(&select.order, bound),
                &mut budget,
                &noticed,
            );
            if let Some((id, record)) = resuming {
                shaping.resume_after(transaction, id, record)?;
            }
            let plan = self.produce_source(
                transaction,
                select,
                prepared,
                &searched,
                &mut shaping,
                Reporting {
                    collected: &mut notes,
                    noticed: &noticed,
                },
            )?;
            asserted(select, &plan)?;
            notes.extend(noticed.drained());
            let records = crate::shape::bounded(shaping.finish(), select.start, select.limit);
            alone(select, &records)?;
            return Ok(Answered {
                records,
                plan,
                notes,
                suggestion: searched.suggestion(),
            });
        }

        let mut collecting = crate::consume::Collecting::new(&mut budget, held_bound(select));
        let plan = self.produce_source(
            transaction,
            select,
            prepared,
            &searched,
            &mut collecting,
            Reporting {
                collected: &mut notes,
                noticed: &noticed,
            },
        )?;
        let mut records = collecting.finish();
        // Before anything groups, projects or sorts, so a projection and a sort
        // key both see the record rather than the reference that named it.
        if !select.fetch.is_empty() {
            // A reference carries a table and an id and not a tenancy, so it
            // resolves in the read's own database — which is also why a fetch
            // cannot reach across one (ADR-0008).
            let context = self.context(transaction, None, select.span)?;
            self.follow(transaction, &mut records, &select.fetch, context)?;
        }
        // After the fetch — a reference resolved once and then opened is the
        // same answer as one opened and then resolved n times, and cheaper — and
        // before everything that counts records, because this is the stage that
        // decides how many there are.
        if let Some(route) = &select.split {
            records = opened(records, &route.path, &mut budget)?;
        }
        let records = if groups(select) {
            // A grouping folds many records into one, and a fold answers about
            // the group rather than about a record — so the star has nothing to
            // contribute here and the grammar has already refused one written
            // beside a fold.
            self.grouped(
                transaction,
                records,
                select.projection.written(),
                &select.group,
            )?
        } else if let Some(wanted) = self.shaped(transaction, select)? {
            let mut projected = Vec::with_capacity(records.len());
            for (id, record) in records {
                let shaped =
                    self.project(transaction, &id, &record, &wanted, &searched, &noticed)?;
                projected.push((id, shaped));
            }
            projected
        } else {
            records
        };
        // Ordering comes after projection so that a key may name what the caller
        // can see: `SELECT address.city AS home … ORDER BY home` reads the name
        // the answer carries rather than the route it came from. A route still
        // works, because a projected record keeps the shape it was given only
        // where the projection preserved it — which is why the sort falls back
        // to the route when the name is not there.
        // A cursor read goes through the ordering stage even when it named no
        // order, because the clause supplies one: with no keys to compare,
        // `ranked` falls through to the identity, so the stage both applies the
        // cursor and answers in the store's own order. Skipping it would leave
        // the page to whatever order the source happened to produce, which is
        // the one thing a resumed read cannot be built on.
        let records = if select.order.is_empty() && select.after.is_none() {
            records
        } else {
            // The same ordering stage the streaming path uses, fed from a vector
            // instead of from the source. One implementation rather than two,
            // because two would be two chances for a `LIMIT` to change which
            // records an order answers with.
            //
            // `None` for the projection: these records have already been through
            // it, since the barrier that forced this path ran above.
            let keys = self.folded_order(transaction, &select.order)?;
            let mut shaping = crate::consume::Shaping::new(
                self,
                None,
                keys,
                &searched,
                crate::shape::Topmost::keeping(&select.order, bound),
                &mut budget,
                &noticed,
            );
            if let Some((id, record)) = resuming {
                shaping.resume_after(transaction, id, record)?;
            }
            for (id, record) in records {
                if shaping.take(transaction, id, record)?.is_break() {
                    break;
                }
            }
            shaping.finish()
        };
        asserted(select, &plan)?;
        notes.extend(noticed.drained());
        let records = crate::shape::bounded(records, select.start, select.limit);
        alone(select, &records)?;
        Ok(Answered {
            records,
            plan,
            notes,
            suggestion: searched.suggestion(),
        })
    }

    /// The record a cursor resumes after: its identity, and itself when the
    /// order needs a key evaluated over it.
    ///
    /// The asymmetry in the middle is the whole of this function. A read that
    /// named **no order** resumes in the store's own, where the identity *is*
    /// the key — so nothing is read, and a page walk survives the deletion of
    /// the record it resumed from, which is the ordinary way a long walk ends
    /// otherwise. A read that named **an order** needs the anchor's value for
    /// that key, and there is nowhere to get it but the record: a missing anchor
    /// is refused rather than guessed at, because every guess picks a page.
    fn resuming(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Option<(RecordId, Option<Value>)>> {
        let Some(anchor) = &select.after else {
            return Ok(None);
        };
        let (_, address) = self.address(transaction, anchor)?;
        if select.order.is_empty() {
            return Ok(Some((address.id, None)));
        }
        let visible = self.visible_in(transaction, address.table)?;
        let Some(payload) = transaction.get(&address)? else {
            return Err(Error::AnchorGone {
                table: anchor.table.name.text.clone(),
                span: anchor.span,
            });
        };
        let record = self.record_of(&payload, &visible)?;
        Ok(Some((address.id, Some(record))))
    }

    /// What this read's projection produces, or nothing when it produces the
    /// record unchanged.
    ///
    /// `None` is the read that copies nothing — a bare `*` with no `OMIT` — and
    /// it is the commonest read in the language, so it keeps the path it had:
    /// the records reach the answer as they were decoded.
    ///
    /// The projection is folded here, once above the records, because its
    /// constant parts are constant across every record it is applied to.
    /// Rebuilding them per record is what the benchmark harness once found
    /// dominating a nearest-neighbour read.
    fn shaped(&self, transaction: &mut Transaction<'_>, select: &Select) -> Result<Option<Shaped>> {
        match &select.projection {
            Projection::All if select.omit.is_empty() => Ok(None),
            Projection::All => Ok(Some(Shaped {
                everything: true,
                omit: select.omit.clone(),
                values: Vec::new(),
            })),
            Projection::Values { everything, values } => Ok(Some(Shaped {
                everything: everything.is_some(),
                omit: select.omit.clone(),
                values: self.folded_projection(transaction, values)?,
            })),
        }
    }

    /// One record, reduced to the values a read asked for.
    ///
    /// **A projection that reaches nothing omits its field** rather than
    /// answering `none`. `Value::None` means the field is not there, so writing
    /// it into an object would say the field is there and holds
    /// not-being-there — the contradiction the value system spends its own rules
    /// avoiding. The consequence is that projected records keep differing
    /// shapes, which is the same property that makes a table able to hold
    /// documents at all.
    ///
    /// A computed projection is evaluated against this record, so it is the
    /// same evaluator a `WHERE` uses and cannot disagree with it.
    pub(crate) fn project(
        &self,
        transaction: &mut Transaction<'_>,
        id: &RecordId,
        record: &Value,
        wanted: &Shaped,
        searched: &Searched,
        noticed: &Noticed,
    ) -> Result<Value> {
        // The star first, so a value written out by name is written **over** the
        // field it shares a name with. `SELECT *, upper(name) AS name` answers
        // with the computed one, which is the same rule the ordering stage's
        // overlay already follows — an alias shadows the field it is named for.
        let mut projected = match (wanted.everything, record) {
            (true, Value::Object(fields)) => fields
                .iter()
                .filter(|(name, _)| !omits(&wanted.omit, name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            // A star over something that is not an object contributes nothing
            // rather than failing: a projection reaching nothing omits its field
            // everywhere else here, and this is the same rule one level up.
            _ => BTreeMap::new(),
        };
        // A route that reaches inside is removed after the copy rather than
        // filtered during it, because the field it names still belongs in the
        // answer — `OMIT address.postcode` keeps the address.
        for route in &wanted.omit {
            if !route.path.steps().is_empty() {
                omit_within(&mut projected, &route.path);
            }
        }
        for value in &wanted.values {
            // A route reaching several values is **collected** here rather than
            // resolved, and the rule lives in the projection the way the
            // existential rule lives in the comparison. Reading it in the
            // evaluator's path arm instead would be fewer lines and would hand
            // `array::len(tags[*])` an answer nobody decided on — a rule kept
            // honest only by a parser refusal is a rule waiting for the day
            // somebody moves the refusal.
            //
            // A relation is **total**, so this always writes its field: every
            // record has a reach, and zero of them is an empty array rather than
            // an absence. An empty array, an absent field and a single value all
            // reach nothing and so all answer `[]` — a projection that told them
            // apart would be reading whether the field exists, which is a
            // different question that `tags` already answers on its own.
            if let ExprKind::Path(field) = &value.value.kind
                && field.path.is_several()
            {
                let reached = field.path.reach(record).into_iter().cloned().collect();
                projected.insert(value.name.text.clone(), Value::Array(reached));
                continue;
            }
            // The searched context reaches here as well as the `WHERE` and the
            // `ORDER BY`: a projection is where a caller most often asks for a
            // score, and it needs the same collection the ordering measures
            // against or the two would disagree in the same statement.
            //
            // A fold never reaches here: a projection holding one goes through
            // `grouped`, which is the only place many records become one.
            let held = self.evaluate_in(
                transaction,
                &value.value,
                Scope::searching(record, searched)
                    .identified(id)
                    .noticing(noticed),
            )?;
            if held.is_present() {
                projected.insert(value.name.text.clone(), held);
            }
        }
        Ok(Value::Object(projected))
    }

    /// What one record scores against the collection its field is indexed in.
    ///
    /// The first argument must be a **path**: a score is measured against the
    /// statistics of one indexed field, and an arbitrary expression names no
    /// field to have statistics for. Refusing that is refusing to guess.
    /// Where in this record's text the read's own query matched, as an ordered
    /// array of `{ start, end }` byte ranges.
    ///
    /// # Everything here answers `[]` rather than refusing
    ///
    /// A highlight is a projection, not a filter: it decorates records some
    /// other clause already chose. So a field with no declared analyzer, a
    /// record holding no text there, and a field nobody asked about all answer
    /// *no marks* — which is the true answer in each case, and is what lets one
    /// `search::highlight(body)` be written over a table whose records do not
    /// all carry a body.
    ///
    /// That is the opposite of [`rank`](Self::rank), which refuses, and the
    /// difference is real rather than a style choice: a score with nothing to
    /// measure against has no honest number, while a highlight with nothing to
    /// mark has an honest answer and it is the empty one.
    fn highlight(
        &self,
        transaction: &mut Transaction<'_>,
        arguments: &[Expr],
        scope: Scope<'_>,
    ) -> Result<Value> {
        let none = Ok(Value::Array(Vec::new()));
        let Some(first) = arguments.first() else {
            return none;
        };
        // The argument is the field, and the field is where both the analyzer
        // and the recorded query are found — so an expression that is not a path
        // names neither and marks nothing.
        let ExprKind::Path(field) = &first.kind else {
            return none;
        };
        let Some(analyzer) = scope.analyzer(&field.path) else {
            return none;
        };
        let Value::String(text) = self.evaluate_in(transaction, first, scope)? else {
            return none;
        };
        Ok(Value::Array(
            marked(analyzer, &text, scope.wanted(&field.path))
                .into_iter()
                .map(|bytes| {
                    Value::Object(BTreeMap::from([
                        ("start".to_owned(), at(bytes.start)),
                        ("end".to_owned(), at(bytes.end)),
                    ]))
                })
                .collect(),
        ))
    }

    fn rank(
        &self,
        transaction: &mut Transaction<'_>,
        arguments: &[Expr],
        scope: Scope<'_>,
        span: Span,
    ) -> Result<Value> {
        // The query is the second argument and it is deliberately **not**
        // evaluated here. It was evaluated and analysed once, while the corpus
        // was resolved, and doing it again per scored record is half of the cost
        // this function used to carry. Its presence is still what makes the call
        // a score rather than a mistake.
        let (Some(first), Some(_query)) = (arguments.first(), arguments.get(1)) else {
            return Ok(Value::None);
        };
        let ExprKind::Path(field) = &first.kind else {
            return Err(Error::NoSearchIndex {
                field: "that expression".to_owned(),
                span,
            });
        };
        // Refused before either argument is evaluated: there is nothing to
        // measure against, so evaluating them would be work done to reach a
        // conclusion already known.
        //
        // The record's identity is part of that. A row with none is a row no
        // index holds — a join's pair, a fold's result — so there are no postings
        // to read and no honest number to return, which is the same refusal for
        // the same reason.
        let (Some(ranked), Some(analyzer), Some(id)) = (
            scope.ranked(&field.path),
            scope.analyzer(&field.path),
            scope.id,
        ) else {
            return Err(Error::NoSearchIndex {
                field: field.path.to_string(),
                span,
            });
        };
        let corpus = &ranked.corpus;

        // The record's two numbers, read from the postings the writer already
        // put them in. `None` is the term not posted against this record, which
        // scores nothing — reached without touching the record at all.
        //
        // Every posting of one record carries the same length, written by one
        // analysis in one batch, so the last one read is as good as any. A record
        // holding none of the asked terms leaves it at zero, which changes
        // nothing: with no occurrences there is no term for the length to divide.
        let mut occurrences = BTreeMap::new();
        let mut length = 0_u32;
        let mut membership = false;
        for term in corpus.terms.keys() {
            match transaction.posting(&ranked.index, term, id)? {
                None => {}
                Some(Posting::Counted {
                    frequency,
                    length: tokens,
                }) => {
                    occurrences.insert(term.clone(), frequency);
                    length = tokens;
                }
                // An index written before postings carried a payload. It knows
                // the term is here and not how often, so the numbers come from
                // the text — the old cost, paid only by an old index, exactly as
                // `document_frequency` falls through to its count.
                Some(Posting::Membership) => {
                    membership = true;
                    break;
                }
            }
        }
        if !membership {
            return Ok(score(corpus, &Held::counted(occurrences, length)));
        }
        let held = self.evaluate_in(transaction, first, scope)?;
        let Value::String(text) = held else {
            // Not text: it holds none of the words, which scores zero. The same
            // answer a document of the wrong shape gets from `MATCHES`, in the
            // ranking's own terms.
            return Ok(score(corpus, &Held::default()));
        };
        Ok(score(
            corpus,
            &Held::analysed(analyzer, &text, &corpus.asked),
        ))
    }

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

    fn prepare_source<'a>(
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
                Ok((Prepared::Filtered(context, id, condition), searched))
            }
            Source::Join {
                left,
                right,
                left_key,
                right_key,
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
                // `None` for the held ceiling: the grammar already refuses a
                // materialised source that names no `LIMIT`, so the bound here
                // is the author's own and a second one below it would be a rule
                // in two places that could only ever disagree.
                let inner = self.read(transaction, read, within, None)?;
                reporting.collected.extend(inner.notes);
                reporting
                    .collected
                    .extend(ceiling_reached(read, inner.records.len()));
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

    /// The records a source produces, handed over one at a time.
    ///
    /// Returns the path the walk turned out to take. Nothing is returned that a
    /// consumer could have kept — that is the point of the contract (ADR-0014):
    /// the source never learns what the consumer keeps, so a bound reaches it
    /// only through the `Break` the consumer answers with.
    fn produce_source(
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
                // Descending only. The walk under a condition retries past its
                // bound, and an ascending retry would read further into values
                // the answer has already passed rather than further into the
                // ones it still needs — a different read, not a longer one.
                // The decline is remembered rather than acted on here, because
                // what the read falls back *to* is not known until `candidates`
                // has chosen — an index on the condition serves this read even
                // when no index could serve its order.
                let mut declined = false;
                if let Some(bound) = plan::ordered(select).filter(|bound| bound.descending) {
                    match self.descend_matching(
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
                let Some(Reached {
                    records: candidates,
                    plan,
                    answered,
                }) = self.candidates(transaction, id, context, condition, searched, named)?
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

    /// Two tables matched on a value neither of them stores a pointer for.
    ///
    /// # The map is ordered, and a hash map here would be a silent bug
    ///
    /// [`Value`] derives `Hash` structurally, but its **equality is not
    /// structural**: `Number`'s `PartialEq` is defined as `cmp() == Equal`, so
    /// `3` and `3.0` are equal — deliberately, because a comparison that
    /// disagreed with the order its own index is stored in is the failure this
    /// store keeps refusing.
    ///
    /// Which means `Value` breaks the `Hash`/`Eq` contract: two equal values can
    /// hash differently. A `HashMap` keyed by one would put `3` and `3.0` in
    /// different buckets and the join would **miss matches with no error at
    /// all**. A `BTreeMap` uses `Ord`, which agrees with equality exactly here,
    /// so the join matches what `=` matches — which is the requirement, since a
    /// join is spelled with the same operator.
    ///
    /// The index path below re-tests for a related reason: an index normalises
    /// its encoding, so a lookup can offer candidates the condition would not
    /// accept. Re-testing them is the rule every other index read in this store
    /// already follows — an index narrows and never answers.
    ///
    /// # Which side is read and which is probed
    ///
    /// Rule-based, because the store keeps no row counts and SGB.T4 already
    /// refused a cost model over statistics it would have to invent. An index on
    /// the right side's key means the left side drives and each of its records
    /// probes that index; otherwise the right side is read once into the map and
    /// the left side probes memory. Either way the work is `n + m` rather than
    /// `n × m`, and what happened is reported through [`AccessPath`].
    ///
    /// # An empty answer that a type mistake explains is refused
    ///
    /// `no rows` is the honest answer to a join over data that happens not to
    /// match, and it is also what a join answers when one side stores an
    /// identity as text and the other stores it as a reference. Those two are
    /// indistinguishable to whoever reads the answer and only one of them is a
    /// mistake, so the store separates them: when the answer is empty and the
    /// two sides' key kinds are both non-empty and share nothing, the read
    /// fails with [`Error::JoinKeysDiffer`] instead of answering.
    ///
    /// The rule is deliberately not "refuse as soon as one compared pair
    /// differs". Records here carry no declared type, so one stray value among a
    /// thousand would refuse a join that works — trading a silent wrong answer
    /// for a loud wrong refusal. Making a single mismatched pair *visible*
    /// without failing the read is the note channel's job and belongs with it.
    #[expect(
        clippy::too_many_arguments,
        reason = "every one is a distinct part of the clause, and a struct here                   would be the clause spelled twice"
    )]
    fn join(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        left: &JoinSide,
        right: &JoinSide,
        left_key: &tessari_ql::FieldPath,
        right_key: &tessari_ql::FieldPath,
        condition: Option<&Expr>,
        reporting: Reporting<'_>,
        within: Option<Deadline>,
    ) -> Result<Joined> {
        let left_name = left.name().to_owned();
        let right_name = right.name().to_owned();

        // The right side takes one of two shapes. A table carrying an ordered
        // index on the key is **probed**, one left record at a time; anything
        // else is read once into an ordered map. A materialised read is always
        // the second: it has no index of its own, and building one for a single
        // statement would cost more than the map it replaces.
        //
        // Each side is redacted by its own grant — a join is two reads and
        // neither borrows the other's permission. A side that is a read applied
        // its own on the way through.
        let mut probed = None;
        let mut built: BTreeMap<Value, Vec<(RecordId, Value)>> = BTreeMap::new();
        match right {
            JoinSide::Table { table, .. } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                let visible = self.visible_in(transaction, id)?;
                match ordered_index_on(transaction, id, right_key)? {
                    Some(index) => probed = Some((index, visible, context, id)),
                    None => {
                        let found =
                            transaction.scan_table(context.namespace, context.database, id)?;
                        collect_by_key(&mut built, self.records_of(found, &visible)?, right_key);
                    }
                }
            }
            JoinSide::Read { read, .. } => {
                let answered = self.read(transaction, read, within, None)?;
                reporting.collected.extend(answered.notes);
                reporting
                    .collected
                    .extend(ceiling_reached(read, answered.records.len()));
                collect_by_key(&mut built, answered.records, right_key);
            }
        }

        // A read has no table to resolve an analyzer against, so a scored
        // expression over a joined subquery falls back to the default context
        // rather than borrowing the other side's.
        let (driving, searched) = match left {
            JoinSide::Table { table, .. } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                let visible = self.visible_in(transaction, id)?;
                let searched = self.searched_for(transaction, id, &shown(select))?;
                let found = transaction.scan_table(context.namespace, context.database, id)?;
                (self.records_of(found, &visible)?, searched)
            }
            JoinSide::Read { read, .. } => {
                let answered = self.read(transaction, read, within, None)?;
                reporting.collected.extend(answered.notes);
                reporting
                    .collected
                    .extend(ceiling_reached(read, answered.records.len()));
                (answered.records, Searched::default())
            }
        };

        let mut rows = Vec::new();
        let mut left_kinds = BTreeSet::new();
        for (id, record) in driving {
            // A left record with nothing at the key matches nothing: `NONE` is a
            // value and the right side would have to carry it to match, which is
            // what an inner join means.
            let Some(key) = left_key.path.resolve(&record).cloned() else {
                continue;
            };
            left_kinds.insert(key.type_name());
            let matches = match &probed {
                Some((index, visible, _, _)) => {
                    let offered =
                        transaction.records_by_index(index, core::slice::from_ref(&key))?;
                    self.records_of(offered, visible)?
                        .into_iter()
                        .filter(|(_, held)| right_key.path.resolve(held) == Some(&key))
                        .collect()
                }
                None => built.get(&key).cloned().unwrap_or_default(),
            };
            for (_, far) in matches {
                let row = Value::Object(BTreeMap::from([
                    (left_name.clone(), record.clone()),
                    (right_name.clone(), far),
                ]));
                if let Some(condition) = condition {
                    let held = self.evaluate_in(
                        transaction,
                        condition,
                        Scope::searching(&row, &searched).noticing(reporting.noticed),
                    )?;
                    if !boolean(&held, condition.span)? {
                        continue;
                    }
                }
                // The left record's id. A row is not a record, and two rows from
                // one left record carry one id — stated in `Source::Join` rather
                // than left to be discovered.
                rows.push((id.clone(), row));
            }
        }
        // Only here, and only on the answer that was about to lie. A join that
        // produced a row matched a value, and two values that are equal are of
        // one kind, so the sets overlap and this cannot fire; a join that
        // produced nothing is the one whose emptiness needs explaining.
        if rows.is_empty() {
            let right_kinds = match &probed {
                // The index path never read the right side, so learning what it
                // holds costs a scan. It is paid once, after an empty answer,
                // and never by a join that worked.
                Some((_, visible, context, id)) => {
                    let found = transaction.scan_table(context.namespace, context.database, *id)?;
                    self.records_of(found, visible)?
                        .iter()
                        .filter_map(|(_, record)| right_key.path.resolve(record))
                        .map(Value::type_name)
                        .collect()
                }
                None => built.keys().map(Value::type_name).collect::<BTreeSet<_>>(),
            };
            // Both sides must have held something: a join over an empty table
            // has no kinds to reconcile and answers nothing for the ordinary
            // reason.
            if !left_kinds.is_empty()
                && !right_kinds.is_empty()
                && left_kinds.is_disjoint(&right_kinds)
            {
                return Err(Error::JoinKeysDiffer {
                    left_key: left_key.path.to_string(),
                    left_kinds: listed(&left_kinds),
                    right_key: right_key.path.to_string(),
                    right_kinds: listed(&right_kinds),
                    span: select.span,
                });
            }
        }
        // `join`, whichever way the sides were read: neither side's own path is
        // how the joined answer was reached, and reporting one of them named half
        // a read. What is worth naming is the index the right side was **probed**
        // through, because that is the difference between a probe per left record
        // and a map of the whole right table — and `EXPLAIN` names it from the
        // same `ordered_index_on`.
        let plan = Plan {
            index: probed.map(|(index, ..)| index.name),
            ..Plan::new(AccessPath::Join)
        };
        Ok((rows, plan, searched))
    }

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
    fn candidates(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        context: crate::context::Context,
        condition: &Expr,
        searched: &Searched,
        named: Option<&str>,
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
            Catalog::new(transaction).indexes_on(table)?
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
            Some(candidate) if plan::worth_serving(transaction, table, &candidate)? => {
                Some(candidate)
            }
            _ => None,
        };
        if let Some(chosen) = chosen {
            // Built by the candidate itself, which is the same function
            // `EXPLAIN` calls on the candidate its own `choose` returned. The
            // two report one structure because one function writes it.
            let plan = chosen.plan(named);
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
    fn scan_matching(
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
    fn trusts(
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
    /// `None` means there is not — no index on the path, one built for another
    /// distance, or a query that is not a vector — and the caller scans, which
    /// is exact. That is the whole safety story: the approximate path is taken
    /// only when the statement asked and the index matches, and the exact path
    /// is what every other case falls into.
    fn walk(
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
    /// The query position is the last: `geo::distance` takes positions, so an
    /// argument that is not one is an error in the statement, and the scan is
    /// what reports it. [`Transaction::records_by_place`] owns the three
    /// remaining refusals, which are facts about the records rather than about
    /// the session.
    fn walk_to_place(
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

    /// A bounded read ordered by how well a record answers a query.
    ///
    /// # The candidate set is the answer's, and the rest of the table is pruned
    ///
    /// A record holding none of the query's words scores zero, so the records
    /// that can fill a `LIMIT k` are the ones the index posts against at least
    /// one of those words. The scan reaches the same answer by scoring every
    /// record in the table and discarding the zeros.
    ///
    /// Which terms are worth enumerating is then decided between them, and this
    /// is where the stored extremes earn their keep. The terms are taken in
    /// descending order of the most they can contribute, and the remaining
    /// suffix is abandoned once its **combined** bound falls below the score
    /// already held in `k`th place: a record holding only those terms cannot
    /// score above that suffix's sum, so it cannot reach the answer, so its
    /// postings are never read. On a query pairing a rare word with a common one
    /// that is the common word's whole posting list.
    ///
    /// The threshold is strict — `sum < kth` rather than `<=` — so a record the
    /// suffix excludes scores strictly below the `k`th, and cannot tie with it
    /// either.
    ///
    /// # What it hands back, and why not the top `k`
    ///
    /// The whole candidate set, for the ordering stage to sort exactly as it
    /// sorts a scan's. Selecting the `k` best here would mean implementing that
    /// stage's comparison a second time — and it is a **total** order, ties
    /// broken by record id (see [`crate::shape`]), precisely so that which access
    /// path ran cannot reorder equal rows. A second copy of it would agree until
    /// somebody edited one of them.
    ///
    /// Which is also why the order these candidates are produced in does not
    /// matter, and why nothing here sorts them.
    ///
    /// # Declining
    ///
    /// Fewer candidates than the read wants means the answer is filled out with
    /// records that hold none of the query's words. Their order among themselves
    /// is the scan's, so the scan is the read that can produce it.
    fn walk_scored(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Scored<'_>,
        searched: &crate::search::Searched,
    ) -> Result<Walked> {
        // A read wanting nothing has nothing to prune against — `best[0]` below
        // is the score in last place, and there is no last place in an empty
        // answer.
        if wanted.wanted == 0 {
            return Ok(Walked::NotServed);
        }
        let Some(ranked) = searched.ranked(wanted.field) else {
            return Ok(Walked::NotServed);
        };
        let Some(visible) = self.index_serving_score(transaction, context, table, wanted.field)?
        else {
            return Ok(Walked::NotServed);
        };
        let corpus = &ranked.corpus;

        // A term written twice in a query weighs twice, so its bound is twice as
        // large. Counting the multiset here rather than deduplicating it keeps
        // the bound above the score it bounds.
        let mut terms = Vec::new();
        for term in corpus.terms.keys() {
            let repeats = corpus.asked.iter().filter(|asked| *asked == term).count();
            // A term this build cannot bound is a term the walk may not prune,
            // and no suffix containing it is bounded either. Handing the read
            // back rather than walking the postings unpruned is not caution: a
            // union of posting lists is read one record at a time, and over a
            // word most of the table holds that is measurably **more** work than
            // the scan it was standing in for.
            let Some(one) = rank::bound(corpus, term) else {
                return Ok(Walked::NotServed);
            };
            terms.push((term, one * as_count(repeats)));
        }
        terms.sort_by(|left, right| right.1.total_cmp(&left.1));
        // `suffix[at]` is the most everything from `at` onward could contribute,
        // accumulated from the back so the walk can ask, at each term, what the
        // whole remaining tail is worth.
        let mut suffix = Vec::with_capacity(terms.len().saturating_add(1));
        let mut running = 0.0_f64;
        suffix.push(running);
        for (_, one) in terms.iter().rev() {
            running += one;
            suffix.push(running);
        }
        suffix.reverse();

        let mut candidates = BTreeMap::new();
        let mut best: Vec<f64> = Vec::new();
        for (at, (term, _)) in terms.iter().enumerate() {
            if best.len() >= wanted.wanted && suffix[at] < best[0] {
                break;
            }
            for id in transaction.records_with_term(&ranked.index, term)? {
                let address = RecordAddress::new(context.namespace, context.database, table, id);
                let Some(payload) = transaction.get(&address)? else {
                    continue;
                };
                if candidates.insert(address.id.clone(), payload).is_some() {
                    continue;
                }
                // A posting written before it carried a payload. Its numbers are
                // in the record's text, and re-analysing it here would be the
                // scan's own work paid inside the walk that exists to avoid it —
                // so the read goes back to the scan rather than losing its
                // threshold and finishing as an unpruned union.
                let Some(held) =
                    self.scored_from_postings(transaction, ranked, corpus, &address.id)?
                else {
                    return Ok(Walked::NotServed);
                };
                keep_best(&mut best, rank::scored(corpus, &held), wanted.wanted);
            }
        }
        if candidates.len() < wanted.wanted {
            return Ok(Walked::Declined);
        }
        Ok(Walked::Served {
            found: self.records_of(candidates.into_iter().collect(), &visible)?,
            index: ranked.index.name.clone(),
        })
    }

    /// One candidate's score, read from the postings alone.
    ///
    /// `None` is a posting that predates the payload, which says the term is
    /// there and not how often. Only the record's own text answers then, and
    /// re-analysing it here would be the scan's cost paid inside the walk that
    /// exists to avoid it — so the caller stops pruning instead.
    fn scored_from_postings(
        &self,
        transaction: &mut Transaction<'_>,
        ranked: &crate::search::Ranked,
        corpus: &rank::Corpus,
        id: &RecordId,
    ) -> Result<Option<rank::Held>> {
        let mut occurrences = BTreeMap::new();
        let mut length = 0_u32;
        for term in corpus.terms.keys() {
            match transaction.posting(&ranked.index, term, id)? {
                None => {}
                Some(Posting::Counted {
                    frequency,
                    length: tokens,
                }) => {
                    occurrences.insert(term.clone(), frequency);
                    length = tokens;
                }
                Some(Posting::Membership) => return Ok(None),
            }
        }
        Ok(Some(rank::Held::counted(occurrences, length)))
    }

    /// Whether a search index can serve a ranked read of this field, and what
    /// the caller may see of the table.
    ///
    /// The corpus resolution already found the index — this asks the questions
    /// that are about the *read* rather than about the field, and it asks them
    /// in one place so the executor and `EXPLAIN` cannot come to disagree, the
    /// same reason [`Evaluator::index_serving_place`] gathers its own.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub(crate) fn index_serving_score(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        path: &tessari_types::Path,
    ) -> Result<Option<crate::redact::Visible>> {
        let visible = self.visible_in(transaction, table)?;
        if visible
            .as_ref()
            .is_some_and(|fields| !fields.contains(path.root()))
        {
            return Ok(None);
        }
        // Uncommitted writes are not in the postings, so a walk over them would
        // answer without the records this transaction itself just wrote.
        if transaction.writes_in(context.namespace, context.database, table) {
            return Ok(None);
        }
        if !transaction.indexes_are_current()? {
            return Ok(None);
        }
        Ok(Some(visible))
    }

    /// The spatial index that may serve an order by distance from this field,
    /// with the caller's field visibility.
    ///
    /// The kind check is not a formality: an index keyed by **values** on the
    /// same field would let a walk over places loose in a keyspace it has no
    /// business in, where it would find nothing, answer with no rows, and report
    /// no error. So the test names what it admits rather than what it rejects,
    /// and a kind added later is refused by default rather than admitted by
    /// omission.
    ///
    /// Asked here rather than at each call site, so the executor and `EXPLAIN`
    /// cannot come to disagree about which reads are servable — the same reason
    /// [`Evaluator::index_serving_order`] gathers its own four.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    pub(crate) fn index_serving_place(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        path: &tessari_types::Path,
    ) -> Result<Option<(tessari_storage::IndexDefinition, crate::redact::Visible)>> {
        let Some(index) = Catalog::new(transaction)
            .indexes_on(table)?
            .into_iter()
            .find(|index| index.spatial && index.fields.first() == Some(path))
        else {
            return Ok(None);
        };
        let visible = self.visible_in(transaction, table)?;
        if visible
            .as_ref()
            .is_some_and(|fields| !fields.contains(path.root()))
        {
            return Ok(None);
        }
        if transaction.writes_in(context.namespace, context.database, table) {
            return Ok(None);
        }
        if !transaction.indexes_are_current()? {
            return Ok(None);
        }
        Ok(Some((index, visible)))
    }

    /// A bounded descending read, taken from an index that is already in that
    /// order.
    ///
    /// `None` is the scan, and every `None` below is a way the store — rather
    /// than the statement, which [`plan::descending`] has already judged — makes
    /// the order the index holds differ from the order the read must answer in:
    ///
    /// - **no ordered index on that field.** A search index holds terms and a
    ///   vector index holds a graph; neither is stored in this order.
    ///   A composite index **is** taken when the ordered field is its leading
    ///   one, because its entries are stored by that field before anything else.
    ///   What changes with it is the tie group: the entries sharing one leading
    ///   value are ordered by the *next* indexed field rather than by the
    ///   record's identity, so the group at the bound is drained in **both**
    ///   directions. The group's edge is read off the key without decoding it —
    ///   [`tessari_encoding::IndexValues`] cannot be reversed, but two entries
    ///   agree on their leading values exactly when those bytes are equal, and
    ///   agreement is the only thing a tie test asks.
    /// - **the field is not visible to this caller.** A field permission removes
    ///   the field *before* anything looks at the record, so today a caller
    ///   without it sorts by `none` and gets identity order. An ordering taken
    ///   from the index would sort by the values themselves — the order
    ///   disclosing what the projection hides, one comparison at a time.
    /// - **this transaction has written to the table.** Entries are derived at
    ///   commit, so an uncommitted record has none and the index cannot place it.
    /// - **the snapshot is not the committed tail.** Entries hold the current
    ///   state and carry no version, so a record changed since the snapshot sits
    ///   in the index under a value this reader cannot see — and the answer that
    ///   comes back is not short, it is in the **wrong order**, with the record
    ///   placed where its newer value belongs. A condition served by an index
    ///   survives that because its candidates are re-tested against the record;
    ///   an ordering has nothing to re-test, since the entry's position is the
    ///   answer. The storage suite demonstrates it rather than this sentence
    ///   asserting it.
    ///
    /// The last `None` is the index running out before the bound was filled,
    /// which is the answer needing records the index does not hold.
    fn walk_in_order(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Bounded<'_>,
    ) -> Result<Walked> {
        let Some((index, visible)) =
            self.index_serving_order(transaction, context, table, wanted.path, wanted.descending)?
        else {
            return Ok(Walked::NotServed);
        };
        // The two directions differ in what a short walk *means*, which is why
        // one returns an option and the other does not. Descending, an index
        // that runs out is missing the records whose value is absent — they sort
        // below everything it holds, so the answer needs them and the caller
        // scans. Ascending is admitted only where there are no absences, so an
        // index that runs out has answered the whole table and a short answer is
        // a complete one.
        let found = if wanted.descending {
            match transaction.records_in_descending_order(
                &index,
                ORDERED_LEADING_FIELDS,
                wanted.wanted,
            )? {
                Some(found) => found,
                None => return Ok(Walked::Declined),
            }
        } else {
            transaction.records_in_ascending_order(&index, ORDERED_LEADING_FIELDS, wanted.wanted)?
        };
        Ok(Walked::Served {
            found: self.records_of(found, &visible)?,
            index: index.name,
        })
    }

    /// A bounded descending read **under a condition**, taken from the index
    /// that holds the order.
    ///
    /// `None` means the read is not served this way and the caller narrows and
    /// sorts, which is what it did before this existed.
    ///
    /// # The trap, and the whole of why this is not a call site
    ///
    /// An index narrows and the **condition decides** — every candidate is
    /// re-tested against the whole of it above the source. So a walk that filled
    /// the caller's bound with ten *entries* can answer with fewer than ten
    /// *records*, because some of them fail that test. Not an error, not a
    /// crash: real records, fewer of them, returned confidently. That is the
    /// same failure class as a limit pushed past a clause that changes the
    /// count, and it is why this asks for more until enough **survive** rather
    /// than until enough are read.
    ///
    /// # Why taking the first `wanted` survivors of the top `k` is the answer
    ///
    /// The walk yields records in index order, and that **is** the sort order —
    /// the index and the sort use one order, the value system's. So no record
    /// outside the top `k` can rank above one inside it, and the first `wanted`
    /// survivors of the top `k` are the first `wanted` survivors of the whole
    /// table.
    ///
    /// Absences are the one case that could break that argument and cannot: a
    /// record with no value for the key has **no index entry** and sorts *last*
    /// descending, so it is never near the top of the order. That is the same
    /// fact that makes the unconditioned case descending-only, inherited here
    /// rather than re-derived.
    ///
    /// # The ceiling bounds the cost and never the answer
    ///
    /// How far past the bound the walk must go depends on how selective the
    /// condition is over the order — the distribution statistic this store
    /// deliberately does not keep. Past [`ORDERED_FILTER_REACH`] multiples of
    /// the bound, the order is not worth serving from the index and the read
    /// falls back to the scan it would have taken anyway. Every exit is either
    /// an ordered answer that filled the bound or the scan; there is no exit
    /// that answers short.
    ///
    /// The answer may be **longer** than the bound, which is correct and
    /// deliberate: it is in order, and `shape::bounded` takes the window the
    /// statement asked for, as it does for every other path.
    fn descend_matching(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Bounded<'_>,
        condition: &Expr,
        scope: Scope<'_>,
    ) -> Result<Walked> {
        // Descending, stated rather than taken from the bound: this walk's whole
        // argument rests on absences sorting *last*, and passing the caller's
        // direction through would make that argument depend on a value from
        // somewhere else. The caller refuses an ascending bound before it gets
        // here; this is the second lock on the same door.
        let Some((index, visible)) =
            self.index_serving_order(transaction, context, table, wanted.path, true)?
        else {
            return Ok(Walked::NotServed);
        };
        let ceiling = wanted.wanted.saturating_mul(ORDERED_FILTER_REACH);
        let mut asking = wanted.wanted;
        loop {
            // `None` is the index unable to fill `asking` — it has run out of
            // entries, and the records that would fill the rest of the answer
            // are ones it does not hold. The scan is the read that can find
            // those.
            let Some(found) =
                transaction.records_in_descending_order(&index, ORDERED_LEADING_FIELDS, asking)?
            else {
                return Ok(Walked::Declined);
            };
            let mut matched = Vec::new();
            for (id, record) in self.records_of(found, &visible)? {
                let held = self.evaluate_in(transaction, condition, scope.with(&id, &record))?;
                if boolean(&held, condition.span)? {
                    matched.push((id, record));
                }
            }
            if matched.len() >= wanted.wanted {
                return Ok(Walked::Served {
                    found: matched,
                    index: index.name,
                });
            }
            if asking >= ceiling {
                return Ok(Walked::Declined);
            }
            // Doubling, so reaching the ceiling costs about twice the ceiling in
            // entries rather than a walk per step.
            asking = asking.saturating_mul(2).min(ceiling);
        }
    }

    /// The index that may serve this order, if one may — and what the caller may
    /// see of the table, which deciding that had to read anyway.
    ///
    /// Everything above [`Self::descend`]'s list except the bound filling, which
    /// only the read itself can know. It is one function because `EXPLAIN` asks
    /// the same question and a second implementation would answer it correctly
    /// until the day one of them changed.
    ///
    /// The visible set travels back rather than being read again: it is a
    /// catalog read per statement, and the read that follows needs the same one.
    pub(crate) fn index_serving_order(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        path: &tessari_types::Path,
        descending: bool,
    ) -> Result<Option<(tessari_storage::IndexDefinition, crate::redact::Visible)>> {
        let Some(index) = self.index_ordering_on_path(transaction, table, path)? else {
            return Ok(None);
        };
        if !index.is_ordered() {
            return Ok(None);
        }
        // Ascending, the records the index does **not** hold are the ones that
        // come first, so the read is only sound where there are none of them.
        // `REQUIRED` is that guarantee and it holds in both directions in time:
        // the declaration is refused against a table already holding a record
        // without the field, and every write after it is checked.
        //
        // Asked here rather than at each call site, so the executor and
        // `EXPLAIN` cannot come to disagree about which reads are servable —
        // the same reason the other four refusals below live here.
        if !descending && !self.every_record_has(transaction, table, path)? {
            return Ok(None);
        }
        let visible = self.visible_in(transaction, table)?;
        if visible
            .as_ref()
            .is_some_and(|fields| !fields.contains(path.root()))
        {
            return Ok(None);
        }
        if transaction.writes_in(context.namespace, context.database, table) {
            return Ok(None);
        }
        if !transaction.indexes_are_current()? {
            return Ok(None);
        }
        Ok(Some((index, visible)))
    }

    /// Whether every record of this table is guaranteed to hold a value at this
    /// route — which is what makes it certain that every record has an index
    /// entry.
    ///
    /// **Only a plain top-level field can answer yes.** `REQUIRED` is declared
    /// on a field, so it says nothing about what lives *inside* one: a required
    /// `address` does not promise an `address.city`, and a route with steps
    /// below its root is therefore refused rather than approximated.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be read.
    fn every_record_has(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        path: &tessari_types::Path,
    ) -> Result<bool> {
        if !path.steps().is_empty() {
            return Ok(false);
        }
        Ok(Catalog::new(transaction)
            .fields_on(table)?
            .iter()
            .any(|field| field.name == path.root() && field.required))
    }

    /// Run the candidate the plan chose.
    ///
    /// Every arm has everything it needs on the candidate — the value, the
    /// literal prefix, the analysed terms — because [`crate::plan`] computed
    /// them while ranking. There is nothing here to recompute and no shape that
    /// can arrive without its argument.
    ///
    /// The one thing that does not travel on the candidate is the field's
    /// analyzer, and the two term reads need it: they settle this transaction's
    /// own writes by re-deriving each pending record's terms, and re-deriving
    /// them with a different analyzer than the query was built with would make
    /// the two halves of one read disagree. It is passed rather than looked up
    /// again for exactly that reason.
    fn serve(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        chosen: &plan::Candidate,
        analyzer: Option<&tessari_types::Analyzer>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        match &chosen.served {
            plan::Served::Equality(values) => transaction.records_by_index(&chosen.index, values),
            plan::Served::Prefix(prefix) => {
                transaction.records_with_string_prefix(&chosen.index, prefix)
            }
            plan::Served::Range {
                fixed,
                lower,
                upper,
            } => transaction.records_in_range(&chosen.index, fixed, lower.as_ref(), upper.as_ref()),
            plan::Served::Region {
                cells,
                bounds,
                relation,
            } => {
                // The filter half. What comes back is a **candidate set** — the
                // cells are coarser than the boxes and the boxes are coarser
                // than the shapes — and the condition above refines it against
                // the real geometry, as it does for every other index read here.
                //
                // The counts the read measured are dropped on this path and that
                // is deliberate rather than an oversight: this store has no
                // statement that runs a read and reports its cost, so there is
                // nowhere truthful to put them yet. They are returned, asserted
                // by the tests that gate the query budget, and will surface here
                // when an analysing `EXPLAIN` exists to carry them.
                transaction
                    .records_in_region(&chosen.index, cells, *bounds, *relation)
                    .map(|region| region.rows)
            }
            plan::Served::Terms(terms) => {
                let mut rows = Vec::new();
                for id in transaction.records_by_terms(&chosen.index, analyzer, terms)? {
                    let at = RecordAddress::new(context.namespace, context.database, table, id);
                    if let Some(payload) = transaction.get(&at)? {
                        rows.push((at.id, payload));
                    }
                }
                Ok(rows)
            }
            // One arm for three variants: a union of posting lists per group,
            // intersected across groups, is one read however the groups were
            // arrived at — a prefix walk, a fuzzy walk, or the `OR`s somebody
            // wrote. They stay separate variants so `EXPLAIN` can still say
            // which question produced them.
            plan::Served::PrefixTerms(expansions)
            | plan::Served::FuzzyTerms(expansions)
            | plan::Served::AnyTerms(expansions) => {
                let mut rows = Vec::new();
                for id in transaction.records_by_expansions(&chosen.index, analyzer, expansions)? {
                    let at = RecordAddress::new(context.namespace, context.database, table, id);
                    if let Some(payload) = transaction.get(&at)? {
                        rows.push((at.id, payload));
                    }
                }
                Ok(rows)
            }
        }
        .map_err(Error::from)
    }

    /// A walk along one or more edge tables.
    ///
    /// Every step is an index read. The edge table was given an index on each
    /// endpoint when it was declared, so finding the edges out of a record is
    /// `records_by_index` on `out` — the same call an equality filter makes, with
    /// a record reference standing where any other value would.
    ///
    /// # A chain is one step repeated, and the repetition is where the care is
    ///
    /// Each step reads the edges out of **every** anchor it was handed, so a
    /// walk's cost multiplies by the branching factor at each hop. That is a real
    /// cost and `docs/tessariql.md` §4a states it rather than leaving it to be found.
    ///
    /// **The landing is deduplicated by record id**, which matters from the
    /// second hop onward and cannot happen on the first: two of ada's follows may
    /// follow one person, and this store's answers are keyed by record, so
    /// answering that person twice is a wrong answer rather than a verbose one.
    ///
    /// **A cycle is data.** The number of steps is written in the statement, so a
    /// walk cannot run away; if the walk arrives back where it started, that is
    /// the true answer to what was asked and not something to filter out.
    ///
    /// A dangling far endpoint drops that path rather than raising. A record can
    /// be deleted while an edge still names it, and that is a state of the graph,
    /// not a failure of the query — the alternative is a read that breaks because
    /// of a write it has nothing to do with.
    fn traverse(
        &self,
        transaction: &mut Transaction<'_>,
        from: &RecordTarget,
        direction: Direction,
        hops: &[Hop],
        depth: Option<u64>,
    ) -> Result<Vec<(RecordId, Value)>> {
        // Refused rather than served, and refused here rather than at the index
        // lookup below, which would report `NotAnEdgeTable` and send a reader
        // looking at their schema for a fault that is not there.
        //
        // A traversal is the one read of the six that has no scan to fall back
        // to: an edge table's direction indexes are how edges are followed, not
        // an optimisation over following them, so `index_on_path` returning
        // `None` is treated as catalog corruption everywhere else. Inventing a
        // scan for the historical case would be a second traversal
        // implementation with its own direction handling, built inside a wave
        // whose subject is guarding the paths that already exist.
        //
        // Serving it from the present-day index is the alternative that must not
        // happen: a traversal's answer is the least inspectable shape this
        // language produces — a set of records reached through edges nobody
        // sees — so an answer assembled from today's edges over yesterday's
        // records would be believed.
        if !transaction.indexes_are_current()? {
            return Err(Error::NoHistoricalTraversal {
                table: hops
                    .first()
                    .map_or_else(String::new, |hop| hop.edges.name.text.clone()),
                span: from.span,
            });
        }
        let (origin, start) = self.address(transaction, from)?;
        let anchors = vec![RecordRef::new(start.table, start.id.clone())];
        match depth {
            None => self.walk_written_out(transaction, &origin, anchors, direction, hops),
            // The parser has already established that there is exactly one hop
            // and that it names the table it lands on, so this indexes rather
            // than re-checking: a second copy of the rule is a second place for
            // it to disagree with itself.
            Some(limit) => {
                self.walk_repeatedly(transaction, &origin, anchors, direction, &hops[0], limit)
            }
        }
    }

    /// A walk whose steps are written out, one hop each.
    ///
    /// Bounded because the statement says how many steps there are — which is
    /// why this form needs no visited set and may legitimately answer with the
    /// same record twice if two written steps reach it.
    fn walk_written_out(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        mut anchors: Vec<RecordRef>,
        direction: Direction,
        hops: &[Hop],
    ) -> Result<Vec<(RecordId, Value)>> {
        let mut answer = Vec::new();
        for hop in hops {
            let (found, next) = self.one_hop(transaction, origin, &anchors, direction, hop)?;
            answer = found;
            anchors = next;
            // The last step named no node, so the edges themselves are the
            // answer. The grammar allows this only at the end, so there is no
            // case here where the walk would have had to continue.
            if hop.target.is_none() {
                break;
            }
        }
        Ok(answer)
    }

    /// `DEPTH n` — one hop repeated, answering with everything within `n` steps.
    ///
    /// Breadth-first over a visited set, and **the visited set is what makes the
    /// bound mean anything.** `n` is a literal, so the number of rounds is
    /// bounded by the statement; without the set, a cycle would make the *work*
    /// grow with `n` regardless — the walk would keep re-expanding records it
    /// had already reached, and a graph with one loop in it would run for as
    /// long as the number said. With the set each record is expanded once, so
    /// the walk costs the reachable subgraph however large `n` is written, and
    /// terminates on a cycle rather than on the count running out.
    ///
    /// The start is marked seen before the first round. That is not a special
    /// case for the start; it is the same rule, and it happens to give the
    /// answer people mean — a neighbourhood that contained its own centre would
    /// make a count of it wrong, and `SELECT * FROM person:1` already says the
    /// centre.
    ///
    /// Breadth-first rather than depth-first for the same reason: a record first
    /// reached in `d` steps has every neighbour of its own reached by `d + 1`,
    /// so expanding it again from a longer path can add nothing.
    fn walk_repeatedly(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        start: Vec<RecordRef>,
        direction: Direction,
        hop: &Hop,
        limit: u64,
    ) -> Result<Vec<(RecordId, Value)>> {
        let mut seen: BTreeSet<(TableId, RecordId)> = start
            .iter()
            .map(|anchor| (anchor.table, anchor.id.clone()))
            .collect();
        let mut frontier = start;
        let mut answer = Vec::new();
        for _ in 0..limit {
            if frontier.is_empty() {
                break;
            }
            let (found, next) = self.one_hop(transaction, origin, &frontier, direction, hop)?;
            // One pass, and the answer is filtered against a SET rather than
            // against a scan of the round's own results. A round's frontier is a
            // node's whole neighbourhood, so a linear search per record reached
            // would make the round quadratic in the fan-out — in the one code
            // path whose entire purpose is that a hop does not cost the degree.
            let mut fresh = BTreeSet::new();
            let mut next_frontier = Vec::new();
            for anchor in next {
                if seen.insert((anchor.table, anchor.id.clone())) {
                    fresh.insert(anchor.id.clone());
                    next_frontier.push(anchor);
                }
            }
            answer.extend(found.into_iter().filter(|(id, _)| fresh.contains(id)));
            frontier = next_frontier;
        }
        Ok(answer)
    }

    /// One step of a walk, whichever path serves it.
    ///
    /// Factored out of the loop so that `DEPTH` can take the same step more than
    /// once. Both branches answer with the same pair — the records this step
    /// reached, and the anchors the next step would start from — so a repeat is
    /// the same call again and not a second traversal implementation.
    fn one_hop(
        &self,
        transaction: &mut Transaction<'_>,
        origin: &crate::context::Context,
        anchors: &[RecordRef],
        direction: Direction,
        hop: &Hop,
    ) -> Result<Hopped> {
        // An edge kind is looked for first, because it is served by
        // adjacency rather than by an index: the neighbours of one node under
        // one kind in one direction are contiguous, so reaching them is one
        // range read instead of an index probe and a random read of every
        // edge record. The far records are still read individually — the
        // caller asked for records — but the edges themselves are never
        // touched, and that is the difference the layout buys.
        if let Some(kind) = Catalog::new(transaction).edge_kind_id(
            origin.namespace,
            origin.database,
            &hop.edges.name.text,
        )? {
            return self.hop_in_graph(transaction, kind, anchors, direction, hop);
        }
        let (_, edge_table) = self.resolve_table(transaction, &hop.edges)?;
        if !Catalog::new(transaction)
            .table(edge_table)?
            .is_some_and(|found| found.is_edge())
        {
            return Err(Error::NotAnEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop.edges.span,
            });
        }
        let Some(index) = self.index_on_path(
            transaction,
            edge_table,
            &Path::field(direction.from_field()),
        )?
        else {
            // An edge table always has both, so reaching here means the
            // catalog and the flag disagree — which is corruption, not a
            // slow path.
            return Err(Error::NotAnEdgeTable {
                table: hop.edges.name.text.clone(),
                span: hop.edges.span,
            });
        };

        let edge_visible = self.visible_in(transaction, edge_table)?;
        let mut found = Vec::new();
        for anchor in anchors {
            let value = Value::Record(anchor.clone());
            let offered = transaction.records_by_index(&index, &[value])?;
            found.extend(self.records_of(offered, &edge_visible)?);
        }

        let Some(target) = hop.target.as_ref() else {
            // This step named no node, so the edges themselves are the answer
            // and there is nothing for a next step to start from.
            return Ok((found, Vec::new()));
        };

        let (context, target_table) = self.resolve_table(transaction, target)?;
        // The far side is read from its own table, so its own grant applies —
        // reaching a record through an edge is not a way around one, and that
        // holds at every hop rather than only at the first.
        let far_visible = self.visible_in(transaction, target_table)?;
        let mut reached: BTreeMap<RecordId, Value> = BTreeMap::new();
        for (_, edge) in found {
            let Value::Object(fields) = &edge else {
                continue;
            };
            let Some(Value::Record(far)) = fields.get(direction.to_field()) else {
                continue;
            };
            if far.table != target_table {
                continue;
            }
            let address = RecordAddress::new(
                context.namespace,
                context.database,
                target_table,
                far.id.clone(),
            );
            if let Some(payload) = transaction.get(&address)? {
                reached.insert(far.id.clone(), self.record_of(&payload, &far_visible)?);
            }
        }
        let next = reached
            .keys()
            .map(|id| RecordRef::new(target_table, id.clone()))
            .collect();
        Ok((reached.into_iter().collect(), next))
    }

    /// One hop over adjacency, and the anchors the next hop starts from.
    ///
    /// The neighbours come from a single range read per anchor. When the step
    /// names no node the edges themselves are the answer, and they are assembled
    /// from the adjacency entry rather than fetched: the endpoints are in the key
    /// and the properties are in the value, so an edge of a declared kind is
    /// never read as a record on this path at all.
    fn hop_in_graph(
        &self,
        transaction: &mut Transaction<'_>,
        kind: tessari_types::EdgeKindId,
        anchors: &[RecordRef],
        direction: Direction,
        hop: &Hop,
    ) -> Result<Hopped> {
        let declared =
            Catalog::new(transaction)
                .edge_kind(kind)?
                .ok_or_else(|| Error::NotAnEdgeTable {
                    table: hop.edges.name.text.clone(),
                    span: hop.edges.span,
                })?;
        let along = match direction {
            Direction::Outgoing => AdjacencyDirection::Out,
            Direction::Incoming => AdjacencyDirection::In,
        };

        let mut edges = Vec::new();
        for anchor in anchors {
            for neighbour in transaction.neighbours(&declared, anchor.table, &anchor.id, along)? {
                edges.push((anchor.clone(), neighbour));
            }
        }

        let Some(target) = hop.target.as_ref() else {
            let answer = edges
                .into_iter()
                .map(|(anchor, neighbour)| {
                    let mut fields = match neighbour.properties {
                        Value::Object(given) => given,
                        _ => BTreeMap::new(),
                    };
                    let (out, into) = match direction {
                        Direction::Outgoing => (
                            RecordRef::new(anchor.table, anchor.id.clone()),
                            RecordRef::new(neighbour.table, neighbour.id.clone()),
                        ),
                        Direction::Incoming => (
                            RecordRef::new(neighbour.table, neighbour.id.clone()),
                            RecordRef::new(anchor.table, anchor.id.clone()),
                        ),
                    };
                    let id = RecordId::from(format!(
                        "{}:{}->{}:{}",
                        out.table, out.id, into.table, into.id
                    ));
                    fields.insert(EDGE_OUT.to_owned(), Value::Record(out));
                    fields.insert(EDGE_IN.to_owned(), Value::Record(into));
                    (id, Value::Object(fields))
                })
                .collect();
            return Ok((answer, Vec::new()));
        };

        let (context, target_table) = self.resolve_table(transaction, target)?;
        // The far side is read from its own table, so its own grant applies —
        // reaching a record through an edge is not a way around one.
        let far_visible = self.visible_in(transaction, target_table)?;
        let mut reached: BTreeMap<RecordId, Value> = BTreeMap::new();
        for (_, neighbour) in edges {
            if neighbour.table != target_table {
                continue;
            }
            let address = RecordAddress::new(
                context.namespace,
                context.database,
                target_table,
                neighbour.id.clone(),
            );
            // A deleted neighbour drops out of the walk rather than failing it,
            // as it does on the edge-table path: a record can go while an entry
            // still names it, and that is a state of the graph.
            if let Some(payload) = transaction.get(&address)? {
                reached.insert(
                    neighbour.id.clone(),
                    self.record_of(&payload, &far_visible)?,
                );
            }
        }
        let next = reached
            .keys()
            .map(|id| RecordRef::new(target_table, id.clone()))
            .collect();
        Ok((reached.into_iter().collect(), next))
    }

    /// A read standing where a value stands.
    ///
    /// One record answers with its own value; a read of several answers with an
    /// array, so that the shape of the answer follows the shape of the question
    /// rather than the number of rows that happened to match.
    fn read_as_value(&self, transaction: &mut Transaction<'_>, select: &Select) -> Result<Value> {
        // The notes are dropped here, and this is the one place they are. An
        // expression position has no channel to carry them: the answer *is* a
        // value, and a value has no room beside it. Reported at the statement
        // that holds this one would be worse than silence — a note about an
        // inner read, attached to an outer answer it does not describe.
        // `None` for the deadline, for the same reason the notes are dropped: an
        // expression position has no channel to carry one *in* either, so a read
        // standing here enforces its own ceiling and not its caller's.
        //
        // The held ceiling is the one thing that does reach here, and this is the
        // position that most needs it: the answer is a `Value` built whole, so an
        // unbounded read is an unbounded array, and there is no note channel a
        // truncating default could have reported through.
        let Answered { records, .. } =
            self.read(transaction, select, None, Ceiling::over(select))?;
        // `$node` alongside `Source::Record` because it is one record too: a
        // read of one answers with its own value, and wrapping it in an array of
        // one would make the shape of the answer follow the source rather than
        // the question.
        //
        // `ONLY` says the same thing about a source that could have answered
        // with many — `FROM ONLY users WHERE email = $e` — which is the half of
        // this rule the source alone cannot tell. The read has already refused
        // if more than one answered, so there is at most one here either way.
        if select.only.is_some() || matches!(select.from, Source::Record(_) | Source::Node) {
            return Ok(records
                .into_iter()
                .next()
                .map_or(Value::None, |(_, value)| value));
        }
        Ok(Value::Array(
            records.into_iter().map(|(_, value)| value).collect(),
        ))
    }
}

/// The record identity a range bound names.
///
/// A key is a record id, so a bound has to be one of the four kinds an identity
/// has; anything else is a bound that could never match a key.
pub(crate) fn key_bound(value: &Value, span: Span) -> Result<RecordId> {
    match value {
        Value::Number(Number::Integer(id)) => Ok(RecordId::Int(*id)),
        Value::String(text) => Ok(RecordId::Text(text.clone())),
        Value::Uuid(bytes) => Ok(RecordId::Uuid(*bytes)),
        Value::Bytes(bytes) => Ok(RecordId::Bytes(bytes.clone())),
        _ => Err(Error::InvalidKeyBound { span }),
    }
}

/// Whether a key falls inside a bound pair.
pub(crate) fn within(id: &RecordId, start: &RecordId, end: &RecordId, inclusive: bool) -> bool {
    if id < start {
        return false;
    }
    if inclusive { id <= end } else { id < end }
}

/// What a source produced: the records, how they were reached, and what its
/// searched fields need.
///
/// The searched context travels with the records because a sort key is an
/// expression too, and one holding a `MATCHES` or a score must mean the same
/// thing there as in the `WHERE` that produced them.
/// The ordered index that serves a join key, when there is one.
///
/// Ordered and single-field only. A search index holds terms rather than whole
/// values and a vector index answers a distance, so neither can answer "which
/// records hold exactly this"; a composite index answers a question about its
/// first field and this is not that question unless it is the only field.
/// File records into an ordered map under the value at one route.
///
/// A record with nothing at the route contributes nothing: `NONE` is a value
/// and the other side would have to carry it to match, which is what an inner
/// join means.
fn collect_by_key(
    into: &mut BTreeMap<Value, Vec<(RecordId, Value)>>,
    records: Vec<(RecordId, Value)>,
    key: &tessari_ql::FieldPath,
) {
    for (id, record) in records {
        let Some(found) = key.path.resolve(&record).cloned() else {
            continue;
        };
        into.entry(found).or_default().push((id, record));
    }
}

/// Kind names as a reader would say them: `record`, or `record and string`.
///
/// A join key usually holds one kind, so the common message reads as a bare
/// noun rather than as a set with one element in it.
fn listed(kinds: &BTreeSet<&'static str>) -> String {
    let held: Vec<&str> = kinds.iter().copied().collect();
    match held.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

pub(crate) fn ordered_index_on(
    transaction: &mut Transaction<'_>,
    table: TableId,
    key: &tessari_ql::FieldPath,
) -> Result<Option<tessari_storage::IndexDefinition>> {
    if !transaction.indexes_are_current()? {
        return Ok(None);
    }
    Ok(Catalog::new(transaction)
        .indexes_on(table)?
        .into_iter()
        .find(|held| {
            held.is_ordered() && held.fields.len() == 1 && held.fields.first() == Some(&key.path)
        }))
}

/// What a join produces, which is still a collection.
///
/// A join builds a map of one side and probes it with the other, so its work is
/// not per-record and streaming it would move the materialisation rather than
/// remove it. Named separately so the difference is visible in the signature
/// rather than resting on a comment.
type Joined = (Vec<(RecordId, Value)>, Plan, Searched);

/// What one hop over adjacency reached, and where the next hop starts.
///
/// Both halves are lists of the same length only by coincidence, and the second
/// is empty whenever the step named no node — so they are named rather than left
/// as a tuple two `Vec`s wide that a caller could read in either order.
type Hopped = (Vec<(RecordId, Value)>, Vec<RecordRef>);

/// The records a read reached, how it reached them, and whether reaching them
/// settled the condition.
///
/// A struct rather than a triple for the reason `Answered` is one: the last
/// field is a bare `bool` that a caller could silently drop or, worse, read the
/// wrong way round. Naming it makes `answered: false` — which is what a scan and
/// every ordinary index read say — a statement rather than a position.
struct Reached {
    /// The records to test, or to answer with when `answered`.
    records: Candidates,
    /// How they were reached, as `EXPLAIN` would report it.
    plan: Plan,
    /// Whether the read has already settled the whole condition.
    ///
    /// `false` unless a search index answered a plain conjunction that was the
    /// entire `WHERE`, over a field this session may read — see
    /// [`Session::trusts`]. A caller that ignores this is correct and slower,
    /// which is the right way round for a field of this kind.
    answered: bool,
}

/// How an index-served read's candidates are available to the caller.
///
/// Every index read produces a **candidate set** the condition then refines, and
/// for most of them that set is built before the first record can be tested.
/// A range is the exception: its entries can be named in one pass and its
/// records read afterwards, so a caller that fills its bound can stop the fetch
/// it has not reached yet.
///
/// Why only the fetch, and not the entry walk: the answer is in record order —
/// a bounded read answers the records a scan of the same predicate answers, and
/// nothing else, which the tests in `bounded_index_reads.rs` pin — and the
/// lowest identity among the candidates is not known until every candidate has
/// been named. A walk that stopped early would answer with whichever records the
/// index reached first, which for an index whose order is not identity order is
/// a different set of records. So the entry walk runs to the end by
/// construction, and what the bound reaches is the half whose cost grows with
/// the answer.
enum Candidates {
    /// Built whole before the first one can be tested.
    Held(Vec<(RecordId, Value)>),
    /// A range the caller can walk, stopping where its answer fills.
    Range {
        index: Box<IndexDefinition>,
        fixed: Vec<Value>,
        lower: Option<Value>,
        upper: Option<Value>,
    },
}

/// What a vector walk came back with, and the index that answered it.
///
/// No `Walked` here: every empty return is a shape this walk does not serve — no
/// index on the path, one built for another distance, a query that is not a
/// vector — and none of them is an index that ran out.
type Approximated = (Vec<(RecordId, Value)>, String);

/// What a read produced, and what it has to say about how.
///
/// A struct rather than the tuple this was, because the third element is the one
/// a caller is most likely to drop on the floor — and a `_` in a tuple pattern
/// says nothing about what was dropped, while a named field does.
pub(crate) struct Answered {
    /// The records, in the order the statement asked for.
    pub records: Vec<(RecordId, Value)>,
    /// How they were reached — the plan the read took, in the structure
    /// `EXPLAIN` answers with.
    pub plan: Plan,
    /// What the read did that the records do not show.
    pub notes: Vec<Note>,
    /// What the query might have meant, when it named a term nothing holds.
    ///
    /// Carried from the searched context rather than computed here, because it
    /// is a fact about the query and the collection and not about the read: it
    /// is resolved before an access path exists, so that planning a read
    /// differently cannot give it a different suggestion.
    pub suggestion: Option<Suggestion>,
}

/// The note a materialised read owes, when it reached the ceiling it stated.
///
/// A materialised source and a materialised join side are the same case seen
/// twice — the outer statement asks its question of whatever the inner read
/// handed over, and a prefix of an answer and a whole one are the same shape. A
/// top-level read filling its own `LIMIT` is *not* this: there is no outer
/// question for it to have misled, and the caller wrote the bound and can see
/// how many records came back.
fn ceiling_reached(read: &Select, held: usize) -> Option<Note> {
    let ceiling = read.limit?;
    u64::try_from(held)
        .is_ok_and(|held| held >= ceiling)
        .then_some(Note::SubqueryCeiling { rows: ceiling })
}

/// Whether the read did what the statement said it expected.
///
/// A refusal and never a router: nothing here reaches the planner, and a read
/// with no `USING` is not touched. It is compared against the plan the read
/// **took**, not the one the planner chose, which is the whole difference — an
/// ordered index that could not fill the bound hands the read to the scan, and
/// an assertion checked against the intention would pass in exactly the case it
/// was written to catch.
///
/// The cost of a refused statement is the read it already did. That is the
/// honest semantics and not an oversight: the assertion is about what happened,
/// so it cannot be settled before anything has. Refusing early where the
/// planner's own choice already contradicts the assertion is a real improvement
/// and a separate one (Q-203), because the planner may name a path the read then
/// falls back from.
/// `SPLIT ON <route>` — one record per element of the array the route reaches.
///
/// # What each shape at the route means
///
/// **An array** is the case the clause is for: one record per element, each
/// carrying the element where the array stood, so `GROUP BY tags` after a
/// `SPLIT ON tags` groups by a tag. The identity is carried unchanged onto every
/// row, so an answer may hold one id more than once — which is what "one row per
/// element" means and is why the clause is written rather than implied.
///
/// **An empty array** answers with no rows at all. Zero elements, zero rows: any
/// other rule would make the count depend on a special case, and a read that
/// asked for a row per tag over a record with no tags asked for nothing.
///
/// **Anything else — an absence, a scalar, an object — passes through once,
/// unchanged.** An array says "these are the elements"; an absence says nothing
/// about elements at all, so it is not an empty one. In a store where a field's
/// kind is per record rather than per table, the alternative is a read that
/// refuses because one record out of ten thousand holds a string.
///
/// # The budget
///
/// This is a stage of the read in the sense [`Budget::stage`] means, and it is
/// the one stage that can produce *more* records than it consumed — so it is
/// counted, or a held read could pass its ceiling here after honouring it above.
fn opened(
    records: Vec<(RecordId, Value)>,
    route: &Path,
    budget: &mut Budget,
) -> Result<Vec<(RecordId, Value)>> {
    budget.stage();
    let mut opened = Vec::with_capacity(records.len());
    for (id, record) in records {
        let Some(Value::Array(items)) = route.resolve(&record).cloned() else {
            budget.spend()?;
            opened.push((id, record));
            continue;
        };
        for item in items {
            budget.spend()?;
            let mut row = record.clone();
            if let Some(slot) = route.resolve_mut(&mut row) {
                *slot = item;
            }
            opened.push((id.clone(), row));
        }
    }
    Ok(opened)
}

/// `ONLY` is an assertion about how many records answer, and this is where it is
/// tested.
///
/// After the bound, so `FROM ONLY users LIMIT 1` is the author saying which one
/// they want rather than a contradiction.
///
/// **None passes, more than one refuses**, and the two are not the same mistake.
/// `ONLY` asserts *at most* one, so an absence is a legitimate answer to a
/// question about one thing — refusing it would make
/// `SELECT * FROM ONLY users:99 ?? {}` unsayable, and that is the shape `??`
/// exists for. More than one falsifies what the author wrote, and it refuses
/// rather than answering with the first: the records found are already correct,
/// so a prefix of them costs nothing and looks exactly like success.
/// Whether this read's cursor is served by seeking rather than by walking.
///
/// True for exactly one shape, and the reason is the keyspace rather than a
/// preference: a record's key is its table prefix followed by its identity, so a
/// read that answers in the store's own order can begin at a position in that
/// keyspace. Both halves are load-bearing.
///
/// An `ORDER BY` breaks it because the answer's order is then the key the author
/// named, and a record sorting before the anchor by that key may sort after it
/// by identity — so seeking would drop records the page is owed.
///
/// A source other than a plain table breaks it because its records do not come
/// from that keyspace in that order: a condition may be served by an index, a
/// walk arrives along edges, a join and a materialised read build their rows.
/// Each of those is walked and says so.
fn sought(select: &Select) -> bool {
    select.after.is_some() && select.order.is_empty() && matches!(select.from, Source::Table(_))
}

fn alone(select: &Select, records: &[(RecordId, Value)]) -> Result<()> {
    let Some(span) = select.only else {
        return Ok(());
    };
    if records.len() <= 1 {
        return Ok(());
    }
    Err(Error::NotAlone {
        found: records.len(),
        span,
    })
}

fn asserted(select: &Select, plan: &Plan) -> Result<()> {
    match &select.using {
        None => Ok(()),
        Some(Using::Path(named)) => {
            let Some(wanted) = AccessPath::named(&named.text) else {
                return Err(Error::NoSuchAccessPath {
                    named: named.text.clone(),
                    known: AccessPath::known(),
                    span: named.span,
                });
            };
            if wanted == plan.access {
                return Ok(());
            }
            Err(Error::PathNotTaken {
                expected: wanted.name().to_owned(),
                took: plan.access.name().to_owned(),
                span: named.span,
            })
        }
        Some(Using::Index(named)) => {
            if plan.index.as_deref() == Some(named.text.as_str()) {
                return Ok(());
            }
            Err(Error::IndexNotUsed {
                expected: named.text.clone(),
                // Named rather than described, because "used `by_city`" is what
                // an author has to see to know what went wrong; "no index" is
                // the other thing that can be true and reads as a sentence in
                // the same slot.
                took: plan
                    .index
                    .clone()
                    .map_or_else(|| "no index".to_owned(), |index| format!("`{index}`")),
                span: named.span,
            })
        }
    }
}

/// What an index-served walk came back with.
///
/// Three cases rather than an [`Option`], because coming back empty happens for
/// two unrelated reasons and only one of them is worth telling anybody about.
/// **No index holds this order** is the ordinary state of a table nobody has
/// indexed; **an index holds it and could not fill the bound** is the case the
/// index was built to prevent. Collapsed into `None` they are the same value,
/// and a note raised on it would fire on every unindexed read — which is how a
/// diagnostic becomes noise and then becomes ignored.
///
/// The planner cannot tell them apart either: `plan::ordered` reads the
/// statement and never the schema, so it says `Some` for an `ORDER BY … LIMIT`
/// over a table with no index at all.
enum Walked {
    /// The index answered, and named itself so the plan can report which one.
    Served {
        /// What it came back with.
        found: Vec<(RecordId, Value)>,
        /// The index that served it.
        index: String,
    },
    /// An index holds this order and could not fill the bound.
    Declined,
    /// No index holds this order, so nothing was given up.
    NotServed,
}

/// What resolving a source reached, and what producing it still needs.
///
/// Three cases rather than five, because what matters here is not which clause
/// was written but whether the records exist yet.
enum Prepared<'a> {
    /// A table, resolved to its tenancy. Nothing has been read.
    Table(Context, TableId),
    /// A table and the condition its records must satisfy. The condition is
    /// carried rather than re-matched out of the statement, so producing needs
    /// no arm that cannot happen.
    Filtered(Context, TableId, &'a Expr),
    /// A source whose records exist already, because reaching its context meant
    /// reading them: one record by identity, a traversal, a join. Each is a
    /// barrier in its own right — a join builds a map of one side — so producing
    /// lazily would move the materialisation rather than remove it.
    Held(Vec<(RecordId, Value)>, Plan),
}

/// The table a source names, for the plan that reports it.
///
/// A traversal, a join and a materialised source name none: each reaches records
/// from more than one place, or from a read rather than a table.
fn table_named(source: &Source) -> Option<&str> {
    match source {
        Source::Table(table) | Source::Where { table, .. } | Source::Range { table, .. } => {
            Some(table.name.text.as_str())
        }
        Source::Record(target) => Some(target.table.name.text.as_str()),
        Source::Node | Source::Traverse { .. } | Source::Join { .. } | Source::Subquery { .. } => {
            None
        }
    }
}

/// This node, as the one record `$node` answers.
///
/// The id sits beside the value rather than inside it, which is where a record's
/// id sits everywhere else in this store — so a caller reads it the same way it
/// reads any other answer, and no projection has to learn a special field.
fn node_row(store: &Store) -> Result<(RecordId, Value)> {
    let identity = store.node_identity()?;
    let mut fields = BTreeMap::new();
    fields.insert(
        "roles".to_owned(),
        Value::Array(
            identity
                .roles
                .names()
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    );
    fields.insert(
        "membership".to_owned(),
        Value::from(identity.membership.name()),
    );
    // The one field here that moves. A caller asking what a node is running is
    // asking the same question an upgrade asks, and this is where both look.
    fields.insert(
        "version".to_owned(),
        Value::from(identity.version.to_string().as_str()),
    );
    // Beside it rather than instead of it, because the two answer different
    // questions. `version` is the stored, ordered form an upgrade compares;
    // `build` is what this binary actually is, pre-release suffix included. On
    // a final release they read the same, which is the point — the difference
    // only appears when there is one.
    fields.insert("build".to_owned(), Value::from(BUILD_VERSION));
    fields.insert(
        "endpoints".to_owned(),
        Value::Array(
            identity
                .endpoints
                .iter()
                .map(|endpoint| Value::from(endpoint.as_str()))
                .collect(),
        ),
    );
    Ok((identity.record_id(), Value::Object(fields)))
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

/// Remember one score if it is among the best `wanted` seen so far.
///
/// Kept ascending and capped, so `best[0]` is the score in last place — the
/// threshold a pruning walk compares a term suffix against. A shorter list is a
/// read that has not yet seen enough records to have a last place, which is why
/// the caller checks the length before reading the front.
fn keep_best(best: &mut Vec<f64>, score: f64, wanted: usize) {
    if best.len() >= wanted && score <= best[0] {
        return;
    }
    let at = best.partition_point(|seen| *seen < score);
    best.insert(at, score);
    if best.len() > wanted {
        best.remove(0);
    }
}

/// A count of repeats as a weight, without an `as` cast.
fn as_count(repeats: usize) -> f64 {
    f64::from(u32::try_from(repeats).unwrap_or(u32::MAX))
}

/// Whether the stages left between the source and the answer are all per-record.
///
/// Two are not, and both keep the collecting path: a `FETCH` batches every
/// reference into one ask, which needs every record in hand before the first one
/// is resolved (G004 C9, and ADR-0014 decided that criterion wins); and a
/// grouping folds many records into one.
///
/// A read with no `ORDER BY` keeps it too, and that one is not a barrier — it is
/// that the ordering stage is where the saving lives, and with no key it would
/// order by record id instead, which is a different answer from the one a scan
/// gives.
fn streams(select: &Select) -> bool {
    // `SPLIT` joins `FETCH` on the barrier side rather than becoming a stage of
    // the streaming path: it changes how many records there are, and the
    // ordering stage below it keeps only as many as the bound can still reach.
    // Streamed, the two would decide that together — the sort discarding rows
    // the split had not produced yet.
    select.fetch.is_empty() && select.split.is_none() && !select.order.is_empty() && !groups(select)
}

/// Whether the read folds many records into one.
///
/// Named once and asked twice — by the test above and by the projection stage —
/// because the two must not drift apart. A grouping routed to the streaming path
/// would be a fold evaluated against one record at a time, which is the one
/// thing a fold is not.
pub(crate) fn groups(select: &Select) -> bool {
    match &select.projection {
        Projection::All => false,
        Projection::Values { values, .. } => folds(values) || !select.group.is_empty(),
    }
}

/// How many records a collecting read may stop at, when it may stop at all.
///
/// A `LIMIT` bounds the **answer**. It becomes a bound on the **source** exactly
/// when the records the source produces are, in order, the records the answer
/// holds — and this is the read that has no ordering stage between the two, so
/// for it that is a question about the statement's shape and nothing else.
///
/// Stated as a whitelist, for ADR-0013's reason: a blacklist makes every clause
/// somebody adds later a silent short answer until they remember this function.
/// Each condition names a way the two sets differ. An `ORDER BY` decides which
/// records the answer holds after the source has produced them. An `AFTER`
/// cursor drops records at the front, so a count taken here is not the page's.
/// A `SPLIT` changes how many records there are. A grouping or a fold makes the
/// bound count groups, and stopping the source would cut a group's input instead
/// of the answer. A `FETCH` is excluded because it holds the whole set to batch
/// its references, which is what put this read on the collecting path to begin
/// with.
///
/// Where none of those hold, the answer is a prefix of what the source produced,
/// the rest of the table cannot change it, and reading it cost 83.3 ms to answer
/// with one record found third of a hundred thousand (Q-72).
fn held_bound(select: &Select) -> Option<usize> {
    if !select.order.is_empty()
        || select.after.is_some()
        || !select.fetch.is_empty()
        || select.split.is_some()
        || groups(select)
    {
        return None;
    }
    order_bound(select)
}

/// How many records the ordering stage may keep.
///
/// The start is added because `bounded` skips before it truncates, so a record
/// the start will discard still has to survive the sort to be discarded from the
/// right place.
fn order_bound(select: &Select) -> Option<usize> {
    select.limit.map(|limit| {
        usize::try_from(limit.saturating_add(select.start.unwrap_or(0))).unwrap_or(usize::MAX)
    })
}

/// Whether a route names this field at the top of the record.
///
/// A route with steps below it names something *inside* the field, so the field
/// itself stays — which is why the deeper case is handled after the copy rather
/// than by filtering it out here.
fn omits(omit: &[FieldPath], name: &str) -> bool {
    omit.iter()
        .any(|route| route.path.steps().is_empty() && route.path.root() == name)
}

/// Remove what a route names from inside an already-copied record.
///
/// Silent where the route reaches nothing: a record that does not hold the field
/// already answers without it, and there is nothing for an error to tell anyone.
fn omit_within(fields: &mut BTreeMap<String, Value>, route: &Path) {
    let Some((Step::Field(last), above)) = route.steps().split_last() else {
        return;
    };
    let mut held = Value::Object(std::mem::take(fields));
    let holder = Path::new(route.root().to_owned(), above.to_vec());
    if let Some(Value::Object(inside)) = holder.resolve_mut(&mut held) {
        inside.remove(last);
    }
    if let Value::Object(back) = held {
        *fields = back;
    }
}

/// What a read's projection produces, worked out once above the records.
///
/// One type rather than three parameters, because the three are one decision —
/// what the answer is built from — and they were about to be added to the same
/// signatures one at a time, which is the drift `Reporting` was made to stop.
#[derive(Debug)]
pub(crate) struct Shaped {
    /// Whether the record's own fields start the answer.
    pub(crate) everything: bool,
    /// The routes the record's fields must not reach the answer by.
    ///
    /// Subtracts from what the star put there and from nothing else, so it is
    /// empty and unread whenever `everything` is false — which the grammar
    /// already guarantees by refusing `OMIT` without a `*`.
    pub(crate) omit: Vec<FieldPath>,
    /// The values written out by name, with their constant parts folded once.
    pub(crate) values: Vec<Projected>,
}

/// The two channels a read reports on, which travel together everywhere.
///
/// A note the source *decides* to raise — a fall-back, an approximate path, a
/// subquery that reached its ceiling — is pushed straight onto `collected`. A
/// note the *evaluator* discovers while comparing values is recorded in
/// `noticed` and drained when the read reports. One parameter rather than two,
/// because they were being added to the same signatures one at a time and were
/// drifting apart at the call sites.
#[derive(Debug)]
/// What a walk needs to test a condition against each record it finds.
///
/// The three travel together because they are one question asked once per
/// record — does this record satisfy the statement's `WHERE` — and each is
/// meaningless to the walk without the other two: the condition to evaluate, the
/// analyzers its searched fields are read with, and where a comparison across
/// two kinds is recorded so the answer can say it happened.
struct Testing<'a> {
    /// The statement's whole condition.
    condition: &'a Expr,
    /// The analyzers the condition's searched fields were resolved with.
    searched: &'a Searched,
    /// Where the evaluator records a comparison across two kinds.
    noticed: &'a Noticed,
}

pub(crate) struct Reporting<'a> {
    /// Notes the source raised.
    pub(crate) collected: &'a mut Vec<Note>,
    /// Where the evaluator records a comparison across two kinds.
    pub(crate) noticed: &'a Noticed,
}

/// What the evaluator can see besides the expression itself.
///
/// The record a condition is being tested against, and what its searched fields
/// need. Both are absent in a value position, where there is no record and
/// nothing to search.
#[derive(Clone, Copy, Default)]
pub(crate) struct Scope<'a> {
    /// The record being tested, when there is one.
    pub(crate) record: Option<&'a Value>,
    /// Which record that is, when it is a stored one.
    ///
    /// A record's *value* answers `MATCHES`, because holding a word is a property
    /// of the text alone. A **score** additionally needs what the index knows
    /// about this record — how often it holds each asked term, and how long it is
    /// — and an index is addressed by record id. So the id travels beside the
    /// value rather than being recovered from it.
    ///
    /// Absent where there is no stored record to name: a joined row, a fold's
    /// result, an expression in a value position. Such a row is in no index, and
    /// a score against it is refused for the same reason a score without an index
    /// is.
    id: Option<&'a RecordId>,
    /// The analyzers and collection statistics the searched paths need.
    searched: Option<&'a Searched>,
    /// Where a comparison across two kinds is recorded, when this evaluation is
    /// part of a read that reports notes.
    ///
    /// Borrowed, so it cannot outlive the read — which is the whole reason it
    /// hangs here rather than on the session.
    noticed: Option<&'a Noticed>,
}

impl<'a> Scope<'a> {
    /// No record at all.
    ///
    /// For an expression that has none to read: a fold's value substituted into
    /// its projection is arithmetic over a literal, and a path standing beside
    /// one would be a value per record where a value per group belongs — which
    /// the grouping rule refuses before anything runs.
    pub(crate) const fn none() -> Self {
        Self {
            record: None,
            id: None,
            searched: None,
            noticed: None,
        }
    }

    /// A record, with nothing searched.
    pub(crate) const fn of(record: &'a Value) -> Self {
        Self {
            record: Some(record),
            id: None,
            searched: None,
            noticed: None,
        }
    }

    /// A record, and what its searched fields need.
    pub(crate) const fn searching(record: &'a Value, searched: &'a Searched) -> Self {
        Self {
            record: Some(record),
            id: None,
            searched: Some(searched),
            noticed: None,
        }
    }

    /// The same scope, over a record the store can name.
    ///
    /// Left off where the value in scope is not a stored record, which is what
    /// makes the absence meaningful rather than an omission somebody forgot.
    pub(crate) const fn identified(self, id: &'a RecordId) -> Self {
        Self {
            id: Some(id),
            ..self
        }
    }

    /// The same scope, reporting what it compares to this read's notes.
    ///
    /// Added by the read path and left off everywhere else, so an evaluation in
    /// a value position — which has no answer to hang a note on — costs nothing
    /// and says nothing.
    pub(crate) const fn noticing(self, noticed: &'a Noticed) -> Self {
        Self {
            noticed: Some(noticed),
            ..self
        }
    }

    /// The same environment, over this record.
    ///
    /// A `Scope` with no record is what an evaluation needs *besides* the record
    /// — the analyzers, and where to note a crossing — so a walk that evaluates
    /// per record is handed one of those and attaches each record in turn. It is
    /// one parameter where `searched` and `noticed` were two, and it stops the
    /// pair drifting apart at the call sites.
    /// It takes the id as well as the value, so that a scope carrying the
    /// identity of the *previous* record is not a thing this type can hold.
    pub(crate) const fn with(self, id: &'a RecordId, record: &'a Value) -> Self {
        Self {
            record: Some(record),
            id: Some(id),
            ..self
        }
    }

    /// The environment alone: what evaluation needs besides a record.
    pub(crate) const fn over(searched: &'a Searched, noticed: &'a Noticed) -> Self {
        Self {
            record: None,
            id: None,
            searched: Some(searched),
            noticed: Some(noticed),
        }
    }

    /// Record a comparison, when this scope is reporting them.
    fn compared(self, left: &Value, right: &Value) {
        if let Some(noticed) = self.noticed {
            noticed.compared(left, right);
        }
    }

    /// The analyzer this path's field declares, if it declares one.
    fn analyzer(self, path: &Path) -> Option<&'a Analyzer> {
        self.searched.and_then(|held| held.analyzer(path))
    }

    /// What this path was ranked against, if it was ranked at all.
    fn ranked(self, path: &Path) -> Option<&'a Ranked> {
        self.searched.and_then(|held| held.ranked(path))
    }

    /// What this read asked of this path, as the rewrite recorded it.
    fn wanted(self, path: &Path) -> &'a [(BinaryOp, String)] {
        self.searched.map_or(&[], |held| held.wanted(path))
    }
}

/// The expressions a read evaluates besides its condition: what it projects,
/// and what it orders by.
///
/// A fold is left out. `search::score` inside one would be scoring the group
/// rather than the record, which is a different question and is not this one.
fn shown(select: &Select) -> Vec<&Expr> {
    let mut found = Vec::new();
    if let Projection::Values { values: wanted, .. } = &select.projection {
        for projected in wanted {
            found.push(&projected.value);
        }
    }
    for ordering in &select.order {
        found.push(&ordering.key);
    }
    found
}

/// A byte offset as a value a caller can read.
///
/// `try_from` rather than a cast, which would wrap silently at a width the
/// types no longer show. The saturation it guards is unreachable — a text long
/// enough to overflow `i64` would need eight exabytes to hold it — and it is
/// written anyway because a bound is a better answer than a panic in a
/// projection over somebody's whole table.
fn at(offset: usize) -> Value {
    Value::Number(Number::Integer(i64::try_from(offset).unwrap_or(i64::MAX)))
}

/// The two ends of a span of identities, and whether the upper one is inside it.
///
/// One argument rather than three for the reason the storage side gives about
/// the same three values: they are one fact and are wrong together — a caller
/// handed the bounds and not the inclusivity silently removes a half-open span
/// as a closed one, and nothing in the answer says which it was.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IdentitySpan<'a> {
    /// The first identity, always inside the span.
    pub(crate) lower: &'a Identity,
    /// The last, inside only when `inclusive`.
    pub(crate) upper: &'a Identity,
    /// Whether the upper bound is itself inside.
    pub(crate) inclusive: bool,
    /// Where the span sits, for a refusal about an unbound parameter.
    pub(crate) at: Span,
}
