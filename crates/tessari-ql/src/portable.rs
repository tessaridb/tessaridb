//! A condition written so another node evaluates it exactly as this one would
//! (ADR-0097).
//!
//! # Only what answers the same everywhere
//!
//! A node that lacks a shard asks the shard's leader to narrow the records it
//! sends. The asker evaluates the condition again over whatever arrives, so a
//! leader that keeps too much costs bandwidth and nothing else; a leader that
//! keeps too LITTLE is a record missing from an answer that says it is whole.
//! So a condition travels only when it reads nothing but the record in hand:
//! no clock, no generator, no search state, no other record, no subquery.
//!
//! # Values never become text
//!
//! The condition a session holds has its parameters bound into it, so its
//! literals include caller values. Each literal is lifted out into a parameter
//! of its own and travels as a value; the text carries only operators, paths
//! and function names — which is the only thing the renderer will write.

use crate::ast::{Expr, ExprKind};
use crate::bind::Parameters;
use crate::error::Result;
use crate::function::Purity;
use tessari_types::BinaryOp;

/// The condition as text another node can read, and the values its parameters
/// stand for — or `None` when it reads more than the record in hand.
#[must_use]
pub fn portable(condition: &Expr) -> Option<(String, Parameters)> {
    if !reads_only_the_record(condition) {
        return None;
    }
    let mut lifted = condition.clone();
    let mut parameters = Parameters::new();
    lift(&mut lifted, &mut parameters);
    let mut text = String::new();
    crate::render::write_expr(&mut text, &lifted).ok()?;
    Some((text, parameters))
}

/// Read an expression [`portable`] wrote, with its values bound.
///
/// The text arrives from another node, so this is a trust boundary: what is
/// read back is held to the whitelist [`portable`] sends under, whoever wrote
/// it. An expression reading more than the record — another table, the clock —
/// would otherwise be evaluated with this node's whole store behind it, and
/// its value sent back to a peer entitled to one table.
///
/// # Errors
///
/// A parse failure, [`crate::Error::UnboundParameter`] for a parameter the
/// values do not carry, and [`crate::Error::Unrenderable`] for text that is not
/// one expression reading only the record in hand.
pub fn bound_condition(text: &str, parameters: &Parameters) -> Result<Expr> {
    // Read in the position it was written in. A bare name is a field inside a
    // `WHERE` and a table outside one, so reading the text on its own would
    // turn every path into a table — a condition that compares tables and
    // keeps nothing, on the node whose answer the asker trusts not to drop.
    let script =
        crate::parse(&format!("SELECT * FROM portable WHERE {text};"))?.bind(parameters)?;
    let refused = crate::Error::Unrenderable {
        statement: "an expression that does not read back as one reading only the record",
        span: crate::Span::new(0, text.len()),
    };
    let mut statements = script.statements.into_iter();
    let (Some(statement), None) = (statements.next(), statements.next()) else {
        return Err(refused);
    };
    let crate::StatementKind::Select(select) = statement.kind else {
        return Err(refused);
    };
    let crate::Source::Where { condition, .. } = select.from else {
        return Err(refused);
    };
    if !reads_only_the_record(&condition) {
        return Err(refused);
    }
    Ok(*condition)
}

/// Whether `expr` reads the record in hand and nothing else, the same way on
/// every node. A whitelist, so a form the language gains later stays home until
/// somebody decides it travels.
fn reads_only_the_record(expr: &Expr) -> bool {
    match &expr.kind {
        // A path routes inside the record; a literal is a value.
        ExprKind::Literal(_) | ExprKind::Path(_) => true,
        ExprKind::Not(inner) | ExprKind::Negate(inner) => reads_only_the_record(inner),
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Coalesce(left, right)
        | ExprKind::Arithmetic { left, right, .. } => {
            reads_only_the_record(left) && reads_only_the_record(right)
        }
        // A full-text match reads the field's analyzer and the query's terms,
        // which this node resolved for this read.
        ExprKind::Binary { op, left, right } => {
            !matches!(
                op,
                BinaryOp::Matches | BinaryOp::MatchesPrefix | BinaryOp::MatchesFuzzy
            ) && reads_only_the_record(left)
                && reads_only_the_record(right)
        }
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            reads_only_the_record(condition)
                && reads_only_the_record(then)
                && otherwise.as_deref().is_none_or(reads_only_the_record)
        }
        // A pure function answers the same everywhere; the clock, the
        // generator and the search family do not.
        ExprKind::Call {
            function,
            arguments,
            ..
        } => {
            function.purity() == Purity::Pure
                && !function.spelling().starts_with("search::")
                && arguments.iter().all(reads_only_the_record)
        }
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().all(reads_only_the_record),
        ExprKind::Object(fields) => fields
            .iter()
            .all(|field| reads_only_the_record(&field.value)),
        // A parameter still standing is a `LET` not yet run; the rest read
        // other records, other tables, or fold many records into one.
        ExprKind::Parameter(_)
        | ExprKind::Range(_)
        | ExprKind::Fold { .. }
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Ttl(_)
        | ExprKind::Select(_) => false,
    }
}

