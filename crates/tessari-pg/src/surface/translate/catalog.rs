//! The catalog responder: the small static relation set the dialect's clients
//! interrogate — `pg_type`, `pg_namespace`, `pg_range` for this wave — answered
//! locally, never against the store's own tables.
//!
//! The subset is deliberately narrow and refuses everything else by name (code
//! `0A000`): a projection of column paths, INNER/LEFT joins on one equality,
//! a WHERE of `=` / `IS NULL` terms, a LIMIT. The relations hold what this
//! build emits; user tables and the wider introspection family arrive in the
//! next wave.

use tessaridb::Value;

use super::super::types;
use super::parser::Parser;
use super::{Columns, Item, Refused, Shaped, code};

/// One relation reference: its name and the alias the query bound it under.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    name: &'static str,
    alias: String,
}

/// One join leg: the relation, whether absent rows survive, and the two
/// columns the equality names (`alias.column` on each side).
#[derive(Debug, Clone)]
struct Join {
    source: Source,
    outer: bool,
    left: String,
    right: String,
}

/// One WHERE term; the terms conjoin.
#[derive(Debug, Clone)]
enum Term {
    /// `column = operand`
    Eq { column: String, operand: Operand },
    /// `column IS [NOT] NULL`
    IsNull { column: String, negated: bool },
}

/// One comparison operand: a bound `$n` or a literal.
#[derive(Debug, Clone)]
enum Operand {
    Param(usize),
    Lit(String),
}

/// A parsed catalog statement over the recognized subset.
#[derive(Debug, Clone)]
pub(crate) struct Query {
    projection: Vec<Item>,
    from: Source,
    joins: Vec<Join>,
    terms: Vec<Term>,
    limit: Option<u64>,
    /// The type each `$n` resolves to, from the column its term compares
    /// against — what Describe(statement) answers so the driver binds in the
    /// shape it asked about instead of guessing.
    pub(crate) param_types: Vec<u32>,
}

impl Query {
    /// The RowDescription columns the projection answers under: the output
    /// names, typed by the static catalog column types.
    pub(crate) fn columns(&self) -> Vec<crate::codec::write::Column> {
        self.projection
            .iter()
            .map(|item| {
                let (oid, name) = catalog_type_of(&item.column);
                crate::codec::write::Column {
                    name,
                    type_oid: oid,
                    type_len: types::size_of(oid),
                    format: 0,
                }
            })
            .collect()
    }
}

/// Whether the dotted name opens a catalog schema the responder serves.
pub(crate) fn opens(name: &str) -> bool {
    let head = name.split('.').next().unwrap_or(name);
    matches!(head, "pg_catalog" | "information_schema")
}

/// Parse the rest of a catalog statement. `parser` sits after the first
/// source's schema word (`pg_catalog` / `information_schema`), before the
/// dot; the projection was already parsed into `items`.
pub(crate) fn rest(
    parser: &mut Parser<'_>,
    items: Vec<Item>,
    first: &str,
) -> Result<Shaped, Refused> {
    if items.is_empty() {
        return Err(Refused::surface(
            code::FEATURE,
            "a catalog query names its columns in this build; * is not spoken over the catalog",
        ));
    }
    let from = source(parser, first)?;
    let mut joins = Vec::new();
    while let Some(joined) = join(parser)? {
        joins.push(joined);
    }
    let mut terms = Vec::new();
    if parser.keyword("WHERE").is_ok() {
        terms.push(term(parser)?);
        while parser.keyword("AND").is_ok() {
            terms.push(term(parser)?);
        }
    }
    let limit = if parser.keyword("LIMIT").is_ok() {
        Some(parser.number()?)
    } else {
        None
    };
    // What follows — GROUP BY, ORDER BY, another statement — is the caller's
    // to end or refuse; rest() stops at its own clauses.
    let columns = Columns::Named(items.iter().map(|item| item.column.clone()).collect());
    let mut param_types = vec![0_u32; parser.slots_count()];
    for held in &terms {
        if let Term::Eq {
            column,
            operand: Operand::Param(index),
        } = held
            && let Some(slot) = param_types.get_mut(*index)
        {
            *slot = catalog_type_of(column).0;
        }
    }
    Ok(Shaped {
        script: String::new(),
        columns,
        table: None,
        catalog: Some(Query {
            projection: items,
            from,
            joins,
            terms,
            limit,
            param_types,
        }),
    })
}

