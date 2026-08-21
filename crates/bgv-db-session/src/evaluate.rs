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
    Direction, Expr, ExprKind, FieldPath, Projected, Projection, RecordTarget, Select, Source,
    Span, TableRef, Test,
};
use bgv_db_storage::{Catalog, RecordAddress, Transaction};
use bgv_db_types::{Number, Path, RecordId, RecordRef, Value, ValueRange};

use crate::error::{Error, Result};
use crate::outcome::AccessPath;
use crate::session::Session;

impl Session<'_> {
    /// The value an expression denotes.
    pub(crate) fn evaluate(&self, transaction: &mut Transaction<'_>, expr: &Expr) -> Result<Value> {
        match &expr.kind {
            ExprKind::Literal(value) => Ok(value.clone()),
            ExprKind::Table(table) => {
                let (_, id) = self.resolve_table(transaction, table)?;
                Ok(Value::Table(id))
            }
            ExprKind::Record(target) => {
                let (_, address) = self.address(transaction, target)?;
                Ok(Value::Record(RecordRef::new(address.table, address.id)))
            }
            ExprKind::Array(items) => Ok(Value::Array(self.values(transaction, items)?)),
            ExprKind::Set(items) => Ok(Value::Set(
                self.values(transaction, items)?.into_iter().collect(),
            )),
            ExprKind::Object(fields) => {
                let mut object = BTreeMap::new();
                for field in fields {
                    let value = self.evaluate(transaction, &field.value)?;
                    object.insert(field.name.text.clone(), value);
                }
                Ok(Value::Object(object))
            }
            ExprKind::Range(range) => {
                let start = self.evaluate(transaction, &range.start)?;
                let end = self.evaluate(transaction, &range.end)?;
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

    fn values(&self, transaction: &mut Transaction<'_>, items: &[Expr]) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            values.push(self.evaluate(transaction, item)?);
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
        let Projection::Values(wanted) = &select.projection else {
            return Ok((records, path));
        };
        let projected = records
            .into_iter()
            .map(|(id, record)| (id, project(&record, wanted)))
            .collect();
        Ok((projected, path))
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
            Source::Filter {
                table,
                field,
                test,
                value,
            } => {
                let (context, id) = self.resolve_table(transaction, table)?;
                let wanted = self.evaluate(transaction, value)?;

                // An index serves an equality on a named field.
                if *test == Test::Equals {
                    if let Some(index) = self.index_on_path(transaction, id, &field.path)? {
                        let found = transaction.records_by_index(&index, &[wanted])?;
                        return Ok((decode_all(found)?, AccessPath::Index));
                    }
                }

                // And it serves a pattern that is a literal followed by a
                // trailing `%`, because that asks for the values beginning with
                // the literal — a range over the same ordered index. No new
                // index kind, no new statement, and the same records the scan
                // would have found. Every other shape of pattern, and every
                // `ILIKE`, keeps the scan: a case-folded or infix match needs a
                // second stored form, which is an analyzer decision, and serving
                // a narrower answer quickly would be worse than serving the
                // right one slowly.
                if *test == Test::Like {
                    if let Value::String(pattern) = &wanted {
                        if let Some(prefix) = literal_prefix(pattern) {
                            if let Some(index) = self.index_on_path(transaction, id, &field.path)? {
                                let found =
                                    transaction.records_with_string_prefix(&index, &prefix)?;
                                return Ok((decode_all(found)?, AccessPath::Index));
                            }
                        }
                    }
                }

                let scanned =
                    decode_all(transaction.scan_table(context.namespace, context.database, id)?)?;
                let matched = scanned
                    .into_iter()
                    .filter(|(_, record)| matches_filter(record, field, *test, &wanted))
                    .collect();
                Ok((matched, AccessPath::Scan))
            }
        }
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

/// One record, reduced to the values a read asked for.
///
/// **A route that reaches nothing omits its field** rather than answering
/// `none`. `Value::None` means the field is not there, so writing it into an
/// object would say the field is there and holds not-being-there — the
/// contradiction the value system spends its own rules avoiding. The consequence
/// is that projected records keep differing shapes, which is the same property
/// that makes a table able to hold documents at all.
fn project(record: &Value, wanted: &[Projected]) -> Value {
    let mut projected = BTreeMap::new();
    for value in wanted {
        if let Some(found) = value.path.path.resolve(record) {
            projected.insert(value.name.text.clone(), found.clone());
        }
    }
    Value::Object(projected)
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

/// Whether one record satisfies a filter.
///
/// A route that reaches nothing matches nothing. That covers a record which is
/// not an object at all — a space holds single values, and searching one by
/// field is a question with no answer rather than an error — and it covers every
/// way a nested route can end early, which is the same answer the index gives
/// for the same record. The two agreeing is not a coincidence: both ask
/// [`Path::resolve`].
fn matches_filter(record: &Value, field: &FieldPath, test: Test, wanted: &Value) -> bool {
    field
        .path
        .resolve(record)
        .is_some_and(|held| satisfies(held, test, wanted))
}

/// Whether one held value satisfies the test.
fn satisfies(held: &Value, test: Test, wanted: &Value) -> bool {
    match test {
        Test::Equals => held == wanted,
        Test::Like => like(held, wanted, false),
        Test::Ilike => like(held, wanted, true),
        Test::Contains => holds(held, wanted),
    }
}

/// Membership: does this collection hold that value.
///
/// A different question from [`Test::Like`], which is why the language has both.
/// Only a collection answers it — a field holding a single value is not a
/// one-element collection, because treating it as one would make
/// `name CONTAINS 'ada'` quietly mean `name = 'ada'` and hide a mistake in the
/// query rather than showing it as no match.
fn holds(held: &Value, wanted: &Value) -> bool {
    match held {
        Value::Array(items) => items.contains(wanted),
        Value::Set(items) => items.contains(wanted),
        _ => false,
    }
}

/// The literal a pattern begins with, when the pattern is exactly that literal
/// followed by a trailing `%`.
///
/// Only that shape. For it, "begins with the literal" and "matches the pattern"
/// are the same statement, so a range read over the index needs no second test
/// and cannot answer differently from a scan. `'%a%'`, `'a_b%'` and `'a%b'` are
/// all left to the scan rather than served from a bound that would be a guess.
///
/// The escape is honoured, so `'50\%%'` asks for values beginning with `50%`.
fn literal_prefix(pattern: &str) -> Option<String> {
    let mut literal = String::new();
    let mut characters = pattern.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => literal.push(characters.next()?),
            '_' => return None,
            '%' => {
                return match (characters.next(), literal.is_empty()) {
                    // A trailing `%` and something before it.
                    (None, false) => Some(literal),
                    _ => None,
                };
            }
            other => literal.push(other),
        }
    }
    // No wildcard at all. That is an equality written the long way, and it is
    // left alone rather than quietly rewritten into one.
    None
}

/// SQL's `LIKE`, over the whole value.
///
/// Only text matches a text pattern: a number in that field is not an error, it
/// simply does not satisfy the filter. Deliberately no tokenising, stemming or
/// ranking — those belong to an analyzer, and a scan-shaped approximation of one
/// would give answers a real text index later disagrees with.
fn like(held: &Value, wanted: &Value, fold_case: bool) -> bool {
    let (Value::String(text), Value::String(pattern)) = (held, wanted) else {
        return false;
    };
    if fold_case {
        matches_pattern(&text.to_lowercase(), &pattern.to_lowercase())
    } else {
        matches_pattern(text, pattern)
    }
}

/// `%` stands for any run of characters, `_` for exactly one, and `\\` escapes
/// either of them.
///
/// Iterative with a single backtrack point rather than recursive: a pattern of
/// many `%` would otherwise cost exponentially in the length of the text, which
/// is a denial of service written by whoever typed the query.
fn matches_pattern(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut t, mut p) = (0_usize, 0_usize);
    let (mut star_at, mut resume) = (None, 0_usize);

    while t < text.len() {
        let current = pattern.get(p).copied();
        let escaped = current == Some('\\');
        let literal = if escaped {
            pattern.get(p.saturating_add(1)).copied()
        } else {
            current
        };
        match (current, literal) {
            (Some('%'), _) if !escaped => {
                star_at = Some(p);
                p = p.saturating_add(1);
                resume = t;
            }
            (Some('_'), _) if !escaped => {
                p = p.saturating_add(1);
                t = t.saturating_add(1);
            }
            (Some(_), Some(want)) if text.get(t).copied() == Some(want) => {
                p = p.saturating_add(if escaped { 2 } else { 1 });
                t = t.saturating_add(1);
            }
            _ => {
                // No match here. Give the last `%` one more character and retry.
                let Some(star) = star_at else {
                    return false;
                };
                resume = resume.saturating_add(1);
                t = resume;
                p = star.saturating_add(1);
            }
        }
    }
    pattern
        .get(p..)
        .is_some_and(|rest| rest.iter().all(|c| *c == '%'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::matches_pattern;

    #[test]
    fn a_pattern_covers_the_whole_value() {
        assert!(matches_pattern("ada", "ada"));
        assert!(!matches_pattern("ada lovelace", "ada"));
        assert!(matches_pattern("ada lovelace", "ada%"));
        assert!(matches_pattern("ada lovelace", "%lovelace"));
        assert!(matches_pattern("ada lovelace", "%love%"));
        assert!(!matches_pattern("ada", ""));
        assert!(matches_pattern("", ""));
        assert!(matches_pattern("", "%"));
    }

    #[test]
    fn an_underscore_stands_for_exactly_one_character() {
        assert!(matches_pattern("ada", "ad_"));
        assert!(!matches_pattern("ad", "ad_"));
        assert!(!matches_pattern("adam", "ad_"));
    }

    #[test]
    fn a_backslash_makes_a_wildcard_literal() {
        assert!(matches_pattern("100%", "100\\%"));
        assert!(!matches_pattern("100x", "100\\%"));
        assert!(matches_pattern("a_b", "a\\_b"));
        assert!(!matches_pattern("axb", "a\\_b"));
    }

    #[test]
    fn many_wildcards_do_not_cost_exponentially() {
        // The reason the matcher backtracks from one remembered star rather than
        // recursing: this pattern against this text is the classic blow-up, and
        // it is written by whoever typed the query.
        let text = "a".repeat(64);
        assert!(!matches_pattern(&text, "%a%a%a%a%a%a%a%a%b"));
        assert!(matches_pattern(&text, "%a%a%a%a%a%a%a%a%a"));
    }

    #[test]
    fn trailing_wildcards_match_nothing_at_all() {
        assert!(matches_pattern("ada", "ada%"));
        assert!(matches_pattern("ada", "ada%%%"));
        assert!(!matches_pattern("ada", "ada_"));
    }
}
