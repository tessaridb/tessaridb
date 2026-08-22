//! Giving a parsed script the values its caller supplied.
//!
//! # Why binding is a pass over the tree, and not anything else
//!
//! Three places could have taken the caller's values. Substituting into the
//! **text** before parsing is string interpolation with extra steps, and
//! removing string interpolation is the entire point. Resolving at
//! **evaluation** looks tidier, but the planner reads the expression tree to
//! find a right-hand side an index can serve — so a parameter it cannot resolve
//! would silently drop the index for exactly the reads parameters exist for.
//!
//! What is left is this: walk the parsed tree once, replace each parameter with
//! the value it is bound to, and hand everything downstream a literal. Two
//! properties fall out rather than being enforced.
//!
//! **A supplied value can never become syntax.** Substitution happens *after*
//! parsing, so there is no stage left at which one could be read as grammar.
//! That is structural — not a claim about quoting or escaping, which is the
//! claim every injection defect in the world was built on.
//!
//! **A script either binds or does nothing.** The walk finishes before the first
//! statement runs, so an unbound parameter in the last statement of a script
//! leaves the store untouched rather than half-written.
//!
//! A binding nobody used is accepted. A caller who sends a map for a script that
//! stopped using one entry has not made a mistake this store can see, and
//! refusing it would break every caller who reuses one map across two scripts.

use std::collections::BTreeMap;

use bgv_db_types::Value;

use bgv_db_types::{Number, RecordId};

use crate::ast::{
    Edit, Expr, ExprKind, Identity, Projection, RangeExpr, RecordTarget, Script, Select, Source,
    StatementKind,
};
use crate::error::{Error, Result};

/// The values a script's parameters are bound to, by name without the marker.
///
/// A map of [`Value`] rather than of text: `36` and `'36'` are different
/// questions, and a caller must not have to know how this store would have
/// parsed a string to ask the one they meant.
pub type Parameters = BTreeMap<String, Value>;

impl Script {
    /// Replace every parameter in this script with the value bound to it.
    ///
    /// # Errors
    ///
    /// [`Error::UnboundParameter`] for the first parameter no value was supplied
    /// for, naming it and pointing at where it was written. Nothing is bound
    /// when that happens: the script is consumed and no partly-bound tree
    /// escapes.
    pub fn bind(mut self, parameters: &Parameters) -> Result<Self> {
        for statement in &mut self.statements {
            bind_statement(&mut statement.kind, parameters)?;
        }
        Ok(self)
    }
}

/// Every expression a statement holds, and no statement holds one by accident.
///
/// Exhaustive, with no catch-all arm: a statement form added later that carries
/// an expression will not compile until it is named here, which is the only way
/// this stays complete as the language grows.
fn bind_statement(kind: &mut StatementKind, parameters: &Parameters) -> Result<()> {
    match kind {
        StatementKind::Create { target, value }
        | StatementKind::Set { target, value }
        | StatementKind::Put { target, value, .. } => {
            bind_target(target, parameters)?;
            bind_expr(value, parameters)
        }
        // Both shapes of an update hold expressions, and the field shape holds
        // one per assignment: a parameter is legal in each of them, the same as
        // it is anywhere else a value may stand.
        StatementKind::Update { target, edit } => {
            bind_target(target, parameters)?;
            match edit {
                Edit::Whole(value) => bind_expr(value, parameters),
                Edit::Fields(assignments) => {
                    for assignment in assignments {
                        bind_expr(&mut assignment.value, parameters)?;
                    }
                    Ok(())
                }
            }
        }
        StatementKind::Get { target }
        | StatementKind::Delete { target }
        | StatementKind::Del { target }
        | StatementKind::Read { target, .. } => bind_target(target, parameters),
        StatementKind::Relate {
            from, to, value, ..
        } => {
            bind_target(from, parameters)?;
            bind_target(to, parameters)?;
            match value {
                Some(value) => bind_expr(value, parameters),
                None => Ok(()),
            }
        }
        StatementKind::DeleteWhere { condition, .. } => bind_expr(condition, parameters),
        StatementKind::Keys { range, .. } => match range {
            Some(range) => bind_range(range, parameters),
            None => Ok(()),
        },
        StatementKind::Select(select) => bind_select(select, parameters),
        // The read it explains is a read, so its parameters bind the same way —
        // and an `EXPLAIN` of a parameterised read is exactly what somebody
        // debugging one reaches for.
        StatementKind::Explain(select) => bind_select(select, parameters),
        // Everything else names things and holds no values: the definitions, the
        // drops, the grants, the tenancy statements, the point reads and the
        // transaction words.
        StatementKind::Use { .. }
        | StatementKind::DefineNamespace { .. }
        | StatementKind::DefineDatabase { .. }
        | StatementKind::DefineTable { .. }
        | StatementKind::DefineSpace { .. }
        | StatementKind::DefineBucket { .. }
        | StatementKind::DefineIndex { .. }
        | StatementKind::DefineField { .. }
        | StatementKind::DefineAnalyzer { .. }
        | StatementKind::DefineUser { .. }
        | StatementKind::DropUser { .. }
        | StatementKind::Grant { .. }
        | StatementKind::Revoke { .. }
        | StatementKind::DropField { .. }
        | StatementKind::DropTable { .. }
        | StatementKind::DropIndex { .. }
        | StatementKind::RebuildIndex { .. }
        | StatementKind::Backup { .. }
        // A subject is a name and never a value. `INFO FOR TABLE $t` would be a
        // parameter supplying a *table*, which is refused everywhere else in
        // this language for the reason `bind_target` gives.
        | StatementKind::Info { .. }
        | StatementKind::Begin
        | StatementKind::Commit
        | StatementKind::Cancel => Ok(()),
    }
}

