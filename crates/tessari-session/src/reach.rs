//! Which tables a statement reaches.
//!
//! # This match is a ratchet, and that is the whole point of it
//!
//! A grant is a permission on a *table*, so answering "may this user run this"
//! needs to know which tables the statement names. Written as an exhaustive
//! match, a statement form added later **cannot compile** until somebody decides
//! whether grants apply to it — the same device `form_name` uses in the
//! conformance crate, and the reason neither has a `_ =>` arm.
//!
//! A wildcard here would be the worst possible default in both directions: it
//! would either exempt a new statement from every grant in the store, or refuse
//! it to everybody, and nobody would find out which until it mattered.

use tessari_ql::{
    Edit, Expr, ExprKind, InfoSubject, Projection, Select, Source, StatementKind, TableRef,
};

/// Every table this statement names, in the order it names them.
///
/// Empty means the statement reaches no table at all — a `USE`, a transaction
/// verb, or a declaration of something that is not a table.
#[must_use]
pub(crate) fn tables_named(kind: &StatementKind) -> Vec<&TableRef> {
    match kind {
        // Declarations of a table itself. A grant names a table that already
        // exists, so these are handled by the caller rather than by listing the
        // table they are about to create — see `Error::GrantedUserCannotDeclare`.
        StatementKind::DefineTable { .. }
        | StatementKind::DefineSpace { .. }
        | StatementKind::DefineBucket { .. }
        // Nothing here touches a table: a tenancy, an analyzer, a user, a grant,
        // a selection or a transaction verb.
        | StatementKind::Use { .. }
        | StatementKind::DefineNamespace { .. }
        | StatementKind::DefineDatabase { .. }
        | StatementKind::DefineAnalyzer { .. }
        | StatementKind::DefineUser { .. }
        | StatementKind::AlterUser { .. }
        | StatementKind::DropUser { .. }
        | StatementKind::Grant { .. }
        | StatementKind::Revoke { .. }
        // Configuring the node names no table either, and here that emptiness is
        // the `BACKUP` shape again rather than the harmless kind: what a node is
        // for, and which machines hold its data, are not anybody's tables. Both
        // are `Needs::Administer`, decided before this list is consulted.
        | StatementKind::DefineNode { .. }
        | StatementKind::DefineReplica { .. }
        // Forgetting a consumer names no table. Declaring one does, and it is
        // listed below rather than here — see the arm that returns its
        // destination.
        | StatementKind::DropConsumer { .. }
        | StatementKind::Begin
        | StatementKind::Commit
        | StatementKind::Cancel
        // A backup names **no** table because it reaches every one. That is the
        // opposite of what an empty answer means everywhere else here, so the
        // authorization for it is a role check that does not consult this list
        // at all (`Needs::Administer`), and `within_grants` refuses a
        // grant-governed user by name — because a rule shaped "every table it
        // names is granted" passes vacuously over an empty list.
        | StatementKind::Backup { .. } => Vec::new(),

        // `INFO FOR TABLE users` names its table, so the grant loop below asks
        // about it exactly as a `SELECT` from it would — which is the rule the
        // statement is meant to follow: it reports what the caller could have
        // found out anyway.
        StatementKind::Info {
            subject: InfoSubject::Table(table),
        } => vec![table],

        // A consumer names the table it will write into, and that is the whole
        // reason it appears here at all: without it the grant loop would pass
        // over `DEFINE CONSUMER` vacuously, and a caller could point a
        // background writer at a table they were never granted — the `BACKUP`
        // hole again, in a statement that keeps writing after it is issued.
        //
        // It is the destination and not the brokers because a broker is not a
        // table; what this store must check is where the records land.
        StatementKind::DefineConsumer { destination, .. } => vec![destination],

        // The remaining subjects name **no** table, and that emptiness is the
        // `BACKUP` shape — a loop reading "every table it names is granted"
        // passes over an empty list vacuously. Three of them are safe here for a
        // reason that has to be stated rather than assumed, because the reason
        // is somewhere else: the executor **filters** each report down to what
        // the caller may read, so a table they were never granted is not in the
        // answer to be refused. If that filter is ever removed, this arm is
        // where the hole opens, and `info::tables` is where it is held shut.
        //
        // `INFO FOR USER` and `INFO FOR NODE` are the two that refuse instead,
        // because neither has a smaller truthful form to filter down to. Both
        // need `Administer`, decided by `Needs::of` before this list is
        // consulted — so for them this arm is never the check that matters.
        StatementKind::Info { .. } => Vec::new(),

        // Declarations *on* a table, which is a table that already exists.
        StatementKind::DefineIndex { table, .. }
        | StatementKind::DefineField { table, .. }
        | StatementKind::DropField { table, .. }
        | StatementKind::DropTable { table }
        | StatementKind::DropIndex { table, .. }
        | StatementKind::RebuildIndex { table, .. } => vec![table],

        // The condition is walked for the same reason a read's is: a subquery
        // inside it reaches a table this statement does not name.
        StatementKind::DeleteWhere {
            table, condition, ..
        } => {
            let mut found = vec![table];
            found.extend(in_expr(condition));
            found
        }

        StatementKind::Keys { space, .. } => vec![space],

        // A written value may hold a read — `CREATE audit:1 = { copy: (SELECT
        // * FROM salaries) }` reaches `salaries` — so the value is walked
        // beside the target rather than trusted to be inert.
        StatementKind::Create { target, value } | StatementKind::Set { target, value } => {
            let mut found = vec![&target.table];
            found.extend(in_expr(value));
            found
        }
        StatementKind::Put {
            target,
            value: written,
            ..
        } => {
            let mut found = vec![&target.table];
            found.extend(in_expr(written));
            found
        }
        StatementKind::Update { target, edit } => {
            let mut found = vec![&target.table];
            match edit {
                Edit::Whole(value) => found.extend(in_expr(value)),
                Edit::Fields(assignments) => {
                    for assignment in assignments {
                        found.extend(in_expr(&assignment.value));
                    }
                }
            }
            found
        }

        StatementKind::Get { target }
        | StatementKind::Delete { target }
        | StatementKind::Del { target }
        // A file is a record in the bucket, so the bucket is the table a grant
        // is asked about. The chunks live in a table nothing can name, and are
        // reached only through these two statements — which is what keeps a
        // file's bytes and its metadata behind **one** permission question
        // rather than two (ADR-0011).
        | StatementKind::Read { target, .. } => vec![&target.table],

        // An edge reaches three: the two records it connects and the table the
        // relation is recorded in. A grant on the edge table alone would let
        // somebody write a link between records they cannot see.
        StatementKind::Relate {
            from,
            edges,
            to,
            value,
        } => {
            let mut found = vec![&from.table, edges, &to.table];
            if let Some(value) = value {
                found.extend(in_expr(value));
            }
            found
        }

        StatementKind::Select(select) => in_select(select),
        // **The read's tables, not none.** An `EXPLAIN` that named no table
        // would pass a grant check vacuously — the shape that let a backup
        // through until it was refused by name — and it would leak which index
        // serves a table the caller may not read: a metadata disclosure wearing
        // a diagnostic's clothes.
        StatementKind::Explain(select) => in_select(select),

        // A binding holds an expression, and an expression may hold a read.
        // `LET $all = (SELECT * FROM salaries)` reaches `salaries` as surely as
        // the bare read does, so it is answered for here rather than left to the
        // fact that the statement's own `from` is not a table.
        StatementKind::Let { value, .. } | StatementKind::Return { value } => in_expr(value),
    }
}

