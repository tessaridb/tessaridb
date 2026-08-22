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

use bgv_db_ql::{Select, Source, StatementKind, TableRef};

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
        | StatementKind::DropUser { .. }
        | StatementKind::Grant { .. }
        | StatementKind::Revoke { .. }
        | StatementKind::Begin
        | StatementKind::Commit
        | StatementKind::Cancel => Vec::new(),

        // Declarations *on* a table, which is a table that already exists.
        StatementKind::DefineIndex { table, .. }
        | StatementKind::DefineField { table, .. }
        | StatementKind::DropField { table, .. }
        | StatementKind::DropTable { table }
        | StatementKind::DropIndex { table, .. }
        | StatementKind::RebuildIndex { table, .. }
        | StatementKind::DeleteWhere { table, .. } => vec![table],

        StatementKind::Keys { space, .. } => vec![space],

        StatementKind::Create { target, .. }
        | StatementKind::Update { target, .. }
        | StatementKind::Set { target, .. }
        | StatementKind::Get { target }
        | StatementKind::Delete { target }
        | StatementKind::Del { target }
        // A file is a record in the bucket, so the bucket is the table a grant
        // is asked about. The chunks live in a table nothing can name, and are
        // reached only through these two statements — which is what keeps a
        // file's bytes and its metadata behind **one** permission question
        // rather than two (ADR-0011).
        | StatementKind::Put { target, .. }
        | StatementKind::Read { target } => vec![&target.table],

        // An edge reaches three: the two records it connects and the table the
        // relation is recorded in. A grant on the edge table alone would let
        // somebody write a link between records they cannot see.
        StatementKind::Relate {
            from, edges, to, ..
        } => vec![&from.table, edges, &to.table],

        StatementKind::Select(select) => in_source(select),
    }
}

/// The tables a read's source names.
fn in_source(select: &Select) -> Vec<&TableRef> {
    match &select.from {
        Source::Record(target) => vec![&target.table],
        Source::Table(table) | Source::Where { table, .. } => vec![table],
        // The far side counts. A traversal that could read records in a table
        // nobody granted, because the edge table was granted, is a way around
        // the grant rather than a use of it.
        Source::Traverse {
            from,
            edges,
            target,
            ..
        } => {
            let mut found = vec![&from.table, edges];
            found.extend(target.as_ref());
            found
        }
        Source::Join { left, right, .. } => vec![left, right],
    }
}