/// One relation source after the schema word: the dot, the name, the alias.
fn source(parser: &mut Parser<'_>, schema: &str) -> Result<Source, Refused> {
    if !parser.dot() {
        return Err(Refused::surface(
            code::SYNTAX,
            "the catalog wants a relation after its schema",
        ));
    }
    let name = parser.ident()?;
    let known = relations()
        .into_iter()
        .map(|(held, _)| held)
        .collect::<Vec<_>>();
    if !known.contains(&name.as_str()) {
        return Err(Refused::with_detail(
            code::FEATURE,
            "this catalog relation is not spoken in this build",
            format!("{schema}.{name}"),
        ));
    }
    let alias = if parser.keyword("AS").is_ok() {
        parser.ident()?
    } else {
        parser.bare_alias().unwrap_or_else(|| name.clone())
    };
    Ok(Source {
        name: known
            .into_iter()
            .find(|held| *held == name)
            .unwrap_or("pg_type"),
        alias,
    })
}

/// One join leg, or `None` when no join follows.
fn join(parser: &mut Parser<'_>) -> Result<Option<Join>, Refused> {
    let at = parser.snapshot();
    let mut outer = false;
    if parser.keyword("LEFT").is_ok() {
        outer = true;
    } else {
        let _ = parser.keyword("INNER");
    }
    if outer && parser.keyword("OUTER").is_ok() {
        // the dialect's own optional word, changing nothing
    }
    if parser.keyword("JOIN").is_err() {
        parser.restore(at);
        return Ok(None);
    }
    let schema = parser.ident()?;
    if !opens(&schema) {
        return Err(Refused::surface(
            code::FEATURE,
            "a catalog join serves catalog relations in this build",
        ));
    }
    let joined = source(parser, &schema)?;
    if parser.keyword("ON").is_err() {
        return Err(Refused::surface(code::SYNTAX, "a join wants ON"));
    }
    let left = parser.ident_path()?;
    if !parser.operator("=") {
        return Err(Refused::surface(
            code::FEATURE,
            "a catalog join speaks one equality in this build",
        ));
    }
    let right = parser.ident_path()?;
    Ok(Some(Join {
        source: joined,
        outer,
        left,
        right,
    }))
}

/// One WHERE term: `col = $n | literal`, or `col IS [NOT] NULL`.
fn term(parser: &mut Parser<'_>) -> Result<Term, Refused> {
    let column = parser.ident_path()?;
    if parser.keyword("IS").is_ok() {
        let negated = parser.keyword("NOT").is_ok();
        parser
            .keyword("NULL")
            .map_err(|_| Refused::surface(code::SYNTAX, "IS wants NULL in this build"))?;
        return Ok(Term::IsNull { column, negated });
    }
    if !parser.operator("=") {
        return Err(Refused::surface(
            code::FEATURE,
            "a catalog WHERE speaks equality and IS NULL in this build",
        ));
    }
    let operand = if let Some(held) = parser.placeholder() {
        let digits = held
            .trim_start_matches("$e")
            .parse::<usize>()
            .map_err(|_| Refused::surface(code::INTERNAL, "a placeholder lost its number"))?;
        Operand::Param(digits.saturating_sub(1))
    } else if let Some(value) = parser
        .literal()
        .map_err(|_| Refused::surface(code::SYNTAX, "a catalog literal did not read"))?
    {
        Operand::Lit(types::text_of(&value).unwrap_or_default())
    } else {
        return Err(Refused::surface(
            code::FEATURE,
            "a catalog comparison wants a parameter or a literal in this build",
        ));
    };
    Ok(Term::Eq { column, operand })
}

/// Evaluate the query against the static relations: joins, terms, projection,
/// limit — answering value rows in projection order. `slots` and `parameters`
/// are the placeholder names and the values a Bind bound (the simple path's
/// translation carries both in one; the extended path's portal keeps the
/// values beside the statement's translation).
pub(crate) fn evaluate(
    query: &Query,
    slots: &[Option<String>],
    parameters: &tessaridb::Parameters,
) -> Result<Vec<Vec<Option<Value>>>, Refused> {
    let mut rows = materialize(&query.from)?;
    for joined in &query.joins {
        rows = join_rows(rows, joined)?;
    }
    let mut out: Vec<Vec<Option<Value>>> = Vec::new();
    for row in &rows {
        if query
            .terms
            .iter()
            .all(|held| term_holds(held, row, slots, parameters))
        {
            let projected = query
                .projection
                .iter()
                .map(|item| lookup(&item.text, row).cloned())
                .collect::<Vec<_>>();
            out.push(projected);
        }
        if let Some(limit) = query.limit
            && u64::try_from(out.len()).unwrap_or(u64::MAX) >= limit
        {
            break;
        }
    }
    Ok(out)
}

/// One materialized row: the qualified columns it holds.
type Row = Vec<(String, Option<Value>)>;

