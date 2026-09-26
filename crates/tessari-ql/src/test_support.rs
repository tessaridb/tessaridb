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
    CreateTarget, Edit, Expr, ExprKind, FieldPath, InfoSubject, JoinSide, Name, Projection,
    ReachRef, RecordTarget, Script, Select, SetCondition, Source, Statement, StatementKind,
    TableRef, UserChange, UserGrant, Written,
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
            // A consumer name is a literal and carries no span of its own, so
            // there is nothing here to normalise.
            consumer: _,
        } => {
            erase_optional_name(namespace.as_mut());
            erase_optional_name(database.as_mut());
        }
        StatementKind::DefineNamespace { name, .. }
        | StatementKind::DefineDatabase { name, .. }
        | StatementKind::DefineTable { name, .. }
        | StatementKind::DefineSpace { name, .. }
        | StatementKind::DefineTopic { name, .. }
        | StatementKind::DefineBucket { name, .. }
        | StatementKind::DefineCollection { name, .. }
        // In the name-only list rather than in its own arm, unlike
        // `DEFINE VECTOR`: a geo store declares no clause, so a name is all
        // there is to erase.
        | StatementKind::DefineGeo { name, .. }
        | StatementKind::DefineGraph { name, .. }
        | StatementKind::DefineEdge { name, .. }
        | StatementKind::DropEdge { name }
        | StatementKind::DropVector { name }
        | StatementKind::DropGeo { name }
        // The name-only list for the same reason `DEFINE GEO` is there: a vault
        // declares no clause either, and its key is minted at execution rather
        // than written in the statement.
        | StatementKind::DefineVault { name, .. }
        | StatementKind::DropVault { name }
        // The name-only list, because a queue's two clauses are literals with no
        // span of their own to erase.
        | StatementKind::DefineQueue { name, .. }
        | StatementKind::DropQueue { name }
        // A series joins it for the same reason: its retention is a literal.
        | StatementKind::DefineSeries { name, .. }
        | StatementKind::DropSeries { name }
        // A view joins the name-only list because its read is stored as text
        // rather than as a tree, so it carries no span to erase either.
        | StatementKind::DefineView { name, .. }
        | StatementKind::DropView { name }
        | StatementKind::DropGraph { name }
        | StatementKind::DropUser { name }
        | StatementKind::DropAnalyzer { name }
        | StatementKind::DropReplica { name }
        | StatementKind::DropDatabase { name }
        | StatementKind::DropNamespace { name } => erase_name(name),
        // The change is a word the statement was written with rather than a
        // value, so there is nothing under it to normalise.
        StatementKind::AlterTable { table, .. } => erase_table(table),
        StatementKind::AlterField {
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
            InfoSubject::Table(table) | InfoSubject::Access(table) => erase_table(table),
            InfoSubject::Recipients(target)
            | InfoSubject::Versions(target)
            | InfoSubject::History(target) => {
                erase_record(target);
            }
            // The only subject whose name is optional, so it cannot join the
            // list below without unwrapping there.
            InfoSubject::Audit(actor) => {
                if let Some(name) = actor {
                    erase_name(name);
                }
            }
            InfoSubject::User(name)
            | InfoSubject::Consumer(name)
            | InfoSubject::Graph(name)
            | InfoSubject::Vector(name)
            | InfoSubject::Geo(name)
            | InfoSubject::Vault(name)
            | InfoSubject::Bucket(name) => {
                erase_name(name);
            }
            InfoSubject::Topic(table) => erase_table(table),
        },
        // Its own arm rather than the name-only list above, because the
        // distance is a `Name` too: left unerased it carries a span, and two
        // identical declarations would compare as different.
        StatementKind::DefineVector { name, distance, .. } => {
            erase_name(name);
            erase_name(distance);
        }
        StatementKind::DefineAnalyzer { name, .. } => erase_name(name),
        StatementKind::DefineUser {
            name, scope, role, ..
        } => {
            erase_name(name);
            if let Some(scope) = scope {
                erase_reach(scope);
            }
            match role {
                UserGrant::Role(role) => erase_name(role),
                UserGrant::Authorities(kinds) => erase_names(Some(kinds)),
            }
        }
        StatementKind::GrantAuthority { kinds, reach, user }
        | StatementKind::RevokeAuthority { kinds, reach, user } => {
            erase_names(Some(kinds));
            erase_reach(reach);
            erase_name(user);
        }
        StatementKind::AlterUser { name, change } => {
            erase_name(name);
            if let UserChange::Role(role) = change {
                erase_name(role);
            }
        }
        StatementKind::AlterNamespace { name, .. } => erase_name(name),
        StatementKind::DefineNode { roles, .. } => erase_names(roles.as_deref_mut()),
        // Nothing to erase: a failover policy is five durations and carries no
        // name at all. The arm exists so that adding a name to the statement
        // later cannot pass this helper silently.
        StatementKind::DefineFailover { .. } => {}
        StatementKind::DefineReplica {
            name,
            roles,
            replicates,
            leads,
            ..
        } => {
            erase_name(name);
            erase_names(roles.as_deref_mut());
            if let Some(reach) = replicates {
                erase_reach(reach);
            }
            if let Some(reach) = leads {
                erase_reach(reach);
            }
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
        StatementKind::DropTable { table } | StatementKind::CheckTable { table } => {
            erase_table(table);
        }
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
        StatementKind::DeleteEdge {
            from, edges, to, ..
        } => {
            erase_record(from);
            erase_table(edges);
            erase_record(to);
        }
        StatementKind::Insert {
            table,
            columns,
            rows,
        } => {
            erase_table(table);
            for column in columns.iter_mut() {
                erase_name(column);
            }
            for row in rows.iter_mut() {
                for value in row.iter_mut() {
                    erase_expr(value);
                }
            }
        }
        StatementKind::Create { target, value, .. } => {
            match target {
                CreateTarget::Named(named) => erase_record(named),
                CreateTarget::Generated(table) => erase_table(table),
            }
            erase_expr(value);
        }
        StatementKind::Set {
            target,
            value,
            expire,
            condition,
        } => {
            erase_record(target);
            erase_expr(value);
            if let Some(expire) = expire {
                erase_expr(expire);
            }
            if let Some(SetCondition::Equals(expected)) = condition {
                erase_expr(expected);
            }
        }
        StatementKind::Put { target, value, .. } | StatementKind::Expire { target, at: value } => {
            erase_record(target);
            erase_expr(value);
        }
        StatementKind::Incr { target, by } => {
            erase_record(target);
            if let Some(by) = by {
                erase_expr(by);
            }
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
        | StatementKind::Persist { target }
        | StatementKind::Read { target, .. } => erase_record(target),
        StatementKind::DeleteWhere {
            table, condition, ..
        } => {
            erase_table(table);
            erase_expr(condition);
        }
        StatementKind::DeleteSpan { table, span, .. } => {
            erase_table(table);
            *span = CANONICAL;
        }
        StatementKind::Claim { table, span, .. } => {
            erase_table(table);
            *span = CANONICAL;
        }
        StatementKind::ClaimRecord { target, span } | StatementKind::Release { target, span, .. } => {
            erase_record(target);
            *span = CANONICAL;
        }
        StatementKind::ReleaseAll { table, span, .. } => {
            erase_table(table);
            *span = CANONICAL;
        }
        StatementKind::ReadTopic {
            topic,
            after,
            limit,
            ..
        } => {
            erase_table(topic);
            for bound in [after, limit].into_iter().flatten() {
                erase_expr(bound);
            }
        }
        StatementKind::Keys {
            space,
            range,
            prefix,
            after,
            ..
        } => {
            erase_table(space);
            if let Some(range) = range {
                erase_expr(&mut range.start);
                erase_expr(&mut range.end);
            }
            for bound in [prefix, after].into_iter().flatten() {
                erase_expr(bound);
            }
        }
        // `REVEAL` names one record, so its target is erased the way every
        // other single-record statement's is.
        // Both recipient statements name one record; the material is an
        // expression and is erased as one.
        StatementKind::AddRecipient {
            target,
            recipient,
            material,
            span,
        } => {
            erase_record(target);
            erase_expr(recipient);
            erase_expr(material);
            *span = CANONICAL;
        }
        StatementKind::RemoveRecipient {
            target,
            recipient,
            span,
        } => {
            erase_record(target);
            erase_expr(recipient);
            *span = CANONICAL;
        }
        StatementKind::Reveal {
            target,
            fields,
            span,
        } => {
            erase_record(target);
            for field in fields {
                erase_name(field);
            }
            *span = CANONICAL;
        }
        // The passphrase is not erased because it is not a span — and it is not
        // compared either: two `UNSEAL`s differing only in their passphrase are
        // two different statements, which is the right answer.
        StatementKind::UnsealVault { span, .. } | StatementKind::SealVault { span } => {
            *span = CANONICAL;
        }
        StatementKind::Backup { .. }
        | StatementKind::Begin
        | StatementKind::Commit
        | StatementKind::Cancel
        | StatementKind::Verify => {}
    }
}

