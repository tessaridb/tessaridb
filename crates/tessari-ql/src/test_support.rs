//! What a test needs to compare two trees that were written in two places.
//!
//! # Why this is not a `PartialEq`
//!
//! Every node of this tree carries a span, and equality is derived, so equality
//! includes every byte offset. That is right for an abstract syntax — two
//! identical expressions written at two places in one script are genuinely
//! different nodes — and it is wrong for exactly one question: *did this tree
//! survive a round trip through text?* A built statement has no source, so it
//! has no offsets to survive with.
//!
//! The fix is deliberately **not** a span-insensitive `PartialEq` on the tree.
//! That would change equality for every existing consumer in order to serve a
//! test, and it would silently make two statements written at different places
//! compare equal in production code that relies on the derive today. Changing
//! what production means to make a test expressible is the wrong direction, so
//! the normalisation lives here, behind a feature, and the tree is left alone.
//!
//! # Eleven sites, enumerated mechanically
//!
//! A `Span` is declared in **eleven** places in the syntax, not the nine a
//! search for `pub span: Span` reports: `ExprKind::Fold` and `ExprKind::Call`
//! hold theirs as **enum-variant fields**, which carry no `pub` because variant
//! fields are already public. Missing those two would leave every `count(*)`
//! and every function call holding a real offset, and the round trip would fail
//! on the first query containing one — with a difference pointing at a span
//! instead of at the arm that was never written.
//!
//! All eleven are walked, and so is every statement form — not only the one
//! [`crate::render`] writes back out. A normalisation that reaches part of a
//! tree does not refuse the rest: it reports two identical statements as
//! **different**, which is a wrong answer carrying no signal at all. The
//! exhaustive matches below are what keep it complete as the language grows.

use crate::ast::{
    Edit, Expr, ExprKind, FieldPath, InfoSubject, JoinSide, Name, Projection, RecordTarget, Script,
    Select, Source, Statement, StatementKind, TableRef, UserChange, Written,
};
use crate::token::Span;

/// The span every node is set to, so that none of them can differ.
const CANONICAL: Span = Span::new(0, 0);

/// Set every span in the script to one value, so two trees compare by shape.
///
/// The walk is exhaustive over both statement forms and expression forms: a new
/// variant fails to compile here rather than silently keeping its offsets.
pub fn erase_spans(script: &mut Script) {
    script.span = CANONICAL;
    for statement in &mut script.statements {
        erase_statement(statement);
    }
}