/// The tables an expression reaches.
///
/// An expression can hold a read — `(SELECT …)` is an ordinary term — and a
/// read names tables. Nothing else in an expression does: a path is a route
/// inside a record the caller already reached, and a literal names nothing.
///
/// Exhaustive for the two arms that carry a table and deliberately shallow
/// everywhere else, walked recursively so that a read nested two groups deep is
/// found as surely as one written at the top.
fn in_expr(expr: &Expr) -> Vec<&TableRef> {
    match &expr.kind {
        ExprKind::Select(select) => in_select(select),
        // A point read of one record names that record's table.
        ExprKind::Record(target) | ExprKind::Get(target) => vec![&target.table],
        ExprKind::Table(table) => vec![table],
        ExprKind::Not(inner) | ExprKind::Negate(inner) => in_expr(inner),
        // Both arms of a conditional, because either may run and a permission
        // question is asked before anything does.
        ExprKind::If {
            condition,
            then,
            otherwise,
        } => {
            let mut found = in_expr(condition);
            found.extend(in_expr(then));
            if let Some(otherwise) = otherwise {
                found.extend(in_expr(otherwise));
            }
            found
        }
        // The right side of a coalesce runs only when the left holds nothing —
        // and it is still named here, because whether it runs is a property of
        // the data and a grant must not depend on one.
        ExprKind::Coalesce(left, right) => {
            let mut found = in_expr(left);
            found.extend(in_expr(right));
            found
        }
        ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Arithmetic { left, right, .. }
        | ExprKind::Binary { left, right, .. } => {
            let mut found = in_expr(left);
            found.extend(in_expr(right));
            found
        }
        ExprKind::Fold { over, .. } => over.as_deref().map(in_expr).unwrap_or_default(),
        ExprKind::Call { arguments, .. } => arguments.iter().flat_map(in_expr).collect(),
        ExprKind::Array(items) | ExprKind::Set(items) => items.iter().flat_map(in_expr).collect(),
        ExprKind::Object(fields) => fields
            .iter()
            .flat_map(|field| in_expr(&field.value))
            .collect(),
        ExprKind::Range(range) => {
            let mut found = in_expr(&range.start);
            found.extend(in_expr(&range.end));
            found
        }
        // A literal, a parameter and a path name nothing.
        ExprKind::Literal(_) | ExprKind::Parameter(_) | ExprKind::Path(_) => Vec::new(),
    }
}

