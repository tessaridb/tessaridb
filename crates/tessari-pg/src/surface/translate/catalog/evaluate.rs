//! The catalog's evaluator: joins and terms over materialized rows, the
//! projection (plain paths and computed items), the ORDER BY and the LIMIT —
//! answered locally, never against the store.

use tessaridb::Value;

use crate::surface::types;

use super::super::{Refused, code};
use super::dynamic::{self, Dynamic};
use super::relations::relation;
use super::terms::{Cmp, Term, operand_text};
use super::{Join, Order, Query, Row, Source};

/// Evaluate the query against the relations: joins, terms, projection,
/// order, limit — answering value rows in projection order. `slots` and
/// `parameters` are the placeholder names and the values a Bind bound (the
/// simple path's translation carries both in one; the extended path's
/// portal keeps the values beside the statement's translation). `dynamic`
/// carries the user-table rows the store's introspection built, when the
/// query names them. `user` names the signed-in connection, what the owner
/// answers ride on — anonymous sessions name no owner, as the store holds
/// One row and the ordering keys it answered under: the keys the
/// ordering compares, beside the row itself.
type KeyedRow = (Vec<Option<Value>>, Vec<Option<Value>>);

/// none.
pub(crate) fn evaluate(
    query: &Query,
    slots: &[Option<String>],
    parameters: &tessaridb::Parameters,
    dynamic: Option<&Dynamic>,
    user: Option<&str>,
) -> Result<Vec<Vec<Option<Value>>>, Refused> {
    let mut rows = materialize(&query.from, dynamic)?;
    for joined in &query.joins {
        rows = join_rows(rows, joined, dynamic)?;
    }
    validate_patterns(query, slots, parameters)?;
    let mut keyed: Vec<KeyedRow> = Vec::new();
    for row in &rows {
        if query
            .terms
            .iter()
            .all(|held| term_holds(held, row, slots, parameters))
        {
            let keys = query
                .order
                .iter()
                .map(|key| key_value(key, query, row, user))
                .collect::<Vec<_>>();
            let projected = query
                .projection
                .iter()
                .map(|item| project(item, row, user))
                .collect::<Vec<_>>();
            keyed.push((keys, projected));
        }
    }
    if !query.order.is_empty() {
        keyed.sort_by(|(lk, _), (rk, _)| {
            for (i, key) in query.order.iter().enumerate() {
                let ord = compare_cells(
                    lk.get(i).unwrap_or(&None),
                    rk.get(i).unwrap_or(&None),
                    key.desc,
                );
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    let mut out: Vec<Vec<Option<Value>>> = keyed.into_iter().map(|(_, row)| row).collect();
    if let Some(limit) = query.limit {
        out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    Ok(out)
}

/// One sort key's value for a row: an ordinal names a projection item (a
/// computed item answers computed), a path looks its column up.
fn key_value(key: &Order, query: &Query, row: &Row, user: Option<&str>) -> Option<Value> {
    match (&key.ordinal, &key.path) {
        (Some(n), _) => query
            .projection
            .get(n.saturating_sub(1))
            .map(|item| project(item, row, user))
            .unwrap_or(None),
        (None, Some(path)) => lookup(path, row).cloned(),
        (None, None) => None,
    }
}

fn materialize(source: &Source, dynamic: Option<&Dynamic>) -> Result<Vec<Row>, Refused> {
    let dynamic_rows = dynamic.and_then(|held| dynamic::materialize(source, held));
    let relation = relation(source.name);
    // `pg_class` and `pg_attribute` live only in the dynamic rows — the
    // answer path guaranteed them when the query named them.
    let Some(relation) = relation else {
        return dynamic_rows.ok_or_else(|| {
            Refused::with_detail(
                code::FEATURE,
                "this catalog relation is not spoken in this build",
                source.name.to_owned(),
            )
        });
    };
    let mut rows: Vec<Row> = relation
        .rows
        .iter()
        .map(|cells| {
            relation
                .columns
                .iter()
                .zip(cells.iter())
                .map(|(column, value)| {
                    (format!("{}.{}", source.alias, column), Some(value.clone()))
                })
                .collect()
        })
        .collect();
    if let Some(extra) = dynamic_rows {
        // The connection's own namespace beside the two static ones.
        rows.extend(extra);
    }
    Ok(rows)
}

fn join_rows(
    left: Vec<Row>,
    joined: &Join,
    dynamic: Option<&Dynamic>,
) -> Result<Vec<Row>, Refused> {
    let right = materialize(&joined.source, dynamic)?;
    let mut out = Vec::new();
    for lrow in &left {
        let mut matched = false;
        for rrow in &right {
            // Each ON side resolves on whichever relation holds it — the
            // dialect's clients write the equality both ways round.
            let lval = lookup(&joined.left, lrow).or_else(|| lookup(&joined.left, rrow));
            let rval = lookup(&joined.right, rrow).or_else(|| lookup(&joined.right, lrow));
            if lval.is_some() && lval == rval {
                let mut merged = lrow.clone();
                merged.extend(rrow.iter().cloned());
                out.push(merged);
                matched = true;
            }
        }
        if !matched && joined.outer {
            let mut padded = lrow.clone();
            for (key, _) in &right.clone().into_iter().next().unwrap_or_default() {
                padded.push((key.clone(), None));
            }
            out.push(padded);
        }
    }
    Ok(out)
}

fn term_holds(
    term: &Term,
    row: &Row,
    slots: &[Option<String>],
    parameters: &tessaridb::Parameters,
) -> bool {
    match term {
        Term::Visible => true,
        Term::Bool { column } => {
            matches!(row_text(lookup(column, row)).as_deref(), Some("t" | "true"))
        }
        Term::Not(inner) => !term_holds(inner, row, slots, parameters),
        Term::IsNull { column, negated } => {
            let absent = lookup(column, row)
                .map(|value| matches!(value, Value::Null))
                .unwrap_or(true);
            // IS NULL holds where the column is absent; IS NOT NULL elsewhere.
            absent != *negated
        }
        Term::In {
            column,
            values,
            negated,
        } => {
            let held = row_text(lookup(column, row));
            let any = values.iter().any(|value| {
                operand_text(value, slots, parameters)
                    .as_deref()
                    .is_some_and(|wanted| held.as_deref() == Some(wanted))
            });
            any != *negated
        }
        Term::Cmp {
            column,
            op,
            operand,
        } => {
            let held = row_text(lookup(column, row));
            let Some(expected) = operand_text(operand, slots, parameters) else {
                return false;
            };
            let Some(held) = held else {
                return false;
            };
            match op {
                Cmp::Eq => held == expected,
                Cmp::Ne => held != expected,
                Cmp::Gt => numeric_compare(&held, &expected).is_some_and(|ord| ord.is_gt()),
                Cmp::Ge => numeric_compare(&held, &expected).is_some_and(|ord| ord.is_ge()),
                Cmp::Lt => numeric_compare(&held, &expected).is_some_and(|ord| ord.is_lt()),
                Cmp::Le => numeric_compare(&held, &expected).is_some_and(|ord| ord.is_le()),
                Cmp::Match => pattern_holds(&expected, &held),
                Cmp::NotMatch => !pattern_holds(&expected, &held),
            }
        }
    }
}

/// The cell's text form, NULL reading as absent.
fn row_text(held: Option<&Value>) -> Option<String> {
    held.and_then(types::text_of)
}

/// Ordered comparison over numeric cells: integers as integers, the rest as
/// floats — the catalog's numeric columns hold whole numbers today, and a
/// non-numeric cell makes the comparison false rather than coerced.
fn numeric_compare(held: &str, expected: &str) -> Option<std::cmp::Ordering> {
    if let (Ok(a), Ok(b)) = (held.parse::<i64>(), expected.parse::<i64>()) {
        return Some(a.cmp(&b));
    }
    match (held.parse::<f64>(), expected.parse::<f64>()) {
        (Ok(a), Ok(b)) => a.partial_cmp(&b),
        _ => None,
    }
}

/// An anchored-literal pattern, the only regex shapes this subset speaks:
/// `^(literal)$` is equality, `^(literal)` a prefix test, the literal free
/// of pattern metacharacters. Anything richer refuses by name at parse
/// time, never approximates here.
fn pattern_holds(pattern: &str, held: &str) -> bool {
    let Some((literal, full)) = anchored(pattern) else {
        return false;
    };
    if full {
        return held == literal;
    }
    held.starts_with(&literal)
}

/// The operand of every pattern term, resolved where it is bound: a pattern
/// outside the anchored-literal shapes refuses by name here, at answer time,
/// never mid-join.
fn validate_patterns(
    query: &Query,
    slots: &[Option<String>],
    parameters: &tessaridb::Parameters,
) -> Result<(), Refused> {
    for held in &query.terms {
        let Term::Cmp {
            op: Cmp::Match | Cmp::NotMatch,
            operand,
            ..
        } = held
        else {
            continue;
        };
        let Some(pattern) = operand_text(operand, slots, parameters) else {
            continue;
        };
        if anchored(&pattern).is_none() {
            return Err(Refused::with_detail(
                code::FEATURE,
                "this pattern shape is not spoken in this build",
                pattern,
            ));
        }
    }
    Ok(())
}

/// The dialect's clients wrap the anchored literal in one pair of grouping
/// parentheses — `^(name)$` — which carries no alternation here, so it
/// unwraps to the literal.
fn unwrap_group(held: &str) -> &str {
    match held
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .filter(|inner| !inner.contains('|'))
    {
        Some(inner) => inner,
        None => held,
    }
}

/// An anchored-literal pattern, the only regex shapes this subset speaks:
/// `^(literal)$` is equality, `^(literal)` a prefix test, the literal free
/// of pattern metacharacters. Anything richer refuses by name, never
/// approximates.
fn anchored(pattern: &str) -> Option<(String, bool)> {
    let body = pattern.strip_prefix('^')?;
    let metachar_free = |held: &str| {
        !held.chars().any(|c| {
            matches!(
                c,
                '.' | '[' | ']' | '{' | '}' | '(' | ')' | '|' | '*' | '+' | '?' | '\\'
            )
        })
    };
    if let Some(inner) = body.strip_suffix('$') {
        let inner = unwrap_group(inner);
        if metachar_free(inner) {
            return Some((inner.to_owned(), true));
        }
        return None;
    }
    let body = unwrap_group(body);
    if metachar_free(body) {
        return Some((body.to_owned(), false));
    }
    None
}

/// One projection cell: a computed item answers from the row and the
/// connection's identity; a plain item looks its column path up.
fn project(item: &super::super::Item, row: &Row, user: Option<&str>) -> Option<Value> {
    match &item.computed {
        Some(super::super::Computed::Case {
            subject,
            arms,
            otherwise,
        }) => {
            let held = row_text(lookup(subject, row));
            for (when, then) in arms {
                if held.as_deref() == types::text_of(when).as_deref() {
                    return Some(then.clone());
                }
            }
            otherwise.clone()
        }
        Some(super::super::Computed::OwnerOf) => user.map(|name| Value::String(name.to_owned())),
        None => lookup(lookup_path(item), row).cloned(),
    }
}

/// The expression a projection item looks up: the translator renders an
/// alias into the item text (`path AS alias`), and the alias is not a
/// column — a path holds no spaces, so the suffix is unambiguous.
fn lookup_path(item: &super::super::Item) -> &str {
    let suffix = format!(" AS {}", item.column);
    item.text.strip_suffix(&suffix).unwrap_or(&item.text)
}

/// Two cells under one key: numbers numerically, text lexically, the absent
/// cell last ascending and first descending.
fn compare_cells(left: &Option<Value>, right: &Option<Value>, desc: bool) -> std::cmp::Ordering {
    let ord = match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => match (types::text_of(a), types::text_of(b)) {
            (Some(x), Some(y)) => match numeric_compare(&x, &y) {
                Some(ord) => ord,
                None => x.cmp(&y),
            },
            _ => std::cmp::Ordering::Equal,
        },
    };
    if desc { ord.reverse() } else { ord }
}

/// A qualified `alias.column`, or a bare column resolved when unambiguous.
pub(crate) fn lookup<'a>(path: &str, row: &'a Row) -> Option<&'a Value> {
    if let Some((_, value)) = row.iter().find(|(key, _)| key == path) {
        return value.as_ref();
    }
    let suffix = format!(".{path}");
    let mut found = row
        .iter()
        .filter(|(key, _)| key.ends_with(&suffix))
        .filter_map(|(_, value)| value.as_ref());
    let first = found.next();
    first.filter(|_| found.next().is_none())
}