/// Replace every literal in `expr` with a parameter of its own, numbered in
/// the order met, and keep its value.
///
/// Called only on a tree [`reads_only_the_record`] accepted, so the forms with
/// no literal to lift are never reached.
fn lift(expr: &mut Expr, parameters: &mut Parameters) {
    match &mut expr.kind {
        ExprKind::Literal(value) => {
            let name = format!("p{}", parameters.len());
            parameters.insert(name.clone(), value.clone());
            expr.kind = ExprKind::Parameter(name);
        }
        ExprKind::Not(inner) | ExprKind::Negate(inner) => lift(inner, parameters),
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Coalesce(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            lift(left, parameters);
            lift(right, parameters);
        }
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            lift(condition, parameters);
            lift(then, parameters);
            if let Some(otherwise) = otherwise {
                lift(otherwise, parameters);
            }
        }
        ExprKind::Call { arguments, .. } => {
            for argument in arguments {
                lift(argument, parameters);
            }
        }
        ExprKind::Array(items) | ExprKind::Set(items) => {
            for item in items {
                lift(item, parameters);
            }
        }
        ExprKind::Object(fields) => {
            for field in fields {
                lift(&mut field.value, parameters);
            }
        }
        ExprKind::Parameter(_)
        | ExprKind::Path(_)
        | ExprKind::Range(_)
        | ExprKind::Fold { .. }
        | ExprKind::Table(_)
        | ExprKind::Record(_)
        | ExprKind::Get(_)
        | ExprKind::Ttl(_)
        | ExprKind::Select(_) => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;
    use crate::parse;

    /// The `WHERE` of `SELECT * FROM t WHERE …`, bound with `parameters`.
    fn condition(text: &str, parameters: &Parameters) -> Expr {
        let script = parse(&format!("SELECT * FROM t WHERE {text};"))
            .unwrap()
            .bind(parameters)
            .unwrap();
        match &script.statements[0].kind {
            crate::StatementKind::Select(select) => match &select.from {
                crate::Source::Where { condition, .. } => condition.as_ref().clone(),
                other => panic!("not a WHERE: {other:?}"),
            },
            other => panic!("not a SELECT: {other:?}"),
        }
    }

    #[test]
    fn a_condition_travels_with_its_values_beside_it_and_reads_back_the_same() {
        let supplied: Parameters = [(
            "secret".to_owned(),
            tessari_types::Value::from("it's 'quoted'"),
        )]
        .into();
        let held = condition(
            "total > 10 AND name = $secret AND string::len(note) < 3 * 4",
            &supplied,
        );
        let (text, parameters) = portable(&held).unwrap();
        assert!(
            !text.contains("quoted") && !text.contains("10"),
            "a value became text: {text}"
        );
        assert_eq!(parameters.len(), 4, "{parameters:?}");
        assert!(
            bound_condition(&text, &parameters)
                .unwrap()
                .same_shape(&held),
            "{text} does not read back as the condition it was written from"
        );
    }

    #[test]
    fn a_condition_reading_more_than_the_record_stays_home() {
        let none = Parameters::new();
        for text in [
            "at < time::now()",
            "id = rand::uuid()",
            "body MATCHES 'lock'",
            "total IN (SELECT total FROM other)",
            "search::score(body, 'lock') > 1",
        ] {
            assert_eq!(portable(&condition(text, &none)), None, "{text} was pushed");
        }
    }

    /// The text comes from another node, so reading it back is a trust
    /// boundary: what is read is held to the same whitelist that sent it,
    /// whoever wrote it.
    #[test]
    fn a_text_reading_more_than_the_record_is_refused_where_it_is_read() {
        let none = Parameters::new();
        for text in [
            "(SELECT * FROM users)",
            "total IN (SELECT total FROM other)",
            "at < time::now()",
            "true; SELECT * FROM users",
        ] {
            assert!(
                matches!(
                    bound_condition(text, &none),
                    Err(crate::Error::Unrenderable { .. })
                ),
                "{text} was read back: {:?}",
                bound_condition(text, &none)
            );
        }
        assert!(
            bound_condition(
                "(total > $p0)",
                &[("p0".to_owned(), tessari_types::Value::from(1_i64))].into()
            )
            .is_ok()
        );
    }
}
