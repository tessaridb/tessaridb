//! The SQL this surface speaks, translated to the store's own statement text
//! with every literal bound as a parameter.
//!
//! The shape the BI baseline reads: `SELECT` over tables with a boolean
//! condition, joins on a value, folds with a `GROUP BY`, an ordering, and a
//! limit — plus the placeholder the extended protocol binds. Everything the
//! dialect allows but this build does not speak is refused by name (code
//! `0A000`), never approximated: a client sees "not spoken" and knows what it
//! is holding.

mod expression;
mod parser;
mod shape;
mod verbs;

pub(crate) mod catalog;
pub(crate) mod write;

use tessaridb::{Parameters, Value};

pub(crate) use super::errors::{Refused, code};
use parser::Parser;

/// What the translator owes the caller: the script, its bound values, and one
/// shape per statement the query held. `slots` maps a `$n` placeholder to the
/// parameter name the Bind's nth value takes — `None` marks a slot the query
/// never used, which Bind refuses.
#[derive(Debug, Clone)]
pub(crate) struct Translated {
    pub(crate) script: String,
    pub(crate) parameters: Parameters,
    /// The statements' shapes, in the order they run — one for the extended
    /// protocol (which speaks one statement per Parse), more for a
    /// multi-statement simple query.
    pub(crate) statements: Vec<Shaped>,
    /// The parameter name each `$1..$n` takes, in placeholder order.
    pub(crate) slots: Vec<Option<String>>,
}

/// The projection's shape: the names it answers under, in order, or a star
/// whose names the answer itself must supply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Columns {
    Named(Vec<String>),
    Star,
}

/// The name the dialect gives a column the projection did not name.
pub(crate) const UNNAMED: &str = "?column?";

/// Translate one statement — the extended protocol's Parse, which speaks
/// exactly one. A refusal carries the wire code and the words a client shows.
pub(crate) fn translate(query: &str) -> Result<Translated, Refused> {
    if let Some(first) = query.split_whitespace().next()
        && write::opens(first)
    {
        return write::translate(query);
    }
    let mut parser = Parser::new(query);
    parser.select()
}

/// Translate a simple query's text, which the dialect says may hold several
/// statements: they run as one implicit transaction block, the store's own
/// `BEGIN … COMMIT` wrapping them — except when a transaction verb opens any
/// of the statements, which is then the client's own block, run as it is
/// written (`verbs`).
pub(crate) fn translate_many(query: &str) -> Result<Translated, Refused> {
    if verbs::present(query) {
        return verbs::translate(query);
    }
    if let Some(first) = query.split_whitespace().next()
        && write::opens(first)
    {
        return write::translate_many(query);
    }
    let mut parser = Parser::new(query);
    parser.select_many()
}

/// The words a bare alias may not be: the shape's own keywords. `AS` is not
/// here because the `AS` path consumed it already.
fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "select"
            | "from"
            | "where"
            | "limit"
            | "offset"
            | "and"
            | "or"
            | "not"
            | "is"
            | "null"
            | "true"
            | "false"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "full"
            | "outer"
            | "on"
            | "group"
            | "having"
            | "order"
            | "by"
            | "asc"
            | "desc"
            | "nulls"
            | "first"
            | "last"
            | "distinct"
            | "in"
            | "case"
            | "when"
            | "then"
            | "else"
            | "end"
            | "collate"
            | "operator"
    )
}

