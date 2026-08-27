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

use std::collections::{BTreeMap, BTreeSet};

use tessari_types::Value;

use tessari_types::{Number, RecordId};

use crate::ast::{
    Edit, Expr, ExprKind, Identity, Projection, RangeExpr, RecordTarget, Script, Select, Source,
    Statement, StatementKind,
};
use crate::error::{Error, Result};
use crate::token::Span;

/// The values a script's parameters are bound to, by name without the marker.
///
/// A map of [`Value`] rather than of text: `36` and `'36'` are different
/// questions, and a caller must not have to know how this store would have
/// parsed a string to ask the one they meant.
pub type Parameters = BTreeMap<String, Value>;

/// What a name may resolve to during the walk.
///
/// Two sources, and they are not interchangeable. `supplied` is the caller's
/// map, whose values are known now and are substituted now. `deferred` holds
/// the names a `LET` **above this point** will bind when the script runs — they
/// have no value yet, so the walk leaves them standing rather than refusing
/// them, and the run loop substitutes each one the moment its `LET` produces a
/// value.
///
/// Position matters, which is why `deferred` grows as the walk descends the
/// script rather than being collected up front: `$x` written *above* its own
/// `LET` is a name nothing will ever bind, and it is refused here — before the
/// first statement runs — rather than failing halfway down.
struct Binding<'a> {
    supplied: &'a Parameters,
    deferred: BTreeSet<&'a str>,
    /// Whether a name with no value anywhere is a failure.
    ///
    /// True for the walk a caller's map drives, where an unbound name is a
    /// mistake and must be caught before anything runs. False for the walk a
    /// `LET` drives at run time, which knows one name and must leave every
    /// other one exactly as it found it — the bindings further down the script
    /// have not happened yet.
    strict: bool,
}

impl Script {
    /// Replace every parameter in this script with the value bound to it.
    ///
    /// A name a `LET` binds is left alone: it is substituted when that `LET`
    /// runs, into the statements that have not run yet. Everything else is
    /// resolved here, so the property the whole design rests on is unchanged —
    /// by the time a statement executes, every parameter in it is a literal, and
    /// the planner still sees a right-hand side it can serve from an index.
    ///
    /// # Errors
    ///
    /// [`Error::UnboundParameter`] for the first parameter neither the caller
    /// supplied nor a preceding `LET` binds, naming it and pointing at where it
    /// was written. [`Error::BindingCollidesWithParameter`] where a `LET` and
    /// the caller name the same thing. Nothing is bound when either happens: the
    /// script is consumed and no partly-bound tree escapes.
    pub fn bind(mut self, parameters: &Parameters) -> Result<Self> {
        // Collected first, and borrowed from a copy of the names rather than
        // from `self`, because the walk below needs `&mut` on each statement
        // while holding the set.
        let names: Vec<(String, Span)> = self
            .statements
            .iter()
            .filter_map(|statement| match &statement.kind {
                StatementKind::Let { name, span, .. } => Some((name.clone(), *span)),
                _ => None,
            })
            .collect();
        for (name, span) in &names {
            if parameters.contains_key(name.as_str()) {
                return Err(Error::BindingCollidesWithParameter {
                    name: name.clone(),
                    span: *span,
                });
            }
        }
        let mut binding = Binding {
            supplied: parameters,
            deferred: BTreeSet::new(),
            strict: true,
        };
        let mut bound_so_far = 0usize;
        for statement in &mut self.statements {
            // A `LET`'s own value is bound against the names above it and not
            // its own: `LET $x = $x + 1` binds nothing, it names a value that
            // does not exist yet.
            bind_statement(&mut statement.kind, &binding)?;
            if let StatementKind::Let { .. } = &statement.kind {
                binding.deferred.insert(names[bound_so_far].0.as_str());
                bound_so_far = bound_so_far.saturating_add(1);
            }
        }
        Ok(self)
    }
}

impl Statement {
    /// Replace one name in this statement with the value a `LET` produced.
    ///
    /// Unlike [`Script::bind`] this is **not** strict: it knows one name, and
    /// every other parameter it meets belongs either to a caller's map that has
    /// already been applied or to a `LET` further down that has not run yet.
    /// Refusing those would break the very composition this exists for.
    ///
    /// # Errors
    ///
    /// Only where a supplied value cannot stand where it was written — a record
    /// identity that is not one of the four kinds a key has. An unknown name is
    /// left alone rather than refused.
    pub fn substitute(&mut self, supplied: &Parameters) -> Result<()> {
        bind_statement(
            &mut self.kind,
            &Binding {
                supplied,
                deferred: BTreeSet::new(),
                strict: false,
            },
        )
    }
}