/// One statement, and everything under it.
///
/// Every form is walked, not only the one [`crate::render`] writes. A
/// normalisation that reaches part of a tree does not refuse the rest — it
/// reports two identical statements as **different**, which is a wrong answer
/// carrying no signal. That is worth more than the arms it costs.
fn erase_statement(statement: &mut Statement) {
    statement.span = CANONICAL;
    match &mut statement.kind {
        StatementKind::Select(select) => erase_select(select),
        StatementKind::Explain(select) => erase_select(select),
        StatementKind::Return { value } | StatementKind::Throw { value } => erase_expr(value),
        StatementKind::Let { value, span, .. } => {
            *span = CANONICAL;
            erase_expr(value);
        }
        StatementKind::Use {
            namespace,
            database,
        } => {
            erase_optional_name(namespace.as_mut());
            erase_optional_name(database.as_mut());
        }
        StatementKind::DefineNamespace { name, .. }
        | StatementKind::DefineDatabase { name, .. }
        | StatementKind::DefineTable { name, .. }
        | StatementKind::DefineSpace { name, .. }
        | StatementKind::DefineBucket { name, .. }
        | StatementKind::DropUser { name } => erase_name(name),
        StatementKind::DefineIndex {
            name,
            table,
            fields,
            vector,
            ..
        } => {
            erase_name(name);
            erase_table(table);
            for route in fields {
                erase_path(route);
            }
            erase_optional_name(vector.as_mut());
        }
        StatementKind::DefineField {
            name,
            table,
            analyzer,
            default,
            ..
        } => {
            erase_name(name);
            erase_table(table);
            erase_optional_name(analyzer.as_mut());
            if let Some(default) = default {
                erase_written(default);
            }
        }
        StatementKind::Info { subject } => match subject {
            InfoSubject::Store
            | InfoSubject::Namespace
            | InfoSubject::Database
            | InfoSubject::Users
            | InfoSubject::Node
            | InfoSubject::Consumers => {}
            InfoSubject::Table(table) => erase_table(table),
            InfoSubject::User(name) | InfoSubject::Consumer(name) => erase_name(name),
        },
        StatementKind::DefineAnalyzer { name, .. } => erase_name(name),
        StatementKind::DefineUser {
            name, scope, role, ..
        } => {
            erase_name(name);
            if let Some(scope) = scope {
                erase_table(scope);
            }
            erase_name(role);
        }
        StatementKind::AlterUser { name, change } => {
            erase_name(name);
            if let UserChange::Role(role) = change {
                erase_name(role);
            }
        }
        StatementKind::DefineNode { roles, .. } => erase_names(roles.as_deref_mut()),
        StatementKind::DefineReplica { name, roles, .. } => {
            erase_name(name);
            erase_names(roles.as_deref_mut());
        }
        StatementKind::DefineConsumer {
            name,
            format,
            identity,
            mapping,
            destination,
            ..
        } => {
            erase_name(name);
            erase_name(format);
            erase_path(identity);
            for pair in mapping {
                erase_path(&mut pair.from);
                erase_name(&mut pair.to);
            }
            erase_table(destination);
        }
        StatementKind::DropConsumer { name } => erase_name(name),
        StatementKind::Grant {
            verbs,
            table,
            fields,
            user,
        } => {
            erase_names(Some(verbs));
            erase_table(table);
            erase_names(Some(fields));
            erase_name(user);
        }
        StatementKind::Revoke { verbs, table, user } => {
            erase_names(Some(verbs));
            erase_table(table);
            erase_name(user);
        }
        StatementKind::DropField { name, table }
        | StatementKind::DropIndex { name, table }
        | StatementKind::RebuildIndex { name, table } => {
            erase_name(name);
            erase_table(table);
        }
        StatementKind::DropTable { table } => erase_table(table),
        StatementKind::Relate {
            from,
            edges,
            to,
            value,
        } => {
            erase_record(from);
            erase_table(edges);
            erase_record(to);
            if let Some(value) = value {
                erase_expr(value);
            }
        }
        StatementKind::Create { target, value, .. }
        | StatementKind::Set { target, value }
        | StatementKind::Put { target, value, .. } => {
            erase_record(target);
            erase_expr(value);
        }
        StatementKind::Update { target, edit, .. } | StatementKind::Upsert { target, edit, .. } => {
            erase_record(target);
            match edit {
                Edit::Whole(value) | Edit::Merge(value) => erase_expr(value),
                Edit::Fields(assignments) => {
                    for assignment in assignments {
                        erase_path(&mut assignment.route);
                        erase_expr(&mut assignment.value);
                    }
                }
            }
        }
        StatementKind::Delete { target, .. }
        | StatementKind::Get { target }
        | StatementKind::Del { target }
        | StatementKind::Read { target, .. } => erase_record(target),
        StatementKind::DeleteWhere {
            table, condition, ..
        } => {
            erase_table(table);
            erase_expr(condition);
        }
        StatementKind::Keys { space, range } => {
            erase_table(space);
            if let Some(range) = range {
                erase_expr(&mut range.start);
                erase_expr(&mut range.end);
            }
        }
        StatementKind::Backup { .. }
        | StatementKind::Begin
        | StatementKind::Commit
        | StatementKind::Cancel => {}
    }
}

/// One read, and everything under it.
fn erase_select(select: &mut Select) {
    select.span = CANONICAL;
    match &mut select.projection {
        Projection::All => {}
        Projection::Values(values) => {
            for projected in values {
                erase_expr(&mut projected.value);
                erase_name(&mut projected.name);
            }
        }
    }
    erase_source(&mut select.from);
    for route in &mut select.fetch {
        erase_path(route);
    }
    for key in &mut select.group {
        erase_expr(key);
    }
    for ordering in &mut select.order {
        erase_expr(&mut ordering.key);
    }
}