/// One read, and everything under it.
fn erase_select(select: &mut Select) {
    select.span = CANONICAL;
    if let Some(at) = &mut select.only {
        *at = CANONICAL;
    }
    match &mut select.projection {
        Projection::All => {}
        Projection::Values { everything, values } => {
            if let Some(at) = everything {
                *at = CANONICAL;
            }
            for projected in values {
                erase_expr(&mut projected.value);
                erase_name(&mut projected.name);
            }
        }
    }
    erase_source(&mut select.from);
    if let Some(route) = &mut select.split {
        erase_path(route);
    }
    for route in &mut select.omit {
        erase_path(route);
    }
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
        Source::Range { table, span, .. } => {
            erase_table(table);
            *span = CANONICAL;
        }
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
        ExprKind::Record(record) | ExprKind::Get(record) | ExprKind::Ttl(record) => {
            erase_record(record);
        }
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

/// A reach, in whichever of its three spellings the statement used.
fn erase_reach(reach: &mut ReachRef) {
    match reach {
        ReachRef::Store => {}
        ReachRef::Namespace(name) => erase_name(name),
        ReachRef::Database(table) => erase_table(table),
        ReachRef::Shard {
            namespace,
            database,
            table,
            ..
        } => {
            erase_name(namespace);
            erase_name(database);
            erase_name(table);
        }
    }
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
