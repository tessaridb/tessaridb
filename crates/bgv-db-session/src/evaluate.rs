//! Turning an expression into a value.
//!
//! Two of the forms are reads, and that is the whole of what makes the models
//! compose. A `GET` inside a record statement runs **in the same transaction**,
//! so it sees the same snapshot as the statement around it — two models that
//! cannot share a snapshot are two databases sharing a process.

use core::ops::Bound;
use std::collections::BTreeMap;

use bgv_db_constants::ORDERED_FILTER_REACH;
use bgv_db_ql::{
    BinaryOp, Direction, Expr, ExprKind, Function, Hop, Projected, Projection, RecordTarget,
    Select, Source, Span, TableRef,
};
use bgv_db_storage::{Catalog, RecordAddress, Store, Transaction};
use bgv_db_types::{
    Analyzer, Number, Path, RecordId, RecordRef, TableId, Value, ValueRange, apply,
};

use crate::aggregate::folds;
use crate::arithmetic::{arithmetic, negate};
use crate::call::call;
use crate::condition::boolean;
use crate::consume::Consumer;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::AccessPath;
use crate::plan;
use crate::rank::{Corpus, score};
use crate::search::{Searched, matches_terms};
use crate::session::Session;

/// How many of an index's leading values an index-served order compares.
///
/// One, because `plan::ordered` refuses a second sort key — so the field the
/// order names is the only one a tie group may be defined by. It is the width of
/// the comparison rather than a tunable, which is why it lives here beside the
/// reads that use it and not in `bgv-db-constants`.
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
                    return Ok(Value::Bool(held.into_iter().any(|value| {
                        if *op == BinaryOp::Matches {
                            matches_terms(analyzer, value, &other)
                        } else {
                            apply(*op, value, &other)
                        }
                    })));
                }
                let held = self.evaluate_in(transaction, left, scope)?;
                // A term match is the one test that needs the *schema*: which
                // analyzer turns this field's text into terms is a property of
                // the field, so that both a scan and an index ask the same
                // question of it.
                if *op == BinaryOp::Matches {
                    let analyzer = match &left.kind {
                        ExprKind::Path(field) => scope.analyzer(&field.path),
                        _ => None,
                    };
                    return Ok(Value::Bool(matches_terms(analyzer, &held, &other)));
                }
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
    /// table is a very large commit. Bounding it is `LIMIT` on a delete, which
    /// is a different statement and is not built.
    pub(crate) fn delete_where(
        &self,
        transaction: &mut Transaction<'_>,
        table: &TableRef,
        condition: &Expr,
    ) -> Result<crate::outcome::Outcome> {
        let (context, id) = self.resolve_table(transaction, table)?;
        let searched = self.searched_for(transaction, id, &[condition])?;
        let (candidates, _) = self.candidates(transaction, id, context, condition, &searched)?;

        let mut removed = 0_u64;
        for (record_id, record) in candidates {
            // Tested against the whole condition, exactly as a read is: the
            // index narrowed, and the condition decides. A delete that trusted
            // the narrowing would remove records the statement did not name.
            let held =
                self.evaluate_in(transaction, condition, Scope::searching(&record, &searched))?;
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
    ) -> Result<(Vec<(RecordId, Value)>, AccessPath)> {
        // The searched context is resolved before the source produces anything,
        // because a sort key is an expression too and one holding a `MATCHES` or
        // a score must mean the same thing there as it does in the `WHERE` that
        // produced them. That is the whole reason the source is split in two: it
        // knew `searched` before it walked, and only reported it on return, so
        // the consumer could not be built until every record already existed.
        let (prepared, searched) = self.prepare_source(transaction, select)?;
        // The bound the sort may keep to. `bounded` is applied to the ordering
        // stage's output below, so keeping only what it will keep is an identity
        // between two adjacent stages rather than a decision about the
        // statement — which is why, unlike the bound handed to the source, this
        // one needs no whitelist of shapes (ADR-0013).
        let bound = order_bound(select);

        if streams(select) {
            // The projection folded once, above the records rather than per
            // record: a projection's constant parts are constant across every
            // record it is applied to.
            let wanted = match &select.projection {
                Projection::All => None,
                Projection::Values(wanted) => Some(self.folded_projection(transaction, wanted)?),
            };
            let keys = self.folded_order(transaction, &select.order)?;
            let mut shaping = crate::consume::Shaping::new(
                self,
                wanted,
                keys,
                &searched,
                crate::shape::Topmost::keeping(&select.order, bound),
            );
            let path =
                self.produce_source(transaction, select, prepared, &searched, &mut shaping)?;
            return Ok((
                crate::shape::bounded(shaping.finish(), select.start, select.limit),
                path,
            ));
        }

        let mut collecting = crate::consume::Collecting::new();
        let path =
            self.produce_source(transaction, select, prepared, &searched, &mut collecting)?;
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
        let records = match &select.projection {
            Projection::All => records,
            Projection::Values(wanted) if groups(select) => {
                self.grouped(transaction, records, wanted, &select.group)?
            }
            Projection::Values(wanted) => {
                // Folded once, above the loop: a projection's constant parts are
                // constant across every record it is applied to, and rebuilding
                // them per record is what the benchmark harness found dominating
                // a nearest-neighbour read.
                let wanted = self.folded_projection(transaction, wanted)?;
                let mut projected = Vec::with_capacity(records.len());
                for (id, record) in records {
                    projected.push((id, self.project(transaction, &record, &wanted, &searched)?));
                }
                projected
            }
        };
        // Ordering comes after projection so that a key may name what the caller
        // can see: `SELECT address.city AS home … ORDER BY home` reads the name
        // the answer carries rather than the route it came from. A route still
        // works, because a projected record keeps the shape it was given only
        // where the projection preserved it — which is why the sort falls back
        // to the route when the name is not there.
        let records = if select.order.is_empty() {
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
            );
            for (id, record) in records {
                if shaping.take(transaction, id, record)?.is_break() {
                    break;
                }
            }
            shaping.finish()
        };
        Ok((
            crate::shape::bounded(records, select.start, select.limit),
            path,
        ))
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
        record: &Value,
        wanted: &[Projected],
        searched: &Searched,
    ) -> Result<Value> {
        let mut projected = BTreeMap::new();
        for value in wanted {
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
                Scope::searching(record, searched),
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
    fn rank(
        &self,
        transaction: &mut Transaction<'_>,
        arguments: &[Expr],
        scope: Scope<'_>,
        span: Span,
    ) -> Result<Value> {
        let (Some(first), Some(second)) = (arguments.first(), arguments.get(1)) else {
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
        let (Some(corpus), Some(analyzer)) =
            (scope.corpus(&field.path), scope.analyzer(&field.path))
        else {
            return Err(Error::NoSearchIndex {
                field: field.path.to_string(),
                span,
            });
        };
        let held = self.evaluate_in(transaction, first, scope)?;
        let wanted = self.evaluate_in(transaction, second, scope)?;
        Ok(score(corpus, analyzer, &held, &wanted))
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
    fn prepare_source<'a>(
        &self,
        transaction: &mut Transaction<'_>,
        select: &'a Select,
    ) -> Result<(Prepared<'a>, Searched)> {
        match &select.from {
            // Resolved from `meta` rather than read from a table, because that
            // is where it is: the identity is deliberately outside the log
            // (ADR-0018 §1). It needs no tenancy, so `$node` answers without a
            // `USE` — a node is not in a database.
            Source::Node => Ok((
                Prepared::Held(vec![node_row(self.store)?], AccessPath::Record),
                Searched::default(),
            )),
            Source::Record(target) => {
                let (_, address) = self.address(transaction, target)?;
                let visible = self.visible_in(transaction, address.table)?;
                let found = match transaction.get(&address)? {
                    Some(payload) => {
                        vec![(address.id, self.record_of(&payload, &visible)?)]
                    }
                    None => Vec::new(),
                };
                Ok((
                    Prepared::Held(found, AccessPath::Record),
                    Searched::default(),
                ))
            }
            Source::Table(table) => {
                let (context, id) = self.resolve_table(transaction, table)?;
                let searched = self.searched_for(transaction, id, &shown(select))?;
                Ok((Prepared::Table(context, id), searched))
            }
            Source::Traverse {
                from,
                direction,
                hops,
            } => {
                let (found, path) = self.traverse(transaction, from, *direction, hops)?;
                Ok((Prepared::Held(found, path), Searched::default()))
            }
            Source::Where { table, condition } => {
                let (context, id) = self.resolve_table(transaction, table)?;
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
                let (found, path, searched) = self.join(
                    transaction,
                    select,
                    left,
                    right,
                    left_key,
                    right_key,
                    condition.as_deref(),
                )?;
                Ok((Prepared::Held(found, path), searched))
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
    ) -> Result<AccessPath> {
        match prepared {
            Prepared::Held(found, path) => {
                hand_over(found, transaction, consumer)?;
                Ok(path)
            }
            Prepared::Table(context, id) => {
                // A statement that asked for an approximate ordering, over a
                // field carrying a graph built for the distance it named, is the
                // one read in this store an index answers differently from a
                // scan. Every other shape falls through to the scan below, which
                // is exact.
                if let Some(walk) = plan::nearest(select)
                    && let Some(found) = self.walk(transaction, context, id, &walk)?
                {
                    hand_over(found, transaction, consumer)?;
                    return Ok(AccessPath::Index);
                }
                // The other shape an index serves without a condition: an order
                // it is already stored in, and a bound to stop at. Exact — the
                // records come back for the ordering stage and `bounded` to
                // shape, the same two the other answers go through.
                if let Some(bound) = plan::ordered(select)
                    && let Some(found) = self.walk_in_order(transaction, context, id, &bound)?
                {
                    hand_over(found, transaction, consumer)?;
                    return Ok(AccessPath::Ordered);
                }
                // The bound reaches the source here, and only here, because this
                // is the one arm where the records the source produces are the
                // records the answer holds. `plan::bound` returns nothing for
                // every shape where they differ (ADR-0013).
                let found = match plan::bound(select) {
                    Some(wanted) => transaction.first_records_of(
                        context.namespace,
                        context.database,
                        id,
                        wanted,
                    )?,
                    None => transaction.scan_table(context.namespace, context.database, id)?,
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
                Ok(AccessPath::Scan)
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
                if let Some(bound) = plan::ordered(select)
                    && bound.descending
                    && let Some(found) = self.descend_matching(
                        transaction,
                        context,
                        id,
                        &bound,
                        condition,
                        searched,
                    )?
                {
                    hand_over(found, transaction, consumer)?;
                    return Ok(AccessPath::Ordered);
                }
                let (candidates, path) =
                    self.candidates(transaction, id, context, condition, searched)?;

                // No `expecting` here, deliberately: how many candidates survive
                // the condition is not known until it has been run, and an
                // over-estimate would reserve exactly the memory this contract
                // exists to give back.
                //
                // The candidates are tested against the **whole** condition, not
                // only the conjunct the index answered. That is what makes an
                // index a narrowing device rather than an answer, and it is why
                // adding one still cannot change what a query returns.
                for (id, record) in candidates {
                    let held = self.evaluate_in(
                        transaction,
                        condition,
                        Scope::searching(&record, searched),
                    )?;
                    if boolean(&held, condition.span)?
                        && consumer.take(transaction, id, record)?.is_break()
                    {
                        break;
                    }
                }
                Ok(path)
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
    #[expect(
        clippy::too_many_arguments,
        reason = "every one is a distinct part of the clause, and a struct here                   would be the clause spelled twice"
    )]
    fn join(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        left: &bgv_db_ql::TableRef,
        right: &bgv_db_ql::TableRef,
        left_key: &bgv_db_ql::FieldPath,
        right_key: &bgv_db_ql::FieldPath,
        condition: Option<&Expr>,
    ) -> Result<Joined> {
        let (left_context, left_id) = self.resolve_table(transaction, left)?;
        let (right_context, right_id) = self.resolve_table(transaction, right)?;
        let left_name = left.name.text.clone();
        let right_name = right.name.text.clone();

        // Each side is redacted by its own table's grant: a join is two reads
        // and neither borrows the other's permission.
        let left_visible = self.visible_in(transaction, left_id)?;
        let right_visible = self.visible_in(transaction, right_id)?;

        let served = ordered_index_on(transaction, right_id, right_key)?;
        let mut built = BTreeMap::new();
        if served.is_none() {
            let found = transaction.scan_table(
                right_context.namespace,
                right_context.database,
                right_id,
            )?;
            for (id, record) in self.records_of(found, &right_visible)? {
                let Some(key) = right_key.path.resolve(&record).cloned() else {
                    continue;
                };
                built.entry(key).or_insert_with(Vec::new).push((id, record));
            }
        }

        let searched = self.searched_for(transaction, left_id, &shown(select))?;
        let mut rows = Vec::new();
        let driving =
            transaction.scan_table(left_context.namespace, left_context.database, left_id)?;
        for (id, record) in self.records_of(driving, &left_visible)? {
            // A left record with nothing at the key matches nothing: `NONE` is a
            // value and the right side would have to carry it to match, which is
            // what an inner join means.
            let Some(key) = left_key.path.resolve(&record).cloned() else {
                continue;
            };
            let matches = match &served {
                Some(index) => {
                    let offered =
                        transaction.records_by_index(index, core::slice::from_ref(&key))?;
                    self.records_of(offered, &right_visible)?
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
                        Scope::searching(&row, &searched),
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
        let path = if served.is_some() {
            AccessPath::Index
        } else {
            AccessPath::Scan
        };
        Ok((rows, path, searched))
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
    fn candidates(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        context: crate::context::Context,
        condition: &Expr,
        searched: &Searched,
    ) -> Result<(Vec<(RecordId, Value)>, AccessPath)> {
        // Once for the statement rather than once per conjunct: which indexes a
        // table carries is one question, and it used to be asked as many times
        // as the condition had clauses.
        let declared = Catalog::new(transaction).indexes_on(table)?;
        let offered = self.enumerate(transaction, condition, &declared, searched)?;

        // Resolved before either path, so an index-served read and a scan see
        // the same record — which is what makes candidates re-tested against the
        // whole condition unable to answer what a scan refuses.
        let visible = self.visible_in(transaction, table)?;
        if let Some(chosen) = plan::choose(offered) {
            let found = self.serve(transaction, context, table, &chosen)?;
            return Ok((self.records_of(found, &visible)?, AccessPath::Index));
        }
        let scanned = transaction.scan_table(context.namespace, context.database, table)?;
        Ok((self.records_of(scanned, &visible)?, AccessPath::Scan))
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
    ) -> Result<Option<Vec<(RecordId, Value)>>> {
        let Some(index) = self.index_on_path(transaction, table, wanted.path)? else {
            return Ok(None);
        };
        let Some(declared) = index.vector else {
            return Ok(None);
        };
        if !plan::answers(declared, wanted.distance) {
            return Ok(None);
        }
        let Some(query) = bgv_db_storage::vector_of(&self.evaluate(transaction, wanted.query)?)
        else {
            return Ok(None);
        };
        let visible = self.visible_in(transaction, table)?;
        let mut rows = Vec::new();
        for id in transaction.records_by_vector(&index, &query, wanted.wanted)? {
            // Resolved at this reader's own snapshot, like every index read, so
            // a node left behind by a deleted record produces nothing.
            let at = RecordAddress::new(context.namespace, context.database, table, id);
            if let Some(payload) = transaction.get(&at)? {
                rows.push((at.id, self.record_of(&payload, &visible)?));
            }
        }
        Ok(Some(rows))
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
    ///   [`bgv_db_encoding::IndexValues`] cannot be reversed, but two entries
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
    ) -> Result<Option<Vec<(RecordId, Value)>>> {
        let Some((index, visible)) =
            self.index_serving_order(transaction, context, table, wanted.path, wanted.descending)?
        else {
            return Ok(None);
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
                None => return Ok(None),
            }
        } else {
            transaction.records_in_ascending_order(&index, ORDERED_LEADING_FIELDS, wanted.wanted)?
        };
        self.records_of(found, &visible).map(Some)
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
        searched: &Searched,
    ) -> Result<Option<Vec<(RecordId, Value)>>> {
        // Descending, stated rather than taken from the bound: this walk's whole
        // argument rests on absences sorting *last*, and passing the caller's
        // direction through would make that argument depend on a value from
        // somewhere else. The caller refuses an ascending bound before it gets
        // here; this is the second lock on the same door.
        let Some((index, visible)) =
            self.index_serving_order(transaction, context, table, wanted.path, true)?
        else {
            return Ok(None);
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
                return Ok(None);
            };
            let mut matched = Vec::new();
            for (id, record) in self.records_of(found, &visible)? {
                let held =
                    self.evaluate_in(transaction, condition, Scope::searching(&record, searched))?;
                if boolean(&held, condition.span)? {
                    matched.push((id, record));
                }
            }
            if matched.len() >= wanted.wanted {
                return Ok(Some(matched));
            }
            if asking >= ceiling {
                return Ok(None);
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
        path: &bgv_db_types::Path,
        descending: bool,
    ) -> Result<Option<(bgv_db_storage::IndexDefinition, crate::redact::Visible)>> {
        let Some(index) = self.index_ordering_on_path(transaction, table, path)? else {
            return Ok(None);
        };
        if index.search || index.vector.is_some() {
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
        if transaction.snapshot() != self.store.committed_tail()? {
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
        path: &bgv_db_types::Path,
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
    fn serve(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        chosen: &plan::Candidate,
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
            plan::Served::Terms(terms) => {
                let mut rows = Vec::new();
                for id in transaction.records_by_terms(&chosen.index, terms)? {
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
    /// cost and `docs/bgvql.md` §4a states it rather than leaving it to be found.
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
    ) -> Result<(Vec<(RecordId, Value)>, AccessPath)> {
        let (_, start) = self.address(transaction, from)?;
        let mut anchors = vec![RecordRef::new(start.table, start.id.clone())];
        let mut answer = Vec::new();

        for hop in hops {
            let (_, edge_table) = self.resolve_table(transaction, &hop.edges)?;
            if !Catalog::new(transaction)
                .table(edge_table)?
                .is_some_and(|found| found.edge)
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
            for anchor in &anchors {
                let value = Value::Record(anchor.clone());
                let offered = transaction.records_by_index(&index, &[value])?;
                found.extend(self.records_of(offered, &edge_visible)?);
            }

            let Some(target) = hop.target.as_ref() else {
                // The last step named no node, so the edges themselves are the
                // answer. The grammar allows this only at the end, so there is
                // no case here where the walk would have had to continue.
                answer = found;
                break;
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
            anchors = reached
                .keys()
                .map(|id| RecordRef::new(target_table, id.clone()))
                .collect();
            answer = reached.into_iter().collect();
        }
        Ok((answer, AccessPath::Index))
    }

    /// A read standing where a value stands.
    ///
    /// One record answers with its own value; a read of several answers with an
    /// array, so that the shape of the answer follows the shape of the question
    /// rather than the number of rows that happened to match.
    fn read_as_value(&self, transaction: &mut Transaction<'_>, select: &Select) -> Result<Value> {
        let (records, _) = self.read(transaction, select)?;
        // `$node` alongside `Source::Record` because it is one record too: a
        // read of one answers with its own value, and wrapping it in an array of
        // one would make the shape of the answer follow the source rather than
        // the question.
        if matches!(select.from, Source::Record(_) | Source::Node) {
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
fn ordered_index_on(
    transaction: &mut Transaction<'_>,
    table: TableId,
    key: &bgv_db_ql::FieldPath,
) -> Result<Option<bgv_db_storage::IndexDefinition>> {
    Ok(Catalog::new(transaction)
        .indexes_on(table)?
        .into_iter()
        .find(|held| {
            !held.search
                && held.vector.is_none()
                && held.fields.len() == 1
                && held.fields.first() == Some(&key.path)
        }))
}

/// What a join produces, which is still a collection.
///
/// A join builds a map of one side and probes it with the other, so its work is
/// not per-record and streaming it would move the materialisation rather than
/// remove it. Named separately so the difference is visible in the signature
/// rather than resting on a comment.
type Joined = (Vec<(RecordId, Value)>, AccessPath, Searched);

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
    Held(Vec<(RecordId, Value)>, AccessPath),
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
    select.fetch.is_empty() && !select.order.is_empty() && !groups(select)
}

/// Whether the read folds many records into one.
///
/// Named once and asked twice — by the test above and by the projection stage —
/// because the two must not drift apart. A grouping routed to the streaming path
/// would be a fold evaluated against one record at a time, which is the one
/// thing a fold is not.
fn groups(select: &Select) -> bool {
    match &select.projection {
        Projection::All => false,
        Projection::Values(wanted) => folds(wanted) || !select.group.is_empty(),
    }
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

/// What the evaluator can see besides the expression itself.
///
/// The record a condition is being tested against, and what its searched fields
/// need. Both are absent in a value position, where there is no record and
/// nothing to search.
#[derive(Clone, Copy, Default)]
pub(crate) struct Scope<'a> {
    /// The record being tested, when there is one.
    pub(crate) record: Option<&'a Value>,
    /// The analyzers and collection statistics the searched paths need.
    searched: Option<&'a Searched>,
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
            searched: None,
        }
    }

    /// A record, with nothing searched.
    pub(crate) const fn of(record: &'a Value) -> Self {
        Self {
            record: Some(record),
            searched: None,
        }
    }

    /// A record, and what its searched fields need.
    pub(crate) const fn searching(record: &'a Value, searched: &'a Searched) -> Self {
        Self {
            record: Some(record),
            searched: Some(searched),
        }
    }

    /// The analyzer this path's field declares, if it declares one.
    fn analyzer(self, path: &Path) -> Option<&'a Analyzer> {
        self.searched.and_then(|held| held.analyzer(path))
    }

    /// What this path's collection looks like, if it was ranked against.
    fn corpus(self, path: &Path) -> Option<&'a Corpus> {
        self.searched.and_then(|held| held.corpus(path))
    }
}

/// The expressions a read evaluates besides its condition: what it projects,
/// and what it orders by.
///
/// A fold is left out. `search::score` inside one would be scoring the group
/// rather than the record, which is a different question and is not this one.
fn shown(select: &Select) -> Vec<&Expr> {
    let mut found = Vec::new();
    if let Projection::Values(wanted) = &select.projection {
        for projected in wanted {
            found.push(&projected.value);
        }
    }
    for ordering in &select.order {
        found.push(&ordering.key);
    }
    found
}
