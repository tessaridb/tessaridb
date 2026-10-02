//! Which statements change who may do what, or who belongs to the cluster.
//!
//! One list, read by two rules (ADR-0108): such a statement is recorded in the
//! audit trail with its change (D8), and it never travels under another node's
//! assertion of a user (D2) — authority and membership change only on a
//! connection that proved a credential to the node judging it.
//!
//! `DEFINE NODE` is not here: it changes this machine's own `META`, outside the
//! log, and runs on a node that may not write — a record in a replicated trail
//! would make it a write and close the one door back from read-only.

use tessari_ql::StatementKind;

/// The statement's spelling and the user or member it is about, when `kind`
/// administers authority or membership.
pub(crate) fn administered(kind: &StatementKind) -> Option<(&'static str, &str)> {
    Some(match kind {
        StatementKind::DefineUser { name, .. } => ("DEFINE USER", name.text.as_str()),
        StatementKind::AlterUser { name, .. } => ("ALTER USER", name.text.as_str()),
        StatementKind::DropUser { name } => ("DROP USER", name.text.as_str()),
        StatementKind::Grant { user, .. } | StatementKind::GrantAuthority { user, .. } => {
            ("GRANT", user.text.as_str())
        }
        StatementKind::Revoke { user, .. } | StatementKind::RevokeAuthority { user, .. } => {
            ("REVOKE", user.text.as_str())
        }
        StatementKind::DefineReplica { name, .. } => ("DEFINE REPLICA", name.text.as_str()),
        StatementKind::AlterReplica { name, .. } => ("ALTER REPLICA", name.text.as_str()),
        StatementKind::DropReplica { name } => ("DROP REPLICA", name.text.as_str()),
        StatementKind::DefineFailover { .. } => ("DEFINE FAILOVER", "the store"),
        _ => return None,
    })
}