fn materialize(source: &Source) -> Result<Vec<Row>, Refused> {
    let relation = relation(source.name).ok_or_else(|| {
        Refused::with_detail(
            code::FEATURE,
            "this catalog relation is not spoken in this build",
            source.name.to_owned(),
        )
    })?;
    Ok(relation
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
        .collect())
}

fn join_rows(left: Vec<Row>, joined: &Join) -> Result<Vec<Row>, Refused> {
    let right = materialize(&joined.source)?;
    let mut out = Vec::new();
    for lrow in &left {
        let lval = lookup(&joined.left, lrow);
        let mut matched = false;
        for rrow in &right {
            if lval.is_some() && lval == lookup(&joined.right, rrow).cloned().as_ref() {
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
        Term::Eq { column, operand } => {
            let held = lookup(column, row);
            if held.is_none() {
                return false;
            }
            let expected: Option<String> = match operand {
                Operand::Lit(text) => Some(text.clone()),
                Operand::Param(index) => slots
                    .get(*index)
                    .and_then(|slot| slot.as_ref())
                    .and_then(|name| parameters.get(name))
                    .and_then(types::text_of),
            };
            expected.is_some_and(|wanted| held.and_then(types::text_of).as_ref() == Some(&wanted))
        }
        Term::IsNull { column, negated } => {
            let absent = lookup(column, row)
                .map(|value| matches!(value, Value::Null))
                .unwrap_or(true);
            // IS NULL holds where the column is absent; IS NOT NULL elsewhere.
            absent != *negated
        }
    }
}

/// A qualified `alias.column`, or a bare column resolved when unambiguous.
fn lookup<'a>(path: &str, row: &'a Row) -> Option<&'a Value> {
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

/// The static type of a catalog column by its output name: the oid the
/// surface reports it under, and the name it answers with.
fn catalog_type_of(output: &str) -> (u32, String) {
    let name = output.rsplit('.').next().unwrap_or(output);
    let oid = match name {
        "oid" | "typelem" | "typbasetype" | "typrelid" | "rngtypid" | "rngsubtype"
        | "typnamespace" => types::oid::OID,
        "attnum" => types::oid::INT4,
        "reltuples" => types::oid::FLOAT8,
        "attnotnull" | "atthasdef" => types::oid::BOOL,
        _ => types::oid::TEXT,
    };
    (oid, name.to_owned())
}

/// One relation: its columns and rows, a row column-ordered.
struct Relation {
    columns: &'static [&'static str],
    rows: Vec<Vec<Value>>,
}

/// `pg_namespace`: the catalog schema, named for the clients that ask.
fn pg_namespace() -> Relation {
    Relation {
        columns: &["oid", "nspname"],
        rows: vec![
            vec![int(11), text("pg_catalog")],
            vec![int(13000), text("information_schema")],
        ],
    }
}

/// `pg_type`: exactly the types this build emits, under the dialect's oids.
fn pg_type() -> Relation {
    const ROWS: &[(u32, &str)] = &[
        (16, "bool"),
        (17, "bytea"),
        (20, "int8"),
        (21, "int2"),
        (23, "int4"),
        (25, "text"),
        (26, "oid"),
        (700, "float4"),
        (701, "float8"),
        (705, "unknown"),
        (1043, "varchar"),
        (1184, "timestamptz"),
        (1186, "interval"),
        (1700, "numeric"),
        (2950, "uuid"),
        (3802, "jsonb"),
    ];
    Relation {
        columns: &[
            "oid",
            "typname",
            "typtype",
            "typelem",
            "typbasetype",
            "typnamespace",
            "typrelid",
        ],
        rows: ROWS
            .iter()
            .map(|(oid, name)| {
                vec![
                    int(i64::from(*oid)),
                    text(name),
                    text("b"),
                    int(0),
                    int(0),
                    int(11),
                    int(0),
                ]
            })
            .collect(),
    }
}

/// `pg_range`: this build emits no range types; the LEFT JOIN answers NULLs.
fn pg_range() -> Relation {
    Relation {
        columns: &["rngtypid", "rngsubtype"],
        rows: Vec::new(),
    }
}

/// The static relations by name.
fn relations() -> Vec<(&'static str, Relation)> {
    vec![
        ("pg_type", pg_type()),
        ("pg_namespace", pg_namespace()),
        ("pg_range", pg_range()),
    ]
}

fn relation(name: &str) -> Option<Relation> {
    relations()
        .into_iter()
        .find(|(held, _)| *held == name)
        .map(|(_, rel)| rel)
}

fn int(held: i64) -> Value {
    Value::Number(held.into())
}

fn text(held: &str) -> Value {
    Value::String(held.to_owned())
}

#[cfg(test)]
mod tests;
