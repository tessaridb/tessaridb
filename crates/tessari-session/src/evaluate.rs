//! Turning an expression into a value.
//!
//! Two of the forms are reads, and that is the whole of what makes the models
//! compose. A `GET` inside a record statement runs **in the same transaction**,
//! so it sees the same snapshot as the statement around it — two models that
//! cannot share a snapshot are two databases sharing a process.

use core::ops::Bound;
use std::collections::BTreeMap;

use tessari_ql::{BinaryOp, Expr, ExprKind, FieldPath, Function, Identity, Projected, Span};
use tessari_storage::Transaction;
use tessari_types::{Number, RecordId, RecordRef, Value, ValueRange, apply};

use crate::arithmetic::{arithmetic, negate};
use crate::call::call;
use crate::condition::boolean;
use crate::error::{Error, Result};
use crate::search::{
    matches_fuzzy_terms, matches_infix_terms, matches_prefix_terms, matches_terms,
};
use crate::session::Session;

pub(crate) use keys::{collect_by_key, key_bound, listed, ordered_index_on};
pub(crate) use scope::{Reporting, Scope, Testing};
pub(crate) use shape_rules::{
    alone, asserted, ceiling_reached, groups, held_bound, node_row, omit_within, omits, opened,
    order_bound, shown, sought, streams, table_named,
};
pub(crate) use stages::{
    Answered, Approximated, Asked, Candidates, Gated, Hopped, Joined, Prepared, Reached, Walked,
};

mod asof;
mod candidates;
mod delete;
mod folded;
mod fused;
mod graph;
mod join;
mod keys;
mod latest;
mod lent;
mod nearest;
mod nearest_place;
mod ordered;
mod partition;
mod paths;
mod phrase;
mod produce;
mod projection;
mod read;
mod scope;
mod scored;
mod shape_rules;
mod source;
mod stages;

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
            // A step that reaches nothing is `none`, for the reason a path's is
            // above (ADR-0110).
            ExprKind::Route { value, steps } => {
                let held = self.evaluate_in(transaction, value, scope)?;
                Ok(walk_route(held, steps))
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
                // A `FROM SEARCH` record answers its own score, table, snippet
                // and marks, from what the search ranked it with (ADR-0105).
                if let Some(answer) =
                    self.answered_by_hit(transaction, *function, arguments, scope, *span)?
                {
                    return Ok(answer);
                }
                // A score is the second thing in this language that needs more
                // than its arguments — the field's analyzer, and what the
                // collection looks like. `call` takes values, and neither of
                // those is one, so it is answered here where the scope is.
                // An explanation is the same score, answered with its parts.
                if matches!(function, Function::SearchScore | Function::SearchExplain) {
                    let explaining = *function == Function::SearchExplain;
                    return self.rank(transaction, arguments, scope, *span, explaining);
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
                if *function == Function::SessionContext {
                    return self.session_context();
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
                        BinaryOp::MatchesInfix => matches_infix_terms(analyzer, value, &other),
                        held_op => {
                            scope.compared_by(held_op, value, &other);
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
                    BinaryOp::Matches
                        | BinaryOp::MatchesPrefix
                        | BinaryOp::MatchesFuzzy
                        | BinaryOp::MatchesInfix
                ) {
                    let analyzer = match &left.kind {
                        ExprKind::Path(field) => scope.analyzer(&field.path),
                        _ => None,
                    };
                    return Ok(Value::Bool(match *op {
                        BinaryOp::MatchesPrefix => matches_prefix_terms(analyzer, &held, &other),
                        BinaryOp::MatchesFuzzy => matches_fuzzy_terms(analyzer, &held, &other),
                        BinaryOp::MatchesInfix => matches_infix_terms(analyzer, &held, &other),
                        _ => matches_terms(analyzer, &held, &other),
                    }));
                }
                // Beside the comparison rather than inside it: what an operator
                // means once both sides are values belongs to `tessari_types`,
                // which the store's `ASSERT` path shares and which has no notes
                // and should not grow any.
                scope.compared_by(*op, &held, &other);
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

/// The value `steps` reach inside `value`, or `none` where a step reaches
/// nothing — an object without the field, an array without the position, or a
/// value that is neither.
fn walk_route(value: Value, steps: &[tessari_types::Step]) -> Value {
    let mut current = value;
    for step in steps {
        current = match (step, current) {
            (tessari_types::Step::Field(name), Value::Object(mut fields)) => {
                fields.remove(name).unwrap_or(Value::None)
            }
            (tessari_types::Step::Index(at), Value::Array(mut items)) => {
                match usize::try_from(*at) {
                    Ok(at) if at < items.len() => items.swap_remove(at),
                    _ => Value::None,
                }
            }
            _ => Value::None,
        };
    }
    current
}
