//! The catalog's WHERE terms: what the recognized subset parses a condition
//! into, and the operand shapes each comparison speaks.

use crate::surface::types;

use super::super::parser::Parser;
use super::super::{Refused, code};
use super::relations::catalog_type_of;

/// One WHERE term; the terms conjoin.
#[derive(Debug, Clone)]
pub(crate) enum Term {
    /// `column <op> operand`
    Cmp {
        column: String,
        op: Cmp,
        operand: Operand,
    },
    /// `column IS [NOT] NULL`
    IsNull { column: String, negated: bool },
    /// `column [NOT] IN (values)`
    In {
        column: String,
        values: Vec<Operand>,
        negated: bool,
    },
    /// `NOT <term>`
    Not(Box<Term>),
    /// A bare boolean column read as a truth test.
    Bool { column: String },
    /// `pg_table_is_visible(<path>)`: the dialect's clients ask it of every
    /// row; the store's introspection already filters by what this caller
    /// may see, so every row the responder holds is visible — the predicate
    /// folds to true for exactly the rows the query can name.
    Visible,
}

/// One comparison operator the catalog terms speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    /// `~` with an anchored-literal pattern.
    Match,
    /// `!~` with an anchored-literal pattern.
    NotMatch,
}

/// One comparison operand: a bound `$n` or a literal.
#[derive(Debug, Clone)]
pub(crate) enum Operand {
    Param(usize),
    Lit(String),
}

/// One WHERE term: `NOT <term>`, a visibility predicate, a bare boolean
/// column, `col IS [NOT] NULL`, `col [NOT] IN (values)`, or `col <op>
/// operand` — with the `OPERATOR(pg_catalog.~)` spelling and a trailing
/// `COLLATE pg_catalog.default` riding through, both the dialect's own noise
/// words for what this subset already does.
pub(crate) fn term(parser: &mut Parser<'_>) -> Result<Term, Refused> {
    if parser.keyword("NOT").is_ok() {
        return Ok(Term::Not(Box::new(term(parser)?)));
    }
    let column = parser.ident_path()?;
    if column.ends_with("pg_table_is_visible") && parser.open_paren() {
        let _ = parser.ident_path()?;
        parser.close_paren()?;
        return Ok(Term::Visible);
    }
    if parser.keyword("IS").is_ok() {
        let negated = parser.keyword("NOT").is_ok();
        parser
            .keyword("NULL")
            .map_err(|_| Refused::surface(code::SYNTAX, "IS wants NULL in this build"))?;
        return Ok(Term::IsNull { column, negated });
    }
    let negated = parser.keyword("NOT").is_ok();
    if parser.keyword("IN").is_ok() {
        if !parser.open_paren() {
            return Err(Refused::surface(code::SYNTAX, "IN wants parentheses"));
        }
        let mut values = Vec::new();
        loop {
            values.push(operand(parser)?);
            if parser.comma().is_err() {
                break;
            }
        }
        parser.close_paren()?;
        if values.is_empty() {
            return Err(Refused::surface(
                code::SYNTAX,
                "IN wants at least one value",
            ));
        }
        return Ok(Term::In {
            column,
            values,
            negated,
        });
    }
    if negated {
        return Err(Refused::surface(code::SYNTAX, "NOT wants IN here"));
    }
    if !operator_follows(parser) {
        // A bare boolean column is its own truth test — `NOT attisdropped`
        // reads the column, as the dialect's clients write it.
        return Ok(Term::Bool { column });
    }
    let op = if parser.keyword("OPERATOR").is_ok() {
        operator_spelling(parser, &column)?
    } else {
        comparison(parser, &column)?
    };
    let operand = operand(parser)?;
    collate(parser)?;
    Ok(Term::Cmp {
        column,
        op,
        operand,
    })
}

/// Whether a comparison follows the cursor: an operator byte, or the
/// `OPERATOR(schema.op)` spelling — a bare column with neither is a boolean
/// truth test, not a syntax fault.
fn operator_follows(parser: &mut Parser<'_>) -> bool {
    let at = parser.snapshot();
    parser.skip_space();
    let found = parser
        .source
        .get(parser.at)
        .is_some_and(|byte| matches!(byte, b'=' | b'!' | b'<' | b'>' | b'~'))
        || parser.bare_ident().is_some_and(|word| word == "operator");
    parser.restore(at);
    found
}

