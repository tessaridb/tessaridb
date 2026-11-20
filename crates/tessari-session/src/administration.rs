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
        // The cross-cluster half: the row names an external party, which is
        // membership-shaped however the data it ships is not — the authority
        // and membership rules (ADR-0108) apply by analogy.
        StatementKind::DefineReplication { name, .. } => ("DEFINE REPLICATION", name.text.as_str()),
        StatementKind::AlterReplication { name, .. } => ("ALTER REPLICATION", name.text.as_str()),
        StatementKind::DropReplication { name } => ("DROP REPLICATION", name.text.as_str()),
        StatementKind::DefineFailover { .. } => ("DEFINE FAILOVER", "the store"),
        // The scheme beside the policy: one per cluster, named so the trail
        // says which scheme a statement touched.
        StatementKind::DefineTopology { name, .. } => ("DEFINE TOPOLOGY", name.text.as_str()),
        StatementKind::AlterTopology { name, .. } => ("ALTER TOPOLOGY", name.text.as_str()),
        StatementKind::DropTopology { name } => ("DROP TOPOLOGY", name.text.as_str()),
        // Beside the failover policy it runs under: one per store line and
        // unnamed, so the trail names the statement the way the failover
        // row's does.
        StatementKind::DefineBalancer { .. } => ("DEFINE BALANCER", "the store"),
        StatementKind::DropBalancer => ("DROP BALANCER", "the store"),
        StatementKind::FinalizeFormat => ("ALTER STORE FINALIZE FORMAT", "the store"),
        StatementKind::RevokeCertificate { fingerprint } => {
            ("REVOKE CERTIFICATE", fingerprint.as_str())
        }
        StatementKind::CreateJoinToken { replica, .. } => {
            ("CREATE JOIN TOKEN", replica.text.as_str())
        }
        StatementKind::CreateLinkToken { replication, .. } => {
            ("CREATE LINK TOKEN", replication.text.as_str())
        }
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