/// A record target's **id** may be supplied; its table may not.
fn bind_target(target: &mut RecordTarget, parameters: &Parameters) -> Result<()> {
    let Identity::Parameter(name) = &target.id else {
        return Ok(());
    };
    let Some(value) = parameters.get(name.as_str()) else {
        return Err(Error::UnboundParameter {
            name: name.clone(),
            span: target.span,
        });
    };
    let id = match value {
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
                span: target.span,
            });
        }
    };
    target.id = Identity::Fixed(id);
    Ok(())
}

fn bind_select(select: &mut Select, parameters: &Parameters) -> Result<()> {
    if let Projection::Values(projected) = &mut select.projection {
        for one in projected {
            bind_expr(&mut one.value, parameters)?;
        }
    }
    match &mut select.from {
        Source::Record(target) => bind_target(target, parameters)?,
        Source::Traverse { from, .. } => bind_target(from, parameters)?,
        Source::Where { condition, .. } => bind_expr(condition, parameters)?,
        Source::Join { condition, .. } => {
            if let Some(condition) = condition {
                bind_expr(condition, parameters)?;
            }
        }
        Source::Table(_) => {}
    }
    for key in &mut select.group {
        bind_expr(key, parameters)?;
    }
    for ordering in &mut select.order {
        bind_expr(&mut ordering.key, parameters)?;
    }
    Ok(())
}

fn bind_range(range: &mut RangeExpr, parameters: &Parameters) -> Result<()> {
    bind_expr(&mut range.start, parameters)?;
    bind_expr(&mut range.end, parameters)
}

fn bind_expr(expr: &mut Expr, parameters: &Parameters) -> Result<()> {
    match &mut expr.kind {
        ExprKind::Parameter(name) => {
            let Some(value) = parameters.get(name.as_str()) else {
                return Err(Error::UnboundParameter {
                    name: name.clone(),
                    span: expr.span,
                });
            };
            expr.kind = ExprKind::Literal(value.clone());
            Ok(())
        }
        ExprKind::Not(inner) | ExprKind::Negate(inner) => bind_expr(inner, parameters),
        // What a fold folds over is an ordinary per-record expression, so a
        // parameter inside it binds like any other. `count(*)` folds over the
        // records themselves and has nothing to bind.
        ExprKind::Fold { over, .. } => match over {
            Some(over) => bind_expr(over, parameters),
            None => Ok(()),
        },
        ExprKind::Call { arguments, .. } => {
            for argument in arguments {
                bind_expr(argument, parameters)?;
            }
            Ok(())
        }
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            bind_expr(left, parameters)?;
            bind_expr(right, parameters)
        }
        ExprKind::Array(items) | ExprKind::Set(items) => {
            for item in items {
                bind_expr(item, parameters)?;
            }
            Ok(())
        }
        ExprKind::Object(fields) => {
            for field in fields {
                bind_expr(&mut field.value, parameters)?;
            }
            Ok(())
        }
        ExprKind::Range(range) => bind_range(range, parameters),
        ExprKind::Select(select) => bind_select(select, parameters),
        // A literal is already a value; a path, a table and a record are names,
        // which a parameter may never be.
        ExprKind::Record(target) | ExprKind::Get(target) => bind_target(target, parameters),
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Table(_) => Ok(()),
    }
}