/// `OPERATOR(schema.~)` — the dialect's long spelling of a pattern match.
/// Only the pattern operators ride here; anything else refuses by name.
fn operator_spelling(parser: &mut Parser<'_>, column: &str) -> Result<Cmp, Refused> {
    if !parser.open_paren() {
        return Err(Refused::surface(code::SYNTAX, "OPERATOR wants parentheses"));
    }
    let schema = parser.ident()?;
    if !parser.dot() {
        return Err(Refused::surface(
            code::SYNTAX,
            "OPERATOR wants a dotted operator name",
        ));
    }
    let op = if parser.operator("~") {
        Cmp::Match
    } else if parser.operator("!~") {
        Cmp::NotMatch
    } else {
        return Err(Refused::with_detail(
            code::FEATURE,
            "this OPERATOR spelling is not spoken in this build",
            schema,
        ));
    };
    parser.close_paren()?;
    if !is_pattern_column(column) {
        return Err(Refused::with_detail(
            code::FEATURE,
            "a pattern match speaks the catalog's name columns in this build",
            column.to_owned(),
        ));
    }
    Ok(op)
}

/// Which catalog columns a pattern match speaks: the name columns, whose
/// text form is the whole of what an anchored literal compares.
fn is_pattern_column(column: &str) -> bool {
    matches!(
        column.rsplit('.').next().unwrap_or(column),
        "relname" | "nspname" | "typname"
    )
}

/// The comparison operator at the cursor, in longest-first order. Ordered
/// comparisons speak numeric catalog columns alone — an ordered compare over
/// a name refuses by name rather than inventing a collation.
fn comparison(parser: &mut Parser<'_>, column: &str) -> Result<Cmp, Refused> {
    for (mark, op) in [
        ("!=", Cmp::Ne),
        ("<>", Cmp::Ne),
        ("<=", Cmp::Le),
        (">=", Cmp::Ge),
        ("!~", Cmp::NotMatch),
        ("~", Cmp::Match),
        ("=", Cmp::Eq),
        ("<", Cmp::Lt),
        (">", Cmp::Gt),
    ] {
        if !parser.operator(mark) {
            continue;
        }
        if matches!(op, Cmp::Gt | Cmp::Ge | Cmp::Lt | Cmp::Le) && !is_numeric_catalog_column(column)
        {
            return Err(Refused::with_detail(
                code::FEATURE,
                "an ordered catalog comparison speaks numeric columns in this build",
                column.to_owned(),
            ));
        }
        return Ok(op);
    }
    Err(Refused::with_detail(
        code::FEATURE,
        "a catalog WHERE speaks equality, IN, IS NULL and anchored patterns in this build",
        format!("near {column}"),
    ))
}

/// The dialect's regex operator arrives as `OPERATOR(pg_catalog.~)` — the
/// same `~` this subset speaks, spelled long.
fn collate(parser: &mut Parser<'_>) -> Result<(), Refused> {
    if parser.keyword("COLLATE").is_err() {
        return Ok(());
    }
    let name = parser.ident_path()?;
    if name == "pg_catalog.default" || name == "default" {
        return Ok(());
    }
    Err(Refused::with_detail(
        code::FEATURE,
        "only the default collation is spoken in this build",
        name,
    ))
}

/// One comparison operand: a bound `$n` or a literal.
fn operand(parser: &mut Parser<'_>) -> Result<Operand, Refused> {
    if let Some(held) = parser.placeholder() {
        let digits = held
            .trim_start_matches('$')
            .trim_start_matches('e')
            .parse::<usize>()
            .map_err(|_| Refused::surface(code::INTERNAL, "a placeholder lost its number"))?;
        return Ok(Operand::Param(digits.saturating_sub(1)));
    }
    if let Some(value) = parser
        .literal()
        .map_err(|_| Refused::surface(code::SYNTAX, "a catalog literal did not read"))?
    {
        return Ok(Operand::Lit(types::text_of(&value).unwrap_or_default()));
    }
    Err(Refused::surface(
        code::FEATURE,
        "a catalog comparison wants a parameter or a literal in this build",
    ))
}

/// The operand's text: a literal as written, a parameter resolved through
/// the values a Bind bound (or the simple path's translation carried).
pub(crate) fn operand_text(
    operand: &Operand,
    slots: &[Option<String>],
    parameters: &tessaridb::Parameters,
) -> Option<String> {
    match operand {
        Operand::Lit(text) => Some(text.clone()),
        Operand::Param(index) => slots
            .get(*index)
            .and_then(|slot| slot.as_ref())
            .and_then(|name| parameters.get(name))
            .and_then(types::text_of),
    }
}

/// Whether a catalog column sorts numerically under an ordered comparison:
/// the identity and count columns this subset types as oid/int/float.
fn is_numeric_catalog_column(column: &str) -> bool {
    matches!(
        catalog_type_of(column).0,
        types::oid::OID | types::oid::INT4 | types::oid::FLOAT8
    )
}