/// One shaped query, before the parser's collected state moves out.
#[derive(Debug, Clone)]
pub(crate) struct Shaped {
    pub(crate) script: String,
    pub(crate) columns: Columns,
    pub(crate) table: Option<String>,
    /// A catalog statement's local form: answered by the responder, never
    /// sent to the store (`script` stays empty).
    pub(crate) catalog: Option<catalog::Query>,
    /// The parameter inference the statement collected: `(slot index,
    /// column path)` for every `column = $n` comparison — what Parse-time
    /// type inference asks the table catalog about.
    pub(crate) inference: Vec<(usize, String)>,
    /// The record field each unnamed arithmetic column answers under in the
    /// store's shape (the port's own `eN` alias), `None` where the wire
    /// name and the record name are the same.
    pub(crate) record_names: Vec<Option<String>>,
    /// The rows the answer drops from its head — the dialect's `OFFSET m`,
    /// exact: the store is asked for a wider bounded read (`LIMIT n+m`), and
    /// the port discards the first `m` rows of the answer it gets.
    pub(crate) offset: u64,
    /// A WARNING the statement's answer carries beside the completion — the
    /// dropped column's kept values, so the store's declaration-not-data
    /// semantic is never silent.
    pub(crate) notice: Option<String>,
    /// The completion tag the wire answers the statement under.
    pub(crate) tag: write::Tag,
    /// A write's RETURNING projection — `None` where the wire owes only the
    /// count.
    pub(crate) returning: Option<Columns>,
}

/// A projection item the catalog answers by computing rather than looking a
/// column up; over the store these refuse by name.
#[derive(Debug, Clone)]
pub(crate) enum Computed {
    /// `CASE <path> WHEN <literal> THEN <literal> … [ELSE <literal>] END`.
    Case {
        subject: String,
        arms: Vec<(Value, Value)>,
        otherwise: Option<Value>,
    },
    /// `CASE WHEN <condition> THEN <expr> … [ELSE <expr>] END` — the
    /// searched form the deep describe folds conditions through. The
    /// conditions are the catalog's own term shape, evaluated against the
    /// row like a WHERE; the arms are literals or casted paths.
    SearchedCase {
        arms: Vec<(catalog::terms::Term, Expr)>,
        otherwise: Option<Expr>,
    },
    /// A bare boolean the projection carries (`'d' = any(stxkind) AS
    /// ndist_enabled`): a catalog term read as a truth value against the row.
    Predicate(catalog::terms::Term),
    /// A bare literal the projection carries (`false AS relhasoids`, `''`) —
    /// the catalog answers it from itself; the store shape binds it as a
    /// parameter like any other literal.
    Lit(Value),
    /// `pg_catalog.pg_get_userbyid(<path>)` — answered from the connection's
    /// identity, not from the store.
    OwnerOf,
    /// `pg_catalog.<fn>(<args>)` — a catalog function the responder answers
    /// from its own rows or the store's introspection (`format_type`,
    /// `pg_get_expr`).
    Call { path: String, args: Vec<CallArg> },
    /// `(SELECT … FROM pg_catalog.… WHERE …)` in the projection — a scalar
    /// subquery the responder evaluates against the outer row (the WHERE may
    /// name the outer aliases, as the deep describe's column listing does).
    ScalarSubquery { query: catalog::Query },
}

/// One searched-CASE arm value: a literal, a path carrying `::` casts, or a
/// catalog function call (`array_to_string` over a role list).
#[derive(Debug, Clone)]
pub(crate) enum Expr {
    Lit(Value),
    Path {
        path: String,
        casts: Vec<String>,
    },
    Call {
        path: String,
        args: Vec<CallArg>,
    },
    /// `<expr> = <literal>` read as the boolean it is — an ORDER BY key the
    /// partition listing sorts on.
    Eq {
        left: Box<Expr>,
        right: Value,
    },
}

/// One catalog function argument: a column path, a literal, or a subquery —
/// `array(SELECT …)` the policy listing folds a role list through.
#[derive(Debug, Clone)]
pub(crate) enum CallArg {
    Path(String),
    Lit(Value),
    Subquery(Box<catalog::Query>),
    /// A nested catalog call — `array(SELECT …)` the role-list fold hands
    /// its outer function.
    Call {
        path: String,
        args: Vec<CallArg>,
    },
}

/// One projection, rendered and named, with the items it was built from —
/// the catalog path answers from the items directly.
pub(crate) struct Projection {
    pub(crate) text: String,
    pub(crate) columns: Columns,
    pub(crate) items: Vec<Item>,
}

/// One projection item, rendered and named.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub(crate) text: String,
    pub(crate) column: String,
    /// What the catalog computes instead of looking up, when this item is
    /// not a plain column path.
    pub(crate) computed: Option<Computed>,
}

#[cfg(test)]
mod tests;
