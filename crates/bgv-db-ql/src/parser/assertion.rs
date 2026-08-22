//! Lowering `ASSERT <expr>` into the constraint the store can check.
//!
//! The store checks assertions on its apply path, where the verdict has to be a
//! pure function of the record and the catalog — so what it holds is a closed
//! [`Assertion`] rather than an expression tree. This is where the one source
//! spelling becomes that one compiled form, and where everything outside the
//! vocabulary is refused **by name**.
//!
//! Refusing here rather than at evaluation is the whole point: the store is then
//! incapable of meeting an assertion it cannot check, so there is no runtime
//! branch for one and no way for two replicas to differ over what a constraint
//! meant.

use bgv_db_types::{Assertion, Value};

use crate::ast::{Expr, ExprKind};
use crate::error::{Error, Result};

/// The parameter an assertion may name, and the only one.
const VALUE: &str = "value";

/// The constraint an assertion expression describes.
///
/// # Errors
///
/// Returns [`Error::AssertionNotAConstraint`] for anything outside the
/// vocabulary — a call, an arithmetic expression, a path into the record, a
/// parameter other than `$value`, a fold, a bare literal.
pub(super) fn lower(expr: &Expr) -> Result<Assertion> {
    match &expr.kind {
        ExprKind::And(left, right) => Ok(Assertion::All(vec![lower(left)?, lower(right)?])),
        ExprKind::Or(left, right) => Ok(Assertion::Any(vec![lower(left)?, lower(right)?])),
        ExprKind::Not(inner) => Ok(Assertion::Not(Box::new(lower(inner)?))),
        // `$value <op> <literal>`, and in that order. The mirror spelling
        // (`0 < $value`) is refused rather than flipped: flipping is only
        // correct for the ordered operators, and a rule that holds for six of
        // eleven cases is one a reader has to memorise.
        ExprKind::Binary { op, left, right } if matches!(&left.kind, ExprKind::Parameter(name) if name == VALUE) => {
            Ok(Assertion::Compare {
                op: *op,
                against: literal(right)
                    .ok_or(Error::AssertionNotAConstraint { span: right.span })?,
            })
        }
        _ => Err(Error::AssertionNotAConstraint { span: expr.span }),
    }
}

/// The value an expression is, when it is one written down.
///
/// A collection literal counts, because `$value IN ['new', 'paid']` is the
/// natural spelling of a set of permitted values and every element of it is
/// still written in the statement. Anything that would have to be *computed* —
/// a call, arithmetic, a path — is not a literal here even when it happens to
/// be constant, because "happens to be constant" is a property somebody would
/// have to re-establish every time the function set grows.
fn literal(expr: &Expr) -> Option<Value> {
    match &expr.kind {
        ExprKind::Literal(held) => Some(held.clone()),
        ExprKind::Array(items) => items
            .iter()
            .map(literal)
            .collect::<Option<_>>()
            .map(Value::Array),
        ExprKind::Set(items) => items
            .iter()
            .map(literal)
            .collect::<Option<_>>()
            .map(Value::Set),
        _ => None,
    }
}
