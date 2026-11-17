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
    /// A set-returning function the dialect's listings place in FROM —
    /// `unnest(stxkeys) s(attnum)`. This build holds the relations these
    /// accompany empty, so the source answers no rows of its own; the
    /// argument and column names ride through for the grammar's honesty.
    pub(crate) function: Option<FunctionSource>,
}

/// One function source's spelling: the path, its argument, and the column
/// names the alias pair bound.
#[derive(Debug, Clone)]
pub(crate) struct FunctionSource {
    pub(crate) path: String,
    pub(crate) arg: String,
    pub(crate) columns: Vec<String>,
}

/// One join leg: the relation, whether absent rows survive, and the terms
/// the pairing must hold — one equality in the common shape, a conjunction
/// in the dialect's listing queries. No terms at all is the comma spelling's
/// cross.
#[derive(Debug, Clone)]
struct Join {
    source: Source,
    outer: bool,
    terms: Vec<terms::Term>,
}

/// One ORDER BY key and the direction.
#[derive(Debug, Clone)]
struct Order {
    key: OrderKey,
    desc: bool,
}

/// What an ORDER BY key reads: an output ordinal, a column path, or a
/// scalar expression of the dialect's own — a catalog call compared to a
/// literal, a casted path — the partition listing's shapes.
#[derive(Debug, Clone)]
enum OrderKey {
    Ordinal(usize),
    Path(String),
    Expr(super::Expr),
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
    /// A compound leg: `UNION ALL <query>` the publication listing folds two
    /// asks into one. Rows concatenate; `ALL` keeps duplicates, the only
    /// spelling this build speaks.
    union: Option<Box<Query>>,
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
            .iter()
            .any(|name| matches!(*name, "pg_class" | "pg_attribute" | "pg_attrdef"))
    }

    /// The projection items the query answers under, in order.
    pub(crate) fn projection(&self) -> &[Item] {
        &self.projection
    }

    /// The relation names the query reads: the source, then every join,
    /// then the compound leg's.
    pub(crate) fn relations(&self) -> Vec<&'static str> {
        let mut named = vec![self.from.name];
        named.extend(self.joins.iter().map(|join| join.source.name));
        if let Some(leg) = &self.union {
            named.extend(leg.relations());
        }
        named
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
    // The dialect's comma spelling is a cross of everything it separates —
    // `FROM a, b` pairs every row of one with every row of the other, and
    // the WHERE is where the pairing narrows. An empty ON pair marks it.
    while parser.comma().is_ok() {
        let schema = parser.ident()?;
        if !opens(&schema) {
            return Err(Refused::surface(
                code::FEATURE,
                "a catalog cross serves catalog relations in this build",
            ));
        }
        let added = source(parser, &schema)?;
        joins.push(Join {
            source: added,
            outer: false,
            terms: Vec::new(),
        });
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
    // A compound leg: `UNION ALL SELECT …`. The second ask is a catalog
    // query of its own; the projection's SELECT is consumed here.
    let union = if parser.keyword("UNION").is_ok() {
        if parser.keyword("ALL").is_err() {
            return Err(Refused::surface(
                code::FEATURE,
                "UNION wants ALL in this build; DISTINCT is not spoken",
            ));
        }
        if parser.keyword("SELECT").is_err() {
            return Err(Refused::surface(code::SYNTAX, "UNION wants SELECT"));
        }
        let leg_items = parser.projection()?.items;
        if parser.keyword("FROM").is_err() {
            return Err(Refused::surface(code::SYNTAX, "UNION wants FROM"));
        }
        let leg_schema = parser.ident()?;
        if !opens(&leg_schema) {
            return Err(Refused::surface(
                code::FEATURE,
                "a UNION leg serves catalog relations in this build",
            ));
        }
        let leg = rest(parser, leg_items, &leg_schema)?;
        Some(Box::new(leg.catalog.ok_or_else(|| {
            Refused::surface(code::INTERNAL, "a UNION leg built no catalog query")
        })?))
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
            union,
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
            key: OrderKey::Ordinal(ordinal),
            desc,
        });
    }
    parser.restore(at);
    let expr = parser.order_expr()?;
    let desc = desc_or_asc(parser)?;
    let key = match expr {
        super::Expr::Path { path, casts } if casts.is_empty() => OrderKey::Path(path),
        other => OrderKey::Expr(other),
    };
    Ok(Order { key, desc })
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
    // A set-returning function in FROM: `unnest(stxkeys) s(attnum)`. The
    // relations it accompanies are empty in this build, so the source
    // answers no rows; the spelling is parsed, not faked.
    if parser.open_paren() {
        let arg = parser.ident_path()?;
        parser.close_paren()?;
        let name: &'static str = match asked.as_str() {
            "unnest" => "unnest",
            _ => {
                return Err(Refused::with_detail(
                    code::FEATURE,
                    "this catalog function source is not spoken in this build",
                    format!("{schema}.{asked}"),
                ));
            }
        };
        let alias = if parser.keyword("AS").is_ok() {
            parser.ident()?
        } else {
            parser.bare_alias().unwrap_or_else(|| asked.clone())
        };
        let mut columns = Vec::new();
        if parser.open_paren() {
            loop {
                columns.push(parser.ident()?);
                if parser.comma().is_err() {
                    break;
                }
            }
            parser.close_paren()?;
        }
        return Ok(Source {
            name,
            alias,
            function: Some(FunctionSource {
                path: format!("{schema}.{asked}"),
                arg,
                columns,
            }),
        });
    }
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
    Ok(Source {
        name,
        alias,
        function: None,
    })
}

/// The relations the responder serves: the static set, plus the two the
/// store's introspection builds at answer time.
fn relation_name(name: &str) -> Option<&'static str> {
    match name {
        "pg_type" | "pg_namespace" | "pg_range" | "pg_am" | "pg_class" | "pg_attribute"
        | "pg_attrdef" | "pg_collation" | "pg_policy" | "pg_roles" | "pg_rewrite"
        | "pg_inherits" | "pg_statistic_ext" | "pg_publication" | "pg_publication_rel" => {
            Some(match name {
                "pg_namespace" => "pg_namespace",
                "pg_range" => "pg_range",
                "pg_am" => "pg_am",
                "pg_class" => "pg_class",
                "pg_attribute" => "pg_attribute",
                "pg_attrdef" => "pg_attrdef",
                "pg_collation" => "pg_collation",
                "pg_policy" => "pg_policy",
                "pg_roles" => "pg_roles",
                "pg_rewrite" => "pg_rewrite",
                "pg_inherits" => "pg_inherits",
                "pg_statistic_ext" => "pg_statistic_ext",
                "pg_publication" => "pg_publication",
                "pg_publication_rel" => "pg_publication_rel",
                _ => "pg_type",
            })
        }
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
    // The dialect's clients parenthesise the condition — `(a.x = b.y)` —
    // and the listing queries conjoin several: the parens ride through and
    // every term must hold, changing nothing about the common single-equality
    // shape, which is one term.
    let grouped = parser.open_paren();
    let mut conditions = vec![terms::term(parser)?];
    while parser.keyword("AND").is_ok() {
        conditions.push(terms::term(parser)?);
    }
    if grouped {
        parser.close_paren()?;
    }
    Ok(Some(Join {
        source: joined,
        outer,
        terms: conditions,
    }))
}

pub(crate) mod dynamic;
mod evaluate;
mod relations;
pub(crate) mod terms;
#[cfg(test)]
mod tests;
