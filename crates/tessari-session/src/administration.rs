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

/// The statement in `kind` that may not travel under another node's
/// assertion, if it is one (ADR-0108 D2): every administered statement, the
/// whole-store backup and restore, and the vault's custody.
pub(crate) fn stays_home(kind: &StatementKind) -> Option<&'static str> {
    if let Some((statement, _)) = administered(kind) {
        return Some(statement);
    }
    Some(match kind {
        StatementKind::DefineNode { .. } => "DEFINE NODE",
        StatementKind::Backup { .. } => "BACKUP",
        StatementKind::Restore { .. } => "RESTORE",
        StatementKind::UnsealVault { .. } => "UNSEAL VAULT",
        StatementKind::SealVault { .. } => "SEAL VAULT",
        StatementKind::ChangeVaultPassphrase { .. } => "CHANGE VAULT PASSPHRASE",
        _ => return None,
    })
}

/// Whether `source` may be carried to another node under an assertion: it
/// parses, and holds no statement that stays home (ADR-0108 D2).
///
/// The node that would carry it asks first, so a caller sending such a
/// statement to the wrong node gets the redirect or refusal it always got; the
/// answering node asks again, because it does not trust the asker.
#[must_use]
pub fn travels(source: &str) -> bool {
    tessari_ql::parse(source).is_ok_and(|script| {
        script
            .statements
            .iter()
            .all(|statement| stays_home(&statement.kind).is_none())
    })
}