/// What a `FROM` names.
fn erase_source(source: &mut Source) {
    match source {
        Source::Node => {}
        Source::Record(record) => erase_record(record),
        Source::Table(table) => erase_table(table),
        Source::Traverse { from, hops, .. } => {
            erase_record(from);
            for hop in hops {
                erase_table(&mut hop.edges);
                if let Some(target) = &mut hop.target {
                    erase_table(target);
                }
            }
        }
        Source::Where { table, condition } => {
            erase_table(table);
            erase_expr(condition);
        }
        Source::Join {
            left,
            right,
            left_key,
            right_key,
            condition,
        } => {
            erase_join_side(left);
            erase_join_side(right);
            erase_path(left_key);
            erase_path(right_key);
            if let Some(condition) = condition {
                erase_expr(condition);
            }
        }
        Source::Subquery { read, condition } => {
            erase_select(read);
            if let Some(condition) = condition {
                erase_expr(condition);
            }
        }
    }
}

/// One side of a join, whichever of the two it is.
fn erase_join_side(side: &mut JoinSide) {
    match side {
        JoinSide::Table { table, alias } => {
            erase_table(table);
            if let Some(alias) = alias {
                erase_name(alias);
            }
        }
        JoinSide::Read { read, alias } => {
            erase_select(read);
            erase_name(alias);
        }
    }
}

/// One expression, and everything under it.
fn erase_expr(expr: &mut Expr) {
    expr.span = CANONICAL;
    match &mut expr.kind {
        // A value holds no spans, and a parameter is a name without one.
        ExprKind::Literal(_) | ExprKind::Parameter(_) => {}
        ExprKind::Path(path) => erase_path(path),
        ExprKind::Not(inner) | ExprKind::Negate(inner) => erase_expr(inner),
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            erase_expr(condition);
            erase_expr(then);
            if let Some(otherwise) = otherwise {
                erase_expr(otherwise);
            }
        }
        ExprKind::Coalesce(left, right) => {
            erase_expr(left);
            erase_expr(right);
        }
        // The first of the two sites a search for `pub span: Span` cannot see.
        ExprKind::Fold { over, span, .. } => {
            *span = CANONICAL;
            if let Some(over) = over {
                erase_expr(over);
            }
        }
        // The second.
        ExprKind::Call {
            arguments, span, ..
        } => {
            *span = CANONICAL;
            for argument in arguments {
                erase_expr(argument);
            }
        }
        ExprKind::And(left, right) | ExprKind::Or(left, right) => {
            erase_expr(left);
            erase_expr(right);
        }
        ExprKind::Arithmetic { left, right, .. } | ExprKind::Binary { left, right, .. } => {
            erase_expr(left);
            erase_expr(right);
        }
        ExprKind::Table(table) => erase_table(table),
        ExprKind::Record(record) | ExprKind::Get(record) => erase_record(record),
        ExprKind::Array(items) | ExprKind::Set(items) => {
            for item in items {
                erase_expr(item);
            }
        }
        ExprKind::Object(fields) => {
            for field in fields {
                erase_name(&mut field.name);
                erase_expr(&mut field.value);
            }
        }
        ExprKind::Range(range) => {
            erase_expr(&mut range.start);
            erase_expr(&mut range.end);
        }
        ExprKind::Select(select) => erase_select(select),
    }
}

/// A record reference: `users:1`.
fn erase_record(record: &mut RecordTarget) {
    record.span = CANONICAL;
    erase_table(&mut record.table);
}

/// A table reference, and the database qualifying it.
fn erase_table(table: &mut TableRef) {
    table.span = CANONICAL;
    if let Some(database) = &mut table.database {
        erase_name(database);
    }
    erase_name(&mut table.name);
}

/// A route into a record.
fn erase_path(path: &mut FieldPath) {
    path.span = CANONICAL;
}

/// A name as written.
fn erase_name(name: &mut Name) {
    name.span = CANONICAL;
}

/// A name a clause may leave out.
fn erase_optional_name(name: Option<&mut Name>) {
    if let Some(name) = name {
        erase_name(name);
    }
}

/// A list of names a clause may leave out.
fn erase_names(names: Option<&mut [Name]>) {
    for name in names.unwrap_or_default() {
        erase_name(name);
    }
}

/// An expression kept as the text it was written as.
///
/// The eleventh site, and the one nothing reachable from a read can hold: it
/// sits under `DEFINE FIELD`'s default only.
fn erase_written(written: &mut Written) {
    written.span = CANONICAL;
}
