//! Turning an expression into a value.
//!
//! Two of the forms are reads, and that is the whole of what makes the models
//! compose. A `GET` inside a record statement runs **in the same transaction**,
//! so it sees the same snapshot as the statement around it — two models that
//! cannot share a snapshot are two databases sharing a process.

use core::ops::Bound;
use std::collections::{BTreeMap, BTreeSet};

use tessari_ql::{
    BinaryOp, Expr, ExprKind, FieldPath, Function, Identity, Projected, Projection, RecordTarget,
    Select, Source, Span, Using,
};
use tessari_storage::{BUILD_VERSION, Catalog, IndexDefinition, RecordAddress, Store, Transaction};
use tessari_types::{
    Analyzer, Number, Path, RecordId, RecordRef, Step, TableId, Value, ValueRange, apply,
};

use crate::aggregate::folds;
use crate::arithmetic::{arithmetic, negate};
use crate::budget::{Budget, Ceiling};
use crate::call::call;
use crate::condition::boolean;
use crate::consume::Consumer;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::noticed::Noticed;
use crate::outcome::{AccessPath, Note, Suggestion};
use crate::plan::Plan;
use crate::search::{Ranked, Searched, matches_fuzzy_terms, matches_prefix_terms, matches_terms};
use crate::session::Session;

mod candidates;
mod delete;
mod fused;
mod graph;
mod join;
mod lent;
mod ordered;
mod produce;
mod projection;
mod read;
mod scored;
mod source;

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

/// How much of a table a read needs, for [`Session::refuse_reading_a_part`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Part<'a> {
    /// Every record — a scan, a filter, an index read, a join side.
    Whole,
    /// One record, by identity.
    Record(&'a RecordId),
    /// The identities between two positions.
    Span {
        /// Where the span begins, always included.
        lower: &'a RecordId,
        /// Where it ends.
        upper: &'a RecordId,
        /// Whether `upper` is included.
        inclusive: bool,
    },
}

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
                // And a rank, which is the fusion's and not the record's: it
                // exists only while a fused read projects what it ordered.
                if *function == Function::SearchRanks {
                    let Some(ranks) = scope.ranks else {
                        return Err(Error::NotFused { span: *span });
                    };
                    return Ok(Value::Array(
                        ranks
                            .iter()
                            .map(|rank| {
                                rank.and_then(|rank| i64::try_from(rank).ok())
                                    .map_or(Value::None, |rank| {
                                        Value::Number(Number::Integer(rank))
                                    })
                            })
                            .collect(),
                    ));
                }
                if let Some(distance) =
                    self.distance_between(transaction, *function, arguments, scope)?
                {
                    return Ok(distance);
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
            ExprKind::Ttl(target) => self.ttl_of(transaction, target),
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
/// What the statement itself said about the plan, as opposed to what the store
/// worked out.
///
/// The two travel together because they come from one place — the read's tail —
/// and are read by one function. A pair rather than two arguments because a
/// planner call taking eight things has stopped being readable, and grouping
/// them by where they came from is the division that survives the next one being
/// added.
#[derive(Clone, Copy)]
struct Asked<'a> {
    /// The table the plan reports, when the source names one.
    named: Option<&'a str>,
    /// `WITHOUT SCAN GUARD` — the planner's size veto is lifted for this read.
    lift_scan_guard: bool,
}

impl Asked<'_> {
    /// A read whose statement said nothing about its plan.
    ///
    /// A `DELETE` carries no read tail to say anything in, so it asks for the
    /// defaults rather than for a privilege no caller could have written down.
    const fn nothing() -> Self {
        Self {
            named: None,
            lift_scan_guard: false,
        }
    }
}

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
    // The effective role, matching `INFO FOR NODE` — both are reports of the
    // same fact, and a reader comparing them is entitled to one answer.
    fields.insert(
        "roles".to_owned(),
        Value::Array(
            store
                .effective_roles()?
                .names()
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
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
    // A fused order projects after it orders — its ranks are what
    // `search::ranks()` answers — so it takes the collecting path, where the two
    // stages can be put in that order.
    select.fetch.is_empty()
        && select.split.is_none()
        && !select.order.is_empty()
        && select.fusion.is_none()
        && !groups(select)
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
    /// Where this record came in each branch of the fused order that answered
    /// it, when it is being projected by a fused read — what `search::ranks()`
    /// answers, and the reason it answers nowhere else.
    ranks: Option<&'a [Option<u64>]>,
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
            ranks: None,
        }
    }

    /// A record, with nothing searched.
    pub(crate) const fn of(record: &'a Value) -> Self {
        Self {
            record: Some(record),
            id: None,
            searched: None,
            noticed: None,
            ranks: None,
        }
    }

    /// A record, and what its searched fields need.
    pub(crate) const fn searching(record: &'a Value, searched: &'a Searched) -> Self {
        Self {
            record: Some(record),
            id: None,
            searched: Some(searched),
            noticed: None,
            ranks: None,
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
            ranks: None,
        }
    }

    /// The same scope, projecting a record of a fused read with its ranks.
    pub(crate) const fn with_ranks(self, ranks: &'a [Option<u64>]) -> Self {
        Self {
            ranks: Some(ranks),
            ..self
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
