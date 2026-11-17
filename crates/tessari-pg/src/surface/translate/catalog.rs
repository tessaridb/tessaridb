//! The catalog responder: the small static relation set the dialect's clients
//! interrogate — `pg_type`, `pg_namespace`, `pg_range`, `pg_am` — plus the
//! user-table relations `pg_class` and `pg_attribute`, answered from the
//! store's own introspection (the dynamic rows a query names are built at
//! answer time, never ahead of it).
//!
//! The subset stays deliberately narrow and refuses everything else by name
//! (code `0A000`): a projection of column paths, INNER/LEFT joins on one
//! equality, a WHERE of equality, ordered comparison over numeric catalog
//! columns, `IN` value lists, anchored-literal pattern matches, `IS NULL`,
//! the visibility predicate the dialect's clients fold on, a CASE mapping in
//! the projection, and an ORDER BY. Everything the dialect allows but this
//! build does not speak is refused by name, never approximated.

use tessaridb::Value;

use super::super::types;
use super::parser::Parser;
use super::{Columns, Item, Refused, Shaped, code};

pub(crate) use dynamic::Dynamic;
pub(crate) use evaluate::evaluate;
use relations::catalog_type_of;
use terms::{Cmp, Operand, Term, operand_text, term};

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

/// One ORDER BY key: an output ordinal or a column path, and the direction.
#[derive(Debug, Clone)]
struct Order {
    ordinal: Option<usize>,
    path: Option<String>,
    desc: bool,
}

/// One materialized row: the qualified columns it holds.
type Row = Vec<(String, Option<Value>)>;

/// A parsed catalog statement over the recognized subset.
#[derive(Debug, Clone)]
pub(crate) struct Query {
    projection: Vec<Item>,
    from: Source,
    joins: Vec<Join>,
    terms: Vec<Term>,
    order: Vec<Order>,
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

    /// Whether answering names user-table rows the store's introspection
    /// must build: `pg_class` or `pg_attribute` in the source or a join.
    pub(crate) fn needs_dynamic(&self) -> bool {
        self.relations()
            .any(|name| matches!(name, "pg_class" | "pg_attribute"))
    }

    /// The projection items the query answers under, in order.
    pub(crate) fn projection(&self) -> &[Item] {
        &self.projection
    }

    /// The relation names the query reads: the source, then every join.
    pub(crate) fn relations(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.from.name).chain(self.joins.iter().map(|join| join.source.name))
    }

    /// A term pinning an identity column (`oid`, `attrelid`) to exactly one
    /// value — which user table a query asks about, so the introspection
    /// reads only that table's shape.
    pub(crate) fn pinned_oid(
        &self,
        slots: &[Option<String>],
        parameters: &tessaridb::Parameters,
    ) -> Option<i64> {
        let identity = |column: &str| {
            matches!(
                column.rsplit('.').next().unwrap_or(column),
                "oid" | "attrelid"
            )
        };
        for term in &self.terms {
            let Term::Cmp {
                column,
                op: Cmp::Eq,
                operand,
            } = term
            else {
                continue;
            };
            if !identity(column) {
                continue;
            }
            let held = match operand_text(operand, slots, parameters) {
                Some(held) => held,
                None => continue,
            };
            if let Ok(parsed) = held.parse::<i64>() {
                return Some(parsed);
            }
        }
        None
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
    let order = if parser.keyword("ORDER").is_ok() {
        if parser.keyword("BY").is_err() {
            return Err(Refused::surface(code::SYNTAX, "ORDER wants BY"));
        }
        let mut keys = Vec::new();
        loop {
            keys.push(order_key(parser)?);
            if parser.comma().is_err() {
                break;
            }
        }
        keys
    } else {
        Vec::new()
    };
    let limit = if parser.keyword("LIMIT").is_ok() {
        Some(parser.number()?)
    } else {
        None
    };
    // What follows — another statement — is the caller's to end or refuse;
    // rest() stops at its own clauses.
    let columns = Columns::Named(items.iter().map(|item| item.column.clone()).collect());
    let mut param_types = vec![0_u32; parser.slots_count()];
    for held in &terms {
        if let Term::Cmp {
            column,
            op: Cmp::Eq,
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
            order,
            limit,
            param_types,
        }),
        inference: Vec::new(),
        record_names: Vec::new(),
        tag: super::write::Tag::Select,
        returning: None,
    })
}

/// One ORDER BY key: a 1-based output ordinal or a column path, and the
/// direction the absence of ASC/DESC leaves as ascending.
fn order_key(parser: &mut Parser<'_>) -> Result<Order, Refused> {
    let at = parser.snapshot();
    let digits = parser.digits();
    if !digits.is_empty() {
        let ordinal = digits
            .parse::<usize>()
            .map_err(|_| Refused::surface(code::SYNTAX, "the ordinal does not fit"))?;
        let desc = desc_or_asc(parser)?;
        return Ok(Order {
            ordinal: Some(ordinal),
            path: None,
            desc,
        });
    }
    parser.restore(at);
    let path = parser.ident_path()?;
    let desc = desc_or_asc(parser)?;
    Ok(Order {
        ordinal: None,
        path: Some(path),
        desc,
    })
}

fn desc_or_asc(parser: &mut Parser<'_>) -> Result<bool, Refused> {
    if parser.keyword("DESC").is_ok() {
        return Ok(true);
    }
    if parser.keyword("ASC").is_ok() {
        return Ok(false);
    }
    Ok(false)
}

/// One relation source after the schema word: the dot, the name, the alias.
fn source(parser: &mut Parser<'_>, schema: &str) -> Result<Source, Refused> {
    if !parser.dot() {
        return Err(Refused::surface(
            code::SYNTAX,
            "the catalog wants a relation after its schema",
        ));
    }
    let asked = parser.ident()?;
    let name = relation_name(&asked).ok_or_else(|| {
        Refused::with_detail(
            code::FEATURE,
            "this catalog relation is not spoken in this build",
            format!("{schema}.{asked}"),
        )
    })?;
    let alias = if parser.keyword("AS").is_ok() {
        parser.ident()?
    } else {
        parser.bare_alias().unwrap_or(asked)
    };
    Ok(Source { name, alias })
}

/// The relations the responder serves: the static set, plus the two the
/// store's introspection builds at answer time.
fn relation_name(name: &str) -> Option<&'static str> {
    match name {
        "pg_type" | "pg_namespace" | "pg_range" | "pg_am" | "pg_class" | "pg_attribute"
        | "pg_attrdef" | "pg_collation" => Some(match name {
            "pg_namespace" => "pg_namespace",
            "pg_range" => "pg_range",
            "pg_am" => "pg_am",
            "pg_class" => "pg_class",
            "pg_attribute" => "pg_attribute",
            "pg_attrdef" => "pg_attrdef",
            "pg_collation" => "pg_collation",
            _ => "pg_type",
        }),
        _ => None,
    }
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

pub(crate) mod dynamic;
mod evaluate;
mod relations;
pub(crate) mod terms;
#[cfg(test)]
mod tests;
