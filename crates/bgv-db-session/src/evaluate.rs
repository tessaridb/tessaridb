//! Turning an expression into a value.
//!
//! Two of the forms are reads, and that is the whole of what makes the models
//! compose. A `GET` inside a record statement runs **in the same transaction**,
//! so it sees the same snapshot as the statement around it — two models that
//! cannot share a snapshot are two databases sharing a process.

use core::ops::Bound;
use std::collections::BTreeMap;

use bgv_db_encoding::decode_payload;
use bgv_db_ql::{
    BinaryOp, Direction, Expr, ExprKind, Projectable, Projected, Projection, RecordTarget, Select,
    Source, Span, TableRef,
};
use bgv_db_storage::{Catalog, RecordAddress, Transaction};
use bgv_db_types::{Analyzer, Number, Path, RecordId, RecordRef, TableId, Value, ValueRange};

use crate::aggregate::folds;
use crate::arithmetic::{arithmetic, negate};
use crate::call::call;
use crate::condition::{apply, boolean, literal_prefix};
use crate::error::{Error, Result};
use crate::outcome::AccessPath;
use crate::search::matches_terms;
use crate::session::Session;

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
                let arguments = self.values(transaction, arguments, scope)?;
                call(*function, &arguments, *span)
            }
            ExprKind::Arithmetic { op, left, right } => {
                let held = self.evaluate_in(transaction, left, scope)?;
                let other = self.evaluate_in(transaction, right, scope)?;
                arithmetic(*op, &held, &other, expr.span)
            }
            ExprKind::Binary { op, left, right } => {
                let held = self.evaluate_in(transaction, left, scope)?;
                let other = self.evaluate_in(transaction, right, scope)?;
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
                target.id.clone(),
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
        match transaction.get(&address)? {
            Some(payload) => Ok(decode_payload(&payload)?),
            None => Ok(Value::None),
        }
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
        let (records, path) = self.read_source(transaction, &select.from)?;
        let records = match &select.projection {
            Projection::All => records,
            Projection::Values(wanted) if folds(wanted) || !select.group.is_empty() => {
                self.grouped(transaction, records, wanted, &select.group)?
            }
            Projection::Values(wanted) => {
                let mut projected = Vec::with_capacity(records.len());
                for (id, record) in records {
                    projected.push((id, self.project(transaction, &record, wanted)?));
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
        let records = crate::shape::sorted(records, &select.order);
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
    fn project(
        &self,
        transaction: &mut Transaction<'_>,
        record: &Value,
        wanted: &[Projected],
    ) -> Result<Value> {
        let mut projected = BTreeMap::new();
        for value in wanted {
            let Projectable::Value(expr) = &value.value else {
                // A fold never reaches here: a projection carrying one goes
                // through `grouped`, which is the only place many records
                // become one.
                continue;
            };
            let held = self.evaluate_in(transaction, expr, Scope::of(record))?;
            if held.is_present() {
                projected.insert(value.name.text.clone(), held);
            }
        }
        Ok(Value::Object(projected))
    }

    /// The records a source produces, as they are stored.
    fn read_source(
        &self,
        transaction: &mut Transaction<'_>,
        from: &Source,
    ) -> Result<(Vec<(RecordId, Value)>, AccessPath)> {
        match from {
            Source::Record(target) => {
                let (_, address) = self.address(transaction, target)?;
                let found = match transaction.get(&address)? {
                    Some(payload) => vec![(address.id, decode_payload(&payload)?)],
                    None => Vec::new(),
                };
                Ok((found, AccessPath::Record))
            }
            Source::Table(table) => {
                let (context, id) = self.resolve_table(transaction, table)?;
                let found = transaction.scan_table(context.namespace, context.database, id)?;
                Ok((decode_all(found)?, AccessPath::Scan))
            }
            Source::Traverse {
                from,
                direction,
                edges,
                target,
            } => self.traverse(transaction, from, *direction, edges, target.as_ref()),
            Source::Where { table, condition } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                let (candidates, path) = self.candidates(transaction, id, context, condition)?;
                // Resolved once for the query rather than once per record: which
                // analyzer a field carries is a property of the schema, and the
                // schema does not change under a read.
                let analyzers = self.analyzers_for(transaction, id, condition)?;

                // The candidates are tested against the **whole** condition, not
                // only the conjunct the index answered. That is what makes an
                // index a narrowing device rather than an answer, and it is why
                // adding one still cannot change what a query returns.
                let mut matched = Vec::new();
                for (id, record) in candidates {
                    let held = self.evaluate_in(
                        transaction,
                        condition,
                        Scope::searching(&record, &analyzers),
                    )?;
                    if boolean(&held, condition.span)? {
                        matched.push((id, record));
                    }
                }
                Ok((matched, path))
            }
        }
    }

    /// The records worth testing, and how they were reached.
    ///
    /// An index narrows when the condition contains an equality or a prefix
    /// pattern on an indexed path; otherwise the table is read. Which one
    /// happens is decided by what exists, never by how the query was written.
    fn candidates(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        context: crate::context::Context,
        condition: &Expr,
    ) -> Result<(Vec<(RecordId, Value)>, AccessPath)> {
        for seek in seekable(condition) {
            // A right-hand side that reads the record is not a constant, so it
            // cannot be a bound; `seekable` has already excluded those.
            let wanted = self.evaluate(transaction, seek.value)?;
            let Some(index) = self.index_on_path(transaction, table, seek.path)? else {
                continue;
            };
            let found = match seek.shape {
                Shape::Equality => transaction.records_by_index(&index, &[wanted])?,
                Shape::Prefix => {
                    let Value::String(pattern) = &wanted else {
                        continue;
                    };
                    let Some(prefix) = literal_prefix(pattern) else {
                        continue;
                    };
                    transaction.records_with_string_prefix(&index, &prefix)?
                }
            };
            return Ok((decode_all(found)?, AccessPath::Index));
        }
        let scanned = transaction.scan_table(context.namespace, context.database, table)?;
        Ok((decode_all(scanned)?, AccessPath::Scan))
    }

    /// One hop along an edge table, and optionally one more into its far side.
    ///
    /// Both halves are index reads. The edge table was given an index on each
    /// endpoint when it was declared, so finding the edges out of a record is
    /// `records_by_index` on `out` — the same call an equality filter makes, with
    /// a record reference standing where any other value would.
    ///
    /// A dangling far endpoint yields nothing for that hop rather than an error.
    /// A record can be deleted while an edge still names it, and that is a state
    /// of the graph, not a failure of the query — the alternative is a read that
    /// breaks because of a write it has nothing to do with.
    fn traverse(
        &self,
        transaction: &mut Transaction<'_>,
        from: &RecordTarget,
        direction: Direction,
        edges: &TableRef,
        target: Option<&TableRef>,
    ) -> Result<(Vec<(RecordId, Value)>, AccessPath)> {
        let (_, edge_table) = self.resolve_table(transaction, edges)?;
        if !Catalog::new(transaction)
            .table(edge_table)?
            .is_some_and(|found| found.edge)
        {
            return Err(Error::NotAnEdgeTable {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        }
        let Some(index) = self.index_on_path(
            transaction,
            edge_table,
            &Path::field(direction.from_field()),
        )?
        else {
            // An edge table always has both, so reaching here means the catalog
            // and the flag disagree — which is corruption, not a slow path.
            return Err(Error::NotAnEdgeTable {
                table: edges.name.text.clone(),
                span: edges.span,
            });
        };

        let (_, start) = self.address(transaction, from)?;
        let anchor = Value::Record(RecordRef::new(start.table, start.id.clone()));
        let found = decode_all(transaction.records_by_index(&index, &[anchor])?)?;

        let Some(target) = target else {
            return Ok((found, AccessPath::Index));
        };

        let (context, target_table) = self.resolve_table(transaction, target)?;
        let mut reached = Vec::new();
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
                reached.push((far.id.clone(), decode_payload(&payload)?));
            }
        }
        Ok((reached, AccessPath::Index))
    }

    /// A read standing where a value stands.
    ///
    /// One record answers with its own value; a read of several answers with an
    /// array, so that the shape of the answer follows the shape of the question
    /// rather than the number of rows that happened to match.
    fn read_as_value(&self, transaction: &mut Transaction<'_>, select: &Select) -> Result<Value> {
        let (records, _) = self.read(transaction, select)?;
        if matches!(select.from, Source::Record(_)) {
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

fn decode_all(found: Vec<(RecordId, Vec<u8>)>) -> Result<Vec<(RecordId, Value)>> {
    let mut records = Vec::with_capacity(found.len());
    for (id, payload) in found {
        records.push((id, decode_payload(&payload)?));
    }
    Ok(records)
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

/// The shape of a test an index can answer.
enum Shape {
    /// `<path> = <constant>` — one entry.
    Equality,
    /// `<path> LIKE '<literal>%'` — a range over the values beginning with it.
    Prefix,
}

/// One conjunct an index could narrow with.
struct Seek<'a> {
    path: &'a Path,
    value: &'a Expr,
    shape: Shape,
}

/// The conjuncts of a condition an index could serve, outermost first.
///
/// Only `AND` is walked into. Under `OR` neither side alone narrows the
/// answer — a record satisfying the other half would be missed — and under `NOT`
/// an index that finds the matching records is exactly the wrong set. Both are
/// left to the scan rather than served with a bound that would be a guess.
///
/// A right-hand side that reads the record is not a constant and cannot be a
/// bound, so it is excluded here rather than discovered when it is evaluated
/// without a record in scope.
fn seekable(condition: &Expr) -> Vec<Seek<'_>> {
    match &condition.kind {
        ExprKind::And(left, right) => {
            let mut found = seekable(left);
            found.extend(seekable(right));
            found
        }
        ExprKind::Binary { op, left, right } => {
            let ExprKind::Path(field) = &left.kind else {
                return Vec::new();
            };
            if reads_a_record(right) {
                return Vec::new();
            }
            let shape = match op {
                BinaryOp::Equal => Shape::Equality,
                BinaryOp::Like => Shape::Prefix,
                // An ordered index can serve `<` and `>` as a bounded range, and
                // this does not build it: that needs a bounded scan on
                // `Transaction` and an equivalence test of its own. Reported as
                // a scan until it does, rather than served as a guess.
                _ => return Vec::new(),
            };
            vec![Seek {
                path: &field.path,
                value: right,
                shape,
            }]
        }
        _ => Vec::new(),
    }
}

/// What the evaluator can see besides the expression itself.
///
/// The record a condition is being tested against, and the analyzers the
/// searched fields declare. Both are absent in a value position, where there is
/// no record and nothing to search.
#[derive(Clone, Copy, Default)]
pub(crate) struct Scope<'a> {
    /// The record being tested, when there is one.
    pub(crate) record: Option<&'a Value>,
    /// The analyzer each searched path carries.
    analyzers: Option<&'a BTreeMap<Path, Analyzer>>,
}

impl<'a> Scope<'a> {
    /// A record, with nothing searched.
    pub(crate) const fn of(record: &'a Value) -> Self {
        Self {
            record: Some(record),
            analyzers: None,
        }
    }

    /// A record, and the analyzers its searched fields declare.
    const fn searching(record: &'a Value, analyzers: &'a BTreeMap<Path, Analyzer>) -> Self {
        Self {
            record: Some(record),
            analyzers: Some(analyzers),
        }
    }

    /// The analyzer this path's field declares, if it declares one.
    fn analyzer(self, path: &Path) -> Option<&'a Analyzer> {
        self.analyzers.and_then(|named| named.get(path))
    }
}

/// Whether an expression reads the record being tested.
fn reads_a_record(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Path(_) => true,
        ExprKind::Not(inner) | ExprKind::Negate(inner) => reads_a_record(inner),
        ExprKind::And(left, right) | ExprKind::Or(left, right) => {
            reads_a_record(left) || reads_a_record(right)
        }
        ExprKind::Arithmetic { left, right, .. } | ExprKind::Binary { left, right, .. } => {
            reads_a_record(left) || reads_a_record(right)
        }
        ExprKind::Call { arguments, .. } => arguments.iter().any(reads_a_record),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().any(reads_a_record),
        ExprKind::Object(fields) => fields.iter().any(|field| reads_a_record(&field.value)),
        ExprKind::Range(range) => reads_a_record(&range.start) || reads_a_record(&range.end),
        ExprKind::Literal(_)
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Select(_) => false,
    }
}