/// Every expression a statement holds, and no statement holds one by accident.
///
/// Exhaustive, with no catch-all arm: a statement form added later that carries
/// an expression will not compile until it is named here, which is the only way
/// this stays complete as the language grows.
fn bind_statement(kind: &mut StatementKind, binding: &Binding<'_>) -> Result<()> {
    match kind {
        StatementKind::Create { target, value }
        | StatementKind::Set { target, value }
        | StatementKind::Put { target, value, .. } => {
            bind_target(target, binding)?;
            bind_expr(value, binding)
        }
        // Both shapes of an update hold expressions, and the field shape holds
        // one per assignment: a parameter is legal in each of them, the same as
        // it is anywhere else a value may stand.
        StatementKind::Update { target, edit } | StatementKind::Upsert { target, edit } => {
            bind_target(target, binding)?;
            match edit {
                Edit::Whole(value) | Edit::Merge(value) => bind_expr(value, binding),
                Edit::Fields(assignments) => {
                    for assignment in assignments {
                        bind_expr(&mut assignment.value, binding)?;
                    }
                    Ok(())
                }
            }
        }
        StatementKind::Get { target }
        | StatementKind::Delete { target }
        | StatementKind::Del { target }
        | StatementKind::Read { target, .. } => bind_target(target, binding),
        StatementKind::Relate {
            from, to, value, ..
        } => {
            bind_target(from, binding)?;
            bind_target(to, binding)?;
            match value {
                Some(value) => bind_expr(value, binding),
                None => Ok(()),
            }
        }
        StatementKind::DeleteWhere { condition, .. } => bind_expr(condition, binding),
        StatementKind::Keys { range, .. } => match range {
            Some(range) => bind_range(range, binding),
            None => Ok(()),
        },
        StatementKind::Let { value, .. } | StatementKind::Return { value } => {
            bind_expr(value, binding)
        }
        StatementKind::Select(select) => bind_select(select, binding),
        // The read it explains is a read, so its parameters bind the same way —
        // and an `EXPLAIN` of a parameterised read is exactly what somebody
        // debugging one reaches for.
        StatementKind::Explain(select) => bind_select(select, binding),
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
        | StatementKind::AlterUser { .. }
        // A role and an endpoint are written where they stand. A parameter here
        // would be a node configured by whatever a caller happened to supply,
        // which is the file-beside-the-store problem in a different shape.
        | StatementKind::DefineNode { .. }
        | StatementKind::DefineReplica { .. }
        // A broker address, a group name and a mapping are written where they
        // stand, for the reason above: a consumer whose destination arrived as a
        // parameter is a background writer aimed by whoever last called, and
        // unlike a node's configuration it keeps running afterwards.
        | StatementKind::DefineConsumer { .. }
        | StatementKind::DropConsumer { .. }
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
fn bind_target(target: &mut RecordTarget, binding: &Binding<'_>) -> Result<()> {
    let Identity::Parameter(name) = &target.id else {
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

fn bind_select(select: &mut Select, binding: &Binding<'_>) -> Result<()> {
    if let Projection::Values(projected) = &mut select.projection {
        for one in projected {
            bind_expr(&mut one.value, binding)?;
        }
    }
    match &mut select.from {
        Source::Record(target) => bind_target(target, binding)?,
        Source::Traverse { from, .. } => bind_target(from, binding)?,
        Source::Where { condition, .. } => bind_expr(condition, binding)?,
        Source::Join { condition, .. } => {
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

fn bind_range(range: &mut RangeExpr, binding: &Binding<'_>) -> Result<()> {
    bind_expr(&mut range.start, binding)?;
    bind_expr(&mut range.end, binding)
}

fn bind_expr(expr: &mut Expr, binding: &Binding<'_>) -> Result<()> {
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
        ExprKind::Fold { over, .. } => match over {
            Some(over) => bind_expr(over, binding),
            None => Ok(()),
        },
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
        ExprKind::Record(target) | ExprKind::Get(target) => bind_target(target, binding),
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Table(_) => Ok(()),
    }
}
