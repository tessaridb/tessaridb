//! Binding the parameters inside identities, reads and expressions.

use super::{Binding, bind_join_side, bind_range, bind_target};
use crate::ast::{Expr, ExprKind, Identity, Projection, Select, Source};
use crate::error::{Error, Result};
use crate::token::Span;
use tessari_types::Value;
use tessari_types::{Number, RecordId};

/// One identity, wherever it stands.
///
/// Pulled out of [`bind_target`] rather than copied when `Source::Range` needed
/// the same thing at both ends of a span: two copies of a value-to-identity
/// conversion is two lists of which kinds may name a record, and the second one
/// goes out of step the first time a kind is added.
pub(crate) fn bind_identity(id: &mut Identity, at: Span, binding: &Binding<'_>) -> Result<()> {
    let Identity::Parameter(name) = &*id else {
        return Ok(());
    };
    if binding.deferred.contains(name.as_str()) {
        return Ok(());
    }
    let Some(value) = binding.supplied.get(name.as_str()) else {
        if !binding.strict {
            return Ok(());
        }
        return Err(Error::UnboundParameter {
            name: name.clone(),
            span: at,
        });
    };
    let held = match value {
        Value::Number(Number::Integer(held)) => RecordId::Int(*held),
        Value::String(text) => RecordId::Text(text.clone()),
        Value::Uuid(bytes) => RecordId::Uuid(*bytes),
        Value::Bytes(bytes) => RecordId::Bytes(bytes.clone()),
        // A float, an object, a duration: values a record cannot be identified
        // by. Refused where it is supplied rather than converted into text,
        // which would make `1.0` and `'1.0'` the same record.
        other => {
            return Err(Error::NotARecordIdentity {
                name: name.clone(),
                found: other.type_name(),
                span: at,
            });
        }
    };
    *id = Identity::Fixed(held);
    Ok(())
}

pub(crate) fn bind_select(select: &mut Select, binding: &Binding<'_>) -> Result<()> {
    if let Projection::Values {
        values: projected, ..
    } = &mut select.projection
    {
        for one in projected {
            bind_expr(&mut one.value, binding)?;
        }
    }
    match &mut select.from {
        Source::Record(target) => bind_target(target, binding)?,
        // Both ends, because either may be a parameter: `FROM events:$from..$to`
        // is how a window arrives from a caller rather than from a literal.
        Source::Range {
            lower, upper, span, ..
        } => {
            bind_identity(lower, *span, binding)?;
            bind_identity(upper, *span, binding)?;
        }
        Source::Traverse { from, .. } => bind_target(from, binding)?,
        Source::Where { condition, .. } => bind_expr(condition, binding)?,
        Source::Join {
            left,
            right,
            condition,
            ..
        } => {
            // A side that is a read carries every position a binding reaches,
            // so it is bound the same way the outer read is. This is what keeps
            // a bound name reaching an index inside a joined subquery, exactly
            // as it does outside one.
            bind_join_side(left, binding)?;
            bind_join_side(right, binding)?;
            if let Some(condition) = condition {
                bind_expr(condition, binding)?;
            }
        }
        Source::Subquery { read, condition } => {
            bind_select(read, binding)?;
            if let Some(condition) = condition {
                bind_expr(condition, binding)?;
            }
        }
        // Neither names a value a caller could bind: a table is a name, and
        // this node's identity is not addressed at all.
        Source::Node | Source::Table(_) => {}
    }
    for key in &mut select.group {
        bind_expr(key, binding)?;
    }
    for ordering in &mut select.order {
        bind_expr(&mut ordering.key, binding)?;
    }
    Ok(())
}

pub(crate) fn bind_expr(expr: &mut Expr, binding: &Binding<'_>) -> Result<()> {
    match &mut expr.kind {
        ExprKind::Parameter(name) => {
            if binding.deferred.contains(name.as_str()) {
                // A `LET` above this point will bind it when the script runs.
                // Left standing deliberately — see [`Binding`].
                return Ok(());
            }
            let Some(value) = binding.supplied.get(name.as_str()) else {
                if !binding.strict {
                    return Ok(());
                }
                return Err(Error::UnboundParameter {
                    name: name.clone(),
                    span: expr.span,
                });
            };
            expr.kind = ExprKind::Literal(value.clone());
            Ok(())
        }
        ExprKind::Not(inner) | ExprKind::Negate(inner) => bind_expr(inner, binding),
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            bind_expr(condition, binding)?;
            bind_expr(then, binding)?;
            match otherwise {
                Some(otherwise) => bind_expr(otherwise, binding),
                None => Ok(()),
            }
        }
        ExprKind::Coalesce(left, right) => {
            bind_expr(left, binding)?;
            bind_expr(right, binding)
        }
        // What a fold folds over is an ordinary per-record expression, so a
        // parameter inside it binds like any other. `count(*)` folds over the
        // records themselves and has nothing to bind.
        ExprKind::Fold { over, at, .. } => {
            if let Some(over) = over {
                bind_expr(over, binding)?;
            }
            match at {
                Some(at) => bind_expr(at, binding),
                None => Ok(()),
            }
        }
        ExprKind::Call { arguments, .. } => {
            for argument in arguments {
                bind_expr(argument, binding)?;
            }
            Ok(())
        }
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            bind_expr(left, binding)?;
            bind_expr(right, binding)
        }
        ExprKind::Array(items) | ExprKind::Set(items) => {
            for item in items {
                bind_expr(item, binding)?;
            }
            Ok(())
        }
        ExprKind::Object(fields) => {
            for field in fields {
                bind_expr(&mut field.value, binding)?;
            }
            Ok(())
        }
        ExprKind::Range(range) => bind_range(range, binding),
        ExprKind::Select(select) => bind_select(select, binding),
        // A literal is already a value; a path, a table and a record are names,
        // which a parameter may never be.
        ExprKind::Record(target) | ExprKind::Get(target) | ExprKind::Ttl(target) => {
            bind_target(target, binding)
        }
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Table(_) => Ok(()),
    }
}