/// Every table a read reaches — its source **and** its expressions.
///
/// # The half that was missing, and what it cost
///
/// This used to answer with the source alone, and that was a grant bypass
/// rather than an omission. A projection, a `WHERE`, an `ORDER BY` and a
/// `GROUP BY` are all expressions, and an expression may hold a read: `SELECT
/// (SELECT pay FROM salaries) AS leaked FROM public` names `public` as its
/// source and answers with `salaries`. A caller granted `read` on `public`
/// alone was handed the contents of a table nobody granted them, with no error
/// anywhere, because the loop that checks grants was given a list the second
/// table was never on.
///
/// The rule that replaces it is the one the module header already stated for
/// statements: **every table the statement can reach is named here**, wherever
/// in it the name was written.
fn in_select(select: &Select) -> Vec<&TableRef> {
    let mut found = in_source(&select.from);
    if let Projection::Values(projected) = &select.projection {
        for one in projected {
            found.extend(in_expr(&one.value));
        }
    }
    for key in &select.group {
        found.extend(in_expr(key));
    }
    for key in &select.order {
        found.extend(in_expr(&key.key));
    }
    found
}

/// The tables a read's source names.
fn in_source(from: &Source) -> Vec<&TableRef> {
    match from {
        // **No table, and that emptiness is the `BACKUP` shape** — a loop
        // reading "every table it names is granted" passes over an empty list
        // for a reason that has nothing to do with permission. So this one is
        // not governed here at all: `Needs::of` classifies it `Administer`
        // before this list is consulted, and `within_grants` refuses a
        // grant-governed user by role rather than by an empty answer.
        Source::Node => Vec::new(),
        Source::Record(target) => vec![&target.table],
        Source::Table(table) => vec![table],
        // The condition is an expression, and an expression may hold a read.
        Source::Where { table, condition } => {
            let mut found = vec![table];
            found.extend(in_expr(condition));
            found
        }
        // **Every** table in the chain counts, not only the first and the last.
        // A traversal that could read records in a table nobody granted, because
        // the edge table was granted, is a way around the grant rather than a
        // use of it — and a walk of several hops passes through several tables,
        // each of which somebody has to have been granted.
        Source::Traverse { from, hops, .. } => {
            let mut found = vec![&from.table];
            for hop in hops {
                found.push(&hop.edges);
                found.extend(hop.target.as_ref());
            }
            found
        }
        Source::Join {
            left,
            right,
            condition,
            ..
        } => {
            let mut found = vec![left, right];
            if let Some(condition) = condition {
                found.extend(in_expr(condition));
            }
            found
        }
    }
}
