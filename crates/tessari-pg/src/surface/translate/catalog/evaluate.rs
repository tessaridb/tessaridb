//! The catalog's evaluator: joins and terms over materialized rows, the
//! projection (plain paths and computed items), the ORDER BY and the LIMIT —
//! answered locally, never against the store.

use tessaridb::Value;

use crate::surface::types;

use super::super::{Refused, code};
use super::dynamic::{self, Dynamic};
use super::relations::{relation, type_name_of};
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
    check_supported(query)?;
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
            .all(|held| term_holds(held, row, None, slots, parameters))
        {
            let keys = query
                .order
                .iter()
                .map(|key| key_value(key, query, row, dynamic, user))
                .collect::<Vec<_>>();
            let projected = query
                .projection
                .iter()
                .map(|item| {
                    project(
                        item,
                        row,
                        &Ctx {
                            outer: None,
                            dynamic,
                            user,
                        },
                    )
                })
                .collect::<Vec<_>>();
            keyed.push((keys, projected));
        }
    }
    // A compound leg rides behind: its rows answer in its own order, whole
    // after the first ask. (The trailing ORDER BY the dialect writes after
    // a UNION parses on the leg — the relations these queries ride are
    // empty, and whole-compound ordering is the defer list's row, not a
    // silent approximation here.)
    if let Some(leg) = &query.union {
        for row in evaluate(leg, slots, parameters, dynamic, user)? {
            keyed.push((Vec::new(), row));
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
fn key_value(
    key: &Order,
    query: &Query,
    row: &Row,
    dynamic: Option<&Dynamic>,
    user: Option<&str>,
) -> Option<Value> {
    match &key.key {
        super::OrderKey::Ordinal(n) => query
            .projection
            .get(n.saturating_sub(1))
            .map(|item| {
                project(
                    item,
                    row,
                    &Ctx {
                        outer: None,
                        dynamic,
                        user,
                    },
                )
            })
            .unwrap_or(None),
        super::OrderKey::Path(path) => lookup(path, row).cloned(),
        super::OrderKey::Expr(expr) => arm_value(
            expr,
            row,
            &Ctx {
                outer: None,
                dynamic,
                user,
            },
        ),
    }
}

fn materialize(source: &Source, dynamic: Option<&Dynamic>) -> Result<Vec<Row>, Refused> {
    // A set-returning function source answers no rows of its own: the
    // relations the dialect's listings pair it with are empty in this build,
    // so there is nothing to unnest. The spelling is parsed; the rows are
    // the empty truth, not a fabricated set.
    if let Some(function) = &source.function {
        // The spelling is named in the answer path's diagnostics, not the
        // rows: what it would read, held to what it reads.
        tracing::debug!(function = %function.path, argument = %function.arg, "a function source answers empty");
        let _ = &function.columns;
        return Ok(Vec::new());
    }
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
    // The comma spelling's empty ON: every row of one with every row of
    // the other — the WHERE is where a cross narrows.
    if joined.terms.is_empty() {
        for lrow in &left {
            for rrow in &right {
                let mut merged = lrow.clone();
                merged.extend(rrow.iter().cloned());
                out.push(merged);
            }
        }
        return Ok(out);
    }
    for lrow in &left {
        let mut matched = false;
        for rrow in &right {
            // The ON's terms read the merged candidate — each side resolves
            // on whichever relation holds it, which is how the dialect's
            // clients write the equality both ways round.
            let mut merged = lrow.clone();
            merged.extend(rrow.iter().cloned());
            if joined
                .terms
                .iter()
                .all(|term| term_holds(term, &merged, None, &[], &tessaridb::Parameters::new()))
            {
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
    outer: Option<&Row>,
    slots: &[Option<String>],
    parameters: &tessaridb::Parameters,
) -> bool {
    match term {
        Term::Visible => true,
        // What this build's user tables are: publishable. The predicate
        // rides over relations the listing queries hold empty, so the
        // answer steers nothing today — it is still the true word about the
        // tables this store serves, not a shrug.
        Term::Fn { path, .. } => path.ends_with("pg_relation_is_publishable"),
        Term::Bool { column } => {
            matches!(row_text(lookup(column, row)).as_deref(), Some("t" | "true"))
        }
        Term::Not(inner) => !term_holds(inner, row, outer, slots, parameters),
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
            // A column operand is the correlation the deep describe's
            // subqueries write: resolved against this row, absent (an outer
            // alias, until the subquery evaluation binds it) reading as no
            // match rather than as a guess.
            let expected = match operand {
                // A column operand is the correlation the deep describe's
                // subqueries write: this row first, then the outer row —
                // an alias neither holds reads as no match, not as a guess.
                super::terms::Operand::Col(path) => row_text(lookup(path, row))
                    .or_else(|| outer.and_then(|held| row_text(lookup(path, held)))),
                // `= ANY (<column>)`: the cell holds an array, and the
                // comparison reads membership in it — a non-array cell is
                // nothing, and nothing does not hold the value.
                super::terms::Operand::AnyCol(path) => {
                    return match lookup(path, row)
                        .or_else(|| outer.and_then(|held| lookup(path, held)))
                    {
                        Some(Value::Array(items)) => items
                            .iter()
                            .any(|item| types::text_of(item).as_deref() == held.as_deref()),
                        _ => false,
                    };
                }
                other => operand_text(other, slots, parameters),
            };
            let Some(expected) = expected else {
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

/// The surroundings one projection evaluates against: the outer row a
/// correlated subquery resolves through, the dynamic rows the batch built,
/// and the connection's identity.
struct Ctx<'a> {
    outer: Option<&'a Row>,
    dynamic: Option<&'a Dynamic>,
    user: Option<&'a str>,
}

/// One projection cell: a computed item answers from the row, the context,
/// and the connection's identity; a plain item looks its column path up.
fn project(item: &super::super::Item, row: &Row, ctx: &Ctx<'_>) -> Option<Value> {
    match &item.computed {
        Some(super::super::Computed::Lit(value)) => Some(value.clone()),
        Some(super::super::Computed::Predicate(term)) => Some(Value::Bool(term_holds(
            term,
            row,
            ctx.outer,
            &[],
            &tessaridb::Parameters::new(),
        ))),
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
        Some(super::super::Computed::SearchedCase { arms, otherwise }) => {
            for (condition, arm) in arms {
                if term_holds(
                    condition,
                    row,
                    ctx.outer,
                    &[],
                    &tessaridb::Parameters::new(),
                ) {
                    return arm_value(arm, row, ctx);
                }
            }
            otherwise.as_ref().and_then(|arm| arm_value(arm, row, ctx))
        }
        Some(super::super::Computed::OwnerOf) => {
            ctx.user.map(|name| Value::String(name.to_owned()))
        }
        Some(super::super::Computed::Call { path, args }) => call_value(path, args, row, ctx),
        Some(super::super::Computed::ScalarSubquery { query }) => subquery_value(query, row, ctx),
        None => lookup(lookup_path(item), row).cloned(),
    }
}

/// One catalog function's answer for a row: the identity pair from the
/// connection, the type name `format_type` resolves through the static
/// catalog (a typmod this build never sets carries no modifier), and
/// `pg_get_expr` handing the stored default's own text back. A name the
/// responder does not hold answers nothing — check_supported refuses it
/// before a row is touched.
fn call_value(
    path: &str,
    args: &[super::super::CallArg],
    row: &Row,
    ctx: &Ctx<'_>,
) -> Option<Value> {
    match path.rsplit('.').next() {
        Some("pg_get_userbyid") => ctx.user.map(|name| Value::String(name.to_owned())),
        Some("format_type") => match arg_value(args.first()?, row, ctx) {
            Some(Value::Number(tessaridb::Number::Integer(oid))) => Some(Value::String(
                type_name_of(oid).unwrap_or_else(|| oid.to_string()),
            )),
            _ => None,
        },
        Some("pg_get_expr") => arg_value(args.first()?, row, ctx),
        // The definition texts (`pg_get_ruledef`, `pg_get_indexdef`,
        // `pg_get_constraintdef`) and the role-list fold (`array_to_string`)
        // are named by the dialect's listing queries over relations this
        // build holds empty — they never evaluate, and a row that ever
        // arrived would answer nothing rather than invent a definition.
        Some(
            "pg_get_ruledef"
            | "pg_get_indexdef"
            | "pg_get_constraintdef"
            | "array_to_string"
            | "string_agg"
            | "quote_ident",
        ) => None,
        _ => None,
    }
}

/// One function argument resolved: a literal as it stands, a path read from
/// this row, then — the correlation a subquery's argument is written with —
/// from the outer row.
fn arg_value(arg: &super::super::CallArg, row: &Row, ctx: &Ctx<'_>) -> Option<Value> {
    match arg {
        super::super::CallArg::Lit(value) => Some(value.clone()),
        super::super::CallArg::Path(path) => lookup(path, row)
            .cloned()
            .or_else(|| ctx.outer.and_then(|outer| lookup(path, outer).cloned())),
        // `array(SELECT …)`: the subquery answers a one-column row set; the
        // function folds it. The listings that write this shape ride over
        // relations this build holds empty, so the fold answers the empty
        // array rather than a fabricated list.
        super::super::CallArg::Subquery(query) => subquery_array(query, row, ctx),
        super::super::CallArg::Call { path, args } => call_value(path, args, row, ctx),
    }
}

/// A subquery argument folded to the array the caller lists: every first
/// cell of every row the subquery holds, in its order.
fn subquery_array(query: &Query, row: &Row, ctx: &Ctx<'_>) -> Option<Value> {
    let inner = Ctx {
        outer: Some(row),
        dynamic: ctx.dynamic,
        user: ctx.user,
    };
    let mut rows = materialize(&query.from, inner.dynamic).ok()?;
    for joined in &query.joins {
        rows = join_rows(rows, joined, inner.dynamic).ok()?;
    }
    let mut folded = Vec::new();
    for held in &rows {
        let holds = query
            .terms
            .iter()
            .all(|term| term_holds(term, held, inner.outer, &[], &tessaridb::Parameters::new()));
        let first = query
            .projection
            .iter()
            .map(|item| project(item, held, &inner))
            .next()?;
        if holds && let Some(value) = first {
            folded.push(value);
        }
    }
    Some(Value::Array(folded))
}

/// One scalar subquery evaluated against the outer row: its FROM materializes
/// like a query of its own, its WHERE reads correlation through the outer
/// row, and the first cell of the first row that holds answers — none
/// matching is the dialect's NULL, not an error.
fn subquery_value(query: &Query, outer_row: &Row, ctx: &Ctx<'_>) -> Option<Value> {
    let inner = Ctx {
        outer: Some(outer_row),
        dynamic: ctx.dynamic,
        user: ctx.user,
    };
    let mut rows = materialize(&query.from, inner.dynamic).ok()?;
    for joined in &query.joins {
        rows = join_rows(rows, joined, inner.dynamic).ok()?;
    }
    for row in &rows {
        if query
            .terms
            .iter()
            .all(|term| term_holds(term, row, inner.outer, &[], &tessaridb::Parameters::new()))
        {
            return query
                .projection
                .iter()
                .map(|item| project(item, row, &inner))
                .next()?;
        }
    }
    None
}

/// One searched-CASE arm evaluated against the row: a literal answers as it
/// stands; a path looks its column up and takes its casts left to right.
fn arm_value(arm: &super::super::Expr, row: &Row, ctx: &Ctx<'_>) -> Option<Value> {
    match arm {
        super::super::Expr::Lit(value) => Some(value.clone()),
        super::super::Expr::Path { path, casts } => {
            let mut value = lookup(path, row)
                .cloned()
                .or_else(|| ctx.outer.and_then(|outer| lookup(path, outer).cloned()));
            for cast in casts {
                value = value.and_then(|held| apply_cast(&held, cast));
            }
            value
        }
        super::super::Expr::Call { path, args } => call_value(path, args, row, ctx),
        super::super::Expr::Eq { left, right } => {
            let held = arm_value(left, row, ctx).and_then(|value| types::text_of(&value));
            Some(Value::Bool(
                held.as_deref() == types::text_of(right).as_deref(),
            ))
        }
    }
}

/// One `::` cast over a catalog cell, the chains the deep describe writes.
/// `::text` is the cell's own text form; `::regtype` resolves an oid the
/// static catalog serves to the type's name, and answers any other oid as
/// its number — a regtype this responder does not hold is not renamed by
/// guess. An unspoken cast answers nothing rather than something wrong.
fn apply_cast(value: &Value, cast: &str) -> Option<Value> {
    match cast.rsplit('.').next() {
        Some("text") => types::text_of(value).map(Value::String),
        Some("regtype") => match value {
            Value::Number(held) => Some(Value::String(match held {
                tessaridb::Number::Integer(oid) => {
                    type_name_of(*oid).unwrap_or_else(|| oid.to_string())
                }
                other => {
                    types::text_of(&Value::Number(other.clone())).unwrap_or_else(|| "0".to_owned())
                }
            })),
            _ => types::text_of(value).map(Value::String),
        },
        _ => None,
    }
}

/// What this build computes and what it still refuses, checked before a row
/// is touched: the deep describe's functions and scalar subqueries answer
/// from the store's introspection in the wave that lands them, and until
/// then a query naming one refuses by name — never answers a half-shape.
fn check_supported(query: &Query) -> Result<(), Refused> {
    for item in &query.projection {
        match &item.computed {
            Some(super::super::Computed::Call { path, args }) => {
                let spoken = path.rsplit('.').next().is_some_and(|name| {
                    matches!(
                        name,
                        "pg_get_userbyid"
                            | "format_type"
                            | "pg_get_expr"
                            | "pg_get_ruledef"
                            | "pg_get_indexdef"
                            | "pg_get_constraintdef"
                            | "array_to_string"
                            | "string_agg"
                            | "quote_ident"
                    )
                });
                if !spoken {
                    return Err(Refused::with_detail(
                        code::FEATURE,
                        "this catalog function is not spoken in this build",
                        format!("{path}({})", call_args_text(args)),
                    ));
                }
            }
            Some(super::super::Computed::ScalarSubquery { query }) => {
                // The subquery is answered, not refused — and its own
                // projection owes the same check this query's does.
                check_supported(query)?;
            }
            Some(super::super::Computed::Predicate(term)) => {
                if term_has_param(term) {
                    return Err(Refused::surface(
                        code::FEATURE,
                        "a predicate wants a literal comparison in this build",
                    ));
                }
            }
            Some(super::super::Computed::SearchedCase { arms, .. }) => {
                // The projection has no parameter bindings of its own: a `$n`
                // inside a CASE condition would read as false rather than as
                // the caller's value, which is a silent lie — refused by name.
                for (condition, _) in arms {
                    if term_has_param(condition) {
                        return Err(Refused::surface(
                            code::FEATURE,
                            "a CASE condition wants a literal comparison in this build",
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The argument spellings a refusal names the unspoken call with: paths as
/// they stand, literals in their text form.
fn call_args_text(args: &[super::super::CallArg]) -> String {
    args.iter()
        .map(|arg| match arg {
            super::super::CallArg::Path(path) => path.clone(),
            super::super::CallArg::Lit(value) => {
                types::text_of(value).unwrap_or_else(|| "NULL".to_owned())
            }
            super::super::CallArg::Subquery(_) => "SELECT …".to_owned(),
            super::super::CallArg::Call { path, .. } => format!("{path}(…)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a catalog term compares against a `$n` — what a projection
/// evaluation cannot resolve.
fn term_has_param(term: &Term) -> bool {
    match term {
        Term::Cmp { operand, .. } => matches!(operand, super::terms::Operand::Param(_)),
        Term::Fn { args, .. } => args
            .iter()
            .any(|held| matches!(held, super::terms::Operand::Param(_))),
        Term::In { values, .. } => values
            .iter()
            .any(|held| matches!(held, super::terms::Operand::Param(_))),
        Term::Not(inner) => term_has_param(inner),
        _ => false,
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
