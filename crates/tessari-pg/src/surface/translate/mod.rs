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

use tessaridb::Parameters;

pub(crate) use super::errors::{Refused, code};
use parser::Parser;

/// What the translator owes the caller: the script, its bound values, and the
/// column order the projection named (or the mark that it was `*`). `slots`
/// maps a `$n` placeholder to the parameter name the Bind's nth value takes —
/// `None` marks a slot the query never used, which Bind refuses.
#[derive(Debug, Clone)]
pub(crate) struct Translated {
    pub(crate) script: String,
    pub(crate) parameters: Parameters,
    pub(crate) columns: Columns,
    /// The table the FROM named, for the identity column's spelling.
    pub(crate) table: Option<String>,
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

/// Translate one query string. A refusal carries the wire code and the words
/// a client shows.
pub(crate) fn translate(query: &str) -> Result<Translated, Refused> {
    let mut parser = Parser::new(query);
    parser.select()
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
    )
}

/// One shaped query, before the parser's collected state moves out.
pub(crate) struct Shaped {
    pub(crate) script: String,
    pub(crate) columns: Columns,
    pub(crate) table: Option<String>,
}

/// One projection, rendered and named.
pub(crate) struct Projection {
    pub(crate) text: String,
    pub(crate) columns: Columns,
}

/// One projection item, rendered and named.
pub(crate) struct Item {
    pub(crate) text: String,
    pub(crate) column: String,
}

#[cfg(test)]
mod tests;
